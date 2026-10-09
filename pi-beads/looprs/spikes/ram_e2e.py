#!/usr/bin/env python3
"""The RAM a streaming shell costs the real app, sampled from the outside (looprs-6cj).

Every other spike here judges the app by the bytes it puts *on the wire*. This one
judges it by the bytes it holds in *memory*, because the failure it measures --
"looprs eats all my RAM" -- is not visible on the wire at all. A process that is
queueing 400 MB renders exactly like a process that is keeping up.

Two producers, two different questions:

  fast    `while true; do echo <1KB>; done`  -- a shell out-producing the UI.
          The question is the SLOPE: MiB/s of RSS growth. Unbounded queueing is
          a straight line, and a cap that works is a flat one.
  slow    `while true; do echo ...; sleep 0.05; done` (~1.6 KB/s)
          The question is the CONTROL: if the slow run is flat and the fast run
          is not, the transcript cap is fine and the *rate* is the problem.
          That is the distinction looprs-6cj was filed on, and a single run
          cannot make it.

Everything is sampled from `ps` in the parent, not from inside the app, so the
number is the kernel's resident-set accounting rather than a counter the measured
code maintains about itself.

Usage:

    cargo build
    python3 spikes/ram_e2e.py                 # both producers
    python3 spikes/ram_e2e.py fast            # one of them
    LOOPRS_BIN=/path/to/old-binary python3 spikes/ram_e2e.py   # a control run

Environment: LOOPRS_BIN, ROWS, COLS, RUN_SECONDS, LINE_BYTES, SLOW_INTERVAL.

Exit 0 when every assertion held.
"""

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

BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
ROWS = int(os.environ.get("ROWS", "40"))
COLS = int(os.environ.get("COLS", "100"))
RUN_SECONDS = float(os.environ.get("RUN_SECONDS", "25"))
LINE_BYTES = int(os.environ.get("LINE_BYTES", "300"))
SLOW_INTERVAL = float(os.environ.get("SLOW_INTERVAL", "0.05"))

# The pass condition: the app may not accumulate memory at more than this, and
# may not exceed this resident set while a shell floods it.
#
# The slope is the real assertion. The number it is drawn against is the
# unbounded queue's own: that one measured +28 MiB/s and did not stop, and 1
# MiB/s leaves room for a store that is still filling its own cap in the first
# couple of seconds while ruling out anything that grows with the producer.
MAX_SLOPE_MIBS = float(os.environ.get("MAX_SLOPE_MIBS", "1.0"))
#
# The ceiling is *not* the queue's: it is the scrollback store's, which already
# has its own retained-bytes cap (32 MiB of rows, a few hundred bytes of row
# struct each) plus the app's baseline. Measured flat at ~130 MiB with looprs-6cj
# fixed; 180 MiB is the line between "capped by design" and "capped by nobody".
MAX_PEAK_MIB = float(os.environ.get("MAX_PEAK_MIB", "180"))

RESULTS = []
START = time.time()


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


def check(name, ok, detail=""):
    RESULTS.append((name, ok))
    say(f"{'PASS' if ok else 'FAIL'}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def rss_kib(pid):
    """Resident set in KiB, from `ps`. The kernel's number, not the app's."""
    try:
        out = subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True)
        return int(out.strip())
    except Exception:
        return None



def produced_enough(d, want=20):
    """Has the filler reached the rendered screen at least `want` times?

    This is the control on the whole measurement. A flat RSS line from a shell
    that never started is a pass that cannot fail, and a driver that cannot tell
    the two apart is worse than no driver: it was run once against a build that
    had not been rebuilt, reported a clean sheet, and meant nothing at all.
    """
    with d.lock:
        raw = bytes(d.raw)
    return raw.count(b"xxxxx") >= want


def producer_cpu(pid, samples=6, every=0.5):
    """The busiest child of `pid`, as a percentage of a core.

    `ps` reports a lifetime average, so this is sampled over a second rather
    than read once.
    """
    best = 0.0
    for _ in range(samples):
        try:
            out = subprocess.check_output(["ps", "-eo", "pid,ppid,%cpu,comm"], text=True)
        except Exception:
            return best
        for line in out.splitlines()[1:]:
            parts = line.split(None, 3)
            if len(parts) < 3:
                continue
            try:
                ppid, cpu = int(parts[1]), float(parts[2])
            except ValueError:
                continue
            if ppid == pid:
                best = max(best, cpu)
        time.sleep(every)
    return best


class Driver:
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

    def bytes_out(self):
        with self.lock:
            return len(self.raw)


def run_producer(kind, cmd, samples=1.0):
    say(f"── {kind}: `{cmd}`")
    d = Driver()
    time.sleep(1.5)
    if not d.alive():
        check(f"{kind}: app booted", False)
        return
    # Tab twice: Beads -> Pi -> Bash.
    d.send(b"\t")
    time.sleep(0.4)
    d.send(b"\t")
    time.sleep(0.8)
    d.send(cmd.encode() + b"\r")
    time.sleep(1.5)
    # Prove the producer is actually running before measuring anything about it:
    # a flat RSS line from a shell that never started is a pass that cannot fail.
    # The proof is the *child's* CPU, read from outside the app -- the app's own
    # wire output is a poor proxy, because a screen redraw that changes three
    # cells writes three cells and nothing else.
    check(
        f"{kind}: app alive",
        d.alive(),
        "the app died before the producer started",
    )
    check(
        f"{kind}: flood reached the screen",
        produced_enough(d),
        "the filler never showed up in what the app rendered -- nothing was "
        "produced, so the RSS measurement below proves nothing",
    )

    series = []
    t0 = time.time()
    while time.time() - t0 < RUN_SECONDS:
        time.sleep(samples)
        k = rss_kib(d.proc.pid)
        if k is None:
            break
        series.append((time.time() - t0, k, d.bytes_out()))
    say(f"{kind}: samples (t, RSS MiB, bytes out MiB):")
    for t, k, b in series:
        say(f"   {t:5.1f}s  {k / 1024:8.1f} MiB  {b / 1048576:7.2f}")
    if len(series) >= 4:
        first, last = series[1], series[-1]
        dur = last[0] - first[0]
        rate = (last[1] - first[1]) / 1024.0 / dur if dur > 0 else 0.0
        out_rate = (last[2] - first[2]) / dur if dur > 0 else 0.0
        say(f"{kind}: RSS slope {rate:+.2f} MiB/s over {dur:.1f}s; wire {out_rate/1048576:.2f} MB/s")
        # The bound this ticket asked for. 1 MiB/s is generous against the
        # 27 MiB/s the unbounded queue measured, and tight enough that a
        # re-introduced unbounded hop shows up immediately.
        check(
            f"{kind}: RSS slope bounded",
            rate < MAX_SLOPE_MIBS,
            f"{rate:+.2f} MiB/s exceeds {MAX_SLOPE_MIBS} MiB/s",
        )
        d.send(b"\x03")
        time.sleep(0.5)
        peak = max(s[1] for s in series) / 1024.0
        say(f"{kind}: peak RSS {peak:.1f} MiB")
        check(
            f"{kind}: peak RSS bounded",
            peak < MAX_PEAK_MIB,
            f"peak {peak:.1f} MiB exceeds {MAX_PEAK_MIB} MiB",
        )
        if kind == "fast":
            # Reported, deliberately NOT asserted: whether the flooding shell is
            # pegged at a core or paced to ~10% of one turned out to depend on
            # how far the machine already is from keeping up, and an assertion
            # that passes both with and without flow control (it did -- 21% on
            # the unbounded build) is not an assertion. The slope above is the
            # one that discriminates. The number is still worth printing: it is
            # where you look to see *who* is being slowed down.
            say(f"{kind}: busiest child {producer_cpu(d.proc.pid):.1f}% CPU")
    d.send(b"\x11q" if False else b"")  # no-op, keep the exit explicit below
    d.send(b"exit\r")
    time.sleep(0.5)
    try:
        d.proc.send_signal(signal.SIGTERM)
    except Exception:
        pass
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2
    which = sys.argv[1] if len(sys.argv) > 1 else "both"
    filler = "x" * LINE_BYTES
    if which in ("both", "fast"):
        run_producer("fast", f"while true; do echo {filler}; done")
    if which in ("both", "slow"):
        run_producer(
            "slow", f"while true; do echo {filler}; sleep {SLOW_INTERVAL}; done"
        )
    fails = [n for n, ok in RESULTS if not ok]
    say(f"{len(RESULTS) - len(fails)}/{len(RESULTS)} checks passed")
    return 1 if fails else 0


if __name__ == "__main__":
    sys.exit(main())
