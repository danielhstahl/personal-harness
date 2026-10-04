use crate::utils::render::FRAMES;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

/// The live (not-yet-final) tail, bottom-aligned in the space it is given.
///
/// It is handed the lines rather than a [`SessionView`](crate::session::view::SessionView)
/// because the frame needs those same lines *before* it draws: the height of the
/// live region is computed from how many of them there are (looprs-afw), and a
/// second render would be a second opinion about what the pane is showing. The
/// pairing is still the invariant — it is just enforced one level up, by
/// [`App::preview_active`](crate::app::App::preview_active) being the only door to
/// a preview, and that door only ever reads the active view's own
/// transcript-plus-flusher.
pub struct LiveTextPreview<'a> {
    spinner: usize,
    lines: &'a [Line<'static>],
}

impl<'a> LiveTextPreview<'a> {
    pub fn new(spinner: usize, lines: &'a [Line<'static>]) -> Self {
        Self { spinner, lines }
    }
}

impl Widget for LiveTextPreview<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut lines = self.lines.to_vec();
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
