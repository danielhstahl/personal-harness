//! The beads decision layer, kept pure (looprs-6ol).
//!
//! Nothing in here reads a process, a clock, or a mutable thing. Every
//! condition that decides *whether the beads loop may act* is a plain function
//! of plain arguments, which is what makes the money-safety guards testable as
//! tables rather than as trajectories — see the banner that was already written
//! over the pass gate below, and `tables` in [`tests`](super::tests) for the
//! enumeration it buys.

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
