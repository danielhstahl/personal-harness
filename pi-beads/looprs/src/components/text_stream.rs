use crate::state::scrollback::DisplayRow;
use crate::utils::render::FRAMES;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};

/// The frame's transcript band: the window the scroll store opened onto, plus the
/// live tail of what the session is saying now, pinned to the **bottom** of the
/// space the band was given.
///
/// Bottom-pinned because that is what the band replaced. Until the full-screen
/// frame (looprs-pdl.4) the settled lines were printed into the terminal's own
/// scrollback and the live tail sat in a pane directly below them, so the newest
/// line was always the row right above the chrome. Anything else — top-aligning
/// the transcript, say — puts a gap between the newest thing the session said
/// and the row that says what the session is doing, and reads as if the session
/// had stopped.
///
/// It now takes *store rows* rather than lines because the store is what decides
/// which rows are on the screen: `Scrollback::window(visible)` answers with the
/// slice the offset says, and this widget draws that slice and nothing else. The
/// rows' provenance and cell map are not read here — they are read by the
/// selection path (looprs-pdl.9) — but passing the row rather than its `Line`
/// keeps the band from having a second, shallower copy of the transcript to keep
/// track of.
///
/// Scrolling itself is not here. This widget lays out what it is handed and never
/// moves it: the offset, the pin and the re-wrap are
/// [`crate::state::scrollback::Scrollback`]'s, and what this widget owns is one
/// thing only — the newest row of what it was given is on the bottom row.
pub struct TranscriptBand<'a> {
    settled: &'a [DisplayRow],
    live: &'a [Line<'static>],
    spinner: usize,
    streaming: bool,
}

impl<'a> TranscriptBand<'a> {
    pub fn new(
        settled: &'a [DisplayRow],
        live: &'a [Line<'static>],
        spinner: usize,
        streaming: bool,
    ) -> Self {
        Self {
            settled,
            live,
            spinner,
            streaming,
        }
    }
}

/// Where the transcript band's rows land on the screen.
///
/// Split out of [`TranscriptBand::render`] for one reason: the drag hit-test
/// (looprs-pdl.9) has to know where those rows are, and if it worked that out
/// for itself it would be a **second opinion about the layout** — the exact
/// failure this codebase keeps naming and deleting. A band that grew a row, or
/// started one line later, would then paint one thing and select another, in a
/// way that only shows up when the live tail is a certain height.
///
/// So the band computes this and draws from it, and the frame publishes it to
/// the App (`App::record_band`), which is the only other reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandLayout {
    /// How many of the rows handed in were **not** drawn, off the front — so
    /// `rows[skip]` is the first row that made it to the screen.
    pub skip: usize,
    /// Settled rows drawn, and the screen row the first of them is on.
    pub settled: usize,
    pub settled_y: u16,
    /// Live-tail rows drawn, and the screen row the first of them is on.
    /// `live_y` is one past the last settled row whether or not either exists.
    pub live: usize,
    pub live_y: u16,
}

/// The band's row layout: who gets what, bottom-up, when the content is taller
/// than the space.
///
/// The live tail wins what it needs first — it is the newest thing there is —
/// and the settled rows take the rest from their end, which is the newest end
/// of those. What is left over above is padding, and that is what pins the
/// content down.
pub fn band_layout(area: Rect, settled_avail: usize, live_avail: usize) -> BandLayout {
    if area.is_empty() {
        return BandLayout {
            skip: settled_avail,
            settled: 0,
            settled_y: area.top(),
            live: 0,
            live_y: area.top(),
        };
    }
    let rows = area.height as usize;
    let live = live_avail.min(rows);
    let room = rows - live;
    let settled = settled_avail.min(room);
    // The bottom edge is the anchor: the block starts wherever it has to for its
    // last row to land there.
    let start = area.top() as usize + rows - (settled + live);
    BandLayout {
        skip: settled_avail - settled,
        settled,
        settled_y: start as u16,
        live,
        live_y: (start + settled) as u16,
    }
}

impl Widget for TranscriptBand<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let lay = band_layout(area, self.settled.len(), self.live.len());
        let settled_slice = &self.settled[lay.skip..];
        let live_slice = &self.live[self.live.len() - lay.live..];

        let mut y = lay.settled_y as usize;
        for row in settled_slice {
            (&row.line).render(one_row(area, y), buf);
            y += 1;
        }
        for line in live_slice {
            line.render(one_row(area, y), buf);
            y += 1;
        }
        // A streaming session with nothing to show yet still shows that it is
        // streaming: an empty band over a working child is the "is it hung?"
        // question, and the spinner is the answer. "Nothing to show" means
        // nothing but blanks, which is the same test the old band made — settled
        // rows that are only separators say as little as no rows at all.
        let said_anything = settled_slice
            .iter()
            .any(|r| !r.to_string().trim().is_empty())
            || live_slice.iter().any(|l| !l.to_string().trim().is_empty());
        if self.streaming && !said_anything {
            let frame = FRAMES[self.spinner % FRAMES.len()];
            Line::styled(frame, Style::new().cyan()).render(
                one_row(area, area.top() as usize + area.height as usize - 1),
                buf,
            );
        }
    }
}

/// The single buffer row at `y`, full width of the band.
fn one_row(area: Rect, y: usize) -> Rect {
    Rect {
        x: area.x,
        y: y as u16,
        width: area.width,
        height: 1,
    }
}

/// The "N new" affordance: a pill on the band's bottom row, right-aligned,
/// saying that the tail has moved and what single action gets you back to it.
///
/// An **overlay**, not a row. Reserving a row for it would re-shape the band on
/// every arrival — the text the user stopped to read would jump by a row each
/// time the count ticked, which is the same failure ADR-0004 R21 prices for the
/// copy toast ("a toast that adds a row is a reshape"). Covering a few cells in
/// the corner for as long as the count stands is the cheaper lie, and the pill
/// says the number so the covered cells are accounted for.
pub struct NewRowsPill(pub usize);

impl Widget for NewRowsPill {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if self.0 == 0 || area.is_empty() {
            return;
        }
        // Widest possible text is a last resort: clip rather than wrap, because a
        // wrapped pill is a pill that pushed the transcript around.
        let text = format!("\u{25b2} {} new \u{00b7} End for the tail", self.0);
        let w = (text.chars().count() as u16 + 2).min(area.width);
        if w < 3 {
            return;
        }
        let slot = Rect {
            x: area.right().saturating_sub(w),
            y: area.bottom().saturating_sub(1),
            width: w,
            height: 1,
        };
        Paragraph::new(Line::from(Span::styled(
            format!(" {text} ")
                .chars()
                .take(w as usize)
                .collect::<String>(),
            Style::new().black().bg(ratatui::style::Color::Yellow),
        )))
        .render(slot, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollback::RenderedRow;
    use crate::state::scrollback::{RowEnd, rows_from_rendered};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// Store rows out of plain text, through the store's own builder, so the band
    /// is tested against the type it really draws.
    fn rows(texts: &[&str]) -> Vec<DisplayRow> {
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

    fn lines(texts: &[&str]) -> Vec<Line<'static>> {
        texts.iter().map(|t| Line::from(t.to_string())).collect()
    }

    fn screen(
        settled: &[DisplayRow],
        live: &[Line<'static>],
        streaming: bool,
        h: u16,
    ) -> Vec<String> {
        let backend = TestBackend::new(20, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(TranscriptBand::new(settled, live, 0, streaming), f.area());
        })
        .unwrap();
        let w = term.backend().buffer().area.width as usize;
        term.backend()
            .buffer()
            .content
            .chunks(w)
            .map(|row| {
                row.iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn paint(settled: &[&str], live: &[&str], h: u16) -> Vec<String> {
        screen(&rows(settled), &lines(live), !live.is_empty(), h)
    }

    /// The whole point of the band: the newest line is on the bottom row, with
    /// the blank space above it rather than below.
    #[test]
    fn the_newest_line_is_on_the_bottom_row() {
        let rows = paint(&["one", "two"], &["live"], 5);
        assert_eq!(rows[4], "live");
        assert_eq!(rows[3], "two");
        assert_eq!(rows[2], "one");
        assert!(rows[0].is_empty() && rows[1].is_empty());
    }

    /// When the content is longer than the band, the *oldest* lines are the ones
    /// that fall off — never the newest.
    #[test]
    fn what_does_not_fit_falls_off_the_front_not_the_back() {
        let texts = ["s0", "s1", "s2", "s3", "s4", "s5", "s6", "s7", "s8", "s9"];
        let rows = paint(&texts, &["live"], 3);
        assert_eq!(rows[2], "live");
        assert_eq!(rows[1], "s9");
        assert_eq!(rows[0], "s8");
    }

    /// The live tail is not allowed to push the settled lines off the band: it
    /// takes only what it needs, and the settled lines keep the rest.
    #[test]
    fn a_long_live_tail_gives_the_band_back_what_it_cannot_use() {
        let rows = paint(&["s1", "s2"], &["a", "b", "c"], 3);
        assert_eq!(rows, vec!["a", "b", "c"]);
    }

    /// Streaming with nothing yet: the spinner is on the bottom row, because that
    /// is where the newest thing would be.
    #[test]
    fn a_streaming_session_with_no_text_shows_the_spinner_at_the_bottom() {
        let rows = screen(&[], &[], true, 3);
        let bottom = &rows[2];
        assert!(
            FRAMES.contains(&bottom.as_str()),
            "no spinner on the bottom row: {rows:?}"
        );
        assert!(rows[0].is_empty() && rows[1].is_empty());
    }

    /// …and blanks are the same as nothing: a transcript of separators over a
    /// working child still says "working".
    #[test]
    fn blanks_over_a_working_child_still_show_the_spinner() {
        let rows = screen(&rows(&["", ""]), &[], true, 3);
        assert!(
            FRAMES.contains(&rows[2].as_str()),
            "blank content hid the spinner: {rows:?}"
        );
    }

    /// Not streaming and nothing to show is an empty band, not a spinner: the
    /// spinner means "working", and a session at rest does not.
    #[test]
    fn a_session_at_rest_shows_no_spinner() {
        let rows = screen(&[], &[], false, 3);
        assert!(rows.iter().all(|r| r.is_empty()), "{rows:?}");
    }

    /// A zero-height band renders nothing and does not panic: the frame can hand
    /// one over when a window is being dragged to nothing.
    #[test]
    fn an_empty_band_renders_nothing() {
        let backend = TestBackend::new(10, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(
                TranscriptBand::new(&rows(&["x"]), &[], 0, false),
                Rect::ZERO,
            );
        })
        .unwrap();
    }
}
