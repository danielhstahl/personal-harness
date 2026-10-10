#!/usr/bin/env python3
"""looprs-pdl.13: the copy chords and the page keys, driven through tmux.

Every other spike here drives the app in a pty it owns. This one drives it inside
**tmux**, because tmux is where these chords actually get used — it is the
multiplexer the SSH story rides on, and it sits between the terminal and the app
with its own opinions about control keys, the alternate screen and the mouse.

What is measured, group by group:

  delivery   Does `Ctrl-S` (0x13) arrive as a key event at all? A terminal that
             honours XON/XOFF swallows it and the chord can never arm. The
             arming hint is the proof: nothing else paints that string.
  page       `PageUp`/`PageDown`/`Home`/`End` over a 200-row Bash transcript:
             Home reaches row 1, End reaches row 200, and a PageDown taken at the
             tail does not move (the pin End sets).
  copy       `Ctrl-S o` in Bash, `Ctrl-S a` in Pi, the refusal Bash gives for
             `Ctrl-S a`, and `Ctrl-S t`: the file the toast names is read back
             and its character count is the toast's number.
  ctrlc      `Ctrl-C` interrupts the child and copies nothing.
  child      A full-screen child is never handed 0x13. If it were, it would stop
             painting, and looprs owns `Ctrl-Q` as *quit* — so there would be no
             XON left in the keyboard to give the child back.
  esc        `Esc` with the chord armed lowers the chord and nothing else: the
             next letter types into the box instead of copying.
  quit       `Ctrl-Q` leaves tmux's pane out of the alternate screen.

`pi` and `bd` are fakes this script writes, and that is not optional. The real
`looprs` boots into Beads mode and the beads loop runs `bd update <id> --claim`
before a Tab can reach it: a spike that spends a real claim and a model call to
look at a keyboard binding is a spike that must not be run twice. The run counts
the claims it made against the fake board and prints the number.

  run:     python3 spikes/tmux_keyboard_e2e.py
  one:     python3 spikes/tmux_keyboard_e2e.py delivery copy quit
  output:  spikes/results/tmux-keyboard-e2e.log
"""

import os
import re
import shutil
import subprocess
import sys
import tempfile
import time

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
BIN = os.path.join(ROOT, "target", "debug", "looprs")
LOG = os.path.join(HERE, "results", "tmux-keyboard-e2e.log")
SESSION = "looprs-kbd-e2e"
COLS, ROWS = 100, 32

results = []
lines = []
tmp_global = tempfile.gettempdir()


def say(msg):
    print(msg)
    lines.append(msg)


def check(name, ok, detail=""):
    ok = bool(ok)
    results.append((name, ok))
    say(("PASS  " if ok else "FAIL  ") + name + ("" if ok else f"   [{detail}]"))
    return ok


def report(name, detail):
    """Something observed that is neither pass nor fail — printed, not counted."""
    say(f"note  {name}: {detail}")


def tmux(*args):
    return subprocess.run(["tmux", *args], capture_output=True, text=True)


def pane():
    return tmux("capture-pane", "-pt", SESSION).stdout


def screen():
    return "\n".join(l.rstrip() for l in pane().splitlines() if l.strip())


def send(*keys):
    tmux("send-keys", "-t", SESSION, *keys)


def send_hex(hexc):
    tmux("send-keys", "-t", SESSION, "-H", *hexc.split())


def wait_for(needle, timeout=3.0):
    end = time.time() + timeout
    while time.time() < end:
        if needle in screen():
            return True
        time.sleep(0.05)
    return False


def until_idle(timeout=8.0, stable=3):
    last, n = None, 0
    end = time.time() + timeout
    while time.time() < end:
        cur = pane()
        n = n + 1 if cur == last else 0
        if n >= stable:
            return True
        last = cur
        time.sleep(0.15)
    return False


def in_mode(label, timeout=3.0):
    """The frame names the active mode twice — the status row leads with it and the
    transcript box is titled with it — so look for the box title, which cannot
    match a word inside someone's transcript."""
    end = time.time() + timeout
    while time.time() < end:
        p = pane()
        if f"\u250c{label}" in p:
            return True
        time.sleep(0.1)
    return False


def tab_to(label, max_tabs=3):
    for _ in range(max_tabs):
        send("Tab")
        time.sleep(0.4)
        until_idle(4.0, 2)
        if in_mode(label):
            return True
    return in_mode(label)


FAKE_PI = r"""#!/usr/bin/env python3
import json, sys
def emit(o): sys.stdout.write(json.dumps(o)+"\n"); sys.stdout.flush()
def response(rid, cmd, data=None):
    rec = {"type": "response", "command": cmd, "success": True}
    if rid is not None: rec["id"] = rid
    if data is not None: rec["data"] = data
    emit(rec)
n = 0
while True:
    line = sys.stdin.readline()
    if not line: break
    line = line.strip()
    if not line: continue
    try: cmd = json.loads(line)
    except ValueError: continue
    kind, rid = cmd.get("type"), cmd.get("id")
    if kind == "prompt":
        n += 1
        response(rid, "prompt", {"disposition": "sent"})
        emit({"type": "agent_start"})
        emit({"type": "message_start", "message": {"role": "assistant", "content": []}})
        emit({"type": "message_update", "assistantMessageEvent":
              {"type": "text_delta", "contentIndex": 0,
               "delta": "SPIKE ANSWER %d: the thing to copy with ctrl-s a." % n}})
        emit({"type": "message_end", "message": {"role": "assistant", "content": []}})
        emit({"type": "agent_end", "messages": [], "willRetry": False})
        emit({"type": "agent_settled"})
    else:
        response(rid, kind)
"""

FAKE_BD = """#!/bin/sh
echo "bd $*" >>"$FAKE_BD_LOG"
case "$1" in
  ready) printf '%s\\n' '{"data":[{"id":"spike-bead","title":"a bead from the fake board","status":"open","issue_type":"task"}],"schema_version":1}' ;;
  show)  printf '%s\\n' '[]' ;;
  *)     printf '%s\\n' '{}' ;;
esac
"""


def boot():
    tmux("kill-server")
    time.sleep(0.4)
    tmp = tempfile.mkdtemp(prefix="looprs-tmux-kbd-")
    pi_bin, bd_bin = os.path.join(tmp, "pi"), os.path.join(tmp, "bd")
    bd_log = os.path.join(tmp, "bd.log")
    open(pi_bin, "w").write(FAKE_PI)
    os.chmod(pi_bin, 0o755)
    open(bd_bin, "w").write(FAKE_BD)
    os.chmod(bd_bin, 0o755)
    # A short fake HOME, so the *default* dump directory is the one a real run
    # gets (`$HOME/.cache/looprs`) and the toast's `~` fold is exercised rather
    # than assumed. Short on purpose: at the real `/var/folders/17/...` path the
    # toast is clipped before the file name, which is the finding that moved the
    # default off `temp_dir()` in the first place. Writes stay under /tmp.
    fake_home = "/tmp/lkbdhome"
    os.makedirs(fake_home, exist_ok=True)
    dump_dir = os.path.join(fake_home, ".cache", "looprs")
    env = dict(os.environ)
    env.pop("LOOPRS_TRANSCRIPT_DIR", None)
    env.pop("XDG_CACHE_HOME", None)
    env.update(
        {
            "HOME": fake_home,
            "LOOPRS_PI_BIN": pi_bin,
            "LOOPRS_BD_BIN": bd_bin,
            "FAKE_BD_LOG": bd_log,
            # The runner's real clipboard is shared and its size cap is not ours
            # to probe; osc52 keeps the copy on the wire where the toast's `via`
            # word can be read without a third party.
            "LOOPRS_CLIPBOARD": "osc52",
            "TERM": "tmux-256color",
        }
    )
    subprocess.run(
        ["tmux", "new-session", "-d", "-s", SESSION, "-x", str(COLS), "-y", str(ROWS), BIN],
        env=env,
        check=True,
    )
    report("tmux", tmux("-V").stdout.strip())
    report("TERM inside the pane", tmux("display-message", "-pt", SESSION, "#{TERM}").stdout.strip())
    return tmp, dump_dir, bd_log


# ── groups ─────────────────────────────────────────────────────────────────────
def g_delivery():
    say("\n== 1. does 0x13 survive the trip ==")
    send("C-s")
    armed = wait_for("Copy: ", timeout=2.0)
    check("Ctrl-S reaches the app as a key event (not swallowed as XOFF)", armed)
    row = next((l.strip() for l in screen().splitlines() if "Copy: " in l), "")
    report("hint row", row)
    check(
        "the hint lists every target the table arms",
        all(w in row for w in ("a answer", "o last output", "s selection", "t transcript", "Esc cancel")),
        row,
    )
    hexed = tmux("list-keys", "-T", "root").stdout
    report("tmux's own binding for C-s", "none" if "send-prefix" not in hexed else "see list-keys")
    send("Escape")
    time.sleep(0.3)


def g_page():
    say("\n== 2. the page keys over a 200-row Bash transcript ==")
    check("Tab reaches Bash mode", tab_to("Bash"))
    send("for i in $(seq 1 200); do echo SCROLLROW-$i; done", "Enter")
    time.sleep(1.5)
    until_idle()

    send("End")
    time.sleep(0.6)
    tail = screen()
    check("End shows the last row of the transcript", "SCROLLROW-200" in tail)
    send("PageDown")
    time.sleep(0.5)
    check("PageDown taken at the tail does not move (End re-pinned)", screen() == tail)

    send("Home")
    time.sleep(0.6)
    top = screen()
    check("Home shows the top of the transcript", "SCROLLROW-1" in top and "SCROLLROW-200" not in top)
    send("PageUp")
    time.sleep(0.4)
    check("PageUp at the top does not move", screen() == top)
    send("PageDown")
    time.sleep(0.5)
    mid = screen()
    check("PageDown from the top moved the view", mid != top)
    say(f"note  rows visible at the top: {len(top.splitlines())} (pane is {ROWS} rows)")


def g_copy():
    say("\n== 3. the copy family, in the modes that own each target ==")
    check("Tab reaches Bash mode", tab_to("Bash"))
    send("echo THE-LAST-COMMAND-OUTPUT-MARKER", "Enter")
    time.sleep(1.2)
    until_idle()

    send("C-s")
    time.sleep(0.3)
    send("o")
    toast = wait_for("Copied", timeout=2.5)
    got = next((l.strip() for l in screen().splitlines() if "Copied" in l), "")
    check("Bash: Ctrl-S o copies the last command's output", toast, got)
    report("toast", got)
    m = re.search(r"Copied (\d+) characters", got)
    check("the toast counts characters", bool(m), got)

    first = int(m.group(1)) if m else 0

    # Run a second command of the same shape and copy again. If the command
    # boundary were missing, the second copy would be the *session* and its count
    # would roughly double; equal counts is what "the boundary is real" looks
    # like from outside the process.
    send("echo SECOND-COMMAND-OUTPUT-MARKER", "Enter")
    time.sleep(1.2)
    until_idle()
    send("C-s")
    time.sleep(0.3)
    send("o")
    time.sleep(0.6)
    got2 = next((l.strip() for l in screen().splitlines() if "Copied" in l), "")
    m2 = re.search(r"Copied (\d+) characters", got2)
    second = int(m2.group(1)) if m2 else 0
    check(
        "the second copy is the second command, not the session so far",
        first > 0 and second > 0 and abs(second - first) < max(first, 1),
        f"first={first} second={second}",
    )
    report("counts", f"first={first} second={second}")

    send("C-s")
    time.sleep(0.3)
    send("a")
    time.sleep(0.5)
    refuse = next((l.strip() for l in screen().splitlines() if "Nothing copied" in l), "")
    check("Bash: Ctrl-S a refuses and names the chord that works", "Ctrl-S o" in refuse, refuse)

    check("Tab reaches Pi mode", tab_to("Pi"))
    send("spike prompt", "Enter")
    time.sleep(1.5)
    until_idle()
    send("C-s")
    time.sleep(0.3)
    send("a")
    toast = wait_for("Copied", timeout=2.5)
    report("toast", next((l.strip() for l in screen().splitlines() if "Copied" in l), ""))
    check("Pi: Ctrl-S a copies the last answer", toast)

    send("C-s")
    time.sleep(0.3)
    send("t")
    wrote = wait_for("Wrote ", timeout=3.0)
    said = next((l.strip() for l in screen().splitlines() if "Wrote " in l), "")
    check("Ctrl-S t reports with `Wrote`, not `Copied`", wrote and "Copied" not in said, said)
    m = re.search(r"Wrote (\d+) characters of transcript to (.+)", said)
    check("the toast names a path", bool(m), said)
    if m:
        n, path = int(m.group(1)), m.group(2).strip()
        if path.startswith("~"):
            path = os.path.join("/tmp/lkbdhome", path[2:])
        if os.path.exists(path):
            body = open(path, encoding="utf-8", errors="replace").read()
            check("the file exists and its character count is the toast's number", len(body) == n,
                  f"file={len(body)} toast={n}")
            check("the file holds the answer text the app was showing",
                  "SPIKE ANSWER" in body or "MARKER" in body, repr(body[:80]))
            report("file", f"{path} ({len(body)} chars, {len(body.splitlines())} lines)")
        else:
            check("the file the toast names exists", False, path)


def g_ctrl_c():
    say("\n== 4. Ctrl-C is still the child's ==")
    check("Tab reaches Bash mode", tab_to("Bash"))
    # End first: the page group left this view scrolled up on purpose, and an
    # unpinned view does not follow the tail — the prompt was there the whole
    # time, just not where an unpinned reader is looking.
    send("End")
    time.sleep(0.4)
    send("sleep 30", "Enter")
    time.sleep(1.0)
    send("End")
    time.sleep(0.3)
    t0 = time.time()
    send("C-c")
    back = False
    end = time.time() + 4.0
    while time.time() < end:
        s = screen()
        if "bash-" in s or "$ " in s:
            back = True
            break
        time.sleep(0.05)
    dt = time.time() - t0
    check("Ctrl-C interrupts the child and the shell returns", back, "no prompt within 4s")
    report("interrupt round trip", f"{dt:.2f}s")
    check("Ctrl-C copied nothing", "Copied" not in screen())
    send("C-s")
    time.sleep(0.4)
    check("the app is still alive and arms the chord after the interrupt", "Copy: " in screen())
    send("Escape")
    time.sleep(0.3)


def g_child():
    say("\n== 5. a child holding the screen is never handed 0x13 ==")
    check("Tab reaches Bash mode", tab_to("Bash"))
    vim = shutil.which("vim") or shutil.which("vi")
    scratch = os.path.join(tmp_global, "child-view.txt")
    open(scratch, "w").write("localhost\nsecond row\nthird row\n")
    child = f"{vim or 'less'} -u NONE -i NONE {scratch}" if vim else f"less {scratch}"
    label = "vim" if vim else "less"
    send(child, "Enter")
    time.sleep(2.0)
    until_idle(6.0)
    before = screen()
    check(f"the child ({label}) took the screen", "localhost" in before or label in before,
          before.splitlines()[-1][:COLS] if before else "empty")
    check("looprs's copy hint is not painted over the child", "Copy: " not in before)

    send("C-s")
    time.sleep(0.6)
    check("Ctrl-S did not arm a chord over the child", "Copy: " not in screen())
    # Prove the child is still alive and still painting: type into it and look for it.
    if vim:
        send("iNOT-XOFFED")
        time.sleep(1.0)
        shown = screen()
        repaints = "NOT-XOFFED" in shown
        check("the child still repaints after our Ctrl-S (it was never stopped by XOFF)",
              repaints,
              "nothing new appeared; top row was: %r"
              % (shown.splitlines()[0][:80] if shown else ""))
        report("child's top row after typing", shown.splitlines()[0][:80] if shown else "")
        send("Escape")
        time.sleep(0.3)
        send(":q!", "Enter")
    else:
        send("q")
    time.sleep(1.2)
    until_idle(6.0, 2)
    check("the screen came back to looprs", in_mode("Bash") or "Copy: " not in screen())
    report("after the child", screen().splitlines()[-1][:COLS] if screen() else "")


def g_esc():
    say("\n== 6. Esc with the chord armed lowers the chord, nothing else ==")
    check("Tab reaches Pi mode", tab_to("Pi"))
    send("C-s")
    time.sleep(0.3)
    armed = "Copy: " in screen()
    check("the chord is armed", armed)
    send("Escape")
    time.sleep(0.3)
    check("Esc removed the hint", "Copy: " not in screen())
    # The next `a` must be typing, not a copy.
    send("a")
    time.sleep(0.5)
    check("the letter after the cancelled chord typed instead of copying",
          "Copied" not in screen())
    send("Escape")
    time.sleep(0.4)
    check("Esc with no chord armed did not quit", tmux("has-session", "-t", SESSION).returncode == 0)
    send("Escape")
    time.sleep(0.4)
    check("and the app is still here after a second Esc", tmux("has-session", "-t", SESSION).returncode == 0)


def g_quit():
    say("\n== 7. the alternate screen through tmux, and the hand-back ==")
    alt = tmux("display-message", "-pt", SESSION, "#{alternate_on}").stdout.strip()
    report("pane alternate_on while looprs runs", alt)
    check("looprs holds the alternate screen inside tmux", alt == "1", alt)
    send("C-q")
    time.sleep(1.5)
    gone = tmux("has-session", "-t", SESSION).returncode != 0
    check("Ctrl-Q ends the tmux session (looprs exited, the pane closed)", gone,
          "session still alive")
    if not gone:
        after = tmux("display-message", "-pt", SESSION, "#{alternate_on}").stdout.strip()
        report("alternate_on after quit", after)


GROUPS = {
    "delivery": g_delivery,
    "page": g_page,
    "copy": g_copy,
    "ctrlc": g_ctrl_c,
    "child": g_child,
    "esc": g_esc,
    "quit": g_quit,
}


def main():
    if not os.path.exists(BIN):
        say(f"no binary at {BIN}; run `cargo build` first")
        return 2
    if not shutil.which("tmux"):
        say("no tmux on PATH")
        return 2
    names = sys.argv[1:] or list(GROUPS)
    tmp, dump_dir, bd_log = boot()
    say(f"tmux session {SESSION} ({COLS}x{ROWS}); fakes and dump dir in {tmp}")
    if not wait_for("Tab switch", timeout=6.0):
        say("the app did not come up; aborting")
        return 2
    for n in names:
        fn = GROUPS.get(n)
        if fn is None:
            say(f"unknown group: {n}")
            continue
        try:
            fn()
        except Exception as e:  # noqa: BLE001 - a broken group must not hide the others
            check(f"{n}: group ran", False, repr(e))
    passed = sum(1 for _, ok in results if ok)
    claims = open(bd_log).read().count("--claim") if os.path.exists(bd_log) else 0
    say(f"\n{passed}/{len(results)} checks passed")
    say(f"claims taken against the FAKE board this run: {claims} (the real board was never touched)")
    os.makedirs(os.path.dirname(LOG), exist_ok=True)
    open(LOG, "w").write("\n".join(lines) + "\n")
    tmux("kill-server")
    return 0 if passed == len(results) else 1


if __name__ == "__main__":
    sys.exit(main())
