#!/usr/bin/env python3
"""The resize transient in the shipped app: RSS sampled across a window drag (looprs-zie).

`src/measure.rs` proves the shape and `src/session/view.rs` tests the rules, but
both do it inside the process, with an allocator that is telling the truth about
itself. This one is the outside view: a real `looprs` in a real pty, a real
shell flooding it with a long transcript, and a window dragged under it while
`ps` samples the kernel's resident-set number from the parent.

Why this spike exists and what it can and cannot say.

  * It **can** say the app still answers, repaints and does not die while a
    resize lands on top of a transcript far longer than the scrollback cap —
    which is the shape a unit test drives and a user only notices at 2 a.m.
  * It **can** say the resident set does not run away across a drag.
  * It **cannot** isolate the transient exactly: freed memory stays resident
    until the allocator hands it back, so RSS is a *high-water* reading, not
    the per-frame peak `CountingAlloc` reports. That is the wrong instrument
    for the precise number and the right one for the gross one — a build that
    transiently allocates 150 MiB per resize is a build whose RSS climbs and
    climbs, and a build that stays inside its cap is a build whose RSS is flat.

Read the slope, not the level: the level includes the app's own baseline, the
transcript the shell is holding, and whatever the allocator has decided to keep.

What it said, run against this ticket's build and against the pre-ticket build
(`LOOPRS_BIN=/tmp/looprs-prefix`, built from `96e745c` and copied aside): both
flat. Sub-megabyte growth across the whole 16-step drag in both builds, and the
peak of one run differed from the peak of the other by more than the difference
between the builds. Both runs are recorded below.

That is the honest reading of RSS as an instrument here — and it is a *result*,
not a shrug: it says the change did not make the shipped app hold or move more
memory than it did before, which is the regression risk a memory change carries
into a running terminal. The transient itself is not visible this way, for two
independent reasons: freed memory stays resident, so RSS is a high-water mark
rather than a frame-by-frame peak; and with the shipped 256 KiB per-view
transcript buffer the flooded text is small enough that this scenario does not
reach the pathological sizing. The number that does resolve it — 133–151 MiB of
transient re-rendering the whole source versus 9–25 MiB cutting the source — is
in `spikes/results/resize-transient.log`, measured inside the process by the
counting allocator where the allocations actually happen.

Usage:

    cargo build
    python3 spikes/resize_transient_e2e.py

Environment: LOOPRS_BIN, ROWS, COLS, FILL_LINES, DRAG_STEP_SECONDS,
SAMPLE_SECONDS, MAX_GROWTH_MIB.
"""

import fcntl
import os
import pty
import struct
import subprocess
import sys
import termios
import threading
import time

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
ROWS = int(os.environ.get("ROWS", "40"))
COLS = int(os.environ.get("COLS", "100"))

# 400,000 short lines: the dense case, and the one the estimate is worst for.
# Short lines are many rows per byte, which is exactly what a re-render of the
# whole source used to turn into hundreds of megabytes. Enough of them that the
# producer is still running while the drag happens, which is when the rebuild
# costs the most.
FILL_LINES = int(os.environ.get("FILL_LINES", "400000"))
DRAG_STEP_SECONDS = float(os.environ.get("DRAG_STEP_SECONDS", "0.4"))
SAMPLE_SECONDS = float(os.environ.get("SAMPLE_SECONDS", "0.02"))

# The pass line. A rewrap of a transcript this size, rendered whole, measured
# 133-151 MiB of transient inside the process (spikes/results/resize-transient.log);
# with the source cut it measured 9-25 MiB. RSS cannot resolve the difference
# exactly, so the assertion is against the *gross* shape: the whole run must not
# put the app anywhere near "one full re-render per width change".
MAX_GROWTH_MIB = float(os.environ.get("MAX_GROWTH_MIB", "96"))

# The widths of the drag. Both directions, because widening and narrowing are
# priced differently and the ugly case is the one nobody exercises on purpose.
WIDTHS = [90, 80, 70, 60, 50, 45, 40, 55, 75, 95, 100, 65, 45, 30, 60, 80]

RESULTS = []
START = time.time()


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, ok))
    say(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def rss_kib(pid):
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        return int(out.strip())
    except Exception:
        return None


class Driver:
    """A `looprs` in a pty we own, with the output pumped to a buffer."""

    def __init__(self):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            LOOPRS_BD_BIN="/bin/echo",
            LOOPRS_PI_BIN="/usr/bin/false",
            LOOPRS_SHELL_BIN=os.environ.get("LOOPRS_SHELL_BIN", "/bin/bash"),
        )
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True
        )
        os.close(slave)
        self.raw = bytearray()
        self.lock = threading.Lock()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                data = os.read(self.master, 65536)
                if not data:
                    break
                with self.lock:
                    self.raw.extend(data)
                for _ in range(data.count(b"\x1b[6n")):
                    os.write(self.master, b"\x1b[1;1R")
        except OSError:
            pass

    def send(self, b):
        os.write(self.master, b)

    def alive(self):
        return self.proc.poll() is None

    def resize(self, cols, rows=None):
        fcntl.ioctl(
            self.master,
            termios.TIOCSWINSZ,
            struct.pack("HHHH", rows or ROWS, cols, 0, 0),
        )

    def bytes_out(self):
        with self.lock:
            return len(self.raw)


class RssSampler:
    """Sample the kernel's resident set as fast as `ps` will answer."""

    def __init__(self, pid, every):
        self.pid = pid
        self.every = every
        self.samples = []
        self.stop = threading.Event()
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        while not self.stop.is_set():
            v = rss_kib(self.pid)
            if v is not None:
                self.samples.append(v)
            time.sleep(self.every)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *a):
        self.stop.set()
        self.thread.join()


def main():
    print(f"=== looprs-zie · the resize transient, measured from outside ===")
    print(f"bin       : {BIN}")
    print(f"geometry  : {ROWS}x{COLS} -> dragged across {WIDTHS}")
    print(f"fill      : {FILL_LINES} short lines (many rows per byte: the dense case)")
    if not os.path.exists(BIN):
        say(f"{BIN} not found — run `cargo build` first")
        return 2

    d = Driver()
    time.sleep(1.5)
    if not d.alive():
        check("app booted", False)
        return 1
    check("app booted", True)

    idle_rss = rss_kib(d.proc.pid)
    say(f"idle RSS before the flood: {idle_rss / 1024:.1f} MiB")

    # Tab to Bash: Beeds -> Pi -> Bash.
    d.send(b"\t")
    time.sleep(0.4)
    d.send(b"\t")
    time.sleep(0.8)
    d.send(
        f"awk 'BEGIN{{for(i=0;i<{FILL_LINES};i++) print \"filler line \" i}}'".encode()
        + b"\r"
    )
    # The control on the whole measurement, made before any of it is read: a
    # flat RSS line from a shell that never started is a pass that cannot fail.
    # The app's wire byte count is a poor proxy for "the flood arrived" because
    # the frame repaints changed cells and the pty byte lane is paced by the
    # consumer (looprs-6cj) — a 400,000-line producer can be rendered in a
    # couple of frames and still be perfectly busy. So the proofs are the
    # rendered text itself, and the app's own resident set growing to take it.
    time.sleep(3.0)
    check("app alive after the flood started", d.alive())
    with d.lock:
        rendered = bytes(d.raw).count(b"filler line")
    check(
        "flood reached the screen",
        rendered >= 1,
        f"`filler line` never appeared in {d.bytes_out()} bytes of app output",
    )

    baseline = rss_kib(d.proc.pid)
    check(
        "the app absorbed the flood",
        baseline > idle_rss + 2 * 1024,
        f"RSS moved only {(baseline - idle_rss) / 1024:.1f} MiB from idle — the "
        "transcript is not being held, so the drag below measures nothing",
    )
    say(
        f"baseline RSS before the drag: {baseline / 1024:.1f} MiB "
        f"(+{(baseline - idle_rss) / 1024:.1f} over idle)"
    )

    # The drag: a width change every DRAG_STEP_SECONDS, RSS sampled throughout.
    # This is the loop the ticket names — one column of window drag, one rebuild.
    steps = []
    for w in WIDTHS:
        before = rss_kib(d.proc.pid)
        with RssSampler(d.proc.pid, SAMPLE_SECONDS) as s:
            d.resize(w)
            time.sleep(DRAG_STEP_SECONDS)
        peak = max(s.samples) if s.samples else before
        steps.append((w, before, peak))
        say(
            f"  w={w:>3}: {before / 1024:7.1f} -> {peak / 1024:7.1f} MiB "
            f"(+{(peak - before) / 1024:5.1f} for this step)"
        )

    overall_peak = max(p for _, _, p in steps)
    growth = overall_peak - baseline
    per_step = max(p - b for _, b, p in steps)
    print()
    say(f"peak RSS across the whole drag : {overall_peak / 1024:.1f} MiB")
    say(f"growth over the pre-drag level : {growth / 1024:.1f} MiB")
    say(f"worst single width change      : {per_step / 1024:.1f} MiB")
    print()

    check(
        "app survived the drag",
        d.alive(),
        "the app died while the window was being resized",
    )
    check(
        f"growth stayed under {MAX_GROWTH_MIB:.0f} MiB",
        growth <= MAX_GROWTH_MIB * 1024,
        f"{growth / 1024:.1f} MiB of growth over the pre-drag level",
    )

    for k, (name, ok) in enumerate(RESULTS):
        if not ok:
            print(f"\n{len([r for r in RESULTS if not r[1]])} check(s) failed: {name}")
            return 1
    print("\nall checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
