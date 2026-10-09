# Configuration reference

**One table for every `LOOPRS_*` variable, and the only place their defaults are
written down.**

The repo's rule is one home per fact: a page that needs a knob *links here* rather
than re-listing the knob's default. `docs/kanban.md` is the one deliberate
exception and it is the owner page for its own family — where a knob's behaviour
needs a paragraph rather than a cell, the paragraph lives with the feature and this
table indexes it. [`./scripts/docs_check.sh`](../../scripts/docs_check.sh) compares
the defaults wherever two pages state them and fails on a disagreement, so the
exception cannot rot into a contradiction.

**23 `LOOPRS_*` variables are read by `src/`.** That count is not from the docs:
`./scripts/docs_check.py --list-knobs` prints them with the `file:line` of every
read, and the gate that keeps this page complete is a step of
[`./scripts/check.sh`](../../scripts/check.sh).

```sh
./scripts/docs_check.py --list-knobs   # what the code actually reads, right now
```

**Every variable is read once, at startup, and resolved into plain values.** Nothing
here is re-read per frame, per command, or per keystroke. That is a design rule
rather than a note: the App reads no environment at all, so every branch a knob
selects is reachable from a unit test without mutating a process
([contributor guide](contributing.md#the-no-environment-rule)). The consequence for
you is that **changing a variable means restarting the app**, and every resolved
choice is logged — `grep 'kanban board' looprs.log` shows what is on, how often and
reading which binary. Every fallback is loud, because a silently ignored knob is a
knob you will keep turning.

---

## Binary resolution

Which program each terminal state runs, and what happens when it is missing.

| knob | default | values | what it changes | set at | read in |
| --- | --- | --- | --- | --- | --- |
| `LOOPRS_PI_BIN` | `pi` | a path or a program name | the `pi` binary both `pi`-backed modes spawn — Beads's worker/planner and Pi's chat child | startup | `SessionConfig::default()`, [`src/services/pi.rs`](../../src/services/pi.rs) |
| `LOOPRS_BD_BIN` | `bd` | a path or a program name | the `bd` binary that reads the board **and** runs the loop — deliberately one binary, because two binaries in one run means two boards | startup | [`bd_bin_from_env`](../../src/services/bd.rs), used by the loop and by [`board_poller`](../../src/services/board_poller.rs) |
| `LOOPRS_SHELL_BIN` | `$SHELL` if it looks like bash, else `/bin/bash` | a path | the shell the Bash mode spawns as `bash -i` on a real pty ([ADR-0001](../adr/0001-bash-terminal-state-pty.md)) | startup | [`default_shell_bin`](../../src/session/mod.rs) |
| `$SHELL` | your login shell | a path | consulted only as the fallback source for the shell the Bash mode spawns, and only when it *looks like* bash | startup | same |

**A missing binary is a state, not a startup failure.** `bd` missing shows on the
status row as `bd unavailable: <reason>` and on the band footer with the same words;
`pi` missing surfaces when a session tries to spawn and says so in that mode's
transcript. Both keep the app up, because "the board is unreadable" and "there is no
board" are different facts and the app is not allowed to conflate them
([ADR-0007 §4](../adr/0007-kanban-board.md)).

`LOOPRS_SHELL_BIN` pointing at something that is not bash is warned about at spawn,
not silently accepted: exit codes and readiness detection need bash, and a zsh that
half-works is worse than a refusal that names the fix.

## Terminal modes, and the exit contract

| knob | default | values | what it changes | set at | read in |
| --- | --- | --- | --- | --- | --- |
| `LOOPRS_MODES` | `raw`, `alt_screen`, `cursor_hidden`, `mouse_report`, `mouse_drag`, `mouse_sgr` | comma list; `+name`/`name` to add, `-name` to drop, `mouse` for the three mouse modes, `all` for everything | which terminal modes the app switches on at startup, through the ledger. **`raw` and `alt_screen` are never droppable** — losing raw mode because of a typo is a worse failure than the typo | startup, before any mode is applied, and an unknown name stops the app with the valid names listed | [`Mode::startup_set`](../../src/teardown.rs) |
| `LOOPRS_PANIC` | unset | `draw` (and other stage names) | **fault injection**: makes the app panic at the named stage, so the exit contract can be tested against a real panic. Not a user knob; see below | startup | [`teardown::panic_injected`](../../src/teardown.rs) |

There is **no mouse-specific variable.** The mouse is a mode in the mode list, and there has never been a separate switch for it:

```sh
LOOPRS_MODES=-mouse ./target/debug/looprs    # no mouse: the emulator keeps its own selection
```

That is the switch you want inside tmux, over some SSH clients, and on any terminal
where mouse reporting fights with the emulator's own selection. With the mouse off,
`Ctrl-S s` still copies whatever selection you have.

`[ADR-0006](../adr/0006-terminal-mode-ledger.md)` is the argument for why this knob
exists at all: every mode in that list is a mode the app switches off exactly once
on every path out — normal exit, panic, `SIGTERM`, `SIGHUP`, and a full-screen child
that never gave the screen back.

## Frame and display

| knob | default | values | what it changes | set at | read in |
| --- | --- | --- | --- | --- | --- |
| `LOOPRS_KANBAN_ROWS` | *(unset — the height function decides)* | `3`–`8`; `0` = off | a fixed kanban band height. Below `3` the band is off **with a warning** (header + footer leaves no row for a bead); above `8` it is clamped to `8` with a warning. A pin is a ceiling on the *request*, never a guarantee | startup | [`KanbanBudget::from_raw`](../../src/viewport.rs) |

The rest of the kanban family (`LOOPRS_KANBAN`, `LOOPRS_KANBAN_POLL_MS`,
`LOOPRS_KANBAN_EVENTS`, `LOOPRS_KANBAN_RECONCILE_MS`, and the `bd` binary the band
reads) is documented **once**, with its reasoning, in
**[docs/kanban.md → The knobs](../kanban.md#the-knobs)**. Its defaults, restated
here only so this index is complete and identical to the owner page:

| knob | default | one line |
| --- | --- | --- |
| `LOOPRS_KANBAN` | *(unset — the board is **on**)* | `0` / `off` / `no` / `false` takes the board off entirely: no poller task, no `bd` read, no band |
| `LOOPRS_KANBAN_POLL_MS` | `5000` | the tick; `250` ms floor, clamped up with a warning |
| `LOOPRS_KANBAN_EVENTS` | *(unset — the change detector is **on**)* | `0` / `off` / `no` / `false`: every tick is a full board read and the journal is never touched |
| `LOOPRS_KANBAN_RECONCILE_MS` | `30000` | how long the poller may go without a full board read, however quiet the journal |

## Clipboard and mouse

| knob | default | values | what it changes | set at | read in |
| --- | --- | --- | --- | --- | --- |
| `LOOPRS_CLIPBOARD` | `auto` | `auto` \| `native` \| `osc52` \| `off` | which transport copies. `auto`: OSC 52 if this is an SSH session, otherwise the first native helper found (`pbcopy` / `wl-copy` / `xclip`), with OSC 52 as the fallback inside the same copy | startup | [`clipboard_from_env`](../../src/services/clipboard.rs) |
| `LOOPRS_CLIPBOARD_MAX_BYTES` | *(unset — **no cap**)* | a byte count | the one exception to "never truncate", and it is a cap *you* asked for. Even then the toast reports the bytes that actually went | startup | same |
| `LOOPRS_COPY_ON_SELECT` | *(unset — the automatic copy is **on**)* | `0` / `off` / `no` / `false` turns it off | whether releasing a mouse drag copies by itself. The explicit `Ctrl-S` chords keep working either way | startup | [`copy_on_select_enabled`](../../src/services/clipboard.rs), applied in `main` |

There is deliberately **no capability query**. Everything `auto` needs is free to
read (`SSH_CONNECTION`, `SSH_CLIENT`, `SSH_TTY`, the `PATH` lookups), and a terminal
that answers a capability query tells you about its parser, not its policy
([ADR-0004 R7](../adr/0004-fullscreen-tui.md)).

The transport actually chosen is logged once: `grep clipboard: looprs.log` →
`clipboard: osc52 (remote session)` or `clipboard: pbcopy` — which is the answer to
"why did that not paste".

## Transcript, journal and logging

| knob | default | values | what it changes | set at | read in |
| --- | --- | --- | --- | --- | --- |
| `LOOPRS_TRANSCRIPT` | *(unset — the journal is **on**)* | `off` / `0` / `no` / `false` | the running journal: every transcript entry appended and flushed **as it finalises**, while the process lives. `off` means nothing transcript-shaped is written to disk | startup | [`journal_from_env`](../../src/services/journal.rs) |
| `LOOPRS_TRANSCRIPT_DIR` | `~/.local/share/looprs/transcripts` (via `XDG_DATA_HOME`) | a directory | where the journal files go. A directory that cannot be created degrades to `Noop` with the reason logged — the loop works without a transcript dump | startup | same |
| `LOOPRS_TRANSCRIPT_DUMP` | *(unset — `Ctrl-S t` **writes**)* | `off` / `0` / `no` / `false` | the `Ctrl-S t` escape hatch that writes the whole settled transcript to a file you asked for. Separate from the journal: the dump answers "give me this, now"; the journal answers "what did the loop say at 03:12" | startup | [`transcript_sink_from_env`](../../src/services/transcript_file.rs) |
| `$XDG_DATA_HOME` | `~/.local/share` | a directory | the base for the journal directory | startup | `default_journal_dir` |
| `$XDG_CACHE_HOME` | `~/.cache` | a directory | the base for the **dump** directory (`$XDG_CACHE_HOME/looprs`) | startup | `default_dump_dir` |
| `RUST_LOG` | `debug` | any `EnvFilter` directive (`info`, `looprs=warn`, `looprs::bus=trace`) | the log filter. **The default is `debug`, not `warn`** — a long beads run produces a lot of lines, which is what `LOOPRS_PANIC`-scale debugging needs and what makes rotation a real question (see below) | startup | [`init_logging`](../../src/main.rs) |

Two things this group is responsible for that are worth knowing before you set them:

* **Nothing rotates anything.** The journal is one file per run per mode and the log
  is one file per run, both growing until the run ends. `~/.local/share/looprs/` is
  yours to clean; the path is logged at startup so you know where to point a
  cleanup. The open ticket on making the log destination configurable and the log
  itself bounded is `looprs-00u.13`.
* **These files contain everything the agent saw.** Including secrets that appeared
  in tool output. They are written `0600` in a `0700` directory — mode bits are the
  protection, nothing else — and the way to turn each sink off is on this page.

## Notifications

| knob | default | values | what it changes | set at | read in |
| --- | --- | --- | --- | --- | --- |
| `LOOPRS_NTFY_URL` | unset (notifications **off**) | an ntfy endpoint, e.g. `https://ntfy.sh` | where a finished ticket is announced. Needs `LOOPRS_NTFY_TOPIC` as well; with either missing the notifier is `Noop` and nothing leaves the terminal | startup | [`notifier_from_env`](../../src/services/notification.rs) |
| `LOOPRS_NTFY_TOPIC` | unset (notifications **off**) | a topic name | which topic the announcement is published to | startup | same |

Exactly one `notify` call exists in the binary, on the "the board says this ticket
closed" edge: not when the worker settled, not when a pass was cancelled, not when
the planner settled, not when the worker handed a `blocked` bead to a human. The
table of those seven endings is in
[Driving the Beads loop](beads-loop.md#what-gets-announced-and-when).

```sh
LOOPRS_NTFY_URL=https://ntfy.sh LOOPRS_NTFY_TOPIC=looprs-$USER ./target/debug/looprs
```

Both are required because a half-configured notifier is worse than none: the app
would either guess a topic (leaks your ticket titles to strangers) or silently do
nothing. `grep 'notifications: off' looprs.log` tells you which you have.

## Harness-only variables — do not tune production with these

Everything below exists so a **test, spike or measurement** can drive the app. None
of them changes how the app behaves for you in a way you want to depend on, and
several of them exist only to make a test's assertion possible.

| knob | who sets it | what it is |
| --- | --- | --- |
| `LOOPRS_BASH` | **set by looprs, for bash** | exported into the Bash mode's child as a marker so the injected `--rcfile` integration knows it is inside looprs. Setting it yourself changes nothing about the app |
| `LOOPRS_PROBE` | a unit test | the variable a test exports to check that shell env survives between commands. Not a knob |
| `LOOPRS_PANIC` | the shutdown spike | fault injection (`draw`); documented above because it is *read* by production code |
| `LOOPRS_MEASURE_CORPUS` | `src/measure.rs` | a real `pi` session file or directory to replay for the working-set measurement. Both measurement tests are `#[ignore]`d and print a helpful message without it |
| `LOOPRS_MEASURE_TICKETS` | `src/measure.rs` | how many corpus files to replay (default 12, oldest first) |
| `LOOPRS_MEASURE_BUFFER` | `src/measure.rs` | `0` disables the buffer for the resize-transient measurement |
| `LOOPRS_BIN` | the spike drivers | which binary a spike runs — the control-run lever, so a spike can run the same script against the pre-change build |
| `LOOPRS_SPIKE`, `LOOPRS_SPIKE_N`, `LOOPRS_SPIKE_SCALE`, `LOOPRS_SPIKE_PROBES`, `LOOPRS_SPIKE_TRACE`, `LOOPRS_SPIKE_BASH`, `LOOPRS_SPIKE_BD` | the spike harness | spike-internal: scale, probe set, trace flag, which fake binary |
| `LOOPRS_E2E_RAW`, `LOOPRS_E2E_TIMELINE` | e2e spike drivers | capture/debug levers in the pty drivers |
| `LOOPRS_FAKE_HOLD` | the fakes | makes the fake `pi` hold a run open so a test can watch a run in flight |
| `LOOPRS_REWRAP_REPLY` | the rewrap spike | the reply text the fake emits, for re-wrap assertions |

The rule the last two columns encode: **if the variable's purpose is to make a test
assert something, it does not belong in a run you care about.** The gate keeps them
*out of this page's* tables by listing them here, which is also what stops
"documented knob nobody reads" failures for harness variables.

## Combinations that are actually useful

Each of these was run. They are a starting point rather than a menu.

**The SSH box** — OSC 52 is chosen automatically; nothing to set. If the terminal
eats OSC 52, say so explicitly rather than debugging a silent copy:

```sh
LOOPRS_CLIPBOARD=osc52 ./target/debug/looprs
```

**No mouse** — inside tmux, or anywhere the emulator's own selection should win:

```sh
LOOPRS_MODES=-mouse ./target/debug/looprs
```

**Clipboard off entirely** — nothing this app does touches your pasteboard. Useful
when a session will contain things you do not want in a clipboard you sync
everywhere:

```sh
LOOPRS_CLIPBOARD=off LOOPRS_COPY_ON_SELECT=0 ./target/debug/looprs
```

**Nothing on disk** — no journal, no dump. The transcript lives and dies in the
process:

```sh
LOOPRS_TRANSCRIPT=off LOOPRS_TRANSCRIPT_DUMP=off ./target/debug/looprs
```

**Board off** — no kanban band, no poller task, no `bd` read at all. The frame is
then exactly the pre-board frame:

```sh
LOOPRS_KANBAN=0 ./target/debug/looprs
```

**The tiny window** — a 3-row band is the smallest thing that is still a board; a
window that cannot hold it gets no band rather than a stub:

```sh
LOOPRS_KANBAN_ROWS=3 ./target/debug/looprs
```

**Quiet logs** — the default is `debug`, which is a lot on a long run:

```sh
RUST_LOG=info ./target/debug/looprs
RUST_LOG=looprs::bus=trace,looprs=warn ./target/debug/looprs   # one module, loudly
```

**A harness-shaped dev setup** — fakes for everything, no model, no board, and the
transcript somewhere obvious:

```sh
LOOPRS_PI_BIN=$PWD/tmp/fakes/pi \
LOOPRS_BD_BIN=$PWD/tmp/fakes/bd \
LOOPRS_TRANSCRIPT_DIR=./tmp/transcripts \
RUST_LOG=debug \
  ./target/debug/looprs
```

## How this table is kept true

`./scripts/docs_check.sh` (step 5 of [`./scripts/check.sh`](../../scripts/check.sh))
checks three things about this page:

1. **every `LOOPRS_*` read in `src/` appears here** — an undocumented knob is a
   knob nobody will find at 2am;
2. **every knob documented here exists somewhere in the repo** — a documented knob
   nothing reads is a lie about a feature;
3. **no two pages state a different default for the same knob** — the two-decimal
   version of "a second list of the same variables is a future argument with the
   first about which one is true".

Add a knob to the code without adding a row here and the build fails with the
`file:line` of every read. That is the whole point: the default is documented
*because* it is checked, not because someone remembered.

**See also:** [docs/kanban.md](../kanban.md) (the board family's owner page) ·
[operator reference](operator.md) (where the files those knobs choose actually end
up) · [contributor guide](contributing.md#add-a-looprs_-knob) (how to add one
without breaking this page)
