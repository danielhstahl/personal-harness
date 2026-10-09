# Contributor guide

Every ADR in this repo opens by telling you which file to read before you change the
thing it covers. That only works if you can find the right file. This is the map.

Read this page if you are about to change code. Read
[docs/testing.md](../testing.md) when you want to know how what you changed gets
checked — this page maps the testing ladder and links there rather than restating it.
And when reading the code turns up something that made you wince, the protocol for
what happens next is [the improvement sweep](improvement-sweep.md): a finding gets
a ticket with evidence before it gets a fix.

---

## The shape of the program

One process, one tokio runtime, four kinds of component: the frame, the router, the
sessions (subprocesses), and a set of injected sinks that are the only things
allowed to touch the outside world.

```text
                         ┌──────────────────────── crossterm EventStream
                         │  keyboard · mouse · resize
                         ▼
   ┌─────────────────────────────────────────────────────────────────────┐
   │ main::run   the one task that owns the UI                            │
   │   ├─ App (state) ── SessionView per mode ── Scrollback / Selection  │
   │   └─ viewport::ScreenFrame ──► ratatui ──► stdout ──► the pty      │
   └───────┬───────────────────────────────────────────────▲────────────┘
           │ UiCommand (mpsc, 16)                           │ Msg (crate::bus, byte-bounded)
           ▼                                                │
   ┌───────────────────────────────────────────────┐         │
   │ session::Router  owns every backend; the only │         │
   │ task that may touch a session                 │         │
   │   ├─ BeadsSession ──► BeadsTask (own task) ──┼─────────┘
   │   ├─ PiChatSession ─► pi --mode rpc (child) ─┼─────────┘
   │   └─ BashSession ───► bash -i on a pty ──────┼─────────┘
   └───────┬───────────────────────────────────────┘
           │ spawn / write / kill
           ▼
     ┌───────────────┐   ┌──────────────┐   ┌────────────────┐
     │ pi (child)    │   │ bash (pty)   │   │ bd (short-lived)│
     └───────────────┘   └──────────────┘   └────────────────┘

  Outside the hot path, built once in main and handed in as values:
     services::clipboard · services::journal · services::transcript_file
     services::notification · services::board_poller (its own task)
     teardown::Teardown (the terminal-mode ledger)
```

**Every arrow has a rule attached**, and the rules are the reason the type boundaries
exist:

* The App talks to sessions **only** by sending the Router a `UiCommand`. Nothing
  else may touch a backend.
* Sessions talk to the App **only** by sending `Msg` on the bus. The bus is byte
  bounded and coalescing: a producer that outruns the UI waits rather than being
  *stored* ([`bus.rs`](../../src/bus.rs)).
* Nothing that touches a filesystem, a clipboard, a network or a subprocess lives
  inside the UI. Those are **injected sinks** built in `main`, once, and `SessionConfig`'s
  defaults are `Noop` — which is what keeps ~600 tests off the disk, the clipboard
  and the network **by construction** rather than by nobody remembering to unset a
  variable.
* The teardown object is built **before** anything can switch a terminal mode on,
  and the panic hook goes in before anything can fail with one switched on
  ([ADR-0006](../adr/0006-terminal-mode-ledger.md)).

## Module map

Every module in `src/`, what it owns, and what it must never own. Cross-checked
against `find src -name '*.rs' | sort`; a module missing from this table means this
page is incomplete, which is the failure mode it is written against.

### The frame and the run loop

| module | owns | must never own |
| --- | --- | --- |
| [`main.rs`](../../src/main.rs) | startup order: the ledger, the mode set, the frame, the sinks, the run loop, the shutdown drain. The **only** place a real sink is built | business logic; environment reads below the "resolve once" layer |
| [`viewport.rs`](../../src/viewport.rs) | frame geometry: the five bands, the ladder that pays for them, `frame_areas`, the kanban row budget, the window poll, `repaint_all`. Has an "adding a band" checklist at the top of the file | knowing what any band *means* |
| [`screen.rs`](../../src/screen.rs) | the screen-debt watcher: which terminal modes a full-screen child switched on through us, so the exit can hand them back | switching modes itself (that is the ledger) |
| [`teardown.rs`](../../src/teardown.rs) | `Mode`, the ledger, `startup_set`, `restore`, the panic hook. Exact-once hand-back from every path | asking the terminal a question at exit (forbidden: it is a round trip on the exit path) |
| [`signals.rs`](../../src/signals.rs) | `SIGTERM`/`SIGHUP`/`SIGINT` delivery into the run loop | deciding what a signal means (the run loop does) |

### The wire

| module | owns | must never own |
| --- | --- | --- |
| [`wire.rs`](../../src/wire.rs) | the message vocabulary: `Msg`, `UiCommand`, `SessionEvent`; the named value sets (`EntryRole`, `CompactionReason`, `StopReason`) and `WIRE_INVENTORY`, the table the [protocol page](wire-protocol.md) is generated from | transport, buffering, policy |
| [`bus.rs`](../../src/bus.rs) | the bounded UI byte lane: the cap, the merge rule for `BashOutput`, the back-pressure on producers | any app semantics. It is a queue with opinions about bytes and nothing else |

### Sessions

| module | owns | must never own |
| --- | --- | --- |
| [`session/mod.rs`](../../src/session/mod.rs) | `TerminalType`, `SessionId` + generation, the `Session` trait, `SwitchAway`, the dead-code policy written out where the blanket `allow` used to be | rendering |
| [`session/router.rs`](../../src/session/router.rs) | who owns a session, key routing, the shutdown grace, spawn/respawn | deciding what a keystroke means to a session beyond delivering it |
| [`session/beads.rs`](../../src/session/beads.rs) | the beads session half: `BeadsCmd`, the handle the Router holds, the pieces wired together | the UI. It reports `SessionEvent` and does not reach into a `Msg` type any more |
| [`session/beads/machine.rs`](../../src/session/beads/machine.rs) | the pass boundary: the claim guard, `verify_plan`, `verify_worker_pass`, which cause a pass answers for | whether a pass may start right now (the guards decide that) |
| [`session/beads/task.rs`](../../src/session/beads/task.rs) | the owning task: the parked-state flags, settle / abort / shutdown handling, the mirrors a stale reader sees | the pass itself (the machine decides that) |
| [`session/beads/guards.rs`](../../src/session/beads/guards.rs) | the pure decision layer: `PassGate`, `StepCause`, the cancel and stall-timer predicates | a clock, a process, or a mutation — it answers, it does not act |
| [`session/beads/notes.rs`](../../src/session/beads/notes.rs) | the wording: the worker prompt, and every note about a plan, a claim or a pass | the verdict the note is attached to |
| [`session/pi_chat.rs`](../../src/session/pi_chat.rs) | the `pi --mode rpc` child: one child, many turns, queueing, abort | the beads loop's notion of a pass |
| [`session/bash.rs`](../../src/session/bash.rs) | the Bash session half: `BashCmd`, the handle the Router holds, the `Esc` decision reached in terms of the cancel state machine | the pty itself |
| [`session/bash/pty.rs`](../../src/session/bash/pty.rs) | the real pty: spawn `bash -i`, the exit-marker rcfile, the blocking reader thread and its byte lane | re-writing the child's bytes ([ADR-0001 rule 1](../adr/0001-bash-terminal-state-pty.md)) |
| [`session/bash/task.rs`](../../src/session/bash/task.rs) | the owning task: the command queue, `ensure_shell`, the output pump, the exit marker, stream end, shutdown | `Esc` (interrupt does) and the screen handover (screen does) |
| [`session/bash/interrupt.rs`](../../src/session/bash/interrupt.rs) | `Esc`: a queued command taken out, or `0x03` on the master, and the stalled-cancel report | the grace window (`session/cancel.rs` owns that) |
| [`session/bash/screen.rs`](../../src/session/bash/screen.rs) | resize, and the full-screen handover that gives the child the terminal and takes it back | who hosts the screen (the frame decides that) |
| [`session/bash/reap.rs`](../../src/session/bash/reap.rs) | the never-reap-alone rule (looprs-2ck) and the wording around a dead shell | the cancel escalation policy itself |
| [`session/cancel.rs`](../../src/session/cancel.rs) | the cancel state machine and the grace window | deciding what `Esc` means per mode (the chord table does) |
| [`session/view.rs`](../../src/session/view.rs) | the view contract: `SessionView` per mode, how each is built, the answers read off it | touching a clock or the environment (there is a `MAX_VIEWS`/buffer budget constant each) |
| [`session/view/flush.rs`](../../src/session/view/flush.rs) | the one door every finalized line goes through, and the record/push verbs around it | the width policy (rewrap owns that) |
| [`session/view/rewrap.rs`](../../src/session/view/rewrap.rs) | rebuild at a new width from the source slice the budget covers, and the drop accounting for what it cannot reach | the store (scrollback owns rows) |
| [`session/view/buffer.rs`](../../src/session/view/buffer.rs) | the retained-history ceiling and the two ways a view lets go when it runs over | what one row costs the screen |
| [`session/view/shell_tail.rs`](../../src/session/view/shell_tail.rs) | the Bash tail: pty chunks in, sealed transcript entries out | the command boundary (bash decides that) |
| [`session/view/chord.rs`](../../src/session/view/chord.rs) | the keyboard vocabulary: `KeySym`, `Effect`, `Owner`, `ChordState`, `ChordRow` | the rows themselves |
| [`session/view/chord_table.rs`](../../src/session/view/chord_table.rs) | `CHORD_TABLE`: the keymap as data, generated against by `docs/guide/keymap.md` | dispatch code — the table is read by rule tests, not by the key path |

### State (the store)

| module | owns | must never own |
| --- | --- | --- |
| [`state/transcript.rs`](../../src/state/transcript.rs) | the entry model: what an entry is, plain text, open/closed cards | layout, widths |
| [`state/scrollback.rs`](../../src/state/scrollback.rs) | the bounded store: rows, provenance, pin/unpin, rewrap, the cell map (clusters, combining marks), eviction | reading the clock or the env |
| [`state/selection.rs`](../../src/state/selection.rs) | the selection and its band snapshot | the clipboard (a sink does that) |
| [`state/wheel.rs`](../../src/state/wheel.rs) | the wheel throttle and gesture boundaries | scroll position (the store owns that) |
| [`state/toast.rs`](../../src/state/toast.rs) | toast lifetime and queue | what a toast says |
| [`state/board.rs`](../../src/state/board.rs) | the `bd` status → column function and the snapshot | calling `bd` (the poller does) — and the mapping's *rationale*, which is [ADR-0007 §1](../adr/0007-kanban-board.md) |

### Services (the injected sinks)

| module | owns | must never own |
| --- | --- | --- |
| [`services/clipboard.rs`](../../src/services/clipboard.rs) | the transport ladder (native helper / OSC 52), the cap, the receipt shape | being called from the draw path |
| [`services/journal.rs`](../../src/services/journal.rs) | the running transcript file: per-entry append and flush, the `last` symlinks, `0600`/`0700` | being read back. Nothing parses the journal |
| [`services/transcript_file.rs`](../../src/services/transcript_file.rs) | the `Ctrl-S t` dump: path resolution, `~` folding, the polled receipt | being the journal (it is not) |
| [`services/notification.rs`](../../src/services/notification.rs) | the ntfy publish: envelope, headers, byte limits, bounded retries | deciding *when* to notify (the beads loop does that, once) |
| [`services/board_poller.rs`](../../src/services/board_poller.rs) | the state the poller publishes: the owner `main` holds, and the `BoardHandle` every reader takes | the decision of which read is owed (schedule decides that) |
| [`services/board_poller/config.rs`](../../src/services/board_poller/config.rs) | the knobs, resolved once from strings by pure functions | a policy the operator did not ask for |
| [`services/board_poller/schedule.rs`](../../src/services/board_poller/schedule.rs) | the watermark logic: `ReadReason`, `Decision`, `decide` | calling `bd` (read does) |
| [`services/board_poller/read.rs`](../../src/services/board_poller/read.rs) | the tick loop and the `bd` calls it makes, one read at a time | deciding whether this tick owes one |
| [`services/bd.rs`](../../src/services/bd.rs) | every `bd` invocation, its parsing, and the "a failing `bd` is not an empty board" rule | the board's visual meaning |
| [`services/pi.rs`](../../src/services/pi.rs) | the `pi` binary resolution and the rpc envelope | the conversation model |
| [`services/prompts.rs`](../../src/services/prompts.rs) | the prompt text handed to planner and worker | the loop's control flow |

### Rendering and content

| module | owns | must never own |
| --- | --- | --- |
| [`components/text_stream.rs`](../../src/components/text_stream.rs) | the transcript band layout, the "N new" pill, band layout helpers | the store's state |
| [`components/line_render.rs`](../../src/components/line_render.rs) | turning a rendered row into styled spans | deciding row content |
| [`components/card.rs`](../../src/components/card.rs) · [`tool.rs`](../../src/components/tool.rs) | the live tool card wall | the tool's own state machine |
| [`components/compaction.rs`](../../src/components/compaction.rs) | the compaction card and its three endings | the summariser (pi does that) |
| [`components/status.rs`](../../src/components/status.rs) | the status row: verbs, glyphs, the drop ladder, the 40-column floor | guessing state — it is a pure function of the app's state |
| [`components/kanban.rs`](../../src/components/kanban.rs) | the band as a pure function of snapshot + area | a clock, the env, or a `bd` call ([ADR-0007 rule 10](../adr/0007-kanban-board.md)) |
| [`components/selection.rs`](../../src/components/selection.rs) | the highlight overlay | selection state itself |
| [`components/toast.rs`](../../src/components/toast.rs) | toast painting | toast policy |
| [`components/input.rs`](../../src/components/input.rs) | the input box state | submission semantics beyond the buffer |
| [`theme/`](../../src/theme) | the palette and the style roles | ad-hoc colours in components |
| [`utils/shelltext.rs`](../../src/utils/shelltext.rs) | the shell-output content model: what may be rewritten, ANSI handling ([ADR-0005](../adr/0005-shell-output-content-model.md)) | the shell itself |
| [`utils/md.rs`](../../src/utils/md.rs) | markdown rendering for the transcript (the same `pulldown-cmark` this site is built with) | the site build (that is `docs/tools/gen`) |
| [`utils/render.rs`](../../src/utils/render.rs) | shared render helpers | layout policy |

### Test-only modules (compiled under `#[cfg(test)]`)

| module | owns |
| --- | --- |
| [`testing.rs`](../../src/testing.rs) | `Fakes`: builds real executables in a temp dir per test, the board/journal/show/fail levers, the log pollers |
| [`measure.rs`](../../src/measure.rs) | the working-set, resize-transient and live-preview-per-frame measurements over a real corpus (`LOOPRS_MEASURE_*`); the live-preview numbers are in [`spikes/results/live-preview-cost.log`](../../spikes/results/live-preview-cost.log) |
| [`app/tests/*.rs`](../../src/app/tests) | the App-level behaviour suites, one file per concern: chord table, drag selection, copy-on-select, scrollback band, status row, token window, wheel, screen-held, input box, pi state, user band |

## The no-environment rule

Two rules, and almost every design decision above follows from them:

**The App reads no environment and no clock.** Knobs are resolved in `main`, once,
into plain values, and handed in. `viewport::kanban_rows(h, tools, input, budget)`
takes its budget as an argument; `BoardSnapshot::stamped_at(instant)` takes its
instant. Every branch a knob can select is therefore reachable from a unit test
without mutating a process or waiting on a timer.

**Sinks are injected and default to `Noop`.** The App builds nothing that touches a
filesystem, a clipboard, a socket or a subprocess. `main` builds the real ones.
This is stated in four places in the code because it is the rule that gets broken
first, and the reason it gets broken first is that the shortcut (a `std::fs::write`
inside a key handler) works perfectly until the day it makes a test touch the disk.

If you find yourself writing `std::env::var` or `Instant::now()` outside `main` and
outside the resolve-once layer, stop and read
[`main.rs`](../../src/main.rs)'s wiring before you do.

## Recipes

Each is a procedure, in order, with the files to touch.

### Add a wire message variant

1. `wire.rs` — add the variant to `Msg` (or `UiCommand` if it is App → Router).
   Give it a payload that carries what the *consumer* needs, not what the producer
   happens to have.
2. If it is on the byte lane, decide its coalescing class in `bus.rs`: mergeable
   (`BashOutput`-like) or queued-whole. Getting this wrong in the mergeable
   direction is how output disappears.
3. Route it in `session/router.rs` if it can arrive for a non-active mode.
4. Handle it in `app.rs` and write the state; add a test that the *right* view gets
   it and the wrong one does not (`a_status_edge_lands_in_its_own_view_and_not_in_the_active_one`
   is the template).
5. If it renders, add the component case. Do not add a `String` field to the App to
   make a renderer happy.

### Add a pi protocol value (a role, an event type, a reason)

1. `wire.rs` — add the variant. If it belongs to one of the three `WireValue`
   types, you must also classify it in that type's **exhaustive `outcome()`
   match**: a new value nobody has decided the fate of does not compile. That is
   the point. `Unknown(_)` is not the lazy answer to a new value either — it is
   the arm for values that do not exist yet, which is why it carries the string.
2. Add its row to `WIRE_INVENTORY`: what happens to it, who reads it, and — if
   nobody reads it today — what it is waiting for. `reader: None` without a
   `waiting_on` fails `app::tests::wire_protocol`, on the same rule that makes
   every `#[allow(dead_code)]` carry a reason.
3. Handle the arm in `app.rs` (or say why the existing arm covers it). A role
   nothing renders must still be *visible*: see the unrendered-role note.
4. `./scripts/docs_check.py --fix-wire` and read the diff. A value in the type
   with no row on the protocol page fails the gate; so does hand-editing the
   page instead of regenerating it.

### Add a band, or change a band's row budget

1. `viewport.rs` — there is an **"Adding a band" checklist** at the top of the
   file. Follow it; it is the accumulated cost of the last four times.
2. Add the budget type or constant, and a pure function of `(height, other bands,
   budget)` that returns rows, with `0` meaning "not drawn" — never `1` as a stub.
3. Add it to `frame_areas`. Decide, explicitly, which band pays for it: the ladder
   order is the argument, and the transcript's surplus is the only free money.
4. Test the invariant that matters: **turning the new band on costs only the band
   that was going to pay for it** — the input box, the cards and the status row keep
   their rows and their places. That test exists for the kanban band
   (`turning_the_board_on_only_ever_costs_the_transcript_its_surplus`) and it is the
   shape to copy.

### Add a key / chord

1. `session/view/chord_table.rs` — add a row to `CHORD_TABLE`: `mode`, `key`, `keys`,
   `state`, `owner`, `does`, `note`. **Every row names one mode explicitly**; a row
   that says "all modes" hides the fact that `Ctrl-C` does not mean the same thing
   in all of them.
2. Wire the handler where the row's `does` says it goes. The table is not walked by
   the key path, so a row with no handler is documentation of a key that does
   nothing — the audit in `app/tests/chord_table.rs` checks the table's internal
   rules, not that a handler exists.
3. `./scripts/docs_check.py --fix-keymap` — regenerate the keymap tables.
   Without this the build fails, which is the point.
4. If the chord *copies* something, it belongs in the `Ctrl-S` family and the
   refusal message has to name the chord that works
   ([ADR-0004 R17](../adr/0004-fullscreen-tui.md)).

### Add a session backend / a fourth terminal state

This is the expensive one, and the table is where the cost lands.

1. `session/mod.rs` — a new `TerminalType` variant, its `label()`, `next()` (the
   `Tab` cycle is a 3-cycle today; a 4th mode changes the cycle and every test that
   walks it), and a `switch_away_policy()` with an explicit choice.
2. A `Session` implementation; a spawn arm in the factory; a `SessionId`
   generation per the existing rule.
3. `session/router.rs` — the new arm.
4. `session/view/buffer.rs` — `MAX_VIEWS` is `TerminalType::ALL.len()`, so the retained
   byte ceiling moves: `RETAINED_BYTES_WORST_CASE = MAX_VIEWS * (store + buffer)`.
   Re-read the startup log line and make sure the number is one you want.
5. `CHORD_TABLE` rows for the new mode for **every** chord. The table has no
   wildcard rows; that is by design.
6. The status row's mode walk; the docs: index page, keymap, configuration.
7. If it is a subprocess: `SessionConfig` gets its binary and its sink, `main`
   builds the real sink, and the default stays `Noop`.

### Add a `LOOPRS_*` knob

1. Resolve it in `main.rs` (or in the service's own `*_from_env`, which is the
   pattern for services), as a **pure function of the string** so every branch is
   testable without a process env.
2. Resolve it **once**, at startup, and log the resolved value. A fallback must be
   loud: `viewport::KanbanBudget::from_raw` warns on every fallback it takes, and
   that is why "the knob was ignored" is a greppable event.
3. Choose the default so a typo cannot turn a feature off: `kanban` uses "only an
   explicit `0/off/no/false` is off". Inverting that means a mistyped variable eats a
   feature silently.
4. Add the row to
   [`docs/guide/configuration.md`](configuration.md) — the gate fails without it,
   and prints the `file:line` of every read.
5. If the knob changes what a key does, update the keymap prose. If it changes what
   a band looks like, update the owner page (kanban.md).

### Add an ADR

1. Copy the shape of `docs/adr/0007-kanban-board.md`: title, status, date,
   ticket, Context, Decision, Consequences, a "what this forbids" list, and
   measured numbers with the log or spike that produced them.
2. Number it next (`0008`, `0009`, …). Do not renumber existing records; links
   are by number.
3. Add it to `docs/SUMMARY.md` and to the table in
   [`docs/README.md`](../README.md). `docs_check` fails on a page that is not in
   the tree.
4. **Amend rather than replace.** The convention is an "Amended" section citing the
   ticket that changed the decision, keeping the original text — see ADR-0004's
   amendment about the fifth band. A rewritten ADR destroys the record of what was
   believed when the decision was made.
5. A claim in an ADR with no measurement behind it is worse than no claim. Either
   measure it or write "not measured, because X" — the second is acceptable, the
   silence is not.

## The testing ladder (a map, not a duplicate)

The detail lives in [docs/testing.md](../testing.md). What belongs here is **which
rung a change has to touch**:

| your change | the rung that covers it | how to run it |
| --- | --- | --- |
| a pure function (mapping, budget, slug, parse) | unit test, no fakes | `cargo test <module>::tests::<name>` |
| a session backend's behaviour | the fakes: real subprocesses, real pipes | `cargo test session::<name>` |
| a keystroke's effect | the App tests + the `CHORD_TABLE` audits | `cargo test app::tests` |
| "what is painted" | `TestBackend` tests in `main.rs` / components | `cargo test` |
| "what reaches the wire" | a real-pty spike (`spikes/*_e2e.py`) with a **control run** against the previous build | `python3 spikes/status_e2e.py` |
| "the protocol page still says what the code does" | the generated tables vs `WIRE_INVENTORY`, plus the inventory tests that run each role through `apply_pi` | `./scripts/docs_check.py --fix-wire && cargo test wire_protocol` |
| a spike's check count quoted in a page | the capture committed under `spikes/results/`, which the docs gate reads back against the prose | `./scripts/capture.sh <spike> <capture-name>` then `./scripts/docs_check.sh` (`--list-captures` shows every total and which capture is current) |
| memory / cost claims | `src/measure.rs` (needs `LOOPRS_MEASURE_CORPUS`), `spikes/*.py` | `cargo test -- --ignored` |
| anything behind a cargo feature | the **same** suite over that build; the seam is not allowed to change shape | `cargo test --features notify` |
| a dependency's right to be in the manifest | `scripts/dep_audit.py` — named in `src/`, or answered at its own line | `python3 scripts/dep_audit.py --gate` |
| the whole gate | `./scripts/check.sh` — fmt, clippy `-D warnings` and tests in **both** feature configurations, dead-code audit, dependency audit, docs gate | `./scripts/check.sh` |
| docs only | `./scripts/docs_check.sh` | same, or on its own |

Two rules about the ladder that are not obvious from the run commands:

**A spike without a control run is an untested claim.** Every spike can run against
an arbitrary binary (`LOOPRS_BIN=…`), and the control is what proves the spike
measures the thing it says. When a control run *passes* on a pre-feature binary, the
needle was firing on something else — that happened twice and both are written up in
[docs/testing.md](../testing.md).

**An `#[allow(dead_code)]` must name its reader.** The dead-code blanket allow is gone
and the policy is written where it used to be
([`session/mod.rs`](../../src/session/mod.rs)). `dead_audit.py --gate` asks the
compiler, through the allow, whether the item is still dead, and a redundant allow
fails the build. It exists because nine allows promised a consumer that landed
somewhere else.

**The same rule, one file over: a dependency must name its user.** The manifest used
to carry `unused_dependencies = "allow"` — a second blanket, over the half of the
tree `dead_audit.py` cannot see. It is gone (looprs-00u.15), and
`scripts/dep_audit.py --gate` is what keeps it gone: every `[dependencies]` entry
has to be named in `src/`, or answered at its own line with a `dep-audit:
<reason>` comment for the case where the grep is wrong (a macro, a rename, anything
that is not a `path::` expression). Two things that audit settled, both worth
knowing before you trust one signal:

* cargo's own lint is not a complete witness here. Dropping the blanket reports
  nothing unused — correct — but a *deliberately unused* `ryu = "1"` added to the
  same manifest also reported nothing, while the identical mistake in a
  two-dependency crate was caught. Hence a grep of the crate's own making.
* the feature this ticket claimed was spike-only (`crossterm`'s `osc52`) is **not**.
  `src/services/clipboard.rs::osc52_bytes` calls crossterm's own writer to frame
  the app's remote-session copy path, so the app and the spike ship one
  implementation rather than two that merely agree. The premise was wrong; the
  manifest says so where the feature is declared.

## House style

The conventions that make this codebase read like one person wrote it. They are not
cosmetic; each is a defence against a specific failure.

**Comment the constraint, not the code.**

```rust
// bad — restates the code
let (a, b) = split(x);          // split x into a and b

// good — states the constraint the code cannot express
// Bounded, and coalescing, on purpose (looprs-6cj). This line used to be
// `mpsc::unbounded_channel`: every producer wrote into a queue with no ceiling
// and one task read it, so a producer that could out-write the UI did not get
// slowed down — it got *stored*, in this process's RAM, until the machine
// stopped.
let (app_tx, mut app_rx) = bus::channel(bus::DEFAULT_CAP_BYTES);
```

**Measure before you claim.** "About 4× faster" is not a claim. A claim is:

> the probe costs ~0.18 s against ~0.46 s on this repo's 50-bead board
> ([`spikes/results/board-poll-cost.log`](../../spikes/results/board-poll-cost.log))

…with the log committed and the script that produced it in the same repo.

**One home per fact.** A list of knobs in two places is a future argument about
which one is true. A status→column mapping copied into three files is a mapping that
drifts. The docs site enforces this mechanically (`docs_check`'s default-agreement
check) for the case that comes up most.

**A refusal must name the way out.** `Nothing copied` is a dead end.
`Nothing copied: X is not a copy chord — Ctrl-S ? lists them` is a message. Same
rule in the CLI surface: `bd unavailable: command not found (LOOPRS_BD_BIN=bd)`
gives you something to do.

**Names state the property.** `SwitchAway::DrainThenPark` says what it does.
`last_good_read` says what it is. `a_ticket_that_is_never_closed_stops_the_loop_instead_of_spinning`
is a test name that is a sentence, and that is deliberate: the test list is a
specification, and a specification you have to read the body of is not one. The
second half of that is that the quotation has to be *exact* — the docs gate fails a
backticked test-shaped name that no `fn` in the tree answers to, because a
specification you cannot grep is not a specification either. Quote the name out of
`cargo test --list`, not out of memory.

**Bad, from a review comment I have made more than once:**

```rust
// bad — the "why" is a guess, the number is unsourced, and the fallback is silent
if timeout_reached {
    // probably fine, just retry
    retry();
}
```

Three problems: *probably* is not a reason, *fine* is not measured, and a silent
retry is how a wedged dependency becomes an infinite loop nobody can see. The
version this repo writes names the grace, cites where the grace was measured, and
logs the retry once.

## Where to look when you are stuck

* **the ADR** for the subsystem you are in — read it first, it is cheaper than the
  bug
* **`docs/testing.md`** — the scenario index names the test that already covers
  most "how do I test X" questions
* **the module header comment** — every module in this crate opens with why it
  exists, and several have the "three rules a later editor is most likely to break"
  list at the top (`services/board_poller.rs` does)
* **the spikes** — if you are about to make a performance claim, someone has
  probably already measured a neighbour of it
  ([spikes/README.md](../../spikes/README.md))
* **the improvement sweep** — if you found something wrong while reading and are
  wondering whether to fix it now: no, you file it
  ([the protocol](improvement-sweep.md), and `./scripts/sweep_check.py` to see
  what is already on the list)

**See also:** [docs/testing.md](../testing.md) ·
[configuration reference](configuration.md) ·
[ADR index](../README.md#decision-records)
