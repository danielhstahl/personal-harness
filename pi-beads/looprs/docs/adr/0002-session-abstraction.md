# ADR-0002: One subprocess + one event stream per terminal state

- **ID:** looprs-u9e
- **Status:** Accepted — 2026-10-03
- **Epic:** looprs-fkc (three terminal states: Beads loop, Pi session, plain Bash)
- **Decides for:** looprs-05j (router), looprs-ctn (Pi chat), looprs-msj (provenance bug),
  looprs-553 (Bash shell), looprs-5g7 (Esc), looprs-ecr (shutdown), looprs-guh (status row),
  looprs-afw (viewport)
- **Related:** ADR-0001 (Bash gets a real pty)

## Decision

A **terminal state is a `Session`**: at most one child process, plus exactly one event stream
about that process. A **`Router` actor owns every session** and is the only code that talks to
them; **`App` only renders**. Every event is stamped with the **`SessionId`** that produced it —
origin is data, never inference. **`Tab` changes what you are looking at; it never destroys work.**

The types live in `src/session/`: `mod.rs` (ids, envelope, trait, policy), `router.rs` (owner),
`view.rs` (per-session scrollback), `stubs.rs` (the three backends). They compile and are tested;
**no behavior is implemented** — that is tickets 3–8.

## Q1. The session interface

```rust
pub trait Session: Send {
    fn id(&self) -> SessionId;
    fn mode(&self) -> TerminalType { self.id().mode }
    fn send_text(&mut self, text: String) -> anyhow::Result<()>; // queued, NOT finished
    fn abort(&mut self) -> anyhow::Result<()>;                   // Esc; idle => no-op
    fn shutdown(&mut self) -> anyhow::Result<()>;               // close stdin, then kill
    fn set_active(&mut self, _active: bool) -> anyhow::Result<()> { Ok(()) }
    fn status(&self) -> SessionStatus;  // NotStarted | Idle | Running | Aborting | Dead
    fn is_running(&self) -> bool { self.status().is_busy() }
}

pub fn spawn(mode: TerminalType, cfg: &SessionConfig, generation: u64) -> anyhow::Result<Spawned>;
// Spawned { session: Box<dyn Session>, events: UnboundedReceiver<SessionEvent> }
```

Four changes from the ticket's draft, each with a reason:

1. **`UnboundedReceiver<SessionEvent>`, not `…<Msg>`.** `Msg` carries `Tick` and `Term(Event)` —
   UI concerns a backend must not see. Sessions emit `SessionEvent`; one function
   (`router::wrap`) makes `Msg`. One translation point turns "forgot the origin" into a compile
   error instead of a 3 a.m. bug.
2. **`generation` in the id.** `spawn(mode, …)` cannot express *which* Pi session.
   `SessionId { mode, generation }` makes a respawn a different *thing* from the incarnation it
   replaced, which Q2's staleness rule needs. (Named `generation` because `gen` is a reserved
   keyword in edition 2024.)
3. **Sync methods, no `async` in the trait.** Not just to dodge `async-trait`: a
   `Box<dyn Session>` is a **handle onto a session running in its own task**, so `send_text` is
   honestly non-blocking — it queues, the task does. The consequence that matters is that **the
   Router never blocks on a child.** If the router `await`ed a prompt round-trip, `Esc` would
   queue behind 30 seconds of somebody else's model call and cancellation would be a lie.
   `shutdown()` likewise returns at once and reports completion as `SessionEvent::Exited`.
4. **`status()` next to `is_running()`, plus `set_active()`.** The status row (looprs-guh) must
   distinguish "never started" / "warm, idle" / "running" / "aborting"; input gating needs
   `is_busy()`. `set_active` carries the Q3 rule that the beads loop must not start new passes
   while it is off-screen.

`SessionConfig { pi_bin, bd_bin, shell_bin }` replaces `BeadsLoopConfig` (same env overrides:
`LOOPRS_PI_BIN`, `LOOPRS_BD_BIN`, `LOOPRS_SHELL_BIN` falling back to `$SHELL`), so one injected
config drives fakes for all three backends.

## Q2. The event envelope

**Input mode is authoritative for *intent* (where my keystrokes go). The envelope is authoritative
for *origin* (who made this). Never mix them.** The App must never read `input.mode` in event
handling — that is the root cause looprs-msj describes.

```rust
pub enum Msg {
    Term(Event),                                                    // the user; no origin needed
    Agent       { session: SessionId, event: PiEvent },
    BashOutput  { session: SessionId, stream: ByteStream, chunk: String },
    BeadStep    { session: SessionId, step: BeadStep },
    SessionDown { session: SessionId, reason: ExitReason },
    Error       { session: Option<SessionId>, text: String },       // None => harness-level
    System      { session: Option<SessionId>, text: String },
    Tick,
}
```

* `Msg::Agent(PiEvent)` → `Msg::Agent { session, event }`. A Pi session settling can no longer
  be mistaken for a beads worker settling, because the message says who made it.
* `router::wrap(SessionId, SessionEvent) -> Msg` is the **only** constructor of the session-tagged
  variants: you cannot build one without naming a session. (`Error`/`System` with `session: None`
  are the router's own harness-level notices.)
* **`chunk`, not `line`** (the draft said `line`): a pty reader hands back buffer-sized reads, and
  re-splitting on `\n` would rewrite the child's own framing, which ADR-0001 rule 1 forbids.
  `ByteStream::Merged` because a pty *has* one stream — stdout/stderr are indistinguishable
  there, so `Stdout`/`Stderr` exist only for genuinely separate pipes and nobody is invited to
  fake a split.
* **Staleness is structural, not filtered.** Killing a session is `shutdown()` **and**
  `pump.abort()`: the per-session forwarding task dies, so a dead incarnation has *no path* to
  the UI. The generation is what lets the router tell `Pi#1`'s late event from `Pi#2`'s, which
  is what makes looprs-05j's "no late event from the dead session is applied" testable rather
  than aspirational.
* **Exactly one `SessionDown` per session, guaranteed.** If a stream ends without an exit report,
  the pump synthesizes `ExitReason::Unknown`. That guarantee is what makes Q5's
  "always seal on death" unconditional.

## Q3. Lifecycle on mode switch

**A `Tab` is a view change, not a Cancel.** No mode switch kills a child. Only `Esc` (cancel the
work, keep the session) and app shutdown (kill it) do.

| | **Beads** | **Pi** | **Bash** |
|---|---|---|---|
| child identity | one **fresh pi per pass** (workers are deliberately stateless) | **one persistent** `pi --mode rpc` | **one persistent** shell on a pty (ADR-0001) |
| process starts | lazily, and only when `bd ready` has something | first submit in Pi mode | first submit in Bash mode |
| Tab away, **idle** | no child exists (the pass boundary reaped it); loop state kept | **kept warm** | **kept warm** |
| Tab away, **busy** | **drain then park**: the running pass finishes; **no new pass starts while hidden** | **keeps running**; the answer completes and buffers into the Pi view | **keeps running**; the command is the user's; output buffers |
| Tab back | resume **only from the parked state** — never double-spawn | same process, zero respawn latency | same process; cwd/env/jobs intact |
| `Esc` | abort the pi run **and park**; an aborted worker must never read as `agent_settled` → next bead | `clear_queue` → `abort`; put the returned text back in the box | `0x03` to the pty: interrupts the command, spares the shell |
| App quits | `shutdown()`: close stdin → grace → SIGKILL | same | same |

Why, cell by cell:

* **Pi warm** — the mode's entire value is that turn 2 knows turn 1, and that lives in the child's
  memory.
* **Bash warm** — a killed shell loses cwd, exports, aliases, functions and background jobs:
  precisely what ADR-0001 exists to preserve. Killing it on a Tab discards the reason for the mode.
* **Beads drains rather than dies** — killing a worker mid-run throws away paid-for work and leaves
  the ticket half-done, then re-claims and redoes it on return. The loop's own pass boundary is
  where reaping belongs, and already is.
* **Beads parks rather than keeps going** — the one place we stop. The loop self-advances, so
  "keep running while nobody looks" would silently mean "keep spending money on the board".
  Matches the stated prior ("beads wants cold") without destroying in-flight work. Want unattended
  draining later? Change one function, add a pass budget; the types do not move.
* The policy is a **value** — `TerminalType::switch_away_policy()` → `KeepRunning` (Bash, Pi) /
  `DrainThenPark` (Beads) — so reversing a cell later is not a rewrite.

**Children alive: one per terminal type, up to three in total.** The `HashMap<TerminalType, _>`
enforces "one per type" structurally; there is deliberately **no global one-child rule**, because a
global rule can only be implemented by killing something and every kill in the table costs the user
something real. Honest cost: two Node processes can be resident at once (warm Pi + a beads worker),
both possibly pointed at the same repo. That is only dangerous when invisible, which makes
**looprs-guh's status row load-bearing, not cosmetic**: it must show per-mode liveness so "beads
is still working while you chat" is never a surprise. `Esc` in the beads view is the control.

Not adopted: `pi --session-id <id>` (pi `docs/cli.md`) would let Pi mode be cold-killed and
resumed from disk, trading the resident process for startup latency plus a second source of truth
for "what was this conversation". Warm is simpler and instant.

**The `Esc` row is a summary; the decision behind it is ADR-0003.** What each mode does when the
thing it cancelled *refuses* to stop, why Esc never quits, why the grace is three seconds, and
why Bash is the one mode that must not kill — all of that lives in
[ADR-0003](0003-cancellation.md). This table says where the mechanism goes; that one says what
happens when the mechanism does not work.

## Q4. Ownership and channel topology

**The `Router` actor owns every session. `App` renders.**

```text
  crossterm EventStream ──> run() loop ──keys──> App.input ──UiCommand──> cmd_tx (mpsc, 16)
                                       │                                        │
   app_tx (unbounded Msg) <────────────┘                                        │
       │   ┌──────────────────── Router task (sole owner) ───────────────────────┘
       │   │  sessions: HashMap<TerminalType, Managed{ id, Box<dyn Session>, pump }>
       │   │
       │   │   pump(id, rx) ── SessionEvent ──wrap(id, ·)──> Msg ──┐
       │   │   pump(id, rx) ── SessionEvent ──wrap(id, ·)──> Msg ──┼──> app_tx
       │   │   pump(id, rx) ── SessionEvent ──wrap(id, ·)──> Msg ──┘
       │   └───────────────────────────────────────────────────────┘
       ▼
  run() loop: Msg → App.update(view state) → flush → draw (band | cards | status | input)
```

* **Only the Router task touches a `dyn Session`** — one owner at a time, so no mutex and no torn
  lifecycle. The router serializes commands, so "Submit then Tab" vs "Tab then Submit" has one
  answer instead of a race.
* **Only the `run()` loop touches `App`**, and only in response to a `Msg`.
* **Sessions know only `SessionEvent`**; `wrap` is where the UI tag appears.
* **One pump task per session** (the generalization of today's `forward_pi_events`) tags and
  forwards, so the router never has to select over a growing set of streams.
* Commands keep the existing `cmd_tx` (cap 16). `UiCommand` becomes
  `Submit { mode, text }` (replacing the `UserMessage` / `UserBeadMessage` pair),
  `SwitchMode { from, to }`, `Cancel`. ~~plus the legacy `BeadsNext` that
  looprs-msj deletes~~ — deleted, as of msj; see its amendment at the end.

Rejected: **`App` owns the session map.** `App` is driven by the draw loop and must stay cheap
enough for ~60fps; session control is nothing but awaits (spawn, prompt round-trip, close stdin,
reap). Same task ⇒ a slow child stalls the screen and `Esc` queues behind it. Same struct ⇒ the
render path can reach into a child process.

## Q5. Transcript model

**Per session**, in a type that pairs the halves:
`SessionView { session, transcript, flusher: private, status, last_error }`, with
`App: HashMap<TerminalType, SessionView>`. **Events are applied to the view that *owns* the
session (`Msg::*{ session }`); rendering selects the view that is *active*.** Those two selections
being independent is the fix for the whole "I tabbed away and the other session wrecked this view"
class.

Invariants, all still load-bearing. The surface they protect moved from the terminal's scrollback to the frame's transcript band in `pdl.4`; the amendment at the end of this file says which words to substitute where:

1. **A `Flusher` is a cursor into one specific `Transcript`.** `SessionView` creates both and
   keeps `flusher` private, so a mismatched pairing is not expressible. The API is
   `view.flush(width)` / `view.preview(width)`.
2. **The per-frame sequence is unchanged**: `flush()` → `insert_before()` → `draw()`, for the
   active view only. "Each finalized line reaches the scrollback exactly once" holds per pair, and
   pairs do not interfere (`views_do_not_interfere`).
3. **Death must seal.** On `SessionDown` (exactly once, per Q2) the App calls `view.seal()` =
   `finish_last()`. Miss it and the flusher stalls forever on an entry that can never become
   `done`: nothing that session produced reaches scrollback again and the preview spins over dead
   text. This is *the* seam, so it is contractual —
   `an_unsealed_entry_stalls_the_flusher_and_seal_clears_it` pins it.
4. **Inactive views buffer.** They are flushed only when active; the backlog then goes out as one
   `insert_before` burst, which is already the normal shape of a burst.

Rejected: **one shared transcript with provenance-tagged entries.** Simpler-looking, not actually:
Bash output must never enter the markdown path while Pi output must, so every renderer and
`Flusher::preview` (which re-parses the open block) would branch on entry provenance *throughout*
— the same "infer what this was from context, downstream" mistake this ADR removes. And the live
preview is single-occupancy by construction (`VIEWPORT_H`, looprs-afw), so a shared transcript
would preview whichever session streamed last rather than the mode you are in: worse than
interleaved, *wrong*.

## Skeleton map — who fills in what

| File | Contains | State | Filled by |
|---|---|---|---|
| `session/mod.rs` | `TerminalType` (moved here from `components/input`), `SessionId`, `ByteStream`, `SessionStatus`, `ExitReason`, `SessionEvent`, `BeadStep` (moved here), `SwitchAway` + `switch_away_policy()`, `SessionConfig`, `Session` trait, `spawn()` | **done — this is the contract** | — |
| `session/router.rs` | `Router`, `Managed`, `wrap()` and `pump()` (implemented), `handle()` / `switch_to()` / `shutdown_all()` | stubs `Err(… not implemented)` | looprs-05j, looprs-ecr |
| `session/view.rs` | `SessionView`: `flush`, `preview`, `seal`, `push_note`, `push_error` | **done**; adopting it in `App`/`main` is the wiring | looprs-05j, looprs-afw |
| `session/stubs.rs` | `BashSession` | every operation refuses, naming its ticket | 553 |
| `session/pi_chat.rs` | `PiChatSession` — one persistent `pi --mode rpc` child, steer-while-running, Esc = `clear_queue`+`abort`+restore, respawn after a dead child | **done — looprs-ctn** | — |
| `src/app.rs` | `Msg` envelope applied; `UiCommand::Submit`/`SwitchMode`; `BeadsLoop` stamps its `SessionId` | **done** — `on_pi` renders and decides nothing (msj) | — |
| `components/input.rs` | `Tab` now emits `SwitchMode` | wired | 05j |

Stubs return `Err`, never a plausible no-op: a stub that silently succeeds lets a wiring ticket
pass a test that exercised nothing.

## Reconciliation (tickets that contradicted this ADR)

* **looprs-05j** — *"Tab from Beads to Pi while a worker run is streaming: the pi child is dead
  within one frame"*, *"exactly one pi child at any time"*. **Contradicted** by Q3: a Tab does not
  kill the beads worker, and warm Pi/Bash persist. Replaced with the exact invariant: ≤1 child per
  terminal type; beads drain-then-park on switch-away; no post-kill event can reach the UI
  (`pump.abort()`); switching back never double-spawns.
* **looprs-ctn** — *"zero [pi children] after switching away"*. **Contradicted** by warm-Pi.
  Updated to: exactly one Pi child once the mode has been used, idle while hidden, never killed by
  a switch. "Spawn once, reuse forever" and "respawn on next input after a crash" stand.
* **looprs-msj** — **confirmed, unchanged**; this ADR is what unblocks it. `Msg::Agent { session }`
  now exists, and the fix is exactly "drive the beads machine off `session` + the beads session's
  own `BeadStep`". `App::on_pi` keeps its bug, with a comment marking the line, so msj owns it.
  *(Landed: the bug is gone. `App::on_pi` renders; the transition lives in `BeadsSession`. See the
  msj amendment below.)*
* **looprs-553** — gets `Msg::BashOutput { session, stream: Merged, chunk }`: chunk not line, one
  merged stream (Q2).
* **looprs-5g7 / looprs-ecr / looprs-guh** — consistent with the interfaces they need
  (`abort()`, `shutdown()` + guaranteed `SessionDown`, `SessionStatus` / `live_modes()`).
  Esc-per-mode is in the Q3 table; grace/escalation numbers stay ecr's; **guh is now required**,
  because the warm-child policy is only safe while liveness is visible.

## Consequences

Good:

* Provenance is a type, not a habit: `Msg::Agent` with no origin does not compile.
* Mode switching is a policy value, not a rewrite — reverse any Q3 cell in one function.
* Cancellation stays responsive: the router never awaits a child.
* The repo's process-level test style survives: fakes behind `SessionConfig` still assert "was a
  child spawned, was it prompted, was the previous one reaped" rather than trusting a mock.
* One obvious home for the next six tickets, each stub naming its owner.

Costs / risks:

* **Up to three resident children**, two of them Node. Memory is the price of not destroying work.
  Mitigation is looprs-guh (see above) — a policy whose failure mode is "invisible background
  process" must ship with the row that shows it.
* **Buffered output while hidden is unbounded**: a `yes | sleep 1000000`-style shell, or a chatty
  beads worker, grows its transcript forever. **looprs-553 and looprs-05j must cap per-view
  buffered bytes** and insert an explicit `… N bytes dropped …` notice. Unbounded scrollback is a
  memory leak with good manners.
* **Two pi processes can touch one repo concurrently** (warm Pi chat + beads worker). Deliberate,
  and only a problem if hidden — again the status row, plus Esc-in-beads as the control.
* `set_active` is one hook with one real consumer. It must not grow into a general
  "notify everything" bus; if a second consumer appears, design that interface then.
* Sync methods mean callers cannot await teardown; they must watch for the `Exited` edge. Only safe
  because the pump guarantees exactly one per session.
* `TerminalType`/`BeadStep` moved out of `components/input.rs` and `app.rs` into the session layer.
  Re-exported where they were used, but any code outside this repo's current tree that imported them
  from the old paths needs the new import.
* The variant is spelled `Beads`; this ADR calls the mode "Beads". Renaming it was deliberately left
  out to keep this ticket type-only.

## Amendments made while implementing looprs-05j (the router)

Three things this ADR did not anticipate. Each is small, but the contract above is the
contract, so they are recorded rather than slipped in.

1. **`Session::advance()` was added** — the routing target for the legacy
   `UiCommand::BeadsNext`. The Router routes it by *mode* (`Beads`), never by the
   active mode, because the settled event that triggered it named the beads session.
   It is a wart on a generic trait and it should die with looprs-msj, which moves the
   step machine inside `BeadsSession` where the decision belongs. Default is a no-op
   so only the beads session answers it.
   *(Dead, both of them, as of msj — see below. The Router no longer has any
   beads-specific command to route.)*
2. **The per-view buffer cap is entry-granular, and the notice is inserted at the
   render cursor, not at the head of the transcript.** Doing it the obvious way
   (prepend, then shift) either re-emits lines the terminal already has or never
   emits the notice at all. `Flusher::consumed()` and `Flusher::reseat()` exist to
   make this safe: any cursor move must reset the per-entry cursor state, or
   `drain_stream` slices the wrong text. The cap reserves room for its own notice
   (`NOTICE_BUDGET`), otherwise the view lands at `limit + notice` and the cap is a
   rounding error with an apology attached.
3. **`SessionFactory` is injected.** The lifecycle rules — park, resume, one per
   mode, "a replaced generation has no route to the UI" — are worth testing, and
   testing them through three half-built child processes would test the fakes rather
   than the Router. `default_factory(cfg)` is the real thing; tests hand the router
   sessions whose behavior they control, and the beads backend keeps the repo's
   process-level style (real pids, real reaping) in `session::beads::tests`.

Also: `BeadsLoop` moved out of `app.rs` into `session/beads.rs` and now reports
`SessionEvent` instead of `Msg`, which is what lets the Router own it; `ChatState`
moved into `session::view` because it describes one session's live region, not the
app's; and `App` holds `HashMap<TerminalType, SessionView>` with `need_input` /
`chat_state` derived from the active view, as Q5 required.

## Amendments made while implementing looprs-ctn (the Pi chat session)

1. **`SessionEvent::RestoreInput { text }` and `Msg::RestoreInput { session, text }
   were added.** pi's interactive `Esc` recipe is `clear_queue` **then** `abort`,
   and the text `clear_queue` returns belongs in the user's input box — which is UI
   state a session has no handle on. A session cannot write the box, so the round
   trip is an event like every other one it makes. It carries the `SessionId` for
   the usual reason: a Pi cancel must not be able to type into the Beads box. Two
   rules the App applies (`App::restore_input`): the box is filled only while that
   session's mode is the one on screen (keyed on `active`, never on `input.mode`,
   so "event handling never reads the input mode" still holds), and a restore never
   overwrites text the user typed in the meantime — the restore is asynchronous and
   their keystrokes are newer. The displaced text goes to the transcript rather than
   into the void, because silently dropping it is the failure the recipe exists to
   prevent. The single-line box folds newlines to spaces; the message boundaries are
   not lost, they are in the transcript.

2. **The "follow-up while running" question the ticket left open is answered: input
   is not gated, and the follow-up goes in as `steer`.** Gating the box during a Pi
   run would make the mode that exists to be *talked to* the one that will not take
   feedback. pi rejects a plain `prompt` while streaming unless a
   `streamingBehavior` is named, so `send_text` routes on the session's own running
   flag — `steer` mid-run, `prompt` otherwise — and if a `steer` turns out to have
   raced the end of its run it is re-sent as a fresh prompt rather than dropped.
   Two send attempts, never more, so a pi that will not take it cannot turn one
   Enter into a resend loop. (Verified against real `pi --mode rpc`: `steer`/`clear_queue`
   responses carry `data.disposition` and `data.steering`/`data.followUp` exactly
   as assumed, and `abort` on an idle session answers immediately and harmlessly.)

3. **A Pi child's death is reported by whichever path notices it first, and the
   session then respawns itself under the same `SessionId`.** The two paths are the
   stream end (the forwarder's `StreamEnd`, tagged with the child's serial so a
   notice from an already-replaced child is ignored) and a failed send (the corpse
   is discovered by the write, or by `try_wait` on a refusal). Both emit exactly
   one `SessionEvent::Exited`, so the App's "always seal" rule fires promptly
   either way. This does bend one sentence of Q2 — "the last thing that session
   reports" — because a respawned Pi session keeps its id and keeps talking after
   that `Exited`. It is still safe, and the reason is the serial: the dead child's
   forwarder cannot clobber the live one, the App seals on the death it was told
   about, and if the Router *does* bump the generation (its status mirror said
   `Dead` before the session's own respawn) the adopt-a-new-generation rule in
   `view_mut` covers that path instead. The alternative — making the send path
   refuse until the Router noticed — trades a delivered message for a purer
   sentence.


## Amendments made while implementing looprs-msj (provenance for loop transitions)

The bug this ticket filed was Q2 being broken in one specific place: `App::on_pi`
answered "does this settle mean *take the next bead*?" from `session.mode == Beads`,
and `session.mode` was standing in for a question only the beads session can answer.
Everything below is that correction, plus three things it turned out to require.

1. **`UiCommand::BeadsNext` and `Session::advance()` are deleted.** Not
   deprecated, not routed to a no-op — gone, so that "the UI asks the beads loop to
   go again" is not expressible again. The beads loop is now driven by
   `BeadsCmd::WorkerSettled`, produced *inside* `BeadsSession` by the forwarder on
   its own worker's stdout (`BeadsLoop::forward_records`). The App renders a settle
   (`view.chat = Stopped`) and sends nothing for it. `apply_pi` no longer returns a
   command, which is the compile-time form of "the UI is out of this decision". The
   App-side test is therefore the blunt one: handling a settle emits **no**
   `UiCommand` at all, whichever session made it and whichever mode the box is in —
   plus a source check that event handling never reads the input mode.

2. **A worker pass gets its own serial, one level below `SessionId`.** This ADR made
   generations mandatory because a bare `TerminalType` cannot tell a live session
   from its corpse; the same argument applies inside the loop, because
   `BeadsLoop::close()` kills a worker whose last records are already in flight. An
   untagged late `agent_settled` reads as "*my* pass finished", retires the live
   pass's `streaming` flag, and starts a third pass over the top of a second one —
   killing work that was already paid for. So every worker is tagged at spawn, and
   the loop only advances on a settle whose serial it is currently holding. Two
   consequences worth stating: a settle with no pass behind it starts nothing, and a
   duplicate from a retired pass is a no-op.

3. **Esc-in-beads is implemented to the point where the Q3 rule is enforceable.**
   The table already said "abort the pi run **and** park; an aborted worker must
   never read as `agent_settled` → next bead" — but that rule is unenforceable
   without state, because the settle looks identical either way. `BeadsTask::aborted`
   is that state: set before the abort goes out, so the unwinding settle parks the
   loop instead of claiming the next bead. **looprs-5g7 still owns the polish** (the
   grace/escalation numbers, the wording, the spinner, and Bash's `0x03`); what
   landed here is the minimum needed so that a settle cannot be misread.

4. **A worker whose stream ends without settling parks the loop with an error.**
   Once the settle was the only thing that drove the loop, the stream end became the
   symmetric edge, and it was unhandled: a worker that crashed or exited early left
   the loop claiming `WorkTickets` forever — which, via `SessionView::set_step`,
   means the input box stayed hidden behind a claim that nothing would ever
   discharge. Note that Q2's exactly-one-`SessionDown` guarantee does *not* cover
   this: that promise is about the **session**, and here the session outlives its
   worker by design (fresh child per pass, Q3). The loop has to notice its own
   worker's going, and a park is the right answer rather than an auto-retry — a
   crash that auto-restarts is a respawn storm with a bill attached.

**Test seams added**, all mirrors or polling, none of them production behavior:
`BeadsSession::in_flight()` (the live pass serial, published the same way `status`
is, so a test can settle against a serial it did not invent),
`Fakes::wait_for_pi_spawns`, and the settle-path tests run on `PiFake::Chat`, so
the settle under test is a real `agent_settled` off a real child process rather
than a hand-built event. Incidentally: both sessions' `quiesce()` now publish their
status mirror *before* acking, so "waited on the seam, then read the status" means
what it says on a multi-threaded runtime.

## Amendments made while implementing looprs-ecr (the exit path)

This ADR said what a session owes on quit (`shutdown()` → close stdin → grace → SIGKILL) and left
the **order of the exit itself** to looprs-ecr. It landed as six steps in `run()`, and three of
them are amendments to what this file said or implied.

1. **`UiCommand::Quit` exists, and dropping the channel is only the fallback.** Q4's topology has
   the App holding `cmd_tx`, so "close the channel" was the natural exit signal — the Router
   treats it as exactly that, still. But closing it closes the *reader* too, and the sessions do
   most of their talking after being told to leave: the tail of a streamed answer, and the
   `SessionEvent::Exited` that Q2 promises and that `SessionView::seal()` depends on. A quit that
   drops the App loses all of it in the pane. So the UI now *sends* `Quit`, keeps the App alive,
   and drains `app_rx` until every sender is gone — bounded, so a child that will not stop
   talking cannot hold the screen.

2. **The run loop holds no `app_tx` at all.** Q4's diagram shows the App as the source of `Msg`,
   and it is, via the Router's clones. A copy left in `run()` is a sender that never dies, which
   silently turns "drain until the senders are gone" into "drain until the timeout". Measured:
   every quit cost 2.5s of frozen screen for nothing; with the stray sender dropped the same quit
   takes 0.33s. The invariant worth keeping is *n senders, and every one of them is a task that
   can finish*.

3. **`shutdown_all` shares one deadline across the set and `abort()`s past it.** The grace per
   session was three grace periods laid end to end — paid in full, in series, by the worst run
   there is. One shared deadline makes the worst case the same as the single-session case. And
   past the deadline the pump is cut rather than abandoned: an abandoned pump keeps its `app_tx`
   clone (see point 2, again) and its receiver on the session's stream, so "abandon" would have
   quietly unbounded the thing it was bounding.

Also worth writing down, because it is the part that bites anyone who touches this next: **the exit
path must never ask the terminal a question.** `Terminal::clear()` opens with a cursor query
(`ESC[6n`), and at exit the async key stream's reader thread is still parked on the same stdin
and eats the answer — which is how every run of this app used to end, in `Error: The cursor
position could not be read within a normal duration`, exit code 1, with the live pane still
painted. The clear is done from the anchor `LiveView` publishes instead, which is also why
`insert_before` has to move that anchor with the pane: erase from the row the pane occupied
*before* the last insert and the erase deletes the lines that insert just wrote.

---

## Amendment — the frame replaced the pane (`looprs-pdl.4`)

Two of the words used above are no longer live. The **invariants** are.

* **`insert_before` is gone.** Finalized lines reach the screen as the **transcript band**:
  the tail of `SessionView::display` plus the live preview, drawn into the top band of the
  full-screen frame (`viewport::frame_areas`). Read "scrollback" as "the transcript band"
  wherever a sentence above is about where a line *lands*. Invariant 1's "one flusher per
  one transcript" is now enforced against `display`, which only `flush` writes, so "each
  finalized line reaches the band exactly once" is the same property with a new owner — and
  a new reason it cannot leak across modes, since the band reads the active view and nothing
  else.
* **`LiveView` / `LiveAnchor` are gone**, and with them the anchor half of the last paragraph
  above. The rule that paragraph was teaching — *the exit path must never ask the terminal a
  question* — is not merely intact, it is structural: the frame never queries the cursor on
  any path, and a test backend that counts `get_cursor_position` calls fails the suite if it
  ever does (`viewport::tests::the_frame_never_asks_the_terminal_where_the_cursor_is`).
  There is therefore no query for the key stream to eat, at exit or anywhere else. The
  hand-back is `?1049l`, one byte that restores the user's main screen and their cursor
  (ADR-0006).
* **The per-frame sequence is now** `flush()` → take the preview and the input band →
  `draw()`, for the active view only, and the reason for the order is the reason invariant 2
  gave: the width the lines were wrapped at, the row count that sized a band, and the pixels
  that fill it must be one number.
* **"Inactive views buffer, then go out as one burst"** still holds. The burst is now a band
  that jumps rather than a pane that pushes, which is what makes switching modes one repaint
  of one band instead of a reprint above the pane — and is why ADR-0002's cross-mode class of
  bug stays closed without the ordering dance the pane needed.
