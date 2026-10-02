use crate::state::state::MessageKind;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

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
