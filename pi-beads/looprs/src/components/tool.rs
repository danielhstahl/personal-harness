//Visualize tool execution
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::state::transcript::{Entry, MessageKind};
use crate::utils::render::FRAMES;

/// The tool card.
///
/// Drawn from the [`Entry`] alone, live (with a spinner) and again once finished
/// from the scrollback, which is why nothing here holds state of its own.
/// The live-region widget that wraps it is
/// [`LiveCardPreview`](crate::components::card::LiveCardPreview) — the same one
/// the compaction card uses.
pub fn tool_line(e: &Entry, spinner: usize) -> Line<'static> {
    let MessageKind::Tool {
        name, state, input, ..
    } = &e.kind
    else {
        return Line::default();
    };
    let spans = vec![Span::styled(
        format!("{} {name} {input} ", state.icon(spinner)),
        Style::new().fg(state.color()),
    )];
    Line::from(spans)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ToolStateCategory {
    InProgress,
    Error,
    Success,
    /// The session ended while this tool was still open — `SessionView::seal` found
    /// it that way and closed it, because an entry left `!done` stalls the flusher
    /// forever.
    ///
    /// Its own state rather than `Error`: we do not know whether the tool ran, only
    /// that nothing is coming to report it, and painting "unknown" in failure's
    /// red would be a guess dressed as a fact. Freezing the `InProgress` spinner
    /// was worse still — a card that promises to still be working in a transcript
    /// whose process is gone.
    Aborted,
}

impl ToolStateCategory {
    fn color(self) -> Color {
        match self {
            Self::InProgress => Color::Blue,
            Self::Error => Color::Red,
            Self::Success => Color::Green,
            Self::Aborted => Color::DarkGray,
        }
    }
    fn icon(self, spinner: usize) -> &'static str {
        match self {
            Self::InProgress => FRAMES[spinner % FRAMES.len()],
            Self::Error => "✗",
            Self::Success => "✓",
            Self::Aborted => "⊘",
        }
    }
}
