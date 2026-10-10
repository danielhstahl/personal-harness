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
//!
//! ## What is where (looprs-00u.18)
//!
//! This was one 3,709-line file with six responsibilities in it. What is left
//! here is the beads **session half** — the mailbox type and the handle the
//! Router holds — and the rest sits next door, one responsibility per file:
//!
//! * [`task`](task) — the tokio task that owns the loop: the parked-state
//!   flags, the settle / abort / park handling, the liveness mirrors;
//! * [`machine`](machine) — [`BeadsLoop`] itself: the pass boundary, the bead
//!   claim, the planner and worker spawns, the two board verifications;
//! * [`guards`](guards) — the decision layer kept pure: the pass gate, the step
//!   transition function, the cancel and stall-timer predicates;
//! * [`notes`](notes) — the sentences: the worker prompt, and every note the
//!   loop can print about a plan, a claim or a pass;
//! * [`tests`](tests) — this suite, split along the banners that were already
//!   written in it.
//!
//! Nothing about the behaviour moved with the split: whole items were moved, and
//! the paths below (`beads::BeadsSession`, `beads::StepCause`, `beads::BeadsLoop`)
//! are the ones every caller already used.

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

use crate::session::{
    ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned,
    publish_liveness,
};

mod guards;
mod machine;
mod notes;
mod task;

#[cfg(test)]
mod tests;

pub use guards::StepCause;
pub use machine::BeadsLoop;

use task::BeadsTask;

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
    ///
    /// Only [`BeadsSession::quiesce`] sends one; the loop itself never needs to
    /// ask itself. Needed because the beads machine is driven by another task's
    /// events, so "the loop has caught up" is otherwise unobservable.
    #[allow(dead_code)] // consumer: BeadsSession::quiesce (test seam)
    Sync(oneshot::Sender<()>),
    /// App is exiting: reap the worker and report the session gone.
    Shutdown,
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
    #[allow(dead_code)] // read via BeadsSession::in_flight
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

        // The liveness this session has already told the UI about, so
        // `publish_liveness` fires on changes rather than on every turn of the loop.
        let mut published = SessionStatus::NotStarted;

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
                            task.inner.set_step(StepCause::Awaiting);
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
                        let s = task.status();
                        *task_status.lock().unwrap() = s;
                        publish_liveness(&mut published, s, &ev_tx);
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
                let s = task.status();
                *task_status.lock().unwrap() = s;
                publish_liveness(&mut published, s, &ev_tx);
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
    ///
    /// The status row does not read it: the row's "which bead" comes from
    /// [`SessionEvent::ActiveBead`], which the loop publishes at the two moments
    /// that matter rather than being polled out of the loop by a draw loop that
    /// must not block on one.
    #[allow(dead_code)] // consumers: beads::tests (which pass is running, without a sleep)
    pub fn in_flight(&self) -> Option<u64> {
        *self.in_flight.lock().unwrap()
    }

    /// Test seam: returns once every command queued before this call has been
    /// fully handled by the session's task. Makes lifecycle assertions
    /// deterministic instead of sleep-based.
    #[allow(dead_code)] // consumer: beads::tests (deterministic lifecycle, no sleeps)
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
