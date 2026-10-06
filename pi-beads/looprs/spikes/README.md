# ADR-0001 spikes

Throwaway programs that prove or disprove the risky claims behind
[ADR-0001](../docs/adr/0001-bash-terminal-state-pty.md) (Bash terminal state: pipes vs a real
PTY). Nothing here is production code. Decisions live in the ADR; this directory is the
evidence, so a future reader can re-run it instead of re-arguing.

| File | What it is |
|---|---|
| `examples/spike_bash.rs` | The harness. `spawn_pipes()` (Option A) and `spawn_pty()` (Option B) are the only per-option code; everything after that is shared. |
| `spikes/probes.sh` | The probe script fed line-by-line to a live bash by **both** options. |
| `spikes/interleave_only.sh` | Small stdout/stderr-ordering probe, for repeating under contention. |
| `spikes/interleave_stress.sh` | Same, with 4 KiB filler per line so both pipes are busy. |
| `spikes/pi_bash_rpc.py` | Question 4: does pi's RPC `bash` command keep shell state? (It does not.) |
| `spikes/bash_e2e.py` | Drives the real TUI in a real PTY: the Bash terminal state end to end (echo/cd/exit codes/Ctrl-C/`exit`/Ctrl-Q). |
| `spikes/vim_fullscreen.py` | looprs-4hv: does vim's screen reach the user? Scored against a bare-pty control. |
| `spikes/fullscreen_e2e.py` | looprs-4hv acceptance: vim + less + a program that repaints in place, each against a bare-pty control. |
| `spikes/cancel_e2e.py` | looprs-5g7 acceptance: Esc in all three modes inside the real TUI — latency to the acknowledgement and to the completion word, the silent-idle rows, and "a cancelled beads pass did not start another bead". `pi`/`bd` are fakes the script writes, so it costs no model call. |
| `spikes/viewport_e2e.py` | looprs-afw acceptance: the live region's *shape* in a real pty. A long streamed answer has to reach more than the ten rows `const VIEWPORT_H: u16 = 10` allowed, grow into them as the text arrives, and survive a window resize taken mid-stream. Run against the pre-afw binary as a control it fails both counts (`spikes/results/viewport-e2e-before-afw.log`), which is the point. |
| `spikes/shutdown_e2e.py` | looprs-ecr's exit-path spike, widened by looprs-pdl.3 into the **terminal mode ledger**'s proof. Eight scenarios — quit mid-stream in Pi, quit with a busy Bash shell, quit against a child that ignores stdin EOF **and** SIGTERM, the whole mode set (`LOOPRS_MODES=all`: alternate screen, mouse report/drag/SGR, bracketed paste, hidden cursor) quit cleanly, a full-screen child **killed while it holds the alternate screen**, `SIGTERM` with that set held, `SIGHUP` on the plain inline run, and a forced panic inside the draw (`LOOPRS_PANIC=draw`) — judged on the bytes the pty got and on the process table: the live tail must land **above** the erase line (a repaint cannot do that, so the check cannot be satisfied by a race), the run must end with one erase, the ledger's own leaves and one newline, the tty must be cooked again, no `ESC[6n` may be issued on the exit path, no child may survive, and every mode the app switched on must be left **exactly once** — including by a process that panics or is killed, and including a screen the app only *passed through* to a child that never gave it back (the leave must come off **before** the pane erase, and the tail after it must be the ordinary inline hand-back). The spike keeps its **own** ledger of the wire (`ModeTrace`) rather than reading the app's, and counts the leaves instead of only the final state: a mode turned off twice and one turned off once end in the same place. **149/149**; against the pre-ticket binary **85/112** (`spikes/results/shutdown-e2e-pdl3-control.log`), where the alternate screen is never taken, `SIGTERM`/`SIGHUP` leave the **tty raw**, `?25h` still lands after the closing newline, and quitting on a child-held screen ends with `alt_screen=1` and **zero `?1049l` bytes in the whole capture**. See [ADR-0006](../docs/adr/0006-terminal-mode-ledger.md). Pass a name to run one group: `python3 spikes/shutdown_e2e.py alt child sigterm sighup panic`. |
| `spikes/status_e2e.py` | looprs-guh acceptance: the one-row status band, in a real pty, driven off the real state machines — an empty board, a `bd` that fails outright, a beads pass Tabbed away from mid-run (ADR-0002's "the load-bearing case"), a `sleep` holding a shell busy, and a resize to the 40-column floor taken mid-run. Costs no model call (`pi` is `fake_pi_slow.py`, `bd` is a bash fake the script writes). **20/20**; against the pre-looprs-guh binary, 0 of the 13 row-specific checks fire (`spikes/results/status-e2e-control.log`) — which is what makes the passing run mean something. See *"Why the spike reads the wire and not the screen"* in `docs/testing.md` before editing the needles. |
| `spikes/flash_e2e.py` | The live-region flash, timed off the wire. The app flushes its erase (`ESC[<row>;1H ESC[J`) separately from the frame that replaces what it erased, so the interval between that erase and the next printable bytes **is** the interval a terminal had nothing there — no log parsing, no screenshots, just the clock on the reading side of the pty. Asserts the hole is inside 1.5 ms and that the reshape count stays under half the rows the answer spread over. Against the pre-fix binary: **0/3**, a 3.1–9.6 ms hole on all 25 erases (`spikes/results/flash-e2e-control.log`); fixed: **3/3**, 0.20–0.38 ms over 13 erases. |
| `spikes/fake_pi_slow.py` | The `pi` that spike needs: one assistant message held **open** while a paragraph dribbles out. `tests/fixtures/fake_pi_chat.py` emits `text_delta` and `message_end` back to back, so its reply is flushed before a frame can be drawn over it and there is no live tail to measure. |
| `spikes/results/` | Committed raw output of the runs quoted in the ADR. |

## Running it

**Run under a real terminal.** The harness needs `/dev/tty` to exist and be openable, otherwise
the escape-hatch / vim / resize probes are meaningless. `script -q /dev/null <cmd>` allocates a
PTY for the harness on macOS/BSD (use `script -q -c "<cmd>" /dev/null` on Linux):

```sh
script -q /dev/null cargo run -q --example spike_bash -- pipes | tee spikes/results/pipes.log
script -q /dev/null cargo run -q --example spike_bash -- pty   | tee spikes/results/pty.log
python3 spikes/pi_bash_rpc.py

# The TUI drivers need the binary built, and a PTY of their own (they make one).
cargo build
python3 spikes/bash_e2e.py        | tee spikes/results/bash-e2e.log
python3 spikes/fullscreen_e2e.py  | tee spikes/results/fullscreen-e2e.log
python3 spikes/vim_fullscreen.py  | tee spikes/results/vim-fullscreen.log
python3 spikes/cancel_e2e.py      | tee spikes/results/cancel-e2e.log
python3 spikes/viewport_e2e.py    | tee spikes/results/viewport-e2e.log
python3 spikes/shutdown_e2e.py    | tee spikes/results/shutdown-e2e.log
python3 spikes/status_e2e.py      | tee spikes/results/status-e2e.log

# …and the status spike against the pre-looprs-guh binary, as a control (the same
# worktree recipe as above; `--control` inverts the verdict and names any needle
# that fired without a status row to fire on):
#   LOOPRS_BIN=/tmp/ctl-target/debug/looprs python3 spikes/status_e2e.py --control

# …and the same shutdown spike against the pre-looprs-ecr binary, as a control
# (build that revision into its own target dir first, so this one stays usable):
#   git worktree add --detach /tmp/looprs-prefix HEAD
#   cd /tmp/looprs-prefix/pi-beads/looprs && cargo build --target-dir /tmp/base-target
#   LOOPRS_BIN=/tmp/base-target/debug/looprs python3 spikes/shutdown_e2e.py
```

**Why the full-screen spikes run a control.** "Did the user see it?" is not answered
by grepping the capture for a string the author expects: `--INSERT--` is terminfo-dependent
(vim with `TERM=xterm-256color` never writes it), so such a check fails with the feature
working and can pass with nothing displayed. `fullscreen_e2e.py` runs the same keystrokes
against a bare pty first and requires looprs to show every marker the control actually
drew; a marker the control never drew is reported `n/a`, out loud, rather than quietly
dropped from the list.

Useful knobs:

- `LOOPRS_SPIKE_BASH=/opt/homebrew/bin/bash` — try a bash other than the system 3.2.
- `LOOPRS_SPIKE_PROBES=spikes/interleave_only.sh` — run a different probe script.
- `LOOPRS_MODES=alt_screen,mouse,bracketed_paste` (or `all`) — the modes the ledger
  switches on, **added to** the default `raw,cursor_hidden`. An unknown name stops the app
  with a message rather than quietly switching on nothing. This is how the full-screen mode set
  is proved on a real pty before looprs-pdl.4 makes it the default; see
  [ADR-0006](../docs/adr/0006-terminal-mode-ledger.md).
- `LOOPRS_PANIC=draw` — **debug builds only**. Makes the frame panic on purpose, so the
  teardown contract can be proved against a panic *inside* the draw rather than against a
  bug someone has to reintroduce to cause one.

A run prints a timestamped timeline of every line the child emitted (the timestamp is when the
*harness* saw the bytes, which is what makes the Ctrl-C timing probe readable), then the
extracted `PROBE` lines, then marker checks run against the raw byte stream.

Repeat a probe under contention, which is how the interleaving result was obtained:

```sh
for i in $(seq 1 10); do
  LOOPRS_SPIKE_PROBES=spikes/interleave_only.sh \
    script -q /dev/null cargo run -q --example spike_bash -- pipes \
    | tr -d '\r' | grep 'merged in program order'
done | sort | uniq -c
# 9 NO / 1 YES  ->  Option A's two-thread merge is a race
# (same loop with `-- pty`: 10 YES)
```

## Control lines understood in the probe scripts

```
#wait <ms>       pause the harness
#raw <bytes>     send raw bytes; \xNN, \r, \n, \t are decoded   (e.g. \x03 == Ctrl-C)
#resize <r> <c>  resize the child's pty (Option A reports "nothing to resize")
```

Other `#` lines and blank lines are skipped. Everything else is sent verbatim + `\n`.

## Gotchas worth knowing before editing the probes

- A pty's line discipline **echoes input when it is written**, so in the PTY run the input lines
  appear immediately, in a burst, without a shell prompt. In the pipes run the echo comes from
  bash itself as it reads. Don't read the echo as execution — read the `PROBE` output lines.
- The harness races ahead of the shell. Timing-sensitive probes therefore go **first** in
  `probes.sh`, so `sleep 6` really is the foreground command when the `0x03` lands.
- A live `sudo`/`getpass` password prompt reads `/dev/tty`. In the pipes option that means it
  eats the rest of the probe script, so the probes only ask about it (`dev_tty=openable`) and
  run the non-interactive `-n` / piped `-S` variants last.
- `stty size` before/after `#resize` needs a `#wait` in between: otherwise the shell has not
  caught up and "before" is measured after the resize.
