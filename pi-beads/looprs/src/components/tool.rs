//Visualize tool execution
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::state::transcript::{Entry, MessageKind};
use crate::theme::styles::{BLUE, RED};
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
            Self::InProgress => BLUE,
            Self::Error => RED,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::transcript::Transcript;

    /// An in-flight tool is the palette's light blue; a failed one its soft red.
    ///
    /// These are the colours a user reads a whole transcript by, which is why
    /// neither is an ANSI basic: `Color::Blue` sits at about the background's
    /// luminance on a dark terminal, and `Color::Red` is shrill enough to make
    /// the error text next to it harder to read, not easier. Both come from
    /// `theme::styles` so there is one blue and one red in the app.
    #[test]
    fn in_flight_is_the_light_blue_and_failure_the_soft_red() {
        let mut t = Transcript::new();
        t.start_tool("t1".into(), "bash".into(), "make test".into());
        assert_eq!(
            tool_line(&t.entries[0], 0).spans[0].style.fg,
            Some(BLUE),
            "a running card must be readable, which the old blue was not"
        );

        t.finish_tool("t1".into(), "boom".into(), true);
        assert_eq!(
            tool_line(&t.entries[0], 0).spans[0].style.fg,
            Some(RED),
            "a failed card keeps the signal and loses the shout"
        );
    }
}
