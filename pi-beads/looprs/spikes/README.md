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
| `spikes/shutdown_e2e.py` | looprs-ecr's exit-path spike, widened by looprs-pdl.3 into the **terminal mode ledger**'s proof. Eight scenarios — quit mid-stream in Pi, quit with a busy Bash shell, quit against a child that ignores stdin EOF **and** SIGTERM, the whole mode set (`LOOPRS_MODES=all`: alternate screen, mouse report/drag/SGR, bracketed paste, hidden cursor) quit cleanly, a full-screen child **killed while it holds the alternate screen**, `SIGTERM` with that set held, `SIGHUP` on the plain inline run, and a forced panic inside the draw (`LOOPRS_PANIC=draw`) — judged on the bytes the pty got and on the process table: the live tail must land **above** the erase line (a repaint cannot do that, so the check cannot be satisfied by a race), the run must end with one erase, the ledger's own leaves and one newline, the tty must be cooked again, no `ESC[6n` may be issued on the exit path, no child may survive, and every mode the app switched on must be left **exactly once** — including by a process that panics or is killed, and including a screen the app only *passed through* to a child that never gave it back (the leave must come off **before** the pane erase, and the tail after it must be the ordinary inline hand-back). The spike keeps its **own** ledger of the wire (`ModeTrace`) rather than reading the app's, and counts the leaves instead of only the final state: a mode turned off twice and one turned off once end in the same place. **149/149**; against the pre-ticket binary **85/112** (`spikes/results/shutdown-e2e-pdl3-control.log`), where the alternate screen is never taken, `SIGTERM`/`SIGHUP` leave the **tty raw**, `?25h` still lands after the closing newline, and quitting on a child-held screen ends with `alt_screen=1` and **zero `?1049l` bytes in the whole capture**. See [ADR-0006](../docs/adr/0006-terminal-mode-ledger.md). Pass a name to run one group: `python3 spikes/shutdown_e2e.py alt child sigterm sighup panic`. **Rewritten by looprs-pdl.4**: the erase-anchored checks ("the live pane is erased", "the live tail landed above the erase line", "one closing newline after the erase") measured a pane that no longer exists. In their place — nothing painted before the app takes the alternate screen, no pane-erase shape anywhere in the run, the answer painted on the screen the frame left, and **nothing written after the `?1049l`** (the closing newline belonged to the inline pane's last row; after the leave the cursor is the user's prompt's cursor). **153/153**, the count having gone up by four with the rewrite.  |
| `spikes/status_e2e.py` | looprs-guh acceptance: the one-row status band, in a real pty, driven off the real state machines — an empty board, a `bd` that fails outright, a beads pass Tabbed away from mid-run (ADR-0002's "the load-bearing case"), a `sleep` holding a shell busy, and a resize to the 40-column floor taken mid-run. Costs no model call (`pi` is `fake_pi_slow.py`, `bd` is a bash fake the script writes). **20/20**; against the pre-looprs-guh binary, 0 of the 13 row-specific checks fire (`spikes/results/status-e2e-control.log`) — which is what makes the passing run mean something. See *"Why the spike reads the wire and not the screen"* in `docs/testing.md` before editing the needles. |
| `spikes/flash_e2e.py` | The blank-the-screen interval, timed off the wire. **Premise rewritten by looprs-pdl.4**: this spike used to measure the inline pane's flash — the app flushed `ESC[<row>;1H ESC[J` on its own and composed the replacement frame after, so the gap between them *was* the time a terminal had nothing there (**0/3** on the pre-fix binary at 3.1–9.6 ms over 25 erases, `spikes/results/flash-e2e-control.log`; **3/3** after the erase/paint fix at 0.18–0.35 ms over 13). The full-screen frame has no such step: it diffs the whole screen and writes once per frame. It now asserts **zero** partial erases (`ESC[J`/`ESC[0J`) on the wire, times any `ESC[2J` — the full repaint a resize or a returned full-screen child forces — against the same 1.5 ms budget, and carries two non-vacuity checks so a run that streamed nothing cannot pass for a measured one. **4/4** migrated; the control inverted, and the pre-pdl.4 binary fails it with **26 partial erases, 1.83–3.58 ms** (`spikes/results/flash-e2e-pdl4-control.log`). |
| `spikes/scrollback_e2e.py` | looprs-pdl.6: the scrollback store in a real pty. **23/23.** The four scroll keys have to arrive as themselves off the wire (`ESC[5~`/`ESC[6~`/`ESC[H`/`ESC[F` through crossterm with a real bash in front), the "N new" pill has to be legible on the band's bottom row rather than merely present in a buffer, and a band the user stopped reading must not move by one row when two arrivals land on the tail behind it. Bash mode is the driver: no fake `pi`, no board, output line at a time. Reports, rather than asserts, that an **idle** resize costs the app zero bytes over 2s — identical on the pre-pdl.6 binary at `195e3c0`, so a window drag on a quiet session leaves the old frame; that is `looprs-pdl.15`. **Settled by pdl.15:** the zero was this harness, not the app — `SIGWINCH` goes to the foreground process group of the terminal's session and a `Popen(stdin=slave)` pty is never the child's controlling terminal, so the signal never arrived. The run loop now reads the window from the `ioctl` instead of waiting for the signal, so the measurement this spike prints is non-zero; `spikes/resize_e2e.py` is where that claim lives. The content-anchored half of the resize property is *not* claimed here (no reflowing emulator); it lives in `a_resize_keeps_the_line_the_user_was_looking_at_on_screen` in `src/main.rs`. |
| `spikes/mouse_scroll_e2e.py` | looprs-pdl.8: the wheel, the trackpad burst and the mode trio, on a real wire. **45/45.** The three mouse modes have to be on **by default** and **once each** (counted off the capture at entry and at the leave), `LOOPRS_MODES=-mouse` has to decline the whole trio and leave a working app behind, one injected `ESC[<64;x;yM` notch has to move the painted transcript exactly `WHEEL_ROWS_PER_STEP` rows and the same back, and a **timed** burst (40 reports, ~299 ms of wall clock) has to land the *rate* — 15 rows at ~50.2 rows/s against the 50 rows/s the constants say — with the bound recomputed from the duration measured, so retuning the constants cannot quietly outrun the check. Chrome is priced too: wheel over the status row and the input box moves no row, and middle/right-click scroll nothing while **saying** so in the log (the "not silently swallowed" half of the rule, middle-click paste being looprs-pdl.11's). Reports rather than asserts: bytes emitted with the mouse still over 2s = **0**, and 0 again after a gesture drains. **What it cannot measure is a real finger** — `looprs-pdl.2` #4b is still "no finger on this path", and a synthetic cadence bounds the app without describing the input. The gap is left instrumented rather than closed: `WheelCadence` logs each gesture's reports/duration/rows at `debug` — and the default run level is `info` now — so `grep 'wheel gesture closed' "$LOG"` (the resolved log file; see `docs/guide/operator.md`) after a few real flicks taken under `RUST_LOG=debug` is the measurement, and `WHEEL_ROWS_PER_STEP` / `WHEEL_STEP_INTERVAL` in `src/state/wheel.rs` are the two knobs. It sets `LOOPRS_LOG_DIR` to its own scratch dir for the group that reads the log back: that check used to look in the cwd the binary never wrote to. |
| `spikes/resize_e2e.py` | looprs-pdl.15: the window drag on a session that is doing nothing. **53/53**, six groups, run against **two harnesses on purpose** — `bare` (`Popen(stdin=slave)`, the shape of every spike here, and the one that filed this ticket) and `attached` (`setsid()` + `TIOCSCTTY`, what a real terminal window does with the program inside it). The reason for two is that `SIGWINCH` goes to the **foreground process group of the terminal's session**, not to whatever is drawing on the tty: the bare harness never delivers it, so the pre-ticket app there sits on the old frame indefinitely, while in the attached harness the *same pre-ticket binary* repaints fine — the evidence that `App::set_window`, the `Event::Resize` arm and the `dirty` gate were never the bug. What the fix claims is that the app stopped depending on the signal: the size `ioctl` is the authority, and `untouched` repaints **7.6 ms** after the `TIOCSWINSZ` with no signal in existence. Also measured: a **re-wrap** off the wire (prose that fits one row at 120 comes back as two at 60), a six-size **burst** ending at the *last* size rather than one from the middle (`SIGWINCH` coalesces, so the window has to be read rather than accumulated), a drag taken while the transcript is **scrolled up**, and the part no other ticket could reach — a drag while a **full-screen child holds the screen**, where the child reports its own `stty size` on the way out and says the new window, because a child on a second pty is resized by nothing else. Controls: `--control` against the pre-pdl.15 binary, where **0 of 5 repaint claims survive**, and the two needles that control caught leaking ("`End` reaches the tail", "repaints after the child hands back") which were true pre-fix for reasons unrelated to seeing the resize. **Not claimed:** the anchored-line property (this grid does not reflow; that lives in `a_resize_keeps_the_line_the_user_was_looking_at_on_screen`), and any re-wrap of raw shell output — `MessageKind::Bash` is ended by the child, ADR-0001 rule 1 / ADR-0005 forbid re-wrapping it, so a long shell line is cut at the right edge before and after alike. |
| `spikes/tmux_keyboard_e2e.py` | looprs-pdl.13: the copy chords and the page keys driven **inside tmux**, the first spike here that does not own the pty. Seven groups, **37/37**: `Ctrl-S` (0x13) has to arrive as a key event and not be swallowed as XOFF (the arming hint painting is the proof); `Home`/`End`/`PageUp`/`PageDown` over a 200-row Bash transcript, including the pin property that a `PageDown` taken at the tail does not move; `Ctrl-S o` in Bash and `Ctrl-S a` in Pi through the real sink; `Ctrl-S t` read back from disk with the toast's own character count; `Ctrl-C` interrupting a `sleep 30` in ~0.04 s with no `Copied` toast anywhere; a full-screen vim that must still repaint after our `Ctrl-S` (the check that matters — a child stopped with XOFF needs `Ctrl-Q` to resume and `Ctrl-Q` is looprs's *quit*, so that failure has no recovery from inside the app); `Esc` lowering the chord and the next letter typing; `#{alternate_on}` through the pane and a clean `Ctrl-Q`. Two findings came out of running it rather than out of the unit tests: the shell's trailing `\r` made `Ctrl-S o` refuse a copy of a screen full of output (fixed by reading the command *block*, not the last Bash entry), and macOS's `$TMPDIR` — 49 columns before the file name — clipped the dump toast into naming nothing (fixed by the `~/.cache/looprs` default and the `~` fold). `pi` and `bd` are fakes the script writes, and that is load-bearing: looprs boots into Beeds and the beads loop runs `bd update <id> --claim` before a Tab can reach it, so an unplanned run moves the real board. What tmux cannot test is a real drag — `send-keys` injects SGR reports into the pane's input, which proves our parser and not the multiplexer's forwarding. |
| `spikes/fake_pi_slow.py` | The `pi` that spike needs: one assistant message held **open** while a paragraph dribbles out. `tests/fixtures/fake_pi_chat.py` emits `text_delta` and `message_end` back to back, so its reply is flushed before a frame can be drawn over it and there is no live tail to measure. |
| `spikes/mouse_clipboard_e2e.py` | looprs-pdl.2: the seven claims under the selection/clipboard tickets, measured rather than assumed. **Bare-pty groups** (no terminal needed): SGR 1006 mouse injection decoded by `crossterm` inside the pty (13/13 report types, coordinate offset a constant −1, ctrl/alt/shift bit translation measured), burst capacity (128 reports in one write → 128 events in 0.30 ms, none merged), shift-drag on the wire, the real binary's mode set/restore symmetry against a `stty` baseline, and vim's nested mouse modes. **Emulator leg** (`--in-terminal`, run inside a real window): DEC mode queries answered by the emulator, the OSC 52 size ladder read back off the real clipboard with a latency proxy for permission prompts, chunked copies, the read-back query, and a trackpad/shift-drag recorder. **SSH leg** (`--ssh`): a container `sshd` with a Linux build of the probe, mouse injection through the hop, and OSC 52 issued remotely and landed in the local emulator's clipboard. Controls: `--decode-off` (bytes without a parser), malformed SGR without the `<`, a +5 coordinate differential, `kill -9` residue for the mode detector, and the same payload under OSC 51. **24/24** bare-pty, **15/15** control, **5/5** ssh; the legs and the matrix of what each terminal did are in [`results/terminal-matrix.md`](results/terminal-matrix.md). |
| `examples/spike_mouse_probe.rs` | The thing inside the pty for the spike above: reads events with the same `crossterm` the app uses and prints one timestamped line per event (`EV t=… mouse=drag btn=left x=13 y=7 mods=1`), plus the OSC 52 writer, the read-back query, a raw byte counter for the `--decode-off` control, and a `mode-holder` that leaves its modes switched on for the next process. Its mode bytes are copied from `src/teardown.rs` rather than imported, so it can disagree with the app. |
| `examples/spike_clipboard_cost.rs` | looprs-pdl.1: the two costs ADR-0004 Q2 had not priced, measured. `crossterm`'s own OSC 52 writer buffered (encode + framing only: **0.002 ms** at 128 B, **6.1 ms** at 1 MiB, **1.34×** wire) against a native helper spawned (`pbcopy` write **~9 ms**, `pbpaste` read-back **~10 ms**, both size-flat) — the number behind "native when it is verifiable, OSC 52 when it is not". Second half: the same corpus in **characters / cells / bytes** with the renderer's own `unicode-width` (`漢字` = 2/4/6, decomposed `é` = 2/1/3, the ZWJ family = 5/2/18, per-character widths summed over that family = 6 against the family's own 2), which is what ADR-0004 R19's "N is characters" rests on. Needs no terminal, no network, no app. Run: `cargo run -q --example spike_clipboard_cost \| tee spikes/results/clipboard-cost.log`. |
| `spikes/log_budget_e2e.py` | looprs-00u.13: where the log goes, how loud it is and how big it gets, on the shipped binary. **48/48**, five runs. (a) 60 s at default settings: **2,880 bytes**, **zero** `DEBUG` lines, and inside the ceiling the run's own first line printed. (b) the same 60 s at `RUST_LOG=debug` — the level the old code defaulted to — through the same fixture: **10,803 bytes**, **3.8×**. (c) four runs up the destination ladder (`LOOPRS_LOG_DIR` → `$XDG_STATE_HOME` → `~/.local/state` → the system temp dir), each asserting the app *named* the path it wrote and that nothing appeared at a rung a higher one shadowed. (d) a forced-volume run — debug level, a `bd` failing every poll, 16 KiB cap, `keep=3` — rotated four times, stayed inside `active + keep`, no file more than one line over the cap, `looprs.log` still the active name when it finished. (e) a deliberately bad knob set (`RUST_LOG="=debug"`, `LOOPRS_LOG_MAX_BYTES=lots`, `LOOPRS_LOG_KEEP=many`), which answered at `WARN` and ran on the defaults it fell back to. Then the `docs/guide/operator.md` grep index is re-run over every blob and prints the **level each row's hits arrived at**, which is the column that keeps the page honest about what a default-level run cannot show you. Costs no model call; every run lives in its own `mkdtemp()` including `HOME`. |
| `spikes/results/live-preview-cost.log` | looprs-00u.14: the live-tail preview priced per frame — µs **and** bytes against live-block length, before and after, dev and release. Run from `src/measure.rs` (`the_live_tail_preview_measured_per_frame_against_the_block_it_renders`) over 527 real answer/thinking entries from 12 real `pi` passes, streamed 128 B per simulated frame through the real `SessionView` in the frame's real order (flush, then preview). The headline is that the ticket's premise was half wrong (the live slice is the open paragraph, max 3.0 KiB, not the answer, max 56 KiB) and the cost that was real is churn: 18.8 KB per call pooled. Also caught, in the `flush` context column: syntect's first-highlight-per-context stall, 296 ms → 78 ms once more languages were warmed — `looprs-00u.16`'s finding, photographed by someone else's harness. Run it with the command at the head of the log. |
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
python3 spikes/scrollback_e2e.py | tee spikes/results/scrollback-e2e.log
python3 spikes/mouse_scroll_e2e.py | tee spikes/results/mouse-scroll-e2e.log
python3 spikes/resize_e2e.py | tee spikes/results/resize-e2e.log
# …and the pre-looprs-pdl.15 control for it:
#   git worktree add --detach /tmp/looprs-pdl15-ctl HEAD
#   (cd /tmp/looprs-pdl15-ctl/pi-beads/looprs && cargo build --target-dir /tmp/pdl15-target)
#   LOOPRS_BIN=/tmp/pdl15-target/debug/looprs python3 spikes/resize_e2e.py --control \
#     | tee spikes/results/resize-e2e-control.log

# …and the mouse/clipboard spike. The bare-pty groups need only the example binary;
# the emulator leg has to run inside a real terminal window, and the ssh leg needs
# docker (it builds its own sshd image and caches it).
cargo build --examples
python3 spikes/mouse_clipboard_e2e.py           | tee spikes/results/mouse-clipboard-e2e.log
python3 spikes/mouse_clipboard_e2e.py --control | tee spikes/results/mouse-clipboard-e2e-control.log
python3 spikes/mouse_clipboard_e2e.py ssh       | tee spikes/results/mouse-clipboard-ssh.log
python3 spikes/mouse_clipboard_e2e.py --in-terminal --record-flick
PDL2_LAUNCH='open -a Terminal {file}' python3 spikes/mouse_clipboard_e2e.py --launch

# …and the ADR-0004 transport-cost + copy-units measurement (looprs-pdl.1). No
# terminal, no network, no app: it times crossterm's OSC 52 writer against a
# spawned pbcopy, and prints the corpus in characters / cells / bytes.
cargo run -q --example spike_clipboard_cost | tee spikes/results/clipboard-cost.log

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
  switches on, **added to** the default set. Since looprs-pdl.4 that default is
  `raw,alt_screen,cursor_hidden` — the app is on the alternate screen whether or not anyone
  asks — so what the knob adds now is the mouse report/drag/SGR set and bracketed paste,
  which is what `shutdown_e2e.py` holds and re-counts under `LOOPRS_MODES=all`. An unknown
  name stops the app with a message rather than quietly switching on nothing; see
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
