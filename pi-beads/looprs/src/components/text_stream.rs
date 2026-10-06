use crate::utils::render::FRAMES;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

/// The frame's transcript band: what the session has finished saying, plus the
/// live tail of what it is saying now, pinned to the **bottom** of the space the
/// band was given.
///
/// Bottom-pinned because that is what the band replaced. Until the full-screen
/// frame (looprs-pdl.4) the settled lines were printed into the terminal's own
/// scrollback and the live tail sat in a pane directly below them, so the newest
/// line was always the row right above the chrome. Anything else — top-aligning
/// the transcript, say — puts a gap between the newest thing the session said
/// and the row that says what the session is doing, and reads as if the session
/// had stopped.
///
/// The two halves are passed in separately rather than pre-joined because the
/// frame decides *whether* the live half shows at all: a tool card or a
/// compaction owns the bottom row while it runs, and a session that is not
/// streaming has no live tail. Joining them one level up, from the same two calls
/// that size the band, is what keeps the band showing the same thing that was
/// laid out — the invariant [`App::preview_active`] exists to hold
/// (ADR-0002 Q5).
///
/// The band never scrolls: the tail of the settled lines that does not fit is
/// dropped from the *view*, not from the store, and scrolling the band is
/// looprs-pdl.6's job (offset, pin-to-tail, re-wrap on resize). What this
/// widget owns is one thing only — the newest line is on the bottom row.
pub struct TranscriptBand<'a> {
    settled: &'a [Line<'static>],
    live: &'a [Line<'static>],
    spinner: usize,
    streaming: bool,
}

impl<'a> TranscriptBand<'a> {
    pub fn new(
        settled: &'a [Line<'static>],
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

impl Widget for TranscriptBand<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let rows = area.height as usize;
        // The live tail wins what it needs first — it is the newest thing there
        // is — and the settled lines take the rest from their end, which is the
        // newest end of those.
        let live = tail(self.live, rows);
        let room = rows - live.len();
        let settled = tail(self.settled, room);

        let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows);
        // Pad above rather than below: that is what pins the content down. The
        // three parts are exactly `rows` because `settled` was cut to whatever
        // the live tail left room for.
        lines.extend(std::iter::repeat_n(
            Line::default(),
            rows - live.len() - settled.len(),
        ));
        lines.extend(settled);
        lines.extend(live);
        // A streaming session with nothing to show yet still shows that it is
        // streaming: an empty band over a working child is the "is it hung?"
        // question, and the spinner is the answer.
        let blank = self.streaming
            && lines
                .iter()
                .all(|l| l.spans.iter().all(|s| s.content.trim().is_empty()));
        if blank {
            let frame = FRAMES[self.spinner % FRAMES.len()];
            lines[rows - 1] = Line::styled(frame, Style::new().cyan());
        }
        Paragraph::new(lines).render(area, buf);
    }
}

/// The last `n` lines, cheaply: only what is returned gets cloned.
fn tail(lines: &[Line<'static>], n: usize) -> Vec<Line<'static>> {
    if lines.len() <= n {
        return lines.to_vec();
    }
    lines[lines.len() - n..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn paint(lines: &[Line<'static>], live: &[Line<'static>], h: u16) -> Vec<String> {
        paint_streaming(lines, live, !live.is_empty(), h)
    }

    fn paint_streaming(
        lines: &[Line<'static>],
        live: &[Line<'static>],
        streaming: bool,
        h: u16,
    ) -> Vec<String> {
        let backend = TestBackend::new(20, h);
        let mut term = Terminal::new(backend).unwrap();
        let settled = lines.to_vec();
        term.draw(|f| {
            f.render_widget(TranscriptBand::new(&settled, live, 0, streaming), f.area());
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

    fn l(s: &str) -> Line<'static> {
        Line::from(s.to_string())
    }

    /// The whole point of the band: the newest line is on the bottom row, with
    /// the blank space above it rather than below.
    #[test]
    fn the_newest_line_is_on_the_bottom_row() {
        let rows = paint(&[l("one"), l("two")], &[l("live")], 5);
        assert_eq!(rows[4], "live");
        assert_eq!(rows[3], "two");
        assert_eq!(rows[2], "one");
        assert!(rows[0].is_empty() && rows[1].is_empty());
    }

    /// When the content is longer than the band, the *oldest* lines are the ones
    /// that fall off — never the newest.
    #[test]
    fn what_does_not_fit_falls_off_the_front_not_the_back() {
        let settled: Vec<Line<'static>> = (0..10).map(|i| l(&format!("s{i}"))).collect();
        let rows = paint(&settled, &[l("live")], 3);
        assert_eq!(rows[2], "live");
        assert_eq!(rows[1], "s9");
        assert_eq!(rows[0], "s8");
    }

    /// The live tail is not allowed to push the settled lines off the band: it
    /// takes only what it needs, and the settled lines keep the rest.
    #[test]
    fn a_long_live_tail_gives_the_band_back_what_it_cannot_use() {
        let rows = paint(&[l("s1"), l("s2")], &[l("a"), l("b"), l("c")], 3);
        assert_eq!(rows, vec!["a", "b", "c"]);
    }

    /// Streaming with nothing yet: the spinner is on the bottom row, because that
    /// is where the newest thing would be.
    #[test]
    fn a_streaming_session_with_no_text_shows_the_spinner_at_the_bottom() {
        let rows = paint_streaming(&[], &[], true, 3);
        let bottom = &rows[2];
        assert!(
            FRAMES.contains(&bottom.as_str()),
            "no spinner on the bottom row: {rows:?}"
        );
        assert!(rows[0].is_empty() && rows[1].is_empty());
    }

    /// Not streaming and nothing to show is an empty band, not a spinner: the
    /// spinner means "working", and a session at rest does not.
    #[test]
    fn a_session_at_rest_shows_no_spinner() {
        let backend = TestBackend::new(10, 3);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(TranscriptBand::new(&[], &[], 0, false), f.area());
        })
        .unwrap();
        let rows: Vec<String> = (0..3)
            .map(|y| {
                (0..10)
                    .map(|x| term.backend().buffer()[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert!(rows.iter().all(|r| r.is_empty()), "{rows:?}");
    }

    /// A zero-height band renders nothing and does not panic: the frame can hand
    /// one over when a window is being dragged to nothing.
    #[test]
    fn an_empty_band_renders_nothing() {
        let backend = TestBackend::new(10, 1);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| {
            f.render_widget(TranscriptBand::new(&[l("x")], &[], 0, false), Rect::ZERO);
        })
        .unwrap();
    }
}
