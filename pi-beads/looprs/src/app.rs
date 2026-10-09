//! Typed wire format for pi's session events (json.md / rpc.md), deserialized directly with serde,
//! plus the one place that turns them into transcript changes.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see `parse`).

use crate::components::input::{InputAction, InputState, TerminalType};
use crate::services::bd::get_ready_beads;
use crate::services::pi::PiRpc;
use crate::services::prompts::{PLANNER, WORKER, generate_prompt};
use crate::state::state::{MessageKind, Transcript};
use anyhow::Result;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::{self, Receiver, UnboundedReceiver, UnboundedSender};

pub enum Msg {
    Term(Event),
    Agent(PiEvent),
    BeadStep(BeadStep),
    Tick,
}

///add more (eg change terminal)
#[derive(Debug)]
pub enum UiCommand {
    UserMessage(String),
    UserBeadMessage(String),
    BeadsNext,
    Cancel,
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
        transcript: Transcript,
        width: u16,
    ) -> Self {
        Self {
            input,
            transcript,
            width,
            need_input: true,
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
            Msg::Agent(ev) => {
                self.dirty = true;
                self.on_pi(ev);
            }
            Msg::BeadStep(bead) => {
                self.dirty = true;
                match bead {
                    BeadStep::AwaitInput => self.need_input = true,
                    BeadStep::CreateTickets => self.need_input = false,
                    BeadStep::WorkTickets => self.need_input = false,
                }
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
                            let _ = self
                                .cmd_tx
                                .try_send(UiCommand::UserBeadMessage(text.clone()));
                        }
                        TerminalType::Pi => {
                            let _ = self.cmd_tx.try_send(UiCommand::UserMessage(text.clone()));
                        }
                        TerminalType::Bash => {
                            //todo
                            //let _ = self.cmd_tx.try_send(UiCommand::UserMessage(text.clone()));
                        }
                    }
                    self.transcript.push_done(MessageKind::User, text);
                }
                InputAction::Cancel => {
                    let _ = self.cmd_tx.try_send(UiCommand::Cancel);
                }
            }
        }
    }

    pub fn on_pi(&mut self, ev: PiEvent) {
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
#[derive(Clone)]
pub enum BeadStep {
    AwaitInput,
    CreateTickets,
    WorkTickets,
}
pub struct BeadsLoop {
    pi_rx: Option<PiRpc>,
    ev_tx: UnboundedSender<Msg>,
    bead_step: BeadStep,
}

fn bd_ready(ev_tx: UnboundedSender<Msg>) -> Result<Option<PiRpc>> {
    let beads = get_ready_beads()?;
    if let Some(bead) = beads.first() {
        let args = vec![];
        let (pi, mut ev_rx) = PiRpc::spawn(&args)?;
        tokio::spawn(async move {
            while let Some(v) = ev_rx.recv().await {
                tracing::debug!("Receiving information {}", v);
                if let Some(ev) = parse(&v) {
                    if ev_tx.send(Msg::Agent(ev)).is_err() {
                        break;
                    }
                }
            }
        });
        Ok(Some(pi))
    } else {
        Ok(None)
    }
}

//stateful with the invocation of bd ready
impl BeadsLoop {
    pub fn new(ev_tx: UnboundedSender<Msg>) -> Result<Self> {
        if let Some(pi) = bd_ready(ev_tx.clone())? {
            Ok(Self {
                pi_rx: Some(pi),
                ev_tx,
                bead_step: BeadStep::WorkTickets,
            })
        } else {
            Ok(Self {
                pi_rx: None,
                ev_tx,
                bead_step: BeadStep::AwaitInput,
            })
        }
    }
    fn set_step(&mut self, s: BeadStep) {
        self.bead_step = s.clone(); // keep the field private
        let _ = self.ev_tx.send(Msg::BeadStep(s));
    }
    //listens for input from cmd OR for a trigger
    pub fn listen_input(mut self, mut cmd_rx: Receiver<UiCommand>) {
        tokio::spawn(async move {
            while let Some(input) = cmd_rx.recv().await {
                tracing::debug!("Recieved input on cmd_rx: {:?}", input);
                match input {
                    UiCommand::UserBeadMessage(text) => {
                        let res = self.launch_create_tickets(&text).await;
                        match res {
                            Ok(_v) => tracing::debug!("Success"),
                            Err(e) => tracing::error!("This is err: {}", e),
                        };
                    }
                    UiCommand::BeadsNext => {
                        let res = self.next().await;
                        match res {
                            Ok(_v) => tracing::debug!("Success"),
                            Err(e) => tracing::error!("This is err: {}", e),
                        };
                    }
                    _ => {}
                }
            }
        });
    }
    pub async fn close(&mut self) -> Result<()> {
        if let Some(mut old_pi) = self.pi_rx.take() {
            old_pi.kill().await?;
            //old_rx.close();
        }
        Ok(())
    }
    pub async fn next(&mut self) -> Result<()> {
        self.close().await?;
        if let Some(pi) = bd_ready(self.ev_tx.clone())? {
            let res = pi.prompt(&WORKER).await?; //consider passing bead id into context so worker doesn't have to run `bd ready`
            self.pi_rx = Some(pi);
            self.set_step(BeadStep::WorkTickets);
            //self.bead_step = BeadStep::WorkTickets;
        } else {
            self.set_step(BeadStep::AwaitInput);
            //self.bead_step = BeadStep::AwaitInput;
        }
        Ok(())
    }
    async fn launch_create_tickets(&mut self, instructions: &str) -> Result<()> {
        tracing::debug!("Launched tickets with these instructions: {}", instructions);
        let args = vec!["--tools", "read,bash"];
        let (pi, mut ev_rx) = PiRpc::spawn(&args)?;
        self.set_step(BeadStep::CreateTickets);
        let tx = self.ev_tx.clone();
        tokio::spawn(async move {
            while let Some(v) = ev_rx.recv().await {
                tracing::debug!("Receiving information {}", v);
                if let Some(ev) = parse(&v) {
                    if tx.send(Msg::Agent(ev)).is_err() {
                        break;
                    }
                }
            }
        });
        let prompt = generate_prompt(PLANNER, instructions);
        let _res = pi.prompt(&prompt).await?;
        self.pi_rx = Some(pi);

        Ok(())
    }
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
