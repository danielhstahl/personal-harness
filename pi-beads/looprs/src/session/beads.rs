//! The Beads terminal state: the planner/worker loop, wrapped as a [`Session`].
//!
//! Two types, one per side of the handle/task split that ADR-0002 Q1 requires:
//!
//! * [`BeadsLoop`] — the machine itself. Moved here from `app.rs`: it is a backend,
//!   not UI state, and it now reports [`SessionEvent`] instead of reaching into the
//!   UI's `Msg` type. Same behavior; the only thing that changed is which type it
//!   talks into, which is what lets the Router own it.
//! * [`BeadsSession`] — the `Box<dyn Session>` handle the Router holds. Every trait
//!   method queues a [`BeadsCmd`] into the task that owns the loop and returns
//!   immediately, which is what keeps "the router never blocks on a child" true.
//!
//! The one piece of real lifecycle policy here is
//! [`SwitchAway::DrainThenPark`](crate::session::SwitchAway): while the beads mode
//! is off-screen the loop must not *start* new passes (it self-advances, so "keeps
//! running while nobody looks" means "keeps spending money on the board"), but a
//! pass already running is allowed to finish, because killing it throws away
//! paid-for work. Both rules funnel through [`BeadsTask::run_pending`], so
//! reversing a cell of the Q3 table is one function.
//!
//! ## Who drives the loop (looprs-msj)
//!
//! **The worker drives it, from inside this session.** `agent_settled` off the
//! loop's own `pi` child arrives at [`BeadsTask::worker_settled`] and is where the
//! "is there another pass?" question gets answered — together with the loop's own
//! [`BeadStep`], the parked flag and the abort flag. Nothing upstream of this file
//! is in that path.
//!
//! It used to be. The App read a settle and sent `UiCommand::BeadsNext` back down,
//! which meant the transition was keyed off *whatever the input box was set to*
//! when the event happened to arrive: a Pi answer settling while the box was on
//! Beads drove the beads machine, and a beads worker settling while the box was on
//! Pi left it stalled. Both were the same mistake — asking the UI who made an event.
//!
//! So `UiCommand::BeadsNext` and `Session::advance()` are gone (a generic
//! `Session` had no business carrying "take another bead"), and the settle is
//! routed from the child to the task that owns the loop by
//! [`BeadsLoop::forward_worker`], tagged with the serial of the pass that made it.
//! The App renders the same records and decides nothing.
//!
//! ## Who checks the planner (looprs-k7v)
//!
//! **A planner pass is not believed until the board says so.** `agent_settled` from
//! the planner means "the planner stopped talking", which is a different fact from
//! "there is a plan", and the two used to be treated as one: a planner that
//! misread the instructions, refused, or whose every `bd create` failed settled
//! just as cleanly as one that worked, and the loop rolled into `AwaitInput`
//! looking exactly like a successful no-op with the user's request on the floor.
//!
//! So [`BeadsLoop::launch_create_tickets`] snapshots the open board *before* the
//! child is spawned, [`BeadsLoop::verify_plan`] diffs it after the settle, and
//! [`BeadsTask::worker_settled`] will not queue a worker until that diff has a
//! verdict. Three things fall out of that ordering, and they are the whole feature:
//!
//! * zero new tickets is a loud error that quotes the planner's own last words, and
//!   parks rather than advancing;
//! * a real plan is listed (`id: title`) in the transcript **before** the first
//!   worker is paid to read it;
//! * "the board could not be read" is its own verdict, never a synonym for either
//!   of the above — including for "empty".
//!
//! ## Who claims the bead (looprs-w7q)
//!
//! **The harness claims; the worker just works.** [`BeadsLoop::work_next_bead`]
//! runs `bd update <id> --claim` *before* a `pi` child is bought, so the loop
//! knows — independently of anything the agent later says or fails to say — which
//! ticket it is spending money on. Everything else in this section falls out of
//! that one ordering:
//!
//! * the claimed id and title go into the worker's prompt, so the agent never runs
//!   `bd ready` itself and cannot end up working (and getting billed for) a
//!   different ticket than the one being reported. That was a race, not a style
//!   point, and `bd ready` is gone from [`WORKER`] for exactly that reason;
//! * a claim `bd` refuses costs one CLI call rather than a whole worker session
//!   pointed at a bead nobody holds;
//! * the claim is published to the UI as it is taken and released
//!   ([`SessionEvent::ActiveBead`]), which is what lets the status row
//!   (looprs-guh) name the active ticket without shelling out mid-frame;
//! * when the worker settles, [`BeadsLoop::verify_worker_pass`] asks the board the
//!   only question that matters — *did it close?* — and a ticket left open stops
//!   the loop instead of buying another pass on itself, because `bd ready` hands
//!   the same bead straight back and each pass is billed. That is the runaway this
//!   ticket was filed for.
//!
//! The guard releases on one thing only: a new instruction from a human, which in
//! beads mode arrives through [`BeadsLoop::launch_create_tickets`]. Re-entering
//! the mode is *not* an acknowledgement — a Tab back means "let me look", not
//! "spend on that one again".

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

use crate::app::{AssistantEvent, PiEvent, parse};
use crate::services::bd::{Bead, BeadStatus, claim_with, list_status_with, ready_with, show_with};
use crate::services::pi::PiRpc;
use crate::services::prompts::{PLANNER, WORKER, generate_prompt};
use crate::session::{
    ActiveBead, BeadStep, ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus,
    Spawned, cancel,
};
use serde_json::Value;

/// Commands to the task that owns the [`BeadsCmd`] mailbox.
///
/// Two groups, and the split is the whole point: the top half is what the *UI*
/// asks of this session, the bottom half is what this session's own *worker*
/// reports to it. Both land in one mailbox, so they are handled one at a time, in
/// the order they occurred — which is what makes "the Tab arrived before the
/// settle" and "the settle arrived before the Tab" each have exactly one answer.
enum BeadsCmd {
    /// A planner instruction typed into the beads box.
    Submit(String),
    /// `Esc`: abort the in-flight pass and park (ADR-0002 Q3).
    Abort,
    /// The abort sent `attempt` attempts ago has still not produced a settle (or a
    /// stream end) after [`cancel::GRACE`](crate::session::cancel). The worker is
    /// not unwinding; kill it and say so, naming the bead it leaves claimed
    /// (ADR-0003). `serial` and `attempt` are what make a deadline from a pass the
    /// loop has already moved past unmatchable.
    AbortStalled { serial: u64, attempt: u32 },
    /// The mode became, or stopped being, the one on screen. Q3's policy input.
    Active(bool),
    /// This session's own worker settled (`agent_settled`) on the pass tagged
    /// `serial`. The only thing that ever drives the loop forward.
    WorkerSettled { serial: u64 },
    /// This session's worker's stdout closed. Same serial rule as
    /// [`BeadsCmd::WorkerSettled`]: a notice from a pass this loop already
    /// retired says nothing about the one in flight.
    WorkerGone { serial: u64 },
    /// Test seam: ack once every command queued before this one is fully handled.
    Sync(oneshot::Sender<()>),
    /// App is exiting: reap the worker and report the session gone.
    Shutdown,
}

/// The beads machine, running inside its own task.
///
/// These flags are the whole parked-state machine, and they are the reason a
/// re-entry cannot double-spawn: exactly one place calls [`BeadsLoop::next`], and
/// it refuses unless the loop is started, visible, idle, and holding a deferred
/// request.
struct BeadsTask {
    inner: BeadsLoop,
    /// Has the mode ever been entered? Nothing runs before that.
    started: bool,
    /// Hidden by a Tab. No *new* passes until re-entered.
    parked: bool,
    /// A pass is wanted. Coalesced, so N settles arriving off-screen become one
    /// pass on return rather than a burst of N.
    pending: bool,
    /// A pass is in flight that has not been reported as settled. This is what
    /// keeps "come back and continue" from turning into "kill the running pass and
    /// start another": the only thing that clears it is the settle signal itself
    /// ([`BeadsCmd::WorkerSettled`]) or the worker's death, because the loop's
    /// own step cannot tell "still streaming" from "finished, and nobody has
    /// asked to advance yet".
    streaming: bool,
    /// `Esc` landed on the in-flight pass. The settle that follows is the abort
    /// unwinding, **not** a pass that finished, so it parks instead of taking the
    /// next bead (ADR-0002 Q3: "an aborted worker must never read as
    /// `agent_settled` → next bead"). This flag is why that rule can be enforced
    /// at all: `agent_settled` looks identical either way.
    aborted: bool,
    /// How many aborts this cancel has sent. The stall deadline carries one, so an
    /// old timer cannot be mistaken for the live one — the job `serial` does for
    /// the passes themselves, one level up.
    abort_attempt: u32,
    /// The stall has been reported for the current attempt, so the escalation is
    /// one sentence rather than a repeating alarm. A later `Esc` clears it and
    /// starts a fresh attempt.
    stall_reported: bool,
}

impl BeadsTask {
    /// The single door to [`BeadsLoop::next`].
    async fn run_pending(&mut self) {
        if self.pending && self.started && !self.parked && !self.streaming {
            self.pending = false;
            self.inner.next().await;
            self.streaming = self.inner.has_live_worker();
        }
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
    async fn worker_settled(&mut self, serial: u64) {
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
                }
                // The worker handed this ticket to a human. The rest of the board
                // is still ours, so the loop goes on — with the skip on the record.
                PassOutcome::LeftForHuman(claim, status) => {
                    self.inner.report_system(left_for_human_note(&claim, status));
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
    async fn worker_gone(&mut self, serial: u64) {
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
    /// loop was rewritten to avoid. `set_step(AwaitInput)` is what hands the input
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
        self.inner.set_step(BeadStep::AwaitInput);
    }

    /// `Esc`. Tell the worker to stop, and remember that we did.
    ///
    /// The flag is set *before* the command goes out, so the settle that pi emits
    /// as it unwinds is already classified as a cancellation and cannot be
    /// misread as a finished pass. An idle loop is a no-op, not an error
    /// (looprs-5g7), and a *stalled* abort leaves the ladder in a state where the
    /// user's next `Esc` means "try again" rather than being swallowed.
    fn abort_pass(&mut self) {
        // Already aborting and the attempt is still live: the first abort is on
        // its way, and stacking a second one on the same run buys nothing.
        if self.aborted && !self.stall_reported {
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
    async fn abort_stalled(&mut self, serial: u64, attempt: u32) {
        // Not ours unless this attempt is still the live one: the settle may have
        // landed first, or the user may have Esc'd again and a newer attempt owns
        // the deadline now.
        if !self.aborted || self.stall_reported || self.abort_attempt != attempt {
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

    fn status(&self) -> SessionStatus {
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

/// Handle onto the beads session. Cloning is cheap and legitimate: a clone is
/// another queue into the same task, which is also what tests use to observe it.
#[derive(Clone)]
pub struct BeadsSession {
    id: SessionId,
    cmd: mpsc::UnboundedSender<BeadsCmd>,
    status: Arc<StdMutex<SessionStatus>>,
    /// Mirror of the pass the loop has in flight, published by the task the same way
    /// `status` is. Nothing outside the task may write it, which keeps the loop's
    /// state single-owner while still letting the status row (looprs-guh) say *which*
    /// pass is running — and letting a test settle against a serial it did not make up.
    in_flight: Arc<StdMutex<Option<u64>>>,
}

impl BeadsSession {
    /// Start the beads session. Builds no process: the loop starts no worker until
    /// it is entered *and* the board has something ready
    /// ([`BeadsLoop::next`]'s pass boundary).
    pub fn start(id: SessionId, cfg: &SessionConfig) -> Result<Spawned> {
        let (session, events) = Self::build(id, cfg)?;
        Ok(Spawned {
            session: Box::new(session),
            events,
        })
    }

    /// The un-boxed form, so tests can hold a concrete handle instead of reaching
    /// through `dyn Session`.
    fn build(
        id: SessionId,
        cfg: &SessionConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SessionEvent>)> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<BeadsCmd>();
        let status = Arc::new(StdMutex::new(SessionStatus::NotStarted));
        let task_status = status.clone();
        let in_flight = Arc::new(StdMutex::<Option<u64>>::new(None));
        let task_in_flight = in_flight.clone();
        let mut task = BeadsTask {
            inner: BeadsLoop::new(id, ev_tx.clone(), cmd_tx.clone(), cfg.clone()),
            started: false,
            parked: false,
            pending: false,
            streaming: false,
            aborted: false,
            abort_attempt: 0,
            stall_reported: false,
        };

        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    // Entering the mode: "get on with it". `pending` is *set*
                    // rather than a pass started here, so a live worker still blocks
                    // it — a resume comes from the parked state, never from a spawn.
                    BeadsCmd::Active(true) => {
                        task.started = true;
                        task.parked = false;
                        task.pending = true;
                        task.run_pending().await;
                    }
                    // Leaving the mode: park. No kill, and nothing awaited: the pass
                    // already running finishes on its own (that is the "drain").
                    BeadsCmd::Active(false) => {
                        task.parked = true;
                    }
                    BeadsCmd::Submit(text) => {
                        if let Err(e) = task.inner.launch_create_tickets(&text).await {
                            tracing::error!("planner pass failed: {e:#}");
                            task.inner.report_error(format!("planner: {e:#}"));
                            task.inner.set_step(BeadStep::AwaitInput);
                        }
                    }
                    // This command *is* the settle signal for the pass that made
                    // it, so it is what retires `streaming`. Deferred while
                    // parked, so a Tab back produces one pass rather than one per
                    // settle that happened to arrive off-screen.
                    BeadsCmd::WorkerSettled { serial } => task.worker_settled(serial).await,
                    BeadsCmd::WorkerGone { serial } => task.worker_gone(serial).await,
                    BeadsCmd::Abort => task.abort_pass(),
                    BeadsCmd::AbortStalled { serial, attempt } => {
                        task.abort_stalled(serial, attempt).await
                    }
                    BeadsCmd::Sync(tx) => {
                        // Publish the mirrors *before* acking. A caller that waits
                        // on the seam and then reads `status()` / `in_flight()`
                        // must see the state as of that ack, not one command
                        // stale — and on a multi-threaded runtime the ack wakes
                        // the waiter before this task necessarily gets to the
                        // end-of-iteration mirror below.
                        *task_status.lock().unwrap() = task.status();
                        *task_in_flight.lock().unwrap() = task.inner.worker_serial();
                        let _ = tx.send(());
                    }
                    BeadsCmd::Shutdown => {
                        task.inner.close().await;
                        *task_status.lock().unwrap() = SessionStatus::Dead;
                        *task_in_flight.lock().unwrap() = None;
                        let _ = ev_tx.send(SessionEvent::Exited {
                            reason: ExitReason::Shutdown,
                        });
                        break;
                    }
                }
                *task_status.lock().unwrap() = task.status();
                *task_in_flight.lock().unwrap() = task.inner.worker_serial();
            }
            // The task ends here and `ev_tx` drops with it, so the router's pump
            // still owes — and supplies — the exactly-one SessionDown.
        });

        Ok((
            Self {
                id,
                cmd: cmd_tx,
                status,
                in_flight,
            },
            ev_rx,
        ))
    }

    /// The pass this session has in flight right now, `None` when nothing is
    /// running. A snapshot taken from outside the task, so it is only as fresh as
    /// the last command the task finished — see [`Self::quiesce`].
    pub fn in_flight(&self) -> Option<u64> {
        *self.in_flight.lock().unwrap()
    }

    /// Test seam: returns once every command queued before this call has been
    /// fully handled by the session's task. Makes lifecycle assertions
    /// deterministic instead of sleep-based.
    pub async fn quiesce(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(BeadsCmd::Sync(tx)).is_err() {
            return true; // task already gone: nothing left to wait for
        }
        tokio::time::timeout(Duration::from_secs(10), rx)
            .await
            .is_ok()
    }
}

impl Session for BeadsSession {
    fn id(&self) -> SessionId {
        self.id
    }

    fn send_text(&mut self, text: String) -> Result<()> {
        self.cmd
            .send(BeadsCmd::Submit(text))
            .map_err(|_| anyhow!("beads session task is gone"))
    }

    fn abort(&mut self) -> Result<()> {
        // `Esc` in the beads view: abort the worker *and* park. The parking is
        // not optional — a settle arriving from an aborted worker is pi unwinding,
        // not a pass that finished, and advancing on it would claim the next bead
        // the user just refused to spend the turn on. looprs-5g7 owns the
        // escalation numbers and the wording; the rule itself lives here, because
        // the loop is the only thing that knows what it aborted.
        self.cmd
            .send(BeadsCmd::Abort)
            .map_err(|_| anyhow!("beads session task is gone"))
    }

    fn shutdown(&mut self) -> Result<()> {
        self.cmd
            .send(BeadsCmd::Shutdown)
            .map_err(|_| anyhow!("beads session task is gone"))
    }

    fn set_active(&mut self, active: bool) -> Result<()> {
        self.cmd
            .send(BeadsCmd::Active(active))
            .map_err(|_| anyhow!("beads session task is gone"))
    }

    fn status(&self) -> SessionStatus {
        *self.status.lock().unwrap()
    }
}

// ---------------------------------------------------------------------------------------
// The machine itself — moved out of `app.rs`, and now reporting `SessionEvent`.
// ---------------------------------------------------------------------------------------

/// The beads terminal state, as a `BeadsLoop`.
pub struct BeadsLoop {
    /// This loop's identity, kept for logs and for the events' provenance chain
    /// (ADR-0002 Q2: the pump adds the id to what reaches the UI).
    id: SessionId,
    /// The worker currently driven by the loop. `None` whenever the loop is parked.
    pi_rx: Option<Worker>,
    /// Serial source for workers. One per spawned pass, never reused, so a record
    /// from a pass this loop retired is distinguishable from one it is still
    /// waiting on (see [`BeadsTask::is_live`]).
    next_serial: u64,
    ev_tx: mpsc::UnboundedSender<SessionEvent>,
    /// This session's own mailbox. A worker's control edges go here, **not** out to
    /// the UI: the loop advances itself rather than being told to by whichever
    /// mode happened to be on screen (looprs-msj).
    ctl: mpsc::UnboundedSender<BeadsCmd>,
    cfg: SessionConfig,
    bead_step: BeadStep,
    /// Set for the duration of one planner pass: the board as it looked *before* the
    /// run, plus a handle on the run's own last words. `None` means the pass in
    /// flight is a worker's (or there is no pass), and there is nothing to diff.
    planning: Option<PlanPass>,
    /// The ticket this pass holds a claim on, `None` between passes and for every
    /// planner pass (looprs-w7q).
    ///
    /// Taken before the worker is spawned and published to the UI in the same step
    /// ([`BeadsLoop::set_claim`]), so the loop's own knowledge of what it is
    /// paying for and what the screen is told cannot drift apart.
    claim: Option<ActiveBead>,
    /// Every ticket this loop has put a worker on since the last human instruction.
    ///
    /// This is the re-work guard, and the reason it is a *set* rather than "the
    /// last one" is that a board can hand a worked bead back two passes later — a
    /// cycle of X, Y, X is the same runaway as X, X. It is filled the moment a
    /// claim succeeds (not when a pass ends), so a worker that died, was killed or
    /// settled without closing is all three, and is still covered.
    ///
    /// It clears only on `launch_create_tickets`, i.e. on a new instruction from a
    /// human. That is deliberate: the guard exists to stop *automatic* re-work, and
    /// a typed instruction is the only deliberate thing the beads mode accepts.
    worked: HashSet<String>,
}

/// What one planner pass has to be checked against when it settles (looprs-k7v).
struct PlanPass {
    /// Ids that were already open when the planner was spawned. Set membership, not
    /// a count: on a live board a plan of two plus an unrelated close elsewhere is
    /// not evidence of anything, and a count-based diff would read that as noise.
    baseline: HashSet<String>,
    /// The planner's last assistant text, tee'd off its stdout by
    /// [`BeadsLoop::forward_records`]. A zero-ticket plan is a lot more
    /// diagnosable with the model's own sentence attached: "it refused", "it asked
    /// a question back" and "it narrated a plan it never wrote down" are otherwise
    /// indistinguishable, and only the first two are the harness's problem.
    said: Arc<StdMutex<String>>,
}

/// The verdict of checking a planner pass against the board. Four outcomes, because
/// there are four things the user can act on and a bool cannot name them.
enum PlanCheck {
    /// The pass that settled was a worker's, not a planner's: nothing to check.
    NotPlanning,
    /// The planner added these tickets to the board. Show them, then work them.
    Created(Vec<Bead>),
    /// The planner settled and the board gained nothing — the silent no-op this
    /// ticket was filed for, now an explicit one.
    NothingCreated { said: String },
    /// A `bd` read failed, so the plan is unverifiable. Deliberately **not**
    /// foldable into [`PlanCheck::NothingCreated`]: "the board is empty" and "the
    /// board is unreadable" are the two facts that must never be conflated, which
    /// is the exact conflation looprs-037 was about, one level up.
    Unverifiable { stage: &'static str, reason: String },
}

/// What the loop decided to do with the tickets `bd ready` offered it.
///
/// Three answers rather than an `Option`, because "there is nothing to work" and
/// "the only thing left is one I already burned a pass on" are different
/// sentences and the human's next action differs: wait for instructions, or go
/// close the ticket.
enum Pick {
    /// Work this one.
    Work(Bead),
    /// `bd ready` was empty. The board is done; the loop parks for an instruction.
    Nothing,
    /// The first workable ticket is one this loop already ran a pass on and did not
    /// close. Refused, not re-run — the runaway guard (looprs-w7q).
    AlreadyWorked(Bead),
}

/// What the board says about the ticket a worker pass just stopped talking about
/// (looprs-w7q). One arm per thing the human can act on, because a bool cannot
/// tell "keep going" from "go close it" from "fix `bd`".
enum PassOutcome {
    /// The ticket closed. This pass earned its keep; take the next one.
    Closed(ActiveBead),
    /// The worker left it `blocked` / `deferred`: it stopped on purpose and handed
    /// it to a human. Not the loop's problem, and not a thing it will re-buy — a
    /// blocked ticket is not ready work. Moving on is right; saying nothing about it
    /// would leave the human hunting for why a ticket did not finish.
    LeftForHuman(ActiveBead, BeadStatus),
    /// The pass ended and the ticket is still `open` / `in_progress` / unreadable-
    /// as-`unknown`. `bd ready` offers this exact bead back, so carrying on here
    /// is the loop running itself up a hill. Stops the loop.
    NotClosed(ActiveBead, BeadStatus),
    /// `bd` could not answer, so nobody knows. Deliberately **not** foldable into
    /// [`PassOutcome::Closed`] nor into [`PassOutcome::NotClosed`]: the remedy is
    /// "repair `bd`", not "close the ticket", and the loop must not silently pick
    /// one of those two readings on a read that failed.
    Unverifiable(ActiveBead, String),
    /// A worker settled while the loop held no claim. Nothing can be verified
    /// against nothing; said out loud rather than assumed harmless, because a
    /// settle that cannot be attributed to a ticket is the very event that used to
    /// drive this loop blind (looprs-msj).
    NothingHeld,
}

/// The worker child, plus the serial that says *which* pass it belongs to.
struct Worker {
    serial: u64,
    rpc: PiRpc,
    /// The last non-empty assistant text this child streamed. Only the planner pass
    /// ever reads it, but every worker carries one because the tee lives in the
    /// shared record-forwarding path and a `Worker` without it would be a worker
    /// whose last words were thrown away.
    said: Arc<StdMutex<String>>,
}

/// What a worker pass did.
enum WorkerPass {
    /// Nothing ready: no child was spawned, the loop should park.
    Idle,
    /// A worker was spawned *and prompted*.
    Working,
}

impl BeadsLoop {
    /// Builds the loop without touching any process: it starts parked in
    /// `AwaitInput`. Work only begins when [`BeadsLoop::next`] is called, so a
    /// constructed-but-unstarted loop can never be holding an idle, unprompted
    /// pi child (the bug this replaces: `new()` used to spawn a session nobody
    /// ever prompted, and nothing else would ever prompt it).
    ///
    /// Private to the module on purpose: `BeadsCmd` is private, and a public
    /// constructor taking one would advertise the loop's own mailbox.
    /// [`BeadsSession::start`] is the way in.
    fn new(
        id: SessionId,
        ev_tx: mpsc::UnboundedSender<SessionEvent>,
        ctl: mpsc::UnboundedSender<BeadsCmd>,
        cfg: SessionConfig,
    ) -> Self {
        Self {
            id,
            pi_rx: None,
            next_serial: 1,
            ev_tx,
            ctl,
            cfg,
            bead_step: BeadStep::AwaitInput,
            planning: None,
            claim: None,
            worked: HashSet::new(),
        }
    }

    pub fn set_step(&mut self, s: BeadStep) {
        self.bead_step = s.clone(); // keep the field private
        let _ = self.ev_tx.send(SessionEvent::BeadStep(s));
    }

    pub fn get_step(&self) -> &BeadStep {
        &self.bead_step
    }

    pub fn is_awaiting_input(&self) -> bool {
        matches!(self.bead_step, BeadStep::AwaitInput)
    }

    /// Is there a worker process behind this loop? This is the check that makes
    /// "one pass at a time" a property of the machine rather than a hope.
    pub fn has_live_worker(&self) -> bool {
        self.pi_rx.is_some()
    }

    /// The serial of the pass currently in flight, `None` when nothing is running.
    /// A settle means "advance" only when it carries this serial.
    pub fn worker_serial(&self) -> Option<u64> {
        self.pi_rx.as_ref().map(|w| w.serial)
    }

    /// Tear down the current session, then work the next ready bead or park.
    ///
    /// Infallible by design: a failing `bd`, a `pi` that will not start, or a prompt
    /// that never gets answered is reported to the transcript and the loop parks in
    /// `AwaitInput` for a human. Nothing here retries on a timer, so a broken board
    /// cannot turn into a respawn storm.
    async fn next(&mut self) {
        self.close().await;
        match self.work_next_bead().await {
            Ok(WorkerPass::Working) => {}
            Ok(WorkerPass::Idle) => {
                tracing::debug!("board empty, awaiting input");
                self.report_system("beads: board empty, awaiting input".to_string());
                self.set_step(BeadStep::AwaitInput);
            }
            Err(e) => {
                tracing::error!("beads worker pass failed: {e:#}");
                self.report_error(format!("beads: {e:#}"));
                self.set_step(BeadStep::AwaitInput);
            }
        }
    }

    /// Claim the next bead, then buy the worker for it — in that order.
    ///
    /// The claim comes first because everything after it is paid for: the pass is
    /// only worth reporting, verifying and guarding once the harness actually holds
    /// the ticket. Spawn-first would mean a refused claim still cost a whole `pi`
    /// session, and one pointed at a bead we do not own answers to whoever prompts
    /// it rather than to us (looprs-w7q).
    ///
    /// Also the only place a worker is both spawned *and* prompted, so callers
    /// never see a pi child that is alive but has been given nothing to do.
    async fn work_next_bead(&mut self) -> Result<WorkerPass> {
        let beads = ready_with(&self.cfg.bd_bin).await?;
        let bead = match self.pick_bead(&beads) {
            Pick::Nothing => return Ok(WorkerPass::Idle),
            Pick::AlreadyWorked(bead) => return Err(anyhow!(already_worked_note(&bead))),
            Pick::Work(bead) => bead,
        };
        tracing::info!(bead = %bead.id(), "claiming a worker pass");

        let claim = ActiveBead {
            id: bead.id().to_string(),
            title: bead.title().to_string(),
        };
        claim_with(&self.cfg.bd_bin, &claim.id)
            .await
            .map_err(|e| anyhow!("could not claim {} before buying a worker: {e}", claim.id))?;

        // Two records of one claim, both taken before anything is prompted:
        // `worked` is the guard that outlives the pass, `claim` is the live one the
        // UI and the post-settle check read. Recording the claim *before* the spawn
        // is what covers the pass that never reaches a settle at all — a spawn that
        // fails, a prompt that is refused, a worker that is killed mid-run. Each of
        // those leaves the ticket claimed on the board, and `bd ready` hands it
        // back, so the guard has to already know about it.
        self.worked.insert(claim.id.clone());
        self.set_claim(Some(claim.clone()));

        // The session is built against locals: if any step below fails, `worker`
        // drops here and kill_on_drop reaps it, so a failed pass cannot leave an
        // orphan behind.
        let worker = self.spawn_worker(&[])?;
        let disposition = worker.rpc.prompt(&worker_prompt(&claim)).await?;
        if disposition == "handled" {
            // pi took the prompt but started no run, so no `agent_settled` will ever
            // arrive to advance the loop. Do not hold an idle session open.
            drop(worker);
            return Err(anyhow!(
                "worker prompt for {} was handled without starting a run",
                claim.id
            ));
        }

        self.pi_rx = Some(worker);
        self.report_system(format!("beads: working {}", claim.id));
        self.set_step(BeadStep::WorkTickets);
        Ok(WorkerPass::Working)
    }

    /// Decide which bead this pass works, given what `bd ready` offered.
    ///
    /// Three rules, in this order, and each one is a thing that used to go wrong:
    ///
    /// 1. **A ticket that needs a human is skipped, out loud.** `blocked` and
    ///    `deferred` mean somebody — a planner, or a worker that gave up — took it
    ///    out of the loop on purpose, and `bd ready` should not have listed it.
    ///    Spending a metered pass on one is the worst possible answer; staying
    ///    quiet about the skip would make it indistinguishable from a lost ticket.
    /// 2. **A ticket this loop already worked is refused, never re-run.** If the
    ///    first workable bead is one whose pass already ended without closing it,
    ///    starting another pass on it is the runaway this ticket was filed for:
    ///    `bd ready` will keep offering a bead for exactly as long as it stays
    ///    open, and every pass is billed. The refusal parks the loop with its two
    ///    ways out, both of which belong to a human.
    /// 3. **Otherwise the first workable bead**, in `bd`'s own priority order.
    ///
    /// What is deliberately *not* a rule: an unknown status is workable. Skipping
    /// what this build cannot classify would let a `bd` upgrade silently empty the
    /// board, which is looprs-037's conflation wearing a different hat.
    fn pick_bead(&self, beads: &[Bead]) -> Pick {
        let mut skipped: Vec<Bead> = Vec::new();
        let mut pick = Pick::Nothing;
        for bead in beads {
            if bead.needs_human() {
                skipped.push(bead.clone());
                continue;
            }
            pick = if self.worked.contains(bead.id()) {
                Pick::AlreadyWorked(bead.clone())
            } else {
                Pick::Work(bead.clone())
            };
            break;
        }
        self.report_skipped(&skipped);
        pick
    }

    /// "Here is what I walked past, and why." Bounded like the plan note: the
    /// transcript is a screen, not a dump.
    fn report_skipped(&self, skipped: &[Bead]) {
        if skipped.is_empty() {
            return;
        }
        let mut note = format!(
            "beads: skipping {} ticket(s) that need a human, not a worker:",
            skipped.len()
        );
        for b in skipped.iter().take(MAX_SKIPPED_LISTED) {
            note.push_str(&format!("\n  {} ({}) — {}", b.id(), b.status(), b.title()));
        }
        if let Some(rest) = skipped.len().checked_sub(MAX_SKIPPED_LISTED) {
            note.push_str(&format!("\n  \u{2026} and {rest} more"));
        }
        self.report_system(note);
    }

    /// Spawn one pass's `pi` child and wire its records into this session.
    ///
    /// The child comes back *unassigned*: the caller decides the moment the pass is
    /// live by storing it in `self.pi_rx`, which keeps "a pass is in flight" a
    /// fact the loop controls rather than a side effect of a spawn having
    /// happened somewhere.
    fn spawn_worker(&mut self, args: &[&str]) -> Result<Worker> {
        let (rpc, records) = PiRpc::spawn_with(&self.cfg.pi_bin, args)?;
        let serial = self.next_serial;
        self.next_serial += 1;
        let said = Arc::new(StdMutex::new(String::new()));
        self.forward_records(serial, records, said.clone());
        Ok(Worker { serial, rpc, said })
    }

    /// Pipe one worker's records into this session: two copies of the same fact,
    /// deliberately.
    ///
    /// * the **render** copy goes out on `ev_tx` unchanged, and the App turns it
    ///   into transcript lines knowing nothing about beads;
    /// * the **control** copy — the edges only, `agent_settled` and the end of the
    ///   stream — goes into the task that owns this loop, tagged with the serial of
    ///   the pass that produced it.
    ///
    /// The serial is not decoration. A killed worker's last records can arrive
    /// after the loop has moved on, and an untagged late settle would read as
    /// "the pass in flight finished", killing work that is already paid for to
    /// start a pass nobody asked for. The control copy is also why no UI message is
    /// involved in advancing the loop: this used to be `UiCommand::BeadsNext`,
    /// arriving from whichever mode the input box happened to be in (looprs-msj).
    ///
    /// A third, smaller copy: the child's last completed assistant text lands in
    /// `said` on the way past, so the planner check can quote the model's own
    /// conclusion instead of the harness inventing one. `TextEnd` rather than the
    /// deltas because it is the authoritative final content of the block — the
    /// deltas are what was typed, this is what landed.
    fn forward_records(
        &self,
        serial: u64,
        mut records: mpsc::UnboundedReceiver<Value>,
        said: Arc<StdMutex<String>>,
    ) {
        let render = self.ev_tx.clone();
        let ctl = self.ctl.clone();
        tokio::spawn(async move {
            while let Some(v) = records.recv().await {
                let Some(ev) = parse(&v) else {
                    continue; // `parse` logs the offender
                };
                if let PiEvent::MessageUpdate {
                    assistant_message_event: AssistantEvent::TextEnd { content, .. },
                } = &ev
                {
                    if !content.trim().is_empty() {
                        *said.lock().unwrap() = content.clone();
                    }
                }
                let settled = matches!(ev, PiEvent::AgentSettled);
                if render.send(SessionEvent::Agent(ev)).is_err() {
                    return; // the session is gone: nothing to render, nothing to advance
                }
                if settled {
                    let _ = ctl.send(BeadsCmd::WorkerSettled { serial });
                }
            }
            // stdout closed. Whether that is expected is the task's call, not this
            // task's: it needs the loop's state to know.
            let _ = ctl.send(BeadsCmd::WorkerGone { serial });
        });
    }

    pub fn report_error(&self, text: String) {
        let _ = self.ev_tx.send(SessionEvent::Error(text));
    }

    pub fn report_system(&self, text: String) {
        let _ = self.ev_tx.send(SessionEvent::System(text));
    }

    /// Tear down the current worker, if any.
    ///
    /// `take()` happens before the kill on purpose: from that instant the loop has
    /// no live pass, so the dying child's own `WorkerGone` carries a serial that
    /// matches nothing and is ignored instead of being read as "the pass in flight
    /// died".
    ///
    /// A pass with no child also has no planner to verify: `planning` goes with it,
    /// because a stale `PlanPass` left across the boundary would make the *next*
    /// pass's settle get diffed against this one's baseline — which is how a
    /// worker's settle ends up being reported as a plan of zero.
    pub async fn close(&mut self) {
        self.planning = None;
        self.set_claim(None);
        if let Some(mut old) = self.pi_rx.take() {
            let _ = old.rpc.kill().await;
        }
    }

    /// Take (or release) the loop's claim, and publish it in the same step.
    ///
    /// Setting the field and sending the event are one function rather than two
    /// because the failure mode of two is the one this whole ticket is about: a
    /// screen claiming to show a ticket the loop is not holding, or a loop holding
    /// one the screen never heard about. Nothing may set `claim` without the
    /// publish, so nothing can.
    fn set_claim(&mut self, claim: Option<ActiveBead>) {
        if self.claim == claim {
            return;
        }
        let _ = self.ev_tx.send(SessionEvent::ActiveBead { bead: claim.clone() });
        self.claim = claim;
    }

    /// The ticket this loop holds, if any. Read by the post-settle check to say
    /// *which* ticket it is reporting on.
    pub fn claim(&self) -> Option<&ActiveBead> {
        self.claim.as_ref()
    }

    /// What the pass in flight is working on — a bead id, or "the planner" — for
    /// the cancel sentences that have to name a thing rather than say "a pass".
    ///
    /// Derived from `claim` / `planning` rather than kept beside them: two fields
    /// that must be set and cleared together is one field too many, and a label
    /// that outlived the claim it came from would let a cancel name a bead the
    /// loop is not holding.
    pub fn pass_label(&self) -> Option<&str> {
        if let Some(claim) = &self.claim {
            return Some(claim.id.as_str());
        }
        if self.planning.is_some() {
            return Some("the planner");
        }
        None
    }

    /// `Esc`. Tell the in-flight worker to stop, and return the serial of the pass
    /// that was told, `None` when there was nothing to stop.
    ///
    /// The serial comes back rather than a bool because the caller has to arm a
    /// deadline against *that* pass: "did my abort land?" is only answerable about
    /// a pass with a name, and an untagged timer would eventually fire against a
    /// later pass's run and kill work nobody cancelled.
    ///
    /// Fire-and-forget, exactly like Pi's own Esc: `abort` is answered only once
    /// the run has unwound, and parking this loop on that answer would put the
    /// user's next keystroke behind the very run they are trying to stop. The end
    /// of the run arrives where every other end arrives — on the stream — and the
    /// caller's `aborted` flag is what makes it read as a cancellation rather than
    /// as a pass that finished (ADR-0002 Q3).
    pub fn abort_worker(&mut self) -> Option<u64> {
        let worker = self.pi_rx.as_ref()?;
        if let Err(e) = worker.rpc.abort() {
            tracing::warn!("{}: abort did not reach the worker: {e:#}", self.id);
        }
        Some(worker.serial)
    }

    /// Run the planner over `instructions`, remembering what the board looked like
    /// first so the result can be checked (looprs-k7v).
    ///
    /// The snapshot is taken *before* the child is spawned, which is the only order
    /// in which it means anything: a baseline captured after the run is not a
    /// baseline. And if that snapshot cannot be read, the pass is refused rather
    /// than run — an unverifiable planner is the exact failure this ticket is
    /// about, so buying a run that cannot be checked is worse than not buying it,
    /// and the honest error costs one `bd` call instead of a whole pi session.
    pub async fn launch_create_tickets(&mut self, instructions: &str) -> Result<()> {
        tracing::debug!("Launched tickets with these instructions: {}", instructions);
        // A human spoke. In beads mode this is the *only* door a deliberate human
        // instruction comes through, which makes it the acknowledgement that
        // releases the re-work guard (looprs-w7q): every automatic pass stays
        // answerable to the guard, and nothing restarts a refused ticket on a
        // timer or on a Tab.
        self.worked.clear();
        self.close().await;
        let baseline: HashSet<String> = list_status_with(&self.cfg.bd_bin, "open")
            .await?
            .into_iter()
            .map(|b| b.id)
            .collect();

        let args = vec!["--tools", "read,bash"];
        let worker = self.spawn_worker(&args)?;
        let said = worker.said.clone();
        self.set_step(BeadStep::CreateTickets);
        let prompt = generate_prompt(PLANNER, instructions);
        let disposition = worker.rpc.prompt(&prompt).await?;
        if disposition == "handled" {
            // pi took the prompt but started no run, so there is no settle coming
            // to trigger the verification either. Dropping the child and reporting
            // is the whole response; `planning` deliberately stays unset so this
            // dead pass cannot be mistaken for one that owes a verdict.
            drop(worker);
            return Err(anyhow!(
                "planner prompt was handled without starting a run; no tickets were requested"
            ));
        }
        self.planning = Some(PlanPass { baseline, said });
        self.pi_rx = Some(worker);
        // A planner pass holds no bead — `pass_label` answers "the planner" for it
        // off `planning`, which is set just above — because there is no ticket here
        // to claim, only one to invent.

        Ok(())
    }

    /// Is the pass in flight a planner's? Lets the no-settle path (`WorkerGone`)
    /// say which half of the loop it lost instead of blaming a worker that was
    /// never running.
    pub fn is_planning(&self) -> bool {
        self.planning.is_some()
    }

    /// Did the planner that just settled actually put tickets on the board?
    ///
    /// `agent_settled` means "the planner stopped talking". It is not, and never
    /// was, evidence of a plan: a planner that misread the instructions, refused
    /// them, or whose every `bd create` failed settled just as cleanly as one that
    /// worked. The board answers the real question, and answers it before a single
    /// worker is paid to read a plan that may not exist.
    ///
    /// Takes the pass rather than borrowing it: one planner pass gets exactly one
    /// verdict, so a second settle off the same child cannot re-diff a board that
    /// has since moved under it.
    async fn verify_plan(&mut self) -> PlanCheck {
        let Some(pass) = self.planning.take() else {
            return PlanCheck::NotPlanning;
        };
        let after = match list_status_with(&self.cfg.bd_bin, "open").await {
            Ok(beads) => beads,
            Err(e) => {
                return PlanCheck::Unverifiable {
                    stage: "reading the board after the planner ran",
                    reason: e.to_string(),
                };
            }
        };
        let created: Vec<Bead> = after
            .into_iter()
            .filter(|b| !pass.baseline.contains(b.id()))
            .collect();
        let said = pass.said.lock().unwrap().clone();
        if created.is_empty() {
            PlanCheck::NothingCreated { said }
        } else {
            PlanCheck::Created(created)
        }
    }

    /// Did the worker that just settled actually finish the ticket it was paid for?
    ///
    /// `agent_settled` means the worker stopped talking. It is not evidence that
    /// anything closed, and cannot be: the identical settle arrives whether the
    /// worker shipped the feature, hit a snag and said nothing, or ran out of
    /// steam mid-sentence with the ticket still `open` — and in the last case
    /// `bd ready` hands the *same bead* straight back to the next pass, which is
    /// how a self-advancing loop turns into an uncapped bill. Only the board can
    /// answer, so the loop asks it, one `bd show` per pass, before anything else
    /// is spawned (looprs-w7q).
    async fn verify_worker_pass(&self) -> PassOutcome {
        let Some(claim) = self.claim.clone() else {
            return PassOutcome::NothingHeld;
        };
        match show_with(&self.cfg.bd_bin, &claim.id).await {
            Ok(Some(bead)) if bead.is_closed() => PassOutcome::Closed(claim),
            Ok(Some(bead)) if bead.needs_human() => {
                PassOutcome::LeftForHuman(claim, bead.status())
            }
            Ok(Some(bead)) => PassOutcome::NotClosed(claim, bead.status()),
            // `bd` has never heard of the ticket we are holding. That is not
            // "closed", any more than an empty phone book is "nobody is sick".
            Ok(None) => PassOutcome::NotClosed(claim, BeadStatus::Unknown),
            Err(e) => PassOutcome::Unverifiable(claim, e.to_string()),
        }
    }
}

/// The worker prompt: the standing instructions, plus the one ticket this worker
/// owns and the fact that the harness has already claimed it.
///
/// The claim is stated rather than assumed, because the alternative — telling the
/// agent to go find work — is a race with every other hand on the board, and the
/// worker would end up doing (and billing) a different ticket than the one the
/// loop reports, guards and verifies. Hence `bd ready` is gone from the worker's
/// command list in `prompts::WORKER`: a prompt that advertises it invites the
/// agent to shop.
fn worker_prompt(claim: &ActiveBead) -> String {
    generate_prompt(
        WORKER,
        &format!(
            "Your assigned ticket: {id} — {title}\n\n\
             The harness has already run `bd update {id} --claim`, so this ticket is yours. \
             Work it and close it when the work is done. Do not look for other work and do not \
             pick a different ticket: {id} is the one being reported, guarded and paid for.\n",
            id = claim.id,
            title = claim.title,
        ),
    )
}

/// How many of a plan's tickets get listed before the note truncates.
///
/// Bounded because the transcript is the human's screen, not a dump: past a couple
/// of dozen lines the list is no longer readable anyway, and the count is the
/// number they actually wanted.
const MAX_PLAN_LISTED: usize = 20;

/// How many skipped tickets get listed before the note truncates.
const MAX_SKIPPED_LISTED: usize = 5;

/// "Here is the plan, before anybody spends money on it."
fn plan_note(tickets: &[Bead]) -> String {
    let mut out = format!("beads: planner created {} ticket(s):", tickets.len());
    for t in tickets.iter().take(MAX_PLAN_LISTED) {
        out.push_str(&format!("\n  {}: {}", t.id(), t.title()));
    }
    if let Some(rest) = tickets.len().checked_sub(MAX_PLAN_LISTED) {
        out.push_str(&format!("\n  … and {rest} more"));
    }
    out
}

/// The planner settled with nothing on the board to show for it.
///
/// The planner's own last words are quoted rather than paraphrased: the model
/// usually states its reason ("I need more information about X", "already
/// covered by looprs-1"), and that sentence is the thing the user needs in order
/// to re-instruct it. Without it, "no tickets" is a dead end.
fn no_plan_note(said: &str) -> String {
    let said = clip(said, 400);
    let reason = if said.is_empty() {
        "The planner left no message to explain itself.".to_string()
    } else {
        format!("The planner said: “{said}”")
    };
    format!(
        "beads: the planner finished and created no tickets — nothing was queued, and no workers were started. {reason}"
    )
}

/// The plan could not be checked, which is not the same sentence as "the plan is
/// empty" and must never be shortened into it.
fn unverifiable_note(stage: &str, reason: &str) -> String {
    format!(
        "beads: cannot verify the plan ({stage}): {}. Nothing was queued and no workers started — this is a board read failure, not an empty plan.",
        clip(reason, 300)
    )
}

/// "Worked it, did not close it, and I am not paying for that a second time."
///
/// Names both exits, because the point of stopping the loop is that a human now
/// decides — and a stop that does not say what would unblock it just moves the
/// confusion from the transcript to the terminal.
fn not_closed_note(claim: &ActiveBead, status: BeadStatus) -> String {
    format!(
        "beads: worked {} but `bd` says it is `{}`, not closed. The loop is stopped: `bd ready` hands this same ticket back, so another automatic pass would be a retry nobody asked for. Close it (`bd close {}`) or type a new instruction to let this loop work it again.",
        claim.id, status, claim.id
    )
}

/// A worker pass that could not be checked is not a worker pass that succeeded, and
/// the sentence must not be able to collapse into either of its neighbours.
fn unverified_pass_note(claim: &ActiveBead, reason: &str) -> String {
    format!(
        "beads: cannot verify whether {} was closed: {}. Nothing is queued and the loop is stopped — this is a board read failure, not a finished ticket. Type a new instruction to continue.",
        claim.id,
        clip(reason, 300)
    )
}

/// The worker pushed this one to a human; the rest of the board is still the
/// loop's business.
fn left_for_human_note(claim: &ActiveBead, status: BeadStatus) -> String {
    format!(
        "beads: {} was left `{}` by its worker, so a human has to move that one. The loop is moving on to the rest of the board and will not pick this ticket up again by itself.",
        claim.id, status
    )
}

/// A settle with no ticket behind it: nothing to verify, and something to report.
fn nothing_held_note() -> &'static str {
    "beads: a worker settled while this loop held no claimed ticket, so there is nothing to verify. The loop is stopped; type a new instruction to continue."
}

/// The guard speaking for itself, at the pass boundary: `bd ready` offered a ticket
/// this loop already burned a pass on, and it is still open. A refusal has to say
/// what it refused *and* what would unblock it, or the human reads the stop as a
/// crash.
fn already_worked_note(bead: &Bead) -> String {
    format!(
        "`bd ready` offered {} again, but this loop already worked it and `bd` says it is `{}`, not closed. It will not be started a second time by itself: `bd close {}`, or type a new instruction to continue.",
        bead.id(),
        bead.status(),
        bead.id()
    )
}

/// Bound a quoted child message so a rambling refusal cannot bury the transcript.
/// Char-based, not byte-based, so a multi-byte codepoint cannot be cut in half.
fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}\u{2026}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TerminalType;
    use crate::testing::{BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD, PiFake, process_alive};
    use tokio::time::timeout;

    const SECOND_BOARD: &str = r#"{
  "data": [
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    /// Generous, but bounded: a hang is a failure of this ticket, and a bounded test
    /// reports it instead of wedging the suite.
    const NO_HANG: Duration = Duration::from_secs(10);

    /// A board with two tickets the planner "created" mid-run.
    const PLAN_TWO_TICKETS: &str = r#"{
  "data": [
    {"id": "looprs-101", "title": "first planned ticket", "status": "open", "issue_type": "task"},
    {"id": "looprs-102", "title": "second planned ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    /// A board carrying one ticket that predates any planner.
    const PRE_EXISTING: &str = r#"{
  "data": [
    {"id": "looprs-old", "title": "was already here", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    /// The same pre-existing ticket, plus one the planner added on top of it. The
    /// diff must report only the second one.
    const OLD_PLUS_ONE_NEW: &str = r#"{
  "data": [
    {"id": "looprs-old", "title": "was already here", "status": "open", "issue_type": "task"},
    {"id": "looprs-101", "title": "first planned ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    /// Two open tickets, so "the loop moved on to the next one" is observable
    /// rather than merely not-stopped.
    const TWO_OPEN: &str = r#"{
  "data": [
    {"id": "looprs-26r", "title": "Beads loop never self-starts", "status": "open", "issue_type": "bug"},
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    /// The first ticket blocked and the second left workable: the shape `bd ready`
    /// is not supposed to produce, and the shape the loop has to cope with anyway.
    const BLOCKED_THEN_READY: &str = r#"{
  "data": [
    {"id": "looprs-26r", "title": "waiting on somebody else", "status": "blocked", "issue_type": "bug"},
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    fn fakes_cfg(fakes: &Fakes) -> SessionConfig {
        SessionConfig {
            pi_bin: fakes.pi_bin().to_string(),
            bd_bin: fakes.bd_bin().to_string(),
            ..Default::default()
        }
    }

    /// A bare loop, with its two output streams held open by the test.
    ///
    /// The control receiver comes back rather than being dropped: these tests drive
    /// the loop by calling it directly, so nothing is expected on that mailbox —
    /// but dropping the receiver would silently discard the worker's control edges,
    /// and a test that silently discards the thing under test is worse than one that
    /// fails.
    fn loop_with(
        fakes: &Fakes,
    ) -> (
        BeadsLoop,
        mpsc::UnboundedReceiver<SessionEvent>,
        mpsc::UnboundedReceiver<BeadsCmd>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel::<BeadsCmd>();
        let id = SessionId::new(TerminalType::Beeds, 0);
        (BeadsLoop::new(id, tx, ctl_tx, fakes_cfg(fakes)), rx, ctl_rx)
    }

    /// One event, described as a stable string so a failing assertion prints
    /// something readable instead of four nested enums.

    fn describe(m: &SessionEvent) -> String {
        match m {
            SessionEvent::BeadStep(BeadStep::AwaitInput) => "step:await".into(),
            SessionEvent::BeadStep(BeadStep::CreateTickets) => "step:plan".into(),
            SessionEvent::BeadStep(BeadStep::WorkTickets) => "step:work".into(),
            // The claim, as the UI is told it: which ticket the loop holds, and
            // when it lets go of it.
            SessionEvent::ActiveBead {
                bead: Some(bead),
            } => format!("active:{}", bead.id),
            SessionEvent::ActiveBead { bead: None } => "active:-".into(),
            SessionEvent::Error(text) => format!("error: {text}"),
            SessionEvent::System(text) => format!("system: {text}"),
            SessionEvent::RestoreInput { text } => format!("restore: {text}"),
            SessionEvent::Agent(PiEvent::AgentSettled) => "agent_settled".into(),
            SessionEvent::Agent(_) => "agent".into(),
            SessionEvent::Exited { .. } => "session-down".into(),
            SessionEvent::BashOutput { .. } => "bash".into(),
            // Only Bash mode ever takes a screen over; a beads session saying it did
            // would be a bug worth seeing in the test output.
            SessionEvent::ScreenHeld { active } => format!("screen:{active}"),
        }
    }

    /// Read events until one satisfies `done`, returning everything said on the way.
    ///
    /// "Every pass that ends" is not one end state — a verified plan stops at
    /// `step:work`, a refusal stops at `step:await` — so the parked variant below is
    /// just this with a predicate, and tests that care about ordering can use their own.
    async fn drain_until<F>(rx: &mut mpsc::UnboundedReceiver<SessionEvent>, done: F) -> Vec<String>
    where
        F: Fn(&str) -> bool,
    {
        let mut out = Vec::new();
        loop {
            let line = match timeout(NO_HANG, rx.recv()).await {
                Ok(Some(m)) => describe(&m),
                Ok(None) => panic!("the session stream closed first: {out:?}"),
                Err(_) => panic!("no matching event within {NO_HANG:?}: {out:?}"),
            };
            let stop = done(&line);
            out.push(line);
            if stop {
                return out;
            }
        }
    }

    /// Snapshot of what the loop told the UI, as stable strings.
    fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(describe(&m));
        }
        out
    }

    /// Read events until the loop lands in `AwaitInput`, and return everything it
    /// said on the way.
    ///
    /// "Parked" is the observable end state of every one of these tests, and it is
    /// the state a human has to be able to reach: a loop that keeps saying it is
    /// working while nothing is running is a loop that hid the input box forever.
    async fn drain_until_parked(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        drain_until(rx, |line| line == "step:await").await
    }

    fn has_error(msgs: &[String]) -> bool {
        msgs.iter().any(|m| m.starts_with("error: "))
    }

    /// The last thing the loop called an error. "Last" because the park note is
    /// written after the verdict in some paths, and the assertion should be about
    /// the verdict rather than about whichever string happened to land first.
    fn last_error(msgs: &[String]) -> Option<String> {
        msgs.iter()
            .rev()
            .find(|m| m.starts_with("error: "))
            .cloned()
    }

    /// Send a planner instruction the way the beads input box sends it, and return
    /// only once the pass is demonstrably live — a test that plans "and hopes" is a
    /// test that settles the wrong run half the time.
    async fn start_planner(s: &mut BeadsSession, text: &str) -> u64 {
        s.send_text(text.to_string()).unwrap();
        assert!(s.quiesce().await, "the submit was handled");
        assert_eq!(
            s.status(),
            SessionStatus::Running,
            "the planner pass is live"
        );
        s.in_flight().expect("the planner pass is in flight")
    }

    /// How many times the harness asked the board for the open tickets — the two
    /// halves of the planner diff, counted separately from every other `bd` call.
    fn board_reads(fakes: &Fakes) -> usize {
        fakes
            .bd_log()
            .iter()
            .filter(|l| l.starts_with("list --status open"))
            .count()
    }

    fn bead(id: &str, title: &str) -> Bead {
        bead_status(id, title, crate::services::bd::BeadStatus::Open)
    }

    /// As [`bead`], with a chosen status — the knob for "this ticket is not for a
    /// worker", which the guard tests use without needing a board file for it.
    fn bead_status(id: &str, title: &str, status: crate::services::bd::BeadStatus) -> Bead {
        Bead {
            id: id.to_string(),
            title: title.to_string(),
            status: crate::services::bd::BeadStatusFallback::Known(status),
            issue_type: crate::services::bd::BeadIssueType::Task,
        }
    }

    /// Tell the fake `bd` what status a ticket now has, for the one read the loop
    /// cannot fake for itself: `bd show <id> --json`, the post-settle check
    /// (looprs-w7q).
    ///
    /// Tests that want the loop to *keep going* have to say the worker closed its
    /// ticket, because a settle that leaves the ticket open now stops the loop on
    /// purpose. That is the point of the guard, and it is why every "the settle
    /// drove the next pass" test below carries this line: the loop advances on a
    /// closed ticket, not on a quiet one.
    fn show_status(fakes: &Fakes, id: &str, status: &str) {
        fakes.set_show(&format!(
            r#"{{"id":"{id}","title":"whatever {id} was called","status":"{status}","issue_type":"task"}}"#
        ));
    }

    /// As [`show_status`], with the ticket finished — the shape of a worker that
    /// did the job.
    fn show_closed(fakes: &Fakes, id: &str) {
        show_status(fakes, id, "closed");
    }

    /// A constructed loop must not have touched any process. The old code spawned a
    /// pi child inside new() and never prompted it.
    #[tokio::test]
    async fn constructing_a_loop_spawns_nothing() {
        let fakes = Fakes::new("ctor", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
        let (l, mut rx, _ctl) = loop_with(&fakes);

        assert!(l.is_awaiting_input(), "a new loop starts parked");
        assert_eq!(fakes.pi_spawns(), 0, "new() must not spawn a pi child");
        assert_eq!(fakes.bd_calls(), 0, "new() must not even query the board");
        assert!(drain(&mut rx).is_empty());
    }

    /// The bug: with a non-empty board, launching looprs sat forever with a live,
    /// idle pi child. Driving the loop must start real work with no human input.
    #[tokio::test]
    async fn a_non_empty_board_self_starts_a_prompted_worker() {
        let fakes = Fakes::new("self-start", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(
            timeout(NO_HANG, l.next()).await.is_ok(),
            "next() hung instead of starting a worker"
        );

        assert_eq!(fakes.pi_spawns(), 1, "exactly one worker spawned");
        let prompts = fakes.pi_prompts();
        assert_eq!(prompts.len(), 1, "the worker was prompted, not left idle");
        assert!(
            prompts[0].contains("technical software engineer"),
            "worker prompt missing: {}",
            prompts[0]
        );
        assert!(
            prompts[0].contains("looprs-26r"),
            "worker was not told which bead to work: {}",
            prompts[0]
        );
        // Invariant for the whole run: no child exists that was never prompted.
        assert_eq!(fakes.pi_spawns(), fakes.pi_prompts().len());
        assert!(matches!(l.get_step(), BeadStep::WorkTickets));
        assert!(!l.is_awaiting_input());
        assert!(drain(&mut rx).contains(&"step:work".to_string()));
    }

    #[tokio::test]
    async fn an_empty_board_parks_without_spawning() {
        let fakes = Fakes::new("empty", PiFake::Started, BdFake::Ok, EMPTY_BOARD);
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(timeout(NO_HANG, l.next()).await.is_ok());

        assert_eq!(fakes.pi_spawns(), 0, "empty board must not spawn anything");
        assert!(l.is_awaiting_input());
        let msgs = drain(&mut rx);
        assert!(msgs.contains(&"step:await".to_string()), "{msgs:?}");
        assert!(
            msgs.iter().any(|m| m.contains("board empty")),
            "parking should say why: {msgs:?}"
        );
    }

    /// Switching passes must reap the previous session: no orphan pi children, and no
    /// unprompted child in the gap between sessions.
    #[tokio::test]
    async fn driving_the_loop_reaps_the_previous_worker() {
        let fakes = Fakes::new("reap", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut l, _rx, _ctl) = loop_with(&fakes);

        timeout(NO_HANG, l.next()).await.unwrap();
        let first = fakes.pi_pids()[0];
        assert!(process_alive(first), "worker should be running");

        // A second pass onto a different ticket: the old child goes away, the new one works.
        fakes.set_board(SECOND_BOARD);
        timeout(NO_HANG, l.next()).await.unwrap();
        assert!(
            !process_alive(first),
            "pid {first} survived the pass boundary: orphaned child"
        );
        let second = fakes.pi_pids()[1];
        assert_ne!(first, second);
        assert!(process_alive(second), "second worker should be running");

        // Draining the board parks the loop and reaps the live worker.
        fakes.set_board(EMPTY_BOARD);
        timeout(NO_HANG, l.next()).await.unwrap();
        assert!(!process_alive(second), "parked loop left a child running");
        assert_eq!(fakes.pi_spawns(), 2);
        assert_eq!(fakes.pi_prompts().len(), 2, "every child got a prompt");
        assert!(l.is_awaiting_input());
        assert!(l.pi_rx.is_none(), "parked loop must not hold a session");
    }

    /// pi dying during startup must surface as a transcript error and park the loop,
    /// never as a hang or a panic in main.
    #[tokio::test]
    async fn a_pi_that_dies_during_startup_is_reported() {
        let fakes = Fakes::new(
            "dead-pi",
            PiFake::DiesImmediately,
            BdFake::Ok,
            ONE_BEADED_BOARD,
        );
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(
            timeout(NO_HANG, l.next()).await.is_ok(),
            "next() hung on a pi that died instead of reporting"
        );

        let msgs = drain(&mut rx);
        assert!(has_error(&msgs), "death should be reported: {msgs:?}");
        assert!(
            !msgs.contains(&"step:work".to_string()),
            "a dead worker must not claim to be working: {msgs:?}"
        );
        assert!(l.is_awaiting_input());
        assert!(l.pi_rx.is_none());
    }

    #[tokio::test]
    async fn a_failing_bd_is_reported_and_parks() {
        let fakes = Fakes::new("bad-bd", PiFake::Started, BdFake::Fails, EMPTY_BOARD);
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(timeout(NO_HANG, l.next()).await.is_ok());

        let msgs = drain(&mut rx);
        assert!(
            has_error(&msgs),
            "`bd ready` failure should surface: {msgs:?}"
        );
        assert_eq!(fakes.pi_spawns(), 0, "no worker without a board read");
        assert!(l.is_awaiting_input());
    }

    #[tokio::test]
    async fn a_refused_prompt_is_reported_and_keeps_no_session() {
        let fakes = Fakes::new("refused", PiFake::Rejects, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(timeout(NO_HANG, l.next()).await.is_ok());

        assert!(has_error(&drain(&mut rx)));
        assert!(l.is_awaiting_input());
        assert!(l.pi_rx.is_none());
    }

    /// `disposition: "handled"` means pi took the prompt but started no run, so no
    /// `agent_settled` will ever arrive to advance the loop. Holding that session
    /// open would be the same idle-child trap this ticket is about.
    #[tokio::test]
    async fn a_handled_prompt_does_not_leave_an_idle_worker() {
        let fakes = Fakes::new("handled", PiFake::Handled, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(timeout(NO_HANG, l.next()).await.is_ok());

        let msgs = drain(&mut rx);
        assert!(
            msgs.iter()
                .any(|m| m.starts_with("error: ") && m.contains("handled")),
            "{msgs:?}"
        );
        assert!(l.is_awaiting_input());
        assert!(
            l.pi_rx.is_none(),
            "handled session must be dropped, not kept"
        );
    }

    // ------------------ the session handle: entry, parking, resumption ------------------

    fn beads(
        fakes: &Fakes,
        generation: u64,
    ) -> (BeadsSession, mpsc::UnboundedReceiver<SessionEvent>) {
        // The un-boxed constructor: the test keeps a concrete, cloneable handle and
        // the event stream the Router would otherwise pump.
        BeadsSession::build(
            SessionId::new(TerminalType::Beeds, generation),
            &fakes_cfg(fakes),
        )
        .expect("beads session must start")
    }

    /// Entering the beads mode is what self-starts the loop — the same "opens
    /// working, not idling" behavior the app has always had, now expressed as
    /// `set_active(true)` rather than a special call in main.
    #[tokio::test]
    async fn entering_the_mode_starts_one_pass_and_only_one() {
        let fakes = Fakes::new("enter", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut s, _rx) = beads(&fakes, 1);

        assert_eq!(
            s.status(),
            SessionStatus::NotStarted,
            "start spawns nothing"
        );

        s.set_active(true).unwrap();
        assert!(s.quiesce().await, "the session task acked");
        assert_eq!(fakes.pi_spawns(), 1, "entering the mode ran one pass");
        assert_eq!(s.status(), SessionStatus::Running);

        // Entering again must not run a second pass while a worker is live.
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        assert_eq!(fakes.pi_spawns(), 1, "re-entering cannot double-spawn");
    }

    /// ADR-0002 Q3, DrainThenPark: a Tab away mid-run kills nothing and starts
    /// nothing, and every settle that arrives off-screen is remembered as *one*
    /// pass to run when the user comes back.
    #[tokio::test]
    async fn switching_away_drains_then_parks_and_resuming_never_double_spawns() {
        let fakes = Fakes::new("park", PiFake::Started, BdFake::ShowStatus, ONE_BEADED_BOARD);
        let (mut s, _rx) = beads(&fakes, 2);

        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        let worker = fakes.pi_pids()[0];
        assert!(process_alive(worker), "worker is running");

        // Tab away mid-run.
        s.set_active(false).unwrap();
        assert!(s.quiesce().await);
        assert!(
            process_alive(worker),
            "a Tab must not kill the in-flight worker: that work is already paid for"
        );

        // Three settles arrive while hidden — the loop's own worker, tagged with
        // the pass that made them, on a ticket that has now closed so that none of
        // them is a stop condition (looprs-w7q). Three *un-closed* settles would
        // be a different test: the first one stops the loop.
        fakes.set_board(SECOND_BOARD);
        show_closed(&fakes, "looprs-26r");
        let pass = s.in_flight().expect("a pass is in flight");
        for _ in 0..3 {
            s.cmd
                .send(BeadsCmd::WorkerSettled { serial: pass })
                .unwrap();
        }
        assert!(s.quiesce().await);
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "no new pass may start while the mode is hidden"
        );
        assert!(process_alive(worker), "and still no kill");

        // Tab back: resumes, once.
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        assert_eq!(
            fakes.pi_spawns(),
            2,
            "three deferred settles must coalesce into exactly one resumed pass"
        );
        assert!(
            !process_alive(worker),
            "the resumed pass reaped the old worker at its own pass boundary"
        );
        assert_eq!(s.status(), SessionStatus::Running);
    }

    /// Shutdown reaps the worker, says so once, and leaves nothing behind.
    #[tokio::test]
    async fn shutdown_reaps_the_worker_and_reports_the_session_gone() {
        let fakes = Fakes::new(
            "shutdown-beads",
            PiFake::Started,
            BdFake::Ok,
            ONE_BEADED_BOARD,
        );
        let (mut s, mut rx) = beads(&fakes, 3);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        let worker = fakes.pi_pids()[0];
        assert!(process_alive(worker));

        s.shutdown().unwrap();
        let evs = collect_until_down(&mut rx).await;
        assert!(!process_alive(worker), "shutdown must reap the worker");
        assert_eq!(
            evs.iter()
                .filter(|e| matches!(e, SessionEvent::Exited { .. }))
                .count(),
            1,
            "exactly one Exited: {evs:?}"
        );
        assert_eq!(s.status(), SessionStatus::Dead);
    }

    async fn collect_until_down(
        rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
    ) -> Vec<SessionEvent> {
        let mut out = Vec::new();
        loop {
            match timeout(Duration::from_secs(10), rx.recv()).await {
                Ok(Some(ev)) => {
                    let down = matches!(ev, SessionEvent::Exited { .. });
                    out.push(ev);
                    if down {
                        return out;
                    }
                }
                // Stream closed, or the wait expired: stop collecting. The
                // exactly-one-Exited guarantee itself is pinned in router::tests.
                Ok(None) => return out,
                Err(_) => return out,
            }
        }
    }

    // ------------- who drives the loop? (looprs-msj) -------------
    //
    // Four tests, one claim each: the transition happens inside this session, off
    // this session's own worker, and off nothing else. None of them has an App, an
    // input mode, or a `UiCommand` in it, which is the point — the App cannot be
    // part of the path any more because there is nothing left for it to send.

    /// **The acceptance case, run end to end with real processes.** A beads worker
    /// settles, and the next pass starts. There is no UI in this test in any form:
    /// the only things that ever touched this session were one `set_active` and the
    /// fake worker's own stdout, so the advance cannot have come from anywhere but
    /// the bead's side of the boundary.
    #[tokio::test]
    async fn the_loop_takes_its_next_pass_from_its_own_workers_settle() {
        let fakes = Fakes::new(
            "settle-drives-loop",
            PiFake::Chat,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        // `quiesce`, not a log poll: the fake logs the prompt *before* it answers,
        // so a status asserted against the log races the answer. Quiesce is FIFO on
        // the session's own mailbox — when it returns, the pass has started and the
        // status mirror has been published behind it.
        assert!(s.quiesce().await, "the entry command was handled");
        assert_eq!(fakes.pi_spawns(), 1, "one pass, from entering the mode");
        assert_eq!(s.in_flight(), Some(1));
        // The harness claimed the ticket before it bought the worker, so the id it
        // claimed is the id the worker was prompted with and the id the settle is
        // about to be checked against (looprs-w7q).
        assert!(fakes.claimed("looprs-26r"), "{:?}", fakes.bd_log());

        // The worker did its job: the ticket is closed, and the next ready bead is
        // a *different* one. Both halves are load-bearing — the loop advances on a
        // closed ticket, and an un-closed one stops it dead rather than buying a
        // second pass on the same thing.
        fakes.set_board(SECOND_BOARD);
        show_closed(&fakes, "looprs-26r");
        fakes.settle();
        fakes.wait_for_pi_spawns(2).await;

        assert_eq!(
            fakes.pi_spawns(),
            2,
            "the settle drove the next pass — no command told anyone to"
        );
        assert_eq!(
            fakes.pi_prompts().len(),
            2,
            "and the new pass was prompted, not left idle the way this loop used to start"
        );
        // `wait_for_pi_spawns` returns the moment the fake logs the spawn, which is
        // *inside* the session task's still-running settle handler — before that
        // handler returns and republishes the `in_flight` mirror. This assertion
        // raced that gap (a flake it had been carrying since looprs-msj); the seam
        // is the only honest way to read a mirror written by another task.
        assert!(
            s.quiesce().await,
            "the settle-driven pass finished starting and published its serial"
        );
        assert_eq!(s.in_flight(), Some(2), "the new pass is the live one");
        assert!(
            drain(&mut rx).contains(&"step:work".to_string()),
            "and the UI was told, as a render, not asked, as a command"
        );

        // Nothing drives a *third* pass: the loop moves when a pass settles and
        // nowhere else, so it sits waiting rather than running the board by itself.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(fakes.pi_spawns(), 2, "no settle, no pass");
    }

    /// A settle only moves the loop when it names the pass that is actually in
    /// flight. A late or duplicated tail from an already-retired worker looks
    /// identical on the wire otherwise, and acting on it would kill the live pass
    /// — work already paid for — to start a pass nobody asked for.
    #[tokio::test]
    async fn a_settle_from_a_pass_this_loop_does_not_own_moves_nothing() {
        let fakes = Fakes::new(
            "stale-settle",
            PiFake::Started,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        let (mut s, _rx) = beads(&fakes, 1);

        // (a) Nothing is running: a settle cannot conjure work out of nowhere.
        s.cmd.send(BeadsCmd::WorkerSettled { serial: 1 }).unwrap();
        assert!(s.quiesce().await);
        assert_eq!(
            fakes.pi_spawns(),
            0,
            "a settle with no pass behind it starts nothing"
        );
        assert_eq!(s.status(), SessionStatus::NotStarted);

        // (b) A real pass, retired by a real settle, which then arrives late again.
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        let first = s.in_flight().expect("the first pass is live");
        // The ticket finished and the board moved on, so the pass below is retired
        // by a *successful* settle rather than stopped by the un-closed-ticket
        // guard — which would halt the loop for a reason that has nothing to do
        // with what this test is about.
        fakes.set_board(SECOND_BOARD);
        show_closed(&fakes, "looprs-26r");
        s.cmd
            .send(BeadsCmd::WorkerSettled { serial: first })
            .unwrap();
        assert!(s.quiesce().await);
        assert_eq!(fakes.pi_spawns(), 2, "the real settle drove the next pass");
        let second = s.in_flight().expect("the second pass is live");
        assert_ne!(first, second, "each pass gets its own serial");

        s.cmd
            .send(BeadsCmd::WorkerSettled { serial: first })
            .unwrap();
        assert!(s.quiesce().await);
        assert_eq!(
            fakes.pi_spawns(),
            2,
            "the late echo of a retired pass must not come over the top of the live one"
        );
        assert!(
            process_alive(fakes.pi_pids()[1]),
            "the live worker was not touched"
        );
        assert_eq!(s.in_flight(), Some(second));
    }

    /// **ADR-0002 Q3, the rule that makes the settle path not a tautology**: a
    /// settle means "a pass finished" only when nobody cancelled it. An aborted
    /// worker settles on the way out — that is how pi unwinds — and reading that as
    /// "next bead" would spend a turn on the next ticket *because* the user pressed
    /// Esc, which is the exact opposite of what Esc is for.
    #[tokio::test]
    async fn an_aborted_pass_parks_instead_of_advancing() {
        let fakes = Fakes::new("abort-parks", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        assert!(s.quiesce().await, "the entry command was handled");
        let worker = fakes.pi_pids()[0];
        assert!(process_alive(worker), "a pass is running");

        s.abort().unwrap();
        let msgs = drain_until_parked(&mut rx).await;

        assert_eq!(
            fakes.pi_spawns(),
            1,
            "the abort parked the loop; it did not take the next bead: {msgs:?}"
        );
        assert!(
            !process_alive(worker),
            "a parked beads loop holds no warm child — the mode is cold by policy"
        );
        assert!(
            msgs.iter().any(|m| m.contains("cancel")),
            "the park says why, rather than looking like the board ran dry: {msgs:?}"
        );
        assert_eq!(s.status(), SessionStatus::Idle, "waiting on a human");
        assert_eq!(s.in_flight(), None, "nothing is in flight any more");
        assert_eq!(
            fakes.pi_verbs().iter().filter(|v| *v == "prompt").count(),
            1,
            "exactly one prompt ever went out: {:?}",
            fakes.pi_verbs()
        );
    }

    /// **Acceptance: Esc during a beads worker names the bead it is cancelling.**
    ///
    /// "cancelling…" about a faceless pass is not much of an answer: the thing the
    /// user is stopping is a *bead*, and the only way to be sure you cancelled the
    /// one you meant is for the loop to say which one it was.
    #[tokio::test]
    async fn esc_names_the_bead_it_is_cancelling() {
        let fakes = Fakes::new("esc-names", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await, "the pass started");
        assert_eq!(s.status(), SessionStatus::Running);
        drain(&mut rx);

        s.abort().unwrap();
        // Nothing has been settled yet, so this window is mostly the session's own
        // words — but the fake unwinds quickly, so the park may land inside it
        // too, and the assertions read the whole of it either way.
        let mut got =
            crate::testing::collect_within(&mut rx, Duration::from_millis(800), |ev| describe(&ev))
                .await;
        let ack = got
            .iter()
            .position(|l| l.starts_with("system: cancelling") && l.contains("looprs-26r"));
        assert!(ack.is_some(), "the cancel did not name the bead: {got:?}");
        if !got.iter().any(|l| l == "step:await") {
            got.extend(drain_until_parked(&mut rx).await);
        }
        // Ordering, not absence: "cancelled" must not arrive before
        // "cancelling". A completion word that beats the acknowledgement leaves
        // the same silence in front of it that no acknowledgement at all would.
        if let Some(done) = got.iter().position(|l| l.contains("cancelled")) {
            assert!(
                ack.unwrap() < done,
                "the pass reported itself cancelled before the cancel was acknowledged: {got:?}"
            );
        }
        // The point of naming it: this is the pass that got stopped, and stopping
        // it does not buy another one.
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "naming the bead cancelled that pass and only that pass: {got:?}"
        );
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// Esc on a beads loop with nothing running is invisible: no note, no error,
    /// no worker, and no park — the loop was already waiting for a human, and a
    /// no-op that announces itself is indistinguishable from a cancel that did
    /// something.
    #[tokio::test]
    async fn esc_on_an_idle_loop_says_nothing_and_starts_nothing() {
        let fakes = Fakes::new("esc-idle-loop", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        assert_eq!(fakes.pi_spawns(), 0, "an empty board starts no worker");
        drain(&mut rx);

        s.abort().unwrap();
        assert!(s.quiesce().await, "the Esc was handled");
        let late =
            crate::testing::collect_within(&mut rx, cancel::GRACE + Duration::from_secs(1), |ev| {
                describe(&ev)
            })
            .await;
        assert!(late.is_empty(), "an idle Esc made noise: {late:?}");
        assert_eq!(fakes.pi_spawns(), 0, "and started nothing");
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// **The escalation: a worker that answers the abort and keeps going.**
    ///
    /// This is the mode where the stall matters most, because a beads worker is a
    /// `pi` child being paid by the token and the loop self-advances. Killed, the
    /// loop parked, and — the part the board cannot show the user from here — the
    /// bead named as still claimed, because a cancelled worker leaves its ticket
    /// `in_progress` and that is the state the claim/close guard has to be able to
    /// see (looprs-w7q).
    #[tokio::test]
    async fn a_worker_that_ignores_the_abort_is_killed_and_leaves_the_bead_named() {
        let fakes = Fakes::new("abort-stubborn", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
        fakes.stubborn_pi(true);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await, "the pass started");
        let worker = fakes.pi_pids()[0];
        assert_eq!(s.status(), SessionStatus::Running);

        s.abort().unwrap();
        assert!(s.quiesce().await, "the Esc was handled");
        assert_eq!(
            s.status(),
            SessionStatus::Aborting,
            "waiting on an unwind that is not coming"
        );
        assert!(
            process_alive(worker),
            "the abort alone has not stopped it (that is the point of this fake)"
        );

        let late =
            crate::testing::collect_within(&mut rx, cancel::GRACE + Duration::from_secs(6), |ev| {
                describe(&ev)
            })
            .await;
        assert!(
            !process_alive(worker),
            "the stalled worker was left running — and still billing"
        );
        assert!(
            late.iter().any(|l| l.starts_with("error:")
                && l.contains("looprs-26r")
                && l.contains("killed")),
            "the stall did not say what it did, to which bead: {late:?}"
        );
        assert!(
            late.iter().any(|l| l.contains("stays claimed")),
            "the bead the worker leaves behind was not named as claimed: {late:?}"
        );
        assert!(
            late.iter().any(|l| l == "step:await"),
            "the loop stayed 'working' instead of handing the box back: {late:?}"
        );
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "killing a stalled worker is not a restart: a cancel that auto-retries is a respawn storm"
        );
        assert_eq!(
            s.status(),
            SessionStatus::Idle,
            "parked, waiting on a human"
        );
        assert_eq!(s.in_flight(), None, "nothing in flight");
    }

    /// The symmetric edge of the settle path. A worker that dies mid-run never
    /// sends `agent_settled`, so without handling the stream end the loop sits
    /// claiming it is working forever — with the input box hidden behind that
    /// claim. It must come back to the human instead.
    #[tokio::test]
    async fn a_worker_that_dies_mid_pass_parks_the_loop_instead_of_hanging_it() {
        let fakes = Fakes::new("worker-dies", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        assert!(
            s.quiesce().await,
            "the pass started before anything killed it"
        );
        let victim = fakes.pi_pids()[0];
        assert_eq!(s.status(), SessionStatus::Running);

        crate::testing::kill_pid(victim);

        let msgs = drain_until_parked(&mut rx).await;
        assert!(
            has_error(&msgs),
            "the death is reported, not swallowed: {msgs:?}"
        );
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "parking is not a restart: a crash that auto-retries is a respawn storm"
        );
        assert!(!process_alive(victim));
        assert_eq!(
            s.status(),
            SessionStatus::Idle,
            "the loop is back in the human's hands, so the box comes back too"
        );
    }

    // ---------------- the planner is checked before anybody works (looprs-k7v) ----------------
    //
    // `agent_settled` was being read as "there is a plan". It is not. Every test
    // below pins one of the four verdicts the diff can return, and the two things
    // the ticket asked for: a plan of zero is impossible to miss, and a plan of N
    // is on screen before the first worker is paid to read it.

    /// **The acceptance case.** A planner that creates nothing must not look like a
    /// successful no-op: it must be an error, it must say so, and it must not
    /// queue a worker.
    #[tokio::test]
    async fn a_plan_that_creates_no_tickets_is_a_loud_error_not_a_silent_idle() {
        let fakes = Fakes::new("plan-nothing", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        drain(&mut rx); // the "board empty" park note from entering the mode

        start_planner(&mut s, "gibberish that plans nothing").await;
        assert_eq!(
            board_reads(&fakes),
            1,
            "the board was snapshotted before the planner ran: {:?}",
            fakes.bd_log()
        );

        fakes.settle(); // the planner finishes, having written nothing

        let msgs = drain_until_parked(&mut rx).await;
        let err = last_error(&msgs)
            .unwrap_or_else(|| panic!("a zero-ticket plan must be an error: {msgs:?}"));
        assert!(err.contains("created no tickets"), "{err}");
        assert!(
            err.contains("reply 1"),
            "the planner's own final message is quoted, not paraphrased: {err}"
        );
        assert!(
            !msgs.contains(&"step:work".to_string()),
            "nothing was queued, so no worker runs: {msgs:?}"
        );
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "exactly the planner ran — no worker was bought: {:?}",
            fakes.pi_pids()
        );
        assert_eq!(
            board_reads(&fakes),
            2,
            "one snapshot before, one diff after: {:?}",
            fakes.bd_log()
        );
        assert_eq!(s.status(), SessionStatus::Idle, "the box comes back");
        assert_eq!(s.in_flight(), None, "and nothing is left in flight");
    }

    /// **The other acceptance case**, including its ordering: the human sees the
    /// plan — id and title per ticket — before the first worker starts burning
    /// tokens on it, so a bad plan can be Esc'd while it is still cheap.
    #[tokio::test]
    async fn a_successful_plan_is_listed_before_the_first_worker_starts() {
        let fakes = Fakes::new("plan-listed", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        drain(&mut rx);

        start_planner(&mut s, "two tickets, please").await;
        fakes.set_board(PLAN_TWO_TICKETS); // the planner created them mid-run
        fakes.settle();

        let msgs = drain_until(&mut rx, |m| m == "step:work").await;
        let listed = msgs
            .iter()
            .position(|m| m.contains("planner created 2"))
            .unwrap_or_else(|| panic!("the plan was never listed: {msgs:?}"));
        let working = msgs.iter().position(|m| m == "step:work").unwrap();
        assert!(
            listed < working,
            "the plan is on screen before the first worker runs: {msgs:?}"
        );
        let note = &msgs[listed];
        assert!(note.contains("looprs-101: first planned ticket"), "{note}");
        assert!(note.contains("looprs-102: second planned ticket"), "{note}");
        assert!(
            msgs[..=listed].iter().all(|m| !m.starts_with("error: ")),
            "a plan that worked is not reported as a failure: {msgs:?}"
        );

        assert_eq!(fakes.pi_spawns(), 2, "planner, then exactly one worker");
        let prompts = fakes.pi_prompts();
        assert!(
            prompts[0].contains("engineering manager"),
            "the first child was the planner"
        );
        assert!(
            prompts[1].contains("Your assigned ticket: looprs-101 — first planned ticket"),
            "and the second was put to work on a named ticket from the plan: {}",
            prompts[1]
        );
    }

    /// The diff is a set difference on ticket ids, not a count of the board. A
    /// ticket that was already there is not evidence of a plan, and reporting it as
    /// one would let "nothing was created" pass for "here is the plan".
    #[tokio::test]
    async fn tickets_that_predate_the_planner_are_not_reported_as_planned() {
        let fakes = Fakes::new("plan-diff", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        drain(&mut rx);
        // A ticket that was already on the board before the planner was asked for
        // anything. It goes in after entering the mode and before the submit, so
        // the loop is not off working it by the time the baseline is taken.
        fakes.set_board(PRE_EXISTING);
        start_planner(&mut s, "add one more ticket").await;
        fakes.set_board(OLD_PLUS_ONE_NEW);
        fakes.settle();

        let msgs = drain_until(&mut rx, |m| m == "step:work").await;
        let note = msgs
            .iter()
            .find(|m| m.contains("planner created 1"))
            .unwrap_or_else(|| panic!("only the new ticket should count: {msgs:?}"));
        assert!(note.contains("looprs-101"), "{note}");
        assert!(
            !note.contains("looprs-old"),
            "the pre-existing ticket is not part of the plan: {note}"
        );
    }

    /// A board that cannot be read at verification time is *unverifiable*, which is
    /// a different sentence from "empty" and must never be shortened into it — the
    /// user's next action differs (fix `bd` vs. re-word the instruction).
    #[tokio::test]
    async fn a_board_that_cannot_be_read_after_planning_is_unverifiable_not_empty() {
        let fakes = Fakes::new("plan-unreadable", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        start_planner(&mut s, "plan something").await;
        fakes.fail_bd(true); // the snapshot got through; the diff read does not
        fakes.settle();

        let msgs = drain_until_parked(&mut rx).await;
        let err = last_error(&msgs)
            .unwrap_or_else(|| panic!("an unreadable board must be an error: {msgs:?}"));
        assert!(err.contains("cannot verify"), "{err}");
        assert!(
            !err.contains("created no tickets"),
            "a read failure must not be reported as an empty plan: {err}"
        );
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "no worker runs on a plan that could not be checked"
        );
        assert!(!msgs.contains(&"step:work".to_string()), "{msgs:?}");
    }

    /// The snapshot is taken before the child is bought, so a board that was never
    /// readable costs one `bd` call rather than a whole planner session.
    #[tokio::test]
    async fn a_board_that_cannot_be_snapshotted_costs_no_planner_run() {
        let fakes = Fakes::new("plan-no-baseline", PiFake::Chat, BdFake::Fails, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);

        s.send_text("plan something".to_string()).unwrap();
        assert!(s.quiesce().await);

        let msgs = drain(&mut rx);
        assert!(has_error(&msgs), "the refusal is visible: {msgs:?}");
        assert_eq!(
            fakes.pi_spawns(),
            0,
            "a pass that could not be verified afterwards should not have been paid for"
        );
        assert_eq!(s.status(), SessionStatus::NotStarted);
    }

    /// The planner dying mid-run is the no-settle case: nothing is ever going to
    /// advance this loop, so it has to say so within a bounded time rather than go
    /// on claiming it is planning.
    #[tokio::test]
    async fn a_planner_that_dies_mid_run_is_surfaced_and_starts_no_workers() {
        let fakes = Fakes::new("planner-dies", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        drain(&mut rx); // the empty-board park note
        start_planner(&mut s, "plan something").await;
        let victim = fakes.pi_pids()[0];
        assert!(
            process_alive(victim),
            "the planner is running before the kill"
        );

        crate::testing::kill_pid(victim);

        let msgs = drain_until_parked(&mut rx).await;
        let err =
            last_error(&msgs).unwrap_or_else(|| panic!("the death must be reported: {msgs:?}"));
        assert!(err.contains("planner"), "and named as the planner: {err}");
        assert!(
            !msgs.contains(&"step:work".to_string()),
            "an unverified plan never reaches the workers: {msgs:?}"
        );
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "parking is not a restart: a crash that auto-retries is a respawn storm"
        );
        assert!(!process_alive(victim));
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// `disposition: "handled"` means no run started, so no `agent_settled` will
    /// ever arrive to trigger the verification. Waiting for one is the hang; the
    /// child is dropped and the failure is reported instead.
    #[tokio::test]
    async fn a_handled_planner_prompt_waits_for_no_settle_that_never_comes() {
        let fakes = Fakes::new("planner-handled", PiFake::Handled, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);

        s.send_text("plan something".to_string()).unwrap();
        assert!(s.quiesce().await);

        let msgs = drain(&mut rx);
        assert!(
            msgs.iter()
                .any(|m| m.starts_with("error: ") && m.contains("handled")),
            "{msgs:?}"
        );
        assert_eq!(
            s.in_flight(),
            None,
            "the handled child is dropped, not held"
        );

        // And nothing resurrects the pass later: there is no settle to wait for,
        // and nothing that should.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(fakes.pi_spawns(), 1, "one spawn, ever");
        assert_eq!(s.status(), SessionStatus::NotStarted);
    }

    /// Esc on the planner is a cancellation, not a verdict. Reporting "created no
    /// tickets" here would blame the planner for work the user just stopped, and
    /// would train them to ignore the message that matters.
    #[tokio::test]
    async fn an_aborted_planner_is_cancelled_not_reported_as_an_empty_plan() {
        let fakes = Fakes::new("planner-abort", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
        let (mut s, mut rx) = beads(&fakes, 1);
        start_planner(&mut s, "plan something").await;

        s.abort().unwrap();

        let msgs = drain_until_parked(&mut rx).await;
        assert!(msgs.iter().any(|m| m.contains("cancel")), "{msgs:?}");
        assert!(
            !msgs.iter().any(|m| m.contains("created no tickets")),
            "an interrupted planner is not a failed plan: {msgs:?}"
        );
        assert!(
            !msgs.iter().any(|m| m.contains("cannot verify")),
            "and not an unverifiable one either: {msgs:?}"
        );
        assert_eq!(fakes.pi_spawns(), 1);
        assert!(!msgs.contains(&"step:work".to_string()), "{msgs:?}");
    }

    // --------- the harness claims, and cannot re-work an un-closed bead (looprs-w7q) ---------
    //
    // The bug this section exists for: `bd ready` hands back any ticket that is
    // still open, and a self-advancing loop that takes whatever it is handed will
    // take the same one forever. Every test here pins one of the four things that
    // have to be true instead — the harness claims, the board is asked whether the
    // work landed, a ticket that needs a human is not worked, and nothing restarts
    // without a human.

    /// **The harness claims, before it pays.** `bd update <id> --claim` comes from
    /// the loop rather than from the agent, so "which ticket is this run about" has
    /// an answer that does not depend on the agent saying so.
    #[tokio::test]
    async fn the_harness_claims_the_ticket_it_is_paying_for() {
        let fakes = Fakes::new(
            "w7q-claims",
            PiFake::Started,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        let (mut l, _rx, _ctl) = loop_with(&fakes);

        timeout(NO_HANG, l.next()).await.unwrap();

        assert!(
            fakes.claimed("looprs-26r"),
            "the harness must run `bd update <id> --claim` itself: {:?}",
            fakes.bd_log()
        );
        assert_eq!(
            fakes.bd_call_count("update looprs-26r --claim"),
            1,
            "one claim per pass, not one per question about the pass: {:?}",
            fakes.bd_log()
        );
        assert_eq!(l.claim().map(|c| c.id.as_str()), Some("looprs-26r"));
        assert!(l.worked.contains("looprs-26r"), "claimed == on the hook");
    }

    /// **The ordering proof, run as a consequence.** If the claim came after the
    /// spawn, a refused claim would still have bought a `pi` session — aimed at a
    /// bead the harness does not hold, answering to whoever prompted it. Claiming
    /// first makes a refusal cost one CLI call.
    #[tokio::test]
    async fn a_claim_bd_refuses_buys_no_worker() {
        let fakes = Fakes::new(
            "w7q-claim-refused",
            PiFake::Started,
            BdFake::ShowStatus,
            TWO_OPEN,
        );
        fakes.refuse_claim(true);
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        timeout(NO_HANG, l.next()).await.unwrap();

        assert_eq!(
            fakes.pi_spawns(),
            0,
            "a ticket we could not claim must never have a worker pointed at it"
        );
        let err = last_error(&drain(&mut rx)).expect("a refused claim is reported");
        assert!(err.contains("looprs-26r"), "{err}");
        assert!(err.contains("claim"), "{err}");
        assert!(l.claim().is_none(), "nothing is held on a failed claim");
        assert!(l.is_awaiting_input(), "and the box comes back");
    }

    /// **The acceptance case: a fake `bd` that never closes the ticket proves the
    /// loop stops instead of spinning.** This is the runaway the ticket was filed
    /// for — the worker settles, `bd ready` returns the same open bead, another
    /// worker is bought, and nothing anywhere counts. One pass is spent, the loop
    /// says which ticket and why it stopped, and further nudges buy nothing.
    #[tokio::test]
    async fn a_ticket_that_is_never_closed_stops_the_loop_instead_of_spinning() {
        let fakes = Fakes::new(
            "w7q-never-closes",
            PiFake::Chat,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        // The worker talks, settles, and closes nothing — forever.
        show_status(&fakes, "looprs-26r", "open");
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        assert!(s.quiesce().await, "the pass started");
        assert_eq!(fakes.pi_spawns(), 1, "one worker, claimed and prompted");
        drain(&mut rx);

        fakes.settle();
        let msgs = drain_until_parked(&mut rx).await;
        let err = last_error(&msgs).expect("an un-closed ticket is an error, not a shrug");
        assert!(err.contains("looprs-26r"), "{err}");
        assert!(err.contains("not closed"), "{err}");
        assert!(
            err.contains("bd close looprs-26r"),
            "a stop that does not say what would unblock it just moves the confusion: {err}"
        );
        assert!(!msgs.contains(&"step:work".to_string()), "{msgs:?}");
        assert_eq!(s.status(), SessionStatus::Idle, "waiting on a human");

        // And it *stays* stopped. The board never changed, so every later nudge
        // re-reads the same un-closed ticket and refuses again — three nudges,
        // zero extra workers. Before this ticket the same sequence is unbounded.
        for _ in 0..3 {
            s.set_active(true).unwrap();
            assert!(s.quiesce().await);
        }
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "the guard held: no second pass on the ticket that was never closed"
        );
        assert_eq!(fakes.pi_prompts().len(), 1);
        assert_eq!(s.in_flight(), None);
    }

    /// The guard releases on a human and only on a human: a new instruction is
    /// planned, verified, and the board is workable again — while merely looking at
    /// the mode again (the loop above) deliberately is not.
    #[tokio::test]
    async fn a_new_instruction_is_the_acknowledgement_that_releases_the_guard() {
        let fakes = Fakes::new(
            "w7q-acknowledged",
            PiFake::Chat,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        show_status(&fakes, "looprs-26r", "open");
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        fakes.settle();
        drain_until_parked(&mut rx).await;
        assert_eq!(fakes.pi_spawns(), 1, "stopped by the guard");

        // The human acts: a fresh instruction, planned into real tickets.
        start_planner(&mut s, "here is what I actually want").await;
        fakes.set_board(PLAN_TWO_TICKETS);
        show_closed(&fakes, "looprs-101");
        fakes.settle();

        let msgs = drain_until(&mut rx, |m| m == "step:work").await;
        assert_eq!(
            fakes.pi_spawns(),
            3,
            "the refused worker, the planner, and one worker after the acknowledgement: {msgs:?}"
        );
        assert!(fakes.claimed("looprs-101"), "{:?}", fakes.bd_log());
    }

    /// A ticket `bd` reports as blocked is not work, even in a `ready` list: a
    /// human (or a worker that gave up) took it out of the loop on purpose, and a
    /// metered pass on it finishes nothing. It is skipped **and named**, because a
    /// silent skip is indistinguishable from a lost ticket.
    #[tokio::test]
    async fn a_blocked_ticket_is_skipped_named_and_never_claimed() {
        let fakes = Fakes::new(
            "w7q-blocked-skip",
            PiFake::Started,
            BdFake::ShowStatus,
            BLOCKED_THEN_READY,
        );
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        timeout(NO_HANG, l.next()).await.unwrap();

        assert!(
            !fakes.claimed("looprs-26r"),
            "a blocked ticket is never claimed: {:?}",
            fakes.bd_log()
        );
        assert!(
            fakes.claimed("looprs-99"),
            "the workable ticket behind it is the one worked: {:?}",
            fakes.bd_log()
        );
        assert!(
            !l.worked.contains("looprs-26r"),
            "skipped is not the same as worked"
        );
        let msgs = drain(&mut rx);
        assert!(
            msgs.iter().any(|m| m.contains("need a human") && m.contains("looprs-26r")),
            "the skip is said out loud, with the ticket named: {msgs:?}"
        );
    }

    /// A worker that leaves its ticket `blocked` has handed it to a human, which is
    /// the worker doing the right thing loudly — not the loop failing. So the loop
    /// reports the hand-off and keeps going with the rest of the board rather than
    /// parking on a ticket that was never its own to finish.
    #[tokio::test]
    async fn a_ticket_its_worker_left_blocked_does_not_stop_the_loop() {
        let fakes = Fakes::new(
            "w7q-left-blocked",
            PiFake::Chat,
            BdFake::ShowStatus,
            TWO_OPEN,
        );
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
        assert_eq!(fakes.pi_spawns(), 1);
        assert!(fakes.claimed("looprs-26r"));
        drain(&mut rx);

        // The worker gives up properly: marks the ticket blocked (which takes it
        // out of `bd ready`), then settles.
        show_status(&fakes, "looprs-26r", "blocked");
        fakes.set_board(BLOCKED_THEN_READY);
        fakes.settle();

        let msgs = drain_until(&mut rx, |m| m == "step:work").await;
        assert!(
            !has_error(&msgs),
            "a worker handing a ticket to a human is not an error: {msgs:?}"
        );
        assert!(
            msgs.iter().any(|m| m.contains("blocked") && m.contains("looprs-26r")),
            "the hand-off is on the record: {msgs:?}"
        );
        assert_eq!(fakes.pi_spawns(), 2, "the loop moved on to the next ticket");
        assert!(fakes.claimed("looprs-99"), "{:?}", fakes.bd_log());
    }

    /// A board that cannot answer is neither "closed" nor "left open". The loop
    /// says which of the two it could not determine, because the user's next
    /// command differs — `bd close` the ticket, or fix `bd`.
    #[tokio::test]
    async fn an_unreadable_board_after_a_pass_is_unverifiable_not_a_verdict() {
        let fakes = Fakes::new(
            "w7q-unreadable",
            PiFake::Chat,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        let (mut s, mut rx) = beads(&fakes, 1);

        s.set_active(true).unwrap();
        assert!(s.quiesce().await, "the pass started while bd still worked");
        fakes.fail_bd(true); // the ticket may well have closed; nobody can read that
        fakes.settle();

        let msgs = drain_until_parked(&mut rx).await;
        let err = last_error(&msgs).expect("an unreadable board is an error");
        assert!(err.contains("looprs-26r"), "{err}");
        assert!(err.contains("cannot verify"), "{err}");
        assert!(
            !err.contains("not closed"),
            "a read failure must not be reported as a verdict about the ticket: {err}"
        );
        assert_eq!(
            fakes.pi_spawns(),
            1,
            "no further pass on a ticket nobody can vouch for"
        );
        assert_eq!(s.status(), SessionStatus::Idle, "the box comes back");
    }

    /// The claim is published when it is taken and when it is released, so the
    /// status row (looprs-guh) can name the active ticket without asking `bd` —
    /// and cannot keep naming one after the loop let go of it.
    #[tokio::test]
    async fn the_active_ticket_is_published_when_taken_and_when_released() {
        let fakes = Fakes::new(
            "w7q-active-bead",
            PiFake::Started,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(
            drain(&mut rx).is_empty(),
            "a loop that has not claimed says nothing about a ticket"
        );

        timeout(NO_HANG, l.next()).await.unwrap();
        assert!(
            drain(&mut rx).contains(&"active:looprs-26r".to_string()),
            "the claim is published as it is taken"
        );

        l.close().await;
        assert!(
            drain(&mut rx).contains(&"active:-".to_string()),
            "and released with the pass"
        );
        assert!(l.claim().is_none());
    }

    /// The pick rules on their own, with no subprocess in the way: work the first
    /// workable ticket, skip what a human has taken out of the loop, refuse what
    /// this loop already burned a pass on — and keep working what this build cannot
    /// classify, so a `bd` upgrade cannot quietly empty the board.
    #[tokio::test]
    async fn the_pick_rules_work_skip_then_refuse_in_that_order() {
        let fakes = Fakes::new(
            "w7q-pick-rules",
            PiFake::Started,
            BdFake::ShowStatus,
            EMPTY_BOARD,
        );
        let (mut l, mut rx, _ctl) = loop_with(&fakes);

        assert!(
            matches!(l.pick_bead(&[bead("a", "first"), bead("b", "second")]), Pick::Work(b) if b.id() == "a"),
            "bd's own priority order is kept"
        );

        let mixed = vec![
            bead_status("skip-1", "waiting on a human", BeadStatus::Blocked),
            bead_status("skip-2", "postponed", BeadStatus::Deferred),
            bead("work-me", "actually workable"),
        ];
        assert!(
            matches!(l.pick_bead(&mixed), Pick::Work(b) if b.id() == "work-me"),
            "blocked and deferred are walked past"
        );
        let msgs = drain(&mut rx);
        let said = msgs
            .iter()
            .filter(|m| m.contains("need a human"))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(said.len(), 1, "one note per pass, not one per skipped ticket: {msgs:?}");
        assert!(said[0].contains("skip-1") && said[0].contains("skip-2"), "{said:?}");

        assert!(
            matches!(
                l.pick_bead(&[bead_status("u", "from the future", BeadStatus::Unknown)]),
                Pick::Work(b) if b.id() == "u"
            ),
            "an unfamiliar status is worked, not skipped"
        );

        l.worked.insert("again".to_string());
        assert!(
            matches!(l.pick_bead(&[bead("again", "been worked")]), Pick::AlreadyWorked(b) if b.id() == "again"),
            "a worked-and-un-closed ticket is refused, not re-run"
        );
    }

    /// **No work-discovery in the worker's instructions.** The agent is *told*
    /// which ticket it owns. Handing it `bd ready` and hoping it picks the bead the
    /// loop is reporting on is a race with every other hand on the board, so the
    /// prompt names the ticket, says it is already claimed, and does not offer the
    /// commands that would let the worker wander somewhere else.
    #[tokio::test]
    async fn the_worker_is_told_its_ticket_not_invited_to_go_shopping() {
        let fakes = Fakes::new(
            "w7q-prompt",
            PiFake::Started,
            BdFake::ShowStatus,
            ONE_BEADED_BOARD,
        );
        let (mut l, _rx, _ctl) = loop_with(&fakes);

        timeout(NO_HANG, l.next()).await.unwrap();
        let prompt = fakes
            .pi_prompts()
            .pop()
            .expect("the worker was prompted");

        assert!(prompt.contains("looprs-26r"), "the concrete id is in the prompt: {prompt}");
        assert!(
            prompt.contains("Beads loop never self-starts"),
            "and so is the title, so the worker knows what it is doing: {prompt}"
        );
        assert!(
            prompt.contains("already"),
            "it says the claim is already done: {prompt}"
        );
        assert!(
            !prompt.contains("bd ready"),
            "`bd ready` must not be offered to a worker the harness has assigned: {prompt}"
        );
        assert!(
            !prompt.contains("bd update <id> --claim"),
            "nor a claim instruction — the harness claims, the worker works: {prompt}"
        );
    }

    // ---------------- the notes themselves, pinned without a subprocess in the way ---------

    #[test]
    fn the_plan_note_lists_every_ticket_up_to_the_cap_and_counts_the_rest() {
        let tickets: Vec<Bead> = (0..MAX_PLAN_LISTED + 3)
            .map(|i| bead(&format!("looprs-{i:03}"), &format!("ticket {i}")))
            .collect();
        let note = plan_note(&tickets);
        assert!(
            note.contains(&format!(
                "planner created {} ticket(s)",
                MAX_PLAN_LISTED + 3
            )),
            "{note}"
        );
        assert!(note.contains("looprs-000: ticket 0"), "{note}");
        assert!(note.contains("looprs-019: ticket 19"), "{note}");
        assert!(
            !note.contains("looprs-020"),
            "past the cap only the count is shown: {note}"
        );
        assert!(note.contains("and 3 more"), "{note}");
        assert_eq!(note.lines().count(), MAX_PLAN_LISTED + 2, "{note}");
    }

    #[test]
    fn the_empty_plan_note_quotes_the_planner_s_own_words() {
        let note = no_plan_note("I could not parse that request.");
        assert!(note.contains("created no tickets"), "{note}");
        assert!(note.contains("I could not parse that request."), "{note}");
        let quiet = no_plan_note("   ");
        assert!(quiet.contains("left no message"), "{quiet}");
    }

    #[test]
    fn the_unverifiable_note_never_says_the_board_is_empty() {
        let note = unverifiable_note("reading the board after the planner ran", "bd exited 3");
        assert!(note.contains("cannot verify"), "{note}");
        assert!(note.contains("bd exited 3"), "{note}");
        assert!(
            !note.contains("created no tickets"),
            "the two verdicts must never read alike: {note}"
        );
    }

    #[test]
    fn clipping_a_long_quote_never_cuts_a_multibyte_char() {
        let long = "é".repeat(200); // two bytes each
        let clipped = clip(&long, 10);
        assert_eq!(clipped.chars().count(), 11, "10 kept + the ellipsis");
        assert!(std::str::from_utf8(clipped.as_bytes()).is_ok());
        assert_eq!(clip("short", 100), "short");
    }
}
