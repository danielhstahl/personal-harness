//! Drawing the selection (looprs-pdl.9).
//!
//! The model is in [`crate::state::selection`]; this is the one widget that
//! turns its cell runs into pixels, and the whole contract is one sentence:
//! **restyle cells that are already on the screen, and change nothing else.**
//!
//! # Why an overlay and not a re-render
//!
//! A selection that went through the row renderer would be a re-layout, and a
//! re-layout is a re-flow: the ticket forbids it outright ("a selection that
//! reflows text is worse than no selection"), and the epic's own measurement
//! says why in ADR-0004 R21 — reshaping a band leaves a hole between the erase
//! and the bytes that replace it, measured at 8.8–9.3 ms. A drag produces one
//! of those per motion event, a hundred times a second, over the text the user
//! is trying to read.
//!
//! So the highlight never touches a `Line`, never re-wraps, never re-styles a
//! span. It walks the buffer cells the range covers and flips a modifier. The
//! row's text is where it was, its width is where it was, and the only thing
//! that changes is which cells are bright.
//!
//! # Why reverse video and not a colour
//!
//! The ticket asks for a highlight that survives *every theme* and *every entry
//! kind*, and is legible on both a light and a dark terminal. A fixed pair of
//! colours is legible in one of those and invisible in the other, and the
//! entry-kind half is worse still: a code block arrives with syntax-highlighted
//! spans whose palette nobody chose, and any colour we pick will collide with
//! some token in it.
//!
//! Reverse video cannot fail that test, because it is not a colour — it is a
//! swap of whatever is already there. It is legible on a light background for
//! the same reason it is legible on a dark one, and on a syntax-highlighted
//! token it reads as exactly what it is: that token, inverted. This is also
//! what terminals have used for selection since the selection *was* a video
//! mode, which means it is the one thing every emulator in the matrix renders
//! sanely without us having to measure it per theme.
//!
//! # Why toggle rather than set
//!
//! `REVERSED` is its own inverse. Painting it onto a cell that already carries
//! it — an entry that rendered reversed, or a cell another overlay left that
//! modifier on — would leave the cell looking *untouched*, which is a
//! selection that is invisible exactly where it lands. Toggling means the
//! highlight always changes the cell's appearance: reversed text goes normal,
//! normal text goes reversed, and "is this cell selected" is always visible.
//!
//! The consequence is honest and worth saying: two selections over the same
//! reversed cell would cancel. That cannot happen here — [`Selection`] holds
//! one range, and the widget is driven from it — but if a later ticket paints
//! the highlight twice it will show up as a selection with a hole in it,
//! which is a visible bug rather than an invisible one.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::widgets::Widget;

use crate::components::text_stream::BandLayout;

/// The selection band.
///
/// `runs` is what [`Selection::cells`] produced against the *window the frame
/// is drawing*: `(index into that window, first cell, one past last cell)`.
/// Together with the [`BandLayout`] that is everything needed to turn a
/// content range into screen cells — and note that both halves came from the
/// frame's own current values, which is what keeps the box on the text.
///
/// [`Selection::cells`]: crate::state::selection::Selection::cells
pub struct SelectionHighlight<'a> {
    runs: &'a [(usize, u16, u16)],
    layout: BandLayout,
}

impl<'a> SelectionHighlight<'a> {
    pub fn new(runs: &'a [(usize, u16, u16)], layout: BandLayout) -> Self {
        Self { runs, layout }
    }
}

impl Widget for SelectionHighlight<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        for (row, from, to) in self.runs {
            if to <= from {
                continue;
            }
            // The row index is into the drawn settled rows, so the layout's
            // `settled_y` is the only translation needed. Anything the layout
            // puts outside the band is dropped rather than drawn over chrome:
            // the highlight cannot reach the status row, the tool wall or the
            // input box, and this is where that is guaranteed.
            let Some(y) = self.layout.settled_y.checked_add(*row as u16) else {
                continue;
            };
            if y >= area.bottom() {
                continue;
            }
            for cell in *from..*to {
                let x = area.x.saturating_add(cell);
                if x >= area.right() {
                    break;
                }
                if let Some(c) = buf.cell_mut((x, y)) {
                    c.modifier.toggle(Modifier::REVERSED);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollback::RenderedRow;
    use crate::components::text_stream::{TranscriptBand, band_layout};
    use crate::state::scrollback::{RowEnd, rows_from_rendered};
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::style::{Color, Style};
    use ratatui::text::Line;
    use ratatui::{Frame, Terminal};

    fn rows(texts: &[&str]) -> Vec<crate::state::scrollback::DisplayRow> {
        rows_from_rendered(
            texts
                .iter()
                .map(|t| RenderedRow {
                    entry: 0,
                    end: RowEnd::Hard,
                    line: Line::from(t.to_string()),
                })
                .collect(),
        )
    }

    /// Paint `lines` with `runs` highlighted, and read the screen back as
    /// `(text, reversed flag per screen cell)`.
    ///
    /// The band is exactly as tall as the content: the band bottom-pins, so a
    /// taller band would park the rows at the bottom and every row index in
    /// these tests would be off by the padding.
    fn paint(lines: &[&str], runs: &[(usize, u16, u16)]) -> (Vec<String>, Vec<Vec<bool>>) {
        let backend = TestBackend::new(20, lines.len().max(1) as u16);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f: &mut Frame| {
            let area = f.area();
            let settled = rows(lines);
            f.render_widget(TranscriptBand::new(&settled, &[], 0, false), area);
            let lay = band_layout(area, settled.len(), 0);
            f.render_widget(SelectionHighlight::new(runs, lay), area);
        });
        let w = term.backend().buffer().area.width as usize;
        let mut texts = Vec::new();
        let mut rev = Vec::new();
        for row in term.backend().buffer().content.chunks(w) {
            let mut t = String::new();
            let mut r = Vec::new();
            let mut skip = 0usize;
            for c in row {
                // One flag per screen cell, as the buffer holds it. What the
                // *terminal* was told can differ from this for the trailing
                // column of a wide glyph — see the CJK test below.
                r.push(c.modifier.contains(Modifier::REVERSED));
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                t.push_str(c.symbol());
                skip = c.cell_width().saturating_sub(1) as usize;
            }
            texts.push(t.trim_end().to_string());
            rev.push(r);
        }
        (texts, rev)
    }

    /// **No reflow, ever.** The text on the screen is the same with a
    /// selection as without it; only the modifiers differ.
    #[test]
    fn the_highlight_changes_no_cells_but_its_own() {
        let lines = ["one two three", "second row", "", "fourth"];
        let (plain_text, plain_rev) = paint(&lines, &[]);
        let (sel_text, sel_rev) = paint(&lines, &[(0, 4, 9)]);
        assert_eq!(
            plain_text, sel_text,
            "a selection re-flowed or re-painted text"
        );
        assert_eq!(plain_rev.iter().filter(|r| r.iter().any(|x| *x)).count(), 0);
        assert_eq!(
            sel_rev[0][4..9],
            [true, true, true, true, true],
            "the five cells of \"two t\" are reversed"
        );
        assert!(
            !sel_rev[0][0..4].iter().any(|x| *x) && !sel_rev[0][9..].iter().any(|x| *x),
            "and nothing outside the run is"
        );
        for row in [1usize, 2, 3] {
            assert!(
                !sel_rev[row].iter().any(|x| *x),
                "row {row} got dragged into it"
            );
        }
    }

    /// **Chrome is never highlighted**: a run the layout puts past the band's
    /// bottom edge is dropped, not painted onto whatever is under it.
    #[test]
    fn a_run_outside_the_band_is_dropped_not_drawn_on_chrome() {
        let backend = TestBackend::new(12, 3);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            // A band one row tall, with runs for that row *and* rows below it
            // where the status row / input box would live.
            let band = Rect::new(0, 0, 12, 1);
            let settled = rows(&["in the band"]);
            f.render_widget(TranscriptBand::new(&settled, &[], 0, false), band);
            let lay = band_layout(band, settled.len(), 0);
            f.render_widget(
                SelectionHighlight::new(&[(0, 0, 4), (1, 0, 4), (2, 0, 12)], lay),
                // Into the band, exactly as `view` does it. `area` is the whole
                // screen here so the test can see that nothing below the band
                // got painted.
                band,
            );
        });
        let w = term.backend().buffer().area.width as usize;
        let rows: Vec<Vec<bool>> = term
            .backend()
            .buffer()
            .content
            .chunks(w)
            .map(|r| {
                r.iter()
                    .map(|c| c.modifier.contains(Modifier::REVERSED))
                    .collect()
            })
            .collect();
        assert_eq!(rows[0][0..4], [true; 4], "the band's own row highlighted");
        assert!(
            !rows[1].iter().any(|x| *x) && !rows[2].iter().any(|x| *x),
            "rows outside the band were painted"
        );
    }

    /// Toggling, not setting: an already-reversed cell changes appearance when
    /// selected, which is the difference between a visible selection over
    /// reversed text and an invisible one.
    #[test]
    fn a_reversed_cell_changes_when_it_is_selected() {
        let backend = TestBackend::new(8, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            let area = f.area();
            // 'a' arrives already reversed (a themed entry, say) and 'b' plain,
            // so the two halves of the assertion are telling apart two spans
            // and not one span that covers both.
            let line = Line::from(vec![
                ratatui::text::Span::styled(
                    "a".to_string(),
                    Style::default()
                        .fg(Color::White)
                        .bg(Color::Black)
                        .add_modifier(Modifier::REVERSED),
                ),
                ratatui::text::Span::raw("b".to_string()),
            ]);
            let settled = rows_from_rendered(vec![RenderedRow {
                entry: 0,
                end: RowEnd::Hard,
                line,
            }]);
            f.render_widget(TranscriptBand::new(&settled, &[], 0, false), area);
            let lay = band_layout(area, settled.len(), 0);
            f.render_widget(SelectionHighlight::new(&[(0, 0, 2)], lay), area);
        });
        let buf = term.backend().buffer();
        let a = buf
            .cell((0, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED);
        let b = buf
            .cell((1, 0))
            .unwrap()
            .modifier
            .contains(Modifier::REVERSED);
        assert!(!a, "the reversed 'a' toggled OFF, so it changed appearance");
        assert!(b, "the plain 'b' toggled ON");
    }

    /// Wide characters, at the pixels. The run covers both cells of every glyph
    /// it touches, and what lands on the screen is the glyph itself inverted —
    /// which is the only thing there *to* invert: a wide glyph's trailing
    /// column carries no symbol, so ratatui's diff does not write a style
    /// change to it at all. Reading that as a bug would be backwards; the
    /// inversion rides on the glyph, which is the half the user sees.
    ///
    /// The property asserted here is the one the ticket forbids breaking: the
    /// text is unmoved, and no glyph is left half-lit.
    #[test]
    fn a_wide_glyph_is_highlighted_whole_and_reflows_nothing() {
        let (text, rev) = paint(&["日本語"], &[(0, 0, 6)]);
        assert_eq!(text[0], "日本語", "the text is unmoved");
        for glyph in 0..3usize {
            assert!(
                rev[0][glyph * 2],
                "glyph {glyph}'s own cell is not reversed, so it is half-lit"
            );
        }
        // …and the run past the last glyph does not reach into the row's padding
        // with a lit cell where nothing was selected: 3 glyphs = 6 cells, so a
        // 6-cell run ends exactly at the text.
        assert!(
            rev[0][6..].iter().all(|x| !x),
            "the run spilled past the end of the text"
        );
    }

    /// An empty run list is a no-op — the common case, and it must not touch a
    /// single cell.
    #[test]
    fn no_runs_touches_nothing() {
        let (a, ra) = paint(&["nothing selected here"], &[]);
        let (b, rb) = paint(&["nothing selected here"], &[(0, 3, 3), (1, 0, 0)]);
        assert_eq!(a, b);
        assert_eq!(ra, rb);
    }
}
