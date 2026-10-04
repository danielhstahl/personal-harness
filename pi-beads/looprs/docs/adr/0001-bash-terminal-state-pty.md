# ADR-0001: How the Bash terminal state gets a shell — pipes (`bash -i`) vs a real PTY

- **ID:** looprs-jri
- **Status:** Accepted — 2026-10-03
- **Epic:** looprs-fkc (three terminal states: Beads loop, Pi session, plain Bash)
- **Blocks:** looprs-553 (Bash terminal state: persistent shell, streamed stdout/stderr, exit codes)
- **Related:** looprs-037 (session router / mode switch), looprs-5g7 (Esc/Cancel), looprs-ecr (clean shutdown), looprs-afw (viewport shape)

## Decision

**Option B — a real pseudo-terminal via [`portable-pty`](https://crates.io/crates/portable-pty) `0.9.0`.**
Bash mode owns one long-lived `bash -i` running on its own pty; looprs reads the master end,
writes to it, and forwards resize to it.

Option A (`bash -i` with piped stdin/stdout/stderr) was rejected: it is cheaper to write but it
fails the things that make the mode worth having — interrupting a running command, and running
anything that calls `isatty()`.

Crate/version detail: `portable-pty = "0.9.0"` is currently a **dev-dependency** (the spike needs
it; Bash mode does not exist yet). looprs-553 moves it to `[dependencies]` when it lands. It is
sync-only — the reader is a dedicated OS thread pushing `Vec<u8>` into the existing
`tokio::sync::mpsc` channel pattern, exactly like `PiRpc`'s reader task
(`src/services/pi.rs`).

## Spike

Every claim below was executed, not asserted. `examples/spike_bash.rs` runs the *same* probe
script (`spikes/probes.sh`) against a live bash; the only per-option difference is the
~20-line `spawn_pipes()` / `spawn_pty()` function:

```sh
script -q /dev/null cargo run -q --example spike_bash -- pipes | tee spikes/results/pipes.log
script -q /dev/null cargo run -q --example spike_bash -- pty   | tee spikes/results/pty.log
# interleaving, repeated under contention (see spikes/README.md)
LOOPRS_SPIKE_PROBES=spikes/interleave_only.sh script -q /dev/null cargo run -q --example spike_bash -- pipes
```

Environment: macOS 26.6.2 (Darwin 25.6.0) arm64, `/bin/bash` 3.2.57 (the system bash — note
it, not a 5.x), vim 9.1, rustc 1.100.0-nightly. The harness runs under `script -q /dev/null`
so the harness itself owns a real terminal; without that, `/dev/tty` is unreachable and the
escape-hatch probe is meaningless.

| Probe | Option A — pipes | Option B — portable-pty 0.9 |
|---|---|---|
| `[ -t 0/1/2 ]` in the child | piped / piped / piped | tty / tty / tty |
| Child's controlling tty vs the harness's | **same device** (`ttys003` == `ttys003`) | different (`ttys004` vs `ttys003`) |
| `/dev/tty` openable by the child | **yes** → a `sudo` prompt lands on the real screen, outside our renderer and our input path | yes, but it *is* our pty — we see the bytes and can route them |
| `cd /tmp`, then `pwd` in a later write | `/tmp` — persists | `/tmp` — persists |
| `export` / `alias` across separate writes | persists | persists |
| `jobs` after `sleep 3 &` | 1 visible, but bash printed **"no job control in this shell"** → no Ctrl-Z / `fg` / `bg` / stop | 1 visible, job control live |
| stdout/stderr order for interleaved writes | **not preserved.** 9 of 10 concurrent runs lost it; one run delivered all 12 stdout lines before all 12 stderr lines (`spikes/results/pipes-interleave-race.log`) | preserved, 10/10 (`spikes/results/pty-interleave-10runs.txt`) |
| `stty size` | `stty: stdin isn't a terminal`; `COLUMNS=80 LINES=24` frozen from startup env, nothing to resize | `30 100` at spawn → `40 120` after `master.resize(PtySize{rows:40, cols:120})` |
| Ctrl-C (`0x03`) sent 1.5 s into `sleep 6` | **ignored** — the next probe line arrived 6.01 s later, i.e. `sleep` finished by itself; the byte sat unread in the pipe | **interrupted at 1.81 s**, with `^C` echoed by the line discipline |
| `vim -u NONE -i NONE -n` | Starts with `Vim: Warning: Output is not to a terminal` + `Input is not from a terminal`. Scripting the keystrokes still got a file written (exit 0), but that is the *harness* typing blind, not a usable terminal: there is no correct size (`stty` fails), the alt-screen/cursor addressing is aimed at a terminal that does not exist, and nothing downstream of the pipe can show it to a human. | Fully functional: real 30x100 pty, alt screen, typed text rendered, `:wq!` wrote the file, exit 0, and resize took effect. |
| `sudo -n true` | exit 1, `sudo: a password is required` | exit 1, `sudo: a password is required` |
| `echo pw \| sudo -S true` | exit 1, `Password:Sorry, try again.` (password must arrive on a pipe) | same |
| Startup noise | `bash: no job control in this shell` + the macOS "default interactive shell is now zsh" advisory | zsh advisory only |

Reproduce any of it; the raw logs are committed under `spikes/results/`.

## The four questions, answered

### 1. Which option, and the crate + version?

Option B. `portable-pty = "0.9.0"`, `native_pty_system()` → `openpty(PtySize)` →
`slave.spawn_command(CommandBuilder)` → master `try_clone_reader()` / `take_writer()`.

Why not A, in one line each:

- Ctrl-C does not reach the running command. A Bash pane where `sleep 30` cannot be stopped is
  not a shell; the spike measured the difference (1.8 s vs 6.0 s).
- Every program that calls `isatty()` changes behaviour, and we control none of it. Measured here:
  `vim` warns and can only be driven blind, `stty` fails outright, and `COLUMNS`/`LINES` freeze at
  whatever the process started with. `less`, `htop`, password prompts and colour auto-detection
  are the same class of thing — none of them is our code, and none of them behaves.
- Job control is gone (`no job control in this shell`), so Ctrl-Z / `fg` / `bg` / `kill %1`
  semantics differ from a real shell.
- Two pipes must be merged by two reader threads; the merged order is a scheduling race, and it
  was observed to scramble under load. A pty has one stream, so stdout/stderr ordering is the
  *program's* order for free — which is exactly what looprs-553 requires ("interleaved
  correctly").
- In the pipes case the child's controlling terminal is still looprs' own terminal. `sudo`,
  `ssh`, `passwd`, git's credential helper, or a pager's "press any key" read/write `/dev/tty`
  directly: the prompt appears *through* our TUI and the keystrokes come back from the real
  keyboard, bypassing our input handling entirely. With a pty that hazard is structurally gone
  (`child_tty != harness_tty`).
- There is nothing to resize: `stty size` fails, so `COLUMNS`/`LINES` are wrong and full-screen
  programs lay out for a terminal that does not exist.

Cost of B, stated honestly: one more crate (with a transitive `nix 0.28` that currently trips a
future-incompatibility lint — see Risks), a blocking reader that needs its own thread, and a
decision about what to do with a raw ANSI stream (question 2).

### 2. Raw passthrough, or normalize through the transcript?

**Neither "re-emit through ratatui" nor "blind tee". Decision: raw passthrough to the real
terminal while a Bash command is running; the transcript stores a *literal* copy for scrollback
only.**

Concretely, the rules looprs-553 implements:

1. **No markdown, no theme, no re-wrap.** Bash output never enters the `utils::md` render path
   and is never re-flowed. The child already wrapped its own lines for a known width; re-wrapping
   would corrupt tables, progress bars, and box-drawing output. (This matches the explicit
   requirement in looprs-553.)
2. **Passthrough, not interpretation.** While a foreground Bash command is running, looprs
   copies the pty master bytes verbatim to the real terminal and **does not draw its own live
   viewport** (`AppState::BashRunning` suppresses `term.draw`). The child believes it owns the
   screen, and during that window it does. Our spinner/live-preview/input box is not repainted
   mid-stream; the user's typed line is echoed by the child, not by us.
3. **Cursor movement / alternate screen / colours are the child's business.** We do not emulate a
   VT100 and we do not try to re-drive the child's cursor moves through ratatui — that means
   writing a terminal emulator, which is a different (much larger) project. Because the child is
   attached to a real pty sized to the real window, alt-screen programs (`vim`, `less`) work
   natively and restore the screen on exit; the spike confirms this end-to-end.
4. **Re-anchoring the seam.** When the command finishes (or Bash mode is left), looprs flushes,
   then re-establishes its inline viewport below the child's output and repaints the live region.
   ratatui's cell diff is stale after raw passthrough, so the implementer must force a full
   repaint of the viewport rather than trusting the diff. `Terminal::clear()` in inline mode also
   emits clears at the viewport origin, which is not what we want mid-scrollback; if no public
   API resets the diff state in ratatui 0.30.2, recreating the `Terminal` with the same
   `Viewport::Inline` is the known-safe fallback. *This is the one genuinely fiddly part of
   Option B and must be proven in looprs-553's acceptance run.*
5. **The transcript copy** is escape-stripped plain text (keep the bytes, drop the presentation)
   used for Bash-mode scrollback and for anything that later greps a session. It is not the
   display path.
6. **Pty size = the real terminal size.** The existing `Event::Resize` arm in `main.rs` (which
   today only calls `term.resize`) must also call `master.resize(PtySize { rows, cols, .. })`.
   We do not give the child a virtual 80x24 while showing it in a different box.

Rejected alternative — *normalize by parsing the child's ANSI into a virtual screen and
re-emitting through ratatui*: gives pixel-perfect mixing of our UI with the child's output, but
requires a maintained VT100/xterm emulator (alt-screen, scroll regions, cursor addressing,
DEC-private modes, wide chars). Cost vastly exceeds the feature.

Rejected alternative — *always tee raw bytes into the transcript and render through the live
viewport*: the live viewport is a fixed 10-row region (`VIEWPORT_H` in main.rs, see looprs-afw).
Full-screen programs writing absolute cursor addresses into a 10-row inline region produce
garbage. Only the passthrough rule makes `vim` usable.

### 3. Ctrl-C / Ctrl-D semantics in Bash mode

| Chord | Bash mode | Beads / Pi mode | Rationale |
|---|---|---|---|
| **Ctrl-C** | **Always forwarded** — write `0x03` to the pty master. The line discipline sends SIGINT to the child's foreground process group. The command dies; the shell survives. **Ctrl-C never quits looprs in Bash mode.** | Cancel the current work: `PiRpc::abort()` (aligns with looprs-5g7). | This is what a terminal does. The spike proves the forwarded byte works (1.81 s vs the 6.01 s no-op). Making Ctrl-C quit looprs would make `vim` and `sleep` uncontrollable. |
| **Ctrl-D** | Forwarded as a literal `0x04`. At an idle prompt that is EOF → the shell exits → looprs notices stdout EOF and **restarts it with a visible `shell exited (code N), restarted` notice** (looprs-553 requirement). Mid-line it is just EOF-on-input. | Forwarded to the input box as delete-char, or ignored; never quits. | Quitting by typing a control character into a shell pane is surprising and unrecoverable-looking. `exit` is the explicit way. |
| **Ctrl-Q** | **Quit looprs.** Intercepted by us before forwarding, in all three modes. | Same. | We own the outer terminal, so an unused control char is a safe global chord. In Bash mode it quits without touching the shell (the child is killed on shutdown, see looprs-ecr). Esc remains "cancel" everywhere. |
| **Ctrl-Z** | Forwarded to the pty → the child gets SIGTSTP; with a real tty, job control actually works. | Ignored. | Free with Option B; impossible with Option A. |

`main.rs`'s current raw-mode handler, which quits on Ctrl-C unconditionally, must therefore be
changed to be mode-aware. That is part of looprs-5g7's scope, called out here so it is not lost.

### 4. Can Bash mode reuse pi's RPC `bash` command instead of our own subprocess?

**No — measured, not assumed.** `spikes/pi_bash_rpc.py` drives a real `pi --mode rpc` session
and sends dependent commands:

```
PROBE pwd_first  ='/Users/.../looprs'            exit=0
PROBE cd         =''                             exit=0
PROBE pwd_second ='/Users/.../looprs'   <-- cd did NOT persist
PROBE env_first  ='LOOPRS_SPIKE=unset'
PROBE env_set    ='export LOOPRS_SPIKE=42'
PROBE env_second ='LOOPRS_SPIKE=unset'  <-- export did NOT persist
PROBE bg         ='started'
PROBE bg_visible ='0'                   <-- background jobs invisible to the next command
```

Reasons to stay off that path:

1. **Single-shot.** Each `{"type":"bash","command":...}` is its own process. cwd, exported
   variables, shell functions, aliases, `pushd` stacks, and background jobs do not survive
   between commands. The epic's acceptance criterion ("`cd /tmp` then `pwd` in a later command
   → `/tmp`") is unsatisfiable with it.
2. **Its output is aimed at the model, not at a human.** Per pi's `docs/rpc-commands.md#bash`,
   the result becomes a `BashExecutionMessage` that is rewritten into a *user* message ("Ran
   \`cmd\` …") on the **next** prompt. A plain Bash pane must not silently write into an agent's
   context. (`excludeFromContext` exists but then we are fighting the API to use it as a dumb
   shell.)
3. **No tty either.** It inherits the same `isatty()` problems as Option A, so it does not even
   buy us vim/sudo/job-control on the side.
4. **Cancellation is a different, global command** (`abort_bash`, one at a time), not something
   we can scope per-pane or map to Ctrl-C → SIGINT.
5. Truncation with `fullOutputPath` is pi's policy for model context, not our display policy.

pi's RPC remains the right tool for **agent-driven** work (Beads planner/worker passes, Pi chat).
Bash mode gets its own subprocess. Both patterns already coexist in this repo: `PiRpc` owns its
child with `kill_on_drop`; BashSession will do the same for the pty child.

## Consequences

Good:

- Bash mode behaves like a real shell: Ctrl-C interrupts, `vim` works (measured — `less` and
  friends ride on the same `isatty()` property rather than being separately proven here), colours
  and progress bars self-detect correctly, cwd/env/aliases persist, `stty` works, resize
  propagates.
- stdout/stderr ordering comes for free; looprs-553's "interleaved correctly" needs no locking
  or merging logic.
- No `/dev/tty` escape: nothing can prompt the user behind our back, so the sudo/ssh class of
  bug cannot be filed against us.
- The same pty plumbing gives Beads/Pi modes a template for "own a subprocess, stream its bytes".

Costs / follow-ups:

- **Dependency:** `portable-pty 0.9.0` pulls `nix 0.28`, which trips a
  future-incompatibility lint (`trailing semicolon in macro used in expression position`, in
  nix's `build.rs`). `portable-pty` pins `nix = "0.28"`, so `cargo update` cannot move it.
  Mitigations, in order of preference: watch for a `portable-pty` release that bumps nix; else
  `[patch.crates-io]`; else swap to `xpty` (an async-ready fork of portable-pty) or a ~50-line
  `libc::openpty` wrapper. **Action for looprs-553: keep the epic's "warning-free build" gate
  honest about this — it is a dependency warning, not our code.**
- **Blocking reader:** portable-pty exposes blocking `Read`, so Bash mode needs a dedicated OS
  thread (or `spawn_blocking`) feeding the existing mpsc pattern. Do not block the tokio runtime.
- **Shutdown discipline:** the pty child must die with looprs. Follow looprs-ecr: kill on drop,
  reap, close the master so the reader thread sees EOF, and make terminal-restore idempotent.
- **Terminal-state seam:** raw passthrough then repaint requires forcing ratatui's diff (see
  rule 4 above). Untested seams here are the most likely source of "screen is garbled after
  exit-ing vim" bug reports.
- **`bash` 3.2 on macOS:** the system shell is from 2007 and prints the "default interactive
  shell is now zsh" advisory at startup. Bash mode should let the shell print it once (it is the
  user's machine), and must not assume bash 4+/5 syntax (`${var,,}`, arrays-of-associative-arrays,
  `**` globbing) in anything we generate ourselves.
- **Security note:** Bash mode is an unfiltered shell on the user's account. It inherits looprs'
  env, including anything sensitive in it. Do not echo the child's output through a path that
  also logs to `looprs.log` at debug level without thinking about secrets (`sudo -S`, tokens in
  argv) — see the logging call in `init_logging()`.

## Alternative options considered

- **Option A — pipes (`bash -i`, piped stdin/stdout/stderr).** Cheapest: no crate, trivial
  line-buffered push into the flusher. Rejected on the five points above. If it is ever
  revisited, the README must carry the limitations list (no Ctrl-C, no vim/less/htop, no job
  control, no resize, scrambled stderr interleaving, and sudo/ssh prompts escaping the TUI) so
  nobody files them as bugs.
- **Option C — `libc::openpty` + `fork` by hand.** No `nix` dependency, but unsafe,
  platform-specific, and reimplements what `portable-pty` already got right. Kept as the
  fallback if the nix lint becomes a hard blocker.
- **Option D — pi's RPC `bash`.** Rejected in question 4.
- **Option E — run the shell inside tmux and scrape it.** Solves resize/passthrough but adds a
  harder external dependency and a worse UX than owning the pty.

## Amendment 2 — as built (`looprs-553`), including four things the spike did not know

`BashSession` is in `src/session/bash.rs`; `Msg::BashOutput` carries raw chunks to
`SessionView::push_bash`, and the transcript renders them with no markdown pass. The
decision above holds. Four things found while building it change the shape of the code,
and are recorded here rather than left in comments.

**1. `--rcfile` must come before `-i` on macOS.** The sketch above spawns
`[program, "-i", "--rcfile", rc]`. macOS's `/bin/bash` 3.2 parses `-i` as the end of
option parsing and answers `--: invalid option`; the shell never starts, and the failure
looks like a silent pty, not an argument error. `bash --rcfile <rc> -i` is accepted by
3.2 and by 5.x, so that is the order used. Anyone re-deriving this from the earlier
text in this file will get it wrong; that is why the order is called out.

**2. `$SHELL` is a lie for Bash mode.** This machine has `SHELL=/bin/zsh`. Taking
`$SHELL` as "the bash binary" produces a shell that accepts the spawn, prints a prompt,
and has no idea what our `PROMPT_COMMAND` marker means — every command then looks like it
never returned. Resolution order is: `LOOPRS_SHELL_BIN` verbatim (test override), then
`$SHELL` *only if it looks like bash*, then a search of the usual paths, then
`/bin/bash`. Configuring a `BashSession` with a non-bash program logs a warning rather
than failing, because the user may have a bash-alike we did not recognize.

**3. Readiness is a queue, not a gate.** The first prompt marker arrives *before bash has
read anything*, so the answer to a command sent cold is ambiguous — bash's own first
prompt can be read as that command's `exit 0`. Input arriving early is queued and flushed
when the first marker lands, and each command written is pushed onto an `outstanding`
queue so that each exit marker pops the command it belongs to. That is what makes
"which exit code goes with which line" correct for typed-ahead input instead of merely
plausible.

**4. The reader thread holds a sender, so dropping the session must ask to shut down.**
The `Shell` is owned by the session task, and that task ends when its command mailbox
closes — but the pty reader thread holds a clone of the mailbox sender for its whole
life. So the mailbox never closes while the reader runs: the task never exits, the `Shell`
is never dropped, the child is never killed, the child never exits, and the reader never
sees the EOF that would end it. A cycle with a live child in the middle, which is exactly
how the app leaves a bash behind after "quitting" (`looprs-ecr`'s unkillable bash).
`impl Drop for BashSession` sends `Shutdown` explicitly: the shell dies, the reader hits
EOF, the thread ends, every sender dies. Any session type with a child behind a thread
boundary has the same trap, and the test that catches it is "no orphaned child after
Ctrl-Q" — invisible in the UI, cheap in a driver.

**5. Resize has to survive the session not existing yet.** `main.rs` broadcasts the size
at startup, but Bash spawns lazily, so that broadcast lands in an empty map. Measured
before the fix: `stty size` reporting `24 80` inside a 40x132 pty, which wraps every
program's output at the wrong width forever. The Router now records `last_size` and applies
it to a session at the moment it is created, as well as forwarding every resize to the
sessions that already exist — and still never spawns one on a resize.

**6. A restart is announced by whoever performs it, which is the Router.** `exit` inside
the shell kills the child but not the mode. The next command finds the session `Dead` and
replaces the generation; the replacement does not know it is a replacement, and the old
one is already off the air. Without a word the user gets the dying session's last line
and then a stranger's banner — a crash, not a restart — so `ensure` emits
"`Bash#1` was gone; started `Bash#2` in its place" into the new session's stream.

**7. Ctrl-C is the shell's; Ctrl-Q is ours.** In Bash mode Ctrl-C routes as `Cancel` and
the session writes `0x03` to the master (measured: 0.06 s to `interrupted (exit 130)` on
a `sleep 30`, app alive, shell reused). That chord is gone from the app, so Ctrl-Q quits
in every mode.

## Known gap: full-screen programs do not work yet (measured, not guessed)

Rule 1 says passthrough, and the acceptance line further up this file claims "vim works
(measured)". **That was measured on a raw pty spike, where the child's bytes went to a
terminal. It is not true of the shipped path, and it is not true now.**

`spikes/vim_fullscreen.py` (output in `spikes/results/vim-fullscreen.log`) drives the
real TUI in a real PTY and types into vim. What happens:

| observation | result |
| --- | --- |
| the shell is on a real tty (`stty size` = the window) | works |
| vim starts, no "not to a terminal" warning | works |
| vim's screen reaches the user | **nothing**: no `--INSERT--`, no `~` filler lines, no file written |
| the app survives | yes; Ctrl-Q quits it normally |

The cause is measurable, and it is our path, not vim's. Instrumenting the reader showed
vim's full-screen paint arriving in ~1 KB chunks containing **zero linefeeds** (line-oriented
output always ends in CRLF). The transcript flushes line by line: a partial line stays
live until a `\n` arrives, and for a cursor-addressed program none ever does. So the pane
sits frozen on whatever was there before, while the shell streams a screen's worth of bytes
we cannot show.

Two consequences to carry forward:

* ADR-0001 Q8's "full-screen passthrough, screen-buffer path" is **not** implemented.
  Until it is, Bash mode is a line-oriented shell transcript: superb for `git log`, `ls`,
  `cd`, `grep`, `python -c`; not a terminal for `vim`, `less`, `htop`.
* Bash mode maps Esc to `Cancel`, which is `0x03`. For a line command that is right
  (ticket: "Esc/Ctrl-C interrupts"). For vim, Esc is the key that leaves insert mode, and
  we send SIGINT instead. Raw keystroke passthrough and the Esc mapping are both part of
  the same follow-on; neither can be fixed without the other.

What is *not* the cause, ruled out by measurement: a big burst of ordinary output is fine.
`seq 1 400` flushes, stays responsive, and the app quits clean. The problem is the absence
of newlines, not the volume.
