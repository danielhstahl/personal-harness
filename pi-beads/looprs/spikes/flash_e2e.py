#!/usr/bin/env python3
"""The live-region flash, measured off the wire.

What the user sees of a shape change is the status row and the input box vanishing
for a moment before they come back. That moment has a size, and it is measurable
without asking the binary: the app **flushes** its erase (`ESC[<row>;1H ESC[J`)
separately from the frame that replaces what it erased, so the interval between
the erase reaching the pty and the next printable bytes reaching the pty is
exactly the interval a real terminal had nothing there.

Run against the pre-fix binary as a control it fails both checks and the margin is
wide enough to keep them honest:

    pre-fix   25 erases, blank window  min 3.12  p50 8.25  max 8.81  ms
    fixed     13 erases, blank window  min 0.18  p50 0.26  max 0.35  ms

Nothing here needs the binary's log, and nothing here is a screenshot: the needles
are escape sequences and the clock on the reading side of the pty. Read *"Why the
spike reads the wire and not the screen"* in `docs/testing.md` before changing
them.

Two things are being asserted:

* **the hole is gone** — every erase is followed by printable content inside
  `BLANK_BUDGET_MS`. The pre-fix run is not marginally over this; it is an order
  of magnitude over.
* **the reshape is batched** — the erase count stays under half the number of rows
  the streamed answer spread over. One reshape per wrapped row of an answer is the
  thing `Reshape` exists to stop, and the pre-fix binary sits at roughly one.

Usage:

    cargo build
    python3 spikes/flash_e2e.py                  | tee spikes/results/flash-e2e.log
    LOOPRS_BIN=/path/to/old-binary python3 spikes/flash_e2e.py \\
        | tee spikes/results/flash-e2e-control.log

Environment: `LOOPRS_BIN` (default `target/debug/looprs`), `FAKE_PI` (default
`spikes/fake_pi_slow.py`), `RUN_SECONDS`, `ROWS`, `COLS`, `CPR_DELAY`.
"""

import fcntl
import os
import pty
import re
import shutil
import struct
import subprocess
import sys
import termios
import threading
import time

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
ROWS = int(os.environ.get("ROWS", "40"))
COLS = int(os.environ.get("COLS", "100"))
RUN_SECONDS = float(os.environ.get("RUN_SECONDS", "12"))
CPR_DELAY = float(os.environ.get("CPR_DELAY", "0.0005"))
FAKE_PI = os.environ.get("FAKE_PI", "spikes/fake_pi_slow.py")

# A repaint of a full-height pane costs a few milliseconds of CPU, and the fix is
# about where that cost sits relative to the erase: composing the new frame has to
# happen while the old one is still on screen, so that the rows are missing for
# the write only. 1.5 ms clears the measured 0.35 ms max by a wide margin and
# still catches the pre-fix 8.8 ms with room to spare.
BLANK_BUDGET_MS = 1.5
# `RESHAPE_MIN_DELTA` is 3 rows, so the floor on erases is about one per three
# rows of answer, plus a settle at the end of the flow. Half the answer's rows is
# comfortably above that floor and comfortably below the pre-fix ~1 per row.
MAX_ERASES_PER_ROW = 0.5

# The erase our own live region issues: `ESC[J` / `ESC[0J` (erase below the cursor),
# normally preceded by a cursor move to the pane's top row, which is where the
# `top` in the timeline comes from. `ESC[2J` is the whole-screen clear and belongs
# to another path, so it is not matched here.
WIPE = re.compile(rb"\x1b\[(?:0)?J")
CUP = re.compile(rb"\x1b\[(\d+)(?:;(\d+))?H")
# Cursor-position report, which the rebuild asks for and which has to be answered
# or the app sits there until crossterm gives up.
CPR = re.compile(rb"\x1b\[6n")
CSI = re.compile(rb"\x1b\[([0-9;?]*)([A-Za-z])")
ANSWER_MARKER = "ALPHASHEET"  # fake_pi_slow.py's first word


def has_printable(buf):
    """Anything in here that a terminal would put a glyph in a cell for."""
    stripped = re.sub(rb"\x1b\[[0-9;?]*[A-Za-z]", b"", buf)
    return any(c >= 0x20 and c != 0x7F for c in stripped)


class Cursor:
    """The row the app believes it is writing on — enough to answer `ESC[6n`.

    Deliberately not a terminal emulator. The measurements in this file need two
    numbers from the stream, the row an erase started at and the deepest row that
    received printable bytes, and both are readable off the escape sequences
    without modelling cells, attributes or scrollback.
    """

    def __init__(self, rows, cols):
        self.row, self.col = 0, 0
        self.rows, self.cols = rows, cols
        self.deepest = 0

    def feed(self, buf):
        pos = 0
        for m in CSI.finditer(buf):
            # Any printable bytes before this sequence are text at the cursor.
            self._text(buf[pos:m.start()])
            params_raw = m.group(1)
            # Private-mode sequences (`ESC[?25l`, `ESC[?1049h`, ...) carry a `?`
            # and are not cursor movement; the app uses them for the cursor and the
            # alternate screen, and neither moves the row we are tracking.
            if params_raw[:1] in (b"?", b">", b"="):
                pos = m.end()
                continue
            params = [int(p) for p in params_raw.split(b";") if p.isdigit()]
            code = m.group(2)
            if code == b"H" or code == b"f":
                self.row = (params[0] - 1) if len(params) > 0 else 0
                self.col = (params[1] - 1) if len(params) > 1 else 0
            elif code == b"A":
                self.row -= params[0] if params else 1
            elif code == b"B":
                self.row += params[0] if params else 1
            elif code == b"C":
                self.col += params[0] if params else 1
            elif code == b"D":
                self.col -= params[0] if params else 1
            self._clamp()
            pos = m.end()
        self._text(buf[pos:])

    def _text(self, buf):
        for b in buf:
            if b == 0x0D:  # CR
                self.col = 0
            elif b == 0x0A:  # LF: raw mode, so down without the CR
                self.row += 1
            elif b == 0x09:  # TAB
                self.col += 4
            elif b >= 0x20 and b != 0x7F:
                self.col += 1
                self.deepest = max(self.deepest, self.row)
        self._clamp()

    def _clamp(self):
        self.row = max(0, min(self.row, self.rows - 1))
        self.col = max(0, min(self.col, self.cols - 1))


class Driver:
    def __init__(self, rows, cols, pi_bin):
        self.master, slave = pty.openpty()
        self.rows, self.cols = rows, cols
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            LOOPRS_PI_BIN=pi_bin,
            LOOPRS_BD_BIN="/bin/echo",
            LOOPRS_SHELL_BIN="/bin/bash",
            RUST_LOG="error",
        )
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
        )
        os.close(slave)
        self.cursor = Cursor(rows, cols)
        self.chunks = []  # (arrival_time, bytes), the only record that matters
        self.lock = threading.Lock()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                t = time.time()
                with self.lock:
                    self.chunks.append((t, bytes(data)))
                    query = CPR.search(data)
                    # The tracker is fed the erase sequences too, which is how it
                    # knows the row the pane's top is on when asked.
                    self.cursor.feed(data)
                if query:
                    row = self.cursor.row
                    threading.Timer(CPR_DELAY, self._answer_cpr, args=(row,)).start()
        except OSError:
            pass

    def _answer_cpr(self, row):
        try:
            os.write(self.master, b"\x1b[%d;1R" % (row + 1))
        except OSError:
            pass

    def send(self, b):
        os.write(self.master, b)
        time.sleep(0.4)


def measure(chunks):
    """One record per erase: what row it started at and how long the hole was."""
    events = []
    for i, (t, buf) in enumerate(chunks):
        for m in WIPE.finditer(buf):
            # The top row is whatever cursor move most recently preceded the erase
            # — the app parks the cursor there before clearing, and that parked
            # row is the pane's own top.
            top = None
            for c in CUP.finditer(buf, 0, m.start()):
                top = int(c.group(1)) - 1
            if has_printable(buf[m.end():]):
                gap = 0.0  # the content came in the same read: no visible hole
            else:
                gap = None
                for t2, b2 in chunks[i + 1:]:
                    if has_printable(b2):
                        gap = (t2 - t) * 1000.0
                        break
            events.append({"chunk": i, "top": top, "gap_ms": gap})
    return [e for e in events if e["gap_ms"] is not None]


def main():
    tmp = "/tmp/flash-e2e"
    shutil.rmtree(tmp, ignore_errors=True)
    os.makedirs(tmp)
    pi = os.path.join(tmp, "pi")
    shutil.copy(FAKE_PI, pi)
    os.chmod(pi, 0o755)

    print(f"BIN={BIN}  rows={ROWS} cols={COLS}")
    if not os.path.exists(BIN):
        print(f"no binary at {BIN} — run `cargo build` first")
        return 2

    d = Driver(ROWS, COLS, pi)
    time.sleep(1.2)
    d.send(b"\t")  # Bash -> Pi
    time.sleep(0.4)
    d.send(b"stream several paragraphs please\r")
    time.sleep(RUN_SECONDS)
    d.proc.terminate()
    time.sleep(0.3)

    events = measure(d.chunks)
    gaps = [e["gap_ms"] for e in events]
    answer_rows = d.cursor.deepest  # deepest row the answer ever reached
    print(f"chunks={len(d.chunks)}  erases={len(events)}  answer reached row {answer_rows}")

    if not events:
        print("FAIL  no erase was seen, so nothing here measured anything")
        return 1

    print("\nerase timeline:")
    for e in events:
        flag = "" if e["gap_ms"] <= BLANK_BUDGET_MS else "   <-- OVER BUDGET"
        print(f"  chunk {e['chunk']:4d}  top row {str(e['top']):>4}  hole {e['gap_ms']:7.2f} ms{flag}")

    s = sorted(gaps)
    print(
        f"\nblank window: min={s[0]:.2f}  p50={s[len(s)//2]:.2f}  "
        f"p90={s[int(len(s)*.9)]:.2f}  max={s[-1]:.2f} ms"
    )

    checks = []
    over = [e for e in events if e["gap_ms"] > BLANK_BUDGET_MS]
    checks.append(
        (
            f"every erase got replacement content inside {BLANK_BUDGET_MS} ms",
            not over,
            f"{len(over)} of {len(events)} left a hole longer than that",
        )
    )
    budget = max(2.0, answer_rows * MAX_ERASES_PER_ROW)
    checks.append(
        (
            f"erases batched: {len(events)} <= {budget:.1f} (answer spread over ~{answer_rows} rows)",
            len(events) <= budget,
            f"{len(events) / max(answer_rows, 1):.2f} reshapes per row of answer",
        )
    )
    longest = max(events, key=lambda e: e["gap_ms"])
    checks.append(
        (
            f"the worst hole is short enough not to read as a blink (<{BLANK_BUDGET_MS} ms)",
            longest["gap_ms"] <= BLANK_BUDGET_MS,
            f"worst {longest['gap_ms']:.2f} ms at chunk {longest['chunk']}",
        )
    )

    print()
    failed = 0
    for name, ok, detail in checks:
        print(f"  {'PASS' if ok else 'FAIL'}  {name}   [{detail}]")
        failed += 0 if ok else 1
    print(f"\n{len(checks) - failed}/{len(checks)}")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
