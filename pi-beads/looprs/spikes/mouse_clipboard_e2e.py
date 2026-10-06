#!/usr/bin/env python3
"""Can we drive a selection from the outside, and does the clipboard round-trip?

looprs-pdl.2. Seven claims sit under the selection/clipboard tickets
(looprs-pdl.8/.9/.10/.12 and the mouse half of ADR-0004). Every one of them ends
this file as a number, a yes/no, or a line saying **not measured here** with the
command that would measure it. Nothing in the summary is an assertion dressed up as
a result.

    cargo build --examples
    python3 spikes/mouse_clipboard_e2e.py                    # every bare-pty group
    python3 spikes/mouse_clipboard_e2e.py inject burst       # one group at a time
    python3 spikes/mouse_clipboard_e2e.py --control          # the control run
    python3 spikes/mouse_clipboard_e2e.py --in-terminal      # inside a real terminal window
    python3 spikes/mouse_clipboard_e2e.py --launch           # open that leg in a Terminal window itself
    python3 spikes/mouse_clipboard_e2e.py --ssh              # the SSH leg (docker sshd)

## Where each claim's number comes from

| group | claim | the measurement |
| --- | --- | --- |
| `inject` | 1. a selection can be driven from outside the app | SGR 1006 reports written to a pty master, read back through `crossterm`'s own parser inside that pty (`examples/spike_mouse_probe.rs`) |
| `burst` | 4a. how much the mouse path can carry | the probe's own clock over bursts of 10/50/128 scroll reports written in one call |
| `shift` | 5a. the shift bit survives to the app | the modifier byte on a shift-drag report as the library reports it |
| `modes` | 6. mode set/restore symmetry | the real `looprs` byte stream folded with `shutdown_e2e.ModeTrace`, plus `termios` read off the tty before and after |
| `vim` | 7. a full-screen child's nested mouse modes | vim's own `?1000/1002/1006/2004` writes and leaves, with and without `mouse=a`, alive and SIGKILLed |
| `clipboard` | 2a. the OSC 52 bytes we emit are well formed | the probe's OSC 52 wire, base64-decoded back to the exact payload |
| `--in-terminal` | 2b, 3, 4b, 5b: the emulator's side | a real terminal window: DEC mode queries answered by the emulator, clipboard read back with `pbpaste`/`wl-paste`/`xclip`, a timed copy ladder, and a recorded trackpad flick |
| `--ssh` | the same through a real SSH hop | `sshd` in a container with a Linux build of the probe, key auth |

**Why the emulator leg is separate, and why that is not a hole in the bare-pty
run.** A pty has no terminal emulator behind it. The bytes an app writes are the
whole truth about the app and none of the truth about what the user's clipboard
got, because the thing that interprets OSC 52 is the window, not the kernel. So
the bare-pty groups measure what we *send*; `--in-terminal` measures what an
emulator *does*. Reporting the first as if it covered the second would be exactly
the kind of confident nonsense `docs/testing.md` already has war stories about.

**The controls, and what each one rules out.**

* `events --decode-off` — same tty, same raw mode, the same `?1000h/?1002h/?1006h`
  switched on, no parser. Counts bytes instead of events. A "mouse event"
  appearing here would not have come from parsing.
* malformed SGR (the `<` dropped) — the same three reports with the introducer
  missing. The control claim is that no press/drag/release chain can form without
  it, and that none of it arrives as keystrokes, because a mouse report read as
  keystrokes would land in the user's input box.
* differential re-injection at +5 — every reported coordinate must move by
  exactly +5, which is what makes the numbers track the wire rather than the
  constants typed into this script.
* `kill -9` in the `modes` group — a run that leaves every mode switched on and
  the tty raw. The control for the *detector*: `ModeTrace` must report residue
  when there is residue, or its clean verdicts mean nothing.
* OSC 51 instead of 52 in the emulator leg — the same base64 payload under the
  wrong opcode. A clipboard that changes on that input changed for some reason
  other than our sequence, so a passing OSC 52 test cannot be a stale clipboard.

The clipboard is put back the way it was found at the end of every leg that touches
it, because this runs on a machine a person is using.
"""

import base64
import fcntl
import os
import pty
import re
import select
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from shutdown_e2e import ModeTrace  # the same wire ledger the exit spike uses

PROBE = os.environ.get("SPIKE_PROBE", "target/debug/examples/spike_mouse_probe")
# The source, not the build product: the SSH leg rebuilds this into a Linux binary
# inside a container, and copying the Mach-O out of target/ and calling it a source
# file is a build that fails with a Rust compiler error about an unexpected magic
# number.
PROBE_SRC = os.environ.get("SPIKE_PROBE_SRC", "examples/spike_mouse_probe.rs")
BIN = os.environ.get("LOOPRS_BIN", "target/debug/looprs")
RESULTS = []
NUMBERS = []
NOT_MEASURED = []
START = time.time()
ROWS, COLS = 32, 100
CTRL_Q = b"\x11"
RAW_OUT = [False]  # raw mode needs \r\n, or the log staircases
# The controlling terminal the escape sequences actually go to. Separate from
# stdout so the log can be `tee`d while the OSC bytes still reach the emulator:
# with stdout piped into a file, the window never sees the copy request and every
# check in the emulator leg passes on nothing.
TTY = [None]

# The mouse-report bytes this script writes, and the event `crossterm` should make
# of each. `code` is the SGR 1006 code byte: bits 0-1 the button (0 left,
# 1 middle, 2 right), 0x04 shift, 0x08 alt, 0x10 ctrl, 0x20 drag/motion,
# 64/65/66/67 the wheel. crossterm's modifier bits are its own -- SHIFT=0b001,
# CONTROL=0b010, ALT=0b100 -- so the wire-bit -> library-bit translation is one
# of the measured lines rather than an assumption (and it is not an orderly
# mapping: alt and ctrl swap places on the way in).
SGR_TABLE = [
    ("press left", 0, "down", "left"),
    ("drag left", 32, "drag", "left"),
    ("press right", 2, "down", "right"),
    ("press middle", 1, "down", "middle"),
    ("scroll up", 64, "scroll_up", "-"),
    ("scroll down", 65, "scroll_down", "-"),
    ("scroll left", 66, "scroll_left", "-"),
    ("scroll right", 67, "scroll_right", "-"),
    ("press left + shift", 0 | 4, "down", "left"),
    ("drag left + shift", 32 | 4, "drag", "left"),
    ("press left + ctrl", 0 | 16, "down", "left"),
    ("press left + alt", 0 | 8, "down", "left"),
    ("motion, no button", 35, "moved", "-"),
]


# --------------------------------------------------------------------------
# reporting
# --------------------------------------------------------------------------


def emit(msg=""):
    data = (msg + ("\r\n" if RAW_OUT[0] else "\n")).encode()
    sys.stdout.buffer.write(data)
    sys.stdout.flush()
    if TTY[0] is not None:
        try:
            os.write(TTY[0], data)
        except OSError:
            pass


def say(msg):
    emit(f"[{time.time() - START:6.2f}s] {msg}")


def check(name, ok, detail=""):
    RESULTS.append((name, bool(ok)))
    say(("PASS  " if ok else "FAIL  ") + name + (f"  -- {detail}" if detail and not ok else ""))


def measured(name, value):
    """A number that is a result but not a pass/fail: reported, not asserted."""
    NUMBERS.append((name, value))
    say(f"NUM   {name} = {value}")


def not_measured(claim, why, how):
    NOT_MEASURED.append((claim, why, how))
    say(f"N/A   {claim} -- {why}")
    say(f"      to measure: {how}")


# --------------------------------------------------------------------------
# the pty
# --------------------------------------------------------------------------


class Pty:
    """A child on a real pty, with every byte it wrote kept with its arrival time.

    Nothing here emulates a terminal: reads are timestamped as they arrive at the
    master, which is the harness's clock and the only one that can say when the
    app's bytes got here.
    """

    def __init__(self, cmd, rows=ROWS, cols=COLS, env=None):
        self.master, slave = pty.openpty()
        self.rows, self.cols = rows, cols
        fcntl.ioctl(self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        e = dict(os.environ)
        e.update(TERM="xterm-256color", LINES=str(rows), COLUMNS=str(cols))
        if env:
            e.update(env)
        self.cmd = cmd
        self.proc = subprocess.Popen(cmd, stdin=slave, stdout=slave, stderr=slave,
                                    env=e, preexec_fn=os.setsid)
        os.close(slave)
        # A second handle on the same tty, so the tty's own termios can be read
        # back after the child is gone: raw mode lives on the tty, not in us.
        self.tty_fd = os.dup(self.master)
        self.lock = threading.Lock()
        self.raw = bytearray()
        self.chunks = []  # (arrival, nbytes)
        # The pty emulates one thing only: it answers `ESC[6n`, because an app
        # that cannot read its cursor position quits before any of this can be
        # measured. Same insurance the exit spike runs with -- an instant answer
        # can be eaten by the reader on its way out, so a second lands late.
        self.answer_cpr = env is None or env.get("SPIKE_ANSWER_CPR", "1") != "0"
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
                    self.chunks.append((time.time(), len(data)))
                if self.answer_cpr:
                    for _ in range(data.count(b"\x1b[6n")):
                        row = min(self.last_row + 1, self.rows - 1)
                        threading.Timer(0.02, self._answer_cpr, args=(row,)).start()
                        threading.Timer(0.30, self._answer_cpr, args=(row,)).start()
        except OSError:
            pass

    def _answer_cpr(self, row):
        try:
            os.write(self.master, b"\x1b[%d;1R" % max(1, min(row, self.rows - 1)))
        except OSError:
            pass

    def text(self):
        with self.lock:
            return bytes(self.raw).decode("utf-8", errors="replace")

    def raw_bytes(self):
        with self.lock:
            return bytes(self.raw)

    def mark(self):
        with self.lock:
            return len(self.raw)

    def since(self, mark):
        with self.lock:
            return bytes(self.raw[mark:]).decode("utf-8", errors="replace")

    def send(self, b):
        os.write(self.master, b)

    def wait_for(self, needle, timeout=10.0):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if needle in self.text():
                return True
            time.sleep(0.01)
        return False

    def wait_idle(self, quiet=0.4, timeout=8.0, min_bytes=1):
        """Wait until the child stops writing -- used before driving a full-screen child.

        `min_bytes` keeps it from declaring "idle" during the pause before a program
        first paints, which would have the harness typing its quit keys at a screen
        that has not been drawn yet.
        """
        last, still = self.mark(), 0.0
        deadline = time.time() + timeout
        while time.time() < deadline:
            time.sleep(0.1)
            m = self.mark()
            if m >= min_bytes and m == last:
                still += 0.1
                if still >= quiet:
                    return True
            else:
                still, last = 0.0, m
        return False

    def lines(self, prefix):
        return [ln for ln in self.text().splitlines() if ln.startswith(prefix)]

    def tty_lflags(self):
        try:
            return termios.tcgetattr(self.tty_fd)[3]
        except OSError:
            return None

    def tty_is_cooked(self):
        f = self.tty_lflags()
        return f is not None and bool(f & termios.ICANON) and bool(f & termios.ECHO)

    def go_raw(self):
        """Put this pty's own line discipline into raw mode, the way a terminal has it.

        Without it, canonical mode line-buffers injected escape sequences: a mouse
        report never ends in a newline, so a program on the far side of a hop sits
        on the whole burst until something else sends `\n`. A real terminal has
        been in raw mode since the app asked for it, so raw is the state this
        harness has to be in to be measuring the same thing.
        """
        try:
            a = termios.tcgetattr(self.tty_fd)
        except OSError:
            return False
        a[0] = 0
        a[1] = 0
        a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
        a[3] = 0
        termios.tcsetattr(self.tty_fd, termios.TCSANOW, a)
        return not self.tty_is_cooked()

    def chunk_stats(self, since_idx=0):
        with self.lock:
            ch = self.chunks[since_idx:]
        if not ch:
            return 0, 0, 0.0
        return len(ch), max(c[1] for c in ch), (ch[-1][0] - ch[0][0])

    def kill(self, sig=signal.SIGKILL):
        try:
            os.killpg(os.getpgid(self.proc.pid), sig)
        except (ProcessLookupError, PermissionError):
            pass

    def close(self):
        self.kill()
        try:
            self.proc.wait(timeout=5)
        except Exception:
            pass
        for fd in (self.tty_fd, self.master):
            try:
                os.close(fd)
            except OSError:
                pass


def sgr(code, x, y, release=False):
    """One SGR 1006 mouse report, spelled the way a terminal spells it.

    `ESC[<b;x;yM` for press/drag/motion, `ESC[<b;x;ym` for release. Coordinates
    are 1-based on the wire; that is the whole reason the mapping check exists.
    """
    return b"\x1b[<%d;%d;%d%s" % (code, x, y, b"m" if release else b"M")


EV_MOUSE = re.compile(r"EV t=([\d.]+) mouse=(\w+) btn=(\S+) x=(-?\d+) y=(-?\d+) mods=(\d+)")
EV_KEY = re.compile(r"EV t=([\d.]+) key=(\S+) kind=(\S+) mods=(\d+)")


def ev_mouse(text):
    return [dict(t=float(m.group(1)), kind=m.group(2), btn=m.group(3),
                x=int(m.group(4)), y=int(m.group(5)), mods=int(m.group(6)))
            for m in EV_MOUSE.finditer(text)]


def ev_key(text):
    return [m.group(0) for m in EV_KEY.finditer(text)]


def run_probe(args, inject=None, wait=12.0, rows=ROWS, cols=COLS, env=None, hold=0.4,
             ready="HELLO"):
    """Run the probe on a pty, inject bytes while it reads, hand back the pty.

    Each entry of `inject` is its own write(), so whatever the tty does with
    chunking is part of what gets observed rather than something the harness hides.
    """
    if not os.path.exists(PROBE):
        raise SystemExit(f"probe missing: {PROBE} -- run `cargo build --examples`")
    p = Pty([PROBE] + list(args), rows=rows, cols=cols, env=env)
    if not p.wait_for(ready, 15.0):
        txt = p.text()
        p.close()
        raise SystemExit(f"probe never said {ready}: {txt[:200]}")
    for blob in inject or []:
        p.send(blob)
    p.wait_for("SUMMARY", wait)
    time.sleep(hold)
    return p


def ruler(n):
    """The payload: an ASCII ruler whose last line names its own length.

    The trailing `<<END-PAYLOAD-n>>` is what makes a silent clamp visible from a
    distance: a copy that lost bytes lost the marker with them, and the marker can
    be checked from the far side of a hop without re-deriving the payload.
    """
    buf = bytearray()
    i = 0
    while len(buf) < n:
        buf.extend(f"{i:06} 0123456789 abcdefghijklmnopqrstuvwxyz ABCDE\n".encode())
        i += 1
    buf = buf[:n]
    buf.extend(f"<<END-PAYLOAD-{n}>>\n".encode())
    return bytes(buf)


# --------------------------------------------------------------------------
# claim 1 -- drive a selection from outside
# --------------------------------------------------------------------------


def group_inject(control=False):
    say("--- claim 1: a selection driven from outside the app (SGR 1006 -> crossterm)")
    if control:
        return control_inject()

    inj, expect = [], []
    for j, (label, code, kind, btn) in enumerate(SGR_TABLE):
        x, y = 12 + j, 7 + j
        inj.append(sgr(code, x, y))
        expect.append((label, kind, btn, x, y))
    p = run_probe(["events", "900"], inject=inj)
    txt = p.text()
    evs = ev_mouse(txt)
    say(f"    probe {(' '.join(p.lines('SUMMARY')))}")

    check("inject: every report produced exactly one event, none merged or dropped",
          len(evs) == len(inj), f"{len(evs)} events for {len(inj)} reports")
    if len(evs) < len(inj):
        check("inject: enough events to check the mapping", False, "short run")
        p.close()
        return

    wrong = [f"{lbl}: wanted {wk}/{wb} got {e['kind']}/{e['btn']}"
             for (lbl, wk, wb, _, _), e in zip(expect, evs)
             if (e["kind"], e["btn"]) != (wk, wb)]
    check("inject: kind+button mapping correct for all 13 report types", not wrong,
          "; ".join(wrong[:3]))

    offs = sorted({(w[3] - e["x"], w[4] - e["y"]) for w, e in zip(expect, evs)})
    measured("wire coordinate (1-based) minus reported cell, over all 13 reports", offs)
    check("inject: the mapping is a constant -1 on both axes", offs == [(1, 1)], f"{offs}")

    inside = all(0 <= e["x"] < COLS and 0 <= e["y"] < ROWS for e in evs)
    measured("max reported col,row in a %d x %d grid" % (COLS, ROWS),
             f"{max(e['x'] for e in evs)},{max(e['y'] for e in evs)}")
    check("inject: every report lands inside the grid the pty was sized to", inside)

    by_label = {lbl: e for (lbl, _, _, _, _), e in zip(expect, evs)}
    # Wire bit -> crossterm bit, read out of crossterm's own `parse_cb` and
    # asserted rather than assumed: shift 0x04 -> SHIFT(0b001), alt 0x08 ->
    # ALT(0b100), ctrl 0x10 -> CONTROL(0b010). Not a shift: alt and ctrl swap
    # places on the way in, so a selection handler that guessed the order would
    # read a ctrl-drag as an alt-drag.
    for lbl, bit, name in (("press left + shift", 0b001, "SHIFT"),
                          ("press left + alt", 0b100, "ALT"),
                          ("press left + ctrl", 0b010, "CONTROL")):
        e = by_label[lbl]
        measured(f"crossterm modifier bits for {lbl}", e["mods"])
        check(f"inject: {lbl} arrives with {name} set", e["mods"] & bit == bit, f"mods={e['mods']}")
    p.close()


def control_inject():
    say("--- claim 1 CONTROLS: decode-off, malformed, and a +5 differential")

    one = sgr(0, 12, 7) + sgr(32, 13, 7) + sgr(0, 13, 7, release=True)
    p = run_probe(["events", "700", "--decode-off"], inject=[one])
    txt = p.text()
    got = re.search(r"RAWREAD bytes=(\d+)", txt)
    got = int(got.group(1)) if got else -1
    check("control decode-off: no events without a parser", len(ev_mouse(txt)) == 0,
          f"{len(ev_mouse(txt))} events")
    check("control decode-off: the tty carried every injected byte intact", got == len(one),
          f"{got} of {len(one)} bytes")
    p.close()

    # The same three reports (press, drag, release) with the SGR introducer `<`
    # dropped. Measured: the two whose code byte is 0 are dropped outright, and the
    # one with the drag bit set (32) comes back as a *press* -- same -1 coordinate
    # convention, wrong event. What the control asserts is the thing that matters:
    # without `<` no press+drag+release chain forms, so no selection can be
    # driven by anything that is not properly SGR-framed, and nothing arrives as
    # keystrokes either.
    bad = b"\x1b[0;12;7M\x1b[32;13;7M\x1b[0;13;7m"
    p = run_probe(["events", "700"], inject=[bad])
    txt = p.text()
    keys = ev_key(txt)
    evs = ev_mouse(txt)
    kinds = sorted({e["kind"] for e in evs})
    measured("malformed (no `<`) reports decoded per kind",
             {k: sum(1 for e in evs if e["kind"] == k) for k in kinds} or "none")
    check("control malformed: without the SGR introducer no press/drag/release chain forms",
          not {"down", "drag", "up"} <= set(kinds), f"kinds={kinds}")
    check("control malformed: the malformed reports do not arrive as keystrokes",
          not keys, f"{len(keys)} key events: {keys[:3]}")
    p.close()

    inj = [sgr(0, 12, 7), sgr(32, 13, 8), sgr(0, 14, 9, release=True),
           sgr(0, 7, 2), sgr(32, 8, 3), sgr(0, 9, 4, release=True)]
    p = run_probe(["events", "700"], inject=inj)
    evs = ev_mouse(p.text())
    if len(evs) >= 6:
        dx = [a["x"] - b["x"] for a, b in zip(evs[:3], evs[3:6])]
        dy = [a["y"] - b["y"] for a, b in zip(evs[:3], evs[3:6])]
        measured("differential run: second triple injected 5 rows up and 5 cols left",
                 f"dx={dx} dy={dy}")
        check("control differential: every reported coordinate moves by exactly the injected delta",
              dx == [5, 5, 5] and dy == [5, 5, 5], f"dx={dx} dy={dy}")
    else:
        check("control differential: enough events to compare", False, f"{len(evs)} events")
    p.close()


# --------------------------------------------------------------------------
# claim 4a -- what the wire and the parser can carry
# --------------------------------------------------------------------------


def group_burst(control=False):
    say("--- claim 4a: burst capacity of the mouse path")
    if control:
        blob = b"".join(sgr(64, 40, 10) for _ in range(64))
        p = run_probe(["events", "700", "--decode-off"], inject=[blob])
        got = re.search(r"RAWREAD bytes=(\d+)", p.text())
        check("control burst: the bytes are intact with no parser running",
              int(got.group(1) if got else -1) == len(blob),
              f"{got.group(1) if got else '?'} of {len(blob)}")
        p.close()
        return

    for k in (10, 50, 128):
        blob = b"".join(sgr(64, 40 + (i % 3), 10) for i in range(k))
        p = run_probe(["events", "900"], inject=[blob], hold=0.5)
        evs = ev_mouse(p.text())
        span = (evs[-1]["t"] - evs[0]["t"]) if len(evs) > 1 else 0.0
        gaps = [(b["t"] - a["t"]) for a, b in zip(evs, evs[1:])]
        worst = max(gaps) if gaps else 0.0
        nchunks, biggest, cspan = p.chunk_stats()
        measured(f"{k} scroll reports in one {len(blob)}-byte write",
                 f"decoded={len(evs)}/{k} probe-clock span={span:.3f}ms "
                 f"worst_gap={worst:.3f}ms "
                 f"rate={len(evs) / span * 1000 if span else 0:.0f}/s "
                 f"pty_chunks_to_harness={nchunks}")
        check(f"burst {k}: nothing lost or merged on the way in", len(evs) == k,
              f"{len(evs)} of {k}")
        p.close()

    say("        A real flick is a burst of scroll events at a rate no script knows.")
    say("        Measured above is the ceiling the flick has to fit under; the")
    say("        flick's own shape is the missing half.")
    not_measured("4b. the shape of one physical trackpad flick (events per flick, their spacing)",
                 "there is no finger on this path; the numbers above are the capacity of the wire and the parser, not of the input device",
                 "`--in-terminal --record-flick` inside a real terminal, and move the trackpad when it asks")


# --------------------------------------------------------------------------
# claim 5a -- the shift bit
# --------------------------------------------------------------------------


def group_shift(control=False):
    say("--- claim 5a: shift-drag reaches the app at all")
    if control:
        p = run_probe(["events", "600"], inject=[b"\x1b[<36;20;12M"])
        keys, evs = ev_key(p.text()), ev_mouse(p.text())
        check("control shift: a shift-drag report is one mouse event and not keystrokes",
              len(evs) == 1 and not keys, f"mouse={len(evs)} keys={len(keys)}")
        p.close()
        return

    p = run_probe(["events", "700"],
                  inject=[sgr(0, 20, 12), sgr(36, 21, 12), sgr(36, 22, 12),
                         sgr(4, 22, 12, release=True)],
                  env={"SPIKE_MODES": "raw,mouse_report,mouse_drag,mouse_sgr"})
    evs = ev_mouse(p.text())
    drag = [e for e in evs if e["kind"] == "drag"]
    check("shift: both shift-drag reports arrive as Drag with SHIFT set",
          len(drag) == 2 and all(e["mods"] & 1 for e in drag),
          f"{[(e['kind'], e['mods']) for e in evs]}")
    measured("shift-modified drag reports delivered", len(drag))
    p.close()
    not_measured("5b. whether shift-drag still reaches the terminal's NATIVE selection while we hold 1000+1002+1006",
                 "that is a property of the window, not of the byte stream: nothing comes back up the pty to say whether a selection band got painted",
                 "`--in-terminal`, which turns the three mouse modes on and asks for two shift-drags and a y/n answer")


# --------------------------------------------------------------------------
# claim 6 -- mode set / restore symmetry
# --------------------------------------------------------------------------


def stty_baseline():
    p = Pty(["/bin/sh", "-c", "stty -a; printf 'BASELINE_DONE\\n'"])
    p.wait_for("BASELINE_DONE", 10)
    out, flags = p.text(), p.tty_lflags()
    p.close()
    return out, flags


def app_env():
    """The real app, with the backends that must not be reached silenced."""
    return {"LOOPRS_MODES": "all", "LOOPRS_BD_BIN": "/bin/echo",
            "LOOPRS_PI_BIN": "/bin/echo", "RUST_LOG": "off"}


def group_modes(control=False):
    say("--- claim 6: the terminal is handed back in the mode set it started in")
    _, base = stty_baseline()
    say(f"    baseline lflags={base} ICANON={bool(base and base & termios.ICANON)} "
        f"ECHO={bool(base and base & termios.ECHO)}")

    if control:
        p = Pty([BIN], env=app_env())
        p.wait_for("Tab", 25)
        time.sleep(1.5)
        p.kill(signal.SIGKILL)
        time.sleep(0.8)
        tr = ModeTrace(p.text())
        left, cooked = tr.still_on(), p.tty_is_cooked()
        say(f"    after SIGKILL: still_on={left} tty_cooked={cooked} off={dict(tr.off_counts)}")
        check("control modes: SIGKILL really does leave modes switched on", bool(left),
              "the detector saw no residue, so a clean verdict from it would mean nothing")
        check("control modes: and it leaves the tty raw", not cooked,
              "tty came back cooked on a killed run -- nothing to compare against")
        p.close()
        return

    p = Pty([BIN], env=app_env())
    painted = p.wait_for("Tab", 25)
    time.sleep(1.5)
    mark = p.mark()
    t0 = time.time()
    p.send(CTRL_Q)
    try:
        code = p.proc.wait(timeout=25)
    except subprocess.TimeoutExpired:
        code = None
    dt = time.time() - t0
    time.sleep(0.4)
    tr = ModeTrace(p.since(mark))    # only what the quit key caused
    full = ModeTrace(p.text())       # the whole run: what went on, and what came back
    say(f"    quit rc={code} in {dt:.2f}s taken={dict(full.on_counts)} "
        f"left_whole_run={dict(full.off_counts)} left_after_quit_key={dict(tr.off_counts)}")
    want = {"alt_screen", "mouse_report", "mouse_drag", "mouse_sgr", "bracketed_paste",
            "cursor_hidden"}
    taken = set(full.on_counts)
    check("modes: the whole requested set went on", painted and want <= taken,
          f"painted={painted} taken={sorted(taken)}")
    bad = sorted(m for m in taken - {"cursor_hidden"} if full.off_counts.get(m, 0) != 1)
    check("modes: every mode that went on came back off exactly once", not bad,
          f"counts disagree for {bad}")
    # The cursor is the one mode that is not a one-shot: a frame that ends with no
    # cursor puts `?25h` back at the end of every draw, so the hand-back is the
    # last of several clears rather than the only one. Measured here rather than
    # waved through -- the same exemption shutdown_e2e's ledger check makes, with
    # the number attached so a future change that starts toggling it per keystroke
    # is visible instead of merely tolerated.
    measured("cursor_hidden clears over the whole run (frames end with ?25h; the last is the hand-back)",
             f"on={full.on_counts.get('cursor_hidden', 0)} off={full.off_counts.get('cursor_hidden', 0)}")
    check("modes: nothing is still switched on when the app is gone", not full.still_on(),
          f"still on: {full.still_on()}")
    check("modes: the tty is cooked again", p.tty_is_cooked(), f"lflags={p.tty_lflags()}")
    check("modes: the cursor is visible again", full.cursor_visible)
    measured("leave sequences written after the quit key", sum(tr.off_counts.values()))
    say(f"    lflags after exit: {p.tty_lflags()} (baseline {base})")
    p.close()


# --------------------------------------------------------------------------
# claim 7 -- a full-screen child inside our modes
# --------------------------------------------------------------------------


def send_safe(p, b):
    """Write to the master even if the child already left; a dead child is a finding, not a crash."""
    try:
        p.send(b)
        return True
    except OSError:
        return False


def run_child_with_our_modes(argv, kill=False, quit_keys=b":qa!\r", label=""):
    """Put our mouse modes on the tty with a shell `printf`, then exec the child in them.

    The `printf` is the point: that is *our* mode set -- the one
    `src/teardown.rs` keeps its ledger of -- going onto the wire before the child
    starts, so everything the child leaves behind is measured against a tty that
    was ours first. `exec` matters as much: the child ends up with the same tty the
    shell had, not a pipe.
    """
    pre = "\x1b[?1000h\x1b[?1002h\x1b[?1006h"
    esc = pre.replace("\x1b", "\\033")
    quoted = " ".join(shlex.quote(a) for a in argv)
    shell = f"stty raw -echo 2>/dev/null; printf '{esc}'; exec {quoted}"
    p = Pty(["/bin/sh", "-c", shell])
    p.wait_idle(quiet=0.6, timeout=12.0, min_bytes=300)
    say(f"    [{label}] settled; {p.mark()} bytes on the wire so far")
    if kill:
        p.kill(signal.SIGKILL)
        time.sleep(0.7)
    else:
        if not send_safe(p, quit_keys):
            say("    the child was already gone before the quit keys -- recorded below by its residue")
        try:
            p.proc.wait(timeout=12)
        except subprocess.TimeoutExpired:
            p.kill()
        time.sleep(0.5)
    full, cooked = p.text(), p.tty_is_cooked()
    p.close()
    return full, cooked


def group_vim(control=False):
    say("--- claim 7: what a full-screen child does to our mouse capture")
    if not shutil.which("vim") and not os.path.exists("/usr/bin/vim"):
        not_measured("7. vim's nested mouse modes", "no vim on this host",
                     "install vim and re-run the `vim` group")
        return
    ver = sh(["vim", "--version"]).stdout.splitlines()[0] if os.path.exists("/usr/bin/vim") else "?"
    say(f"    vim: {ver}")
    scratch = tempfile.NamedTemporaryFile("w", prefix="pdl2-vim-", suffix=".txt", delete=False)
    scratch.write("".join(f"line {i} of the file vim is holding\n" for i in range(40)))
    scratch.close()
    variants = [
        ("vim -u NONE, vim's own mouse default",
         ["/usr/bin/vim", "-u", "NONE", "-i", "NONE", scratch.name], False),
        ("vim with :set mouse=a (mouse-tracking vim)",
         ["/usr/bin/vim", "-u", "NONE", "-i", "NONE", "-c", "set mouse=a", scratch.name], False),
        ("vim :set mouse=a, SIGKILLed while it holds the tty",
         ["/usr/bin/vim", "-u", "NONE", "-i", "NONE", "-c", "set mouse=a", scratch.name], True),
    ]
    for label, args, kill in variants:
        full, cooked = run_child_with_our_modes(args, kill=kill, label=label)
        tr = ModeTrace(full)
        ours = ("mouse_report", "mouse_drag", "mouse_sgr")
        setn = {m: tr.on_counts.get(m, 0)
                for m in ours + ("bracketed_paste", "alt_screen", "cursor_hidden")}
        left = tr.still_on()
        say(f"    [{label}]")
        say(f"        taken={setn}")
        say(f"        left_off={dict(tr.off_counts)} still_on_at_end={left}")
        say("        (tty_cooked is not a measurement here: the harness put the tty raw to "
            "stand in for the app's raw mode and only the app would restore it)")
        if kill:
            check("vim control: the killed child leaves residue for us to clean up", bool(left),
                  "a killed vim left nothing on -- the control cannot see residue")
            continue
        # What pdl.12 needs: does the child switch OUR mouse modes off, leaving
        # the app's mouse dead when focus comes back?
        ours_off = sorted(m for m in ours if tr.off_counts.get(m, 0) > 0)
        measured(f"{label}: our mouse modes this child switched off", ours_off or "none")
        check(f"vim: {label} does not leave the alternate screen on",
              "alt_screen" not in left, f"still on: {left}")
    not_measured("7b. what the user's screen looks like while vim holds our alternate screen",
                "the pty has no window; the byte stream says which modes changed, not what a person saw",
                "run vim inside a real terminal window during the --in-terminal leg")
    os.unlink(scratch.name)


# --------------------------------------------------------------------------
# claim 2a -- the OSC 52 bytes we emit
# --------------------------------------------------------------------------


def clipboard_reader():
    if sys.platform == "darwin" and shutil.which("pbpaste"):
        return ["pbpaste"]
    for cand in (["wl-paste", "--no-newline"], ["xclip", "-selection", "clipboard", "-o"]):
        if shutil.which(cand[0]):
            return cand
    return None


def clipboard_writer():
    if sys.platform == "darwin" and shutil.which("pbcopy"):
        return ["pbcopy"]
    for cand in (["wl-copy"], ["xclip", "-selection", "clipboard"]):
        if shutil.which(cand[0]):
            return cand
    return None


def read_clipboard():
    cmd = clipboard_reader()
    if not cmd:
        return None
    return subprocess.run(cmd, capture_output=True).stdout


def osc52_seq(payload, opcode=52, term=b"\x1b\\"):
    return (b"\x1b]" + str(opcode).encode() + b";c;" + base64.b64encode(payload) + term)


# The shape, with the terminator captured: crossterm's `osc!` uses ST (ESC \), and
# BEL is the spelling other tools use. Captured rather than assumed, because the
# terminator is half of what makes an OSC a well-formed one.
OSC52_RE = re.compile(rb"\x1b\]52;([a-zA-Z]*);([A-Za-z0-9+/=]*)(\x1b\\|\x07)")
OSC51_RE = re.compile(rb"\x1b\]51;([a-zA-Z]*);([A-Za-z0-9+/=]*)(\x1b\\|\x07)")


def group_clipboard(control=False):
    say("--- claim 2a: the OSC 52 bytes we put on the wire")
    size = 96
    want = ruler(size)
    p = run_probe(["osc52", str(size)], wait=6.0, ready="OSC52 payload_bytes")
    wire = p.raw_bytes()
    m = OSC52_RE.search(wire)
    check("clipboard: the emitted sequence is ESC ] 52 ; c ; <base64> ESC \\ ", bool(m),
          f"capture head: {wire[:48].hex()}")
    if m:
        got = base64.b64decode(m.group(2))
        at = wire.find(b"\x1b]52")
        measured("OSC 52 wire shape",
                 f"prefix={wire[at:at + 8]!r} b64_bytes={len(m.group(2))} "
                 f"payload_bytes={len(got)} terminator={m.group(3).hex()}")
        check("clipboard: the base64 body decodes to the exact bytes the copy asked for",
              got == want, f"{len(got)} vs {len(want)} bytes")
    p.close()

    if control:
        p = run_probe(["osc52-malformed", str(size)], wait=6.0, ready="OSC51 payload_bytes")
        wire = p.raw_bytes()
        check("control clipboard: the control emits opcode 51 and no OSC 52 anywhere",
              re.search(rb"\x1b\]51;c;", wire) is not None and b"\x1b]52;c;" not in wire,
              f"head {wire[:48].hex()}")
        p.close()
        return

    p = run_probe(["osc52", str(size), "--frag", "7"], wait=8.0, ready="OSC52 payload_bytes")
    wf = p.raw_bytes()
    m2 = OSC52_RE.search(wf)
    check("clipboard: a copy written in 7 chunks reassembles to one OSC 52 sequence",
          bool(m2) and base64.b64decode(m2.group(2)) == want, "no / mismatch")
    nchunks, biggest, _ = p.chunk_stats()
    measured("the fragmented copy arrived at the harness as", f"{nchunks} pty chunks (biggest {biggest}B)")
    p.close()

    p = run_probe(["osc52-query", "1200"], wait=4.0, ready="QUERY sent_at")
    q = re.search(r"QUERY bytes=(\d+) osc52_reply=(\w+)", p.text())
    if q:
        measured("read-back query (ESC ] 52 ; c ; ?) in a bare pty",
                 f"bytes_back={q.group(1)} recognised_reply={q.group(2)}")
        check("clipboard: a bare pty answers nothing to the read-back query (no emulator behind it)",
              q.group(2).lower() == "false", q.group(2))
    p.close()

    if clipboard_reader() and clipboard_writer():
        token = f"pdl2-tool-check-{int(time.time())}"
        subprocess.run(clipboard_writer(), input=token.encode(), check=False)
        back = read_clipboard()
        check(f"clipboard: this host's read-back tool ({clipboard_reader()[0]}) is honest",
              back == token.encode(), f"got {(back or b'')[:48]!r}")
    else:
        not_measured("2c. the OS clipboard read-back tool",
                    "no pbpaste / wl-paste / xclip on this host",
                    "install one; the emulator leg depends on it")


# --------------------------------------------------------------------------
# the real-terminal leg
# --------------------------------------------------------------------------


def tty_setraw(fd):
    a = termios.tcgetattr(fd)
    a[0] = 0                                      # iflag: no translation, no flow control
    a[1] = 0                                      # oflag: nothing translated on output
    a[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
    a[3] = 0                                      # lflag: no echo, no signals, no canonical
    a[4] = termios.B38400
    a[5] = termios.B38400
    a[6][termios.VMIN] = 1
    a[6][termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW, a)


class RawStdin:
    """Raw mode on the tty we are actually sitting in, restored on the way out."""

    def __enter__(self):
        self.fd = sys.stdin.fileno()
        self.saved = termios.tcgetattr(self.fd)
        tty_setraw(self.fd)
        RAW_OUT[0] = True
        return self

    def __exit__(self, *exc):
        try:
            termios.tcsetattr(self.fd, termios.TCSANOW, self.saved)
        except Exception:
            pass
        RAW_OUT[0] = False

    def drain(self):
        n = 0
        while select.select([self.fd], [], [], 0.0)[0]:
            n += len(os.read(self.fd, 65536))
        return n

    def read_timed(self, seconds, idle=0.0):
        """Read for `seconds`, timestamping every arrival. [(t, bytes), ...]."""
        out = []
        deadline = time.time() + seconds
        while time.time() < deadline:
            left = max(deadline - time.time(), 0.0)
            if select.select([self.fd], [], [], min(0.05, left or 0.05))[0]:
                try:
                    d = os.read(self.fd, 65536)
                except OSError:
                    break
                if not d:
                    break
                out.append((time.time(), d))
        return out


def decrq(fd_out, fd_in, mode, timeout=1.2):
    """Ask the emulator whether it has a DEC private mode, and read the answer.

    `CSI ? <mode> $ p` -> `CSI ? <mode> ; <ps> $ y`, where ps is 1=set, 2=reset,
    3=not recognised. This is the emulator answering a question about itself,
    which is the only way to find out whether mouse tracking exists here at all
    before the app commits to needing it.
    """
    os.write(fd_out, b"\x1b[?%d$p" % mode)
    deadline = time.time() + timeout
    buf = bytearray()
    while time.time() < deadline:
        if select.select([fd_in], [], [], 0.05)[0]:
            buf.extend(os.read(fd_in, 4096))
            m = re.search(rb"\x1b\[\?%d;(\d+)\$y" % mode, bytes(buf))
            if m:
                return int(m.group(1)), bytes(buf)
    return None, bytes(buf)


def wait_for_clipboard_change(before, timeout=6.0):
    t0 = time.time()
    while time.time() - t0 < timeout:
        cur = read_clipboard() or b""
        if cur != before:
            return cur, time.time() - t0
        time.sleep(0.02)
    return None, time.time() - t0


def clipboard_ladder(sizes=None, where=""):
    """Escalate the OSC 52 payload until a copy stops landing whole.

    Each size carries its own marker, so "landed intact" is a statement about the
    bytes that came back and not about a hunch. The latency is the
    permission-prompt proxy: a copy that takes more than about a second and a half
    to land was gated by something a human had to answer.

    The ladder stops at the first failure rather than running to the end, which is
    what keeps an emulator that has no OSC 52 at all from being fed a megabyte of
    base64 that it will print to the screen as text.
    """
    sizes = sizes or [int(s) for s in os.environ.get(
        "PDL2_SIZES", "64,1024,16384,65536,262144,1048576").split(",")]
    biggest = 0
    for size in sizes:
        payload = ruler(size)
        before = read_clipboard() or b""
        os.write(TTY[0], osc52_seq(payload))
        sys.stdout.flush()
        landed, lat = wait_for_clipboard_change(before)
        if not landed:
            measured(f"{where} osc52 {size}B", f"nothing landed in {lat:.2f}s")
            check(f"{where} osc52: a {size}B copy reaches the clipboard", False,
                  f"clipboard unchanged after {lat:.2f}s")
            break
        intact = landed == payload
        marker = f"<<END-PAYLOAD-{size}>>".encode() in landed
        measured(f"{where} osc52 {size}B",
                 f"landed={len(landed)}B intact={intact} marker={marker} latency={lat:.2f}s")
        check(f"{where} osc52: a {size}B copy lands byte-exact", intact,
              f"{len(landed)}B landed vs {size}B sent, marker present={marker}")
        if not intact:
            say(f"        a short copy without its marker is the silent clamp the ticket warns about")
            measured(f"{where} osc52 clamp", f"between {biggest}B and {size}B")
            break
        biggest = size
        if lat > 1.5:
            say(f"        {lat:.2f}s to land: something was answered by a human in the middle")
    if biggest:
        measured(f"{where} osc52 largest copy that landed whole", f"{biggest}B")
    return biggest


def parse_reports(raw):
    """SGR mouse reports out of a raw byte string, in arrival order."""
    return [(int(m.group(1)), int(m.group(2)), int(m.group(3)), m.group(4).decode())
            for m in re.finditer(rb"\x1b\[<(\d+);(\d+);(\d+)([Mm])", raw)]


def record_flicks():
    say("--- trackpad / mouse recorder (a real hand is the input here)")
    emit("")
    emit("    You have 8 seconds. Please:")
    emit("      1. do three separate trackpad flicks (one swipe each), and")
    emit("      2. then shift-drag across some text, and")
    emit("      3. then press 'y' if the terminal's OWN selection appeared, else 'n'.")
    emit("")
    with RawStdin() as tty:
        tty.drain()
        chunks = tty.read_timed(8.0)
        time.sleep(0.3)
        chunks += tty.read_timed(0.5)
    raw = b"".join(c for _, c in chunks)
    reports = parse_reports(raw)
    wheels = [r for r in reports if r[0] >= 64]
    shifts = [r for r in reports if r[0] & 4]
    says = b"".join(c for _, c in chunks)
    say(f"    {len(reports)} mouse reports read from the real terminal")
    if wheels:
        # A burst = reports whose arrivals are not separated by a read gap of 120 ms
        # or more. Reads bunch what the wire delivered, which is what a terminal does
        # to an app too, so this is the shape the app would actually see.
        bursts, cur = [], []
        prev_t = None
        for (t, blob) in chunks:
            rs = parse_reports(blob)
            ws = [r for r in rs if r[0] >= 64]
            if not ws:
                continue
            if prev_t is not None and (t - prev_t) > 0.12 and cur:
                bursts.append(cur)
                cur = []
            cur.extend(ws)
            prev_t = t
        if cur:
            bursts.append(cur)
        measured("flick bursts (gap > 120 ms splits them)",
                 f"{len(bursts)} bursts, sizes={[len(b) for b in bursts]}")
        for i, b in enumerate(bursts, 1):
            say(f"        burst {i}: {len(b)} wheel reports  "
                f"up={sum(1 for r in b if r[0] == 64)} down={sum(1 for r in b if r[0] == 65)} "
                f"left={sum(1 for r in b if r[0] == 66)} right={sum(1 for r in b if r[0] == 67)}")
        measured("largest single flick (wheel reports)", max(len(b) for b in bursts))
        measured("total wheel reports this terminal sent for the whole session", len(wheels))
    else:
        not_measured("4b. the trackpad flick shape in this terminal",
                    "no wheel reports arrived: either no flick happened, or this terminal does not report the wheel at all",
                    "re-run --record-flick and flick; if still nothing, this terminal does not send wheel reports under mouse tracking, which is itself the answer")
    measured("shift-modified reports seen", len(shifts))
    answered_y = b"y" in says
    answered_n = b"n" in says
    if answered_y or answered_n:
        check(f"shift-drag native selection under mouse capture (human answered): "
              f"{'yes' if answered_y and not answered_n else 'no'}",
              answered_y and not answered_n,
              "the human said the terminal's own selection did NOT appear while we held the mouse")
    else:
        not_measured("5b. shift-drag native selection while we hold the mouse",
                     "no y/n keypress was recorded after the shift-drag prompt",
                     "re-run --record-flick and press y or n at the end")


def leg_in_terminal(record_flick=False, ssh_emu=False):
    term = os.environ.get("TERM_PROGRAM", "unknown")
    ver = os.environ.get("TERM_PROGRAM_VERSION", "")
    say(f"--- real-terminal leg in {term} {ver} (TERM={os.environ.get('TERM')})")
    if not sys.stdin.isatty():
        say("    this leg must run inside a real terminal window (needs a controlling tty)")
        return 2
    if not clipboard_reader():
        say("    no clipboard read-back tool (pbpaste / wl-paste / xclip); the leg would prove nothing")
        return 2
    try:
        TTY[0] = os.open("/dev/tty", os.O_WRONLY)
    except OSError as exc:
        say(f"    cannot open /dev/tty ({exc}): nothing here to interpret an OSC 52")
        return 2

    saved = read_clipboard()
    say(f"    clipboard saved ({len(saved or b'')} bytes); it goes back at the end")
    out = TTY[0]
    rc = 0
    try:
        with RawStdin() as tty:
            say("    --- positive control on the query path: Primary Device Attributes (CSI c)")
            tty.drain()
            os.write(out, b"\x1b[c")
            sys.stdout.flush()
            da = b"".join(c for _, c in tty.read_timed(1.5))
            # Apple Terminal answers DA1 (`ESC[?1;2c`) but not DECRQM; a modern
            # terminal answers both. Without this the leg could not tell "this
            # emulator does not have the mode" apart from "nobody is listening".
            da_ok = re.search(rb"\x1b\[\?[0-9;]*c", da)
            measured(f"{term} DA1 reply", da[:24].hex() if da else "no answer")
            check(f"{term}: the query path is live (DA1 answered), so a silence below means "
                  f"non-support and not a dead wire", bool(da_ok), f"got {da[:24]!r}")

            say("    --- DEC mode queries: does this emulator even have what we want?")
            tty.drain()
            for mode, name in ((1000, "mouse_report"), (1002, "mouse_drag"),
                              (1003, "any_motion"), (1006, "mouse_sgr"),
                              (1049, "alt_screen"), (2004, "bracketed_paste")):
                ps, _ = decrq(out, tty.fd, mode)
                meaning = {0: "answered but unavailable", 1: "set", 2: "reset",
                          3: "not recognised", None: "no answer at all"}.get(ps, f"ps={ps}")
                measured(f"{term} DECRQM ?{mode} ({name})", f"{ps} ({meaning})")
                # "Answered" is the check. Whether the answer means available is a
                # separate question, and answered-but-unavailable (ps=0) is a real
                # answer some emulators give for a mode they do have, so the code
                # is recorded rather than judged here -- the matrix below sorts it.
                check(f"{term}: mode ?{mode} ({name}) answers DECRQM at all", ps is not None,
                      f"answer={meaning}")
                if ps in (1, 2):
                    say(f"        ?{mode} reported available ({meaning})")

            say("    --- OSC 52 write ladder, each size checked against the real clipboard")
            clipboard_ladder(where=term)

            say("    --- control: the same payload under opcode 51 must NOT touch the clipboard")
            marker51 = b"pdl2-control-51"
            os.write(out, osc52_seq(marker51 + b"-" + ruler(128), opcode=51))
            sys.stdout.flush()
            time.sleep(1.2)
            after = read_clipboard() or b""
            check(f"{term}: an OSC 51 write leaves the clipboard alone "
                  f"(so an OSC 52 pass above is not a stale clipboard)",
                  marker51 not in after, "the control payload landed in the clipboard")

            say("    --- a copy written in 64-byte chunks: does the emulator reassemble it?")
            want = b"pdl2-frag-" + ruler(512)
            frag = osc52_seq(want)
            before = read_clipboard() or b""
            for i in range(0, len(frag), 64):
                os.write(out, frag[i:i + 64])
                sys.stdout.flush()
                time.sleep(0.02)
            landed, lat = wait_for_clipboard_change(before, 5.0)
            measured(f"{term} fragmented OSC 52",
                     f"landed={len(landed or b'')}B want={len(want)}B latency={lat:.2f}s")
            check(f"{term}: a copy written in 64-byte chunks lands whole", landed == want,
                  f"{len(landed or b'')}B of {len(want)}B")

            say("    --- OSC 52 read-back query: does this emulator answer it?")
            target = b"pdl2-readback-" + str(int(time.time())).encode()
            subprocess.run(clipboard_writer(), input=target, check=False)
            tty.drain()
            os.write(out, b"\x1b]52;c?\x1b\\")
            sys.stdout.flush()
            got = b"".join(c for _, c in tty.read_timed(2.5))
            m = re.search(rb"\x1b\]52;([A-Za-z]*);([A-Za-z0-9+/=]*)", got)
            if m and m.group(2):
                answered = base64.b64decode(m.group(2))
                measured(f"{term} OSC 52 read-back",
                         f"answered=True b64_bytes={len(m.group(2))} decoded={len(answered)}B "
                         f"matches_clipboard={answered == target}")
                check(f"{term}: the OSC 52 read-back query answers with the clipboard contents",
                      answered == target, f"{answered[:32]!r} vs {target[:32]!r}")
            else:
                measured(f"{term} OSC 52 read-back",
                         f"answered=False bytes_back={len(got)}")
                check(f"{term}: the OSC 52 read-back query answers at all", False,
                      f"no OSC 52 reply in {len(got)} bytes read back")

            if ssh_emu:
                ssh_emulator_leg(out)
            if record_flick:
                record_flicks()
    finally:
        RAW_OUT[0] = False
        if TTY[0] is not None:
            try:
                os.close(TTY[0])
            except OSError:
                pass
            TTY[0] = None
        if saved is not None and clipboard_writer():
            subprocess.run(clipboard_writer(), input=saved, check=False)
            say(f"    clipboard restored ({len(saved)} bytes)")
    return rc


def ssh_emulator_leg(out):
    say("    --- OSC 52 written by a REMOTE host, through a real SSH session, into this emulator")
    if not ssh_up():
        not_measured("3b. OSC 52 across a real SSH session",
                     "no usable ssh target (docker sshd never came up)",
                     "run `--ssh` on a host with docker, or point SSH_TARGET_* at any sshd with the probe copied in")
        return
    size = 4096
    before = read_clipboard() or b""
    t0 = time.time()
    r = subprocess.run(ssh_cmd(["/usr/local/bin/spike_mouse_probe", "osc52", str(size)]),
                      stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    os.write(out, r.stdout)
    sys.stdout.flush()
    landed, lat = wait_for_clipboard_change(before, 6.0)
    landed_s = (landed or b"").decode("utf-8", "replace")
    measured("ssh osc52 (remote host -> this emulator)",
             f"ssh_rc={r.returncode} landed={len(landed or b'')}B "
             f"whole={f'<<END-PAYLOAD-{size}>>' in landed_s} latency={time.time() - t0:.2f}s")
    check("ssh: an OSC 52 copy written on the remote host reaches this emulator's clipboard whole",
          f"<<END-PAYLOAD-{size}>>" in landed_s, f"{len(landed or b'')}B landed")


# --------------------------------------------------------------------------
# the SSH leg
# --------------------------------------------------------------------------

SSH_PORT = os.environ.get("PDL2_SSH_PORT", "2222")
SSH_IMAGE = "looprs-pdl2-sshd:local"
SSH_CONTAINER = "looprs-pdl2-sshd"
SSH_KEY = os.environ.get("PDL2_SSH_KEY", "/tmp/looprs-pdl2-key")
LINUX_PROBE = os.environ.get("PDL2_LINUX_PROBE", "/tmp/pdl2probe-linux")
REMOTE_PROBE = "/usr/local/bin/spike_mouse_probe"


def sh(args, **kw):
    return subprocess.run(args, capture_output=True, text=True, **kw)


def docker(*args, **kw):
    return sh(["docker"] + list(args), **kw)


def ssh_cmd(remote_argv, alloc_tty=True):
    cmd = ["ssh", "-p", SSH_PORT, "-i", SSH_KEY,
           "-o", "StrictHostKeyChecking=no", "-o", "UserKnownHostsFile=/dev/null",
           "-o", "PreferredAuthentications=publickey", "-o", "LogLevel=ERROR",
           "-o", "ConnectTimeout=5"]
    # `-tt`, not the default. Whether ssh gives the far side a pty is exactly the
    # thing that decides whether a mouse report can reach an app at all: without it
    # the bytes still arrive, and crossterm's event layer yields nothing from a
    # stdin that is a pipe. The measured pair is in group_ssh below.
    cmd += ["-tt"] if alloc_tty else ["-T"]
    return cmd + ["root@127.0.0.1"] + list(remote_argv)


def build_linux_probe():
    """A Linux build of the probe, cached at LINUX_PROBE.

    The macOS binary is not going to run in the container and cross-compiling is not
    set up here, so the build happens in `rust:1-slim-bookworm` from a throwaway
    context that holds nothing but the probe source and a two-line manifest. The
    probe deliberately has no dependency on the app (it duplicates the mode table
    rather than importing it), which is what makes this cheap.
    """
    if os.path.exists(LINUX_PROBE):
        return True
    ctx = tempfile.mkdtemp(prefix="pdl2-probe-build-")
    shutil.copy(PROBE_SRC, os.path.join(ctx, "main.rs"))
    with open(os.path.join(ctx, "Cargo.toml"), "w") as f:
        f.write('[package]\nname = "pdl2probe"\nversion = "0.1.0"\nedition = "2021"\n'
                '[dependencies]\ncrossterm = { version = "0.29.0", features = ["osc52"] }\n'
                "[profile.release]\n")
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write("FROM rust:1-slim-bookworm\n"
                "WORKDIR /b\n"
                "COPY Cargo.toml main.rs ./\n"
                "RUN mkdir -p src && cp main.rs src/main.rs && cargo build --release\n")
    b = docker("build", "-t", "looprs-pdl2-probe:local", ctx)
    if b.returncode != 0:
        say(f"    probe build failed: {b.stdout[-300:]} {b.stderr[-300:]}")
        shutil.rmtree(ctx, ignore_errors=True)
        return False
    c = docker("create", "--name", "pdl2-probe-tmp", "looprs-pdl2-probe:local", "true")
    if c.returncode != 0:
        shutil.rmtree(ctx, ignore_errors=True)
        return False
    cp = docker("cp", "pdl2-probe-tmp:/b/target/release/pdl2probe", LINUX_PROBE)
    docker("rm", "-f", "pdl2-probe-tmp")
    shutil.rmtree(ctx, ignore_errors=True)
    if cp.returncode != 0:
        say(f"    could not extract the Linux probe: {cp.stderr[:200]}")
        return False
    return True


def build_ssh_image():
    ctx = tempfile.mkdtemp(prefix="pdl2-sshd-")
    shutil.copy(LINUX_PROBE, os.path.join(ctx, "spike_mouse_probe"))
    shutil.copy(SSH_KEY + ".pub", os.path.join(ctx, "authorized_keys"))
    with open(os.path.join(ctx, "Dockerfile"), "w") as f:
        f.write("FROM debian:bookworm-slim\n"
                "RUN apt-get update \\\n"
                " && apt-get install -y --no-install-recommends openssh-server \\\n"
                " && rm -rf /var/lib/apt/lists/* \\\n"
                " && ssh-keygen -A \\\n"
                " && mkdir -p /var/run/sshd /root/.ssh && chmod 700 /root/.ssh\n"
                "COPY authorized_keys /root/.ssh/authorized_keys\n"
                "COPY spike_mouse_probe /usr/local/bin/spike_mouse_probe\n"
                "RUN chmod +x /usr/local/bin/spike_mouse_probe\n"
                "EXPOSE 22\n"
                'CMD ["/usr/sbin/sshd", "-D", "-e"]\n')
    b = docker("build", "-t", SSH_IMAGE, ctx)
    shutil.rmtree(ctx, ignore_errors=True)
    if b.returncode != 0:
        say(f"    sshd image build failed: {b.stdout[-300:]} {b.stderr[-300:]}")
        return False
    return True


def ssh_up():
    """Idempotent: build what is missing, run the container, report ready."""
    if not shutil.which("docker"):
        say("    no docker on this host")
        return False
    running = docker("ps", "-q", "-f", f"name=^{SSH_CONTAINER}$")
    if running.stdout.strip():
        return sh(ssh_cmd(["true"])).returncode == 0
    if not os.path.exists(SSH_KEY):
        sh(["ssh-keygen", "-t", "ed25519", "-f", SSH_KEY, "-N", "", "-q"])
    if docker("image", "inspect", SSH_IMAGE).returncode != 0:
        if not build_linux_probe() or not build_ssh_image():
            return False
    docker("rm", "-f", SSH_CONTAINER)
    r = docker("run", "-d", "--name", SSH_CONTAINER,
               "-p", f"127.0.0.1:{SSH_PORT}:22", SSH_IMAGE,
               "/usr/sbin/sshd", "-D", "-e")
    if r.returncode != 0:
        say(f"    sshd would not start: {r.stderr.strip()[:200]}")
        return False
    for _ in range(40):
        time.sleep(0.5)
        if sh(ssh_cmd(["true"])).returncode == 0:
            return True
    say("    ssh never accepted the key")
    return False


def group_ssh(control=False):
    say("--- the SSH leg: a real sshd, a Linux build of the probe, key auth")
    if not ssh_up():
        not_measured("the SSH leg (claims 1, 2 and 3 across a real hop)",
                     "no usable ssh target: docker is missing, or the container never came up",
                     "on a host with docker: `python3 spikes/mouse_clipboard_e2e.py --ssh`; "
                     "or copy the probe to any sshd and point PDL2_SSH_KEY/PDL2_SSH_PORT at it")
        return
    say(f"    sshd up on 127.0.0.1:{SSH_PORT}; probe at {REMOTE_PROBE} (linux)")
    r = sh(ssh_cmd(["true"]))  # a liveness read on the key auth, nothing more
    say(f"    remote shell reachable (rc={r.returncode})")

    if control:
        one = sgr(0, 12, 7) + sgr(32, 13, 7) + sgr(0, 13, 7, release=True)
        p = Pty(ssh_cmd([REMOTE_PROBE, "events", "800", "--decode-off"]))
        if not p.wait_for("HELLO", 20):
            check("ssh control: the remote probe came up under ssh", False, p.text()[:160])
            p.close()
            return
        p.go_raw()
        p.send(one)
        p.wait_for("RAWREAD", 12)
        time.sleep(0.5)
        got = re.search(r"RAWREAD bytes=(\d+)", p.text())
        check("ssh control: the whole mouse-report payload survives the hop, byte for byte",
              int(got.group(1) if got else -1) == len(one),
              f"{got.group(1) if got else '?'} of {len(one)} bytes")
        p.close()
        return

    inj = [sgr(code, 12 + j, 7 + j) for j, (_, code, _, _) in enumerate(SGR_TABLE)]
    p = Pty(ssh_cmd([REMOTE_PROBE, "events", "1200"]))
    if not p.wait_for("HELLO", 25):
        check("ssh: the remote probe came up under a real ssh session", False, p.text()[:200])
        p.close()
        return
    measured("ssh allocated a remote pty", "yes (-tt)" if p.go_raw() else "yes, raw set failed")
    t0 = time.time()
    for blob in inj:
        p.send(blob)
    p.wait_for("SUMMARY", 20)
    rt = time.time() - t0
    time.sleep(0.5)
    evs = ev_mouse(p.text())
    say(f"    remote {p.lines('SUMMARY')[0] if p.lines('SUMMARY') else 'no summary'}")
    check("ssh: mouse reports written here decode remotely, one event per report",
          len(evs) == len(inj), f"{len(evs)} events for {len(inj)} reports")
    if len(evs) == len(inj):
        wrong = [(lbl, e["kind"], e["btn"])
                 for (lbl, _c, kind, btn), e in zip(SGR_TABLE, evs)
                 if (e["kind"], e["btn"]) != (kind, btn)]
        check("ssh: the kind+button mapping survives the hop unchanged", not wrong,
              f"{wrong[:3]}")
        offs = sorted({((12 + j) - e["x"], (7 + j) - e["y"])
                      for j, e in enumerate(evs)})
        measured("ssh wire coordinate minus reported cell", offs)
        check("ssh: the coordinate mapping is still a constant -1 on both axes",
              offs == [(1, 1)], f"{offs}")
    measured("inject -> last report back, over real SSH", f"{rt:.2f}s for {len(inj)} reports")
    p.close()

    # The same injection with no remote pty: this is `ssh host cmd` from a script,
    # where stdin on the far side is a pipe. Measured pair, because it is the
    # difference between "the transport ate it" and "the app has no tty to read a
    # mouse report from" -- and the second one is the app's problem, not ssh's.
    nopy = Pty(ssh_cmd([REMOTE_PROBE, "events", "900", "--decode-off"], alloc_tty=False))
    if nopy.wait_for("HELLO", 20):
        one = sgr(0, 12, 7) + sgr(32, 13, 7)
        nopy.go_raw()
        nopy.send(one)
        nopy.wait_for("RAWREAD", 12)
        time.sleep(0.4)
        got = re.search(r"RAWREAD bytes=(\d+)", nopy.text())
        got = int(got.group(1)) if got else -1
        has_pts = "/dev/pts" in nopy.text()
        measured("ssh -T (no remote pty)",
                 f"remote_has_pts={has_pts} bytes_arrived={got}/{len(one)} "
                 f"events_decoded={len(ev_mouse(nopy.text()))}")
        check("ssh without a remote pty still carries every injected byte",
              got == len(one), f"{got} of {len(one)}")
    nopy.close()

    size = 4096
    r = subprocess.run(ssh_cmd([REMOTE_PROBE, "osc52", str(size)]),
                       stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    m = OSC52_RE.search(r.stdout)
    check("ssh: an OSC 52 copy emitted on the remote host arrives here as one well-formed sequence",
          bool(m) and base64.b64decode(m.group(2)) == ruler(size),
          f"rc={r.returncode} head={r.stdout[:48].hex()}")
    if m:
        measured("ssh osc52 wire", f"b64_bytes={len(m.group(2))} payload_bytes={size} "
                                  f"terminator={m.group(3).hex()} "
                                  "(whether it then lands is the emulator leg's question)")
    say("        Whether the emulator in front of the ssh client *accepts* those bytes is")
    say("        measured by the same copy issued inside a real terminal window:")
    say("        `--launch --ssh-emulator`.")


# --------------------------------------------------------------------------
# running the emulator leg in its own window
# --------------------------------------------------------------------------


def leg_in_window(extra_args=(), label="terminal"):
    """Run the emulator leg in a real Terminal window and fold its results in.

    `open -a Terminal <file>.command` needs no automation permission, which is why
    this works from a headless harness at all. The window is left behind when the
    leg finishes; closing it would need the automation permission this does not have.
    """
    tmpl = os.environ.get("PDL2_LAUNCH", "open -a Terminal {file}")
    repo = os.getcwd()
    log = os.path.join("spikes", "results", f"emulator-leg-{label}.log")
    os.makedirs(os.path.dirname(log), exist_ok=True)
    wrapper = f"/tmp/looprs-pdl2-{label}.command"
    inner = " ".join(shlex.quote(a) for a in ["--in-terminal", *extra_args])
    with open(wrapper, "w") as f:
        f.write("#!/bin/sh\n"
                f"cd {shlex.quote(repo)} || exit 3\n"
                f"python3 spikes/mouse_clipboard_e2e.py {inner} 2>&1 | tee {shlex.quote(log)}\n"
                f"echo \"LEG_DONE rc=${{PIPESTATUS[0]}}\" | tee -a {shlex.quote(log)}\n")
    os.chmod(wrapper, 0o755)
    say(f"    launching the emulator leg: {tmpl.format(file=wrapper)}")
    say(f"    watching {log} (a permission prompt in that window is a finding: let it answer)")
    if "{file}" in tmpl:
        cmd = tmpl.format(file=wrapper).split()
    else:
        cmd = (tmpl + " " + wrapper).split()
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        say(f"    launcher failed: {r.stderr.strip()[:200]}")
        return False
    deadline = time.time() + float(os.environ.get("PDL2_LEG_TIMEOUT", "420"))
    seen = 0
    while time.time() < deadline:
        time.sleep(1.0)
        try:
            with open(log) as f:
                body = f.read()
        except OSError:
            continue
        for ln in body[seen:].splitlines():
            say(f"    | {ln}")
        seen = len(body)
        if "LEG_DONE" in body:
            fold_leg(body)
            return True
    say("    the leg never finished (nobody answered the prompt?)")
    return False


def fold_leg(body):
    """The leg ran in a window; count its checks here so one summary is the whole run."""
    for ln in body.splitlines():
        m = re.match(r"^.*?(PASS|FAIL)  (.+?)(?:  -- (.*))?$", ln)
        if m:
            RESULTS.append((m.group(2), m.group(1) == "PASS"))
        m = re.match(r"^.*?NUM   (.+?) = (.+)$", ln)
        if m:
            NUMBERS.append((m.group(1), m.group(2)))
        m = re.match(r"^.*?N/A   (.+?) -- (.+)$", ln)
        if m:
            NOT_MEASURED.append((m.group(1), m.group(2), "(see the leg log)"))


# --------------------------------------------------------------------------
# main
# --------------------------------------------------------------------------

GROUPS = {
    "inject": group_inject,
    "burst": group_burst,
    "shift": group_shift,
    "modes": group_modes,
    "vim": group_vim,
    "clipboard": group_clipboard,
    "ssh": group_ssh,
}


def main(argv):
    flags = {a for a in argv if a.startswith("--")}
    wanted = {a.lower() for a in argv if not a.startswith("--")}
    control = "--control" in flags

    if "--in-terminal" in flags:
        return leg_in_terminal(record_flick="--record-flick" in flags,
                              ssh_emu="--ssh-emulator" in flags)
    if "--launch" in flags:
        ok = leg_in_window(extra_args=sorted(flags & {"--record-flick", "--ssh-emulator"}),
                          label=os.environ.get("PDL2_LABEL", os.environ.get("TERM_PROGRAM", "terminal")))
        if not ok:
            say("the launched leg never reported done")
            return 1
    else:
        names = list(wanted) or [g for g in GROUPS if g != "ssh"]
        for g in names:
            if g not in GROUPS:
                say(f"unknown group {g!r} (known: {', '.join(GROUPS)})")
                return 2
            GROUPS[g](control=control)

    failed = [n for n, ok in RESULTS if not ok]
    say("=== summary ===")
    say(f"{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed"
        + (" [CONTROL RUN: a pass here means the control behaved like a control]" if control else ""))
    if failed:
        say("failed: " + "; ".join(failed))
    say(f"{len(NUMBERS)} numbers recorded" +
        (" -- " + "; ".join(f"{k}={v}" for k, v in NUMBERS[:4]) + ", ..." if NUMBERS else ""))
    if NOT_MEASURED:
        say(f"{len(NOT_MEASURED)} thing(s) this environment could not measure:")
        for c, w, h in NOT_MEASURED:
            say(f"  - {c}: {w} -> {h}")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
