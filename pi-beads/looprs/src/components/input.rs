use crossterm::event::{KeyCode, KeyEvent};
use ratatui::layout::Rect;

use ratatui::Frame;
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph};
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::theme::styles::mode_color;
use crate::viewport::INPUT_BORDER_ROWS;

/// The terminal states are a session-layer identity (ADR-0002), not a property of
/// this widget. The input box only cycles and labels them, so the type is imported
/// and re-exported here rather than owned here.
pub use crate::session::TerminalType;

pub enum InputAction {
    Submit {
        text: String,
        mode: TerminalType,
    },
    /// Tab. The input box has already moved to `to` when this is returned; the
    /// Router is told `from` so it can apply that mode's switch-away policy
    /// (ADR-0002 Q3) instead of having to remember what it was last showing.
    SwitchMode {
        from: TerminalType,
        to: TerminalType,
    },
    Cancel,
}

pub struct InputState {
    text: String,
    pub mode: TerminalType,
}

impl InputState {
    pub fn new() -> Self {
        Self {
            text: "".to_string(),
            mode: TerminalType::Beeds,
        }
    }

    /// What is in the box right now.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// How the current text lays out at `inner_width` cells: the box's own text
    /// rows, top to bottom, as the widget will draw them.
    ///
    /// This is public because the *height policy* has to count these rows before
    /// the box is ever drawn — the number that sizes the live region and the lines
    /// that fill it must come from one wrapping, or the caret lands off-screen and
    /// the box grows a row nobody can explain. Same reason `SessionView::preview`
    /// is the only way to the live text.
    pub fn display_lines(&self, inner_width: usize) -> Vec<String> {
        wrap_display(&self.text, inner_width)
    }

    /// Put text back in the box, from the app side rather than the keyboard —
    /// Pi's `Esc` handing back the messages it had queued.
    ///
    /// Newlines are folded to single spaces. The box now *displays* as several
    /// wrapped rows, but it still has no cursor keys and Enter still submits the
    /// whole thing, so an embedded newline would be a line break the user cannot
    /// see, edit or delete — and `Submit` is a single message on the wire. The
    /// message boundaries are not lost: they are already in the transcript.
    pub fn set_text(&mut self, text: String) {
        self.text = text.replace('\n', " ").trim_start().to_string();
    }
    pub fn handle_key(&mut self, k: KeyEvent) -> Option<InputAction> {
        match k.code {
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.text);
                (!text.trim().is_empty()).then_some(InputAction::Submit {
                    text,
                    mode: self.mode,
                })
            }
            KeyCode::Tab => {
                let from = self.mode;
                self.mode = self.mode.next();
                Some(InputAction::SwitchMode {
                    from,
                    to: self.mode,
                })
            }
            KeyCode::Esc => Some(InputAction::Cancel),
            KeyCode::Backspace => {
                self.text.pop();
                None
            }
            KeyCode::Char(c) => {
                self.text.push(c);
                None
            }
            _ => None,
        }
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(self.mode.label())
            .border_style(mode_color(self.mode));

        let inner_w = (area.width as usize)
            .saturating_sub(INPUT_BORDER_ROWS as usize)
            .max(1);
        let inner_h = (area.height as usize)
            .saturating_sub(INPUT_BORDER_ROWS as usize)
            .max(1);

        let mut lines = self.display_lines(inner_w);
        // A line that fills the width exactly leaves the caret on the *next* row:
        // the terminal's own auto-wrap would otherwise park it on the border
        // column. Borrow that row only when there is one to spare — in a one-row
        // box the phantom would be the only thing on screen, and an empty box you
        // have just typed into is worse than a caret sitting on the last cell of
        // the text you can still see.
        let last_fills = lines.last().map(|l| l.width()).unwrap_or(0) >= inner_w;
        if last_fills && inner_h >= 2 {
            lines.push(String::new());
        }

        // The caret is always at the end of the text — this box has no cursor keys
        // — so "keep the caret visible" and "show the tail" are the same rule.
        // Showing the *head* instead is how a long message puts the caret off the
        // bottom of the box and the user types blind, which is the bug here.
        let shown = &lines[lines.len().saturating_sub(inner_h)..];
        let caret_col = shown
            .last()
            .map(|l| l.width())
            .unwrap_or(0)
            // Belt and braces: a double-width glyph in a one-column box would put
            // the caret past the last inner column.
            .min(inner_w.saturating_sub(1));
        let caret_row = shown.len().saturating_sub(1);

        let text: Vec<Line<'static>> = shown.iter().map(|s| Line::from(s.clone())).collect();
        f.render_widget(Paragraph::new(text).block(block), area);
        f.set_cursor_position((
            area.x + 1 + caret_col as u16,
            area.y + 1 + caret_row.min(inner_h - 1) as u16,
        ));
    }
}

/// Greedy word wrap for the input box: never loses a character, never overflows
/// `width`, and never leaves the caret past the border.
///
/// Two rules:
///
/// * break on the last space that fits, so words stay whole where they can, the
///   way the transcript wraps the same text after it is submitted;
/// * when a run is longer than `width`, break it **at** the width. A box that can
///   only break on spaces cannot wrap a long path, a URL or a column of
///   `aaaaaaaa…`, and the caret walks off the right edge into the border — which
///   is exactly the "I can't see what I'm typing" this exists to fix.
///
/// Spaces are kept on the line they were typed on rather than eaten, so
/// `lines.concat() == src`: the box shows what was typed, not a tidier version of
/// it. The returned vec is never empty — an empty box still needs the row its
/// caret sits on.
pub fn wrap_display(src: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    // Where the line could be broken instead of cut: the byte index of the most
    // recent space, and the cell width of the line up to (not including) it.
    let mut break_at: Option<(usize, usize)> = None;

    for ch in src.chars() {
        let cw = ch.width().unwrap_or(0);
        if cur_w + cw > width {
            // Prefer the space, but only if it actually helps: breaking early and
            // still overflowing buys a shorter line and the same clipped caret.
            let word_break = match break_at {
                Some((sp, w_up_to)) if w_up_to + cw <= width => Some(sp),
                _ => None,
            };
            if let Some(sp) = word_break {
                let rest = cur[(sp + 1)..].to_string();
                lines.push(cur[..=sp].to_string());
                cur = rest;
                cur_w = cur.width();
            } else {
                lines.push(std::mem::take(&mut cur));
                cur_w = 0;
            }
            break_at = None;
        }
        cur.push(ch);
        cur_w += cw;
        if ch == ' ' {
            break_at = Some((cur.len() - 1, cur_w - cw));
        }
    }
    lines.push(cur);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// Tab is no longer invisible to the rest of the app: it produces a command,
    /// so a mode switch can tear down / bring up backends (looprs-05j).
    #[test]
    fn tab_reports_the_switch_it_made() {
        let mut s = InputState::new();
        assert_eq!(s.mode, TerminalType::Beeds);
        let Some(InputAction::SwitchMode { from, to }) = s.handle_key(key(KeyCode::Tab)) else {
            panic!("Tab must produce SwitchMode");
        };
        assert_eq!(from, TerminalType::Beeds);
        assert_eq!(to, TerminalType::Pi);
        assert_eq!(s.mode, TerminalType::Pi, "the box moved too");
    }

    /// A Submit carries the mode it was typed into: intent, from the keyboard.
    #[test]
    fn submit_carries_the_mode_it_was_typed_in() {
        let mut s = InputState::new();
        s.handle_key(key(KeyCode::Char('h')));
        s.handle_key(key(KeyCode::Char('i')));
        let Some(InputAction::Submit { text, mode }) = s.handle_key(key(KeyCode::Enter)) else {
            panic!("Enter must produce Submit");
        };
        assert_eq!(text, "hi");
        assert_eq!(mode, TerminalType::Beeds);
    }

    // ------------------------------------------------------------- wrapping

    /// Every character survives, and no line is wider than the box asked for. Both
    /// halves matter: dropping a space is a silent lie, and a line one cell too
    /// wide pushes the caret onto the border.
    #[test]
    fn the_wrap_loses_nothing_and_overflows_nothing() {
        let texts: Vec<String> = vec![
            String::new(),
            "hi".into(),
            "a full sentence with plenty of words in it to wrap".into(),
            "x".repeat(57),
            "one  two   three    four".into(),
            "ünïcödé wörds with ✨ wide glyphs ✨ too".into(),
            "/a/very/long/path/with/no/spaces/at/all/what/a/box/can/do".into(),
            "a".repeat(200),
        ];
        for text in &texts {
            // Widths from 2 up: a single glyph wider than the box cannot fit in it
            // at all, and that case gets its own test below rather than being
            // papered over here.
            for w in 2usize..=40 {
                let lines = wrap_display(text, w);
                assert_eq!(
                    lines.concat(),
                    *text,
                    "width {w}: the wrap lost or reordered characters"
                );
                for l in &lines {
                    assert!(
                        l.width() <= w,
                        "width {w}: line {:?} is {} cells wide",
                        l,
                        l.width()
                    );
                }
            }
        }
    }

    /// A run with no spaces in it is broken *at* the width. This is the case a
    /// space-only wrapper cannot do, and the one that leaves a caret off-screen.
    #[test]
    fn a_long_unbreakable_word_is_cut_at_the_width() {
        let lines = wrap_display(&"x".repeat(50), 20);
        assert_eq!(lines.len(), 3, "50 cells at 20 per row");
        assert_eq!(lines[0].width(), 20);
        assert_eq!(lines[2].width(), 10);
    }

    /// Words are not broken when they could have gone on the next line instead.
    #[test]
    fn words_break_at_spaces_rather_than_in_the_middle() {
        assert_eq!(
            wrap_display("alpha beta gamma", 20),
            vec!["alpha beta gamma"]
        );
        assert_eq!(
            wrap_display("alpha beta gamma", 12),
            vec!["alpha beta ", "gamma"]
        );
    }

    /// Wide characters are counted as the two cells they occupy, not as one each.
    #[test]
    fn wide_characters_wrap_by_display_width_not_character_count() {
        // 6 cells per row, each glyph 2 wide → three per row, not six.
        let lines = wrap_display("你好你好你好", 6);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].width(), 6);
    }

    /// A one-cell box cannot hold a two-cell glyph. Nothing is dropped and nothing
    /// panics, and the caret — the thing that must stay reachable — is clamped to
    /// the box's own inner column instead of walking onto the border.
    #[test]
    fn a_glyph_wider_than_the_box_does_not_break_it() {
        let text = "✨";
        let lines = wrap_display(text, 1);
        assert_eq!(lines.concat(), text, "the glyph was dropped");
        let (_, caret) = paint(text, 3, 3); // inner width 1
        assert_eq!(
            caret.x, 1,
            "the caret left the box's inner column: {caret:?}"
        );
        assert_eq!(caret.y, 1);
    }

    // -------------------------------------------------------------- caret

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::layout::Position;

    /// Paint the box over a whole `w x h` terminal and read the screen and the caret
    /// back. The caret is the thing under test: "I can't see what I'm typing" is a
    /// statement about where the caret went, not about the text.
    fn paint(text: &str, w: u16, h: u16) -> (Vec<String>, Position) {
        let mut s = InputState::new();
        s.text = text.into();
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| {
            let area = f.area();
            s.render(f, area);
        })
        .unwrap();
        let caret = term.get_cursor_position().unwrap();
        (rows(term.backend()), caret)
    }

    /// The backend's screen as text rows, wide-character-safe.
    fn rows(b: &TestBackend) -> Vec<String> {
        let w = b.buffer().area.width.max(1) as usize;
        b.buffer()
            .content
            .chunks(w)
            .map(|row| {
                let mut s = String::new();
                let mut skip = 0usize;
                for c in row {
                    if skip > 0 {
                        skip -= 1;
                        continue;
                    }
                    s.push_str(c.symbol());
                    skip = c.cell_width().saturating_sub(1) as usize;
                }
                s
            })
            .collect()
    }

    /// The height the box would be given for `text` at `w` columns, borders included.
    fn box_h(text: &str, w: u16) -> u16 {
        let inner = (w as usize).saturating_sub(2).max(1);
        let mut s = InputState::new();
        s.text = text.into();
        crate::viewport::input_rows(s.display_lines(inner).len() as u16)
    }

    /// A short line still fits the box it always fitted.
    #[test]
    fn a_short_line_sits_on_its_own_row_with_the_caret_behind_it() {
        let (screen, caret) = paint("hi", 20, 3);
        assert!(screen[1].contains("hi"), "{screen:?}");
        assert_eq!(caret, Position::new(3, 1), "just past the 'i'");
    }

    /// The reported bug: a long message had nowhere to go, so the caret walked off
    /// the right edge and the user typed blind. With the box grown to the height
    /// the policy gives it, the caret is inside the borders at every length.
    #[test]
    fn the_caret_stays_inside_a_grown_box_at_every_length() {
        for n in [1usize, 39, 40, 41, 79, 80, 81, 199, 200, 401] {
            let text = "x".repeat(n);
            let h = box_h(&text, 40);
            let (_, caret) = paint(&text, 40, h);
            assert!(
                caret.x >= 1 && caret.x <= 40 - 2,
                "n={n} h={h}: caret column {} is outside the box's inner columns 1..={}",
                caret.x,
                40 - 2
            );
            assert!(
                caret.y >= 1 && caret.y <= h - 2,
                "n={n} h={h}: caret row {} is outside the box's inner rows 1..={}",
                caret.y,
                h - 2
            );
        }
    }

    /// When the text needs more rows than the box is allowed, the box shows the
    /// rows around the caret rather than the rows it started with — otherwise the
    /// tail of a long message is invisible and the box shows text the user has
    /// already typed past.
    #[test]
    fn the_box_shows_the_end_of_the_text_not_the_beginning() {
        let text = format!("{} tail-marker", "word ".repeat(60));
        let h = box_h(&text, 40); // capped at MAX_INPUT_ROWS
        let (screen, caret) = paint(&text, 40, h);
        let body: String = screen
            .iter()
            .skip(1)
            .take(h as usize - 2)
            .map(|r| r.trim_matches(|c| c == '│' || c == ' ').to_string())
            .collect();
        assert!(
            body.contains("tail-marker"),
            "the end of the message is not on screen: {screen:?}"
        );
        assert!(
            caret.y <= h - 2,
            "caret row {} is not on the last visible text row ({}..{})",
            caret.y,
            1,
            h - 2
        );
    }

    /// A line that ends exactly on the inner edge cannot hold the caret there — the
    /// next cell is the border — so the caret goes to a row of its own rather than
    /// sitting on the frame.
    #[test]
    fn a_caret_at_the_exact_inner_edge_gets_its_own_row() {
        let text = "x".repeat(10); // inner width is exactly 10 at w=12
        let (screen, caret) = paint(&text, 12, 4);
        assert!(screen[1].contains(&text), "{screen:?}");
        assert_eq!(caret.x, 1, "wrapped to a fresh row, column 1");
        assert_eq!(caret.y, 2, "one row below the text it follows");
    }

    /// In a one-text-row box there is no spare row to borrow, so the caret sits on
    /// the last inner column *over* the text rather than emptying the box out.
    #[test]
    fn a_one_row_box_never_empties_itself_to_place_the_caret() {
        let text = "x".repeat(10); // exactly fills the 10-cell inner width
        let (screen, caret) = paint(&text, 12, 3); // borders + one text row
        assert!(
            screen[1].contains(&text),
            "the typed text vanished from the box: {screen:?}"
        );
        assert!(caret.x >= 1 && caret.x <= 10, "caret at {caret:?}");
        assert_eq!(caret.y, 1);
    }

    /// The wrap is total: an empty box still has the row its caret sits on.
    #[test]
    fn an_empty_box_still_has_a_row_for_the_caret() {
        assert_eq!(wrap_display("", 20), vec![String::new()]);
        let (_, caret) = paint("", 20, 3);
        assert_eq!(caret, Position::new(1, 1));
    }
}
