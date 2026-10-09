#!/usr/bin/env python3
"""The exit path, in a real pty, judged by bytes and by the process table.

looprs-ecr filed this spike; looprs-pdl.3 widened it into the terminal-mode
ledger's proof. One scenario per thing no unit test can see.

  1. **Nothing visible is lost.** Quit mid-stream, while the streamed answer is
     still open: the answer the user could see is still on the screen when the
     frame is left, and the exit drain gets its flush in. It used to go with the
     pane — `term.clear()` wiped the live area and `insert_before` never got the
     tail.
  2. **The terminal is handed back exactly once.** The capture ends with the
     alternate-screen leave and nothing after it; the pty is cooked again
     (`stty -a` equivalent, read straight off the tty the app was using); the
     exit code is 0; and there is no "cursor position could not be read"
     anywhere. That error used to be the *last line of every run*, because the
     final clear asked the terminal where the cursor was and the async key reader
     ate the answer.

**The shape of the hand-back changed in looprs-pdl.4, and this spike was
rewritten for it rather than left passing on the old premise.** The rules used to
be written against the inline pane: erase the pane, land the drained tail *above*
the erase line, end with one closing newline. Since the frame owns the alternate
screen there is no pane inside the user's screen to erase, nothing the app paints
reaches the user's scrollback at all, and the hand-back is `?1049l`. What is
checked now, in those scenarios' place:

  * the app painted **inside** the alternate screen and nowhere else — the
    user's own screen and scrollback come back exactly as they were found;
  * the answer was on the screen the frame left (the drain ran; nothing was
    dropped between the quit key and the leave);
  * **nothing is written after the leave.** The old closing newline belonged to
    the inline pane's last row. After `?1049l` the cursor belongs to the user's
    prompt, so a newline there is a blank line in their shell.

The transcript no longer landing in scrollback is the deliberate trade of
full-screen (ADR-0004 R1 as amended); looprs-pdl.5/6/7 give it a scrollback of
its own.
  3. **No child outlives the app.** Not the shell from the generated rcfile, not
     the pi child, not the `sleep 30` that shell was running — checked in the
     process table after the app is gone, which is where an orphan lives. Every
     one of those checks is scoped to the pids **this run spawned**, recorded
     while the app was still up, because the version that scanned the whole
     process table with `pgrep -f` measured the machine instead of the build under
     test (`looprs-00u.24`); see "which processes are ours" below. Debris from
     earlier runs is reaped at startup and **counted out loud**, so a dirty
     machine is visible rather than fatal, and `scenario_leak_is_named` SIGKILLs
     the app over a live shell to show the scoped check still fires on the pid it
     recorded — a leak check that cannot fail is the same defect facing the other
     way.
  4. **A wedged child cannot hold the door.** A fake pi that ignores stdin EOF
     *and* SIGTERM still gets killed inside the exit budget, and the app still
     comes back with its terminal.

  5. **Every mode we switch on gets switched off, exactly once** (pdl.3). Run
     with `LOOPRS_MODES=all` — alternate screen, mouse report/drag/SGR,
     bracketed paste, hidden cursor — and the spike's *own* ledger of the bytes on
     the wire says they were all taken and all given back, one leave sequence
     each, with the alternate-screen leave the last word the app wrote.
  6. **A signal from outside gets the same hand-back.** `SIGTERM` with the whole
     mode set held, and `SIGHUP` on the plain inline run: the app leaves by
     itself, inside budget, with nothing left switched on and no `ESC[6n` asked
     after the signal arrived. Before this ticket there was no handler at all —
     both signals killed the app where it stood and the user got a raw,
     cursorless tty back, with the pane still painted on it.
  7. **A panic inside the draw is the same hand-back too.** `LOOPRS_PANIC=draw`
     makes the frame panic on purpose; the ledger still leaves each mode exactly
     once, the tty is cooked again, and the exit code says something went wrong.

Every backend is a fake the script writes, so nothing here costs a model call.

    python3 spikes/shutdown_e2e.py | tee spikes/results/shutdown-e2e.log

**Why the spike keeps its own ledger.** "Not in the alternate screen after exit" is
not something the app can be asked at exit — asking is a `DECRQM`, and the exit
path is forbidden from asking anything of the terminal (that is the `ESC[6n` rule).
So the driver tracks the mode state from the sequences it *observed*, the way a
terminal would, and answers the question from the user's side of the pty. The
app's ledger and the spike's ledger are independent, which is the only way "we
handed it all back" can be a check rather than a claim. There is no terminal
emulator behind a pty anyway: a real `DECRQM` here would go unanswered, so what is
on the wire is the whole truth there is.
"""

import fcntl
import os
import pty
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
from collections import Counter
from typing import NamedTuple

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


# ─── which processes are ours ─────────────────────────────────────────────────
#
# `pgrep -f <pattern>`, which every leak check in this spike used to be built on,
# answers "is anything at all on this box running a command line that looks like
# X?". No shutdown check wants that question, and answering it costs the result
# twice over. From the finding (looprs-00u.24), one machine, one build: the spike
# came in 169/171 with both failures on the leak checks —
#
#     FAIL  no bash from the generated rcfile survived
#     FAIL  no shell from our rcfile outlived the app  -- ['56285','56561','56830','57419']
#
# — while `ps -eo pid,ppid,etime,command` showed every one of those four at least
# 28 minutes old and parented to launchd, five orphaned `looprs` processes with it
# and 263 abandoned rc files in `$TMPDIR`, none of them produced by the binary
# under test. Clear the abandoned runs out from under it and rerun: 171/171. The
# build was clean and the check had spent its two failures reading somebody else's
# history.
#
# Reproduced here rather than taken on trust: `spikes/plant_stale_debris.sh 3` over
# the pre-`00u.24` spike gives 168/171 — three failures, all of them leak checks,
# none of them the build's (`spikes/results/shutdown-e2e-00u24-globalpgrep-
# control.log`) — while the spike below passes 189/189 on the same dirtied machine
# (`spikes/results/shutdown-e2e-00u24-stale.log`).
#
# The mirror image is worse, not better: on a box that noisy, a leak the binary
# *did* cause lands in the same pile as everything it did not, and nothing about
# the result says which pile it came from. Either way the check stopped being
# attributable to the thing it is supposed to be testing.
#
# So the spike records the pids it spawns, and asserts on those.

# The generated bash integration file, and the shape of its name: `write_integration`
# in `src/session/bash.rs` names it after the pid of the app that wrote it
# (`looprs-bash-integration-<owner>-<seq>.sh`). That owner pid is why a leftover
# can be attributed after the fact without a manifest: the app it names is the only
# process that was ever going to clean the thing up, so when that pid is gone the
# leftover is orphaned by definition.
INTEGRATION_RE = re.compile(r"looprs-bash-integration-(\d+)-\d+\.sh")


def proc_table():
    """`{pid: (ppid, pgid, stat, command)}` for every process on the box.

    One `ps` per call rather than one `pgrep -f` per pattern, because the checks
    need the *tree* — who sits under whom — and the process *state*, neither of
    which `pgrep` prints. `-ww` because the rc file path that makes our shells
    identifiable sits at the end of a long argv, and a line truncated to the
    terminal width is a line that matches nothing: run under `| tee` in a narrow
    window and a `pgrep`-free check built on a width-limited `ps` would report a
    clean machine it simply could not see.
    """
    r = subprocess.run(
        ["ps", "-eww", "-o", "pid,ppid,pgid,stat,command"],
        capture_output=True, text=True,
    )
    table = {}
    for line in r.stdout.splitlines():
        fields = line.split(None, 4)
        if len(fields) < 5 or not fields[0].isdigit():
            continue
        try:
            table[int(fields[0])] = (int(fields[1]), int(fields[2]), fields[3], fields[4])
        except ValueError:
            continue
    return table


def running(table, pid):
    """Is `pid` executing? A zombie is not.

    A zombie has already exited — it is an unreaped exit status, not a running
    process, and it will be gone when its parent notices. "No child survived" is a
    claim about what is still executing, so counting a zombie as a survivor would
    fail the spike for a process that is not there.
    """
    entry = table.get(pid)
    return entry is not None and "Z" not in entry[2]


def pid_running(pid):
    """`running` with no table to hand. `EPERM` means the pid exists; hands off."""
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def descendants(root, table):
    """Everything under `root`, transitively, as `{pid: entry}`.

    `root` itself is never in the result. A leaked shell's `sleep 30` is a child of
    the shell rather than of the app, so a check that only looks one level down
    watches the shell and misses the thing the shell was told to run.
    """
    kids = {}
    for pid, entry in table.items():
        kids.setdefault(entry[0], []).append(pid)
    out, stack, seen = {}, [root], {root}
    while stack:
        for kid in kids.get(stack.pop(), ()):
            if kid in seen:
                continue
            seen.add(kid)
            out[kid] = table[kid]
            stack.append(kid)
    return out


def fmt_procs(procs, limit=6):
    """`71234(/bin/bash --rcfile …)` — a check's detail line.

    The pid leads because the detail gets read to answer "which one of ours?", and
    a list of command lines with no numbers on it answers nothing.
    """
    items = sorted(procs.items())
    shown = ", ".join(f"{p}({c})" for p, c in items[:limit])
    if len(items) > limit:
        shown += f", ... (+{len(items) - limit} more)"
    return shown or "none"


def _matches(pattern, cmd):
    """A compiled pattern searches, a plain string is a substring.

    Lets a caller write `pi_path` for "the fake pi we installed" and
    `INTEGRATION_RE` for "anything carrying a generated rc file" in the same
    argument.
    """
    return pattern.search(cmd) if hasattr(pattern, "search") else pattern in cmd


class RunLedger:
    """The pids this run started, remembered while they were still findable.

    Two things make "the children we spawned" a harder question than it looks:

      * they appear at unpredictable moments — a Bash session spawns on a tab
        change, that shell forks a `sleep` a second later — so the record is kept
        continuously while the app is up rather than taken once; and
      * the instant the app dies its children are reparented to pid 1, so a scan
        done *after* the exit can no longer tell that the thing it found belonged
        to the run that just finished. Recording after the fact is not possible,
        which is the whole reason this class exists.

    What it buys is attribution: a leak is a pid we recorded and can name, a
    machine dirty with someone else's abandoned runs cannot make it appear, and
    the same check is therefore still worth running on a dirty box.
    """

    def __init__(self):
        self.lock = threading.Lock()
        self.apps = {}      # app pid -> scenario label
        self.children = {}  # pid -> (app pid, label, command, pgid)

    def note_app(self, app_pid, label):
        with self.lock:
            self.apps[app_pid] = label

    def record_under(self, app_pid, label):
        """Add every live process under `app_pid` to the ledger.

        Additive and repeatable: the driver calls it on a timer, so nothing that
        appears between two calls is missed, and calling it again right before an
        exit trigger records whatever the timer had not got to yet.
        """
        table = proc_table()
        with self.lock:
            for pid, (_ppid, pgid, stat, cmd) in descendants(app_pid, table).items():
                if "Z" in stat:
                    continue
                self.children[pid] = (app_pid, label, cmd, pgid)

    def recorded(self, app_pid=None):
        with self.lock:
            if app_pid is None:
                return dict(self.children)
            return {p: e for p, e in self.children.items() if e[0] == app_pid}

    def survivors(self, app_pid=None, pattern=None):
        """`{pid: command}` for recorded children that are running **now**.

        Same call before an exit ("is the child up that we are about to test the
        killing of?") and after one ("did anything outlive the app"), which is
        what keeps the two halves reading the same set of pids rather than two
        different guesses about the machine.

        "Still running" is not enough: pids get handed out again, and a recycled
        one would be a survivor we never had. The recorded command line has to be
        the line that pid is carrying now, which makes "this is our child" a claim
        about two things rather than one number. Our children are bash, sleep, echo
        and looprs itself — none of them rewrites its argv, so the match is not
        fragile in the direction that would hide a real leak.
        """
        table = proc_table()
        out = {}
        for pid, (_app, _label, cmd, _pgid) in self.recorded(app_pid).items():
            entry = table.get(pid)
            if entry is None or "Z" in entry[2]:
                continue
            if entry[3] != cmd:
                continue  # the pid came back around to somebody else's process
            if pattern is None or _matches(pattern, cmd):
                out[pid] = cmd
        return out


LEDGER = RunLedger()


class Reap(NamedTuple):
    """What one pass of `reap_stale_leftovers` found, killed and left behind.

    Split into "done" and "still there" on purpose: the report a dirty machine gets
    has to be trustworthy in both directions, and a single count of what was
    attempted would let a half-completed cleanup read as a clean machine.
    """

    procs: list      # pids SIGKILLed
    files: list      # generated rc files unlinked
    stubborn: dict   # pids still running after the kill
    untouched_files: list  # stale files that are still on disk


def reap_stale_leftovers():
    """Clear the debris abandoned runs of looprs left behind, and count it.

    Two kinds, both identified by the owner pid in the rc file's name:

      * a shell still carrying a `looprs-bash-integration-<owner>-<n>.sh` marker
        whose owner pid is gone — the app that made it, and that was going to kill
        it, is not coming back, so this is the orphan looprs-ecr exists to prevent,
        arrived; and its whole subtree, because the `sleep 45` inside that shell is
        debris too and killing only the shell would leave it running under launchd;
      * the generated rc file itself, left by a run killed hard enough to skip the
        `Drop` that removes it (`Shell::drop`, `src/session/bash.rs`) — 19 such
        files were sitting in `$TMPDIR` when this was written, 263 on the machine
        the finding was made on.

    **Liveness of the owner pid is the only test.** A live looprs is never touched:
    the user's own session, a spike running in the next window over, anything whose
    owner answers `kill(pid, 0)`, is skipped — which is the discrimination the
    global `pgrep -f` did not have, and the reason it had to treat every match on
    the box as this run's fault. The same test covers this spike's own debris: when
    the leak self-test SIGKILLs an app, that app's rc file stops having an owner
    and becomes reapable like anybody's, which is why main() calls this at the end
    of the run as well as at the start. A guard of the form "skip anything owned by
    a pid I know about" would look safer and be strictly worse — it would protect
    nothing that liveness does not already protect, and it would forbid exactly the
    cleanup that keeps the spike from dirtying the machine it is testing.

    Returns `Reap(procs, files, stubborn_processes, untouched_files)`:

      * `procs` — the pids killed, `files` — the rc files unlinked;
      * `stubborn_processes` — pids still running after being SIGKILLed;
      * `untouched_files` — files that were stale and still exist after the pass.

    The last two are what the caller asserts on: a reap that reported 4 and left 2
    is worse than no reap at all, because it reads as done.
    """
    table = proc_table()

    doomed = {}
    for pid, (_ppid, _pgid, stat, cmd) in table.items():
        if "Z" in stat:
            continue
        m = INTEGRATION_RE.search(cmd)
        if not m:
            continue
        owner = int(m.group(1))
        if running(table, owner):
            continue  # somebody's live session; not ours to touch
        doomed[pid] = cmd
        for cpid, (_cppid, _cpgid, cstat, ccmd) in descendants(pid, table).items():
            if "Z" not in cstat:
                doomed[cpid] = ccmd

    killed = []
    # Highest pid first: the children die before the parents, so no parent gets a
    # chance to notice its child disappearing and try to do something about it.
    for pid in sorted(doomed, reverse=True):
        try:
            os.kill(pid, signal.SIGKILL)
            killed.append(pid)
        except (ProcessLookupError, PermissionError):
            continue

    removed = []
    untouched = []
    tmp = tempfile.gettempdir()
    try:
        names = sorted(os.listdir(tmp))
    except OSError:
        names = []
    for name in names:
        m = INTEGRATION_RE.fullmatch(name)
        if not m or pid_running(int(m.group(1))):
            continue
        path = os.path.join(tmp, name)
        try:
            os.unlink(path)
        except OSError:
            untouched.append(path)
            continue
        removed.append(path)

    # Did the kills and the unlinks actually take? A count of what was *attempted*
    # is not a count of what is gone, and the whole point of reporting the stale
    # number is that it can be trusted. SIGKILL is not synchronous, so this gives
    # the table two seconds to agree before calling anything stubborn.
    deadline = time.time() + 2.0
    stubborn = {p: 0 for p in killed}
    while time.time() < deadline:
        table = proc_table()
        stubborn = {p: table[p][3] for p in killed if running(table, p)}
        if not stubborn and not any(os.path.exists(p) for p in untouched):
            break
        time.sleep(0.1)
    untouched = [p for p in untouched if os.path.exists(p)]
    return Reap(killed, removed, stubborn, untouched)


def last_erase(text):
    """`(end_offset, row)` of the final "move then clear from cursor down`.

    That pair is what the exit writes — park on the live region's own top row,
    then erase downward — and it is the last one in the stream because the
    teardown is the last thing the app does. The row it landed on is the
    boundary between "rows the app owns and just gave back" and "rows above it,
    which are the user's scrollback and must still be there".
    """
    span = last_erase_span(text)
    return (span[1], span[2]) if span else None


def last_erase_span(text):
    """`(start, end, row)` of the final erase, for rules read as an ordering.

    "The leave came before the erase" is a statement about two offsets, and the
    erase's *start* is the one that answers it: the leave has to be off the wire
    before the erase is addressed to the screen it just got back.
    """
    last = None
    for m in re.finditer(r"\x1b\[(\d+);1H\x1b\[(?:2)?J", text):
        last = (m.start(), m.end(), int(m.group(1)) - 1)
    return last


# Every mode the app's ledger can hold, keyed by the DEC private mode number the
# terminal knows it by, with the value that mode takes when the terminal is told to
# *set* it (`CSI ? <n> h`).
#
# The polarity column is not decoration. `?25` is "cursor visible", so `h` means
# the cursor is shown and `l` means it is hidden: tracking the mode as the user
# experiences it (`cursor_hidden`) rather than as the register spells it is what
# makes `cursor_visible` the same fact the ledger holds, and not its inverse.
# 47 and 1047 are aliases of `alt_screen` because `src/screen.rs` treats the
# three spellings as one fact, and a spike that disagreed with the app about what a
# screen is would be checking nothing.
MODE_BY_NUMBER = {
    25: ("cursor_hidden", False),  # `?25h` sets visibility, which is *not* hidden
    47: ("alt_screen", True),
    1000: ("mouse_report", True),
    1002: ("mouse_drag", True),
    1006: ("mouse_sgr", True),
    1047: ("alt_screen", True),
    1049: ("alt_screen", True),
    2004: ("bracketed_paste", True),
}
WATCHED = ["alt_screen", "cursor_hidden", "mouse_report", "mouse_drag",
           "mouse_sgr", "bracketed_paste"]

MODE_SEQ = re.compile(r"\x1b\[\?([0-9;]*)h|\x1b\[\?([0-9;]*)l")


def _numbers(params):
    out = []
    for part in (params or "").split(";"):
        part = part.strip()
        if part.isdigit():
            out.append(int(part))
    return out


class ModeTrace:
    """The terminal's mode state, read off the bytes the app actually wrote.

    This is the spike keeping its own ledger. It is not the app's bookkeeping read
    back — it is a fold over the wire, the way a terminal would keep it, and it is
    the only honest way to answer "is the user in the alternate screen now?"
    without asking the user's terminal a question the exit path is forbidden to
    ask. (There is no terminal emulator behind a pty: a real `DECRQM` here would
    simply go unanswered.)

    It also counts the leaves, because "exactly once" is a claim about a count and
    not about a final state — a mode turned off twice and a mode turned off once
    end in the same place, and only one of those is what was promised.
    """

    def __init__(self, text):
        self.state = {m: False for m in WATCHED}
        self.on_counts = Counter()
        self.off_counts = Counter()
        self.last_leave_of = {}  # name -> (offset just after its leave bytes)
        pos = 0
        while True:
            m = MODE_SEQ.search(text, pos)
            if m is None:
                break
            params = m.group(1) if m.group(1) is not None else m.group(2)
            setting = m.group(1) is not None  # `h` sets, `l` resets
            for num in _numbers(params):
                mapped = MODE_BY_NUMBER.get(num)
                if mapped is None:
                    continue
                name, value_when_set = mapped
                value = value_when_set if setting else not value_when_set
                self.state[name] = value
                if value:
                    self.on_counts[name] += 1
                else:
                    self.off_counts[name] += 1
                    self.last_leave_of[name] = m.end()
            pos = m.end()

    @property
    def cursor_visible(self):
        return not self.state["cursor_hidden"]

    def still_on(self):
        return sorted(m for m, v in self.state.items() if v)

    def summary(self):
        return ", ".join(f"{m}={int(self.state[m])}" for m in WATCHED)


def check_quiet_after_the_handback(trace, text, where):
    """Nothing the terminal has to answer appears after the ledger's last leave.

    Read from the last leave rather than from the key or the signal, because the
    mark on the capture is not the mark on the app: a viewport reshape's `ESC[6n`
    can be sitting in the pump's buffer when the user quits, and that is a query
    the exit path never made. What is unambiguous is the far end of the hand-back —
    after the last leave sequence, the app is done with the terminal, and anything
    that still asks a question there is the exit path asking it.
    """
    offs = list(trace.last_leave_of.values())
    if not offs:
        check(f"{where}: a hand-back to read the tail from", False,
              "no leave sequence anywhere in the capture")
        return
    tail = text[max(offs):]
    check(
        f"{where}: no cursor query after the hand-back",
        "\x1b[6n" not in tail and "^[6n" not in tail,
        f"asked after leaving: {tail[:120]!r}",
    )


def check_ledger_handed_back(trace, entered, where):
    """Every mode we saw switched on is off now, and was left exactly once.

    `entered` is the set the run switched on, taken from `on_counts` while the app
    was up — what it *did*, not what it meant to.

    The cursor is the one mode whose off-sequence is not exclusively the ledger's:
    ratatui writes `?25h` itself when it inserts lines above the pane, so it
    appears many times in a run and none of that is a leaked cursor. What is
    assertable for the cursor is the final state — visible — which is also the only
    question the user ever asks.
    """
    for mode in WATCHED:
        on = trace.on_counts[mode]
        off = trace.off_counts[mode]
        if mode in entered:
            check(f"{where}: {mode} was switched on", on > 0, f"{on} on-sequences")
            if mode != "cursor_hidden":
                check(
                    f"{where}: {mode} left exactly once",
                    off == 1,
                    f"{off} leave sequences for a mode we took",
                )
            check(
                f"{where}: {mode} is off at the end",
                not trace.state[mode],
                f"still on -- {trace.summary()}",
            )
        else:
            check(
                f"{where}: no {mode} leave for a mode nothing switched on",
                off == 0,
                f"emitted {off} leaves for a screen we never took",
            )
    check(
        f"{where}: the cursor is visible at the end",
        trace.cursor_visible,
        f"cursor still hidden -- {trace.summary()}",
    )


def scenario_alt_screen():
    say("=== the whole mode set: alt screen, mouse, bracketed paste, hidden cursor ===")
    d = Driver(LOOPRS_MODES="all")
    time.sleep(2.0)
    check("the app boots with every mode on", d.alive())
    mid = ModeTrace(d.text())
    entered = sorted(mid.on_counts)
    check("the app is in the alternate screen", mid.state["alt_screen"], mid.summary())
    check(
        "all three mouse modes are switched on",
        all(mid.state[m] for m in ("mouse_report", "mouse_drag", "mouse_sgr")),
        mid.summary(),
    )
    check("bracketed paste is on", mid.state["bracketed_paste"], mid.summary())
    check(
        "the app hides the cursor for its own frames",
        mid.on_counts["cursor_hidden"] > 0,
        f"never emitted ?25l -- {mid.summary()}",
    )

    code, mark, dt = d.quit()
    full = ModeTrace(d.text())
    check("the app exits on Ctrl-Q with the whole mode set", code is not None)
    check("exit code is 0", code == 0, f"exit {code}")
    check_ledger_handed_back(full, entered, "alt-screen quit")
    check(
        "mouse modes are silent: nothing left to report a drag into the shell",
        not any(full.state[m] for m in ("mouse_report", "mouse_drag", "mouse_sgr")),
        full.summary(),
    )
    left = full.last_leave_of.get("alt_screen")
    check("the app left the alternate screen", left is not None)
    if left is not None:
        tail = strip_cpr_replies(d.text()[left:])
        check(
            "the alternate-screen leave is the last thing the app wrote",
            tail == "",
            f"bytes after the leave: {tail!r}",
        )
    check_quiet_after_the_handback(full, d.text(), "alt-screen quit")
    check("the tty is cooked again after exit", d.tty_is_cooked())
    d.check_no_survivor("nothing this run spawned survived the whole-mode-set quit")
    say(f"exited {dt:.2f}s after the quit key, code {code}")
    d.close()


def scenario_child_holds_screen():
    """The exit path inherits a screen it did not switch on.

    A full-screen child paints through the passthrough, so the `?1049h` that took
    the user's screen was written by the app's own stdout — by the child's hand.
    When that child is killed instead of quitting, nothing on the session side can
    put the screen back: the shell is dead, its pump is cut, and the task that
    watched the switch is gone. The only thing left holding a write handle to the
    terminal is the app, which is why `ScreenDebt` exists and why the leave is
    paid from `restore` rather than from the session.

    The child here is a `sh` that enters the alternate screen and sits in it, because
    that is `vim` SIGKILLed reduced to the byte that matters — and a spike that
    depends on `vim` being installed is a spike that silently stops running.
    """
    say("=== a full-screen child killed while it holds the screen ===")
    d = Driver()
    time.sleep(1.5)
    d.send(b"\t")  # Beads -> Pi
    time.sleep(0.3)
    d.send(b"\t")  # Pi -> Bash
    time.sleep(0.6)
    d.send(
        b"sh -c 'printf \"\\033[?1049h\\033[2Jthe child owns this screen"
        b"\\033[10;1Hstill here\"; sleep 120'\r"
    )
    time.sleep(1.8)
    mid = ModeTrace(d.text())
    check(
        "the run is on the alternate screen while the child holds it — the frame's "
        "screen, not the child's",
        mid.state["alt_screen"],
        mid.summary(),
    )

    code, mark, dt = d.quit()
    full = ModeTrace(d.text())
    check(
        "the app exits on Ctrl-Q while the child still holds the screen",
        code is not None,
        "it never left",
    )
    check("exit code is 0", code == 0, f"exit {code}")
    check(
        "not in the alternate screen after the exit",
        not full.state["alt_screen"],
        f"the user is still inside the dead program -- {full.summary()}",
    )
    check(
        "the alternate screen was left exactly once",
        full.off_counts["alt_screen"] == 1,
        f"{full.off_counts['alt_screen']} leaves for one screen",
    )
    leave = full.last_leave_of.get("alt_screen")
    # Replaces "the leave and the pane erase are both there to order", which was
    # a check about the inline pane (looprs-pdl.4).
    #
    # The frame hosts the alternate screen now, so a child that asks to switch
    # into it is cut upstream and handed a blank canvas instead (ADR-0004 R22):
    # the child's `?1049h` never reaches the wire, no debt is booked for a screen
    # it never switched, and the app's own single leave at exit is the only
    # `?1049l` in the run. A second enter would be the destructive one — it
    # re-saves whatever is on screen as the user's *main* screen, which here
    # would be the app's own frame.
    whole = d.text()
    enters = whole.count("\x1b[?1049h")
    check(
        "the child's `?1049h` never reached the wire: one enter for the run, the app's own",
        enters == 1,
        f"{enters} enters in the capture",
    )
    check(
        "no pane erase is written on the way out: the inline pane is gone, and so is its erase",
        last_erase_span(whole) is None,
        f"erase span: {last_erase_span(whole)}",
    )
    check(
        "no cursor query anywhere: nothing on this path has to ask where the cursor is",
        "\x1b[6n" not in whole,
    )
    if leave is not None:
        # `last_leave_of` is the offset just *after* the leave bytes, so the tail
        # here is what came after the hand-back: nothing. The old check required
        # "erase, the ledger's own bytes, one closing newline"; the absence of
        # all three is the point now.
        tail = strip_cpr_replies(whole[leave:])
        check(
            "the run ends on the app's own leave: nothing after the alternate screen",
            tail == "",
            f"tail after the leave: {tail!r}",
        )
    # The app's own ledger, plus the screen it inherited and paid for.
    #
    # Since looprs-pdl.8 this set includes the three mouse modes: the app
    # switches `?1000/?1002/?1006` on by default through the ledger, so they are
    # on this list here too, and "left exactly once" is asserted of them on the
    # child-held-screen path as much as anywhere else -- which is the path where a
    # mode left on behind a child is most expensive.
    #
    # Bracketed paste stays out: nothing switched it on in this scenario, and the
    # child here does not switch it on either -- a real `vim` does, and that
    # leftover is a separate gap, recorded in
    # docs/adr/0006-terminal-mode-ledger.md.
    check_ledger_handed_back(
        full,
        {"cursor_hidden", "alt_screen",
         "mouse_report", "mouse_drag", "mouse_sgr"},
        "child-held screen",
    )
    check("the tty is cooked again after exit", d.tty_is_cooked())
    leftovers = [
        m for m in ("mouse_report", "mouse_drag", "mouse_sgr", "bracketed_paste")
        if full.state[m]
    ]
    say(f"  tee'd modes still on at exit: {leftovers or 'none'}")
    # The child-held screen is the path where a surviving child is most expensive,
    # so it gets the leak check too: the shell that painted through the
    # passthrough and never gave the screen back is the one the exit path has to
    # kill anyway, and "killed" is a claim about a pid we recorded.
    d.check_no_survivor("no child of this run survived the child-held-screen exit")
    say(f"exited {dt:.2f}s after the quit key, code {code}")
    d.close()


def scenario_signal(sig, modes, label):
    """A signal from outside gets the same hand-back the quit key gets.

    Nothing in the app knows which one it was answering, and that is the point:
    one exit path, one ledger, one set of promises about the terminal.
    """
    say(f"=== {label}: {sig} with the ledger holding {modes or 'the default modes'} ===")
    env = {"LOOPRS_MODES": modes} if modes else {}
    d = Driver(**env)
    time.sleep(2.2)
    check(f"the app is up before {sig}", d.alive())
    mid = ModeTrace(d.text())
    entered = sorted(mid.on_counts)
    check(f"the modes asked for are actually on ({modes})", bool(entered), mid.summary())

    mark = d.mark()
    t0 = time.time()
    d.record_children()
    d.proc.send_signal(getattr(signal, sig))
    try:
        code = d.proc.wait(timeout=EXIT_BUDGET)
    except subprocess.TimeoutExpired:
        d.proc.kill()
        code = None
    time.sleep(0.3)
    dt = time.time() - t0
    full = ModeTrace(d.text())
    check(f"the app leaves on {sig} by itself", code is not None, "it never exited")
    check(f"exit code is 0 after {sig}", code == 0, f"exit {code}")
    check(f"the {sig} exit stayed inside the budget", dt <= EXIT_BUDGET, f"{dt:.2f}s")
    check_ledger_handed_back(full, entered, f"{sig} exit")
    check_quiet_after_the_handback(full, d.text(), f"{sig} exit")
    check(f"the tty is cooked again after {sig}", d.tty_is_cooked())
    d.check_no_survivor(f"nothing this run spawned survived the {sig} exit")
    say(f"exited {dt:.2f}s after {sig}, code {code}")
    d.close()


def scenario_panic_in_draw():
    say("=== a panic inside the draw: the ledger still leaves everything once ===")
    d = Driver(LOOPRS_MODES="all", LOOPRS_PANIC="draw")
    try:
        code = d.proc.wait(timeout=EXIT_BUDGET)
    except subprocess.TimeoutExpired:
        d.proc.kill()
        code = None
    time.sleep(0.3)
    check("the app is gone after panicking in the frame", code is not None)
    check("the exit code says something failed", code not in (0, None), f"exit {code}")
    check(
        "it panicked where the fault injector put it",
        "LOOPRS_PANIC=draw" in d.text(),
        "no panic message on the wire",
    )
    full = ModeTrace(d.text())
    entered = sorted(full.on_counts)
    check("the frame did get as far as switching the modes on", len(entered) >= 5,
          f"only switched on: {entered}")
    check_ledger_handed_back(full, entered, "panic in draw")
    check_quiet_after_the_handback(full, d.text(), "panic in draw")
    check("the tty is cooked again after the panic", d.tty_is_cooked())
    d.check_no_survivor("nothing this run spawned survived the panic")
    say(f"exited code {code} with the whole mode set held at the moment of the panic")
    d.close()


class Driver:
    """The app under a pty, with a terminal that answers cursor queries.

    The pty is emulated only in the one way the app insists on: it answers
    `ESC[6n`. Everything else the app writes goes into a capture buffer, which is
    the evidence.
    """

    def __init__(self, rows=ROWS, cols=COLS, label="app", **env_over):
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
        # The run-scoped process record. Started here, before anything has been
        # typed, because the ledger has to have seen a child while the app that
        # made it was still alive: once the app is gone the child is launchd's and
        # no later scan can give it back to us.
        self.label = label
        self.app_pid = self.proc.pid
        LEDGER.note_app(self.app_pid, label)
        self._watch_stop = threading.Event()
        threading.Thread(target=self._watch_children, daemon=True).start()

    def _watch_children(self):
        """Keep the ledger current for as long as the app lives.

        A child appears whenever the user's keystrokes say so — a tab change starts
        a Bash session, a command in that shell forks a `sleep` — so "the pids we
        spawned" is only knowable by watching, not by a snapshot taken at some
        point that happened to be early enough. Every 0.2s costs one `ps`, which is
        nothing next to what the scenarios are measuring.
        """
        while not self._watch_stop.wait(0.2):
            if self.proc.poll() is not None:
                break
            LEDGER.record_under(self.app_pid, self.label)

    def record_children(self):
        """Synchronous ledger update, for the moment just before an exit trigger.

        The watcher covers the run; this covers the last 200ms of it, which is
        exactly the window a quit key lands in.
        """
        LEDGER.record_under(self.app_pid, self.label)

    def survivors(self, pattern=None):
        """This run's recorded children that are running now, as `{pid: command}`.

        Scoped to `self.app_pid`: a shell from this run's generated rc file counts,
        and a 40-minute-old one from an abandoned run of an older binary does not.
        """
        return LEDGER.survivors(self.app_pid, pattern)

    def check_no_survivor(self, name, pattern=None):
        """A leak check that can only see this run's own children.

        Returns the leak so a caller can report the same set twice without
        rescanning the box.
        """
        leak = self.survivors(pattern)
        check(name, not leak, f"{len(leak)} still running: {fmt_procs(leak)}")
        return leak

    def reap_recorded(self):
        """SIGKILL everything this run recorded that is still alive, and wait.

        Only the leak self-test needs it: that scenario leaks a shell on purpose and
        has to leave no more debris behind than it found. The general cleanup is
        `reap_stale_leftovers`, which runs at startup and works on other runs'
        leftovers, not on a leak this run is still holding open.
        """
        leak = self.survivors()
        for pid in sorted(leak, reverse=True):
            try:
                os.kill(pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        deadline = time.time() + 3.0
        while time.time() < deadline and self.survivors():
            time.sleep(0.1)
        return leak

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
        # Record first, press second: whatever children are alive at the moment the
        # quit key goes out has to be in the ledger before the exit that orphans
        # them, or the leak check at the other end has nothing to look at.
        self.record_children()
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
        self._watch_stop.set()
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
    # Every mode the app switched on during this run, read off the wire rather
    # than asserted from what it meant to do. `on_counts` and not the state at
    # the instant of the snapshot: a cursor the app hid and later showed is still
    # a cursor the app hid, and the promise is about the hiding.
    entered = sorted(ModeTrace(d.text()).on_counts)

    code, mark, dt = d.quit()
    check("the app exits on Ctrl-Q", code is not None, "it never left")
    check("exit code is 0", code == 0, f"exit {code}")
    check(
        "no 'cursor position could not be read' anywhere in the run",
        "could not be read" not in d.text(),
        [l for l in d.text().splitlines() if "could not be read" in l][:1],
    )
    # What the hand-back looks like now that the frame owns the alternate screen
    # (looprs-pdl.4 rewrote this block). The inline pane's shape — park on the
    # pane's own top row, erase downward, one closing newline — is gone, and with
    # it the offset every rule here used to be measured against. What replaces
    # them is the alternate-screen shape described in the module docstring.
    whole = d.text()
    enter = whole.find("\x1b[?1049h")
    leave = whole.rfind("\x1b[?1049l")
    check("the run opened by taking the alternate screen and painted nothing before it",
          enter >= 0
          and re.sub(r"\x1b\[\?[0-9;]+[hl]", "", whole[:enter]).strip() == "",
          f"bytes before the enter: {whole[:enter][:120]!r}")
    check(
        "no pane erase is written anywhere in the run: the erase went out with the pane",
        last_erase_span(whole) is None,
        f"erase span: {last_erase_span(whole)}",
    )
    check(
        "no cursor query in the whole run — a full-screen frame never has to ask "
        "the terminal where the cursor is",
        "\x1b[6n" not in whole,
        f"{whole.count(chr(27) + '[6n')} queries on the wire",
    )
    check("the alternate screen is left, and nothing at all is written after it",
          leave >= 0
          and strip_cpr_replies(whole[leave + len("\x1b[?1049l"):]) == "",
          repr(strip_cpr_replies(whole[leave + len("\x1b[?1049l"):]))[:200])
    # "Nothing visible was lost", measured the only way it can still be
    # measured: the rows the answer was painted on, in the frames the app drew
    # before it left the screen. It can no longer be measured as "above the erase
    # line", because the answer does not land in the user's scrollback at all —
    # the app's transcript is the band, and the band is on the alternate screen.
    # That is the trade full-screen makes deliberately (ADR-0004 R1 as amended),
    # and looprs-pdl.5/6/7 are the tickets that give the transcript a scrollback
    # of its own.
    painted = rows_painted(whole[:leave] if leave > 0 else whole)
    answer_rows = sorted(
        r for r, txt in painted.items() if "bravo" in txt or MARK in txt
    )
    check(
        "nothing visible was lost: the answer was painted on the screen the frame left",
        len(answer_rows) >= 2,
        f"{len(answer_rows)} answer rows before the leave",
    )
    check_ledger_handed_back(ModeTrace(d.text()), entered, "inline quit")
    check("the tty is cooked again after exit", d.tty_is_cooked())
    # The leak checks, scoped to the pids this run spawned and recorded while the
    # app was up. The named ones stay named — "the shell from the generated rcfile"
    # and "the fake pi" are each a different way a child can outlive its app, and
    # each is worth its own line in the log — and the catch-all after them is the
    # one that catches the child nobody thought to name, which is how a leak is
    # *not* supposed to be discovered.
    d.check_no_survivor("no bash from the generated rcfile survived", INTEGRATION_RE)
    d.check_no_survivor("no fake pi survived the app", pi)
    d.check_no_survivor("nothing else this run spawned outlived the app")
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
    # Record the busy shell and the `sleep` it is running *before* the exit that
    # orphans them. This is the pair the old global check could see and could not
    # attribute: it proved nothing about this run, and this one proves everything
    # about it — including that if the app left the shell behind, the detail names
    # the shell's pid rather than the machine's uptime.
    d.record_children()
    check("this run's shell and its busy child are both recorded",
          bool(d.survivors(INTEGRATION_RE)) and bool(d.survivors("sleep 30")),
          f"shell: {fmt_procs(d.survivors(INTEGRATION_RE))} / "
          f"sleep: {fmt_procs(d.survivors('sleep 30'))}")

    code, mark, dt = d.quit()
    check("the app exits on Ctrl-Q with a busy shell", code is not None)
    check("exit code is 0", code == 0, f"exit {code}")
    check("the exit stayed inside the budget", dt <= EXIT_BUDGET, f"{dt:.2f}s > {EXIT_BUDGET}s")
    d.check_no_survivor("no shell from our rcfile outlived the app", INTEGRATION_RE)
    d.check_no_survivor("no stray `sleep 30` from that shell", "sleep 30")
    d.check_no_survivor("nothing else this run spawned outlived the app")
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
    d.record_children()
    children = d.survivors(pi)
    check("the wedged child is up", len(children) >= 1, str(children))

    code, mark, dt = d.quit()
    check("the app exits even with a child that ignores everything", code is not None)
    check("exit code is 0", code == 0, f"exit {code}")
    check(
        f"the exit stayed inside the {EXIT_BUDGET}s budget",
        dt <= EXIT_BUDGET,
        f"{dt:.2f}s",
    )
    # `procs_matching(pi)` here was a `pgrep -f` over the whole box, which meant
    # the check that a SIGTERM-ignoring child got killed could be satisfied — or
    # failed — by something that was never this run's. Same claim, same fake, now
    # read off the pids recorded when the app was still alive to be their parent.
    d.check_no_survivor("the wedged child was killed, not left behind", pi)
    d.check_no_survivor("nothing else this run spawned outlived the app")
    check("the tty is cooked again after exit", d.tty_is_cooked())
    say(f"exited {dt:.2f}s after the quit key against a child that ignored SIGTERM")
    d.close()
    shutil.rmtree(tmp, ignore_errors=True)


def scenario_leak_is_named():
    """Non-vacuity: the scoped leak check can fail, and fails on the right pid.

    Scoping a leak check to "the pids this run recorded" buys attribution and
    costs a new way to be silently wrong. An empty recorded set looks exactly like
    a clean exit whether the app cleaned up perfectly or the ledger never saw the
    child at all — a check that cannot fail is the same defect facing the other
    way, and a check that only ever runs against binaries that do not leak never
    finds out which of the two it is looking at.

    So this scenario leaks one on purpose. `SIGKILL` is the exit no cleanup can
    cover: no `Drop`, no `kill_and_reap`, no rc-file removal, no exit path at all.
    The shell is told to ignore `SIGHUP` first so that a hang-up on its pty cannot
    quietly tidy up the leak this scenario is here to demonstrate — the survivor
    has to be there because the app died without reaping it, and nothing else.
    Then it cleans up after itself: the point is a machine that stays clean, not a
    demo that leaves its prop behind.
    """
    say("=== the leak check against a real leak: SIGKILL the app over a live shell ===")
    d = Driver(label="leak self-test")
    time.sleep(1.5)
    d.send(b"\t")  # Beads -> Pi
    time.sleep(0.3)
    d.send(b"\t")  # Pi -> Bash
    time.sleep(0.5)
    d.send(b"trap '' HUP; sleep 45\r")
    time.sleep(1.5)
    d.record_children()
    planted_shell = d.survivors(INTEGRATION_RE)
    planted_child = d.survivors("sleep 45")
    check("the ledger recorded the live shell this run spawned",
          bool(planted_shell), fmt_procs(planted_shell))
    check("the ledger recorded the busy child inside that shell too",
          bool(planted_child), fmt_procs(planted_child))

    say(f"  SIGKILLing pid {d.app_pid}: no `Drop`, no `kill_and_reap`, nothing "
        f"on the cleanup path runs at all")
    d.proc.kill()
    try:
        d.proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        pass
    time.sleep(0.5)

    leak = d.survivors()
    check("the scoped leak check fires on a child the run really left behind",
          bool(leak), "nothing was recorded as surviving — the check is vacuous")
    check("it names the shell from this run's generated rc file, by the pid it recorded",
          bool(d.survivors(INTEGRATION_RE)), fmt_procs(leak))
    check("it names the shell's own busy child along with it",
          bool(d.survivors("sleep 45")), fmt_procs(leak))
    say(f"  leaked and named: {fmt_procs(leak)}")

    reaped = d.reap_recorded()
    check("the self-test reaped what it leaked", not d.survivors(),
          f"{len(reaped)} killed, {fmt_procs(d.survivors())} still up")
    say(f"reaped {len(reaped)} process(es) the self-test had leaked on purpose")
    d.close()


def report_reap(where, reap):
    """Print one reap pass so a dirty machine is *visible* rather than fatal.

    This is the line the whole of looprs-00u.24 was asked for: "N stale leftovers
    from earlier runs found and reaped". It goes in the log because a machine that
    was dirty at the start of a run is a fact a later reader needs — it is the
    difference between "the leak checks pass" and "the leak checks pass here, on
    this machine, which had 19 abandoned rc files under it when we started".
    """
    say(f"{where}: {len(reap.procs)} orphaned process(es) killed, "
        f"{len(reap.files)} generated rc file(s) removed")
    for path in reap.files[:8]:
        say(f"  removed {path}")
    if len(reap.files) > 8:
        say(f"  ... and {len(reap.files) - 8} more")
    check(f"{where}: everything the reaper took is really gone",
          not reap.stubborn and not reap.untouched_files,
          f"still running: {fmt_procs(reap.stubborn)}; still on disk: "
          f"{', '.join(reap.untouched_files) or 'none'}")


def main():
    for path, why in ((BIN, "run `cargo build` first"), (SLOW_PI, "missing fixture")):
        if not os.path.exists(path):
            say(f"{why}: {path}")
            return 2

    # A filter on the command line runs one group by name, because these scenarios
    # are minutes-long and the one you just broke is one of them.
    wanted = {a.lower() for a in sys.argv[1:]}

    def run(name, fn):
        if not wanted or name in wanted:
            fn()

    # Debris first, checks second. Everything below is scoped to the pids this run
    # spawns, so this step decides nothing about whether the run passes — what it
    # decides is what the log says about the machine the run happened to sit on.
    # A box dirty with abandoned runs of an older binary used to turn that dirt
    # into two failed checks against a clean build (looprs-00u.24); now it is a
    # counted line at the top of the log, and the build still gets judged on its
    # own processes and nothing else. `LOOPRS_NO_REAP=1` leaves the debris alone
    # for a control run that wants to see it arrive.
    if os.environ.get("LOOPRS_NO_REAP") == "1":
        say("stale-leftover reaper disabled by LOOPRS_NO_REAP=1: earlier runs' "
            "debris is left where it is (the checks below are scoped to this run "
            "either way)")
        reap_at_exit = False
    else:
        report_reap("stale leftovers from earlier runs", reap_stale_leftovers())
        reap_at_exit = True

    run("midstream", scenario_mid_stream)
    run("bash", scenario_bash_child)
    run("wedged", scenario_wedged_child)
    run("leak", scenario_leak_is_named)
    run("alt", scenario_alt_screen)
    run("child", scenario_child_holds_screen)
    if not wanted or "sigterm" in wanted:
        run("sigterm", lambda: scenario_signal("SIGTERM", "all", "the whole mode set"))
    if not wanted or "sighup" in wanted:
        run("sighup", lambda: scenario_signal("SIGHUP", None, "the plain inline run"))
    run("panic", scenario_panic_in_draw)
    # Last, after every scenario has had its own leak checks: the aggregate over
    # everything this run recorded, including anything a scenario leaked without
    # noticing. The named checks say which promise broke; this one says whether the
    # run left the machine as it found it.
    leftover = LEDGER.survivors()
    say(f"this run recorded {len(LEDGER.recorded())} child process(es); "
        f"{len(leftover)} still running at the end")
    check("nothing this run spawned is still running when the spike finishes",
          not leftover, fmt_procs(leftover))
    # Then the same pass over the disk half. This spike is not exempt from the rule
    # it is testing: the leak self-test SIGKILLs an app on purpose, which skips
    # `Shell::drop` and leaves that run's generated rc file behind exactly like an
    # abandoned run would. Reaping at exit keeps the spike from becoming the debris
    # the next run has to explain.
    if reap_at_exit:
        report_reap("this run's own leftovers", reap_stale_leftovers())

    failed = [n for n, ok in RESULTS if not ok]
    say(f"{len(RESULTS) - len(failed)}/{len(RESULTS)} checks passed")
    if failed:
        say("failed: " + "; ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
