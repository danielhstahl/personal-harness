use crate::session::TerminalType;
use crate::state::transcript::MessageKind;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;

/// The colour of a mode, in the two places it is shown: the input box's border and
/// the status row (`components::status`).
///
/// One function because the ticket this replaces had the mode visible *only* as a
/// border colour, and a row that says `Beads` in a colour of its own would be a
/// second, contradicting answer to "which mode am I in".
pub fn mode_color(mode: TerminalType) -> Color {
    match mode {
        TerminalType::Bash => Color::Blue,
        TerminalType::Beeds => Color::DarkGray,
        TerminalType::Pi => Color::Yellow,
    }
}

pub fn content_width(term_width: u16) -> u16 {
    term_width.saturating_sub(2).max(20)
}
pub fn style_for(k: &MessageKind) -> Style {
    /* Thinking => italic+dim, others => default */
    match k {
        MessageKind::Thinking => Style::new()
            .add_modifier(Modifier::ITALIC)
            .add_modifier(Modifier::DIM),
        _ => Style::default(),
    }
}

/// The scrollback's trim marker (`looprs-pdl.7`).
///
/// Italic so it reads as *about* the transcript rather than as transcript — the
/// same trick `Thinking` uses, and the reason it works is that nothing else on
/// the band is italic except thinking, which is at least honest about being the
/// app's own voice. Dark-gray rather than a colour because the row is a loss
/// notice, not an answer, and it must not compete with the yellow the status row
/// spends on "the loop is working"; it stays readable on both palettes, which a
/// dim-only style is not.
pub fn trim_marker_style() -> Style {
    Style::new().dark_gray().add_modifier(Modifier::ITALIC)
}

pub fn restyle(mut line: Line<'static>, base: Style) -> Line<'static> {
    for span in &mut line.spans {
        span.style = base.patch(span.style); // span's own style wins
    }
    line
}
