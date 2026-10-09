#!/usr/bin/env python3
"""The wheel, the trackpad and the mode trio, measured in a real pty (looprs-pdl.8).

The unit tests in `src/state/wheel.rs`, `src/app.rs` and `src/teardown.rs` prove
the gesture's arithmetic and the ledger's bookkeeping against ratatui's own screen
model and a `Vec<u8>` sink. This is the same claim one level out, on a wire, with
the bytes themselves as the evidence, for the four things a unit test cannot
reach:

  1. **the three mouse modes are actually on, on the wire, exactly once each** —
     the ledger's `?1000h/?1002h/?1006h` written at startup and their `l`
     counterparts written once at the leave. A default set that contains a mode is
     not the same statement as bytes reaching a terminal, and "as one unit" has to
     be countable rather than intended.
  2. **a wheel report written as a terminal writes one moves the transcript on a
     real screen by the documented number of rows.** `ESC[<64;x;yM` is the wire's
     wheel-up; whether crossterm's parser, a real pty and the run loop agree that
     it is three rows of history is a property of the whole path.
  3. **the burst is rate-limited in the wall clock, not just in a loop that runs
     instantly.** The synthetic flick here is *timed* — 40 reports, 5 ms apart,
     200 ms of gesture — so what comes out is the number a user would see: how
     many rows a flick of that shape actually scrolls.
  4. **a still mouse costs no bytes.** Counted off the wire rather than asserted
     about a dirty flag: if the app emits nothing while the pointer is idle, it
     drew nothing.

The numbers this prints are the ones `src/state/wheel.rs` cites. Run it and read
them; if they move, the constants in that file are the thing that has to move with
them.

WHAT THIS STILL DOES NOT MEASURE, AND WHO HAS TO

    The shape of a real trackpad flick. looprs-pdl.2 #4b says "NOT MEASURED — no
    finger on this path", and a synthetic burst at a chosen cadence is a
    *cadence*, not a finger. What closes that gap is one flick's worth of
    `debug`-level logging: `WheelCadence` records every gesture's report count,
    duration and rows applied, and the log keeps it. One real flick, read with
    `grep 'wheel gesture closed' "$LOG"` (where `LOG` is the resolved log file —
    `${LOOPRS_LOG_DIR:-$HOME/.local/state/looprs}/looprs.log`, see
    `docs/guide/operator.md`), is the measurement; the constants are then
    retuned against it rather than against this file. That line is `debug`, and
    the run level is `info` by default, so a flick meant to be read back has to
    be taken under `RUST_LOG=debug`.

    cargo build
    python3 spikes/mouse_scroll_e2e.py | tee spikes/results/mouse-scroll-e2e.log
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

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from status_e2e import Screen  # noqa: E402

BIN = os.path.abspath(os.environ.get("LOOPRS_BIN", "target/debug/looprs"))
ROWS, COLS = 30, 100

# The wheel on the wire, SGR (mode 1006) encoding: code 64 is wheel-up, 65 is
# wheel-down. Written as bytes rather than imported so the terminal's own spelling
# is what is under test, exactly as the other pty drivers spell their keys.
WHEEL_UP, WHEEL_DOWN = 64, 65
MIDDLE_CLICK, RIGHT_CLICK = 2, 3

# The constants under test, from `src/state/wheel.rs`. Spelled out here so a
# change to the Rust side that does not come with a change to this file fails
# loudly on a real screen instead of quietly agreeing with itself.
ROWS_PER_STEP = 3
STEP_INTERVAL_MS = 60

# The synthetic flick: 40 reports, 5 ms apart on this side of the wire. The
# gesture's *duration* is measured, not assumed: a `sleep(0.005)` and a pty
# write are not instantaneous, so the burst that actually arrives is a little
# over 200 ms (measured ~290 ms here), and the bound the rows are checked
# against is computed from the duration the harness measured rather than from
# the nominal spacing.
#
# The rule being measured is `rows <= (floor(duration / interval) + 1) * rows_per_step`:
# one step per interval, plus the step that opened the burst.
FLICK_REPORTS = 40
FLICK_SPACING_S = 0.005


def expected_steps(duration_s):
    return int(duration_s * 1000 // STEP_INTERVAL_MS) + 1

START = time.time()
RESULTS = []


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    say(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if not ok and detail else ""))


def measured(name, value):
    say(f"      {name}: {value}")


def sgr(code, x, y):
    """One SGR 1006 mouse report, 1-based coordinates like the wire's."""
    return b"\x1b[<%d;%d;%dM" % (code, x, y)


class Driver:
    """The app in a pty, with its wire bytes and a reconstructed screen."""

    def __init__(self, rows, cols, env=None, cwd=None):
        master, slave = pty.openpty()
        self.master = master
        self.rows, self.cols = rows, cols
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        e = dict(os.environ)
        e.update(
            TERM="xterm-256color",
            LOOPRS_PI_BIN="/usr/bin/false",
            LOOPRS_BD_BIN="/bin/echo",
        )
        e.pop("LOOPRS_MODES", None)
        e.update(env or {})
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave,
            env=e, close_fds=True, cwd=cwd,
        )
        os.close(slave)
        self.lock = threading.Lock()
        self.raw = bytearray()
        self.fed = 0
        self.screen = Screen(rows, cols)
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                with self.lock:
                    self.raw.extend(data)
        except OSError:
            pass

    def sync(self):
        with self.lock:
            text = bytes(self.raw[self.fed:]).decode("utf-8", errors="replace")
            self.fed = len(self.raw)
        self.screen.feed(text)
        return self.screen

    def grid(self):
        self.sync()
        return self.screen

    def dump(self):
        return self.sync().dump()

    def wire(self):
        with self.lock:
            return bytes(self.raw)

    def bytes_now(self):
        with self.lock:
            return len(self.raw)

    def has(self, needle):
        s = self.grid()
        return any(needle in s.text(r) for r in range(s.rows))

    def row_of(self, needle):
        s = self.grid()
        for r in range(s.rows):
            if needle in s.text(r):
                return r
        return None

    def send(self, b):
        os.write(self.master, b)

    def alive(self):
        return self.proc.poll() is None

    def kill(self):
        if self.alive():
            self.proc.kill()


def wait_for(d, needle, timeout=10.0, want=True):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if d.has(needle) == want:
            return True
        time.sleep(0.05)
    return False


def into_bash(d):
    """Tab: Beads -> Pi -> Bash. Bash output arrives line-at-a-time."""
    time.sleep(1.2)
    d.send(b"\t")
    time.sleep(0.4)
    d.send(b"\t")
    time.sleep(0.8)
    return d.has("Bash")


def fill(d, n=60):
    """A transcript with a head, a tail and 60 identifiable rows."""
    d.send(
        b'for i in $(seq -w 1 %d); do echo "FLOOD$i a line of the transcript"; done\r'
        % n
    )
    return wait_for(d, "FLOOD%02d" % n, timeout=15.0)


def chrome_rows(d):
    """The rows that are chrome: the status row and everything under it.

    Found by the status row's own copy ("Tab switch" is in its help segment),
    which is the frame's honest statement of where the transcript band stops;
    the input box lives below that row by construction.
    """
    s = d.grid()
    for r in range(s.rows):
        if "Tab switch" in s.text(r):
            return list(range(r, s.rows))
    return []


def band_row(d, offset=2):
    """A row safely inside the transcript band (0-based screen rows)."""
    chrome = chrome_rows(d)
    top = min(chrome) if chrome else d.rows
    return min(offset, max(0, top - 1))


def top_marker(d):
    """The topmost visible transcript marker, as a needle.

    Chosen rather than hard-coded because the band shows whatever the tail
    happens to end at, and because the marker that scrolls the most legibly is
    the one near the top: scrolling toward the past moves content **down**, so
    a marker near the top of the band has the whole band of room to travel in
    before it can leave the frame.
    """
    s = d.grid()
    for r in range(s.rows):
        m = re.search(r"FLOOD\d\d", s.text(r))
        if m:
            return m.group(0)
    return None


def band_snapshot(d):
    """The band's rows only: chrome is excluded from every "did this move?"
    comparison, because the status row changes on its own (an elapsed timer, a
    spinner) and a test that compares it would be measuring the clock."""
    chrome = chrome_rows(d)
    last = min(chrome) if chrome else d.rows
    s = d.grid()
    return [s.text(r) for r in range(last)]


def count_mode(bytes_, n):
    """How many times `?100<n>h` / `?100<n>l` appear on the wire."""
    h = bytes_.count(b"\x1b[?100%dh" % n)
    l = bytes_.count(b"\x1b[?100%dl" % n)
    return h, l


# ───────────────────────────── groups ─────────────────────────────


def group_trio():
    """The three mouse modes come on as one unit, once, by default."""
    say("--- 1000+1002+1006 on by default, once each, off once each ---")
    d = Driver(ROWS, COLS)
    try:
        time.sleep(1.5)
        check("the app boots with no LOOPRS_MODES set", d.alive())
        wire = d.wire()
        for n, label in [(0, "?1000 (button report)"),
                        (2, "?1002 (drag report)"),
                        (6, "?1006 (SGR coordinates)")]:
            h, l = count_mode(wire, n)
            check(
                f"{label} switched on exactly once at startup",
                h == 1,
                f"found {h} on-sequences in {len(wire)} bytes",
            )
        check(
            "…and none of them switched off while the app is still running",
            all(count_mode(wire, n)[1] == 0 for n in (0, 2, 6)),
        )

        # One unit at the leave too: the ledger hands back what it took, newest
        # first, and the three mouse modes were all in what it took.
        d.send(b"\x11")  # Ctrl-Q
        deadline = time.time() + 8.0
        while time.time() < deadline and d.alive():
            time.sleep(0.1)
        check("Ctrl-Q quits", not d.alive())
        wire = d.wire()
        for n, label in [(0, "?1000"), (2, "?1002"), (6, "?1006")]:
            h, l = count_mode(wire, n)
            check(
                f"{label} switched off exactly once, at the leave",
                l == 1,
                f"found {l} off-sequences (on was {h})",
            )
    finally:
        d.kill()


def group_no_mouse():
    """`LOOPRS_MODES=-mouse` gives the pointer back and costs the app nothing."""
    say("--- LOOPRS_MODES=-mouse: the default mouse is declined, as a unit ---")
    d = Driver(ROWS, COLS, env={"LOOPRS_MODES": "-mouse"})
    try:
        time.sleep(1.5)
        check("the app boots with the mouse declined", d.alive())
        wire = d.wire()
        for n in (0, 2, 6):
            h, l = count_mode(wire, n)
            check(
                f"no ?100{n}h written (and none to take off: {l})",
                h == 0,
                f"found {h} on-sequences",
            )
        check(
            "…and the app still works: keyboard, screen, quit",
            into_bash(d),
            f"Tab did not reach Bash:\n{d.dump()}",
        )
        d.send(b"echo still-here-without-the-mouse\r")
        check(
            "…and the shell still answers",
            wait_for(d, "still-here-without-the-mouse", timeout=12.0),
            d.dump(),
        )
        d.send(b"\x11")
        time.sleep(1.5)
        check("…and it quits cleanly", not d.alive())
    finally:
        d.kill()


def group_notch():
    """One notch = three rows, through the wire."""
    say("--- one wheel notch over the band moves the transcript three rows ---")
    d = Driver(ROWS, COLS)
    try:
        check("the app reached Bash mode", into_bash(d), d.dump())
        check("60 lines of transcript are in", fill(d), d.dump())
        # Give the flush a beat so the marker row is placed before the notch.
        time.sleep(0.5)
        marker = top_marker(d)
        check(f"a transcript marker to track ({marker})", marker is not None, d.dump())
        r0 = d.row_of(marker) if marker else None
        check(f"the marker {marker} is on screen", r0 is not None, d.dump())
        if r0 is None:
            return

        row = band_row(d)
        d.send(sgr(WHEEL_UP, 10, row + 1))
        time.sleep(0.5)
        r1 = d.row_of(marker)
        moved = None if r1 is None else r1 - r0
        measured("rows the notch moved", moved)
        check(
            f"a notch moved exactly {ROWS_PER_STEP} rows (toward the past)",
            moved == ROWS_PER_STEP,
            f"{r0} -> {r1}\n{d.dump()}",
        )

        # A notch back toward the tail returns the same three rows.
        time.sleep(STEP_INTERVAL_MS / 1000.0 + 0.05)
        d.send(sgr(WHEEL_DOWN, 10, row + 1))
        time.sleep(0.5)
        r2 = d.row_of(marker)
        back = None if r2 is None else r2 - r1
        measured("rows the return notch moved", back)
        check(
            "a notch toward the tail moved the same three rows back",
            back == -ROWS_PER_STEP,
            f"{r1} -> {r2}",
        )
    finally:
        d.kill()


def group_flick():
    """A 200 ms burst scrolls the rate-limited distance, not one row a report."""
    say("--- a trackpad-shaped burst is rate-limited in the wall clock ---")
    d = Driver(ROWS, COLS)
    try:
        check("the app reached Bash mode", into_bash(d), d.dump())
        check("60 lines of transcript are in", fill(d), d.dump())
        time.sleep(0.5)
        marker = top_marker(d)
        check(f"a transcript marker to track ({marker})", marker is not None, d.dump())
        r0 = d.row_of(marker) if marker else None
        check(f"the marker {marker} is on screen", r0 is not None, d.dump())
        if r0 is None:
            return

        row = band_row(d)
        t0 = time.time()
        for _ in range(FLICK_REPORTS):
            d.send(sgr(WHEEL_UP, 10, row + 1))
            time.sleep(FLICK_SPACING_S)
        gesture = time.time() - t0
        time.sleep(0.5)
        r1 = d.row_of(marker)
        moved = None if r1 is None else r1 - r0
        measured("reports injected", FLICK_REPORTS)
        measured("gesture duration", f"{gesture * 1000:.0f} ms")
        measured("rows scrolled", moved)
        measured("rows per second", "n/a" if not moved else
                f"{(moved or 0) / gesture:.1f}")

        check(
            f"the burst moved more than one notch's worth (so the reports arrived)",
            moved is not None and moved > ROWS_PER_STEP,
            f"moved {moved}; one notch is {ROWS_PER_STEP}",
        )
        bound = expected_steps(gesture) * ROWS_PER_STEP
        measured("bound the rate admits for that duration",
                f"{expected_steps(gesture)} steps x {ROWS_PER_STEP} = {bound} rows")
        check(
            f"…and no more than the rate admits ({bound} rows)",
            moved is not None and moved <= bound,
            f"moved {moved}",
        )
        check(
            f"…and nowhere near one row per report ({FLICK_REPORTS} rows)",
            moved is not None and moved < FLICK_REPORTS,
            f"moved {moved}",
        )
    finally:
        d.kill()


def group_chrome():
    """Over the input box and the status row the transcript does not move."""
    say("--- events over the input box / status band do not scroll the transcript ---")
    d = Driver(ROWS, COLS)
    try:
        check("the app reached Bash mode", into_bash(d), d.dump())
        check("60 lines of transcript are in", fill(d), d.dump())
        time.sleep(0.5)
        chrome = chrome_rows(d)
        check("chrome rows were found", len(chrome) >= 2, f"{chrome}\n{d.dump()}")
        if len(chrome) < 2:
            return

        before = band_snapshot(d)
        for name, row in [("status row", chrome[0]),
                         ("input box", chrome[-1])]:
            for code in (WHEEL_UP, WHEEL_DOWN):
                d.send(sgr(code, 10, row + 1))
                time.sleep(0.12)
            time.sleep(0.4)
            after = band_snapshot(d)
            moved = [i for i, (a, b) in enumerate(zip(before, after)) if a != b]
            check(
                f"wheel over the {name} scrolled nothing",
                not moved,
                f"rows {moved} changed:\n{d.dump()}",
            )

        # And the band still answers, so the checks above are not passing
        # because the wheel is dead on this wire.
        marker = top_marker(d)
        r0 = d.row_of(marker) if marker else None
        d.send(sgr(WHEEL_UP, 10, band_row(d) + 1))
        time.sleep(0.5)
        r1 = d.row_of(marker)
        check(
            "…while the same report over the band still scrolls it",
            r0 is not None and r1 is not None and r1 - r0 == ROWS_PER_STEP,
            f"{r0} -> {r1}\n{d.dump()}",
        )
    finally:
        d.kill()


def group_idle_cost():
    """A still mouse costs no bytes, which is the same as costing no frames."""
    say("--- no idle frame cost when the mouse is still ---")
    d = Driver(ROWS, COLS)
    try:
        check("the app reached Bash mode", into_bash(d), d.dump())
        check("60 lines of transcript are in", fill(d), d.dump())
        time.sleep(1.0)
        baseline = d.bytes_now()
        time.sleep(2.0)
        quiet = d.bytes_now() - baseline
        measured("bytes emitted in 2s with the mouse still", quiet)
        check("a still mouse emitted nothing", quiet == 0, f"{quiet} bytes")

        # While it is *not* still the app does have to draw: the same window on
        # the wire with a notch in it is non-empty. Without this, the check
        # above passes for "nothing ever draws".
        d.send(sgr(WHEEL_UP, 10, band_row(d) + 1))
        time.sleep(0.6)
        busy = d.bytes_now() - baseline
        measured("bytes emitted over the same window with one notch", busy)
        check("…and the wheel's own work does reach the wire", busy > 0)

        # Settled again: after a burst has been drained, nothing is owed and
        # nothing is written.
        time.sleep(1.0)
        again = d.bytes_now()
        time.sleep(2.0)
        tail = d.bytes_now() - again
        measured("bytes emitted in 2s after the gesture drained", tail)
        check("…and nothing is owed after the gesture drained", tail == 0,
              f"{tail} bytes")
    finally:
        d.kill()


def group_unbound():
    """Right- and middle-click are not silently swallowed."""
    say("--- buttons other than the wheel are reported, not swallowed ---")
    tmp = tempfile.mkdtemp(prefix="looprs-mouse-scroll-")
    # The log destination is a knob now (looprs-00u.13), so the spike that reads
    # the log names it: `LOOPRS_LOG_DIR` points the app at this scratch dir, and
    # the file read below is the file this run wrote. Before the knob existed this
    # check looked for `looprs.log` in a cwd the binary never wrote to (it used
    # `temp_dir()`), so it could only ever report zero hits.
    d = Driver(ROWS, COLS, cwd=tmp, env={"LOOPRS_LOG_DIR": tmp})
    try:
        check("the app reached Bash mode", into_bash(d), d.dump())
        check("60 lines of transcript are in", fill(d), d.dump())
        time.sleep(0.5)
        row = band_row(d)
        before = band_snapshot(d)

        d.send(sgr(MIDDLE_CLICK, 10, row + 1))
        d.send(b"\x1b[<2;10;%dm" % (row + 1))
        d.send(sgr(RIGHT_CLICK, 10, row + 1))
        d.send(b"\x1b[<3;10;%dm" % (row + 1))
        time.sleep(0.6)

        after = band_snapshot(d)
        moved = [i for i, (a, b) in enumerate(zip(before, after)) if a != b]
        check(
            "a middle-click or right-click scrolled nothing",
            not moved,
            f"rows {moved} changed:\n{d.dump()}",
        )

        # The transcript did not move, but the app said so: that is the
        # difference between "no binding" and "swallowed".
        log = os.path.join(tmp, "looprs.log")
        time.sleep(0.3)
        text = ""
        if os.path.exists(log):
            with open(log, "r", errors="replace") as f:
                text = f.read()
        hits = text.count("captured with no binding")
        measured("log lines naming the unbound buttons", hits)
        check(
            "…and the app logged each one, with the button it was",
            hits >= 2,
            f"{hits} lines; log at {log}",
        )
        check("…naming middle-click as one of them",
              "Middle" in text or "Right" in text, text[-2000:])
    finally:
        d.kill()
        shutil.rmtree(tmp, ignore_errors=True)


def main():
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2

    group_trio()
    group_no_mouse()
    group_notch()
    group_flick()
    group_chrome()
    group_idle_cost()
    group_unbound()

    ok = sum(1 for _, v in RESULTS if v)
    total = len(RESULTS)
    say(f"{ok}/{total} checks passed")
    return 0 if ok == total else 1


if __name__ == "__main__":
    sys.exit(main())
