//! One row for one thing the session is doing: a tool call, or a compaction.
//!
//! These two kinds of transcript entry have nothing in common but the shape they
//! are drawn with — a glyph for the state, a name, some detail — and the same
//! lifecycle: open while the thing runs, final once it is reported done. Both are
//! "the session is busy with *this*", which is a different question from "text is
//! streaming", and so both go in the live region's card band rather than into the
//! prose preview above it.
//!
//! One dispatcher rather than two call sites matching on [`MessageKind`] by hand:
//! a kind that is openable but has no renderer would otherwise render as nothing,
//! which is the invisible-failure shape this row exists to remove.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::components::compaction::compaction_line;
use crate::components::tool::tool_line;
use crate::state::transcript::{Entry, MessageKind};

/// The live row for any open card entry.
///
/// Anything that is not a card renders as an empty line rather than panicking:
/// this is reached from the frame, and an unexpected kind is a missing renderer,
/// not a reason to stop drawing the rest of the screen.
pub fn card_line(e: &Entry, spinner: usize) -> Line<'static> {
    match &e.kind {
        MessageKind::Tool { .. } => tool_line(e, spinner),
        MessageKind::Compaction { .. } => compaction_line(e, spinner),
        _ => Line::default(),
    }
}

/// The live-region widget for one open card.
pub struct LiveCardPreview<'a> {
    entry: &'a Entry,
    spinner: usize,
}

impl<'a> LiveCardPreview<'a> {
    pub fn new(entry: &'a Entry, spinner: usize) -> Self {
        Self { entry, spinner }
    }
}

impl<'a> Widget for LiveCardPreview<'a> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(card_line(self.entry, self.spinner)).render(area, buf);
    }
}
