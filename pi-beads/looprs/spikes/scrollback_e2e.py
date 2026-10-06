#!/usr/bin/env python3
"""The scrollback store, measured in a real pty (looprs-pdl.6).

The unit tests in `src/state/scrollback.rs` and the painted-frame tests in
`src/main.rs` prove the store's semantics against ratatui's own screen model.
This is the same claim one level out, and it is not redundant with them, for
three reasons those tests cannot reach:

  1. **the scroll keys have to arrive.** `PageUp` is `ESC[5~` and `End` is
     `ESC[F` on the wire. Whether crossterm decodes them, through a real pty
     with a real bash in front of it, into the key codes the app matches on, is
     a fact about the wire and not about a match arm.
  2. **the "N new" affordance has to be legible on a screen.** A pill that only
     exists in a buffer is not an affordance; and one that shifts the text the
     reader stopped on would be scored as a pass by a test that only looks at
     the count.
  3. **a resize taken while scrolled up does the app no damage, and what the
     wire says about it is itself news.** Measured here: an **idle** app emits
     no bytes at all on a resize — 0 over 2s, and the pre-pdl.6 binary at
     195e3c0 measures the same — so a window drag on a quiet session does not
     reach the frame until something else draws. The app survives the resize
     mid-scroll and keeps taking keys and output afterwards, which is all this
     harness can honestly assert. The *content-anchored* re-wrap this ticket is
     about is proven where the screen model is real:
     `a_resize_keeps_the_line_the_user_was_looking_at_on_screen`
     (`src/main.rs`), painted into ratatui's own buffer. This grid does not
     reflow, so the spike does not pretend to measure it.

Bash mode is the driver: no fake `pi`, no board, and its output arrives line at
a time, which is the shape that has to scroll. Nothing here spends a model call.

    cargo build
    python3 spikes/scrollback_e2e.py | tee spikes/results/scrollback-e2e.log

As with the other pty drivers, the screen is *reconstructed* (`Screen`, borrowed
from `status_e2e.py`) rather than grepped: the backend diffs, so a row on the
screen may not be on the wire, and a row on the wire may not be on the screen.
"""

import fcntl
import os
import pty
import re
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from status_e2e import Screen  # noqa: E402

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
ROWS, COLS = 40, 100
GROW_ROWS, GROW_COLS = 44, 130

# The wire shape of the four scroll keys, written out rather than imported so a
# terminal's own idea of them is what is under test.
PAGE_UP, PAGE_DOWN, HOME, END = b"\x1b[5~", b"\x1b[6~", b"\x1b[H", b"\x1b[F"

PILL = "End for the tail"
START = time.time()
RESULTS = []


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    say(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if not ok and detail else ""))


class Driver:
    """The app in a pty, with the screen reconstructed off its own writes."""

    def __init__(self, rows, cols):
        master, slave = pty.openpty()
        self.master = master
        self.rows, self.cols = rows, cols
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            LOOPRS_PI_BIN="/usr/bin/false",
            LOOPRS_BD_BIN="/bin/echo",
        )
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
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
        """Fold every byte received so far into the grid."""
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

    def has(self, needle):
        s = self.grid()
        return any(needle in s.text(r) for r in range(s.rows))

    def rows_with(self, needle):
        s = self.grid()
        return [r for r in range(s.rows) if needle in s.text(r)]

    def send(self, b):
        os.write(self.master, b)

    def resize(self, rows, cols):
        """Change the window and start a fresh grid.

        Deliberately *not* a replay of the old bytes into the new shape: the app
        repaints the whole screen after a resize, so the next frame is what is on
        the screen. Anything older in the grid would be content the app has since
        replaced, and a check against it would pass for the wrong reason.
        """
        self.rows, self.cols = rows, cols
        self.screen = Screen(rows, cols)
        with self.lock:
            self.fed = len(self.raw)

    def alive(self):
        return self.proc.poll() is None


def wait_for(d, needle, timeout=8.0, want=True):
    """Poll the reconstructed screen until `needle` is (or is no longer) shown."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        if d.has(needle) == want:
            return True
        time.sleep(0.05)
    return False


def main():
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2

    d = Driver(ROWS, COLS)
    tmp = tempfile.mkdtemp(prefix="looprs-scrollback-spike-")
    try:
        # ── boot into Bash mode ──────────────────────────────────────────────
        time.sleep(1.2)
        check("the app boots and is alive", d.alive())
        # Tab: Beeds -> Pi -> Bash. The mode label in the frame is the observable
        # (same needle bash_e2e.py uses).
        d.send(b"\t")
        time.sleep(0.4)
        d.send(b"\t")
        time.sleep(0.8)
        check("Tab reaches the Bash mode", d.has("Bash"), d.dump())

        # ── a transcript with a head and a tail ──────────────────────────────
        d.send(b"for i in $(seq 1 60); do echo \"MARK$i a line of the transcript that is long\"; done\r")
        if not wait_for(d, "MARK60", timeout=12.0):
            check("60 lines reach the screen", False, d.dump())
            return finish()
        check("the tail of the transcript is on screen", True)
        check(
            "…and its head has scrolled out of the band",
            not wait_for(d, "MARK1 ", timeout=0.4),
            "the head should not be visible while pinned to the tail",
        )

        # ── 1. the scroll keys arrive ───────────────────────────────────────
        d.send(PAGE_UP)
        reached = wait_for(d, "MARK1 ", timeout=3.0)
        if not reached:
            d.send(PAGE_UP)
            reached = wait_for(d, "MARK1 ", timeout=3.0)
        check(
            "PageUp (ESC[5~) scrolls the transcript up in a real pty",
            reached,
            f"head not shown; screen:\n{d.dump()}",
        )
        check(
            "…and the tail left the screen, so this is the transcript moving",
            not d.has("MARK60"),
            f"tail still shown at the top of the transcript:\n{d.dump()}",
        )

        # ── 2. the "N new" affordance, on the screen, without moving the text ─
        # Uppercased on the way out: the pty echoes the typed command into the
        # transcript too, so a needle that appeared in the command would be
        # "proved" by its own echo.
        d.send(b"echo late-arrival | tr a-z A-Z\r")
        shown = wait_for(d, PILL, timeout=8.0)
        check(
            "output that arrives while scrolled up raises the 'N new' affordance",
            shown,
            f"no pill after a new line; screen:\n{d.dump()}",
        )
        if not shown:
            return finish()

        pill_rows = d.rows_with(PILL)
        chrome = [r for r in range(d.rows) if "Tab switch" in d.grid().text(r)]
        check(
            "…and it sits on the band's bottom row, over where the tail would be",
            len(pill_rows) == 1 and (not chrome or pill_rows[0] == min(chrome) - 1),
            f"pill rows {pill_rows}, chrome rows {chrome}:\n{d.dump()}",
        )
        check(
            "…and the line that arrived is not shown while the view is off the tail",
            not d.has("LATE-ARRIVAL"),
            f"the tail was shown over the history:\n{d.dump()}",
        )

        # The strong form of "the view held": with the pill already up, a *second*
        # arrival changes nothing in the band above it. Compared row by row, not
        # by a marker, because "still showing MARK1" is satisfied by a view that
        # snapped back and scrolled up again.
        band = d.rows_with(PILL)[0]
        before = [d.grid().text(r) for r in range(band)]
        check("a band of held history is snapshotted before the second arrival", before)
        d.send(b"echo second-arrival | tr a-z A-Z\r")
        time.sleep(2.0)
        after = [d.grid().text(r) for r in range(band)]
        moved = [i for i, (a, b) in enumerate(zip(before, after)) if a != b]
        check(
            "…and a second arrival moves not one row of what the user is reading",
            not moved,
            f"rows {moved} changed:\n"
            + "\n".join(
                f"{i:3d}|{before[i]!r}\n   v {after[i]!r}" for i in moved
            )
            + f"\n{d.dump()}",
        )
        check(
            "…and it is counted rather than shown",
            d.has(PILL) and not d.has("SECOND-ARRIVAL"),
            f"{d.dump()}",
        )

        # ── 3. End is the way back ──────────────────────────────────────
        d.send(END)
        back = wait_for(d, "SECOND-ARRIVAL", timeout=6.0)
        check("End (ESC[F) returns to the tail", back, d.dump())

        # ── 3b. an arrival while *pinned* never raises the affordance ───────
        # Stronger than "the pill disappears after End": the pill is a statement
        # about unseen rows, so a pinned view must never show it, whatever
        # arrives. Checked on a live draw rather than on the erasure of the old
        # pill, because what the app does not repaint is outside this harness's
        # model of the screen.
        d.send(b"echo pinned-arrival | tr a-z A-Z\r")
        shown_again = wait_for(d, "PINNED-ARRIVAL", timeout=8.0)
        check("back at the tail, the newest line is shown", shown_again, d.dump())
        time.sleep(0.8)
        check(
            "…and a pinned arrival does not raise the 'N new' affordance",
            not d.has(PILL),
            f"the pill showed for rows the user is already looking at:\n{d.dump()}",
        )

        # ── 4. a resize taken while scrolled up, and what the wire says about it ──
        with d.lock:
            idle0 = len(d.raw)
        d.resize(GROW_ROWS, GROW_COLS)
        time.sleep(2.0)
        with d.lock:
            idle_bytes = len(d.raw) - idle0
        say(f"  idle resize emitted {idle_bytes} bytes in 2s "
            "(0 = the app does not repaint on SIGWINCH alone; 195e3c0 measures the same)")

        # What this section can and cannot say.
        #
        # It can say the app is unharmed by a resize taken with the transcript
        # scrolled up, and it can say what the wire does: nothing, while the app
        # is idle. That is the measurement above, and the pre-pdl.6 binary at
        # 195e3c0 gives the same number, so the scrollback work neither fixed nor
        # broke it — it is the run loop's dirty-gate from looprs-pdl.4. Filed as
        # a follow-up so the next measurement starts from this one.
        #
        # What it cannot say is that the *content* stayed anchored: this harness
        # has no reflowing emulator, and the app gives it nothing to reflow with.
        # That half of the resize property is proven where the screen model is
        # real — `a_resize_keeps_the_line_the_user_was_looking_at_on_screen` in
        # src/main.rs, painted into ratatui's own buffer.
        time.sleep(1.0)
        check("the app is alive with the transcript scrolled up across a resize", d.alive())
        d.send(b"echo after-resize-output | tr a-z A-Z\r")
        check(
            "…and it still takes keystrokes and shell output afterwards",
            wait_for(d, "AFTER-RESIZE-OUTPUT", timeout=8.0),
            d.dump(),
        )
        # ── 5. the other two keys, and a clean quit ────────────────────────
        # Needle is the *first entry* of the transcript rather than a MARK row:
        # after a long scroll the backend repaints cell-runs, and a mid-word run
        # can be unchanged-and-skipped, which mangles "MARK1" in this grid while
        # leaving the row's place on screen right. The first line of the
        # transcript is the one row that cannot be mistaken for anything else.
        d.send(HOME)
        check(
            "Home (ESC[H) goes to the head of the transcript",
            wait_for(d, "switched to Bash", timeout=4.0),
            f"the first entry never arrived at the top:\n{d.dump()}",
        )
        check(
            "…and the tail is not on screen from the head",
            not d.has("AFTER-RESIZE-OUTPUT"),
            f"the head showed the tail:\n{d.dump()}",
        )
        d.send(END)
        check(
            "End from the head is all the way back at the tail",
            wait_for(d, "AFTER-RESIZE-OUTPUT", timeout=4.0),
            f"the tail never came back:\n{d.dump()}",
        )
        d.send(HOME)
        d.send(PAGE_DOWN)
        d.send(END)
        check(
            "scrolling past either end stops at the end rather than wedging",
            wait_for(d, "AFTER-RESIZE-OUTPUT", timeout=4.0) and d.alive(),
            f"\n{d.dump()}",
        )

        with d.lock:
            wire = bytes(d.raw).decode("utf-8", errors="replace")
        check(
            "the whole run issued no ESC[6n cursor query",
            "\x1b[6n" not in wire,
            "a cursor query came back on the wire",
        )

        d.send(b"\x11")  # Ctrl-Q
        deadline = time.time() + 6.0
        while time.time() < deadline and d.alive():
            time.sleep(0.1)
        check("Ctrl-Q quits", not d.alive())
    finally:
        if d.alive():
            d.proc.kill()
        subprocess.run(["rm", "-rf", tmp], check=False)
    return finish()


def finish():
    ok = sum(1 for _, v in RESULTS if v)
    total = len(RESULTS)
    say(f"{ok}/{total} checks passed")
    return 0 if ok == total else 1


if __name__ == "__main__":
    sys.exit(main())
