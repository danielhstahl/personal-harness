use crate::session::view::SessionView;

use crate::utils::utils::FRAMES;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

/// The live (not-yet-final) tail of **one** session's stream.
///
/// It takes a [`SessionView`] rather than a transcript + flusher pair because that
/// pairing is the invariant: this widget can only ever preview the transcript its
/// cursor belongs to. Handing it two independently-sourced halves was how a
/// session you had tabbed away from could end up rendered in the mode you were
/// looking at.
pub struct LiveTextPreview<'a> {
    spinner: usize,
    view: &'a SessionView,
}

impl<'a> LiveTextPreview<'a> {
    pub fn new(spinner: usize, view: &'a SessionView) -> Self {
        Self { spinner, view }
    }
}

impl<'a> Widget for LiveTextPreview<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines = self.view.preview(area.width);
        if lines.is_empty() {
            lines.push(Line::styled(
                FRAMES[self.spinner % FRAMES.len()],
                Style::new().cyan(),
            ));
        }
        let keep = lines.len().saturating_sub(area.height as usize);
        let visible = lines.split_off(keep);
        Paragraph::new(visible).render(area, buf);
    }
}
