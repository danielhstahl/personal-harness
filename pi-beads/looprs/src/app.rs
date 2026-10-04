//! Typed wire format for pi's session events (json.md / rpc.md), deserialized directly with serde,
//! plus the one place that turns them into transcript changes.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see `parse`).

use crate::components::input::{InputAction, InputState};
use crate::services::bd::{Bead, ready_beads_with};
use crate::services::pi::PiRpc;
use crate::services::prompts::{PLANNER, WORKER, generate_prompt};
use crate::session::{ByteStream, ExitReason, SessionConfig, SessionId, TerminalType};
use crate::state::state::{MessageKind, Transcript};
use anyhow::{Result, anyhow};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::{self, Receiver, UnboundedReceiver, UnboundedSender};

pub use crate::session::BeadStep;

/// Everything that can change the UI.
///
/// ADR-0002 Q2, the part that matters: **every message that came from a session is
/// tagged with the `SessionId` that produced it.** The App must never infer
/// provenance from `self.input.mode` — the mode is what the user last Tabbed to,
/// and it changes independently of what is running. Provenance-in-the-envelope is
/// what makes looprs-msj unrepeatable.
///
/// The rule in one line: **input mode is authoritative for intent (where my
/// keystrokes go); the envelope is authoritative for origin (who made this).**
///
/// Construct these through `session::router::wrap`, never by hand: `wrap` is the
/// only place that has a `SessionId` to attach.
#[derive(Debug)]
pub enum Msg {
    /// Raw terminal input. The only message with no origin, because it *is* the user.
    Term(Event),
    /// A pi protocol event, from the session named in `session`.
    Agent {
        // Provenance: unread until looprs-05j routes Msg into per-session
        // views (and looprs-msj keys the beads transition off it). Declared now so
        // no one can build these variants without saying who made them.
        #[allow(dead_code)]
        session: SessionId,
        event: PiEvent,
    },
    /// Shell output (ADR-0001). `stream` is `Merged` for a pty; `chunk` is a read
    /// buffer, not a line — do not re-split it.
    BashOutput {
        // Provenance: unread until looprs-05j routes Msg into per-session
        // views (and looprs-msj keys the beads transition off it). Declared now so
        // no one can build these variants without saying who made them.
        #[allow(dead_code)]
        session: SessionId,
        stream: ByteStream,
        chunk: String,
    },
    /// The beads machine moved. Rendered, never re-derived.
    BeadStep {
        #[allow(dead_code)] // consumer: looprs-05j's per-session view
        session: SessionId,
        step: BeadStep,
    },
    /// A session's child is gone. Guaranteed exactly once per session, so the
    /// receiver can always seal that session's transcript.
    SessionDown {
        // Provenance: unread until looprs-05j routes Msg into per-session
        // views (and looprs-msj keys the beads transition off it). Declared now so
        // no one can build these variants without saying who made them.
        #[allow(dead_code)]
        session: SessionId,
        reason: ExitReason,
    },
    /// A failure the human needs to see (spawn failure, `bd` failure, ...).
    /// `session: None` means it is harness-level (router/spawn), not a session's.
    Error {
        #[allow(dead_code)] // consumer: looprs-05j (route it to the right view / status row)
        session: Option<SessionId>,
        text: String,
    },
    /// A status line ("working looprs-1", "board empty, awaiting input").
    System {
        #[allow(dead_code)] // consumer: looprs-05j (route it to the right view / status row)
        session: Option<SessionId>,
        text: String,
    },
    Tick,
}

/// UI -> session layer. Every variant says which terminal state it is *for*.
#[derive(Debug)]
pub enum UiCommand {
    /// Enter in the input box. `mode` is where the text was typed: declared
    /// intent, and legitimate routing input. (Contrast with `Msg`, where the tag
    /// is origin and the input mode must not be consulted.)
    Submit { mode: TerminalType, text: String },
    /// Tab. Drives the per-mode switch-away policy (ADR-0002 Q3).
    SwitchMode {
        #[allow(dead_code)] // consumer: looprs-05j's switch policy
        from: TerminalType,
        #[allow(dead_code)] // consumer: looprs-05j's switch policy
        to: TerminalType,
    },
    /// Esc. Routed by the router to the *active* session only.
    Cancel,
    /// Legacy: the App asks the beads loop to advance. Goes away with looprs-msj,
    /// which moves that transition inside `BeadsSession` where it belongs.
    BeadsNext,
}

fn print_json_value_to_string(v: &Value) -> String {
    let mut s = "".to_string();
    if let Some(map) = v.as_object() {
        for (key, value) in map {
            s += &format!("{}: {}", key, value);
        }
    }
    s
}

//UI only
pub enum ChatState {
    Stopped,
    Chat,
    Tool,
}

/// Only fields you consume are declared; everything else in the record is skipped.
#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum PiEvent {
    AgentStart,
    AgentEnd,     // NOT "done": retries / steering / follow-ups can continue after this
    AgentSettled, // pi has no more automatic work => this is "done"
    TurnStart,
    TurnEnd,
    MessageStart {
        message: WireMessage,
    },
    MessageUpdate {
        assistant_message_event: AssistantEvent,
    },
    MessageEnd {
        message: WireMessage,
    }, // authoritative final message
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        partial_result: ToolOutput,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        result: ToolOutput,
        is_error: bool,
    },
    AutoRetryStart {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    AutoRetryEnd {
        success: bool,
        #[serde(default)]
        final_error: Option<String>,
    },
    CompactionStart {
        reason: String,
    },
    CompactionEnd {
        #[serde(default)]
        aborted: bool,
        #[serde(default)]
        error_message: Option<String>,
    },
    ExtensionError {
        extension_path: String,
        error: String,
    },
    #[serde(other)]
    Unknown, // must be last; swallows any event type you haven't modeled
}

#[derive(Debug, Deserialize)]
pub struct WireMessage {
    pub role: String, // "user" | "assistant" | "toolResult" ...; content left undeclared for now
}

/// The nested `assistantMessageEvent` of `message_update` (delta-only on the wire).
#[derive(Debug, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum AssistantEvent {
    Start,
    TextStart {
        content_index: usize,
    },
    TextDelta {
        content_index: usize,
        delta: String,
    },
    TextEnd {
        content_index: usize,
        content: String,
    },
    ThinkingStart {
        content_index: usize,
    },
    ThinkingDelta {
        content_index: usize,
        delta: String,
    },
    ThinkingEnd {
        content_index: usize,
        content: String,
    },
    ToolcallStart {
        content_index: usize,
        id: String,
        tool_name: String,
    },
    ToolcallDelta {
        content_index: usize,
        delta: String,
    }, // serialized (partial) argument JSON
    ToolcallEnd {
        content_index: usize,
        tool_call: Value,
    },
    Done {
        reason: String,
    },
    Error {
        reason: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToolOutput {
    #[serde(default)]
    pub content: Vec<Block>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    #[serde(other)]
    Other,
}

impl ToolOutput {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                Block::Other => None,
            })
            .collect()
    }
}

/// Borrow the Value (no clone per token) and keep it for the error log.
pub fn parse(v: &Value) -> Option<PiEvent> {
    match PiEvent::deserialize(v) {
        Ok(ev) => Some(ev),
        Err(e) => {
            tracing::warn!(error = %e, raw = %v, "unparseable pi event");
            None
        }
    }
}

pub struct App {
    pub input: InputState,
    pub transcript: Transcript,
    pub chat_state: ChatState,
    pub spinner: usize,
    pub width: u16,
    pub dirty: bool,
    pub should_quit: bool,
    pub need_input: bool,
    cmd_tx: mpsc::Sender<UiCommand>, // UI -> agent
}
impl App {
    pub fn new(
        cmd_tx: mpsc::Sender<UiCommand>,
        input: InputState,
        need_input: bool,
        transcript: Transcript,
        width: u16,
    ) -> Self {
        Self {
            input,
            transcript,
            width,
            need_input,
            chat_state: ChatState::Stopped,
            dirty: true,
            should_quit: false,
            spinner: 0,
            cmd_tx,
        }
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => {
                if !matches!(self.chat_state, ChatState::Stopped) {
                    self.spinner = self.spinner.wrapping_add(1);
                    self.dirty = true;
                }
            }
            Msg::Term(Event::Resize(w, _)) => {
                self.width = w;
                self.dirty = true;
            }
            Msg::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                self.dirty = true;
                self.on_key(k);
            }
            Msg::Term(_) => {}
            Msg::Agent { session, event } => {
                self.dirty = true;
                self.on_pi(session, event);
            }
            Msg::BashOutput {
                session,
                stream,
                chunk,
            } => {
                // Not rendered yet: looprs-553 owns the Bash display path (raw
                // passthrough + an escape-stripped copy, never markdown).
                let _ = (session, stream, chunk);
            }
            Msg::BeadStep { session: _, step } => {
                self.dirty = true;
                match step {
                    BeadStep::AwaitInput => self.need_input = true,
                    BeadStep::CreateTickets => self.need_input = false,
                    BeadStep::WorkTickets => self.need_input = false,
                }
            }
            Msg::SessionDown { session, reason } => {
                // looprs-05j/looprs-ecr: apply to *that* session's SessionView —
                // `view.seal()` so its flusher never stalls on an entry that will
                // never be closed, and mark status Dead. A no-op here on purpose:
                // with one shared transcript today, sealing on every pass boundary
                // would be visible churn in another ticket's scope.
                let _ = (session, reason);
            }
            Msg::Error { session: _, text } => {
                self.dirty = true;
                self.chat_state = ChatState::Chat; // make sure the line is on screen
                self.transcript.push_done(MessageKind::Error, text);
            }
            Msg::System { session: _, text } => {
                self.dirty = true;
                self.chat_state = ChatState::Chat;
                self.transcript.push_done(MessageKind::System, text);
            }
        }
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) {
        self.dirty = true;
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if let Some(action) = self.input.handle_key(k) {
            match action {
                InputAction::Submit { text, mode } => {
                    self.chat_state = ChatState::Chat;
                    match mode {
                        TerminalType::Beeds => {
                            let _ = self.cmd_tx.try_send(UiCommand::Submit {
                                mode,
                                text: text.clone(),
                            });
                        }
                        TerminalType::Pi => {
                            // Still unconsumed until looprs-ctn; it is now at least
                            // addressed to a session that can answer it.
                            let _ = self.cmd_tx.try_send(UiCommand::Submit {
                                mode,
                                text: text.clone(),
                            });
                        }
                        TerminalType::Bash => {
                            // Not implemented yet (looprs-553). The shell itself is a real pty,
                            // not a piped `bash -i`, and Ctrl-C is forwarded to it rather than
                            // quitting looprs -- see docs/adr/0001-bash-terminal-state-pty.md.
                            let _ = &text;
                        }
                    }
                    self.transcript.push_done(MessageKind::User, text);
                }
                InputAction::SwitchMode { from, to } => {
                    let _ = self.cmd_tx.try_send(UiCommand::SwitchMode { from, to });
                }
                InputAction::Cancel => {
                    let _ = self.cmd_tx.try_send(UiCommand::Cancel);
                }
            }
        }
    }

    /// `session` is threaded through and deliberately unused for now.
    ///
    /// The `matches!(self.input.mode, ...)` below is looprs-msj: the beads
    /// transition must key off `session` (and the beads session's own step), not
    /// off whatever the input box is showing. Fixed there, with the tests that were
    /// specified; left alone here so this ticket stays type-only.
    pub fn on_pi(&mut self, session: SessionId, ev: PiEvent) {
        // Nothing reads `session` yet. That is exactly looprs-msj's bug and its fix:
        // the beads transition must key off this id, not off `self.input.mode`.
        let _ = session;
        self.dirty = true; // gate this per-arm if you want to skip no-op events
        let t = &mut self.transcript;
        match ev {
            PiEvent::MessageUpdate {
                assistant_message_event: e,
            } => {
                self.chat_state = ChatState::Chat;
                match e {
                    AssistantEvent::TextDelta { delta, .. } => {
                        t.push_delta(MessageKind::Answer, &delta)
                    }
                    AssistantEvent::ThinkingDelta { delta, .. } => {
                        t.push_delta(MessageKind::Thinking, &delta)
                    }
                    _ => {}
                }
            }
            // user messages are already echoed locally on submit; ignore pi's copy
            PiEvent::MessageEnd { message } if message.role == "assistant" => t.finish_last(),
            PiEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.chat_state = ChatState::Tool;
                t.start_tool(tool_call_id, tool_name, print_json_value_to_string(&args)) // upsert: fills in the args
            }
            // PiEvent::ToolExecutionUpdate { .. } => stream partial output into the row if you want it
            PiEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
            } => t.finish_tool(tool_call_id, result.text(), is_error),
            PiEvent::AgentSettled => {
                self.chat_state = ChatState::Stopped;
                if matches!(self.input.mode, TerminalType::Beeds) {
                    //no await
                    //self.bd_loop.next();
                    tracing::debug!("Agent Settled, going to BeedsNext");
                    let _ = self.cmd_tx.send(UiCommand::BeadsNext);
                };
            }
            // AutoRetryStart / CompactionStart: show a status note if you want one
            _ => {}
        }
    }
}

pub enum PiStep {
    AwaitInput,
    DoingWork,
}

pub struct PiLoop {
    pi_rx: Option<(PiRpc, UnboundedReceiver<Value>)>,
    pi_step: PiStep,
}

/// The beads terminal state, as a `BeadsLoop`. `session::stubs::BeadsSession` is
/// the type this becomes when looprs-msj moves the machine behind `dyn Session`;
/// until then this struct is the concrete thing main.rs drives.
pub struct BeadsLoop {
    /// This loop's identity, stamped on every `Msg` it emits (ADR-0002 Q2).
    id: SessionId,
    /// The pi session currently driven by the loop. `None` whenever the loop is parked.
    pi_rx: Option<PiRpc>,
    ev_tx: UnboundedSender<Msg>,
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
    pub fn new(id: SessionId, ev_tx: UnboundedSender<Msg>, cfg: SessionConfig) -> Self {
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
        let _ = self.ev_tx.send(Msg::BeadStep {
            session: self.id,
            step: s,
        });
    }

    pub fn get_step(&self) -> &BeadStep {
        &self.bead_step
    }

    pub fn is_awaiting_input(&self) -> bool {
        matches!(self.bead_step, BeadStep::AwaitInput)
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

    /// Pipe a pi event stream into the UI until the child's stdout closes.
    fn forward_pi_events(&self, mut ev_rx: mpsc::UnboundedReceiver<Value>) {
        let tx = self.ev_tx.clone();
        let sid = self.id;
        tokio::spawn(async move {
            while let Some(v) = ev_rx.recv().await {
                if let Some(ev) = parse(&v)
                    && tx
                        .send(Msg::Agent {
                            session: sid,
                            event: ev,
                        })
                        .is_err()
                {
                    break;
                }
            }
        });
    }

    fn report_error(&self, text: String) {
        let _ = self.ev_tx.send(Msg::Error {
            session: Some(self.id),
            text,
        });
    }

    fn report_system(&self, text: String) {
        let _ = self.ev_tx.send(Msg::System {
            session: Some(self.id),
            text,
        });
    }

    //listens for input from cmd OR for a trigger
    pub fn listen_input(mut self, mut cmd_rx: Receiver<UiCommand>) {
        tokio::spawn(async move {
            while let Some(input) = cmd_rx.recv().await {
                tracing::debug!("Recieved input on cmd_rx: {:?}", input);
                match input {
                    UiCommand::Submit {
                        mode: TerminalType::Beeds,
                        text,
                    } => {
                        if let Err(e) = self.launch_create_tickets(&text).await {
                            tracing::error!("planner pass failed: {e:#}");
                            self.report_error(format!("planner: {e:#}"));
                            self.set_step(BeadStep::AwaitInput);
                        }
                    }
                    UiCommand::BeadsNext => self.next().await,
                    // Not ours: Pi/Bash submits and mode switches belong to the
                    // router the moment looprs-05j gives us one.
                    _ => {}
                }
            }
        });
    }

    pub async fn close(&mut self) {
        if let Some(mut old_pi) = self.pi_rx.take() {
            let _ = old_pi.kill().await;
        }
    }

    async fn launch_create_tickets(&mut self, instructions: &str) -> Result<()> {
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

impl PiLoop {
    pub fn new() -> Result<Self> {
        let args = vec![];
        let (pi, ev_rx) = PiRpc::spawn(&args)?;
        Ok(Self {
            pi_rx: Some((pi, ev_rx)),
            pi_step: PiStep::AwaitInput,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD, PiFake, process_alive};
    use tokio::time::{Duration, timeout};

    const SECOND_BOARD: &str = r#"{
  "data": [
    {"id": "looprs-99", "title": "the next ticket", "status": "open", "issue_type": "task"}
  ],
  "schema_version": 1
}"#;

    /// Generous, but bounded: a hang is a failure of this ticket, and a bounded test
    /// reports it instead of wedging the suite.
    const NO_HANG: Duration = Duration::from_secs(10);

    fn loop_with(fakes: &Fakes) -> (BeadsLoop, UnboundedReceiver<Msg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let cfg = SessionConfig {
            pi_bin: fakes.pi_bin().to_string(),
            bd_bin: fakes.bd_bin().to_string(),
            ..Default::default()
        };
        let id = SessionId::new(TerminalType::Beeds, 0);
        (BeadsLoop::new(id, tx, cfg), rx)
    }

    /// Snapshot of what the loop told the UI, as stable strings.
    fn drain(rx: &mut UnboundedReceiver<Msg>) -> Vec<String> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(match m {
                Msg::BeadStep {
                    step: BeadStep::AwaitInput,
                    ..
                } => "step:await".into(),
                Msg::BeadStep {
                    step: BeadStep::CreateTickets,
                    ..
                } => "step:plan".into(),
                Msg::BeadStep {
                    step: BeadStep::WorkTickets,
                    ..
                } => "step:work".into(),
                Msg::Error { text, .. } => format!("error: {text}"),
                Msg::System { text, .. } => format!("system: {text}"),
                Msg::Agent { .. } => "agent".into(),
                Msg::SessionDown { .. } => "session-down".into(),
                Msg::BashOutput { .. } => "bash".into(),
                Msg::Term(_) | Msg::Tick => "ui".into(),
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
}
