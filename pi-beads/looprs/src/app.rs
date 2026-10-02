use crate::components::input::{InputAction, InputState};
use crate::state::state::{MessageKind, Transcript};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use pi::model::AssistantMessageEvent;
use pi::sdk::{AgentEvent, ContentBlock};
use serde_json::Value;

use tokio::sync::mpsc;
pub enum Msg {
    Term(Event),
    Agent(AgentEvent),
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
                self.on_agent(ev);
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

    fn on_agent(&mut self, ev: AgentEvent) {
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
    }
}
