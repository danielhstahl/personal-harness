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
  **Where that reap gets taken is not a free choice (looprs-2ck).** The task that owns the child
  is the same task that drains the reader thread's byte lane, so a blocking `Child::wait` taken
  there can hold all three at once — the task on the child, the reader on the lane, and the
  child on the tty flush inside `exit(2)` — and nothing in the process can move again. The rule
  is *never reap alone*: keep draining the lane while polling, bound every phase, and hand over
  anything still alive to a detached reaper thread that owns the last blocking wait. See
  "The shutdown hang the `--skip` used to shadow" in [testing.md](../testing.md).
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

## Amendment 3 — the screen-buffer path, as built (`looprs-4hv`)

The section this one replaces was titled "Known gap: full-screen programs do not work
yet", and the measurement in it was right: vim's paint arrived in ~1 KB chunks with no
linefeeds in them, the line-delimited transcript flush showed none of it, and the pane
sat frozen on the previous frame. It is closed. What got built, and what was measured
building it.

**The shape of it is rule 2, not rule 5.** Rule 2 says: while a foreground Bash command
owns the screen, looprs copies the child's bytes verbatim and stops drawing. That is
the whole mechanism — no emulator was written, because the child's own cursor addressing
already targets the real terminal at the real size (rule 6). Rule 5's "literal copy for
scrollback" deliberately does **not** apply to a held screen: a real terminal discards
the alternate screen when the program leaves it, and rendering a second copy of the same
paint above the viewport is how a full-screen program ends up smeared through scrollback.
The two rules read side by side look like a contradiction; the resolution is that one is
the display path and the other is not, and the display path wins while the child holds
the screen.

**Four pieces** (all in `src/screen.rs`, plus the gate in `main.rs`):

1. `ScreenWatch` reads the shell's byte stream and says who owns the screen. Two
   signals: the explicit one (`ESC[?1049h` / `?1047h` / `?47h`, and the `l` that
   leaves), and the inferred one — cursor addressing (`CUU`, `CUP`, `ED` 2/3) in a
   chunk with **no linefeeds**, which is the shape the earlier measurement recorded
   for programs that never touch the alt screen. Colour never triggers it; neither
   does a private mode set like `?25l` or `?2004h`, and a chunk that breaks lines is
   left to the transcript.
2. The **ordering rule**, which is not a detail. A `Takeover` is reported *before*
   the bytes that switch screens, a `Release` *after* the bytes that switch back.
   Invert either one and the failure is invisible: announce late and the `ESC[?1049h`
   goes into the transcript instead of the terminal, so vim paints onto a screen
   nobody switched; announce early and the leave byte is held back and the terminal
   never comes out of the alt screen. The watcher holds an undecided escape-sequence
   tail between reads rather than emit it, because half a switch in the transcript is
   worse than no switch.
3. `key_bytes` re-serialises a parsed `KeyEvent` back into the bytes the terminal
   sent. This is the half the ticket said could not be separated from the `Esc`
   mapping, and it is right: with the screen handed over, `Esc` has to be `0x1b`
   (vim: leave insert mode) rather than the `0x03` a line command needs it to be.
   The rule is *who owns the screen owns the keyboard*: `Esc` and every other key go
   raw to the child while it holds, and go back to the app's own bindings the moment
   it does not. Ctrl-C still means interrupt in both cases (it is forwarded, not
   swallowed), and Ctrl-Q stays ours so a grabbed keyboard is never a locked app.
4. The **re-anchor**. When the screen comes back the run loop stops the key stream,
   resizes the `Terminal` to the real size — which recomputes the inline viewport,
   clears it and resets ratatui's back buffer — and restarts the stream. That is
   rule 4's "force a full repaint, do not trust the diff", done with `resize`
   instead of dropping and rebuilding the `Terminal`. The key stream is stopped
   across it for the same reason `main.rs` already stops it on a window resize: a
   re-anchor reads the cursor position back, and the async reader would eat the
   answer.

**Four things that only showing up when you build it:**

* **A resize during a held screen must not resize the viewport.** A full-screen child
  is drawing on the real screen; `term.resize` queries the cursor and clears a region
  we are not showing — onto the *child's* screen, mid-frame. So during passthrough a
  resize goes to the pty only, and the app's own geometry is marked stale until the
  screen comes back.
* **A program that dies in the alt screen leaves the terminal there.** `SIGKILL`ed vim
  sends no leave sequence, and a terminal stranded on the alternate screen shows a
  dead program until the terminal is restarted. The command boundary pays the debt:
  releasing a hold that was entered via alt emits `ESC[?1049l` on the way out.
* **`--INSERT--` is terminfo, not vim.** The old spike checked for that string on
  screen. With `TERM=xterm-256color` vim uses the terminal's own insert-mode
  signalling and never writes it — measured: `i` on a bare pty emits 14 bytes of
  bracketed-paste toggling and no mode string. A check written against a guessed
  string fails with the feature working, and could pass with nothing displayed. The
  spike now runs a **control**: the same keystrokes against a bare pty, and looprs
  must show every marker the control actually drew; a marker the control never drew
  is reported `n/a` rather than quietly dropped. That is the general shape for
  "did the user see it" checks.
* **Closing the control's pty hangs the closer.** `os.close(master)` on macOS, with a
  pump thread blocked in `read()` on it, does not return (exit 124 under `timeout`,
  the close the last line reached). The spike quits the child instead of closing the
  end. Worth knowing before anyone writes a test harness that "cleans up properly".

**Measured** (`spikes/fullscreen_e2e.py`, `spikes/vim_fullscreen.py`, logs committed
under `spikes/results/`):

| check | result |
| --- | --- |
| vim's screen reaches the user (control drew the filename and `~` filler; looprs showed both) | pass |
| a whole screen of paint arrives, not just the typed characters (control 118 normalised chars, looprs 360) | pass |
| `Esc` then `:wq!` writes the file | pass |
| the app never reported an interrupt during the vim session (Esc was `0x1b`, not `0x03`) | pass |
| after vim, the shell still runs commands and they reach the screen | pass |
| less: first screen as full as the bare one (line 039), scroll down past it, `g` back to 001 | pass |
| a program that repaints in place with no alt screen is shown, and the screen returns on the command boundary | pass |
| Ctrl-Q quits after each of them | pass |
| the line-oriented path is unchanged (`spikes/bash_e2e.py`) | 20/20 |

**What is still not true, stated plainly:**

* There is still **no VT emulator**. The child draws its own screen; looprs neither
  knows nor shadows what is on it. Anything that requires knowing — mirroring the
  child's screen into a pane alongside our own UI, showing two at once, re-rendering
  the program's output after it exits — is still out of scope for this ADR.
* While a full-screen program is up it has **the whole window**: the status row and
  the input box are not visible. That is what handing over a terminal means, and it
  is what the ticket asked for; `looprs-afw` (the viewport shape) does not change it.
* The non-alt heuristic is exactly that — a heuristic over chunk shape. A program
  that emits its cursor addressing in the same chunk as a linefeed is not detected
  until a later chunk fits the shape, which may be one repaint later.
* Mouse events and bracketed paste are not forwarded (`key_bytes` maps keys). A
  full-screen program that needs the mouse gets keyboard-only input.
* The takeover is per Bash session, and only teed while that session is the mode on
  screen. A Bash child holding the screen while the user looks at Pi is not shown
  and is not lost either: its bytes go to its own view.

## Amendment 4 — two owners of one alternate screen: the handover rule (`looprs-pdl.12`)

Amendment 3 made a full-screen child's paint reach the user. It did it in the one
arrangement where that is simple: looprs drawing an **inline** pane, so the only
alternate-screen switch happening in the run is the child's own. ADR-0004 rule 1
then committed this app to taking the alternate screen itself, and that turns every
`vim`, `less` and `htop` into a second claimant on the same resource. This
amendment decides who wins and what the child gets instead. It does not re-open
Amendment 3's mechanism — the watcher, the tee and the file guard all stand; what
changes is what the watcher does with the switch bytes when *we* are the ones
living in the screen.

**R4.1 — While the app holds the alternate screen, a child's own
`?1049`-family enter and leave are cut, not teed.** They are removed from the
stream in `ScreenWatch::observe` (the same place that already sequences the
announcement against the paint), and the takeover and release events are reported
exactly as before. The child believes it switched screens; the terminal never did.

The decision is the asymmetric one. Write the child's `?1049h` and the terminal
saves the current main screen to make the alternate screen its new "back" buffer —
and inside our own alternate screen, the thing being saved **is our frame**. The
user's real scrollback is destroyed at the moment vim starts, before anything
could be corrected, and no later cleanup brings it back. Let the child's `?1049l`
through instead and the terminal restores that saved state: our old frame, with
looprs still drawing into a screen the user has just been dropped out of, and the
ledger still convinced it owns the alternate screen. Cutting the enter costs a
screen the child expected and can be given another way; teeing it costs the user's
history, which cannot be had back. Between a worse-looking program and a destroyed
scrollback, the scrollback wins.

**R4.2 — The cut enter is replaced with a blank canvas.** `?1049h` does two jobs:
switch screens, and hand the program a cleared display. Cutting the switch and the
clear together would paint the child over our previous frame, and every cell the
child does not write would show our UI through it. So the watcher emits
`\x1b[H\x1b[2J` in its place — home and erase, first bytes under the new owner.
Never `3J`: that clears scrollback, which is not ours to clear.

**R4.3 — Taking the screen back means clearing and re-asserting.** The child's
last frame is painted on the screen we still own, and cutting its leave also cut
the vanishing that leave performed. So the take-back path erases the canvas
(same primitive, R4.2) and then writes back every mode the ledger still holds
except two:

* **`Raw`** — a syscall, not a byte string, and still in force.
* **`AltScreen`** — because `?1049h` is the save-the-main-screen sequence, and
  re-sending it inside our own alternate screen re-bases the user's saved state
  onto our frame. The one mode the ledger holds is the one mode it must not
  re-assert. This is the same asymmetry as R4.1 arriving from the other
  direction.

What does get written back is the list that matters, because full-screen programs
switch all of it off on the way out — measured against vim: `?1000l`, `?1002l`,
`?1006l`, `?2004l`. Without the re-assert the mouse is dead for the rest of the
session and a pasted block arrives as if typed, with every newline in it executed.

**R4.4 — A child that dies inside the screen owes no leave.** The mirror of
Amendment 3's paid-debt case. `force_release` used to write a leave for a child
killed mid-paint; with the screen hosted, that leave would drop the *user* out of
the app's own screen, which is the exact inverse of the bug the function was
written for. A cut enter books nothing, so a forced release books nothing. The
watcher rearms all the same, so the next child is still seen.

**R4.5 — In the inline pane nothing changes.** The cut is conditional on
`hosting_alt_screen`, which is set from the ledger's own startup list
(`Mode::alt_screen_claimed()`) rather than re-derived from `$LOOPRS_MODES` at the
call site, so the session that cuts, the viewport that repaints and the exit that
leaves cannot disagree about who owns the screen. With no alternate screen the
child's switches tee through as they did, `ScreenDebt` still pays them, and every
check written before this amendment still passes in that mode set.

### Why not leave first

The other way to avoid the collision is for looprs to leave its own alternate
screen before the child enters, so the child has the terminal to itself. Rejected:
it shows the user their own main screen mid-handover — the transcript they are not
supposed to be able to see the way ADR-0004 R1 arranged it — and if the child
never comes back, or the app dies while it holds the screen, the user is left in a
terminal state neither owner booked. Hosting and cutting keeps the user inside our
screen from the first frame to the last, with no window in which the wrong thing is
visible.

### What R4 does not fix

* **The child's frame is the whole canvas.** It is not composited with our status
  row and input box; it replaces them, exactly as it does for a program run in a
  plain terminal. Handing over a terminal means that.
* **What the rows above the pane show after the handback is the app's problem to
  repaint, and today it repaints the live region only.** The erase in R4.3 makes
  the pane's own rows clean; a future frame migration that owns the whole window
  and can redraw the stored transcript into it is what makes the whole display
  restated rather than partially.
* **A child that asks the terminal about the alternate screen directly (`DECRQM`)
  will get an answer that contradicts what it asked for.** Nobody asked during the
  measurement; a program that does would be told the truth about a screen it
  believes it owns.
* The cut is per-Bash-session, on the session that holds the screen, and only for
  the alternate-screen family. Every other private mode a child sets — application
  cursor keys, the cursor itself, bracketed paste as *the child's* mode — passes
  through untouched, because only the screen belongs to us.

### Measured

`spikes/fullscreen_e2e.py` now runs the whole file in both mode sets, inline and
`LOOPRS_MODES=all`, 70/70 (`spikes/results/fullscreen-alt-handover.log`):

| Check | Result |
| --- | --- |
| exactly one `?1049h` on the wire — ours, at startup; the child's never appears | pass |
| the child was handed a blank canvas in place of the switch | pass |
| no `?1049l` at any point during the run; the user is never dropped to the main screen | pass |
| the take-back erases the child's frame before repainting | pass |
| mouse report / drag / SGR and bracketed paste are back on after vim switched them off (with a control proving the child really did switch them off) | pass |
| no watched mode is left different from before the child took the screen | pass |
| the re-assert does not re-enter the alternate screen | pass |
| a child SIGKILLed while holding the screen owes no leave and the app keeps going | pass |
| a child that leaves its own alt screen mid-command keeps the user on ours, and its later line output reaches the transcript | pass |
| across the whole session the exit leaves the alternate screen exactly once, and it is the exit's own | pass |
| the inline mode set is unchanged by all of this | pass |
| unit tests, `screen.rs` (cut / canvas / no-debt / rearm / non-screen modes pass through) | 449 pass |
| the mode ledger's own spike, unchanged by the handover (`spikes/results/shutdown-e2e-pdl12.log`) | 149/149 |
