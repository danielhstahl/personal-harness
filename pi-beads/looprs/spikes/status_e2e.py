#!/usr/bin/env python3
"""The status row, in a real pty, against the real state machines (looprs-guh).

Three layers already vouch for this row, each one a step closer to the bug:
`status.rs`'s tests prove the ladder (the order segments drop in, the truncation,
the 40-column floor) against the row's own data model; `main.rs`'s TestBackend
tests prove the row is painted into the band `viewport::frame_areas` reserves —
that the band is not empty, with and without an input box, at heights 5 through 20.
This spike adds what neither can see: **the shipped binary, in a real pty, drives
that row off real state machines** — the beads loop claiming and working a bead, a
`bd` that fails outright, a `sleep` holding a shell busy, and a Tab that leaves a
session running off-screen, which is ADR-0002's "load-bearing, not cosmetic"
sentence made checkable.

Costs no model call: `pi` is `spikes/fake_pi_slow.py` (one streamed assistant
message held open) and `bd` is a bash fake this script writes.

    cargo build
    python3 spikes/status_e2e.py | tee spikes/results/status-e2e.log

    # The same script against the pre-looprs-guh binary, as a control. It must
    # find nothing; that nothing is what makes the passing run mean anything:
    #   git worktree add --detach /tmp/looprs-prefix HEAD~1
    #   (cd /tmp/looprs-prefix/pi-beads/looprs && cargo build --target-dir /tmp/base-target)
    #   LOOPRS_BIN=/tmp/base-target/debug/looprs python3 spikes/status_e2e.py --control

What a row can be measured as here, after three ways of trying it failed. The
obvious move — grep the capture for `"Beads · awaiting input"` — cannot work,
because ratatui renders by **diff** and a frame rewrites only the cells that
changed: observed directly, the frame after the board came back empty wrote `●` at
column 1 and `awaiting input · Tab switch ·` at column 11 and nothing in between.
Nor can a cell-grid emulator stand in as the screen: the live region *moves*
(`insert_before` scrolls the terminal inside a DECSTBM region) and the app places
that region from `ESC[6n` cursor queries this harness has to answer for it, so any
disagreement leaves the previous frame's box borders underneath the row under test
and the replay reads rows that were never on screen. (Both attempted; both
produced confident garbage before they produced anything honest.)

So the checks are **row-only words, in fresh windows**:

  * *Row-only.* Every needle is a word or bigram that exists nowhere else in the
    program's output — `Esc cancel`, `Tab switch`, `^C quit`, `bg: `, `warm: `,
    and the verbs. The beads loop's transcript prose is deliberately different at
    exactly those points, which is what makes the match unambiguous: the transcript
    says "beads: … the loop is parked", the row says `paused`; the transcript
    says "beads: working looprs-…", the row says `bg: Beads working`. A needle
    like the **mode name** is deliberately not used: an unchanged cell is not
    repainted, so the name is often simply absent from the frame that changed the
    verb next to it, and "which mode" is a claim `main.rs`'s render tests make
    from a real buffer instead.
  * *Fresh windows.* Each check reads what the app painted since its own mark
    (`Driver.since`), never the whole capture, so a frame painted at boot cannot
    satisfy a claim about the state under test. While a session is busy the row
    repaints on every spinner tick; on a state change it repaints once, which the
    polling covers.

Line width, reflow and the drop order are not measured here — a terminal's line
model cannot be recovered from this byte stream for the reason above. They are
`status.rs`'s width tests and `main.rs`'s TestBackend tests, where every cell can
be checked exactly.
"""

import argparse
import fcntl
import json
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
FAKE_PI = "spikes/fake_pi_slow.py"
RESULTS = []
START = time.time()

ROWS, COLS = 24, 90
NARROW = 40  # the width the ticket names as its floor
BEAD_ID = "looprs-zz9"


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


# Three kinds of check, because the --control pass needs to know which ones are
# allowed to pass on a binary that has no status row:
#   "row" — a positive claim about the row's own text. These are the measurements;
#           on the control binary every one of them must fail.
#   "abs" — a negative claim ("never claims a warm Pi"). Vacuously true with no
#           row at all, so it carries no weight in the control.
#   "app" — about the app rather than the row (still alive, alt screen untouched),
#           legitimately true before the feature too.
def check(name, ok, detail="", kind="row"):
    RESULTS.append((name, ok, kind))
    mark = "PASS" if ok else "FAIL"
    say(f"{mark}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def strip_ansi(text):
    """Drop every escape sequence; only printable content is ever a needle."""
    return re.sub(
        r"\x1b\[[0-9;?]*[A-Za-z]|\x1b][^\x07]*\x07|\x1b[()][0-9A-Za-z]", "", text
    )


def _wide(ch):
    o = ord(ch)
    return (
        0x1100 <= o <= 0x115F
        or 0x2E80 <= o <= 0xA4CF
        or 0xAC00 <= o <= 0xD7A3
        or 0xF900 <= o <= 0xFAFF
        or 0xFF01 <= o <= 0xFF60
    )


class Screen:
    """The grid the app is painting: CUP-addressed cells, scrolled as it scrolls.

    Needed because the byte stream cannot be read directly: ratatui diffs, so a
    frame writes only the cells that changed, and once the row's content shifts by
    one character the stream's order is no longer the screen's order. Observed — a
    repaint of the `Esc cancel` hint reached the wire as `Esc can` and `el`, with
    the `c` absent, because that cell had held the same glyph since an earlier
    frame and needed no rewrite. Grep the stream for `"Esc cancel"` and it fails on
    a screen that is displaying exactly that.

    This is the subset of the terminal the app actually uses: CUP, CUU/CUD/CUF/CUB,
    CR/LF with autowrap, erase display/line, and the scroll region (`ESC[r`) with
    SU/SD/IL/DL, which is how `insert_before` makes room for the live region. The
    scroll sequences are not optional decoration: replayed without them, the grid
    keeps the previous frame's box borders under the row under test and reads back
    a row that was never on screen.
    """

    BLANK = " "
    _CSI = re.compile(r"\x1b\[([0-9;?]*)([A-Za-z])")

    def __init__(self, rows, cols):
        self.rows = rows
        self.cols = cols
        self.grid = [[self.BLANK] * cols for _ in range(rows)]
        self.row = 0
        self.col = 0
        self.top = 0
        self.bottom = rows - 1

    def resize(self, rows, cols):
        self.__init__(rows, cols)

    def put(self, ch):
        if 0 <= self.row < self.rows and 0 <= self.col < self.cols:
            self.grid[self.row][self.col] = ch
        self.col += 2 if _wide(ch) else 1
        if self.col >= self.cols:
            self.col = 0
            self._linefeed()

    def _linefeed(self):
        if self.row == self.bottom:
            self._shift(self.top, self.bottom, up=True, n=1)
        elif self.row < self.rows - 1:
            self.row += 1

    def _shift(self, top, bottom, up, n):
        n = max(1, min(n, bottom - top))
        if up:
            for i in range(top, bottom - n + 1):
                self.grid[i] = self.grid[i + n]
            for i in range(bottom - n + 1, bottom + 1):
                self.grid[i] = [self.BLANK] * self.cols
        else:
            for i in range(bottom, top + n - 1, -1):
                self.grid[i] = self.grid[i - n]
            for i in range(top, top + n):
                self.grid[i] = [self.BLANK] * self.cols

    def feed(self, text):
        i, n = 0, len(text)
        while i < n:
            c = text[i]
            if c == "\x1b":
                i += self._esc(text, i)
            elif c == "\r":
                self.col = 0
                i += 1
            elif c == "\n":
                self._linefeed()
                i += 1
            elif c == "\b":
                self.col = max(0, self.col - 1)
                i += 1
            elif ord(c) < 32:
                i += 1
            else:
                self.put(c)
                i += 1

    def _esc(self, text, i):
        if i + 1 >= len(text):
            return 1
        nxt = text[i + 1]
        if nxt != "[":
            return 3 if i + 2 < len(text) and nxt in "(#" else 2
        m = self._CSI.match(text, i)
        if not m:
            return 2
        self._csi(m.group(1), m.group(2))
        return m.end() - i

    def _csi(self, params, final):
        if params.startswith("?"):
            return  # private modes: alt screen, cursor visibility — no cells
        p = [int(x) for x in params.split(";") if x.isdigit()]

        def arg(k, d):
            return p[k] if len(p) > k else d

        if final in ("H", "f"):
            self.row = max(0, arg(0, 1) - 1)
            self.col = max(0, arg(1, 1) - 1)
        elif final == "A":
            self.row = max(0, self.row - max(1, arg(0, 1)))
        elif final == "B":
            self.row = min(self.rows - 1, self.row + max(1, arg(0, 1)))
        elif final == "C":
            self.col = min(self.cols - 1, self.col + max(1, arg(0, 1)))
        elif final == "D":
            self.col = max(0, self.col - max(1, arg(0, 1)))
        elif final == "d":
            self.row = max(0, arg(0, 1) - 1)
        elif final == "`":
            self.col = max(0, arg(0, 1) - 1)
        elif final == "J":
            mode = arg(0, 0)
            if mode == 2:
                self.grid = [[self.BLANK] * self.cols for _ in range(self.rows)]
            elif mode == 0:
                for cc in range(self.col, self.cols):
                    self.grid[self.row][cc] = self.BLANK
                for r in range(self.row + 1, self.rows):
                    self.grid[r] = [self.BLANK] * self.cols
            elif mode == 1:
                for r in range(0, self.row):
                    self.grid[r] = [self.BLANK] * self.cols
                for cc in range(0, min(self.col + 1, self.cols)):
                    self.grid[self.row][cc] = self.BLANK
        elif final == "K":
            mode = arg(0, 0)
            rng = (
                range(self.col, self.cols)
                if mode == 0
                else (range(0, min(self.col + 1, self.cols)) if mode == 1 else range(self.cols))
            )
            for cc in rng:
                if 0 <= self.row < self.rows:
                    self.grid[self.row][cc] = self.BLANK
        elif final == "r":  # DECSTBM: the scroll region
            self.top = max(0, arg(0, 1) - 1)
            self.bottom = min(self.rows - 1, arg(1, self.rows) - 1)
            if self.bottom <= self.top:
                self.top, self.bottom = 0, self.rows - 1
            self.row, self.col = self.top, 0
        elif final == "T":  # SU
            self._shift(self.top, self.bottom, up=True, n=max(1, arg(0, 1)))
        elif final == "S":  # SD
            self._shift(self.top, self.bottom, up=False, n=max(1, arg(0, 1)))
        elif final == "L":  # IL
            self._shift(self.row, self.bottom, up=False, n=max(1, arg(0, 1)))
        elif final == "M":  # DL
            self._shift(self.row, self.bottom, up=True, n=max(1, arg(0, 1)))
        # "m" (SGR) and unknown finals move no cells.

    def text(self, r):
        return "".join(self.grid[r]).rstrip() if 0 <= r < self.rows else ""

    def dump(self):
        return "\n".join(f"{i:3d}|{self.text(i)}" for i in range(self.rows))



class Fakes:
    """A `bd` whose answers are files, flipped between runs without a rebuild."""

    def __init__(self, tmp):
        self.tmp = tmp
        self.board = os.path.join(tmp, "board.json")
        self.settled = os.path.join(tmp, "settled.json")
        self.claimed = os.path.join(tmp, "claimed")
        self.fail = os.path.join(tmp, "bd-fail")
        self.bin = os.path.join(tmp, "bd")
        self.set_board("[]")
        self.set_settled("[]")
        with open(self.bin, "w") as f:
            f.write(
                """#!/usr/bin/env bash
# Fake `bd` for the status-row spike. Its answers are files the driver rewrites,
# so "the board has a bead", "the board is empty" and "bd is down" are three runs
# of one binary rather than three binaries.
#
# `ready` answers from the board only while the bead is unclaimed, which is real
# `bd` behaviour and which the spike needs: once `update --claim` has run, the
# bead is no longer ready. A board that kept offering the same id would park the
# loop on beads' "already worked this" refusal, and that error would stand in for
# the `working` row this spike is trying to see.
#
# `show` answers from the *settled* file: beads verifies a finished pass with
# `bd show <id>` and treats a bead still reported `open` as the worker having lied
# — likewise a parked loop for a reason this spike is not testing.
set -u
if [ -e "%(fail)s" ]; then
  echo "fake bd: the database is locked (fail marker set)" >&2
  exit 3
fi
verb="${1:-}"
case "$verb" in
  ready|list)
    if [ ! -e "%(claimed)s" ]; then cat "%(board)s"; else echo "[]"; fi
    ;;
  show) cat "%(settled)s" ;;
  update|create|close)
    touch "%(claimed)s"
    echo '{"ok":true}'
    ;;
  *) echo "fake bd: unsupported verb $verb" >&2; exit 2 ;;
esac
"""
                % {
                    "fail": self.fail,
                    "board": self.board,
                    "settled": self.settled,
                    "claimed": self.claimed,
                }
            )
        os.chmod(self.bin, 0o755)

    def set_board(self, js):
        with open(self.board, "w") as f:
            f.write(js)

    def set_settled(self, js):
        with open(self.settled, "w") as f:
            f.write(js)

    def bead(self, title="the spike bead nobody else can claim"):
        """One ready bead that reports itself closed once the pass is verified."""
        self.set_board(
            json.dumps(
                [{"id": BEAD_ID, "title": title, "status": "open", "issue_type": "task"}]
            )
        )
        self.set_settled(
            json.dumps(
                [{"id": BEAD_ID, "title": title, "status": "closed", "issue_type": "task"}]
            )
        )

    def down(self):
        open(self.fail, "w").close()


class Driver:
    """One `looprs` in its own pty, behind fake `bd`/`pi`."""

    def __init__(self, fakes, rows=ROWS, cols=COLS, hold="180"):
        self.master, slave = pty.openpty()
        self.rows, self.cols = rows, cols
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        pi = os.path.join(fakes.tmp, "pi")
        shutil.copy(FAKE_PI, pi)
        os.chmod(pi, 0o755)
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            LOOPRS_PI_BIN=pi,
            LOOPRS_BD_BIN=fakes.bin,
            LOOPRS_SHELL_BIN=os.environ.get("LOOPRS_SHELL_BIN", "/bin/bash"),
            LOOPRS_FAKE_HOLD=hold,
        )
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
        )
        os.close(slave)
        self.lock = threading.Lock()
        self.raw = bytearray()
        self.consumed = 0
        self.screen = Screen(rows, cols)
        threading.Thread(target=self._pump, daemon=True).start()
        say(f"  spawned pid {self.proc.pid} in a {rows}x{cols} pty")

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                chunk = data.decode("utf-8", errors="replace")
                with self.lock:
                    self.raw.extend(data)
                    # The grid is fed here, as the bytes arrive, rather than on
                    # demand: the app asks `ESC[6n` where it is before placing its
                    # live region, so the answer decides where the app draws, and
                    # a stale cursor makes the app lay itself out somewhere the
                    # replay cannot follow. Feeding on the way in keeps the cursor
                    # the answer is taken from current with what the app has sent.
                    self.screen.feed(chunk)
                    row = self.screen.row + 1
                for _ in range(chunk.count("\x1b[6n")):
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

    def tail(self, nbytes=8000):
        """The last `nbytes` of the wire, style stripped.

        Reading a *window per poll* fails for a static row: an idle row repaints
        once and then not at all, so the frames that said the thing are behind the
        window and the check is unsatisfiable for the rest of the run — and if a
        window boundary happens to fall inside the needle, even the frame that
        said it is missed. The tail is fresh in the sense that matters: it is what
        the app most recently sent, and while a session is busy the row repaints on
        every spinner tick, so nothing important ever ages out of 8 KB.
        """
        with self.lock:
            blob = bytes(self.raw[-nbytes:]).decode("utf-8", errors="replace")
        return strip_ansi(blob).replace("\r", "")

    def send(self, b):
        os.write(self.master, b)

    def resize(self, rows, cols):
        self.rows, self.cols = rows, cols
        fcntl.ioctl(
            self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)
        )
        with self.lock:
            self.screen.resize(rows, cols)
            self.consumed = len(self.raw)
        say(f"  window resized to {rows}x{cols}")

    def sync(self):
        """No-op: the grid is fed on the way in. Kept so callers read as intent."""
        return None

    def alive(self):
        return self.proc.poll() is None

    def quit(self):
        self.send(b"\x11")  # Ctrl-Q: leave without touching anything else
        t0 = time.time()
        while self.alive() and time.time() - t0 < 8:
            time.sleep(0.1)
        return not self.alive()

    def dump(self, tag):
        path = f"/tmp/status-e2e-{tag}.log"
        with open(path, "w") as f:
            f.write("plain text (read for colour, never for geometry):\n\n")
            f.write(strip_ansi(self.text()) + "\n\n\nraw capture:\n\n")
            f.write(self.text())
        say(f"  capture -> {path}")

    def full_paint(self):
        """Force the whole screen to be redrawn, then return the recent wire.

        A diff renderer means a needle can be *unreadable* even while it is on the
        screen: a cell that already held the glyph the frame wants is not rewritten,
        so the wire carried `Esc can` and `el` for a row showing `Esc cancel`.
        Resizing makes the redraw non-partial. Nudge two columns out and back so
        the second size is not one the app has already painted at.
        """
        cols = self.cols
        self.resize(self.rows, cols + 2)
        time.sleep(0.4)
        self.resize(self.rows, cols)
        time.sleep(0.6)
        return self.tail()

    def status_row(self):
        """`(row_index, text)` for the status row: the row above the input box.

        `frame_areas` stacks [text, tools, status, input], and the input box's top
        border is the only thing on the screen that opens a line with `┌`, so the
        band is found by position — the same relationship `main.rs` asserts in the
        model, read here off the real wire.
        """
        self.sync()
        for r in range(self.screen.rows):
            if self.screen.text(r).startswith("\u250c"):
                return r - 1, self.screen.text(r - 1)
        return None, ""

    def tail(self, nbytes=12000):
        """The recent wire, style stripped. This is what the checks read.

        A window-per-poll read fails on a static row: an idle row repaints once
        and then not at all, so the frame that said the thing slides out of the
        window and the check is unsatisfiable for the rest of the run. The tail is
        fresh in the sense that matters — it is the most recent thing the app sent,
        and while a session is busy the row repaints on every spinner tick.
        """
        with self.lock:
            blob = bytes(self.raw[-nbytes:]).decode("utf-8", errors="replace")
        return strip_ansi(blob).replace("\r", "")

    @staticmethod
    def hits(needles, window):
        """Every needle present? A needle starting `re:` is a regex, else a substring."""
        for n in needles:
            if n.startswith("re:"):
                if not re.search(n[3:], window):
                    return False
            elif n not in window:
                return False
        return True

    def await_row(self, needles, timeout=12.0, force=False):
        """Poll the recent wire until it contains every needle.

        `force=True` adds a resize-nudge to each attempt, so a needle that is on
        the screen but split across cells by the diff renderer still reads back.
        Costs ~1s per attempt; used where the check is about a specific string.
        """
        deadline = time.time() + timeout
        window = ""
        while time.time() < deadline:
            window = self.full_paint() if force else self.tail()
            if self.hits(needles, window):
                return True, window
            time.sleep(0.1)
        return False, window

    def assert_absent(self, needles):
        """A negative about this run: no needle may appear anywhere in the capture.

        Deliberately the whole run rather than a window. "Never in this scenario did
        the row claim a warm Pi" is the claim, and a window could not say it —
        though it does mean a needle that appeared *anywhere* fails the check, so
        the needles have to be as row-only as the positive ones.
        """
        whole = strip_ansi(self.text()).replace("\r", "")
        return [n for n in needles if n in whole]


def scenario_empty_board():
    say("\n== S1: boot on an empty board ==")
    fakes = Fakes(tempfile.mkdtemp(prefix="looprs-status-1-"))
    d = Driver(fakes)
    time.sleep(1.0)
    check("S1 the app boots", d.alive(), kind="app")
    ok, w = d.await_row(["awaiting input · Tab switch"])
    check("S1 the row says the loop is waiting for a human", ok, w)
    ok, w = d.await_row(["Tab switch · ^C quit"], timeout=6)
    check("S1 the row carries both key hints together", ok, w)
    # Premise changed by looprs-pdl.4: this check used to prove the app never
    # touched the alternate screen, which was true of the inline pane and is the
    # opposite of the full-screen frame (ADR-0004 R1). What is true now, and what
    # this is asked to keep true, is that the frame takes that screen **once** at
    # startup — a second `?1049h` mid-run would re-save the user's own contents
    # as their main screen. The matching leave is proved by `shutdown_e2e.py`.
    enters = d.text().count("\x1b[?1049h")
    check("S1 the app took the alternate screen exactly once", enters == 1,
          f"{enters} `?1049h` in the capture", kind="app")
    d.dump("empty-board")
    d.quit()


def scenario_bd_down():
    say("\n== S2: `bd` is down ==")
    fakes = Fakes(tempfile.mkdtemp(prefix="looprs-status-2-"))
    fakes.down()
    d = Driver(fakes)
    ok, w = d.await_row(["paused · ✗"], timeout=20)
    check("S2 the failure reaches the row, not only the scrollback", ok, w)
    d.dump("bd-down")
    d.quit()


def scenario_background_work():
    say("\n== S3: beads working, then Tab away to Pi (ADR-0002) ==")
    fakes = Fakes(tempfile.mkdtemp(prefix="looprs-status-3-"))
    fakes.bead()
    d = Driver(fakes)
    ok, w = d.await_row([f"working · {BEAD_ID}"], timeout=16)
    check("S3 the row names the bead the loop is paying for", ok, w)
    ok, w = d.await_row(["Esc cancel"], timeout=10, force=True)
    check("S3 `Esc cancel` is offered while the pass runs", ok, w)

    # Tab while the pass is still open. Re-sent until it lands: this pty's input
    # carries the driver's cursor-query answers as well as the keystrokes, and one
    # can be swallowed on the way in.
    for _ in range(3):
        d.send(b"\t")
        ok, w = d.await_row(["bg: Beads working"], timeout=5)
        if ok:
            break
    check("S3 with Pi focused, the row still says beads is working", ok, w)
    check(
        "S3 the beads pass is not relabelled as the focused mode's own work",
        "bg: Pi" not in w,
        kind="abs",
        detail=w,
    )
    # The id is matched *with the elapsed that follows it in its own segment*, not
    # on its own: the transcript says `beads: working looprs-zz9` about the same
    # bead, and a bare-id needle passes on that prose with no row at all. The
    # control run caught exactly that.
    ok, w = d.await_row([rf"re:{BEAD_ID}\s+\d+[ms]"], timeout=10, force=True)
    check(
        "S3 the background segment carries the bead id and its own elapsed",
        ok,
        w,
    )
    d.dump("background")
    d.quit()


def scenario_bash_liveness():
    say("\n== S4: the shell's own liveness ==")
    fakes = Fakes(tempfile.mkdtemp(prefix="looprs-status-4-"))
    d = Driver(fakes)
    time.sleep(1.0)
    for _ in range(3):
        d.send(b"\t\t")  # Beads -> Pi -> Bash
        ok, w = d.await_row(["not started · warm"], timeout=4)
        if ok:
            break
    check("S4 the row reaches the Bash mode by Tab", ok, w)

    d.send(b"sleep 6\r")
    time.sleep(0.8)
    ok, w = d.await_row(["running"], timeout=8)
    check("S4 the row answers 'is the shell working?'", ok, w)
    ok, w = d.await_row(["Esc cancel"], timeout=8, force=True)
    check("S4 …and offers the key that stops it", ok, w)

    time.sleep(7.0)
    ok, w = d.await_row(["idle"], timeout=15)
    check("S4 the row settles back to idle when the command ends", ok, w)
    hits = d.assert_absent(["warm: Pi"])
    check(
        "S4 Pi was only Tabbed through, so it is not claimed as a warm child",
        not hits,
        kind="abs",
        detail=f"found {hits}",
    )
    d.dump("bash-liveness")
    d.quit()


def scenario_narrow():
    say("\n== S5: a 40-column window, mid-run ==")
    fakes = Fakes(tempfile.mkdtemp(prefix="looprs-status-5-"))
    d = Driver(fakes, cols=72)
    time.sleep(1.0)
    for _ in range(3):
        d.send(b"\t\t")  # Beads -> Pi -> Bash
        ok, w = d.await_row(["not started · warm"], timeout=4)
        if ok:
            break
    if not ok:
        check("S5 reaches Bash to set up", False, w)
        d.quit()
        return
    # `sleep 400` rather than a beads pass: the fake `pi` stream ends after ~5s,
    # which is not long enough to straddle a resize and the checks after it, and a
    # state that has already returned reads as "the row survived" when all that
    # happened is that the row went idle. A shell holding the run open is a state
    # this harness controls end to end.
    d.send(b"sleep 400\r")
    time.sleep(1.0)
    # force=True: the hint can sit on screen split across cells by the diff
    # renderer (observed `Esc canel`), which only a non-partial redraw fixes.
    ok, w = d.await_row(["running", "Esc cancel"], timeout=8, force=True)
    check("S5 running at 72 columns (the setup)", ok, w)
    d.resize(NARROW, NARROW)
    check("S5 the app survives a resize to 40 columns", d.alive(), kind="app")
    ok, w = d.await_row(["running", "Esc cancel"], timeout=15)
    check("S5 the row is still painted, and still the running row, at 40 columns",
          ok, w)
    d.dump("narrow")
    d.quit()


def scenario_narrow_error():
    say("\n== S6: a 40-column window with a long error ==")
    # The truncation worst case: the row's longest segment is on, at the width the
    # ticket names as its floor. `bd` failing gives an error sentence longer than
    # the whole row.
    fakes = Fakes(tempfile.mkdtemp(prefix="looprs-status-6-"))
    fakes.down()
    d = Driver(fakes, cols=NARROW)
    ok, w = d.await_row(["paused · ✗"], timeout=20)
    check("S6 the error row reaches the 40-column band without vanishing", ok, w)
    check("S6 the app is still alive at the floor width", d.alive(), kind="app")
    d.dump("narrow-error")
    d.quit()


SCENARIOS = (
    scenario_empty_board,
    scenario_bd_down,
    scenario_background_work,
    scenario_bash_liveness,
    scenario_narrow,
    scenario_narrow_error,
)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--control",
        action="store_true",
        help="run against a binary expected NOT to have the row; invert the verdict",
    )
    args = ap.parse_args()

    for path, why in ((BIN, "run `cargo build` first"), (FAKE_PI, "missing fixture")):
        if not os.path.exists(path):
            say(f"{why}: {path}")
            return 2

    for fn in SCENARIOS:
        try:
            fn()
        except Exception as exc:  # keep going; report loudly rather than silently
            check(f"{fn.__name__} completed", False, repr(exc))

    failed = [n for n, ok, _k in RESULTS if not ok]
    passed = len(RESULTS) - len(failed)
    say(f"{passed}/{len(RESULTS)} checks passed  (binary: {BIN})")
    if failed:
        say("failed: " + "; ".join(failed))
    if not args.control:
        return 1 if failed else 0

    # The control: the same checks against a binary from before this feature.
    informative = [(n, ok) for n, ok, k in RESULTS if k == "row"]
    leaked = [n for n, ok in informative if ok]
    say(f"control: {len(informative)} row-specific checks ran; "
        f"{len(leaked)} of them passed against a binary with no status row")
    if leaked:
        say("CONTROL BROKEN — these needles are not row-specific: " + "; ".join(leaked))
        return 1
    say("CONTROL OK: no row-specific check fires on the pre-feature binary, so the "
        "passing run above measures the row and not merely a running app")
    return 0


if __name__ == "__main__":
    sys.exit(main())
