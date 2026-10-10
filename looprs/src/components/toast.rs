//! Drawing the toast (looprs-pdl.10).
//!
//! The state is [`crate::state::toast`]; this is the one widget that turns it
//! into pixels, and the whole contract is one sentence: **cover a fixed slot in
//! the corner of the band and change nothing else.**
//!
//! # Why an overlay
//!
//! Because the alternative — a row — is a reshape, and ADR-0004 R21 measured
//! what a reshape costs: a hole between the erase and the bytes that replace it,
//! worst case **8.8–9.3 ms**, per reshape (`spikes/results/flash-e2e-at-pdl5.log`,
//! 25 reshapes over a 1.5 ms budget). A copy toast fires on every drag release,
//! and the text it is talking about is the text the user is still looking at. A
//! row would re-lay the transcript out from under the pointer; an overlay
//! covers some cells in the corner for two seconds and moves nothing.
//!
//! The cells it covers are accounted for in the words it shows: "Copied 1,284
//! characters" is the receipt for whatever transcript text is behind it.
//!
//! # Why reverse video
//!
//! Same reasoning as [`crate::components::selection`]: a fixed colour is legible
//! in one theme and invisible in another, and the toast lands on top of whatever
//! the transcript happens to be — prose, code with a syntax palette nobody here
//! chose, a dimmed thinking block. Reverse video is not a colour, it is a swap
//! of what is already there, so it survives every theme by construction. The
//! failure tone adds a red foreground on top of the swap, which is legible
//! against both halves of it.

use crate::theme::styles::RED;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

use crate::state::toast::Tone;

/// The pill, at the bottom-right of whatever area it is given.
///
/// The App hands it the **transcript band**, which is what R18 says to anchor it
/// to: the band is where the text the toast is talking about lives, and the
/// bottom-right is the one corner of that band that is not the tail the user is
/// reading.
pub struct ToastOverlay<'a> {
    text: &'a str,
    tone: Tone,
}

impl<'a> ToastOverlay<'a> {
    pub fn new(text: &'a str, tone: Tone) -> Self {
        Self { text, tone }
    }

    /// The pill's style for its tone.
    fn style(&self) -> Style {
        let s = Style::new().add_modifier(Modifier::REVERSED);
        match self.tone {
            Tone::Good => s,
            Tone::Bad => s.fg(RED).add_modifier(Modifier::BOLD),
        }
    }
}

impl Widget for ToastOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() || self.text.is_empty() {
            return;
        }
        // Padded by one space each side, and clipped rather than wrapped: a toast
        // that wrapped would be a toast that pushed the transcript around, which
        // is the one thing this widget exists not to do.
        let padded = format!(" {} ", self.text);
        let want = (self.text.chars().count() as u16 + 2).min(area.width);
        if want < 3 {
            // Not enough room for the pill and its content. Leaving it
            // undrawn is honest; drawing "C…" is not a confirmation.
            return;
        }
        let shown: String = padded.chars().take(want as usize).collect();
        let slot = Rect {
            x: area.right().saturating_sub(want),
            y: area.bottom().saturating_sub(1),
            width: want,
            height: 1,
        };
        Paragraph::new(Line::from(Span::styled(shown, self.style()))).render(slot, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Modifier;

    fn draw(text: &str, tone: Tone, w: u16, h: u16) -> ratatui::buffer::Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            f.render_widget(ToastOverlay::new(text, tone), f.area());
        })
        .unwrap();
        term.backend().buffer().clone()
    }

    fn paint(text: &str, tone: Tone, w: u16, h: u16) -> Vec<String> {
        let buf = draw(text, tone, w, h);
        buf.content
            .chunks(w as usize)
            .map(|row| {
                row.iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn modifiers_last_row(text: &str, tone: Tone, w: u16, h: u16) -> Vec<Modifier> {
        let buf = draw(text, tone, w, h);
        let row = buf.content.chunks(w as usize).last().unwrap();
        row.iter().map(|c| c.modifier).collect()
    }

    /// The pill's leftmost cell: it is right-aligned, so it starts where the
    /// text's own width puts it, and everything left of that is untouched.
    fn pill_x(text: &str, w: u16) -> usize {
        (w as usize).saturating_sub(text.chars().count() + 2)
    }

    /// The pill is right-aligned on the band's bottom row, and only the pill's
    /// cells are touched: everything else in the buffer still holds what the
    /// transcript put there.
    #[test]
    fn the_pill_sits_in_the_bottom_right_corner() {
        let rows = paint("Copied 12 characters", Tone::Good, 40, 4);
        // 20 characters of text + one space of padding each side = 22 cells,
        // pushed against the right edge of a 40-column band.
        assert_eq!(rows[3], format!("{:>39}", "Copied 12 characters"));
        for r in &rows[..3] {
            assert_eq!(r.trim(), "", "nothing above the bottom row is drawn");
        }
    }

    #[test]
    fn a_wide_toast_is_clipped_to_the_band_and_never_wraps() {
        let rows = paint("Copied 1,284 characters · clipboard", Tone::Good, 20, 3);
        // Two rows used, not two rows *of toast*: the second row is untouched.
        assert_eq!(rows.iter().filter(|r| !r.trim().is_empty()).count(), 1);
        assert_eq!(rows[2].chars().count(), 20);
        assert!(
            rows[2].starts_with(" Copied 1,284 chara"),
            "clipped, not wrapped: {rows:?}"
        );
    }

    #[test]
    fn too_little_room_draws_nothing_rather_than_a_truncated_lie() {
        // 2 columns cannot hold " Co " and a message.
        let rows = paint("Copy failed: nothing was copied", Tone::Bad, 2, 2);
        assert!(rows.iter().all(|r| r.trim().is_empty()), "{rows:?}");
    }

    /// Reverse video on the pill's cells: legible on a light terminal and on a
    /// dark one, over prose and over a syntax-highlighted code block, because it
    /// is a swap and not a colour.
    #[test]
    fn the_pill_is_reversed_and_the_failure_shouts() {
        let text = "Copied 3 characters";
        let x = pill_x(text, 40);
        let good = modifiers_last_row(text, Tone::Good, 40, 2);
        assert!(
            good[x].contains(Modifier::REVERSED),
            "the pill's own leftmost cell is reversed"
        );
        assert!(
            !good[x - 1].contains(Modifier::REVERSED),
            "one cell left of the pill is untouched"
        );
        assert!(
            !good[0].contains(Modifier::REVERSED),
            "so is the far left of the band"
        );

        let bad_text = "Copy failed: nope";
        let bx = pill_x(bad_text, 40);
        let bad = modifiers_last_row(bad_text, Tone::Bad, 40, 2);
        assert!(bad[bx].contains(Modifier::REVERSED));
        assert!(bad[bx + 1].contains(Modifier::BOLD));
        assert!(!bad[bx - 1].contains(Modifier::BOLD));
    }
}
