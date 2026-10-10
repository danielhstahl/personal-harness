#!/usr/bin/env python3
"""End-to-end drive of Esc (cancellation) inside the real TUI, in all three modes.

The unit tests in `src/session/{bash,pi_chat,beads}.rs` prove the *sessions*
behave, with real child processes and real signals. What they cannot prove is the
thing a user actually experiences: that the acknowledgement survives
`App.update` -> `Flusher` -> `insert_before` -> the real terminal, that the keystroke
is routed by the Router to the mode on screen rather than to the mode that was
last typed into, and that the app is still usable on the far side of a cancel.

So this drives the built binary in a real pty and scores the four rows of the
cancellation contract (ADR-0003) per mode:

    idle            Esc changes nothing, and says nothing
    in flight       "cancelling …" appears in well under a second
    unwound         "cancelled" / "interrupted (exit 130)" follows
    refuses         (unit-tested only — a real stubborn tool is a real hang)

`pi` and `bd` are pointed at small fakes written by this script, so the run costs
no model call while still exercising the real RPC wire, real pids and real
liveness. Bash is the real shell on a real pty, because `0x03` is the subject.

    python3 spikes/cancel_e2e.py | tee spikes/results/cancel-e2e.log

Exit 0 means every assertion held.
"""

import json
import os
import pty
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
ROWS, COLS = 40, 132
BEAD = "looprs-5g7"

RESULTS = []
START = time.time()


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, ok))
    say(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def visible(text):
    """Strip the ANSI our own renderer emits, so greps match words on screen."""
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", text)
    return text


def norm(text):
    return re.sub(r"\s+", "", visible(text))


class Driver:
    def __init__(self, env_extra):
        self.master, slave = pty.openpty()
        import fcntl
        import termios

        fcntl.ioctl(
            slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0)
        )
        env = dict(os.environ)
        env.update(TERM="xterm-256color")
        env.update(env_extra)
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
        )
        os.close(slave)
        self.lock = threading.Lock()
        self.raw = bytearray()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                with self.lock:
                    self.raw.extend(data)
                # Play the terminal that answers ratatui's cursor query, or the
                # app dies before it ever draws (see bash_e2e.py for why).
                for _ in range(data.count(b"\x1b[6n")):
                    os.write(self.master, b"\x1b[1;1R")
        except OSError as e:
            with self.lock:
                self.raw.extend(b"\n[pump ended: %s]\n" % str(e).encode())

    def send(self, b):
        os.write(self.master, b)

    def text(self):
        with self.lock:
            return bytes(self.raw).decode("utf-8", errors="replace")

    def shown(self, needle, since=None):
        hay = self.text() if since is None else self.since(since)
        return norm(needle) in norm(hay)

    def since(self, mark):
        t = self.text()
        i = t.rfind(mark)
        return t[i + len(mark):] if i >= 0 else t

    def wait_for(self, needle, timeout=8.0, since=None):
        """Seconds until `needle` reached the screen, or None if it never did."""
        t0 = time.time()
        deadline = t0 + timeout
        while time.time() < deadline:
            if self.shown(needle, since=since):
                return time.time() - t0
            time.sleep(0.01)
        return None

    def expect(self, name, needle, timeout=8.0, since=None):
        took = self.wait_for(needle, timeout=timeout, since=since)
        check(name, took is not None, f"never saw {needle!r} within {timeout}s")
        return took

    def absent(self, name, needle, for_seconds=1.5, since=None):
        """`needle` must not appear for the whole window (the no-op rows)."""
        end = time.time() + for_seconds
        while time.time() < end:
            if self.shown(needle, since=since):
                check(name, False, f"saw {needle!r} during the quiet window")
                return
            time.sleep(0.02)
        check(name, True)


FAKE_PI = r"""#!/usr/bin/env python3
import json, os, sys, threading, time

HERE = os.path.dirname(os.path.realpath(__file__))
LOG = os.path.join(HERE, "pi.log")
out_lock = threading.Lock()
live = []

def log(m):
    with open(LOG, "a") as f:
        f.write(m + "\n"); f.flush()

def emit(o):
    with out_lock:
        sys.stdout.write(json.dumps(o) + "\n"); sys.stdout.flush()

def response(rid, cmd, data=None):
    rec = {"type": "response", "command": cmd, "success": True}
    if rid is not None: rec["id"] = rid
    if data is not None: rec["data"] = data
    emit(rec)

def run_turn(n, aborted):
    emit({"type": "agent_start"})
    emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
    emit({"type": "message_update", "assistantMessageEvent":
          {"type": "text_delta", "contentIndex": 0, "delta": "working on it"}})
    emit({"type": "message_end", "message": {"role": "assistant", "content": []}})
    # Hold the run open the way a long tool call does; the test cancels it.
    deadline = time.time() + 120
    while time.time() < deadline:
        if aborted["hit"]:
            break
        time.sleep(0.01)
    emit({"type": "agent_end", "messages": [], "willRetry": False})
    emit({"type": "agent_settled"})
    log("settled %d" % n)

log("spawn pid=%d" % os.getpid())
n = 0
while True:
    line = sys.stdin.readline()
    if not line: break
    line = line.strip()
    if not line: continue
    try:
        cmd = json.loads(line)
    except ValueError:
        continue
    kind = cmd.get("type"); rid = cmd.get("id")
    log("recv %s" % kind)
    if kind == "prompt":
        n += 1
        response(rid, "prompt", {"disposition": "started"})
        aborted = {"hit": False}
        live.append(aborted)
        threading.Thread(target=run_turn, args=(n, aborted), daemon=True).start()
    elif kind == "steer":
        response(rid, "steer", {"disposition": "queued"})
    elif kind == "clear_queue":
        response(rid, "clear_queue", {"steering": [], "followUp": []})
    elif kind == "abort":
        for a in list(live): a["hit"] = True
        del live[:]
        response(rid, "abort")
    elif kind == "get_state":
        response(rid, "get_state", {"isStreaming": len(live) > 0})
    else:
        response(rid, kind)
"""

FAKE_BD = """#!/bin/sh
echo "bd $*" >>"$FAKE_BD_LOG"
case "$1" in
  ready) printf '%s\\n' '{"data":[{"id":"__BEAD__","title":"cancel me","status":"open","issue_type":"task"}],"schema_version":1}' ;;
  show)  printf '%s\\n' '[]' ;;
  *)     printf '%s\\n' '{}' ;;
esac
"""


def main():
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2

    tmp = tempfile.mkdtemp(prefix="looprs-cancel-e2e-")
    pi_bin = os.path.join(tmp, "pi")
    bd_bin = os.path.join(tmp, "bd")
    bd_log = os.path.join(tmp, "bd.log")
    with open(pi_bin, "w") as fh:
        fh.write(FAKE_PI)
    os.chmod(pi_bin, 0o755)
    with open(bd_bin, "w") as fh:
        fh.write(FAKE_BD.replace("__BEAD__", BEAD))
    os.chmod(bd_bin, 0o755)

    d = Driver(
        {
            "LOOPRS_PI_BIN": pi_bin,
            "LOOPRS_BD_BIN": bd_bin,
            "LOOPRS_SHELL_BIN": os.environ.get("LOOPRS_SHELL_BIN", "/bin/bash"),
            "FAKE_BD_LOG": bd_log,
        }
    )
    say(f"spawned {BIN} (pid {d.proc.poll()}) in a {ROWS}x{COLS} pty; fakes in {tmp}")
    time.sleep(1.5)

    # ---------------------------------------------------------------- Beads ----
    # The loop self-starts a worker pass for the one ready bead, so there is
    # something real to cancel the moment the app comes up.
    d.expect("beads: the worker pass started on its own", f"beads: working {BEAD}")

    mark = d.text()  # everything from here on is new
    t_esc = time.time()
    d.send(b"\x1b")  # Esc
    ack = d.wait_for(f"cancelling `{BEAD}`", timeout=1.0, since=mark)
    check(
        "beads: Esc is acknowledged with the bead named, in under 1s",
        ack is not None,
        "no acknowledgement within 1s",
    )
    done = d.wait_for("cancelled", timeout=6.0, since=mark)
    total = time.time() - t_esc
    check(
        "beads: the cancelled pass settles as a cancellation, not a completion",
        done is not None,
        "never reported as cancelled",
    )
    check(
        "beads: keystroke to cancelled, in under 1s",
        total < 1.0,
        f"took {total:.2f}s",
    )
    d.expect(
        "beads: the parked loop says so and asks for an instruction",
        "the loop is parked",
        since=mark,
    )
    # No auto-advance: the fake logged exactly one prompt, and there is exactly one
    # pi child per mode rule, so a second worker would show up as a second prompt.
    time.sleep(1.0)
    prompts = 0
    with open(os.path.join(tmp, "pi.log")) as fh:
        for line in fh:
            if line.startswith("recv prompt"):
                prompts += 1
    check(
        "beads: cancelling did NOT start another bead (one prompt, ever)",
        prompts == 1,
        f"{prompts} prompts went out: the loop advanced on the cancel",
    )

    # Idle Esc: nothing in flight, so nothing new may appear.
    mark = d.text()
    d.send(b"\x1b")
    d.absent("beads: an idle Esc says nothing at all", "cancelling", for_seconds=1.5, since=mark)

    # ------------------------------------------------------------------ Pi ----
    d.send(b"\t")  # Beads -> Pi
    time.sleep(0.5)
    d.send(b"hold the line\r")
    d.expect("pi: the run started", "hold the line")
    time.sleep(0.6)
    mark = d.text()
    t_esc = time.time()
    d.send(b"\x1b")  # Esc
    ack = d.wait_for("cancelling the Pi run", timeout=1.0, since=mark)
    check(
        "pi: Esc is acknowledged in under 1s",
        ack is not None,
        "no acknowledgement within 1s",
    )
    done = d.wait_for("cancelled", timeout=6.0, since=mark)
    total = time.time() - t_esc
    check("pi: the cancelled run reports 'cancelled'", done is not None, "never reported")
    check("pi: keystroke to cancelled, in under 1s", total < 1.0, f"took {total:.2f}s")
    mark = d.text()
    d.send(b"\x1b")
    d.absent("pi: an idle Esc says nothing at all", "cancelling", for_seconds=1.5, since=mark)

    # ---------------------------------------------------------------- Bash ----
    d.send(b"\t")  # Pi -> Bash
    time.sleep(0.6)
    d.send(b"sleep 25\r")
    time.sleep(0.8)
    mark = d.text()
    t_esc = time.time()
    d.send(b"\x1b")  # Esc
    ack = d.wait_for("cancelling `sleep 25`", timeout=1.0, since=mark)
    check(
        "bash: Esc is acknowledged with the command named, in under 1s",
        ack is not None,
        "no acknowledgement within 1s",
    )
    done = d.wait_for("interrupted (exit ", timeout=6.0, since=mark)
    total = time.time() - t_esc
    check(
        "bash: the command is interrupted, with the interrupt named as itself",
        done is not None,
        "the command was never reported interrupted",
    )
    check(
        "bash: keystroke to interrupted, in under 1s",
        total < 1.0,
        f"took {total:.2f}s",
    )
    # The shell survived and is usable, which is the half that matters: an
    # interrupt that kills the shell is a worse outcome than one that misses.
    run_mark = d.text()
    d.send(b"echo still-here | tr a-z A-Z\r")
    d.expect("bash: the same shell answers after the interrupt", "STILL-HERE", since=run_mark)

    mark = d.text()
    d.send(b"\x1b")
    d.absent("bash: an idle Esc says nothing at all", "cancelling", for_seconds=1.5, since=mark)

    # Quit, and look for orphans: a cancelled child that outlived the app would be
    # the failure mode this whole ticket family is about.
    d.send(b"\x11")  # Ctrl-Q
    quit_clean = True
    for _ in range(80):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    else:
        quit_clean = False
    check("Ctrl-Q quits looprs after all that cancelling", quit_clean)

    time.sleep(0.5)
    ps = subprocess.run(["ps", "-eo", "pid,ppid,command"], capture_output=True, text=True).stdout
    ours = []
    for line in ps.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 3:
            continue
        _pid, ppid, cmd = parts
        if tmp in cmd or ppid == str(d.proc.pid):
            ours.append(line)
    check("nothing from this run survives the quit", not ours, "\n".join(ours))

    if d.proc.poll() is None:
        d.proc.send_signal(signal.SIGKILL)
    shutil.rmtree(tmp, ignore_errors=True)

    failed = [n for n, ok in RESULTS if not ok]
    say("")
    say(f"{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    if failed:
        say("failed: " + "; ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
