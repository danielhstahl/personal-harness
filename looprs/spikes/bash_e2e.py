#!/usr/bin/env python3
"""End-to-end drive of the Bash terminal state inside the real TUI.

The unit tests in `src/session/bash.rs` prove the *session* behaves (real pty, real
bash, real markers). This proves the thing nobody can prove from a unit test: that
the bytes get through `App.update` -> `Flusher` -> the frame -> the real terminal,
in a real terminal — and that the keyboard chords actually reach the shell instead
of the app.

Every backend except Bash is pointed at a harmless binary so the run cannot cost a
model call: `LOOPRS_BD_BIN=/bin/echo`, `LOOPRS_PI_BIN=/usr/bin/false`.

    python3 spikes/bash_e2e.py | tee spikes/results/bash-e2e.log

Exit 0 means every assertion held.

**Why some checks read a screen and not the wire** (changed by looprs-pdl.4).
Since the frame owns the alternate screen, every write is a *diff* against what the
frame last painted, and a cell whose glyph did not change is simply not re-sent.
Observed on the first run after the migration: the transcript line
`bash-3.2$ echo hi | tr a-z A-Z` reached the wire as `echo`, `h`, ` |`, `tr`,
because the `i` in `hi` sat in a cell that had held `i` since the previous line
(`stty size`) and needed no rewrite. Greping the capture for `echo hi` fails on a
screen displaying exactly that. `shown_on_screen` therefore replays the capture
into `status_e2e.Screen` — the same VT subset the app emits, already written for
this hazard — and asks the *grid*. `shown` on the wire is kept where the question
really is about the stream, and its needles are chosen so elision cannot bite them.
"""

import codecs
import fcntl
import os
import pty
import re
import signal
import struct
import subprocess
import sys
import termios
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from status_e2e import Screen  # noqa: E402  (sibling spike: the VT model, not a driver)

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
ROWS, COLS = 40, 132

RESULTS = []
START = time.time()


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, ok))
    say(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


class Driver:
    def __init__(self):
        self.master, slave = pty.openpty()
        fcntl.ioctl(
            slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0)
        )
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            LOOPRS_BD_BIN="/bin/echo",
            LOOPRS_PI_BIN="/usr/bin/false",
            LOOPRS_SHELL_BIN=os.environ.get("LOOPRS_SHELL_BIN", "/bin/bash"),
        )
        self.proc = subprocess.Popen(
            [BIN],
            stdin=slave,
            stdout=slave,
            stderr=slave,
            env=env,
            close_fds=True,
        )
        os.close(slave)
        self.lock = threading.Lock()
        self.raw = bytearray()
        # (arrival time, cumulative byte count) so a stall can be seen for what it
        # is instead of guessed at from a flat capture.
        self.chunks = []
        # The screen, as opposed to the stream. See the module docstring: with a
        # diffed full-screen frame the capture cannot answer "is it on screen?".
        self.term = Screen(ROWS, COLS)
        self._decode = codecs.getincrementaldecoder("utf-8")(errors="replace")
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                with self.lock:
                    self.raw.extend(data)
                    self.chunks.append((time.time() - START, len(self.raw)))
                    self.term.feed(self._decode.decode(data))
                # ratatui's inline viewport starts by asking where the cursor is
                # (`ESC[6n`). A real terminal answers; this pty has no terminal, so
                # the app dies with "The cursor position could not be read" unless
                # the driver plays one.
                #
                # One reply per query, not one per chunk: a full-screen child (vim)
                # issues queries of its own, and answering two questions with one
                # answer makes the second asker time out and look broken when the
                # harness is what fell short.
                for n in range(data.count(b"\x1b[6n")):
                    os.write(self.master, b"\x1b[1;1R")
        except OSError as e:
            with self.lock:
                self.raw.extend(b"\n[pump ended: %s]\n" % str(e).encode())

    def send(self, bytes_):
        os.write(self.master, bytes_)

    def wait(self, secs):
        time.sleep(secs)

    def text(self):
        with self.lock:
            return bytes(self.raw).decode("utf-8", errors="replace")

    def since(self, mark):
        """Everything after the last occurrence of `mark` (or all of it)."""
        t = self.text()
        i = t.rfind(mark)
        return t[i + len(mark):] if i >= 0 else t

    def expect_after(self, name, trigger, needle, wait_for=6.0, timeout_msg=""):
        """Send `trigger`, then look for `needle` in what comes after it.

        Matched in `norm` space: the renderer puts an absolute cursor-move between
        every run of cells, so raw text never contains "cd /tmp" and
        whitespace-only folding is not enough.
        """
        before = len(self.text())
        self.send(trigger)
        want = re.sub(r"\s+", "", needle)
        deadline = time.time() + wait_for
        while time.time() < deadline:
            if want in norm(self.text()[before:]):
                check(name, True)
                return self.text()[before:]
            time.sleep(0.05)
        check(name, False, timeout_msg or f"never saw {needle!r} after {trigger!r}")
        return self.text()[before:]


def visible(text):
    """Strip the ANSI our own renderer emits, so greps match words on screen.

    Do not grep the result with a spaced needle — use `norm()` for that. The
    renderer addresses every word with an absolute `ESC[<row>;<col>H` and never
    writes the blank cells between them, so the stripped stream reads
    "No|such|file" with the spaces simply absent (see the raw capture: `\\x1b[27;38HNo
    \\x1b[27;41Hsuch`). Pretending the skips are spaces is guesswork; comparing with
    the whitespace taken out of both sides is not.
    """
    text = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", text)
    text = re.sub(r"\x1b\][^\x07\x1b]*(\x07|\x1b\\)", "", text)
    return text


def norm(text):
    """Whitespace-free form of `visible(text)` — the comparison space."""
    return re.sub(r"\s+", "", visible(text))


def shown(d, needle, since=None):
    """Did `needle` reach the terminal?

    A *stream* question. Only sound for needles that cannot be split by the
    frame's diff — see `shown_on_screen`.
    """
    hay = norm(d.text() if since is None else d.since(since))
    return re.sub(r"\s+", "", needle) in hay


def shown_on_screen(d, needle):
    """Is `needle` on the screen right now, per the terminal's own grid?

    Rebuilt from the capture by `status_e2e.Screen`, so a cell the diff did not
    need to rewrite still reads as the glyph it holds. Compared with whitespace
    folded out of both sides for the same reason `norm` exists: the renderer
    addresses word runs separately and never writes the blanks between them.
    """
    want = re.sub(r"\s+", "", needle)
    with d.lock:
        rows = ["".join(r) for r in d.term.grid]
    return any(want in re.sub(r"\s+", "", row) for row in rows)


def run_for(d, name, cmd, needle, wait_for=8.0):
    """Type `cmd` into the Bash view and wait for `needle`.

    Because the pty echoes the command line right back, a needle that also appears
    in the command would be "proved" by the echo. Every needle here is chosen so it
    can only come from the command's own output (hence the `| tr a-z A-Z` in an
    `echo` — the ticket's literal `echo hi` is checked separately, below).
    """
    return d.expect_after(name, cmd, needle, wait_for)


def main():
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2

    d = Driver()
    say(f"spawned {BIN} (pid {d.proc.pid}) in a {ROWS}x{COLS} pty")
    d.wait(1.5)

    # Tab: Beads -> Pi -> Bash. The shell is NOT spawned yet (lazy), and the
    # mode label in the input box is the observable.
    d.send(b"\t")
    d.wait(0.4)
    d.send(b"\t")
    d.wait(0.6)
    check("Tab reaches the Bash mode", shown(d, "Bash"))

    # The shell must be sized to the window we made, not to a library default. This
    # is the only check that the startup resize survives the fact that Bash does not
    # exist yet when it is broadcast (ROUTER last_size -> ensure -> resize at spawn).
    run_for(
        d,
        "the shell opens at the real window size (stty, not the 24x80 default)",
        b"stty size\r",
        "40 132",
    )

    run_for(
        d,
        "echo hi -> the shell's echo puts the typed line on screen (reads like a shell)",
        b"echo hi | tr a-z A-Z\r",
        "HI",
    )
    check(
        "…and the command line itself is echoed as typed",
        shown_on_screen(d, "echo hi"),
        "the transcript band is not showing the command that was run",
    )
    check(
        "echo hi -> exit 0 is shown",
        shown(d, "exit 0", since="echo hi | tr"),
    )

    run_for(
        d,
        "cd /tmp is accepted",
        b"cd /tmp\r",
        "cd /tmp",
    )
    pwd_out = norm(
        run_for(
            d,
            "pwd, a LATER command, reports the earlier cd (persistence)",
            b"pwd\r",
            "/tmp",
        )
    )
    check(
        "…and it is a real /tmp path, not a cwd we invented",
        "/tmp" in pwd_out or "/private/tmp" in pwd_out,
        pwd_out,
    )

    run_for(
        d,
        "false -> visibly exit 1",
        b"false\r",
        "exit 1",
    )

    run_for(
        d,
        "ls of a missing path -> stderr reaches the transcript",
        b"ls /definitely-not-a-real-path-xyz\r",
        "No such file",
    )

    # Ctrl-C must interrupt the *command* and leave the shell usable, and it must
    # not quit looprs. Timed: the ADR measured 1.8s interrupted vs 6.0s ignored.
    d.send(b"sleep 30\r")
    d.wait(1.2)
    t0 = time.time()
    d.send(b"\x03")
    got = None
    deadline = time.time() + 8
    while time.time() < deadline:
        if shown(d, "interrupted"):
            got = time.time() - t0
            break
        if d.proc.poll() is not None:
            break
        time.sleep(0.05)
    check(
        "Ctrl-C interrupts the running command",
        got is not None,
        "no interrupt notice at all",
    )
    if got is not None:
        check(
            f"Ctrl-C acted in {got:.2f}s (not the 30s the command wanted)",
            got < 5.0,
            f"took {got:.2f}s",
        )
    check(
        "Ctrl-C did not quit looprs",
        d.proc.poll() is None,
    )
    d.expect_after(
        "the shell survives Ctrl-C and still runs commands",
        b"echo alive | tr a-z A-Z\r",
        "ALIVE",
    )

    # `exit` -> the shell is gone, the mode is not.
    d.send(b"exit\r")
    d.wait(1.2)
    check("exit -> a visible notice", shown(d, "shell exited"))
    check("exit -> looprs is still up", d.proc.poll() is None)
    d.expect_after(
        "after exit, the next command brings a shell back",
        b"echo restarted | tr a-z A-Z\r",
        "RESTARTED",
    )
    check(
        "the replacement is announced (nothing appears out of nowhere)",
        shown(d, "was gone") and shown(d, "in its place"),
    )

    # The quit chord we own. Ctrl-C in Bash mode goes to the shell, so without a
    # chord of our own there would be no way out.
    d.send(b"\x11")  # Ctrl-Q
    quit_clean = True
    for _ in range(60):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    else:
        quit_clean = False
    check("Ctrl-Q quits looprs", quit_clean)

    # Orphans: the point of kill-on-drop / shutdown discipline. Look for our own
    # generated rcfile in an argv (that is what BashSession spawns), and for
    # anything still parented to the looprs we started.
    time.sleep(0.5)
    ps = subprocess.run(
        ["ps", "-eo", "pid,ppid,command"], capture_output=True, text=True
    ).stdout
    ours = []
    mine = f"looprs-bash-integration-{d.proc.pid}-"
    for line in ps.splitlines():
        parts = line.split(None, 2)
        if len(parts) < 3:
            continue
        _pid, ppid, cmd = parts
        # Matched on *this* run's pid so a leftover from somebody's earlier run is
        # reported as itself and not confused with ours (and so ours cannot hide
        # behind an unrelated bash on the box).
        if mine in cmd or ppid == str(d.proc.pid):
            ours.append(line)
    check("no orphaned bash child left behind", not ours, "\n".join(ours))

    if d.proc.poll() is None:
        d.proc.send_signal(signal.SIGKILL)

    raw_path = os.environ.get("LOOPRS_E2E_RAW")
    if raw_path:
        with open(raw_path, "wb") as fh:
            fh.write(bytes(d.raw))
        say(f"raw terminal stream: {raw_path} ({len(d.raw)} bytes)")
        tl = os.environ.get("LOOPRS_E2E_TIMELINE")
        if tl:
            with open(tl, "w") as fh:
                for t, n in d.chunks:
                    fh.write(f"{t:8.3f}s  {n:8d} bytes\n")
            say(f"arrival timeline: {tl} ({len(d.chunks)} reads)")

    failed = [n for n, ok in RESULTS if not ok]
    say("")
    say(f"{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    if failed:
        say("failed: " + "; ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
