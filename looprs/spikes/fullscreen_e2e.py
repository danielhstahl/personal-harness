#!/usr/bin/env python3
"""Full-screen programs through Bash mode — the acceptance measurement (looprs-4hv).

ADR-0001's "Known gap" section recorded that a full-screen program's screen never
reached the user: vim started, painted ~1 KB of cursor-addressed bytes with no
linefeeds in them, and the line-delimited transcript flush showed none of it. This
is the measurement that says whether the screen-buffer path fixed that.

Two things are run, and the second is what makes the first honest:

  1. **the baseline** — `vim` (and `less`) driven on a bare pty with no looprs in
     the way, recording which on-screen markers the program actually draws on this
     machine;
  2. **looprs** — the same keystrokes through the real TUI, scored against that
     baseline.

The baseline is not ceremony. Grepping for `--INSERT--` is not a property of vim:
with `TERM=xterm-256color` vim uses the terminal's own insert-mode signalling and
does not write that string at all (measured here: pressing `i` on a bare pty
emits 14 bytes of bracketed-paste toggling and no `--INSERT--`). A check written
against a hardcoded guess fails when the feature works, and could pass when nothing
is displayed. So every marker is scored against what the program really drew, and
a marker the baseline never produced is reported as `n/a` rather than quietly
dropped from the list.

    cargo build
    python3 spikes/fullscreen_e2e.py | tee spikes/results/fullscreen-e2e.log

Exit 0 means every check that *could* be evaluated held.
"""

import os
import pty
import re
import subprocess
import sys
import threading
import time
import fcntl
import termios
import struct
import signal

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bash_e2e as e  # the PTY driver, checks, and normalizing helpers
from shutdown_e2e import ModeTrace  # the wire's mode state, folded the way a terminal keeps it

LONG = "/tmp/looprs-long-for-less.txt"
PROBE = "/tmp/looprs-vim-e2e.txt"
ROWS, COLS = 40, 132


def say(msg):
    e.say(msg)


def check(name, ok, detail=""):
    e.RESULTS.append((name, ok))
    mark = "PASS" if ok else "FAIL"
    say(f"{mark}  {name}" + (f"  -- {detail}" if detail and not ok else ""))


def na(name, why):
    """A check this machine cannot evaluate, said out loud instead of removed."""
    say(f"n/a   {name}  -- {why}")


class Bare:
    """One program on a pty, nothing else. The control for "what does it draw"."""

    def __init__(self, argv):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        self.pid = os.fork()
        if self.pid == 0:
            try:
                os.setsid()
                os.dup2(slave, 0)
                os.dup2(slave, 1)
                os.dup2(slave, 2)
                os.execvp(argv[0], argv)
            finally:
                os._exit(127)
        os.close(slave)
        self.lock = threading.Lock()
        self.raw = bytearray()
        threading.Thread(target=self._pump, daemon=True).start()

    def _pump(self):
        try:
            while True:
                d = os.read(self.master, 65536)
                if not d:
                    break
                with self.lock:
                    self.raw.extend(d)
        except OSError:
            pass

    def send(self, b):
        os.write(self.master, b)

    def text(self):
        with self.lock:
            return bytes(self.raw).decode("utf-8", errors="replace")

    def since(self, mark):
        t = self.text()
        i = t.rfind(mark)
        return t[i + len(mark):] if i >= 0 else t

    def quit(self):
        """Ask the program to leave, then stop it if it will not.

        Note what this is **not**: `os.close(self.master)`. Closing the master
        while the pump thread is blocked in `read()` on it does not return on this
        machine — measured here, the call hangs the process (exit 124 under
        `timeout`, with the close the last line reached). A leaked pty in a
        short-lived spike is the cheaper price, and the child is what the check is
        about anyway.
        """
        try:
            self.send(b"q")
        except OSError:
            pass
        time.sleep(0.6)
        try:
            if os.waitpid(self.pid, os.WNOHANG)[0] == 0:
                os.kill(self.pid, signal.SIGTERM)
                time.sleep(0.3)
        except ChildProcessError:
            pass


def wait_until(fn, timeout=5.0):
    """Poll instead of sleeping once.

    A full-screen program repaints on its own clock; a fixed sleep makes a check
    flaky in the worst direction — it can fail while the feature works. Polling
    keeps the assertion the same and removes the race.
    """
    deadline = time.time() + timeout
    while time.time() < deadline:
        if fn():
            return True
        time.sleep(0.1)
    return False


def markers_of(text):
    """Which of the interesting on-screen markers appear in `text`.

    Deliberately a *named list*, so the report says which ones a program draws here
    rather than trusting an author's guess about what vim is supposed to print.
    """
    normed = e.norm(text)
    table = {
        "the file name was drawn": "looprs-vim-e2e.txt" in normed,
        "the blank-line `~` filler was drawn": "~" in text,
        "the typed text landed at the cursor": "typed-through-the-screen-buffer"
        in normed,
        "a status/ruler line was drawn (line/column text)": bool(
            re.search(r"(line\s+\d|col\s+\d|\[\+\-\]\d|NewFile|\[Not modified\])", text)
        ),
    }
    return table


def report_against_baseline(label, base_text, run_text):
    """Every marker the baseline drew must also have reached the looprs screen."""
    base = markers_of(base_text)
    run = markers_of(run_text)
    for name, drawn in base.items():
        if not drawn:
            na(f"{label}: {name}", "the bare pty never drew it either, so it is not a "
                                  "signal here (checking for it would prove nothing)")
            continue
        check(f"{label}: {name} (baseline drew it, looprs showed it too)", run.get(name, False))


def vim_part():
    if os.path.exists(PROBE):
        os.remove(PROBE)
    argv = ["vim", "-u", "NONE", "-i", "NONE", "-n", PROBE]

    # ---- 1. the baseline: vim on a bare pty, same keystrokes ----
    base = Bare(argv)
    time.sleep(1.5)
    base.send(b"i")
    time.sleep(0.4)
    base.send(b"typed-through-the-screen-buffer")
    time.sleep(0.4)
    # Everything the *bare* program drew before it was quit. This is the yardstick:
    # one screen's worth of paint, however the program chose to spell it.
    base_first = base.text()
    base.send(b"\x1b")  # Esc: leave insert mode
    time.sleep(0.4)
    base.send(b":wq!\r")
    time.sleep(1.5)
    base_text = base.text()
    base.quit()
    base_wrote = os.path.exists(PROBE) and "typed-through-the-screen-buffer" in open(
        PROBE, encoding="utf-8", errors="replace"
    ).read()
    check(
        "baseline: vim on a bare pty can be driven to write the file (the control works)",
        base_wrote,
    )
    if not base_wrote:
        say("the baseline itself could not be driven; every comparison below would be "
            "against a broken control, so stopping here rather than reporting them")
        return
    os.remove(PROBE)

    # ---- 2. the same thing through looprs ----
    d = e.Driver()
    say(f"spawned {e.BIN} (pid {d.proc.pid}) in a {ROWS}x{COLS} pty")
    d.wait(1.2)
    d.send(b"\t")  # Beads -> Pi
    d.wait(0.3)
    d.send(b"\t")  # Pi -> Bash
    d.wait(0.5)

    start = len(d.text())
    d.send(b"vim -u NONE -i NONE -n " + PROBE.encode() + b"\r")
    d.wait(2.5)
    opened = d.text()[start:]
    check(
        "vim starts under looprs without the 'not to a terminal' warning",
        "not to a terminal" not in e.norm(opened),
    )

    start = len(d.text())
    d.send(b"i")
    d.wait(0.5)
    # Typed one character at a time on purpose. Writing the whole string in one
    # burst drops characters through the inline passthrough -- measured on the
    # pre-change binary too (`typed-through-th`, three runs out of three), so it
    # is a pre-existing property of the key path and not what this check is about.
    # Pacing keeps the check asking the question it means: does the typed text
    # *reach the screen while a child holds it*, not does a 31-byte burst survive.
    # The burst itself is recorded in the ticket notes so the pacing here is not
    # mistaken for hiding it.
    burst = "typed-through-the-screen-buffer"
    for ch in burst:
        d.send(ch.encode())
        time.sleep(0.05)
    arrived = wait_until(
        lambda: "typed-through-the-screen-buffer" in e.norm(d.text()[start:]), 6.0
    )
    d.wait(0.4)
    painting = d.text()[start:]
    check(
        "the typed text reaches the screen while the program holds it",
        arrived,
        f"nothing of the screen arrived: {e.norm(painting)[:120]!r}",
    )
    # The whole first screen, measured against the yardstick rather than against a
    # guessed constant: if looprs only ever showed the typed text, this number is a
    # fraction of what the bare program drew.
    vim_window = opened + painting
    say(
        f"screen paint: baseline {len(e.norm(base_first))} chars, "
        f"looprs {len(e.norm(vim_window))} chars"
    )
    check(
        "a whole screen of paint reaches the user, not just the typed characters",
        len(e.norm(vim_window)) >= 0.6 * len(e.norm(base_first)),
        f"looprs showed {len(e.norm(vim_window))} of the {len(e.norm(base_first))} "
        "chars the bare pty drew",
    )

    # The Esc half: it must be the program's Esc, not the app's interrupt.
    d.send(b"\x1b")
    d.wait(0.6)
    d.send(b":wq!\r")
    d.wait(2.5)
    wrote = os.path.exists(PROBE) and "typed-through-the-screen-buffer" in open(
        PROBE, encoding="utf-8", errors="replace"
    ).read()
    check("Esc + `:wq!` writes the file (Esc reached vim as Esc, `:` as `:`)", wrote)
    after = d.text()[len(painting) + start:]
    check(
        "the app never reported an interrupt during the vim session (Esc was not 0x03)",
        "interrupted" not in after,
        after[:200],
    )
    report_against_baseline("vim", base_first, vim_window)

    # Still usable afterwards — the alt-screen hand-back and the re-anchor.
    d.expect_after(
        "after vim, the shell still runs commands and they reach the screen",
        b"echo post-vim | tr a-z A-Z\r",
        "POST-VIM",
    )
    d.send(b"\x11")  # Ctrl-Q
    for _ in range(60):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    check("Ctrl-Q quits after vim", d.proc.poll() is not None)


def visible_lines(text):
    """The line numbers visible in a capture of the screen."""
    return {int(m) for m in re.findall(r"line\s*(\d{3})", e.norm(text))}


def less_part():
    with open(LONG, "w", encoding="utf-8") as f:
        for i in range(1, 201):
            f.write(f"line {i:03d} of the long file\n")

    # ---- the baseline: bare `less`, so "the first screen" is measured, not guessed
    base = Bare(["less", "-n", LONG])
    time.sleep(1.5)
    base_first = base.text()
    base_first_lines = visible_lines(base_first)
    depth = max(base_first_lines) if base_first_lines else 0
    base_had_filler = "~" in base_first
    base.quit()
    say(f"baseline: less's first screen reaches line {depth:03d}"
        f" (filler {'drawn' if base_had_filler else 'not drawn'})")
    if depth < 5:
        say("the baseline less did not draw a readable first screen; skipping the "
            "less checks rather than comparing against a broken control")
        return

    d = e.Driver()
    d.wait(1.0)
    d.send(b"\t")
    d.wait(0.3)
    d.send(b"\t")
    d.wait(0.5)

    start = len(d.text())
    d.send(b"less -n " + LONG.encode() + b"\r")
    d.wait(2.0)
    first = d.text()[start:]
    first_lines = visible_lines(first)
    check(
        "less shows the top of the file",
        1 in first_lines,
        f"visible: {sorted(first_lines)[:5]}",
    )
    check(
        "less's own status line is on screen",
        "looprs-long-for-less.txt" in e.norm(first),
        e.norm(first)[:120],
    )
    check(
        f"the first screen is as full as the bare one (reaches line {depth:03d})",
        max(first_lines) >= depth - 1 if first_lines else False,
        f"looprs reaches {max(first_lines) if first_lines else 0:03d}",
    )
    if base_had_filler:
        check("less's filler (`~`) reaches the screen", "~" in first)

    # Scroll a half page down: what becomes visible must be *deeper* than the whole
    # first screen was, which only a real scroll can do.
    start = len(d.text())
    d.send(b"d")
    d.wait(1.5)
    scrolled = d.text()[start:]
    seen_after = visible_lines(scrolled)
    check(
        f"scrolling down reveals lines below the first screen (past {depth:03d})",
        any(n > depth for n in seen_after),
        f"newly visible: {sorted(seen_after)[:6]}",
    )

    # ...and back up, which is the part that needs real cursor addressing rather
    # than a fresh forward print.
    start = len(d.text())
    d.send(b"g")
    d.wait(1.5)
    back = visible_lines(d.text()[start:])
    check(
        "scrolling back to the top shows line 001 again",
        1 in back and max(back) <= depth,
        f"visible after `g`: {sorted(back)[:6]}",
    )

    d.send(b"q")
    d.wait(1.5)
    d.expect_after(
        "quitting less leaves the shell usable and the screen ours",
        b"echo post-less | tr a-z A-Z\r",
        "POST-LESS",
    )
    d.send(b"\x11")
    for _ in range(60):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    check("Ctrl-Q quits after less", d.proc.poll() is not None)


def paint_part():
    """A program that repaints in place *without* an alt screen.

    Same requirement, different mechanism: `printf` writes a line, then moves the
    cursor back over it. Nothing switches screens, so nothing announces the hand
    back either — the command boundary has to.
    """
    d = e.Driver()
    d.wait(1.0)
    d.send(b"\t")
    d.wait(0.3)
    d.send(b"\t")
    d.wait(0.5)
    start = len(d.text())
    d.send(
        b"printf 'before-the-paint\\n'; sleep 0.4; printf '\\033[1Aoverwrote-the-top'"
        b"; sleep 0.4; printf '\\nfinished\\n'\r"
    )
    d.wait(3.0)
    stretch = d.text()[start:]
    check(
        "a program that repaints in place is shown while it runs",
        "overwrote-the-top" in e.norm(stretch),
        e.norm(stretch)[:160],
    )
    d.expect_after(
        "when it ends, the shell is usable again (the screen came back on the command boundary)",
        b"echo after-paint | tr a-z A-Z\r",
        "AFTER-PAINT",
    )
    d.send(b"\x11")
    for _ in range(60):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    check("Ctrl-Q quits after the repainting program", d.proc.poll() is not None)


def main():
    if not os.path.exists(e.BIN):
        say(f"no binary at {e.BIN}; run `cargo build` first")
        return 2
    # Two mode sets, because there are two different screens to prove against.
    #
    # * "" is the inline pane: what looprs runs by default, and the set every
    #   check already in this file was written for. It has to keep passing
    #   unchanged -- the handover rule must not cost the inline path anything.
    # * "all" is the whole ledger (`alt_screen`, `cursor_hidden`, the three mouse
    #   modes, `bracketed_paste`): the screen ADR-0004 rule 1 takes, and the one a
    #   full-screen child's own `?1049h`/`?1049l` pair collides with. The
    #   handover checks only mean something in that run, so the mode set belongs in
    #   this file rather than in a second spike that can rot separately.
    for modes in ["", "all"]:
        os.environ["LOOPRS_MODES"] = modes
        say("")
        say("########## "
            + ("inline pane (default)" if not modes else f"alternate screen: {modes}")
            + " ##########")
        vim_part()
        less_part()
        paint_part()
        if modes:
            alt_handover_part()
            killed_holds_screen_part()
            child_returns_to_our_screen_part()
        else:
            na("the alt-screen handover checks", "this run has no alternate screen; "
               "they run under LOOPRS_MODES=all, below")

    passed = sum(1 for _, ok in e.RESULTS if ok)
    total = len(e.RESULTS)
    say(f"{passed}/{total} checks passed")
    failed = [n for n, ok in e.RESULTS if not ok]
    if failed:
        say("failed: " + ", ".join(failed))
        return 1
    return 0


# ==================== the alternate screen is OURS (looprs-pdl.12) ==========
# Everything above runs in the inline pane, where a full-screen child switching to
# its own alternate screen is nobody's problem but its own. These parts run with
# `LOOPRS_MODES=all`, where looprs itself lives in the alternate screen, which is
# the arrangement ADR-0004 rule 1 commits to and the one the collision is about.
#
# The decision under test, from the ticket's Q1: the child's `?1049h` is CUT, not
# teed. The reasoning it defends is the asymmetric one:
#
#   teeing the enter  -> the terminal saves whatever is on screen as the "main"
#                       screen. Inside our own alternate screen that saved state
#                       IS our previous frame, so the user's real scrollback is
#                       destroyed the moment vim starts. The damage is permanent
#                       and lands before anything could be corrected.
#   teeing the leave  -> the terminal restores that saved state, i.e. our old
#                       frame, and the user is dropped out of looprs' screen while
#                       looprs is still drawing into it.
#   cutting the enter -> the child paints on a canvas we hand it inside the
#                       screen we already own. Nothing is saved, nothing is
#                       destroyed, and there is no child switch left to undo.
#
# Each part drives the real binary in a real PTY and reads the real wire. Nothing
# below is a guess about a timing race: every one of these is a byte pattern that
# either reached the terminal or did not.
def _fold(text):
    """Fold a slice of the wire the way a terminal keeps its modes."""
    return ModeTrace(text)


def _wire(drv, frm=0, to=None):
    """The raw bytes of the wire, as text -- what ModeTrace folds over."""
    return bytes(drv.raw[frm:(to if to is not None else len(drv.raw))]).decode(
        "utf-8", errors="replace"
    )


def _slice(drv, frm=0, to=None):
    return bytes(drv.raw[frm:(to if to is not None else len(drv.raw))])


def _since(drv, byte_off):
    """The wire after a byte offset, as text.

    Byte offsets are the only marks that mean anything here, and the app draws
    multi-byte glyphs, so the decoded string is shorter than the capture it came
    from: slicing `d.text()` by a capture offset silently drops the tail -- which
    is how one of these checks came to "fail" on a run that was clean.
    """
    return bytes(drv.raw[byte_off:]).decode("utf-8", errors="replace")


def _counts(drv, frm=0, to=None):
    t = _fold(_wire(drv, frm, to))
    return t.on_counts, t.off_counts, t.state


def go_to_bash(d):
    """Two tabs: Beads -> Pi -> Bash, the way the ticket's session starts."""
    d.send(b"\t")
    time.sleep(0.3)
    d.send(b"\t")
    time.sleep(0.5)


def alt_handover_part():
    """The handover proper: vim inside the alternate screen looprs lives in.

    The decision under test is the ticket's Q1: the child's `?1049h` is CUT, not
    teed. What that defends is the asymmetric cost of the two branches:

      teeing the enter  -> the terminal saves whatever is on screen as its "main"
                         screen. In here that saved state *is* our previous frame,
                         so the user's real scrollback is destroyed the moment
                         vim starts. The damage lands before anything could be
                         corrected.
      teeing the leave  -> the terminal restores that saved state: our old frame,
                         with looprs still drawing into a screen the user has been
                         dropped out of.
      cutting the enter -> the child paints on a canvas handed to it inside the
                         screen we already own. Nothing is saved, nothing is
                         destroyed, and there is no child switch left to undo.

    Every check below is a byte pattern that either reached the terminal or did
    not. None of them is a guess about a timing race.
    """
    say("")
    say("--- alt-screen handover: vim takes the screen we are living in ---")
    if os.path.exists(PROBE):
        os.remove(PROBE)
    d = e.Driver()
    d.wait(1.5)
    go_to_bash(d)

    # ---- 1. looprs got the alternate screen exactly once: itself, at startup.
    #
    # Counted over the whole run so far. One enter means the ledger's own at
    # startup; two would mean the child's enter was teed as well, which is the
    # destructive branch — it re-bases the terminal's saved main screen onto our
    # frame, and the user's scrollback is gone whether or not anything else works.
    on, off, st = _counts(d)
    check("alt: one `?1049h` on the wire before the child, ours at startup",
          on["alt_screen"] == 1 and st["alt_screen"],
          f"enter events={on['alt_screen']}, alt_screen={st['alt_screen']}")

    start = len(d.raw)
    # The modes as they stood the moment before the child took the screen. The
    # strongest form of the handback promise is that the child leaves *none* of
    # them different: what was on before is on after, and the handover was
    # invisible in the mode ledger even though it was the whole screen.
    before_modes = dict(_counts(d)[2])
    d.send(f"vim -u NONE -i NONE -n {PROBE}\r".encode())
    d.wait(2.5)

    # ---- 2. the child's enter never reached the terminal.
    #
    # The same count again, now that the child has entered. Still one: the child's
    # `?1049h` was cut, and the blank canvas the child expects was substituted
    # for it (a clear at home, not a switch).
    on, off, st = _counts(d, start)
    check("alt: the child's `?1049h` never reached the wire",
          on["alt_screen"] == 0,
          f"alt enters after the child started={on['alt_screen']}")
    check("alt: the child was handed a blank canvas instead of a screen switch",
          b"\x1b[H\x1b[2J" in _slice(d, start),
          "the enter is cut, but the child still expects the clear `?1049h` implies")

    # ---- 3. the child still gets the screen and still paints.
    d.send(b"i")
    d.wait(0.5)
    # Paced for the same reason as in `vim_part`: a single 34-byte burst loses
    # characters in the key path, which is a separate matter from this check.
    for ch in "alt-screen-typed-through-the-buffer":
        d.send(ch.encode())
        time.sleep(0.05)
    arrived = wait_until(
        lambda: "alt-screen-typed-through-the-buffer" in e.norm(_since(d, start)), 6.0)
    check("alt: the child's paint reaches the user while it holds the screen", arrived)

    # ---- 4. no leave from the child reached the wire mid-run.
    #
    # The bug the ticket names is one `?1049l` from the child dropping the user out
    # of our screen. Nothing in this run may leave the alternate screen until the
    # app itself exits, so the running count of leaves is the assertion.
    on, off, st = _counts(d)
    check("alt: no `?1049l` during the run -- the user stays on our screen",
          off["alt_screen"] == 0,
          f"leaves during the run={off['alt_screen']}")

    # ---- 5. the child's own non-screen modes pass through: control.
    #
    # vim hides the cursor with `?25l`. That is not a screen switch and must not
    # be cut; if it were, the cut would be too wide and this control would see it.
    check("alt: the child's non-screen modes are not cut (cursor hide passed through)",
          b"\x1b[?25l" in _slice(d, start) or b"\x1b[?25h" in _slice(d, start),
          "vim's own cursor mode reached the terminal, as it should")

    # ---- 6. hand the screen back and check the takeback.
    d.send(b"\x1b")
    d.wait(0.6)
    back_at = len(d.raw)
    d.send(b":wq!\r")
    d.wait(2.5)
    took_back = _slice(d, back_at)

    # The leave is cut, so the only leaves in the whole stream are still zero —
    # and the app is still on its own screen.
    on, off, st = _counts(d)
    check("alt: the child's leave was cut too; looprs never left its own screen",
          off["alt_screen"] == 0 and st["alt_screen"],
          f"leaves={off['alt_screen']}, alt_screen={st['alt_screen']}")

    # ---- 7. the repaint cleared the child's frame, so no garbage cells.
    #
    # In a plain terminal the child's last frame vanished when it left the
    # alternate screen. Cutting the switch removes that vanishing, so the takeback
    # has to erase: otherwise dead vim's filler stays above the pane forever.
    check("alt: the takeback cleared the screen before repainting",
          b"\x1b[H\x1b[2J" in took_back,
          "no whole-screen erase after the handback; the child's paint would linger")

    # ---- 8. the modes the child switched off are re-asserted.
    #
    # vim leaves mouse reporting and bracketed paste off on exit (measured:
    # `?1000l`, `?1006l`, `?2004l`). looprs holds those modes, so a session that
    # runs a full-screen program loses the mouse and paste-out protection for the
    # rest of its life unless the takeback re-asserts them.
    on, off, st = _counts(d)
    check("alt: the child really did switch the mouse off (control)",
          off["mouse_report"] > 0 or off["mouse_sgr"] > 0 or off["bracketed_paste"] > 0,
          f"child's switch-offs: mouse={off['mouse_report']}, "
          f"sgr={off['mouse_sgr']}, paste={off['bracketed_paste']}")
    check("alt: mouse reporting is on again after the child",
          st["mouse_report"] and st["mouse_drag"] and st["mouse_sgr"],
          f"report={st['mouse_report']}, drag={st['mouse_drag']}, sgr={st['mouse_sgr']}")
    check("alt: bracketed paste is on again after the child",
          st["bracketed_paste"], f"bracketed_paste={st['bracketed_paste']}")
    # The cursor is not "hidden" after the handback -- the caret is back, because
    # the input box wants it. What matters is that the child left no mode
    # different from how it found it, mode for mode, including the one the app
    # flips itself. A cursor the child switched off and nobody switched back is the
    # bug; the caret being visible is the app's own business.
    changed = {m: (before_modes[m], st[m]) for m in before_modes if before_modes[m] != st[m]}
    check("alt: the child left no watched mode different from before it took the screen",
          not changed,
          " ".join(f"{m}:{int(a)}->{int(b)}" for m, (a, b) in changed.items()))
    check("alt: the re-assert does NOT re-enter the alternate screen",
          b"\x1b[?1049h" not in took_back and b"\x1b[?1047h" not in took_back,
          "re-sending `?1049h` would save our own frame as the user's main screen — "
          "the exact destruction the whole rule exists to prevent")

    # ---- 9. and the app still works.
    d.expect_after("alt: a command runs after the handback",
                  b"echo POST-ALT-HANDOVER | tr a-z A-Z\r", "POST-ALT-HANDOVER")

    # ---- 10. the user's main-screen scrollback was never written to.
    #
    # This is the invariant the whole design is there for, and the wire can prove
    # it: the user's main screen is only ever shown by a leave, and across the
    # entire run — startup, the child, the handback — not one leave went out. So
    # no frame of ours and none of the child's was ever composited onto the user's
    # scrollback, and nothing was saved over it either.
    on, off, st = _counts(d)
    check("alt: across the whole run, the main screen was never shown and never saved",
          off["alt_screen"] == 0 and on["alt_screen"] == 1,
          f"enters={on['alt_screen']} (want 1: ours), leaves={off['alt_screen']} (want 0)")

    # ---- 11. quitting from here leaves the screen exactly once.
    quit_at = len(d.raw)
    d.send(b"\x11")  # Ctrl-Q
    for _ in range(60):
        if d.proc.poll() is not None:
            break
        time.sleep(0.1)
    final = _fold(_wire(d))
    check("alt: Ctrl-Q quits", d.proc.poll() is not None)
    check("alt: the exit leaves the alternate screen exactly once",
          final.off_counts["alt_screen"] == 1,
          f"leaves over the whole session={final.off_counts['alt_screen']} "
          "(exactly one: ours, at exit)")
    # Compared in byte space, which is where the marks are. `ModeTrace` indexes the
    # decoded string, and this app draws multi-byte glyphs, so a character offset
    # and a capture offset are not the same number -- a lesson learned the hard way
    # when this check "failed" on a run that was in fact clean.
    last_leave = bytes(d.raw).rfind(b"\x1b[?1049l")
    check("alt: the one leave is the exit's own, after the quit was pressed",
          last_leave >= quit_at,
          f"leave at {last_leave}, quit pressed at {quit_at} "
          f"(stream is {len(d.raw)} bytes)")
    if os.path.exists(PROBE):
        os.remove(PROBE)


def killed_holds_screen_part():
    """The child is killed while it holds the screen.

    A child that dies inside its own alternate screen never runs its leave — the
    classic "screen is stuck" bug in a plain terminal. Here the enter was cut, so
    there was never a switch for it to leave unpaid: the file guard releases at
    the command boundary, no leave is owed, and the app must neither strand the
    display nor emit a leave that would drop the user out of *our* screen.
    """
    say("")
    say("--- alt screen: the child is killed while holding it ---")
    d = e.Driver()
    d.wait(1.5)
    go_to_bash(d)
    start = len(d.raw)
    # Foreground, and short. Backgrounding it (`&`) lets the shell return while the
    # child is still painting over the prompt, so the release is not the command
    # boundary's to make and the next check races a child that is still drawing.
    # In the foreground the child paints, SIGKILLs itself and the command ends --
    # the shape the ticket describes and the shape the app has to survive. Short,
    # because every second of that sleep is a second of a screen nobody is watching.
    d.send(b"sh -c 'printf \"\\033[?1049h\"; echo CHILD-HOLDS-THE-SCREEN; "
          b"sleep 1.5; kill -9 $$'\r")
    d.wait(3.5)
    on, off, st = _counts(d, start)
    check("killed: the child entered and painted on the screen handed to it",
          "CHILD-HOLDS-THE-SCREEN" in e.norm(_since(d, start)),
          "its paint never arrived, so the child never got the canvas")
    check("killed: the child's enter was cut",
          on["alt_screen"] == 0, f"alt enters={on['alt_screen']}")
    check("killed: a child SIGKILLed inside the screen owes no leave",
          off["alt_screen"] == 0,
          f"leaves on the wire={off['alt_screen']} — the enter was cut, so there "
          "is no child switch to pay back")
    d.expect_after("killed: the app repaints and runs the next command",
                  b"echo ALIVE-AFTER-KILL | tr a-z A-Z\r", "ALIVE-AFTER-KILL")
    on, off, st = _counts(d)
    check("killed: the app never left its own screen", st["alt_screen"],
          f"alt_screen={st['alt_screen']}")


def child_returns_to_our_screen_part():
    """The child leaves the alternate screen and goes back to line output.

    The ticket's last edge: "a child that returns to *our* screen rather than its
    own". Because the enter was cut, the child's leave is the leave of a screen it
    never switched — so it is cut too, the app keeps the screen the user was
    looking at all along, and the child's ordinary line output after it is just
    output: it lands in the transcript like any other command's.
    """
    say("")
    say("--- alt screen: the child returns to line output, not to a screen ---")
    d = e.Driver()
    d.wait(1.5)
    go_to_bash(d)
    start = len(d.raw)
    d.send(b"sh -c 'printf \"\\033[?1049h\\033[2JPAINTED-THEN-LEFT\\n\"; "
          b"printf \"\\033[?1049l\"; echo LINE-OUTPUT-AFTER-LEAVE'\r")
    d.wait(3.0)
    text = e.norm(_since(d, start))
    check("returned: the child's paint arrived on the canvas it was handed",
          "PAINTED-THEN-LEFT" in text)
    check("returned: the child's `?1049l` never reached the terminal",
          _counts(d, start)[1]["alt_screen"] == 0,
          f"leaves on the wire={_counts(d, start)[1]['alt_screen']}")
    check("returned: the child kept drawing and it went to the transcript",
          "LINE-OUTPUT-AFTER-LEAVE" in text,
          "line output after a cut leave is still output, not a lost screen")
    check("returned: the app is still on its own screen afterwards",
          _counts(d)[2]["alt_screen"],
          "a cut leave cannot drop the user; the app never lost the screen")
    d.expect_after("returned: usable after the child went back to line output",
                  b"echo STILL-HERE-AFTER | tr a-z A-Z\r", "STILL-HERE-AFTER")


if __name__ == "__main__":
    sys.exit(main())
