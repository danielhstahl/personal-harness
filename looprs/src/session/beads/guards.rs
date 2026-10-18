//! The beads decision layer, kept pure (looprs-6ol).
//!
//! Nothing in here reads a process, a clock, or a mutable thing. Every
//! condition that decides *whether the beads loop may act* is a plain function
//! of plain arguments, which is what makes the money-safety guards testable as
//! tables rather than as trajectories — see the banner that was already written
//! over the pass gate below, and `tables` in [`tests`](super::tests) for the
//! enumeration it buys.
//!
//! The one exception is [`AutoResume`], and it is an exception in name only: it is
//! a *value* the loop owns, not a read it performs. Its whole state is a ticket id
//! and a counter, it is granted and spent through methods that take nothing from
//! outside, and the tables test the budget against are no bigger than the type.

use crate::session::BeadStep;

// ---------------------------------------------------------------------------------
// The decision layer, kept pure (looprs-6ol).
//
// Every condition that decides *whether the beads loop may act* is a plain function
// of plain arguments here, not an inline `if` inside an `async fn`. That is the
// whole point:
//
// * A condition inside an `async fn` can only be exercised by whatever state the
//   test's trajectory happens to reach, so "the guard holds when the mode is
//   hidden" ends up being a claim about that one test rather than about the guard.
//   A function of four bools can be enumerated, and enumeration is what turns "we
//   tested one path" into "the table has one yes-row and fifteen no-rows".
// * These guards are the money-safety story. One pass at a time; a claim before any
//   spawn; a cancel that cannot fire twice; a stall timer that cannot answer for a
//   pass it did not arm. Each is a handful of boolean operators, and each one being
//   wrong is a bill or a killed worker.
//
// The `async` half of this file stays what it always was — an interpreter that reads
// these answers and then does the thing.
// ---------------------------------------------------------------------------------

/// The four flags in front of [`BeadsLoop::next`], as a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PassGate {
    /// Has the beads mode ever been entered?
    pub(super) started: bool,
    /// Is the mode off-screen ([`SwitchAway::DrainThenPark`])?
    pub(super) parked: bool,
    /// Is a pass in flight that has not been reported as settled?
    pub(super) streaming: bool,
    /// Is a pass wanted?
    pub(super) pending: bool,
}

impl PassGate {
    /// May the loop start a pass right now?
    ///
    /// Exactly one of the sixteen rows says yes — the tests enumerate all of them.
    /// Each conjunct is a thing that used to go wrong: `started` (a mode nobody
    /// entered must not run), `!parked` (a hidden mode must not spend),
    /// `!streaming` (never two passes at once), `pending` (nothing was asked for).
    pub(super) const fn allows(self) -> bool {
        self.pending && self.started && !self.parked && !self.streaming
    }
}

/// Why the beads step machine moved.
///
/// The loop names a *cause* at every transition and the step is computed from it
/// ([`StepCause::step`]), so the table exists once instead of being re-implied at
/// six call sites. A caller cannot put the machine somewhere its own story does not
/// justify: there is no `set_step(anything)`, only `set_step(why)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StepCause {
    /// A human typed an instruction into the beads box, and the planner is
    /// starting. Deliberately the *only* cause that enters `CreateTickets`: a Tab,
    /// a settle, or a timer never plans on somebody's behalf.
    Planning,
    /// A bead was claimed and a worker is running it.
    Working,
    /// The loop is waiting for a human: parked, board empty, claim refused, plan
    /// unverifiable, child died, pass cancelled. Several stories, one step, because
    /// the human's next move is the same in all of them — type something.
    Awaiting,
}

impl StepCause {
    /// The transition function.
    ///
    /// Total and pure: a cause always has exactly one destination, and none of them
    /// depend on anything outside the argument (which is why the loop's own step is
    /// not an input — see the test that pins the canonical cycle
    /// `AwaitInput → CreateTickets → WorkTickets → AwaitInput`).
    pub const fn step(self) -> BeadStep {
        match self {
            StepCause::Planning => BeadStep::CreateTickets,
            StepCause::Working => BeadStep::WorkTickets,
            StepCause::Awaiting => BeadStep::AwaitInput,
        }
    }

    /// Every cause. Test-only on purpose: production gets its exhaustiveness from
    /// the `match` above, and the test needs the list to walk the table.
    #[cfg(test)]
    pub const ALL: [StepCause; 3] = [StepCause::Planning, StepCause::Working, StepCause::Awaiting];
}

/// How many automatic re-buys one ticket is owed after a pass that stopped talking
/// without closing it (workspace-9th).
///
/// A number rather than a bool, because the two failure modes on either side of it
/// are both expensive and only a bound separates them:
///
/// * Too little (zero, i.e. the old hard stop): one transient `pi` failure — a
///   provider error, a run that ran out of context mid-sentence, an agent that did
///   the work and forgot to type `bd close` — parks the whole board until a human
///   notices, which is what the loop was never supposed to need.
/// * Too much (no bound): looprs-w7q's runaway comes straight back. A ticket that
///   *cannot* be closed would be bought again and again, and every pass is billed.
///
/// So: two automatic re-buys per ticket per human instruction — enough to ride out a
/// hiccup twice, small enough that the worst case is three passes on one ticket
/// rather than an evening of them. One knob, and the tests name it.
pub(super) const MAX_AUTO_RESUME: u32 = 2;

/// The re-work guard's one exception, as a value.
///
/// The guard is "this loop already paid for that ticket, so it does not get another
/// pass by itself". The exception is the case the guard was never about: the pass
/// that ended *on its own* — `agent_settled` with the ticket still un-closed, and no
/// human `Esc` anywhere near it. That is an unfinished pass, not a finished one, and
/// the loop is allowed to finish what it started without waiting for a human.
///
/// Two things keep it an exception rather than a hole:
///
/// * **It names a ticket.** A credit that could be spent on *any* already-worked
///   ticket is not an exception, it is the guard deleted — which is exactly what
///   removing `Recovery::Refused` did, and the reason that arm is still here.
/// * **It is spent, not remembered.** Each use decrements it, and a failing retry
///   does not top it back up ([`AutoResume::grant`] is deliberately not a reset), so
///   a ticket that cannot be closed runs out of exception and the guard answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AutoResume {
    /// The ticket this exception was earned by.
    pub(super) id: String,
    /// How many automatic re-buys it still has.
    pub(super) left: u32,
}

impl AutoResume {
    /// Grant the exception for `id` after a pass on it ended without closing.
    ///
    /// Not a reset: a credit already held for this ticket keeps what it has left,
    /// which is the whole bound. A credit held for a *different* ticket is replaced —
    /// only one pass can be in flight at a time, so only the ticket that just failed
    /// can be the one owed a re-buy.
    pub(super) fn grant(current: Option<&Self>, id: &str) -> Self {
        match current {
            Some(held) if held.id == id => Self {
                id: held.id.clone(),
                left: held.left,
            },
            _ => Self {
                id: id.to_string(),
                left: MAX_AUTO_RESUME,
            },
        }
    }

    /// Spend one automatic re-buy on `id`, answering whether that ticket may be
    /// bought again without a human.
    ///
    /// `false` for the wrong ticket *and* for an exhausted budget, and the caller does
    /// not get to tell those apart: both are "the guard answers", and the guard's
    /// sentence is the same in each.
    pub(super) fn spend(&mut self, id: &str) -> bool {
        if self.id != id || self.left == 0 {
            return false;
        }
        self.left -= 1;
        true
    }

    /// Is this credit about some *other* ticket? A credit is about the pass that just
    /// failed, so buying a pass on anything else retires it: a stale credit must not
    /// sit around authorising a re-buy three passes and one ticket later.
    pub(super) fn stale_for(&self, id: &str) -> bool {
        self.id != id
    }
}

/// Can this `Esc` be acted on, or is one already in progress?
///
/// A second `Esc` while the first is still unwinding is dropped: the worker has
/// already been told to stop, and stacking another `abort` on the same run buys
/// nothing and re-arms a stall timer that is already armed. Once the stall *has*
/// been reported the answer flips — that is what makes the user's next `Esc`
/// mean "try again" instead of being swallowed forever (ADR-0003).
pub(super) fn abort_is_redundant(aborted: bool, stall_reported: bool) -> bool {
    aborted && !stall_reported
}

/// Is this stall timer still the live one?
///
/// Four ways a timer arrives that must not be answered, and every one of them is a
/// real race rather than a hypothetical:
///
/// * `!aborted` — nothing was cancelled; a stale timer from a previous attempt.
/// * `stall_reported` — this stall has already been said once; the escalation is
///   one sentence, not an alarm that repeats.
/// * `armed != attempt` — the user pressed `Esc` again, so a *newer* attempt owns
///   the deadline now, and the old timer must be unable to kill the new pass.
/// * (the worker serial is checked at the call site, one level down, where the
///   pass itself is identified.)
pub(super) fn stall_timer_is_live(
    aborted: bool,
    stall_reported: bool,
    armed: u32,
    attempt: u32,
) -> bool {
    aborted && !stall_reported && armed == attempt
}
