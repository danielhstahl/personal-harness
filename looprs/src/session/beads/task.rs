//! The beads task: the tokio task that owns the [`BeadsLoop`].
//!
//! The `BeadsTask` struct here is the parked-state machine — the flags, and the
//! handling of the two things that move them from outside (a `Tab`, an `Esc`)
//! and the two that move them from inside (a worker settling, a worker dying).
//! `BeadsSession::build` in [`super`] spawns this and hands it the mailbox;
//! every arm of that mailbox is a call into a method here.
//!
//! The guards it consults live in [`guards`](super::guards) rather than inline,
//! so that each one can be enumerated instead of reached; the loop's own state
//! machine is in [`machine`](super::machine). This file is the interpreter: it
//! reads those answers, calls into the machine, and publishes the mirrors.

use crate::session::beads::guards::PassGate;

use super::guards::{StepCause, abort_is_redundant, stall_timer_is_live};
use super::machine::{BeadsLoop, PassOutcome, PlanCheck};
use super::notes::{
    left_for_human_note, no_plan_note, not_closed_note, nothing_held_note, plan_note,
    unverifiable_note, unverified_pass_note,
};

use super::BeadsCmd;

use crate::session::{SessionStatus, cancel};

/// The beads machine, running inside its own task.
///
/// These flags are the whole parked-state machine, and they are the reason a
/// re-entry cannot double-spawn: exactly one place calls [`BeadsLoop::next`], and
/// it refuses unless the loop is started, visible, idle, and holding a deferred
/// request.
pub(super) struct BeadsTask {
    pub(super) inner: BeadsLoop,
    /// Has the mode ever been entered? Nothing runs before that.
    pub(super) started: bool,
    /// Hidden by a Tab. No *new* passes until re-entered.
    pub(super) parked: bool,
    /// A pass is wanted. Coalesced, so N settles arriving off-screen become one
    /// pass on return rather than a burst of N.
    pub(super) pending: bool,
    /// A pass is in flight that has not been reported as settled. This is what
    /// keeps "come back and continue" from turning into "kill the running pass and
    /// start another": the only thing that clears it is the settle signal itself
    /// ([`BeadsCmd::WorkerSettled`]) or the worker's death, because the loop's
    /// own step cannot tell "still streaming" from "finished, and nobody has
    /// asked to advance yet".
    pub(super) streaming: bool,
    /// `Esc` landed on the in-flight pass. The settle that follows is the abort
    /// unwinding, **not** a pass that finished, so it parks instead of taking the
    /// next bead (ADR-0002 Q3: "an aborted worker must never read as
    /// `agent_settled` → next bead"). This flag is why that rule can be enforced
    /// at all: `agent_settled` looks identical either way.
    pub(super) aborted: bool,
    /// How many aborts this cancel has sent. The stall deadline carries one, so an
    /// old timer cannot be mistaken for the live one — the job `serial` does for
    /// the passes themselves, one level up.
    pub(super) abort_attempt: u32,
    /// The stall has been reported for the current attempt, so the escalation is
    /// one sentence rather than a repeating alarm. A later `Esc` clears it and
    /// starts a fresh attempt.
    pub(super) stall_reported: bool,
}

impl BeadsTask {
    /// The single door to [`BeadsLoop::next`].
    pub(super) async fn run_pending(&mut self) {
        let gate = PassGate {
            started: self.started,
            parked: self.parked,
            streaming: self.streaming,
            pending: self.pending,
        };
        if !gate.allows() {
            return;
        }
        self.pending = false;
        self.inner.next().await;
        self.streaming = self.inner.has_live_worker();
    }

    /// Is `serial` the pass this loop is actually running right now?
    ///
    /// The staleness check, and the reason every worker edge carries a serial. A
    /// killed worker's last records can arrive after the loop has moved on, and an
    /// untagged late settle would read as "my current pass finished": it would
    /// retire a live pass's `streaming` flag and start another one, killing work
    /// that is already paid for. Same shape as a stale `SessionId` in the router,
    /// one level down.
    fn is_live(&self, serial: u64) -> bool {
        self.inner.worker_serial() == Some(serial)
    }

    /// The pass in flight settled. This is the transition.
    pub(super) async fn worker_settled(&mut self, serial: u64) {
        if !self.is_live(serial) {
            return; // retired pass; nothing here belongs to it
        }
        self.streaming = false;
        if self.aborted {
            self.park_after(
                Some(&format!(
                    "beads: {} — the loop is parked. Type an instruction to start again.",
                    cancel::DONE
                )),
                None,
            )
            .await;
            return;
        }
        // A settle off the *planner* means something different than a settle off a
        // worker, and this is the only place in the loop that knows which one it
        // just got. Checking the board here — before `pending` is set — is what
        // stops a plan of zero from turning into N workers spinning on nothing.
        match self.inner.verify_plan().await {
            // Not a planner, so a worker. The question this side of the boundary is
            // not "is there a plan?" but "did the ticket it was paid for close?"
            // and it gets asked here, before `pending` can buy another worker.
            PlanCheck::NotPlanning => match self.inner.verify_worker_pass().await {
                PassOutcome::Closed(claim) => {
                    self.inner
                        .report_system(format!("beads: {} closed", claim.id));
                    // The out-of-band half of "it's done", and the only `notify`
                    // call in the binary — see [`BeadsLoop::announce_closed`].
                    self.inner.announce_closed(claim);
                }
                // The worker handed this ticket to a human. The rest of the board
                // is still ours, so the loop goes on — with the skip on the record.
                PassOutcome::LeftForHuman(claim, status) => {
                    self.inner
                        .report_system(left_for_human_note(&claim, status));
                }
                PassOutcome::NotClosed(claim, status) => {
                    self.park_after(None, Some(&not_closed_note(&claim, status)))
                        .await;
                    return;
                }
                PassOutcome::Unverifiable(claim, reason) => {
                    self.park_after(None, Some(&unverified_pass_note(&claim, &reason)))
                        .await;
                    return;
                }
                PassOutcome::NothingHeld => {
                    self.park_after(None, Some(nothing_held_note())).await;
                    return;
                }
            },
            // The human sees the plan before the first worker is paid to read it.
            PlanCheck::Created(tickets) => self.inner.report_system(plan_note(&tickets)),
            PlanCheck::NothingCreated { said } => {
                self.park_after(None, Some(&no_plan_note(&said))).await;
                return;
            }
            PlanCheck::Unverifiable { stage, reason } => {
                self.park_after(None, Some(&unverifiable_note(stage, &reason)))
                    .await;
                return;
            }
        }
        self.pending = true;
        self.run_pending().await;
    }

    /// The worker's stream ended without a settle.
    ///
    /// The symmetric edge of `worker_settled`, and the one that used to hang the
    /// mode: crash, OOM, `kill -9`, or a pi that exits early all leave the loop
    /// "working" forever with the input box hidden behind it, because nothing was
    /// ever going to send the settle it was waiting for. A loop that cannot advance
    /// must say so and hand the box back rather than spin.
    pub(super) async fn worker_gone(&mut self, serial: u64) {
        if !self.is_live(serial) {
            // We closed this worker ourselves at a pass boundary. Expected, and
            // silent by design: the next pass reports its own news.
            return;
        }
        self.streaming = false;
        if self.aborted {
            self.park_after(
                Some(&format!("beads: {} — the loop is parked.", cancel::DONE)),
                None,
            )
            .await;
        } else if self.inner.is_planning() {
            // The planner dying mid-run is the no-settle case this branch exists
            // for, and it needs saying in *planner* words: no plan was verified, so
            // nothing is queued and no worker will run. Silently falling through to
            // the worker wording would leave the user guessing which half of the
            // loop they just lost.
            self.park_after(
                None,
                Some(
                    "beads: the planner exited without settling; the plan was never verified, so nothing was queued and no workers started. Type an instruction to try again.",
                ),
            )
            .await;
        } else {
            self.park_after(
                None,
                Some("beads: the worker exited without settling; the loop is parked. Type an instruction to start again."),
            )
            .await;
        }
    }

    /// Stop wanting a pass, reap the worker, and go back to waiting for a human.
    ///
    /// Only the cancel/death paths park here, and they reap because nothing else
    /// will: an ordinary settle that arrives while the mode is hidden deliberately
    /// leaves the settled worker alone (looprs-05j pins "a Tab never kills, hidden
    /// or not", and the next pass reaps it at its own boundary). A pass that was
    /// cancelled or died has no following pass to do that reaping, so the park has
    /// to — a beads loop parked on top of a live child is the idle-child trap this
    /// loop was rewritten to avoid. `set_step(Awaiting)` is what hands the input
    /// box back.
    async fn park_after(&mut self, note: Option<&str>, error: Option<&str>) {
        self.aborted = false;
        self.pending = false;
        self.inner.close().await;
        if let Some(text) = error {
            self.inner.report_error(text.to_string());
        } else if let Some(text) = note {
            self.inner.report_system(text.to_string());
        }
        self.inner.set_step(StepCause::Awaiting);
    }

    /// `Esc`. Tell the worker to stop, and remember that we did.
    ///
    /// The flag is set *before* the command goes out, so the settle that pi emits
    /// as it unwinds is already classified as a cancellation and cannot be
    /// misread as a finished pass. An idle loop is a no-op, not an error
    /// (looprs-5g7), and a *stalled* abort leaves the ladder in a state where the
    /// user's next `Esc` means "try again" rather than being swallowed.
    pub(super) fn abort_pass(&mut self) {
        // Already aborting and the attempt is still live: the first abort is on
        // its way, and stacking a second one on the same run buys nothing.
        if abort_is_redundant(self.aborted, self.stall_reported) {
            return;
        }
        let Some(serial) = self.inner.abort_worker() else {
            // No pass in flight. Silent: nothing was stopped and nothing failed.
            return;
        };
        self.aborted = true;
        self.pending = false;
        self.stall_reported = false;
        self.abort_attempt += 1;
        let attempt = self.abort_attempt;
        // The word goes out now, before anything can come back: the loop has told
        // the worker to stop, and the user should not have to wonder whether the
        // keystroke reached it. `pass_label` is what makes "cancelling…" name the
        // bead rather than a faceless pass.
        let what = self
            .inner
            .pass_label()
            .map(|l| format!("`{l}`"))
            .unwrap_or_else(|| "the beads worker".to_string());
        self.inner.report_system(cancel::started(&what));
        cancel::arm(
            self.inner.ctl.clone(),
            BeadsCmd::AbortStalled { serial, attempt },
        );
    }

    /// The abort armed `cancel::GRACE` ago has produced neither a settle nor a
    /// stream end: the worker is not unwinding.
    ///
    /// Killing it is the right escalation here and not elsewhere, because the beads
    /// loop is the one mode where nothing precious is lost. The pass was a worker
    /// the user just refused to spend another token on, and the loop's own policy
    /// is cold between passes anyway (ADR-0002 Q3). The kill also has to happen
    /// *here*: a parked loop sitting on top of a live child is the idle-child trap
    /// [`park_after`] exists to avoid, and no later pass is coming to reap it.
    ///
    /// What must survive is the *breadcrumb*. A bead whose worker was cancelled
    /// mid-flight stays claimed, and that is a fact about the board the user cannot
    /// see from here — so it is named out loud, with the `bd` command that reads
    /// it back, rather than left to be discovered the next time the loop looks for
    /// work (the looprs-w7q guard reads exactly this state).
    pub(super) async fn abort_stalled(&mut self, serial: u64, attempt: u32) {
        // Not ours unless this attempt is still the live one: the settle may have
        // landed first, or the user may have Esc'd again and a newer attempt owns
        // the deadline now.
        if !stall_timer_is_live(
            self.aborted,
            self.stall_reported,
            self.abort_attempt,
            attempt,
        ) {
            return;
        }
        if self.inner.worker_serial() != Some(serial) {
            return; // that pass is already gone; nothing to kill or explain
        }
        self.stall_reported = true;
        // Captured before `close()`, which takes the label away with the worker.
        let bead = self.inner.pass_label().map(|s| s.to_string());
        self.inner.close().await;
        let left_behind = match &bead {
            Some(id) => format!("{id} stays claimed — `bd show {id}` says where it stopped"),
            None => "the bead it was working on stays claimed".to_string(),
        };
        self.park_after(
            None,
            Some(&format!(
                "{} — the worker was killed. {left_behind}; the loop is parked. Type an instruction to start again.",
                cancel::stalled(bead.as_deref().unwrap_or("the beads worker"))
            )),
        )
        .await;
    }

    pub(super) fn status(&self) -> SessionStatus {
        if self.aborted {
            SessionStatus::Aborting
        } else if self.inner.has_live_worker() {
            SessionStatus::Running
        } else if self.started {
            SessionStatus::Idle
        } else {
            SessionStatus::NotStarted
        }
    }
}
