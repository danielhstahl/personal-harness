//! The Pi terminal state: **one persistent, stateful `pi --mode rpc` chat session.**
//!
//! Same handle/task split as [`BeadsSession`](super::beads::BeadsSession):
//! [`PiChatSession`] is a cheap handle whose every method queues a [`PiCmd`] and
//! returns, and the task owning the [`PiRpc`] is the only thing that awaits a
//! child. That is what keeps ADR-0002's "the Router never blocks on a child" true
//! here, where a child is a Node process answering a model.
//!
//! Four decisions, all of them this mode's whole reason to exist:
//!
//! 1. **Spawn once, reuse forever.** The context that makes turn 2 answer "what is
//!    my name?" lives in *this child's memory*. A child per message would be a
//!    stateless chat wearing a hat. `set_active` is the inherited no-op: a Tab
//!    never touches it (ADR-0002 Q3, warm).
//! 2. **`Ok` from the wire is not `done`.** A `prompt` response means accepted,
//!    queued or handled. The subscription (the reader task) is installed at spawn
//!    time — long before the first prompt — so a fast completion cannot slip past,
//!    and "finished" is `agent_settled` on the event stream, never a return value.
//! 3. **A message during a run *steers*.** The input box is never gated while Pi is
//!    busy: you can talk to it mid-answer. The cost of that choice is that a
//!    follow-up has to go in as `steer` — pi rejects a plain `prompt` during a run
//!    unless a `streamingBehavior` is named — so the session routes by its own
//!    running state, and if `steer` turns out to have raced the end of its run, it
//!    re-sends as a fresh prompt rather than dropping what was typed.
//! 4. **A dead child is a notice, not a wedged mode.** Crash, OOM, `kill -9`: the
//!    stream end is reported as `Exited` (so the App seals that transcript), the
//!    status goes `Dead`, and the next message starts a new child.

use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use anyhow::Result;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use crate::app::{PiEvent, parse};
use crate::services::pi::{PiRpc, disposition_of, queued_text, succeeded};
use crate::session::{
    ExitReason, Session, SessionConfig, SessionEvent, SessionId, SessionStatus, Spawned,
};

/// How long an orderly `close stdin -> pi disposes itself` gets before we SIGKILL.
///
/// Deliberately well inside the Router's [`super::router::SHUTDOWN_GRACE`]: this
/// task has to report `Exited` before the Router stops waiting for it, and the
/// escalation ladder itself belongs to looprs-ecr.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(900);

/// Bound on the round-trip of the Esc-triggered `clear_queue`.
///
/// Esc has to give the input box back in well under a second even if pi has wedged,
/// so we wait for the queued text only as long as it takes to notice it is not
/// coming, then abort anyway. Losing the restore beats losing the key.
const CANCEL_WAIT: Duration = Duration::from_millis(600);

/// Bounded poll for an exit code after a child's stdout closes.
const REAP_POLL: Duration = Duration::from_millis(25);
const REAP_TRIES: usize = 8;

/// Commands to the task that owns the [`PiRpc`].
enum PiCmd {
    /// A chat message, typed into the Pi box.
    Submit(String),
    /// `Esc`: `clear_queue` then `abort` (pi's interactive-Esc recipe).
    Cancel,
    /// A protocol record off the child's stdout.
    Record(Value),
    /// The child's stdout closed. Carries the serial of *that* child, so a late
    /// notice from one we already replaced cannot clobber the live one.
    StreamEnd { serial: u64 },
    /// Test seam: ack once every command queued before this one is handled.
    Sync(oneshot::Sender<()>),
    /// The app is exiting.
    Shutdown,
}

/// How one send attempt ended, in the categories the caller can act on.
enum Send {
    /// pi took it. Payload is `data.disposition`.
    Taken(String),
    /// pi said no, and the process is alive. A steer that raced its run's end is
    /// worth re-sending; anything else is a real refusal.
    Refused(String),
    /// No answer will ever come: the child is gone.
    Gone(String),
}

/// The child, plus the serial that says *which* child.
struct ChildHandle {
    serial: u64,
    rpc: PiRpc,
}

/// The chat, running inside its own task.
struct PiChat {
    id: SessionId,
    cfg: SessionConfig,
    ev_tx: mpsc::UnboundedSender<SessionEvent>,
    /// This task's own mailbox, so the pi forwarder can post into it: one receiver,
    /// no `select!` over a growing set of streams, and commands and child events
    /// keep a single, well-defined order.
    cmd: mpsc::UnboundedSender<PiCmd>,
    child: Option<ChildHandle>,
    next_serial: u64,
    /// Has there ever been a child? The difference between `NotStarted` ("this mode
    /// has not been used") and `Dead` ("it was used and its child died"), which is
    /// a difference the status row has to be able to show.
    had_child: bool,
    /// A run is in flight: pi took the prompt, has not settled.
    running: bool,
    /// `Esc` landed on a running session and we are waiting for it to unwind.
    aborting: bool,
}

impl PiChat {
    fn emit(&self, ev: SessionEvent) {
        let _ = self.ev_tx.send(ev);
    }

    fn note(&self, text: impl Into<String>) {
        self.emit(SessionEvent::System(text.into()));
    }

    fn err(&self, text: impl Into<String>) {
        self.emit(SessionEvent::Error(text.into()));
    }

    fn status(&self) -> SessionStatus {
        if self.child.is_none() {
            return if self.had_child {
                SessionStatus::Dead
            } else {
                SessionStatus::NotStarted
            };
        }
        if self.aborting {
            SessionStatus::Aborting
        } else if self.running {
            SessionStatus::Running
        } else {
            SessionStatus::Idle
        }
    }

    /// Start the child if there is not one, and return it.
    ///
    /// Called from the send path only, which is what makes "process starts on the
    /// first submit in Pi mode" (ADR-0002 Q3) true by construction: nothing else
    /// can bring a child up. A respawn gets a note, because a child that appeared
    /// out of nowhere in the middle of a conversation deserves an introduction.
    fn ensure_child(&mut self) -> Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let respawning = self.had_child;
        // The reader task starts here, before any prompt exists, so the completion
        // of a prompt that finishes immediately is not missed.
        let (rpc, records) = PiRpc::spawn_with(&self.cfg.pi_bin, &[])?;
        let serial = self.next_serial;
        self.next_serial += 1;
        self.had_child = true;
        self.running = false;
        self.aborting = false;
        self.child = Some(ChildHandle { serial, rpc });
        self.forward_records(records, serial);
        if respawning {
            self.note("pi chat restarted (the previous child was gone)");
        }
        Ok(())
    }

    /// Pipe the child's protocol records into this task, and report the stream end.
    fn forward_records(&self, mut records: mpsc::UnboundedReceiver<Value>, serial: u64) {
        let cmd = self.cmd.clone();
        tokio::spawn(async move {
            while let Some(v) = records.recv().await {
                if cmd.send(PiCmd::Record(v)).is_err() {
                    return; // the session task is gone; nothing to tell
                }
            }
            let _ = cmd.send(PiCmd::StreamEnd { serial });
        });
    }

    /// Send one message one way: `steer` if a run is in flight, `prompt` if not.
    async fn dispatch(&mut self, text: &str, steering: bool) -> Send {
        let Some(child) = self.child.as_mut() else {
            return Send::Gone("there is no pi child".to_string());
        };
        let cmd = if steering {
            json!({ "type": "steer", "message": text })
        } else {
            json!({ "type": "prompt", "message": text })
        };
        match child.rpc.request_raw(cmd).await {
            // No answer, ever: pi is gone (or was gone before we wrote).
            Err(e) => Send::Gone(format!("{e:#}")),
            Ok(resp) if succeeded(&resp) => Send::Taken(disposition_of(&resp)),
            Ok(resp) => {
                let msg = resp["error"].as_str().unwrap_or("refused").to_string();
                // Ask the OS rather than parse pi's prose: a "no" from a process
                // that is not running any more is a death, not a refusal.
                if matches!(child.rpc.try_wait(), Ok(Some(_))) {
                    Send::Gone(msg)
                } else {
                    Send::Refused(msg)
                }
            }
        }
    }

    /// One user turn, end to end.
    async fn submit(&mut self, text: String) {
        // Two attempts, never more. The first can fail for a reason that is worth
        // going round again for (a steer that raced the end of its run; a child
        // that died between the status check and the write). A third attempt
        // against the same broken pi is a resend storm with a cursor in it.
        for attempt in 0..2 {
            if let Err(e) = self.ensure_child() {
                self.err(format!("could not start pi: {e:#}"));
                return;
            }
            let steering = self.running;
            match self.dispatch(&text, steering).await {
                Send::Taken(disposition) => {
                    if steering {
                        tracing::debug!(disposition = %disposition, "pi queued the follow-up");
                    } else if disposition == "handled" {
                        // pi consumed the prompt and started no run, so no
                        // `agent_settled` will *ever* arrive. Saying so beats a
                        // spinner over a turn that never existed.
                        self.running = false;
                        self.note("pi handled that message without starting a run");
                    } else {
                        self.running = true;
                    }
                    return;
                }
                Send::Refused(msg) if attempt == 0 && steering => {
                    // The run settled between our check and the command, so pi has
                    // nothing to steer. Send it as its own turn instead of
                    // throwing away what the user just typed.
                    tracing::debug!("{msg}; re-sending as a fresh prompt");
                    self.running = false;
                    continue;
                }
                Send::Gone(msg) if attempt == 0 => {
                    // Noticed on the write rather than on the stream. Report the
                    // death (which seals the transcript) and try again with a live
                    // child.
                    tracing::warn!("{}: {msg}; restarting the pi chat", self.id);
                    let reason = self.reap_child().await;
                    self.emit(SessionEvent::Exited { reason });
                    continue;
                }
                Send::Refused(msg) => {
                    self.err(format!("pi refused the message: {msg}"));
                    return;
                }
                Send::Gone(msg) => {
                    self.err(format!("pi is not answering: {msg}"));
                    return;
                }
            }
        }
    }

    /// `Esc`. `clear_queue` first, then `abort` — pi's documented recipe, in that
    /// order because `abort` runs whatever is still queued.
    async fn cancel(&mut self) {
        // The two round-trips happen with the child borrowed; the reporting happens
        // after that borrow is done, because a session reports through itself.
        let esc = match self.child.as_mut() {
            None => {
                // No child, nothing to stop. A no-op rather than an error: Esc means
                // "stop the thing", and there is no thing (looprs-5g7).
                return;
            }
            Some(child) => Self::esc_round_trip(child).await,
        };

        let (restored, abort_err) = esc;
        if !restored.is_empty() {
            // The user's own words go back to the box rather than into the
            // transcript. The App owns the box, so this travels as an event.
            self.emit(SessionEvent::RestoreInput {
                text: restored.join("\n"),
            });
        }
        if let Some(e) = abort_err {
            self.err(format!("cancel failed: {e}"));
            return;
        }
        if self.running {
            self.aborting = true;
        }
    }

    /// The two commands an interactive Esc is made of, and what they gave back.
    ///
    /// Kept apart from `cancel` so the child borrow has an obvious end. The abort
    /// is fire-and-forget, deliberately: `abort` responds only once the session is
    /// idle, i.e. after the whole run has unwound, and parking this task on that
    /// response would put the user's next keystroke behind the very run they are
    /// trying to stop. The end of the run is seen where every other end is seen —
    /// on the event stream, as `agent_settled`.
    async fn esc_round_trip(child: &mut ChildHandle) -> (Vec<String>, Option<String>) {
        // Pull the queued text out *before* the abort. The alternative is either
        // spending a turn the user just cancelled on messages they no longer want,
        // or losing those messages outright.
        let restored = match tokio::time::timeout(
            CANCEL_WAIT,
            child.rpc.request_raw(json!({ "type": "clear_queue" })),
        )
        .await
        {
            Ok(Ok(resp)) if succeeded(&resp) => queued_text(&resp),
            Ok(Ok(resp)) => {
                tracing::warn!(
                    "clear_queue refused: {}",
                    resp["error"].as_str().unwrap_or("?")
                );
                Vec::new()
            }
            Ok(Err(e)) => {
                tracing::warn!("clear_queue failed: {e:#}");
                Vec::new()
            }
            Err(_) => {
                tracing::warn!(
                    "clear_queue did not answer within {CANCEL_WAIT:?}; aborting anyway"
                );
                Vec::new()
            }
        };
        let abort_err = child.rpc.abort().err().map(|e| format!("{e:#}"));
        (restored, abort_err)
    }

    /// Drop the child and report why it went.
    ///
    /// Collects the exit code only if it is already there — the transcript reads
    /// better with `137` in it than without — but never waits on a child it is only
    /// there to announce.
    async fn reap_child(&mut self) -> ExitReason {
        let mut status: Option<ExitStatus> = None;
        if let Some(child) = self.child.as_mut() {
            for _ in 0..REAP_TRIES {
                match child.rpc.try_wait() {
                    Ok(Some(st)) => {
                        status = Some(st);
                        break;
                    }
                    Ok(None) => tokio::time::sleep(REAP_POLL).await,
                    Err(e) => {
                        tracing::debug!("{}: could not reap the pi child: {e}", self.id);
                        break;
                    }
                }
            }
        }
        self.child = None;
        self.running = false;
        self.aborting = false;
        match status {
            // Exited 0 on its own mid-conversation: not a crash, and not something
            // we asked for. The honest answer is "unknown".
            Some(st) if st.success() => ExitReason::Unknown,
            Some(st) => ExitReason::Crashed { code: st.code() },
            None => ExitReason::Unknown,
        }
    }

    /// A protocol record off the child.
    ///
    /// The control half (is a run in flight? did the cancel land?) is consumed
    /// here; the record itself is forwarded unchanged, because rendering is the
    /// App's business. Note that pi's copy of the user's own message is *not*
    /// filtered here — the transcript rule lives in `app::apply_pi`, where the
    /// local echo is made, so there is exactly one place that decides what the user
    /// sees of themselves.
    fn on_record(&mut self, v: Value) {
        let Some(ev) = parse(&v) else {
            return; // `parse` logs the offender; there is nothing to render
        };
        let cancelling = self.aborting;
        match ev {
            PiEvent::AgentStart => self.running = true,
            PiEvent::AgentSettled => {
                self.running = false;
                self.aborting = false;
            }
            _ => {}
        }
        let cancelled = matches!(ev, PiEvent::AgentSettled) && cancelling;
        self.emit(SessionEvent::Agent(ev));
        if cancelled {
            // The user pressed Esc; tell them it arrived. looprs-5g7 owns the
            // wording and the spinner, but silence while a tool unwinds is not an
            // option either.
            self.note("cancelled");
        }
    }

    /// The child's stdout closed.
    async fn child_gone(&mut self, serial: u64) {
        if self.child.as_ref().map(|c| c.serial) != Some(serial) {
            // A notice from a child we already replaced (the send path got there
            // first). The live one stays exactly where it is.
            return;
        }
        let reason = self.reap_child().await;
        self.emit(SessionEvent::Exited { reason });
    }

    /// Quit-time teardown: close stdin so pi can dispose its runtime, bound the
    /// wait, escalate to a kill, and report the one exit we owe the pump.
    async fn shutdown_child(&mut self) {
        if let Some(child) = self.child.as_mut() {
            child.rpc.close_stdin();
            if tokio::time::timeout(SHUTDOWN_GRACE, child.rpc.wait())
                .await
                .is_err()
            {
                tracing::warn!(
                    "{}: pi did not exit within {SHUTDOWN_GRACE:?}; killing it",
                    self.id
                );
                let _ = child.rpc.kill().await;
            }
        }
        self.child = None;
        self.running = false;
        self.aborting = false;
        self.emit(SessionEvent::Exited {
            reason: ExitReason::Shutdown,
        });
    }
}

pub struct PiChatSession {
    id: SessionId,
    cmd: mpsc::UnboundedSender<PiCmd>,
    status: Arc<StdMutex<SessionStatus>>,
}

impl PiChatSession {
    /// Start the Pi chat. Builds **no process**: the child comes up on the first
    /// submit and then never goes away until the app does.
    pub fn start(id: SessionId, cfg: &SessionConfig) -> Result<Spawned> {
        let (session, events) = Self::build(id, cfg)?;
        Ok(Spawned {
            session: Box::new(session),
            events,
        })
    }

    /// The un-boxed form, so a test can hold a concrete handle next to the event
    /// stream the Router would otherwise pump.
    fn build(
        id: SessionId,
        cfg: &SessionConfig,
    ) -> Result<(Self, mpsc::UnboundedReceiver<SessionEvent>)> {
        let (ev_tx, ev_rx) = mpsc::unbounded_channel::<SessionEvent>();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<PiCmd>();
        let status = Arc::new(StdMutex::new(SessionStatus::NotStarted));
        let task_status = status.clone();

        let mut chat = PiChat {
            id,
            cfg: cfg.clone(),
            ev_tx: ev_tx.clone(),
            cmd: cmd_tx.clone(),
            child: None,
            next_serial: 1,
            had_child: false,
            running: false,
            aborting: false,
        };

        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                match cmd {
                    PiCmd::Submit(text) => chat.submit(text).await,
                    PiCmd::Cancel => chat.cancel().await,
                    PiCmd::Record(v) => chat.on_record(v),
                    PiCmd::StreamEnd { serial } => chat.child_gone(serial).await,
                    PiCmd::Sync(tx) => {
                        // Publish the status mirror before acking, so a caller that
                        // waits on the seam and then reads `status()` sees the state
                        // as of the ack rather than one command stale. (The
                        // end-of-iteration mirror below is not enough: on a
                        // multi-threaded runtime the ack can wake the waiter
                        // first.)
                        *task_status.lock().unwrap() = chat.status();
                        let _ = tx.send(());
                    }
                    PiCmd::Shutdown => {
                        chat.shutdown_child().await;
                        // Report it on the way out: the loop ends here, so the
                        // end-of-iteration mirror below never runs again.
                        *task_status.lock().unwrap() = SessionStatus::Dead;
                        break;
                    }
                }
                *task_status.lock().unwrap() = chat.status();
            }
            // The mailbox is closed: the Router replaced us, or the app is gone.
            // Dropping the chat drops the child, and `kill_on_drop` makes good on
            // that. No child of ours outlives this task.
            drop(chat);
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
    /// fully handled.
    pub async fn quiesce(&self) -> bool {
        let (tx, rx) = oneshot::channel();
        if self.cmd.send(PiCmd::Sync(tx)).is_err() {
            return true; // task already gone: nothing left to wait for
        }
        tokio::time::timeout(Duration::from_secs(15), rx)
            .await
            .is_ok()
    }
}

impl Session for PiChatSession {
    fn id(&self) -> SessionId {
        self.id
    }

    fn send_text(&mut self, text: String) -> Result<()> {
        self.cmd
            .send(PiCmd::Submit(text))
            .map_err(|_| anyhow::anyhow!("pi chat task is gone"))
    }

    fn abort(&mut self) -> Result<()> {
        self.cmd
            .send(PiCmd::Cancel)
            .map_err(|_| anyhow::anyhow!("pi chat task is gone"))
    }

    fn shutdown(&mut self) -> Result<()> {
        self.cmd
            .send(PiCmd::Shutdown)
            .map_err(|_| anyhow::anyhow!("pi chat task is gone"))
    }

    // `set_active` is deliberately *not* overridden: Pi is `KeepRunning`
    // (ADR-0002 Q3), and the trait's no-op default is the structural form of "a
    // Tab never touches this session".
    fn status(&self) -> SessionStatus {
        *self.status.lock().unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::AssistantEvent;
    use crate::session::TerminalType;
    use crate::testing::{BdFake, EMPTY_BOARD, Fakes, PiFake, kill_pid, process_alive};
    use std::time::Instant;

    /// Generous, bounded: a hang is a failure of this ticket and must report as one.
    const NO_HANG: Duration = Duration::from_secs(20);

    fn fakes(tag: &str) -> Fakes {
        Fakes::new(tag, PiFake::Chat, BdFake::Ok, EMPTY_BOARD)
    }

    fn pi_chat(
        f: &Fakes,
        generation: u64,
    ) -> (PiChatSession, mpsc::UnboundedReceiver<SessionEvent>) {
        let cfg = SessionConfig {
            pi_bin: f.pi_bin().to_string(),
            bd_bin: f.bd_bin().to_string(),
            ..Default::default()
        };
        PiChatSession::build(SessionId::new(TerminalType::Pi, generation), &cfg).expect("build")
    }

    /// One turn's worth of session output, kept as stable strings so a failing
    /// assertion prints something readable instead of four nested enums.
    #[derive(Default, Debug)]
    struct Tally {
        /// The assistant text that streamed by.
        text: String,
        /// Every event, in order, described.
        lines: Vec<String>,
    }

    impl Tally {
        fn exits(&self) -> usize {
            self.lines.iter().filter(|l| l.starts_with("down ")).count()
        }
        fn has(&self, needle: &str) -> bool {
            self.lines.iter().any(|l| l.contains(needle))
        }
    }

    fn describe(ev: SessionEvent) -> String {
        match ev {
            SessionEvent::Agent(PiEvent::AgentStart) => "agent_start".into(),
            SessionEvent::Agent(PiEvent::AgentSettled) => "agent_settled".into(),
            SessionEvent::Agent(PiEvent::MessageUpdate {
                assistant_message_event: AssistantEvent::TextDelta { delta, .. },
            }) => format!("delta {delta}"),
            SessionEvent::Agent(other) => format!("agent {other:?}"),
            SessionEvent::System(t) => format!("system: {t}"),
            SessionEvent::Error(t) => format!("error: {t}"),
            SessionEvent::RestoreInput { text } => format!("restore: {text}"),
            SessionEvent::Exited { reason } => format!("down {reason:?}"),
            SessionEvent::BeadStep(s) => format!("step {s:?}"),
            SessionEvent::BashOutput { chunk, .. } => format!("bash {chunk}"),
            // A pi chat session has no screen to take over; seeing this in test
            // output would mean the event came from somewhere it should not have.
            SessionEvent::ScreenHeld { active } => format!("screen:{active}"),
        }
    }

    async fn next_event(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> String {
        let ev = tokio::time::timeout(NO_HANG, rx.recv())
            .await
            .expect("the session went silent")
            .expect("the session stream closed");
        describe(ev)
    }

    /// Whatever has already arrived, described.
    fn drain(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            out.push(describe(ev));
        }
        out
    }

    /// Read one whole turn: everything up to and including `agent_settled`.
    async fn read_turn(rx: &mut mpsc::UnboundedReceiver<SessionEvent>) -> Tally {
        let mut t = Tally::default();
        loop {
            let line = next_event(rx).await;
            if let Some(rest) = line.strip_prefix("delta ") {
                t.text.push_str(rest);
            }
            let settled = line == "agent_settled";
            t.lines.push(line);
            if settled {
                return t;
            }
        }
    }

    /// Send a turn and let the fake finish it. `wait_first` matters: settling before
    /// the child has the message is fine (the trigger just waits), but a test that
    /// settles before *sending* would end the previous turn instead.
    async fn send_and_settle(
        s: &mut PiChatSession,
        f: &Fakes,
        rx: &mut mpsc::UnboundedReceiver<SessionEvent>,
        text: &str,
    ) -> Tally {
        s.send_text(text.into()).unwrap();
        f.wait_for_log_line(&format!("prompt {text}")).await;
        f.settle();
        read_turn(rx).await
    }

    /// Cold start is lazy: entering Pi mode must not be what pays for a Node process.
    #[tokio::test]
    async fn nothing_is_spawned_until_the_first_message() {
        let f = fakes("cold");
        let (_s, mut rx) = pi_chat(&f, 1);

        assert_eq!(f.pi_spawns(), 0, "starting the session spawns no child");
        assert_eq!(f.pi_verbs(), Vec::<String>::new());
        assert!(rx.try_recv().is_err(), "and says nothing either");
    }

    /// **The acceptance criterion.** "my name is Dan", then "what is my name?":
    /// the second answer uses the first turn's context, and it can only do that if
    /// one and the same child answered both.
    #[tokio::test]
    async fn two_turns_share_one_child_and_the_second_remembers_the_first() {
        let f = fakes("persistent");
        let (mut s, mut rx) = pi_chat(&f, 1);

        let first = send_and_settle(&mut s, &f, &mut rx, "my name is Dan").await;
        assert!(first.text.starts_with("reply 1"), "first turn: {first:?}");
        assert!(
            !first.text.contains("memory=my name"),
            "turn 1 has nothing to remember yet: {}",
            first.text
        );

        let second = send_and_settle(&mut s, &f, &mut rx, "what is my name?").await;
        assert!(
            second.text.contains("memory=my name is Dan"),
            "turn 2 must carry turn 1's context, and that context exists only in \
             the child's own memory: {}",
            second.text
        );
        assert_eq!(
            f.pi_spawns(),
            1,
            "spawn once, reuse forever — a child per message is a stateless chat"
        );
        assert_eq!(
            f.pi_prompts(),
            vec!["my name is Dan".to_string(), "what is my name?".to_string()],
            "both turns went to that one child"
        );
        assert_eq!(s.status(), SessionStatus::Idle, "settled, still warm");
    }

    /// Follow-ups while a run is in flight go in as `steer`, not as a second
    /// `prompt`: pi rejects a plain prompt mid-run, so this routing *is* the price
    /// of leaving the input box open while the answer streams.
    #[tokio::test]
    async fn a_message_during_a_run_steers_instead_of_starting_another_run() {
        let f = fakes("steer");
        let (mut s, mut rx) = pi_chat(&f, 1);

        s.send_text("write me a story".to_string()).unwrap();
        f.wait_for_log_line("prompt write me a story").await;
        // The fake logs the prompt before it answers, so the status mirror is only
        // meaningful once this session's own mailbox has drained past the submit.
        assert!(s.quiesce().await, "the submit was handled");
        assert_eq!(
            s.status(),
            SessionStatus::Running,
            "a turn is in flight, so the mode is busy"
        );

        s.send_text("make it about boats".to_string()).unwrap();
        f.wait_for_log_line("recv steer make it about boats").await;
        assert_eq!(
            f.pi_verbs(),
            vec!["prompt", "steer"],
            "a follow-up mid-run must steer; a second `prompt` would be a second run"
        );

        f.settle();
        let turn = read_turn(&mut rx).await;
        assert!(turn.has("agent_settled"), "{turn:?}");
        assert_eq!(s.status(), SessionStatus::Idle);
        assert_eq!(f.pi_spawns(), 1);
    }

    /// **Esc during a run**: `clear_queue` then `abort`, the queued text handed back
    /// for the input box, the run stopped, all of it fast enough not to feel like a
    /// hang.
    #[tokio::test]
    async fn esc_clears_the_queue_before_aborting_and_gives_the_words_back() {
        let f = fakes("esc");
        let (mut s, mut rx) = pi_chat(&f, 1);

        s.send_text("long thing".to_string()).unwrap();
        f.wait_for_log_line("prompt long thing").await;
        s.send_text("and also this".to_string()).unwrap();
        f.wait_for_log_line("recv steer and also this").await;
        assert_eq!(s.status(), SessionStatus::Running);

        let started = Instant::now();
        s.abort().unwrap();
        let mut seen: Vec<String> = Vec::new();
        let restored = loop {
            let l = next_event(&mut rx).await;
            let r = l.clone();
            seen.push(l.clone());
            if l.starts_with("down ") {
                panic!("session died before handing the text back: {l}")
            }
            if l.starts_with("restore: ") {
                break r["restore: ".len()..].to_string();
            }
        };
        let elapsed = started.elapsed();

        assert_eq!(
            restored, "and also this",
            "the queued message comes back out of the child, verbatim"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "Esc took {elapsed:?}; the box has to be usable immediately"
        );
        // The abort is fire-and-forget by design, so let the child catch up before
        // reading its transcript of what arrived, in what order.
        f.wait_for_log_line("recv abort").await;
        assert_eq!(
            f.pi_verbs(),
            vec!["prompt", "steer", "clear_queue", "abort"],
            "clear_queue *before* abort — aborting with the queue intact would run it"
        );

        // The abort unwinds the run, and the session comes back idle and warm.
        // (`read_turn` stops *at* `agent_settled`; the acknowledgement rides just
        // behind it, so read that too.)
        let mut turn = read_turn(&mut rx).await;
        turn.lines.extend(drain(&mut rx));
        assert!(
            turn.has("cancelled"),
            "the cancel is acknowledged. before: {seen:?} after: {turn:?}"
        );
        assert_eq!(s.status(), SessionStatus::Idle);
        assert_eq!(
            f.pi_spawns(),
            1,
            "Esc cancels work, it does not kill the chat"
        );
    }

    /// Esc on an idle-but-warm session: cheap, no error, changes nothing.
    #[tokio::test]
    async fn esc_on_an_idle_session_is_cheap_and_changes_nothing() {
        let f = fakes("esc-idle");
        let (mut s, mut rx) = pi_chat(&f, 1);
        send_and_settle(&mut s, &f, &mut rx, "one turn").await;

        let started = Instant::now();
        s.abort().unwrap();
        assert!(s.quiesce().await, "the cancel was handled");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "an idle Esc took {:?}",
            started.elapsed()
        );

        let rest: Vec<String> = drain(&mut rx);
        assert!(
            !rest.iter().any(|m| m.starts_with("error")),
            "an idle Esc is not a failure: {rest:?}"
        );
        assert_eq!(s.status(), SessionStatus::Idle);
        assert_eq!(f.pi_spawns(), 1, "still the same warm child");
    }

    /// Esc with no child at all is a no-op, not an error, and starts nothing.
    #[tokio::test]
    async fn esc_with_no_child_is_a_no_op() {
        let f = fakes("esc-nada");
        let (mut s, mut rx) = pi_chat(&f, 1);

        s.abort().unwrap();
        assert!(s.quiesce().await);
        assert_eq!(f.pi_spawns(), 0, "Esc must not conjure a child");
        assert_eq!(f.pi_verbs(), Vec::<String>::new(), "it barely spoke");
        assert!(drain(&mut rx).is_empty(), "and it must not complain either");
        assert_eq!(s.status(), SessionStatus::NotStarted);
    }

    /// **Child killed out from under the app**: reported once, mode not wedged,
    /// next message recovers.
    #[tokio::test]
    async fn killing_the_child_says_so_and_the_next_message_restarts_it() {
        let f = fakes("kill");
        let (mut s, mut rx) = pi_chat(&f, 1);

        let first = send_and_settle(&mut s, &f, &mut rx, "hello there").await;
        let victim = f.pi_pids()[0];
        assert!(process_alive(victim));
        assert_eq!(first.exits(), 0);

        kill_pid(victim);

        let down = loop {
            match next_event(&mut rx).await {
                l if l.starts_with("down ") => break l,
                l if l.starts_with("error") => panic!("errored instead of going down: {l}"),
                _ => {}
            }
        };
        assert!(
            down.contains("Crashed") && down.contains("code: None"),
            "SIGKILL leaves no exit code: a crash, not a shutdown: {down}"
        );
        assert_eq!(s.status(), SessionStatus::Dead);
        assert!(!process_alive(victim));

        // The mode is not wedged: the next message brings a child back.
        let second = send_and_settle(&mut s, &f, &mut rx, "are we still chatting?").await;
        assert!(
            second.text.starts_with("reply 1"),
            "a brand-new child remembers nothing of the old one: {}",
            second.text
        );
        assert!(
            second.has("restarted"),
            "the restart is visible, not silent: {:?}",
            second.lines
        );
        assert_eq!(f.pi_spawns(), 2, "respawned exactly once");
        assert_eq!(second.exits(), 0, "the death was announced exactly once");
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// The race worth covering: the child dies between the router's status check and
    /// the write, so the *send* discovers the death. Whichever of the two notices
    /// first, the answer must be the same — one death announced, one replacement
    /// child, the message delivered.
    #[tokio::test]
    async fn a_child_that_dies_racing_the_next_send_is_delivered_not_lost() {
        let f = fakes("dead-send");
        let (mut s, mut rx) = pi_chat(&f, 1);

        send_and_settle(&mut s, &f, &mut rx, "first").await;
        let victim = f.pi_pids()[0];
        kill_pid(victim);
        // Send immediately: this may find the corpse (send path notices) or wait for
        // the stream-end notice (death path notices). Both are covered by one set of
        // assertions rather than by hoping to lose the race on purpose.
        s.send_text("second".to_string()).unwrap();
        f.wait_for_log_line("prompt second").await;
        f.settle();

        let mut exits = 0;
        let mut text = String::new();
        loop {
            let line = next_event(&mut rx).await;
            if line.starts_with("down ") {
                exits += 1;
            }
            if let Some(rest) = line.strip_prefix("delta ") {
                text.push_str(rest);
            }
            if line == "agent_settled" {
                break;
            }
        }
        assert_eq!(
            exits, 1,
            "one death announcement, whichever path saw it first"
        );
        assert!(
            text.starts_with("reply 1"),
            "the message reached a fresh child: {text}"
        );
        assert_eq!(f.pi_spawns(), 2);
        assert_eq!(s.status(), SessionStatus::Idle);
    }

    /// Warm, per ADR-0002 Q3: a Tab is a view change. Entering and leaving Pi must
    /// neither kill the child nor spawn a second one.
    #[tokio::test]
    async fn a_tab_never_touches_the_child() {
        let f = fakes("warm");
        let (mut s, mut rx) = pi_chat(&f, 1);

        send_and_settle(&mut s, &f, &mut rx, "hi").await;
        let pid = f.pi_pids()[0];

        for _ in 0..3 {
            s.set_active(false).unwrap();
            s.set_active(true).unwrap();
        }
        assert!(s.quiesce().await);
        assert_eq!(f.pi_spawns(), 1, "one child for the life of the mode");
        assert!(process_alive(pid), "and it is the same live process");

        // Still chatting in the same session afterwards.
        let later = send_and_settle(&mut s, &f, &mut rx, "still you?").await;
        assert!(
            later.text.contains("memory=hi"),
            "context survived the tabs: {}",
            later.text
        );
        assert_eq!(later.exits(), 0, "a Tab is not a death");
        assert_eq!(f.pi_pids(), vec![pid], "never respawned");
    }

    /// Orderly shutdown: close stdin so pi can dispose its runtime, and report one
    /// exit. No child left breathing.
    #[tokio::test]
    async fn shutdown_closes_stdin_and_reports_one_exit() {
        let f = fakes("shutdown");
        let (mut s, mut rx) = pi_chat(&f, 1);
        send_and_settle(&mut s, &f, &mut rx, "hi").await;
        let pid = f.pi_pids()[0];

        s.shutdown().unwrap();
        let down = loop {
            match next_event(&mut rx).await {
                l if l.starts_with("down ") => break l,
                l if l.starts_with("error") => panic!("errored on the way out: {l}"),
                _ => {}
            }
        };
        assert!(down.contains("Shutdown"), "{down}");
        assert!(!process_alive(pid), "shutdown must leave no child");
        assert_eq!(s.status(), SessionStatus::Dead);

        // The exactly-one-Exited promise, from this side: the stream ends.
        assert_eq!(
            drain(&mut rx)
                .iter()
                .filter(|l| l.starts_with("down "))
                .count(),
            0
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv())
                .await
                .map(|maybe| maybe.is_none())
                .unwrap_or(false),
            "the session stream closes after the exit"
        );
    }

    /// Every protocol record reaches the App unchanged: the session consumes the
    /// control half (running / aborting) but never swallows what the transcript
    /// needs — and pi's copy of the user's message is passed through un-inspected,
    /// because suppressing it is the App's rule, made where the echo is.
    #[tokio::test]
    async fn control_state_is_consumed_but_the_records_still_stream_through() {
        let f = fakes("passthrough");
        let (mut s, mut rx) = pi_chat(&f, 1);

        s.send_text("tell me something".to_string()).unwrap();
        f.wait_for_log_line("prompt tell me something").await;
        f.settle();
        let turn = read_turn(&mut rx).await;

        assert!(turn.has("agent_start"), "{:?}", turn.lines);
        assert!(
            turn.has("MessageStart"),
            "message events must reach the App: {:?}",
            turn.lines
        );
        assert!(
            turn.has("role: \"user\"") || turn.has("MessageEnd"),
            "pi's user-message copy is forwarded for the App to decide about: {:?}",
            turn.lines
        );
        assert_eq!(turn.text, "reply 1 <memory=>", "{:?}", turn.text);
        assert!(!turn.text.is_empty());
    }
}
