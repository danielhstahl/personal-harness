use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Color;

use ratatui::Frame;
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;
#[derive(Clone, Copy)]
pub enum TerminalType {
    Bash,
    Beeds,
    Pi,
}

impl TerminalType {
    fn color(self) -> Color {
        match self {
            Self::Bash => Color::Blue,
            Self::Beeds => Color::DarkGray,
            Self::Pi => Color::Yellow,
        }
    }
    fn title(self) -> &'static str {
        match self {
            Self::Bash => "Bash",
            Self::Beeds => "Beeds",
            Self::Pi => "Pi",
        }
    }
    fn next(self) -> Self {
        match self {
            Self::Bash => Self::Beeds,
            Self::Beeds => Self::Pi,
            Self::Pi => Self::Bash,
        }
    }
}

pub enum InputAction {
    Submit { text: String, mode: TerminalType },
    Cancel,
}

pub struct InputState {
    text: String,
    mode: TerminalType,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            text: "".to_string(),
            mode: TerminalType::Beeds,
        }
    }
    pub fn handle_key(&mut self, k: KeyEvent) -> Option<InputAction> {
        match k.code {
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.text);
                (!text.trim().is_empty()).then(|| InputAction::Submit {
                    text,
                    mode: self.mode,
                })
            }
            KeyCode::Tab => {
                self.mode = self.mode.next();
                None
            }
            KeyCode::Esc => Some(InputAction::Cancel),
            KeyCode::Backspace => {
                self.text.pop();
                None
            }
            KeyCode::Char(c) => {
                self.text.push(c);
                None
            }
            _ => None,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(self.mode.title())
            .border_style(self.mode.color());
        f.render_widget(Paragraph::new(self.text.as_str()).block(block), area);
        f.set_cursor_position((area.x + 1 + self.text.width() as u16, area.y + 1));
    }
}
