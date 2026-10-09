#!/usr/bin/env python3
"""`./scripts/docs.sh serve` — local preview with live reload.

Small on purpose. Two threads:

* the **watcher** stats every `.md` in `docs/` plus `spikes/README.md` every
  half-second and re-runs the generator when one changed. Polling instead of
  `watchdog` because a dependency-free 10-line poll is worth more than a
  file-watching dependency for a docs server, and it works identically on macOS
  and Linux (FSEvents vs inotify differences are exactly what a dependency here
  would be papering over).
* the **server** is `http.server` over the built directory, with one extra route:
  `GET /__build` returns the build token. Each page runs
  `assets/livereload.js`, which polls that and reloads on change.

Not a production server: binds 127.0.0.1, no TLS, no concurrency guarantees. It
exists so the answer to "what does my page look like" takes one keystroke.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import os
import socketserver
import subprocess
import sys
import threading
import time
from pathlib import Path


def snapshot(paths):
    """(path, mtime_ns, size) triples, cheap enough to take twice a second."""
    out = {}
    for root in paths:
        if not root.exists():
            continue
        if root.is_file():
            st = root.stat()
            out[str(root)] = (st.st_mtime_ns, st.st_size)
            continue
        for dirpath, dirnames, filenames in os.walk(root):
            dirnames[:] = [d for d in dirnames if d not in {"_site", "target", "__pycache__"}]
            for name in filenames:
                if name.endswith((".md", ".css", ".js")) or name == "SUMMARY.md":
                    p = os.path.join(dirpath, name)
                    try:
                        st = os.stat(p)
                    except OSError:
                        continue
                    out[p] = (st.st_mtime_ns, st.st_size)
    return out


class Handler(http.server.SimpleHTTPRequestHandler):
    def do_GET(self):  # noqa: N802 - http.server's spelling
        if self.path.rstrip("/") == "/__build" or self.path == "/__build":
            token = "0"
            try:
                token = (Path(self.directory) / "__build").read_text().strip()
            except Exception:
                pass
            body = token.encode()
            self.send_response(200)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)
            return
        return super().do_GET()

    def log_message(self, fmt, *args):  # quieter than the default
        if "livereload" in (fmt % args):
            return
        super().log_message(fmt, *args)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--site", required=True, type=Path)
    ap.add_argument("--root", required=True, type=Path, help="repo root, for the rebuild command")
    ap.add_argument("--port", type=int, default=3000)
    args = ap.parse_args()

    site = args.site.resolve()
    watched = [site.parent, args.root / "spikes" / "README.md"]
    build_file = site / "__build"

    handler = functools.partial(Handler, directory=str(site))

    def rebuild() -> None:
        ts = str(int(time.time()))
        build_file.write_text(ts)

    state = {"last": snapshot(watched)}

    def watch() -> None:
        while True:
            time.sleep(0.5)
            now = snapshot(watched)
            if now != state["last"]:
                changed = [
                    p
                    for p in set(now) | set(state["last"])
                    if now.get(p) != state["last"].get(p)
                ]
                state["last"] = now
                print(
                    "\ndocs.serve: %d file(s) changed (%s) — rebuilding"
                    % (len(changed), ", ".join(sorted(Path(p).name for p in changed)[:3])),
                    flush=True,
                )
                rc = subprocess.call(
                    ["bash", str(args.root / "scripts" / "docs.sh"), "build"], cwd=str(args.root)
                )
                if rc != 0:
                    print(
                        "docs.serve: rebuild FAILED (exit %d) — serving the last good build" % rc,
                        flush=True,
                    )
                else:
                    rebuild()
                    print("docs.serve: rebuilt, reload should be automatic", flush=True)

    threading.Thread(target=watch, daemon=True).start()

    class Server(socketserver.ThreadingTCPServer):
        allow_reuse_address = True
        daemon_threads = True

    with Server(("127.0.0.1", args.port), handler) as httpd:
        print(
            "\ndocs.serve: http://localhost:%d/  (Ctrl-C to stop)\n" % args.port,
            flush=True,
        )
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            print("\ndocs.serve: stopped", flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
