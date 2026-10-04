use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;
use ratatui::style::Color;

use ratatui::Frame;
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthStr;

/// The terminal states are a session-layer identity (ADR-0002), not a property of
/// this widget. The input box only cycles and labels them, so the type is imported
/// and re-exported here rather than owned here.
pub use crate::session::TerminalType;

impl TerminalType {
    fn color(self) -> Color {
        match self {
            Self::Bash => Color::Blue,
            Self::Beeds => Color::DarkGray,
            Self::Pi => Color::Yellow,
        }
    }
}

pub enum InputAction {
    Submit {
        text: String,
        mode: TerminalType,
    },
    /// Tab. The input box has already moved to `to` when this is returned; the
    /// Router is told `from` so it can apply that mode's switch-away policy
    /// (ADR-0002 Q3) instead of having to remember what it was last showing.
    SwitchMode {
        from: TerminalType,
        to: TerminalType,
    },
    Cancel,
}

pub struct InputState {
    text: String,
    pub mode: TerminalType,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            text: "".to_string(),
            mode: TerminalType::Beeds,
        }
    }

    /// What is in the box right now.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Put text back in the box, from the app side rather than the keyboard —
    /// Pi's `Esc` handing back the messages it had queued.
    ///
    /// Newlines are folded to single spaces: this box is single-line, and folding is
    /// the difference between restoring the text and restoring something that
    /// submits itself. The message boundaries are not lost — they are already in
    /// the transcript.
    pub fn set_text(&mut self, text: String) {
        self.text = text.replace('\n', " ").trim_start().to_string();
    }
    pub fn handle_key(&mut self, k: KeyEvent) -> Option<InputAction> {
        match k.code {
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.text);
                (!text.trim().is_empty()).then_some(InputAction::Submit {
                    text,
                    mode: self.mode,
                })
            }
            KeyCode::Tab => {
                let from = self.mode;
                self.mode = self.mode.next();
                Some(InputAction::SwitchMode {
                    from,
                    to: self.mode,
                })
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
            .title(self.mode.label())
            .border_style(self.mode.color());
        f.render_widget(Paragraph::new(self.text.as_str()).block(block), area);
        f.set_cursor_position((area.x + 1 + self.text.width() as u16, area.y + 1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Tab is no longer invisible to the rest of the app: it produces a command,
    /// so a mode switch can tear down / bring up backends (looprs-05j).
    #[test]
    fn tab_reports_the_switch_it_made() {
        let mut s = InputState::new();
        assert_eq!(s.mode, TerminalType::Beeds);
        let Some(InputAction::SwitchMode { from, to }) = s.handle_key(key(KeyCode::Tab)) else {
            panic!("Tab must produce SwitchMode");
        };
        assert_eq!(from, TerminalType::Beeds);
        assert_eq!(to, TerminalType::Pi);
        assert_eq!(s.mode, TerminalType::Pi, "the box moved too");
    }

    /// A Submit carries the mode it was typed into: intent, from the keyboard.
    #[test]
    fn submit_carries_the_mode_it_was_typed_in() {
        let mut s = InputState::new();
        s.handle_key(key(KeyCode::Char('h')));
        s.handle_key(key(KeyCode::Char('i')));
        let Some(InputAction::Submit { text, mode }) = s.handle_key(key(KeyCode::Enter)) else {
            panic!("Enter must produce Submit");
        };
        assert_eq!(text, "hi");
        assert_eq!(mode, TerminalType::Beeds);
    }
}
