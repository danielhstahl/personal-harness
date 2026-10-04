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

use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::{Result, anyhow};
use tokio::sync::{mpsc, oneshot};

use crate::app::parse;
use crate::services::bd::{Bead, ready_beads_with};
use crate::services::pi::PiRpc;
use crate::services::prompts::{PLANNER, WORKER, generate_prompt};
use crate::session::{
    BeadStep, ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned,
};

/// Commands to the task that owns the [`BeadsLoop`].
enum BeadsCmd {
    /// A planner instruction typed into the beads box.
    Submit(String),
    /// "The last pass settled, go again." Legacy App-driven advance (looprs-msj
    /// moves this decision inside the loop, where the step already lives).
    Advance,
    /// The mode became, or stopped being, the one on screen. Q3's policy input.
    Active(bool),
    /// Test seam: ack once every command queued before this one is fully handled.
    Sync(oneshot::Sender<()>),
    /// App is exiting: reap the worker and report the session gone.
    Shutdown,
}

/// The beads machine, running inside its own task.
///
/// These three flags are the whole parked-state machine, and they are the reason a
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
    /// ([`BeadsCmd::Advance`]), because the loop's own step cannot tell
    /// "still streaming" from "finished, and nobody has asked to advance yet".
    streaming: bool,
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

    fn status(&self) -> SessionStatus {
        if self.inner.has_live_worker() {
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
        let mut task = BeadsTask {
            inner: BeadsLoop::new(id, ev_tx.clone(), cfg.clone()),
            started: false,
            parked: false,
            pending: false,
            streaming: false,
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
                    // Deferred while parked, so a Tab back produces one pass rather
                    // than one per settle that happened to arrive off-screen. This
                    // command *is* the settle signal, so it is what retires
                    // `streaming`.
                    BeadsCmd::Advance => {
                        task.streaming = false;
                        task.pending = true;
                        task.run_pending().await;
                    }
                    BeadsCmd::Sync(tx) => {
                        let _ = tx.send(());
                    }
                    BeadsCmd::Shutdown => {
                        task.inner.close().await;
                        *task_status.lock().unwrap() = SessionStatus::Dead;
                        let _ = ev_tx.send(SessionEvent::Exited {
                            reason: ExitReason::Shutdown,
                        });
                        break;
                    }
                }
                *task_status.lock().unwrap() = task.status();
            }
            // The task ends here and `ev_tx` drops with it, so the router's pump
            // still owes — and supplies — the exactly-one SessionDown.
        });

        Ok((
            Self {
                id,
                cmd: cmd_tx,
                status,
            },
            ev_rx,
        ))
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
        // looprs-5g7: abort the pi run AND park; an aborted worker must not be
        // mistaken for agent_settled -> next bead. Refused rather than faked: a
        // half-implemented cancel is worse than none, because it can be mistaken
        // for a working one.
        crate::session::stubs::todo_method("BeadsSession::abort", "looprs-5g7")
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

    fn advance(&mut self) -> Result<()> {
        self.cmd
            .send(BeadsCmd::Advance)
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
    /// This loop's identity, stamped on nothing directly but kept for logs and for
    /// the events' provenance chain (ADR-0002 Q2: the pump adds the id).
    #[allow(dead_code)]
    id: SessionId,
    /// The pi session currently driven by the loop. `None` whenever the loop is parked.
    pi_rx: Option<PiRpc>,
    ev_tx: mpsc::UnboundedSender<SessionEvent>,
    cfg: SessionConfig,
    bead_step: BeadStep,
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
    pub fn new(
        id: SessionId,
        ev_tx: mpsc::UnboundedSender<SessionEvent>,
        cfg: SessionConfig,
    ) -> Self {
        Self {
            id,
            pi_rx: None,
            ev_tx,
            cfg,
            bead_step: BeadStep::AwaitInput,
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

    /// Tear down the current session, then work the next ready bead or park.
    ///
    /// Infallible by design: a failing `bd`, a `pi` that will not start, or a prompt
    /// that never gets answered is reported to the transcript and the loop parks in
    /// `AwaitInput` for a human. Nothing here retries on a timer, so a broken board
    /// cannot turn into a respawn storm.
    pub async fn next(&mut self) {
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

    /// The one code path that owns "spawn a worker for the current ready bead":
    /// spawn, wire up event forwarding, and prompt. Callers never see a pi child
    /// that is alive but unprompted.
    async fn work_next_bead(&mut self) -> Result<WorkerPass> {
        let beads = ready_beads_with(&self.cfg.bd_bin)?;
        let Some(bead) = beads.first() else {
            return Ok(WorkerPass::Idle);
        };
        tracing::info!(bead = %bead.id(), "starting worker pass");

        // The session is built against locals: if any step below fails, `pi` drops here
        // and kill_on_drop reaps it, so a failed pass cannot leave an orphan behind.
        let (pi, ev_rx) = PiRpc::spawn_with(&self.cfg.pi_bin, &[])?;
        self.forward_pi_events(ev_rx);
        let disposition = pi.prompt(&worker_prompt(bead)).await?;
        if disposition == "handled" {
            // pi took the prompt but started no run, so no `agent_settled` will ever
            // arrive to advance the loop. Do not hold an idle session open.
            drop(pi);
            return Err(anyhow!(
                "worker prompt for {} was handled without starting a run",
                bead.id()
            ));
        }

        self.pi_rx = Some(pi);
        self.report_system(format!("beads: working {}", bead.id()));
        self.set_step(BeadStep::WorkTickets);
        Ok(WorkerPass::Working)
    }

    /// Pipe a pi event stream into the session's event stream until the child's
    /// stdout closes.
    fn forward_pi_events(&self, mut ev_rx: mpsc::UnboundedReceiver<serde_json::Value>) {
        let tx = self.ev_tx.clone();
        tokio::spawn(async move {
            while let Some(v) = ev_rx.recv().await {
                if let Some(ev) = parse(&v)
                    && tx.send(SessionEvent::Agent(ev)).is_err()
                {
                    break;
                }
            }
        });
    }

    pub fn report_error(&self, text: String) {
        let _ = self.ev_tx.send(SessionEvent::Error(text));
    }

    pub fn report_system(&self, text: String) {
        let _ = self.ev_tx.send(SessionEvent::System(text));
    }

    pub async fn close(&mut self) {
        if let Some(mut old_pi) = self.pi_rx.take() {
            let _ = old_pi.kill().await;
        }
    }

    pub async fn launch_create_tickets(&mut self, instructions: &str) -> Result<()> {
        tracing::debug!("Launched tickets with these instructions: {}", instructions);
        self.close().await;
        let args = vec!["--tools", "read,bash"];
        let (pi, ev_rx) = PiRpc::spawn_with(&self.cfg.pi_bin, &args)?;
        self.set_step(BeadStep::CreateTickets);
        self.forward_pi_events(ev_rx);
        let prompt = generate_prompt(PLANNER, instructions);
        let disposition = pi.prompt(&prompt).await?;
        if disposition == "handled" {
            drop(pi);
            return Err(anyhow!(
                "planner prompt was handled without starting a run; no tickets were requested"
            ));
        }
        self.pi_rx = Some(pi);

        Ok(())
    }
}

/// The worker prompt, with the target bead named so the worker does not have to
/// re-run `bd ready` to find out what it is supposed to be doing.
fn worker_prompt(bead: &Bead) -> String {
    generate_prompt(
        WORKER,
        &format!("Claim and work ticket {} ({}).", bead.id(), bead.title()),
    )
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

    fn fakes_cfg(fakes: &Fakes) -> SessionConfig {
        SessionConfig {
            pi_bin: fakes.pi_bin().to_string(),
            bd_bin: fakes.bd_bin().to_string(),
            ..Default::default()
        }
    }

    fn loop_with(fakes: &Fakes) -> (BeadsLoop, mpsc::UnboundedReceiver<SessionEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let id = SessionId::new(TerminalType::Beeds, 0);
        (BeadsLoop::new(id, tx, fakes_cfg(fakes)), rx)
    }

    /// Snapshot of what the loop told the UI, as stable strings.
    fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(match m {
                SessionEvent::BeadStep(BeadStep::AwaitInput) => "step:await".into(),
                SessionEvent::BeadStep(BeadStep::CreateTickets) => "step:plan".into(),
                SessionEvent::BeadStep(BeadStep::WorkTickets) => "step:work".into(),
                SessionEvent::Error(text) => format!("error: {text}"),
                SessionEvent::System(text) => format!("system: {text}"),
                SessionEvent::RestoreInput { text } => format!("restore: {text}"),
                SessionEvent::Agent(_) => "agent".into(),
                SessionEvent::Exited { .. } => "session-down".into(),
                SessionEvent::BashOutput { .. } => "bash".into(),
            });
        }
        out
    }

    fn has_error(msgs: &[String]) -> bool {
        msgs.iter().any(|m| m.starts_with("error: "))
    }

    /// A constructed loop must not have touched any process. The old code spawned a
    /// pi child inside new() and never prompted it.
    #[tokio::test]
    async fn constructing_a_loop_spawns_nothing() {
        let fakes = Fakes::new("ctor", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
        let (l, mut rx) = loop_with(&fakes);

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
        let (mut l, mut rx) = loop_with(&fakes);

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
        let (mut l, mut rx) = loop_with(&fakes);

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
        let (mut l, _rx) = loop_with(&fakes);

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
        let (mut l, mut rx) = loop_with(&fakes);

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
        let (mut l, mut rx) = loop_with(&fakes);

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
        let (mut l, mut rx) = loop_with(&fakes);

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
        let (mut l, mut rx) = loop_with(&fakes);

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
        let fakes = Fakes::new("park", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
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

        // Three "settled, go again" signals arrive while hidden.
        fakes.set_board(SECOND_BOARD);
        for _ in 0..3 {
            s.cmd.send(BeadsCmd::Advance).unwrap();
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
}
