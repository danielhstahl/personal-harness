#!/usr/bin/env python3
"""looprs-ecr acceptance: the exit path, in a real pty, judged by bytes and by the process table.

Four things no unit test can see, one scenario each.

  1. **Nothing visible is lost.** Quit mid-stream, while the streamed answer is
     still sitting in the live preview region: that text has to appear in the
     bytes painted *after* the quit key, above the erased pane. It used to go
     with the pane — `term.clear()` wipes the live area and `insert_before` never
     got the tail.
  2. **The terminal is handed back exactly once.** The capture ends with one
     erase and one newline; the pty is cooked again (`stty -a` equivalent, read
     straight off the tty the app was using); the exit code is 0; and there is no
     "cursor position could not be read" anywhere. That error used to be the
     *last line of every run*, because the final clear asked the terminal where
     the cursor was and the async key reader ate the answer.
  3. **No child outlives the app.** Not the shell from the generated rcfile, not
     the pi child — checked in the process table after the app is gone, which is
     where an orphan lives.
  4. **A wedged child cannot hold the door.** A fake pi that ignores stdin EOF
     *and* SIGTERM still gets killed inside the exit budget, and the app still
     comes back with its terminal.

Every backend is a fake the script writes, so nothing here costs a model call.

    python3 spikes/shutdown_e2e.py | tee spikes/results/shutdown-e2e.log
"""

import fcntl
import os
import pty
import re
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
SLOW_PI = "spikes/fake_pi_slow.py"
RESULTS = []
START = time.time()

ROWS, COLS = 44, 100
CTRL_Q = b"\x11"
MARK = "ALPHASHEET"  # first word of the slow fake's answer
# The app's own budget: SHUTDOWN_GRACE (2s) for the sessions, +0.5s of drain,
# +1s for the router task, plus room for a slow machine.
EXIT_BUDGET = 8.0


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, ok))
    mark = "PASS" if ok else "FAIL"
    say(f"{mark}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def strip_style(text):
    return re.sub(r"\x1b\[[0-9;?]*m", "", text)


def strip_cpr_replies(text):
    """Drop cursor-position **replies** (`ESC[<row>;<col>R`).

    These are input the harness itself injected as answers to the app's earlier
    cursor queries, and a late one (the 0.30s insurance reply) can land after the
    app has handed the terminal back. Two forms are matched because the capture
    shows both:

    * `\x1b[2;1R` — the reply itself, read while the tty is raw; and
    * `^[[2;1R`  — the same bytes echoed back by the line discipline in caret
      notation once the tty is cooked again, which is exactly the state it is in
      after the restore. The caret and the bracket are literal characters here
      (`5e 5b`), not an escape sequence, which is why a `\x1b`-only pattern
      misses it and the tail looks like the app wrote something when it wrote
      nothing.

    Neither is the app's output, so neither counts against "the run ends with one
    erase and one newline".
    """
    return re.sub(r"(?:\x1b|\^\[)\[\d+;\d+R", "", text)


def rows_painted(text):
    """row (0-based) -> the text painted there, last write winning.

    Same recovery the viewport spike uses: the backend addresses the screen
    absolutely ahead of every run of cells, so the row a byte belongs to is
    readable out of the escape sequences that preceded it.
    """
    csi = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")
    screen = {}
    row = 0
    pos = 0
    while True:
        m = csi.search(text, pos)
        pre = strip_style(text[pos:m.start()] if m else text[pos:])
        if pre:
            screen[row] = screen.get(row, "") + pre
        if m is None:
            break
        if m.group(2) in ("H", "f"):
            nums = [n for n in m.group(1).split(";") if n != ""]
            row = int(nums[0]) - 1 if nums else 0
        pos = m.end()
    return {r: v for r, v in screen.items() if v.strip()}


def procs_matching(pattern):
    """Live processes whose command line matches `pattern`, by pgrep -f."""
    r = subprocess.run(["pgrep", "-f", pattern], capture_output=True, text=True)
    return [p for p in r.stdout.split() if p]


def last_erase(text):
    """`(end_offset, row)` of the final "move then clear from cursor down`.

    That pair is what the exit writes — park on the live region's own top row,
    then erase downward — and it is the last one in the stream because the
    teardown is the last thing the app does. The row it landed on is the
    boundary between "rows the app owns and just gave back" and "rows above it,
    which are the user's scrollback and must still be there".
    """
    last = None
    for m in re.finditer(r"\x1b\[(\d+);1H\x1b\[(?:2)?J", text):
        last = (m.end(), int(m.group(1)) - 1)
    return last


class Driver:
    """The app under a pty, with a terminal that answers cursor queries.

    The pty is emulated only in the one way the app insists on: it answers
    `ESC[6n`. Everything else the app writes goes into a capture buffer, which is
    the evidence.
    """

    def __init__(self, rows=ROWS, cols=COLS, **env_over):
        self.master, slave = pty.openpty()
        self.rows, self.cols = rows, cols
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        # A copy of the slave stays open here so the *tty's* termios can be read
        # back after the app dies. The settings the app changes (raw mode) live on
        # the slave side, so the master's own termios says nothing about them.
        self.tty_fd = os.dup(slave)
        env = dict(os.environ)
        env.update(TERM="xterm-256color", LOOPRS_BD_BIN="/bin/echo")
        env.update(env_over)
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
        )
        os.close(slave)
        self.lock = threading.Lock()
        self.raw = bytearray()
        self.last_row = 0
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                with self.lock:
                    self.raw.extend(data)
                    row = min(self.last_row + 1, self.rows - 1)
                for _ in range(data.count(b"\x1b[6n")):
                    say("  pump saw ESC[6n cursor query")
                    # Answered on a delay: the app writes `ESC[6n` and *then*
                    # stops its async reader, so an instant answer can be eaten
                    # by that reader on its way out. Insurance reply #2 covers
                    # the window; a stray CPR nobody asked for is ignored.
                    threading.Timer(0.02, self._answer_cpr, args=(row,)).start()
                    threading.Timer(0.30, self._answer_cpr, args=(row,)).start()
        except OSError as exc:
            with self.lock:
                self.raw.extend(b"\n[pump ended: %s]\n" % str(exc).encode())

    def _answer_cpr(self, row):
        try:
            os.write(self.master, b"\x1b[%d;1R" % (min(row, self.rows - 1) + 1))
        except OSError:
            pass

    def text(self):
        with self.lock:
            return bytes(self.raw).decode("utf-8", errors="replace")

    def mark(self):
        with self.lock:
            return len(self.raw)

    def since(self, mark):
        with self.lock:
            return bytes(self.raw[mark:]).decode("utf-8", errors="replace")

    def screen(self):
        sc = rows_painted(self.text())
        if sc:
            self.last_row = max(sc.keys())
        return sc

    def send(self, b):
        os.write(self.master, b)

    def alive(self):
        return self.proc.poll() is None

    def quit(self, timeout=20):
        """Press Ctrl-Q and wait for the app to actually leave."""
        mark = self.mark()
        t0 = time.time()
        self.send(CTRL_Q)
        try:
            code = self.proc.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            return None, None, time.time() - t0
        # Let the last bytes land in the capture before reading it.
        time.sleep(0.3)
        return code, mark, time.time() - t0

    def tty_is_cooked(self):
        """`stty -a` narrowed to the one question: is raw mode still on?

        Raw mode clears ICANON, ECHO, ISIG and IEXTEN; cooked mode has all of
        them. Asking for ICANON and ECHO is enough to tell the two apart, and it
        is the question the ticket's acceptance names — a terminal left raw is a
        terminal the user has to `reset` before they can type their next command.
        """
        try:
            lflag = termios.tcgetattr(self.tty_fd)[3]
        except OSError:
            return False
        return bool(lflag & termios.ICANON) and bool(lflag & termios.ECHO)

    def close(self):
        try:
            os.close(self.tty_fd)
            os.close(self.master)
        except OSError:
            pass


def write_fake(path, body):
    with open(path, "w") as f:
        f.write(body)
    os.chmod(path, 0o755)


def scenario_mid_stream():
    say("=== quit mid-stream: the answer is on screen, then the app is gone ===")
    tmp = tempfile.mkdtemp(prefix="looprs-ecr-slow-")
    pi = os.path.join(tmp, "pi")
    shutil.copy(SLOW_PI, pi)
    os.chmod(pi, 0o755)

    d = Driver(LOOPRS_PI_BIN=pi, LOOPRS_FAKE_HOLD="40")
    say(f"spawned {BIN} (pid {d.proc.pid}) in a {ROWS}x{COLS} pty")
    time.sleep(1.5)
    check("the app boots and is alive", d.alive())

    d.send(b"\t")  # Beads -> Pi
    time.sleep(0.4)
    d.send(b"write me a long answer\r")
    # Let the answer get far enough into the live preview region that there is a
    # tail on screen which has not been flushed yet.
    time.sleep(2.5)
    live = [r for r, t in d.screen().items() if "bravo" in t or MARK in t]
    check("the answer is streaming into the live region", len(live) >= 2, f"{len(live)} rows carrying it")

    code, mark, dt = d.quit()
    since = d.since(mark)
    check("the app exits on Ctrl-Q", code is not None, "it never left")
    check("exit code is 0", code == 0, f"exit {code}")
    check(
        "no cursor query on the exit path",
        "\x1b[6n" not in since and "^[6n" not in since,
        "the exit path asked the terminal where the cursor is",
    )
    check(
        "no 'cursor position could not be read' anywhere in the run",
        "could not be read" not in d.text(),
        [l for l in d.text().splitlines() if "could not be read" in l][:1],
    )
    erased = last_erase(d.text())
    check("the live pane is erased on the way out", erased is not None)
    if erased is not None:
        erase_end, erase_row = erased
        # The precise form of "everything that was on screen is still in the
        # scrollback": the answer was sitting in the live region, and the live
        # region is exactly the rows the final erase takes away. So the answer
        # survives **only if** the drain put it back above that row first.
        #
        # This is the check a repaint racing the quit key cannot satisfy: a
        # repaint only ever touches the pane, and the pane is below the line.
        screen = rows_painted(d.text()[:erase_end])
        above = sorted(
            r
            for r, txt in screen.items()
            if r < erase_row and ("bravo" in txt or MARK in txt)
        )
        check(
            "nothing on screen was lost: the live tail was flushed ABOVE the erase line",
            len(above) >= 2,
            f"{len(above)} answer rows above row {erase_row} — the rest went with the pane",
        )
        after_erase = d.text()[erase_end:]
        check(
            "exactly one closing newline after the erase (no doubled prompt)",
            re.fullmatch(r"\r?\n", strip_cpr_replies(after_erase)) is not None,
            f"trailing bytes after the erase: {after_erase!r}",
        )
    check("the tty is cooked again after exit", d.tty_is_cooked())
    check("no bash from the generated rcfile survived", not procs_matching("looprs-bash-integration"))
    check("no fake pi survived the app", not procs_matching(pi), procs_matching(pi))
    say(f"exited {dt:.2f}s after the quit key, code {code}")
    d.close()
    shutil.rmtree(tmp, ignore_errors=True)


def scenario_bash_child():
    say("=== quit with a Bash child running: the shell goes with the app ===")
    d = Driver()
    time.sleep(1.5)
    d.send(b"\t")  # Beads -> Pi
    time.sleep(0.3)
    d.send(b"\t")  # Pi -> Bash
    time.sleep(0.5)
    d.send(b"sleep 30\r")
    time.sleep(1.5)
    check("the shell is up and busy", d.alive() and "Bash" in d.text())

    code, mark, dt = d.quit()
    check("the app exits on Ctrl-Q with a busy shell", code is not None)
    check("exit code is 0", code == 0, f"exit {code}")
    check("the exit stayed inside the budget", dt <= EXIT_BUDGET, f"{dt:.2f}s > {EXIT_BUDGET}s")
    check("no shell from our rcfile outlived the app", not procs_matching("looprs-bash-integration"),
          procs_matching("looprs-bash-integration"))
    check("no stray `sleep 30` from that shell", not procs_matching("sleep 30"))
    check("the tty is cooked again after exit", d.tty_is_cooked())
    check("no 'could not be read' error at the end of the run",
          "could not be read" not in d.since(mark))
    d.close()


def scenario_wedged_child():
    say("=== a child that will not stop: the budget has to be the answer ===")
    tmp = tempfile.mkdtemp(prefix="looprs-ecr-wedge-")
    pi = os.path.join(tmp, "wedged-pi")
    # Ignores every polite signal and never reads stdin, so "close the child's
    # stdin so it can dispose its runtime" gets nothing back. This is the child
    # that must be killed rather than waited on.
    write_fake(
        pi,
        "#!/bin/bash\n"
        "trap '' TERM INT HUP QUIT\n"
        f"echo 'wedged pi up pid=$$' >&2\n"
        "while :; do sleep 0.5; done\n",
    )

    d = Driver(LOOPRS_PI_BIN=pi)
    time.sleep(1.2)
    d.send(b"\t")  # Beads -> Pi
    time.sleep(0.4)
    d.send(b"anything at all\r")
    time.sleep(1.5)
    children = procs_matching(pi)
    check("the wedged child is up", len(children) >= 1, str(children))

    code, mark, dt = d.quit()
    check("the app exits even with a child that ignores everything", code is not None)
    check("exit code is 0", code == 0, f"exit {code}")
    check(
        f"the exit stayed inside the {EXIT_BUDGET}s budget",
        dt <= EXIT_BUDGET,
        f"{dt:.2f}s",
    )
    check("the wedged child was killed, not left behind", not procs_matching(pi),
          procs_matching(pi))
    check("the tty is cooked again after exit", d.tty_is_cooked())
    say(f"exited {dt:.2f}s after the quit key against a child that ignored SIGTERM")
    d.close()
    shutil.rmtree(tmp, ignore_errors=True)


def main():
    for path, why in ((BIN, "run `cargo build` first"), (SLOW_PI, "missing fixture")):
        if not os.path.exists(path):
            say(f"{why}: {path}")
            return 2
    scenario_mid_stream()
    scenario_bash_child()
    scenario_wedged_child()

    failed = [n for n, ok in RESULTS if not ok]
    say(f"{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    if failed:
        say("failed: " + "; ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
