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
    d.send(b"\t")  # Beeds -> Pi
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
    d.send(b"typed-through-the-screen-buffer")
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
    vim_part()
    less_part()
    paint_part()

    passed = sum(1 for _, ok in e.RESULTS if ok)
    total = len(e.RESULTS)
    say(f"{passed}/{total} checks passed")
    failed = [n for n, ok in e.RESULTS if not ok]
    if failed:
        say("failed: " + ", ".join(failed))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
