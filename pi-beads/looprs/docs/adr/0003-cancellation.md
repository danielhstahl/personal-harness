# ADR-0003: Cancellation — what Esc means in each terminal state

- **ID:** looprs-5g7
- **Status:** Accepted — 2026-10-04
- **Epic:** looprs-fkc (three terminal states: Beads loop, Pi session, plain Bash)
- **Decides for:** `Session::abort` in all three backends (`beads`, `pi_chat`, `bash`), the
  `SessionStatus::Aborting` row of looprs-guh's status display, and the `Esc` binding in
  `components::input`
- **Related:** ADR-0001 (Bash gets a real pty; rule 7 fixes Ctrl-C / Ctrl-Q), ADR-0002
  (Q3 lifecycle table, `abort()` in the trait, "a `Tab` is not a `Cancel`")

## Context

The keystroke existed and went nowhere: `KeyCode::Esc` → `InputAction::Cancel` →
`UiCommand::Cancel` → dropped. `PiRpc::abort()` had been written long before anything could
call it. Esc had to become a real control in all three modes, with four properties the ticket
named:

1. **Fast.** Esc during a long tool call stops it in under a second, app stays usable.
2. **Non-advancing.** Esc during a beads worker must **not** read as `agent_settled` → next
   bead. A cancel that starts the next billable pass is worse than no cancel at all.
3. **Invisible when idle.** Esc with nothing running does nothing — not a quit, not an error,
   not a message.
4. **Never silent.** The user must not be left staring at a screen that has not answered a key
   they already pressed, while a tool takes its time unwinding.

Two questions had no answer in the existing ADRs and had to be decided rather than improvised in
code: whether Esc ever quits, and what happens when the thing being cancelled refuses to stop.

## Decision

**Esc cancels. It never quits, in any mode, at any time.**

| | **Beads** | **Pi** | **Bash** |
|---|---|---|---|
| mechanism | `abort` the pass's `pi` child, set `aborted`, park on the settle that follows | `clear_queue` → `abort` (pi's documented interactive-Esc recipe), hand the queued text back to the input box | `0x03` to the pty master → SIGINT to the child's foreground process group |
| idle | silent no-op | silent no-op (not even a `clear_queue` round trip) | silent no-op (no stray `^C` on the prompt) |
| acknowledged | `cancelling \`looprs-xyz\`…` | `cancelling the Pi run…` | ``cancelling `sleep 25`…`` |
| done | `cancelled` + "the loop is parked" | `cancelled` | `interrupted (exit 130)` |
| **refuses to unwind** (after `cancel::GRACE` = 3 s) | **kill the worker**, park, name the bead that stays claimed | **kill the child**, report `ExitReason::Cancelled`; next message cold-starts a replacement | **say so and hand the choice back** — never kill the shell |
| next Esc while cancelling | dropped while the attempt is live; retried once the stall has been reported | same | same |

The four answers are the contract, and `src/session/cancel.rs` holds its vocabulary and its one
number so that three backends cannot drift into three dialects.

### Why Esc never quits

Double-Esc-to-quit was considered and **not adopted**. The reasons:

* Esc has exactly one meaning elsewhere in this app, and it is not "leave". A mode where the
  same key means "stop that" nine times out of ten and "destroy the session" on the tenth is a
  keybinding that trains the user to be afraid of it — in a program whose whole job is to keep
  three live processes running.
* The property double-Esc is usually bought for ("I want out") already has an unambiguous owner:
  **Ctrl-Q quits in every mode** (ADR-0001 rule 7), and was added precisely because Ctrl-C had
  been handed to the shell.
* The modes disagree about what "quit" would even cost: a warm Pi child's conversation, a
  shell's cwd/env/jobs, a beads loop half-way through a pass. A quit chord must mean one thing
  per app, not one thing per mode.

**Ctrl-C semantics are unchanged by this ticket.** They were decided in ADR-0001 rule 7 and stay
as written: in Bash mode Ctrl-C *is* the shell's (forwarded, never quits); elsewhere it still
quits looprs. Making Ctrl-C a cancel in Beads and Pi too is a tempting symmetry and is
explicitly **not** decided here — it is a change to a quit chord, and quit chords are how a user
escapes a stuck UI. Do it with a measurement of people getting stuck, not as a side effect of
tidying a keymap.

### Why the grace is 3 seconds

The acceptance bar is that a *responsive* child stops in under a second. A cancel that has not
landed by three seconds is therefore not "slow", it is "not going to" — and a shorter grace
would escalate on tools that were still cleaning up after themselves, which is the false alarm
that would make the message worthless. One constant, in one file, firing all three ladders.

The deadline is not a wait. It is a **command posted back into the session's own mailbox**
(`cancel::arm`), which is what keeps ADR-0002's "the Router never blocks on a child" true for
the timeout as much as for the cancel itself. Consequence: every stall handler must check that
the deadline is still *its* own — an attempt counter plus the pass serial — exactly like a stale
`SessionId` in the router. An untagged timer firing three seconds later against a pass nobody
cancelled would be a bug with "random work dies" as its symptom.

### Why each mode escalates differently

* **Beads kills.** The stalled thing is a `pi` worker the user just refused to spend another
  token on, and the mode is cold between passes by policy (ADR-0002 Q3), so nothing precious
  dies. The kill also has to happen *here*: a parked beads loop sitting on a live child is the
  idle-child trap this loop was rewritten to avoid, and no later pass is coming to reap it.
  What must survive is the breadcrumb — the bead stays **claimed**, so the escalation names it
  with the `bd show` that reads it back. That is the state looprs-w7q's claim/close guard has to
  be able to see, which is why it is said out loud rather than left implied.
* **Pi kills too, and says what is lost.** The context that dies with the child is the context
  of the very turn the user just cancelled, and the next message cold-starts a replacement
  through the same already-tested path that a crash uses. The alternative — holding
  `Aborting` forever with the transcript pinned open behind it — is a worse failure than a
  restart the user was warned about.
* **Bash does not kill, and must not.** The stalled thing is somebody else's process; the shell
  is the user's. Killing it to make the prompt come back would throw away exactly what
  ADR-0001 exists to preserve: cwd, exports, aliases, functions, background jobs. So the
  escalation is one honest sentence plus the choice: `Esc` sends another interrupt (a retry is
  allowed once the stall has been reported, rather than being swallowed by the pending state),
  `Ctrl-Q` quits and takes the shell along. `trap '' INT` is the reproducible case; it is the
  shape of every tool that will not be interrupted.

### What "non-advancing" costs, and why it is not negotiable

A beads worker cancelled mid-pass settles on the way out — that is how `pi` unwinds — and the
settle is *identical* to the one that means "pass finished, take the next bead". So the loop
carries an explicit `aborted` flag set **before** the abort is sent, and the settle is read as
a park, not a pass. Without the flag the rule cannot be enforced at all: there is nothing else
in the event that distinguishes them. Same trick, one level down, for the deadline: the abort
returns the pass's serial so the timer can only ever be answered against the pass that armed
it.

## Verification

Two layers, because they answer different questions.

* **In-process, real subprocesses** — `src/session/cancel.rs` plus the cancel tests in each
  backend: the ladder's ordering (`cancelling…` strictly before `cancelled`), the silent-idle
  rows, and every escalation path, driven by `Fakes::stubborn_pi` (a fake that answers `abort`
  and then keeps going — the shape of a tool that traps the cancel). 186 tests pass; the
  stubborn/`trap` cases cover the third row of the table, which cannot be reached any other way.
* **Real terminal, real pty** — `spikes/cancel_e2e.py`, 19 checks × 3 runs, all passing
  (`spikes/results/cancel-e2e.log`). It drives the built binary with `pi`/`bd` pointed at
  fakes (so no model call is spent) and Bash on the real shell, and measures the wall-clock
  latency from the Esc byte to the completion word reaching the screen:

  | check | result |
  |---|---|
  | beads: Esc acknowledged with the bead named | pass, < 1 s |
  | beads: keystroke → `cancelled` | pass, < 1 s |
  | beads: cancel did not start another bead (one prompt ever) | pass |
  | pi: keystroke → `cancelled` | pass, < 1 s |
  | bash: keystroke → `interrupted (exit …)` | pass, < 1 s |
  | bash: the same shell answers afterwards | pass |
  | idle Esc in all three modes says nothing | pass (quiet window each) |
  | Ctrl-Q quits; nothing from the run survives | pass |

  A fake `pi` cannot fake *arriving on screen*, which is the thing the in-process tests are
  blind to: `App.update` → `Flusher` → `insert_before` → the real terminal is where an
  acknowledgement would actually get lost, and where the mode's routing gets exercised.

## Costs / follow-ups

* The stall row is a *sentence*, not a progress bar. A `SessionStatus::Aborting` badge
  (looprs-guh) is the right place to make "still unwinding" visible continuously rather than
  once; the events are already emitted for it.
* Beads' `bd show` breadcrumb is prose, not a state transition. looprs-w7q turns it into a
  guard (never re-work an un-closed bead); the two meet at exactly this message, and neither
  depends on the other's wording beyond "the bead id appears".
* `GRACE` is a global constant by design. If one mode ever needs a different number, that is a
  signal the modes' cancel ladders have diverged and this ADR needs a second decision, not a
  per-mode override.
