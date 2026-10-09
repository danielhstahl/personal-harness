use crate::components::scrollback::Flusher;
use crate::state::state::Transcript;

use crate::utils::utils::FRAMES;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

pub struct LiveTextPreview<'a> {
    spinner: usize,
    transcript: &'a Transcript,
    flusher: &'a Flusher,
}

impl<'a> LiveTextPreview<'a> {
    pub fn new(
        spinner: usize,
        transcript: &'a Transcript,
        flusher: &'a Flusher, /* , style: Style*/
    ) -> Self {
        Self {
            spinner,
            transcript,
            flusher,
        }
    }
}

impl<'a> Widget for LiveTextPreview<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines = self.flusher.preview(self.transcript, area.width);
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
