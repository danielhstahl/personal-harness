#!/usr/bin/env python3
"""Can a full-screen program be *used* through Bash mode as it is built today?

ADR-0001 rule 1 says Bash output is passthrough and never re-flowed, and its
acceptance line claims "vim works (measured)". That claim was measured on a raw pty
spike (`spikes/pty.log`, `spikes/pty-interleave-10runs.txt`), where the child's
bytes went straight to a terminal. The shipped path is not that: bytes go through
`ControlStripper` into a scrollback transcript, which drops CSI colors, erases and
cursor movement so the transcript stays readable as text. Those are exactly the
bytes a full-screen program is made of.

So this measures the difference, instead of asserting it. Run:

    cargo build
    python3 spikes/vim_fullscreen.py

and read `spikes/results/vim-fullscreen.log`.
"""

import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bash_e2e as e  # reuse the PTY driver, checks, and matching helpers

PROBE = "/tmp/looprs-vim-probe.txt"


def main():
    if os.path.exists(PROBE):
        os.remove(PROBE)
    if not os.path.exists(e.BIN):
        e.say(f"no binary at {e.BIN}; run `cargo build` first")
        return 2

    d = e.Driver()
    e.say(f"spawned {e.BIN} (pid {d.proc.pid})")
    d.wait(1.5)
    d.send(b"\t")
    d.wait(0.4)
    d.send(b"\t")
    d.wait(0.6)

    # 1. Does the shell think it is on a terminal? If it does not, nothing below
    #    matters and vim will refuse before we even get to rendering.
    d.expect_after(
        "the shell reports a real tty size (stty), so vim is not warned off",
        b"stty size\r",
        "40 132",
    )

    # 2. Launch vim and see whether it complains about not being on a terminal.
    start = len(d.text())
    d.send(b"vim -u NONE -i NONE -n " + PROBE.encode() + b"\r")
    d.wait(2.0)
    opened = d.text()[start:]
    e.check(
        "vim starts without the 'not to a terminal' warning",
        "not to a terminal" not in e.norm(opened),
    )

    # 3. Drive it: insert text, write, quit. This is the part the ADR's acceptance
    #    line needs to mean something.
    d.send(b"i")
    d.wait(0.3)
    d.send(b"written-by-vim-through-looprs")
    d.wait(0.3)
    d.send(b"\x1b")  # Esc
    d.wait(0.4)
    d.send(b":wq!\r")
    d.wait(2.5)

    wrote = os.path.exists(PROBE) and "written-by-vim-through-looprs" in open(
        PROBE, encoding="utf-8", errors="replace"
    ).read()
    e.check("vim can write the file it was told to write", wrote)
    e.check("vim quit and the shell is back", d.proc.poll() is None)

    # 4. The real question: what did the user SEE? vim's screen is cursor addressing,
    #    a clear, and status lines. The transcript path strips cursor addressing, so
    #    the frame cannot be reconstructed; measure how much survives.
    screen = e.norm(d.text()[start:])
    marks = {
        "the filename appeared": "looprs-vim-probe" in screen,
        "the insert-mode status line appeared": "--INSERT--" in screen,
        "blank-line tildes appeared": "~" in screen,
    }
    for name, ok in marks.items():
        e.check(f"screen evidence: {name}", ok)
    e.say(f"vim screen text captured (normalized, whitespace-free): {len(screen)} chars")
    e.say(f"first 200 chars of the vim stretch: {screen[:200]!r}")

    # 5. Is the app still usable afterwards? (A stuck alt-screen would show up here.)
    d.expect_after(
        "after vim, the shell still runs commands and they reach the screen",
        b"echo post-vim | tr a-z A-Z\r",
        "POST-VIM",
    )
    d.send(b"\x11")  # Ctrl-Q
    for _ in range(60):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    e.check("Ctrl-Q quits after vim", d.proc.poll() is not None)

    passed = sum(1 for _, ok in e.RESULTS if ok)
    e.say(f"{passed}/{len(e.RESULTS)} checks passed")
    failed = [n for n, ok in e.RESULTS if not ok]
    if failed:
        e.say("failed: " + ", ".join(failed))
    with open("/tmp/vimprobe.raw", "wb") as f:
        with d.lock:
            f.write(bytes(d.raw))
    e.say("raw vim stretch: /tmp/vimprobe.raw")
    return 0


if __name__ == "__main__":
    sys.exit(main())
