use crate::components::input::{InputAction, InputState};
use crate::state::state::{MessageKind, Transcript};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use pi::model::AssistantMessageEvent;
use pi::sdk::{AgentEvent, ContentBlock};

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

pub struct App {
    pub input: InputState,
    pub transcript: Transcript,
    //pub running: bool,
    pub spinner: usize,
    pub width: u16,
    //pub dirty: bool,
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
            should_quit: false,
            //answer: BlockStream::default(),
            //thinking: BlockStream::default(),
            //block_stream_type: BlockStreamType::Answer,
            //running: false,
            spinner: 0,
            //scrollback: vec![],
            //active_tools: vec![],
            //width,
            //dirty: true,
            //should_quit: false,
            cmd_tx,
        }
    }

    pub fn update(&mut self, msg: Msg) {
        match msg {
            Msg::Tick => {
                //if self.running {
                self.spinner = self.spinner.wrapping_add(1);
                //self.dirty = true;
                // }
            }
            Msg::Term(Event::Resize(w, _)) => {
                self.width = w;
                //self.dirty = true;
            }
            Msg::Term(Event::Key(k)) if k.kind == KeyEventKind::Press => self.on_key(k),
            Msg::Term(_) => {}
            Msg::Agent(ev) => self.on_agent(ev),
        }
    }

    fn on_key(&mut self, k: crossterm::event::KeyEvent) {
        //self.dirty = true;
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            self.should_quit = true;
            return;
        }

        if let Some(action) = self.input.handle_key(k) {
            match action {
                InputAction::Submit { text, mode } => {
                    //todo, send different if in different mode
                    let _ = self.cmd_tx.try_send(UiCommand::UserMessage(text.clone()));
                    self.transcript.push_done(MessageKind::User, text);
                }
                InputAction::Cancel => {
                    let _ = self.cmd_tx.try_send(UiCommand::Cancel);
                }
            }
        }

        /*match k.code {
            KeyCode::Enter if !self.running && !self.input.trim().is_empty() => {
                let text = std::mem::take(&mut self.input);
                let w = self.md_width() as usize;
                let mut lines =
                    md::wrap(vec![Span::raw(text.clone())], w, "❯ ".into(), "  ".into());
                lines.push(Line::default());
                self.scrollback.extend(lines);
                self.running = true;
                let _ = self.cmd_tx.try_send(UiCommand::UserMessage(text));
            }
            KeyCode::Esc if self.running => {
                let _ = self.cmd_tx.try_send(UiCommand::Cancel);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Char(c) => self.input.push(c),
            _ => {}
        }*/
    }

    fn on_agent(&mut self, ev: AgentEvent) {
        //let w = self.md_width();
        match ev {
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => {
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
                self.transcript
                    .start_tool(tool_call_id, tool_name, args.to_string());
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
                self.transcript.push_done(MessageKind::Error, message);
            }
            AgentEvent::ExtensionError { error, .. } => {
                self.transcript.push_done(MessageKind::Error, error);
            }
            _ => {}
        }
    }
}
