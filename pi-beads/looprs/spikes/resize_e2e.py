#!/usr/bin/env python3
"""A window drag on a quiet session, measured in a real pty (looprs-pdl.15).

The ticket is the shortest sentence in the epic and the hardest to believe:
**the app is idle, the user drags the window, and the old frame stays up.**
This file is the whole argument about why that happened, run twice — once in the
harness that produced the ticket and once in the relationship a real terminal
window actually has with the program inside it.

Why two harnesses were needed. `looprs-pdl.6` measured **zero bytes** out of an
idle app over 2s after a `TIOCSWINSZ`, saw the same zero on the pre-scrollback
binary at `195e3c0`, and filed the discrepancy. That measurement was made with
`Popen(stdin=slave)` and nothing else, which puts the app in a pty it never
made its **controlling terminal**. `SIGWINCH` — the only thing that had ever
told this app about a resize — is not delivered to whatever process is drawing
on the terminal; it is delivered to the **foreground process group of the
terminal's session** (`tty_ioctl(4)`). The app was not in that group, so it
was never going to hear. Group `attached` runs the identical checks against a
child that did `setsid()` + `TIOCSCTTY`, and there even the pre-ticket binary
repaints. That is the evidence that `App::set_window` and the `Event::Resize`
arm were never the bug: the bare harness was measuring its own missing signal.

That is half the ticket. The rest is that **the app should not have to care.**
"Repaint when the window changes size" is a property of this app, and it was
riding on one chain none of which is ours: a signal the kernel may not send us,
into a stream we do not own, onto a `dirty` flag. The fix is
`viewport::WindowPoll` — ask the `ioctl` what the window is once a tick, and
adopt the answer if it differs. `TIOCGWINSZ` reads the descriptor this process
already holds: no process group, no signal, no emulator. It reports the drag in
both harnesses for about one microsecond per frame already painted.

What the groups check:

  * **idle first** (`wait_idle`). Zero bytes out over 1.5s, verified *before*
    every drag. Without it "a repaint followed the resize" could be a repaint
    that was already running, and this whole file would be measuring a spinner.
  * **a repaint follows the drag**, with the time to it printed. The poll's
    bound is one tick (16ms); the budget here is 400ms.
  * **the frame is the new size.** The input box's right border — the widest
    thing the app paints — lands on the new right edge and not the old one.
  * **the transcript re-wraps** (group `rewrap`): prose that occupies one row
    at 120 columns comes back as two at 60, its last word no longer on its
    first word's row. This is the part `pdl.6` said the wire could not show;
    with the repaint forced and the grid fresh at the new size, it can.
  * **a held child gets its window too** (group `held`): a full-screen program
    lives on a *different* pty, so nothing but this app resizes it. The child
    reports its own `stty size` on the way out.
  * **it settles.** One repaint per drag, then silence again. A poll that woke
    the app every tick would show up here as a steady byte rate — a worse bug
    than the one being fixed.

Two things this grid cannot show, both stated rather than skipped:

  * **"the user kept the line they were looking at" is not claimed here.**
    `Screen` (borrowed from `status_e2e.py`) reconstructs painted cells; it is
    not a reflowing emulator, and the app gives it nothing to reflow with. That
    half lives in `a_resize_keeps_the_line_the_user_was_looking_at_on_screen`
    (`src/main.rs`), where the screen model is ratatui's own buffer.
  * **shell output is not re-wrapped at all, at any width, and that is by
    design.** `MessageKind::Bash` is raw: ADR-0001 rule 1 and ADR-0005 say the
    *child* ends a line and nothing re-wraps it, so a shell line longer than
    the window is cut at the right edge before this ticket and after it. The
    re-wrap claim is therefore made on **prose** — the `Answer` kind, which is
    what `Scrollback::rewrap` actually re-renders — and a `bash` line is used
    only to show the contrast. Changing that is a decision, not a fix, and it
    is not this one.

Run:

    cargo build
    python3 spikes/resize_e2e.py                  | tee spikes/results/resize-e2e.log
    python3 spikes/resize_e2e.py drag held        # one group at a time

    # the control: the same script against the pre-looprs-pdl.15 binary. Every
    # `adopt`-kind check must fail in the bare relationship; one that passes
    # there is a needle that is not measuring the repaint.
    git worktree add --detach /tmp/looprs-pdl15-ctl HEAD
    (cd /tmp/looprs-pdl15-ctl/pi-beads/looprs && cargo build --target-dir /tmp/pdl15-target)
    LOOPRS_BIN=/tmp/pdl15-target/debug/looprs python3 spikes/resize_e2e.py --control \
        | tee spikes/results/resize-e2e-control.log
"""

import argparse
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
START = time.time()
RESULTS = []

# The two relationships a process can have with the pty it draws into. `bare` is
# how every other spike in this directory spawns the app: the pty is on
# stdin/stdout and that is the whole of it, which is why no SIGWINCH ever
# arrives. `attached` is a real terminal window's relationship.
BARE, ATTACHED = "bare", "attached"

# Window sizes. 120 and 60 are the re-wrap pair (see `rewrap`); a drag walks
# the widths between them.
START_ROWS, START_COLS = 30, 100
WIDE_ROWS, WIDE_COLS = 30, 120
NARROW_ROWS, NARROW_COLS = 30, 60
DRAG_SIZES = [(30, 118), (30, 110), (30, 96), (30, 84), (30, 72), (30, 64)]

# Prose for the re-wrap: one row at 120 columns (inner ~118), two rows at 60
# (inner ~58). Both needles arrive only through the fake `pi`'s reply text, so
# the echo of the prompt that asked for them can never satisfy a check.
PROSE_HEAD = "PROSEHEAD"
PROSE_TAIL = "PROSETAIL"
PROSE = f"{PROSE_HEAD} one two three four five six seven eight nine ten {PROSE_TAIL}"

# The held-screen child reports its own window through this marker.
HELD_SIZE = "HELDSIZE"

IDLE_SECONDS = 1.5
PAINT_BUDGET_S = 0.4  # the poll's own bound is 0.016; this is headroom x25


def say(msg):
    print(f"[{time.time() - START:6.2f}s] {msg}", flush=True)


# Two kinds of check, because `--control` has to know which ones are allowed to
# pass on a binary with no window poll:
#   "adopt" — a positive claim about the resize repaint itself. In the bare
#             relationship every one of these must fail pre-fix.
#   "app"   — about the app rather than the repaint: alive, idle before the
#             drag, no cursor query. Legitimately true before the fix too.
def check(name, ok, detail="", kind="adopt"):
    RESULTS.append((name, bool(ok), kind))
    mark = "PASS" if ok else "FAIL"
    say(f"{mark}  {name}" + (f"  -- {detail}" if not ok and detail else ""))


def until(pred, timeout=8.0, step=0.05):
    """Poll `pred` until it is truthy; return its last value."""
    deadline = time.time() + timeout
    val = pred()
    while time.time() < deadline and not val:
        time.sleep(step)
        val = pred()
    return val


class Driver:
    """The app in a pty, its screen reconstructed off its own writes.

    `attached=False` reproduces every other spike's spawn exactly.
    `attached=True` adds the two calls that make the pty the child's
    controlling terminal — which is what puts the child in the process group
    `TIOCSWINSZ` signals, and therefore the only way to measure the *event*
    path at all.
    """

    def __init__(self, rows, cols, attached=False, pi_bin="/usr/bin/false", env_extra=None):
        self.master, slave = pty.openpty()
        self.rows, self.cols = rows, cols
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        env = dict(os.environ)
        env.update(
            TERM="xterm-256color",
            LOOPRS_PI_BIN=pi_bin,
            LOOPRS_BD_BIN="/bin/echo",
            LOOPRS_SHELL_BIN=os.environ.get("LOOPRS_SHELL_BIN", "/bin/bash"),
        )
        env.update(env_extra or {})
        kw = {}
        if attached:
            def _own_the_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 1)

            kw["preexec_fn"] = _own_the_terminal
        self.proc = subprocess.Popen(
            [BIN], stdin=slave, stdout=slave, stderr=slave, env=env, close_fds=True, **kw
        )
        os.close(slave)
        self.relationship = ATTACHED if attached else BARE
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
        """Fold every byte received since the last fold into the grid."""
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

    def text(self):
        with self.lock:
            return bytes(self.raw).decode("utf-8", errors="replace")

    def has(self, needle):
        s = self.grid()
        return any(needle in s.text(r) for r in range(s.rows))

    def rows_with(self, needle):
        s = self.grid()
        return [r for r in range(s.rows) if needle in s.text(r)]

    def send(self, b):
        os.write(self.master, b)

    def mark(self):
        with self.lock:
            return len(self.raw)

    def bytes_since(self, mark):
        with self.lock:
            return len(self.raw) - mark

    def idle_bytes(self, seconds=IDLE_SECONDS):
        """Bytes emitted over a quiet window; 0 means the app drew nothing."""
        m = self.mark()
        time.sleep(seconds)
        return self.bytes_since(m)

    def wait_idle(self, budget=12.0, window=IDLE_SECONDS):
        """Block until a whole window passes with nothing drawn.

        The drag checks need the silence to be *immediately* before the drag,
        not merely somewhere earlier in the run, or "the resize made it draw"
        has no antecedent to contrast with. Each attempt is a whole window, so
        the drag that follows a `True` follows `window` seconds of zero bytes.
        """
        return until(lambda: self.idle_bytes(window) == 0, timeout=budget, step=0.0)

    def wait_for_bytes(self, mark, budget_s):
        """Seconds until the next byte after `mark`, or None if the budget ran out."""
        t0 = time.time()
        while time.time() - t0 < budget_s:
            if self.bytes_since(mark) > 0:
                return time.time() - t0
            time.sleep(0.005)
        return None

    def resize(self, rows, cols):
        """Move the window, and start a fresh grid at the new size.

        The fresh grid is the whole method. It is deliberately *not* a replay
        of the older bytes into the new shape: everything already in the grid at
        the moment of the drag is content painted at the old size, so a grid
        that carried it forward would let a repaint that never happened look
        like a screen full of correct content. Empty grid, then whatever the
        app paints next, is the only honest "what is on the screen now" here.
        """
        self.rows, self.cols = rows, cols
        mark = self.mark()
        fcntl.ioctl(
            self.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)
        )
        self.screen = Screen(rows, cols)
        with self.lock:
            self.fed = len(self.raw)
        return mark

    def max_col(self):
        """Rightmost painted column in the grid (-1 if nothing is painted)."""
        s = self.grid()
        right = -1
        for r in range(s.rows):
            for c, ch in enumerate(s.grid[r]):
                if ch != Screen.BLANK:
                    right = max(right, c)
        return right

    def box_right(self):
        """Rightmost column holding an input-box border glyph."""
        s = self.grid()
        border = set("┌┐└┘├┤┬┴│")
        right = -1
        for r in range(s.rows):
            for c, ch in enumerate(s.grid[r]):
                if ch in border:
                    right = max(right, c)
        return right

    def alive(self):
        return self.proc.poll() is None

    def quit(self):
        self.send(b"\x11")  # Ctrl-Q
        until(lambda: not self.alive(), timeout=5.0)

    def close(self):
        self.quit()
        if self.alive():
            self.proc.kill()


def bash_mode(d):
    """Tab Beads -> Pi -> Bash: idle, a real shell, no board, no model call."""
    d.send(b"\t")
    time.sleep(0.35)
    d.send(b"\t")
    time.sleep(0.6)
    return until(lambda: d.has("Bash"), timeout=4.0)


def boot(d, to_bash=True):
    """Start, and get into the mode the group needs."""
    time.sleep(1.5)
    check(f"the app boots ({d.relationship})", d.alive(), kind="app")
    if to_bash:
        check(f"Tab reaches Bash ({d.relationship})", bash_mode(d), "no Bash label", kind="app")


def drag_and_frame(d, rows, cols, label, kind="adopt"):
    """One drag: idle first, then the resize, then what the frame became.

    `kind` is handed to every claim except the idle one. The `attached` group
    passes "app", because with a controlling terminal the *event* path already
    did all of this before the fix — those checks are true twice over there and
    must not be counted as evidence about the poll.

    Returns True if a repaint arrived; the content checks only mean something
    after one.
    """
    quiet = d.wait_idle()
    check(
        f"{label}: the app was idle before the drag [{d.relationship}]",
        quiet,
        f"never went quiet for {IDLE_SECONDS}s before the drag",
        kind="app",
    )
    mark = d.resize(rows, cols)
    latency = d.wait_for_bytes(mark, PAINT_BUDGET_S)
    check(
        f"{label}: the drag repaints [{d.relationship}]",
        latency is not None,
        f"nothing painted within {PAINT_BUDGET_S * 1000:.0f}ms of the TIOCSWINSZ",
        kind=kind,
    )
    if latency is None:
        say(f"  {label}: no repaint; screen at {rows}x{cols}:\n{d.dump()}")
        return False
    say(f"  {label}: first byte {latency * 1000:.1f}ms after the drag")
    time.sleep(0.4)  # let the frame finish before reading the grid out of it
    check(
        f"{label}: the repaint reaches the new right edge [{d.relationship}]",
        d.max_col() == cols - 1,
        f"painted to column {d.max_col()}, the window is {cols} wide",
        kind=kind,
    )
    check(
        f"{label}: the input box's border lands on the new edge [{d.relationship}]",
        d.box_right() == cols - 1,
        f"border at column {d.box_right()}, the window is {cols} wide",
        kind=kind,
    )
    check(f"{label}: the app is alive after the drag [{d.relationship}]", d.alive(), kind="app")
    return True


def settle_check(d, label):
    """The poll must not turn a repaint into a repaint loop."""
    late = d.idle_bytes()
    check(
        f"{label}: the app settles back to silence after the repaint [{d.relationship}]",
        late == 0,
        f"{late} bytes out over {IDLE_SECONDS}s — something is still drawing",
        kind="app",
    )


def no_cursor_query(d, label):
    """The resize path may not ask the terminal anything (`ADR-0006` rule)."""
    wire = d.text()
    check(
        f"{label}: nothing on the wire is a cursor query",
        "\x1b[6n" not in wire,
        "ESC[6n appeared: the app asked the terminal where the cursor is",
        kind="app",
    )


# ─────────────────────────────── the groups ───────────────────────────────


def group_untouched():
    """The ticket's own case: `Popen(stdin=slave)`, no controlling terminal.

    This is the harness `pdl.6` measured with and the shape of every other
    spike here. Nothing in it will ever deliver a `SIGWINCH` to the app, so a
    repaint after the drag can only have come from the size `ioctl`.
    """
    say("== group untouched: an idle drag in a pty with no controlling terminal ==")
    d = Driver(START_ROWS, START_COLS, attached=False)
    try:
        boot(d)
        check(
            "…and the frame is at the starting width",
            d.max_col() == START_COLS - 1,
            f"{d.max_col()} vs {START_COLS - 1}",
            kind="app",
        )
        if drag_and_frame(d, WIDE_ROWS, WIDE_COLS, "untouched wide"):
            settle_check(d, "untouched wide")
        no_cursor_query(d, "untouched")
    finally:
        d.close()


def group_attached():
    """The same drag with a real controlling terminal: `SIGWINCH` arrives.

    Every check here is `app`-kind, because the pre-fix binary passes them too.
    That *is* the finding: this group is the control on the harness, and the
    evidence that the resize handling `pdl.6` blamed was never broken. A
    process that is the foreground group of the terminal it draws on has always
    repainted on a drag — so the ticket's symptom is a bare-pty artifact, and
    the fix is not a repair of a broken path but the removal of the path's
    monopoly.
    """
    say("== group attached: an idle drag with a real controlling terminal ==")
    d = Driver(START_ROWS, START_COLS, attached=True)
    try:
        boot(d)
        # "app"-kind, all of it: with the terminal attached the pre-fix binary
        # passes these too, on the signal alone, so none of them may count as
        # evidence about the poll.
        if drag_and_frame(d, WIDE_ROWS, WIDE_COLS, "attached wide", kind="app"):
            settle_check(d, "attached wide")
        no_cursor_query(d, "attached")
    finally:
        d.close()


FAKE_PI = """#!/usr/bin/env python3
\"\"\"A `pi` that answers with prose this spike owns (looprs-pdl.15).

Protocol-identical to `spikes/fake_pi_slow.py` and deliberately closed rather
than held open: the re-wrap being measured is `Scrollback::rewrap` over *settled*
content, and a run that never ends leaves its text in the live preview, where
there is no stored row to re-wrap. One reply, one paragraph, no blank line, so
it lands as one `Answer` entry and wraps at whatever width the flush was asked
for.

The reply comes from `LOOPRS_REWRAP_REPLY` and not from argv: the app spawns
this bin as `pi --mode rpc`, so argv belongs to the protocol.
\"\"\"
import json
import os
import sys
import threading

REPLY = os.environ.get("LOOPRS_REWRAP_REPLY", "the reply was never set")
lock = threading.Lock()


def emit(obj):
    with lock:
        sys.stdout.write(json.dumps(obj) + "\\n")
        sys.stdout.flush()


def turn(user_text):
    emit({"type": "agent_start"})
    emit({"type": "turn_start"})
    emit({"type": "message_start", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_end", "message": {"role": "user", "content": user_text}})
    emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
    emit({"type": "message_update", "assistantMessageEvent": {
        "type": "text_delta", "contentIndex": 0, "delta": REPLY}})
    emit({"type": "message_update", "assistantMessageEvent": {
        "type": "text_end", "contentIndex": 0, "content": REPLY}})
    emit({"type": "message_end", "message": {"role": "assistant", "content": []}})
    emit({"type": "turn_end", "message": {"role": "assistant"}, "toolResults": []})
    emit({"type": "agent_end", "messages": [], "willRetry": False})
    emit({"type": "agent_settled"})


while True:
    line = sys.stdin.readline()
    if not line:
        break
    line = line.strip()
    if not line:
        continue
    try:
        cmd = json.loads(line)
    except ValueError:
        continue
    rid = cmd.get("id")
    kind = cmd.get("type")
    if kind == "prompt":
        emit({"type": "response", "id": rid, "command": "prompt", "success": True,
              "data": {"disposition": "started"}})
        threading.Thread(target=turn, args=(cmd.get("message", ""),), daemon=True).start()
    else:
        emit({"type": "response", "id": rid, "command": kind, "success": True, "data": {}})
"""


def group_rewrap():
    """The transcript, not the chrome: one row at 120, two rows at 60.

    The prose is written once, at the wide size, where it fits on a single row.
    Then the window narrows. If the store re-wrapped, the row that held head and
    tail together comes back as two rows and the tail is no longer on the head's
    row; if the app never repainted — or repainted without re-wrapping — the
    tail stays where it was, and this says so.

    Why prose and not a shell line: see the module docstring. `MessageKind::Bash`
    is raw and is *never* re-wrapped at any width (ADR-0001 rule 1, ADR-0005),
    so a resize cannot re-wrap it. The group prints that contrast as well as
    claiming the re-wrap.
    """
    say("== group rewrap: settled prose re-wraps into the narrower window ==")
    tmp = tempfile.mkdtemp(prefix="looprs-resize-rewrap-")
    fake = os.path.join(tmp, "fake_pi_rewrap.py")
    with open(fake, "w") as fh:
        fh.write(FAKE_PI)
    os.chmod(fake, 0o755)
    d = Driver(
        WIDE_ROWS,
        WIDE_COLS,
        attached=False,
        pi_bin=fake,
        env_extra={"LOOPRS_REWRAP_REPLY": PROSE},
    )
    try:
        check(f"the app boots ({d.relationship})", d.alive(), kind="app")
        d.send(b"\t")  # Beads -> Pi
        time.sleep(0.6)
        check("Tab reaches Pi", until(lambda: d.has("Pi"), timeout=4.0), "no Pi label", kind="app")
        d.send(b"wrap this answer please\r")
        if not until(lambda: d.has(PROSE_HEAD), timeout=10.0):
            check("the answer reaches the screen", False, f"no {PROSE_HEAD}:\n{d.dump()}")
            return
        head_rows = d.rows_with(PROSE_HEAD)
        wide_row = d.grid().text(head_rows[0]) if head_rows else ""
        check(
            f"the prose fits ONE row at {WIDE_COLS} columns (head and tail together)",
            bool(head_rows) and PROSE_TAIL in wide_row,
            f"head row {head_rows}: {wide_row!r}",
            kind="app",
        )
        check(
            "…and it is settled content, not the live tail",
            d.wait_idle(budget=12.0),
            "the answer never stopped streaming",
            kind="app",
        )

        mark = d.resize(NARROW_ROWS, NARROW_COLS)
        latency = d.wait_for_bytes(mark, PAINT_BUDGET_S)
        check(
            f"a narrowing drag repaints settled prose [{d.relationship}]",
            latency is not None,
            "nothing painted after the drag",
        )
        if latency is None:
            return
        time.sleep(0.6)
        narrow_head = d.rows_with(PROSE_HEAD)
        head_row = d.grid().text(narrow_head[0]) if narrow_head else ""
        tail_rows = d.rows_with(PROSE_TAIL)
        check(
            f"…and the head is still on screen at {NARROW_COLS} columns",
            bool(narrow_head),
            f"the head vanished:\n{d.dump()}",
        )
        check(
            "…and the tail is NOT on the head's row: the line came back re-wrapped",
            bool(narrow_head) and PROSE_TAIL not in head_row,
            f"the head row still carries the tail ({head_row!r}) — the store was not re-wrapped\n{d.dump()}",
        )
        check(
            "…and the tail is on a row of its own below the head",
            bool(narrow_head) and bool(tail_rows) and tail_rows[0] > narrow_head[0],
            f"head rows {narrow_head}, tail rows {tail_rows}:\n{d.dump()}",
        )
        no_cursor_query(d, "rewrap")
    finally:
        d.close()
        subprocess.run(["rm", "-rf", tmp], check=False)


def group_drag():
    """Six sizes in ~250ms: what a hand on the window edge actually does.

    A drag is not one resize, it is a burst, and `SIGWINCH` coalesces — which
    is exactly why the window has to be *read* rather than accumulated. The app
    must end painted at the last size it was given, not one from somewhere in
    the middle of the burst, and it must still go quiet afterwards.
    """
    say("== group drag: a burst of resizes, like a hand on the window edge ==")
    d = Driver(START_ROWS, START_COLS, attached=False)
    try:
        boot(d)
        check("…and it is idle before the drag", d.wait_idle(), "never went quiet", kind="app")
        final_rows, final_cols = DRAG_SIZES[-1]
        # The grid goes to the FINAL size *before* the burst, not after it.
        # The app repaints once per size it notices, so the answer to "what is
        # on the screen at the end of the drag" is the last of those frames —
        # and a grid reset at the end of the burst would sit after it, with
        # nothing left painted in it. Every intermediate frame is wiped by the
        # next one's full `ESC[2J` anyway; that is what a resize repaint is.
        d.screen = Screen(final_rows, final_cols)
        with d.lock:
            d.fed = len(d.raw)
        mark = d.mark()
        for rows, cols in DRAG_SIZES:
            fcntl.ioctl(
                d.master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0)
            )
            time.sleep(0.04)
        d.rows, d.cols = final_rows, final_cols
        latency = d.wait_for_bytes(mark, PAINT_BUDGET_S)
        check(
            f"the burst paints [{d.relationship}]",
            latency is not None,
            "not one byte in 400ms of six resizes",
        )
        if latency is None:
            return
        # Wait for the burst to be fully absorbed before reading the screen: the
        # last frame is the answer, so the check has to run after it rather
        # than while the earlier ones are still landing.
        until(lambda: d.box_right() == final_cols - 1, timeout=2.0)
        repaints = d.text().count("\x1b[2J")
        say(f"  full repaints (ESC[2J) the whole run so far cost: {repaints}")
        check(
            "…and the frame ends at the LAST size of the burst, not one from the middle",
            d.box_right() == final_cols - 1,
            f"border at column {d.box_right()}, final window {final_cols} wide\n{d.dump()}",
        )
        settle_check(d, "drag")
        no_cursor_query(d, "drag")
    finally:
        d.close()


def group_scrolled():
    """A drag taken while the user is reading history.

    The transcript is off the tail, the wheel has stopped, nothing is streaming.
    The drag has to repaint what is on screen without throwing the reader off it,
    and the app still has to be an app afterwards.
    """
    say("== group scrolled: a drag with the transcript scrolled up ==")
    d = Driver(WIDE_ROWS, WIDE_COLS, attached=False)
    try:
        boot(d)
        d.send(b"for i in $(seq 1 80); do echo \"HISTROW$i the transcript of a long session\"; done\r")
        if not until(lambda: d.has("HISTROW80"), timeout=15.0):
            check("80 lines reach the screen", False, d.dump())
            return
        d.send(b"\x1b[5~")  # PageUp
        check(
            "PageUp moves the view off the tail",
            until(lambda: not d.has("HISTROW80"), timeout=4.0),
            f"the tail is still shown:\n{d.dump()}",
            kind="app",
        )
        check(
            "…and the app is idle while the user reads",
            d.wait_idle(budget=12.0),
            "the held view never went quiet",
            kind="app",
        )
        mark = d.resize(NARROW_ROWS, NARROW_COLS)
        latency = d.wait_for_bytes(mark, PAINT_BUDGET_S)
        check(
            f"a drag repaints while the transcript is scrolled up [{d.relationship}]",
            latency is not None,
            "nothing painted while the view was off the tail",
        )
        check("…and the app is alive", d.alive(), kind="app")
        time.sleep(0.4)  # let the resized frame finish before reading the grid
        check("…and the repaint is at the narrow width", d.box_right() == NARROW_COLS - 1, f"{d.box_right()}", kind="app")
        d.send(b"\x1b[F")  # End
        check(
            "…and End still reaches the tail across the resize",
            until(lambda: d.has("HISTROW80"), timeout=10.0),
            f"the tail never came back:\n{d.dump()}",
            # "app": End is a keystroke, and a keystroke always made the app
            # draw — that is the very mechanism the ticket says the resize had
            # to wait for. It is proof the app survived, not proof the resize
            # was seen.
            kind="app",
        )
        settle_check(d, "scrolled")
        no_cursor_query(d, "scrolled")
    finally:
        d.close()


def group_held():
    """A drag while a full-screen program owns the screen.

    The child lives on a **different** pty. Nothing resizes it but this app, so
    a resize the app never heard about is a full-screen program left running at
    a window nobody is using — the same bug with `vim` in it, and one the event
    path could not have covered here either. The child reports its own
    `stty size` on the way out, which is the child's pty and not our guess
    about it.
    """
    say("== group held: a drag while a full-screen program has the screen ==")
    tmp = tempfile.mkdtemp(prefix="looprs-resize-held-")
    holder = os.path.join(tmp, "hold.sh")
    with open(holder, "w") as fh:
        fh.write(
            "#!/bin/bash\n"
            "printf '\\033[?1049h\\033[2JHELD-THE-SCREEN\\n'\n"
            "sleep 3\n"
            "printf '\\033[?1049l'\n"
            f'echo "{HELD_SIZE} $(stty size)"\n'
        )
    os.chmod(holder, 0o755)
    d = Driver(START_ROWS, START_COLS, attached=False)
    try:
        boot(d)
        d.send(f"bash {holder}\r".encode())
        held = until(lambda: d.has("HELD-THE-SCREEN"), timeout=10.0)
        check(
            "the full-screen program took the screen",
            held,
            f"no held screen appeared:\n{d.dump()}",
            kind="app",
        )
        if not held:
            return
        mark = d.resize(WIDE_ROWS, WIDE_COLS)
        say("  window dragged while the child holds the screen")
        # The child holds for 3s; the app should resize it inside one tick of
        # noticing, so this is the child's own clock, not our budget.
        time.sleep(6.0)
        report = " ".join(d.grid().text(r) for r in d.rows_with(HELD_SIZE))
        say(f"  the child reported: {report!r}")
        # `\D+` and not a literal space: the backend paints by diff, so the row
        # this lands on can carry cells of whatever the transcript put there
        # before (here, the echo of the command that ran the holder). The two
        # numbers are the payload; the separators are layout.
        nums = re.search(r"HELDSIZE\D+(\d+)\D+(\d+)", report)
        check(
            "the held child's OWN pty was resized to the new window",
            bool(nums) and (int(nums.group(1)), int(nums.group(2))) == (WIDE_ROWS, WIDE_COLS),
            f"the child says {nums.groups() if nums else 'nothing'}, the window is {WIDE_ROWS}x{WIDE_COLS}",
        )
        check(
            "…and the app repaints its own frame when the child hands the screen back",
            d.box_right() == WIDE_COLS - 1,
            f"painted to column {d.box_right()} after the hand-back",
            # "app", and worth knowing why: the hand-back repaint is
            # `repaint_all`, which existed before this ticket, and it reads the
            # window through ratatui's `autoresize` — which is how the old
            # binary paints *chrome* at a width it never adopted. What it could
            # not do pre-fix is wrap anything with it or resize the child; the
            # two checks above are the claims that actually moved.
            kind="app",
        )
        check("…and the app is alive", d.alive(), kind="app")
        settle_check(d, "held")
        no_cursor_query(d, "held")
    finally:
        d.close()
        subprocess.run(["rm", "-rf", tmp], check=False)


GROUPS = {
    "untouched": group_untouched,
    "attached": group_attached,
    "rewrap": group_rewrap,
    "drag": group_drag,
    "scrolled": group_scrolled,
    "held": group_held,
}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument(
        "--control",
        action="store_true",
        help="run against a binary expected NOT to poll the window; invert the verdict",
    )
    ap.add_argument("groups", nargs="*", help=f"a subset of: {', '.join(GROUPS)}")
    args = ap.parse_args()

    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2

    names = args.groups or list(GROUPS)
    for name in names:
        if name not in GROUPS:
            say(f"unknown group {name!r}; pick from {', '.join(GROUPS)}")
            return 2
    for name in names:
        try:
            GROUPS[name]()
        except Exception as exc:  # keep going; report loudly rather than silently
            check(f"group {name} completed", False, repr(exc), kind="app")

    failed = [n for n, ok, _ in RESULTS if not ok]
    say(f"{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed  (binary: {BIN})")
    if failed:
        say("failed: " + "; ".join(failed))
    if not args.control:
        return 1 if failed else 0

    informative = [(n, ok) for n, ok, k in RESULTS if k == "adopt"]
    leaked = [n for n, ok in informative if ok]
    say(
        f"control: {len(informative)} repaint-claim checks ran; {len(leaked)} of them "
        f"passed against a binary with no window poll"
    )
    if leaked:
        say("CONTROL BROKEN — these needles are not about the repaint: " + "; ".join(leaked))
        return 1
    say(
        "CONTROL OK: no repaint claim survives on the pre-pdl.15 bare harness, so the "
        "passing run measures the size ioctl and not merely a running app"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
