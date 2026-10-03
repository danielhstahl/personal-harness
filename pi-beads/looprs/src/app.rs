//! Typed wire format for pi's session events (json.md / rpc.md), deserialized directly with serde,
//! plus the one place that turns them into transcript changes.
//!
//! Deps: serde = { version = "1", features = ["derive"] }, serde_json.
//! serde ignores unknown *fields* by default (don't add deny_unknown_fields), and unknown event
//! *types* land in `Unknown`, so additive protocol changes are harmless. Renamed/removed fields
//! show up as parse errors; log them loudly (see `parse`).

use crate::components::input::{InputAction, InputState};
use crate::state::state::{MessageKind, Transcript};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;

pub enum Msg {
    Term(Event),
    Agent(Option<PiEvent>),
    Tick,
}

///add more (eg change terminal)
pub enum UiCommand {
    UserMessage(String),
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
    //out_rx: Receiver<String>, // agent -> UI
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
                if let Some(ev) = ev {
                    self.on_pi(ev);
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
                InputAction::Submit { text, ../*mode*/ } => {
                    self.chat_state = ChatState::Chat;
                    //todo, send different if in different mode
                    let _ = self.cmd_tx.try_send(UiCommand::UserMessage(text.clone()));
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
            PiEvent::AgentSettled => self.chat_state = ChatState::Stopped,
            // AutoRetryStart / CompactionStart: show a status note if you want one
            _ => {}
        }
    }

    /*fn on_agent(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::AgentStart { .. } => {
                self.chat_state = ChatState::Chat;
            }
            AgentEvent::AgentEnd { error, .. } => {
                self.chat_state = ChatState::Stopped;
                if let Some(err) = error {
                    self.transcript.push_done(MessageKind::Error, err);
                }
            }
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => {
                self.chat_state = ChatState::Chat;
                match assistant_message_event {
                    AssistantMessageEvent::TextDelta {
                        delta,
                        content_index: _,
                        partial: _,
                    } => {
                        self.transcript.push_delta(MessageKind::Answer, &delta);
                    }
                    AssistantMessageEvent::ThinkingDelta {
                        delta,
                        content_index: _,
                        partial: _,
                    } => {
                        self.transcript.push_delta(MessageKind::Thinking, &delta);
                    }
                    _ => {}
                };
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                self.chat_state = ChatState::Tool;
                self.transcript.start_tool(
                    tool_call_id,
                    tool_name,
                    print_json_value_to_string(&args),
                );
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                self.transcript.finish_tool(
                    tool_call_id,
                    result
                        .content
                        .into_iter()
                        .filter_map(|c| match c {
                            ContentBlock::Text(t) => Some(t.text),
                            _ => None,
                        })
                        .collect(),
                    is_error,
                );
            }
            AgentEvent::ProviderError { message, .. } => {
                self.chat_state = ChatState::Stopped;
                self.transcript.push_done(MessageKind::Error, message);
            }
            AgentEvent::ExtensionError { error, .. } => {
                self.chat_state = ChatState::Stopped;
                self.transcript.push_done(MessageKind::Error, error);
            }
            _ => {}
        }
    }*/
}
