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
pub fn restyle(mut line: Line<'static>, base: Style) -> Line<'static> {
    for span in &mut line.spans {
        span.style = base.patch(span.style); // span's own style wins
    }
    line
}
