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
  run() loop: Msg → App.update(view state) → flush → insert_before → draw
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
  `SwitchMode { from, to }`, `Cancel`, plus the legacy `BeadsNext` that looprs-msj deletes.

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

Invariants, all load-bearing for the existing inline `insert_before` behavior:

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
| `session/stubs.rs` | `BeadsSession`, `PiChatSession`, `BashSession` | every operation refuses, naming its ticket | msj / ctn / 553 |
| `src/app.rs` | `Msg` envelope applied; `UiCommand::Submit`/`SwitchMode`; `BeadsLoop` stamps its `SessionId` | wired, behavior unchanged | msj (`on_pi`), 05j (per-view state) |
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
* The variant is spelled `Beeds`; this ADR calls the mode "Beads". Renaming it was deliberately left
  out to keep this ticket type-only.
