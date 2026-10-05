use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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

/// The box's inner text width, in cells, for a box `outer` columns wide.
///
/// The wrapping is what makes `Up`, `Home` and a caret column mean anything, so
/// the code that handles the keystrokes needs this number exactly as much as the
/// code that sizes and draws the box does. One function owns the border
/// arithmetic; nobody gets to invent a width of their own.
pub fn inner_width(outer: u16) -> usize {
    (outer as usize)
        .saturating_sub(INPUT_BORDER_ROWS as usize)
        .max(1)
}

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
    /// The caret: a byte offset into `text`, always on a `char` boundary.
    ///
    /// A byte offset rather than a (row, column) pair because the rows are a
    /// *derived* view of the text — they depend on the width the box is drawn at.
    /// A stored cell position goes stale on the next character typed and on every
    /// resize; an offset does not. Every row and column the box draws is computed
    /// from this one number at draw time, which is also what lets a resize keep
    /// the caret on the character it was on instead of on the cell it was in.
    cursor: usize,
    /// The cell column the caret is *aiming at* while moving vertically.
    ///
    /// Without it, walking down a wrapped paragraph dumps the caret at column 0
    /// of every row after the first and the user spends the trip typing their way
    /// back right. Any horizontal move clears it, so the next vertical move aims
    /// from where the caret actually is rather than from a stale column.
    want_col: Option<usize>,
    pub mode: TerminalType,
}

impl Default for InputState {
    fn default() -> Self {
        Self::new()
    }
}

impl InputState {
    pub fn new() -> Self {
        Self {
            text: "".to_string(),
            cursor: 0,
            want_col: None,
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
    /// Pi's `Esc` handing back the messages it had queued — with the caret at
    /// the end, ready to keep typing.
    ///
    /// Newlines are *kept*. They used to be folded to spaces on the argument that
    /// an embedded newline was a line break the user could neither see nor edit;
    /// the box now breaks on newlines, draws them and deletes them like anything
    /// else, so the fold would only be a worse copy of what was typed. Leading
    /// whitespace is still trimmed: a restore that opens on a blank row reads as a
    /// box that failed to fill, and nothing of the message goes with it.
    pub fn set_text(&mut self, text: String) {
        self.text = text.trim_start().to_string();
        self.cursor = self.text.len();
        self.want_col = None;
    }

    /// Handle one keystroke, given the width (in cells) the box is drawing into.
    ///
    /// The width is an argument rather than something the box remembers, because
    /// the box does not size itself — the frame does. `Up` is a move across the
    /// *wrapped* rows, not across `\n`s, and there is no honest answer to "what
    /// is above the caret" without knowing how the text breaks at this width.
    pub fn handle_key(&mut self, k: KeyEvent, inner_w: usize) -> Option<InputAction> {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            // Shift-Enter, for terminals that can tell it apart (it arrives as a
            // bare CR in most others, where it is a submit and nothing can be
            // done about that). One more line in the same message.
            KeyCode::Enter if k.modifiers.contains(KeyModifiers::SHIFT) => {
                self.insert("\n");
                None
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.text);
                self.cursor = 0;
                self.want_col = None;
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
            // Shift-Tab is the newline that Shift-Enter mostly cannot be: every
            // terminal has been sending `ESC [ Z` for it since before anyone
            // asked. Newline, not mode switch — the mode switch is Tab's alone.
            KeyCode::BackTab => {
                self.insert("\n");
                None
            }
            KeyCode::Esc => Some(InputAction::Cancel),
            KeyCode::Backspace => {
                self.delete_before();
                None
            }
            KeyCode::Delete => {
                self.delete_after();
                None
            }
            KeyCode::Char(c) => {
                self.insert(c.encode_utf8(&mut [0u8; 4]));
                None
            }
            KeyCode::Left if ctrl => {
                self.move_word(true);
                None
            }
            KeyCode::Right if ctrl => {
                self.move_word(false);
                None
            }
            KeyCode::Left => {
                self.step_left();
                None
            }
            KeyCode::Right => {
                self.step_right();
                None
            }
            KeyCode::Up => {
                self.move_vertical(-1, inner_w);
                None
            }
            KeyCode::Down => {
                self.move_vertical(1, inner_w);
                None
            }
            KeyCode::Home => {
                self.move_to_row_start(inner_w, ctrl);
                None
            }
            KeyCode::End => {
                self.move_to_row_end(inner_w, ctrl);
                None
            }
            _ => None,
        }
    }

    // ------------------------------------------------------------- editing
    //
    // Every mutation goes through one of these. That is not tidiness: the caret
    // has to be a `char` boundary for the byte-offset model to hold, and a
    // `str::insert`/`replace_range` done ad hoc in the match arm above is where a
    // mid-glyph offset would be born — and the next slice would panic.

    /// Insert at the caret.
    fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
        self.want_col = None;
    }

    /// Byte offset of the character before the caret, if there is one.
    fn prev_boundary(&self) -> Option<usize> {
        self.text[..self.cursor]
            .chars()
            .next_back()
            .map(|c| self.cursor - c.len_utf8())
    }

    /// Backspace: the character *before the caret*, not the last character of the
    /// string. Deleting the end of the message from the middle of it is the bug
    /// this whole section exists to prevent.
    fn delete_before(&mut self) {
        if let Some(start) = self.prev_boundary() {
            self.text.replace_range(start..self.cursor, "");
            self.cursor = start;
            self.want_col = None;
        }
    }

    /// Delete (Forward): the character under the caret.
    fn delete_after(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text
                .replace_range(self.cursor..self.cursor + c.len_utf8(), "");
            self.want_col = None;
        }
    }

    fn step_left(&mut self) {
        if let Some(p) = self.prev_boundary() {
            self.cursor = p;
            self.want_col = None;
        }
    }

    fn step_right(&mut self) {
        if let Some(c) = self.text[self.cursor..].chars().next() {
            self.cursor += c.len_utf8();
            self.want_col = None;
        }
    }

    /// Ctrl-Left / Ctrl-Right: words at a time.
    ///
    /// A word is a run of alphanumeric/`_` characters, and the whitespace in
    /// front of it goes with it so a press lands on the *edge* of a word rather
    /// than in the gap behind it. When neither pass finds anything word-shaped — a
    /// wall of punctuation, caret on a `/` — the caret moves one character
    /// instead, because a key that does nothing in the middle of a path reads as
    /// a dropped keystroke.
    fn move_word(&mut self, backwards: bool) {
        let before = self.cursor;
        self.cursor = if backwards {
            let mut cur = self.cursor;
            while let Some(c) = self.text[..cur].chars().next_back() {
                if !c.is_whitespace() {
                    break;
                }
                cur -= c.len_utf8();
            }
            while let Some(c) = self.text[..cur].chars().next_back() {
                if !is_word_char(c) {
                    break;
                }
                cur -= c.len_utf8();
            }
            cur
        } else {
            let end = self.text.len();
            let mut cur = self.cursor;
            while cur < end {
                let c = self.text[cur..].chars().next().expect("cur < end");
                if !c.is_whitespace() {
                    break;
                }
                cur += c.len_utf8();
            }
            while cur < end {
                let c = self.text[cur..].chars().next().expect("cur < end");
                if !is_word_char(c) {
                    break;
                }
                cur += c.len_utf8();
            }
            cur
        };
        if self.cursor == before {
            if backwards {
                self.step_left();
            } else {
                self.step_right();
            }
        }
        self.want_col = None;
    }

    // -------------------------------------------------------- caret movement

    /// Move the caret `delta` display rows (±1) at width `inner_w`.
    ///
    /// Rows, not `\n`s: a wrapped row and a hard-broken row look the same to the
    /// user and must move the same way. The column aimed at is
    /// [`Self::want_col`], so a run of Down presses walks straight down the
    /// column instead of staircase-ing to the left margin.
    fn move_vertical(&mut self, delta: i32, inner_w: usize) {
        let rows = wrap_spans(&self.text, inner_w);
        let (row, col) = caret_cell(&self.text, &rows, self.cursor, inner_w);
        let want = self.want_col.unwrap_or(col);
        let target = row as i32 + delta;
        // Nowhere to go: off the top, or off the bottom — and the phantom row the
        // caret sometimes sits on is not a row of text to move down into.
        if target < 0 || target as usize >= rows.len() {
            return;
        }
        self.cursor = offset_at_col(&self.text, rows[target as usize], want);
        self.want_col = Some(want);
    }

    /// Home: the start of the current display row. Row-local because the rows are
    /// what the user sees — `Home` that jumps past a soft wrap to the top of a
    /// 200-character paragraph is `Home` that did nothing visible from where they
    /// were looking. Ctrl sends it to the start of the whole text instead.
    fn move_to_row_start(&mut self, inner_w: usize, to_text_start: bool) {
        let rows = wrap_spans(&self.text, inner_w);
        let (row, _) = caret_cell(&self.text, &rows, self.cursor, inner_w);
        self.cursor = if to_text_start {
            0
        } else {
            rows[row.min(rows.len() - 1)].0
        };
        self.want_col = None;
    }

    /// End: the end of the current display row — before a `\n` if the row ends
    /// with one, since that newline is not on the row; with Ctrl, the end of the
    /// whole text. Kept idempotent on purpose: a key whose second press means a
    /// different thing is a key that ate the first press.
    fn move_to_row_end(&mut self, inner_w: usize, to_text_end: bool) {
        let rows = wrap_spans(&self.text, inner_w);
        let (row, _) = caret_cell(&self.text, &rows, self.cursor, inner_w);
        self.cursor = if to_text_end {
            self.text.len()
        } else {
            rows[row.min(rows.len() - 1)].1
        };
        self.want_col = None;
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .title(self.mode.label())
            .border_style(mode_color(self.mode));

        let inner_w = inner_width(area.width);
        let inner_h = (area.height as usize)
            .saturating_sub(INPUT_BORDER_ROWS as usize)
            .max(1);

        let shown = show(&self.text, self.cursor, inner_w, inner_h);
        let text: Vec<Line<'static>> = shown.lines.iter().map(|s| Line::from(s.clone())).collect();
        f.render_widget(Paragraph::new(text).block(block), area);
        f.set_cursor_position((
            area.x + 1 + shown.caret_col as u16,
            area.y + 1 + shown.caret_row as u16,
        ));
    }
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// One row of the wrapped box: the byte range of the text it shows.
type Row = (usize, usize);

/// Where the box breaks `src` into display rows at `width` cells.
///
/// Byte ranges into `src`, in order, tiling everything except the `\n` bytes —
/// which are consumed by the break they cause. Nothing is dropped and nothing is
/// reordered, which is the property the caret model leans on: a caret can be a
/// byte offset into `src` and be findable in these rows at any width, rather than
/// a (row, column) pair that goes stale the moment the width changes.
///
/// Two wrapping rules:
///
/// * break on the last space that fits, so words stay whole where they can, the
///   way the transcript wraps the same text after it is submitted;
/// * when a run is longer than `width`, break it **at** the width. A box that can
///   only break on spaces cannot wrap a long path, a URL or a column of
///   `aaaaaaaa…`, and the caret walks off the right edge into the border — which
///   is exactly the "I can't see what I'm typing" this exists to fix.
///
/// A typed `\n` is a hard break, so what the user put on two lines stays on two
/// lines at any width.
///
/// Spaces are kept on the line they were typed on rather than eaten, so
/// `rows.concat() == src` for input with no hard breaks: the box shows what was
/// typed, not a tidier version of it. The returned vec is never empty — an empty
/// box still needs the row its caret sits on.
pub fn wrap_spans(src: &str, width: usize) -> Vec<Row> {
    let width = width.max(1);
    let mut rows: Vec<Row> = Vec::new();
    let mut start = 0usize; // first byte of the row being built
    let mut cur_w = 0usize; // cells used by that row
    // Where the row could be broken instead of cut: the byte index of the most
    // recent space, and the cell width of the row up to (not including) it.
    let mut break_at: Option<(usize, usize)> = None;

    for (i, ch) in src.char_indices() {
        if ch == '\n' {
            // A hard break. The newline itself belongs to no row: it *is* the
            // break. An empty row before it is still a row, because the user put
            // a line there and it has to show.
            rows.push((start, i));
            start = i + 1;
            cur_w = 0;
            break_at = None;
            continue;
        }
        let cw = ch.width().unwrap_or(0);
        if cur_w + cw > width {
            // Prefer the space, but only if it actually helps: breaking early and
            // still overflowing buys a shorter line and the same clipped caret.
            let word_break = match break_at {
                Some((sp, w_up_to)) if w_up_to + cw <= width => Some(sp),
                _ => None,
            };
            if let Some(sp) = word_break {
                rows.push((start, sp + 1));
                start = sp + 1;
                cur_w = src[start..i].width();
            } else if i > start {
                // A row that has nothing in it yet is not a row: the glyph that
                // does not fit gets the one it is starting, rather than being
                // preceded by a blank line nobody typed.
                rows.push((start, i));
                start = i;
                cur_w = 0;
            }
            break_at = None;
        }
        cur_w += cw;
        if ch == ' ' {
            break_at = Some((i, cur_w - cw));
        }
    }
    rows.push((start, src.len()));
    rows
}

/// The same wrap, as the strings the box shows. See [`wrap_spans`].
pub fn wrap_display(src: &str, width: usize) -> Vec<String> {
    wrap_spans(src, width)
        .iter()
        .map(|&(s, e)| src[s..e].to_string())
        .collect()
}

/// The row and cell column a caret byte offset lands on, in the layout `rows`.
///
/// The caret belongs to the first row that still has room for it. At a boundary
/// between two rows — the caret sitting between the last character of one and the
/// first of the next — that is the row *above*, where the character the caret
/// follows is drawn, which is what keeps a caret clamped to the end of a short
/// row on that row instead of dropping it to the next one.
///
/// The exception is a row that filled its width exactly: the cell after its last
/// character is the border, so the caret goes to the head of the row below. That
/// can put the answer at `rows.len()`, a row that does not exist — the phantom row
/// [`show`] has to borrow.
fn caret_cell(text: &str, rows: &[Row], cursor: usize, inner_w: usize) -> (usize, usize) {
    let mut row = rows
        .iter()
        .position(|&(_, end)| cursor <= end)
        .unwrap_or(rows.len() - 1);
    let mut col = text[rows[row].0..cursor].width();
    if col >= inner_w {
        // The cell this caret would take is the border's. It belongs at the head
        // of the next row instead.
        col = 0;
        row += 1;
    }
    (row, col)
}

/// Byte offset of cell column `want` within the row `row`.
///
/// A column can land in the *second* cell of a double-width glyph, where no caret
/// can sit. It stops before that glyph rather than jumping past it: a vertical
/// move should never consume a character the user did not aim at. A `want` past
/// the end of the row clamps to the end of the row, which is what lands the caret
/// on the last thing there is.
fn offset_at_col(text: &str, row: Row, want: usize) -> usize {
    let mut acc = 0usize;
    for (i, ch) in text[row.0..row.1].char_indices() {
        let w = ch.width().unwrap_or(0);
        let at = row.0 + i;
        if acc >= want || acc + w > want {
            return at;
        }
        acc += w;
    }
    row.1
}

/// What the box draws for one state at one size: the rows that fit, and the caret
/// already resolved into them.
pub(crate) struct Shown {
    /// The rows drawn, top to bottom.
    pub lines: Vec<String>,
    /// Index into `lines` of the caret's row.
    pub caret_row: usize,
    /// Cell column of the caret within that row.
    pub caret_col: usize,
}

/// Resolve the caret into the drawn rows, scrolling if it walked out of them.
///
/// Three things happen here and they have to happen in this order, because each
/// one is a way for the caret to end up somewhere the user cannot see:
///
/// 1. **the phantom row** — a line that fills the width exactly leaves the caret
///    no cell inside it: the next cell is the border. Borrow a row only when
///    there is one to spare; in a one-row box the phantom would be the only thing
///    on screen, and an empty box you have just typed into is worse than a caret
///    sitting on the last cell of the text you can still see.
/// 2. **the window** — the box is capped in height (`viewport::MAX_INPUT_ROWS`),
///    so a long message does not fit no matter what. The window is
///    tail-preferred and *minimally* scrolled: show the end of the text, and
///    shift that window back up only as far as the caret requires. That is what
///    makes the caret stay put while typing, and follow the caret when it walks
///    back up the message with `Up` instead of leaving the user editing blind.
/// 3. **the clamps** — the caret is the one thing that must never leave the box's
///    inner area, whatever the text or the width did to get it near an edge.
fn show(text: &str, cursor: usize, inner_w: usize, inner_h: usize) -> Shown {
    let spans = wrap_spans(text, inner_w);
    let (mut caret_row, mut caret_col) = caret_cell(text, &spans, cursor, inner_w);
    let mut lines: Vec<String> = spans.iter().map(|&(s, e)| text[s..e].to_string()).collect();

    if caret_row >= lines.len() {
        if inner_h >= 2 {
            lines.push(String::new());
        } else {
            // No row to borrow: sit on the last cell of the last row that exists.
            caret_row = lines.len() - 1;
            caret_col = inner_w - 1;
        }
    }

    let base = lines.len().saturating_sub(inner_h).min(caret_row);
    let lines = lines[base..].to_vec();
    Shown {
        caret_row: caret_row.saturating_sub(base).min(inner_h - 1),
        caret_col: caret_col.min(inner_w.saturating_sub(1)),
        lines,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyModifiers};

    /// The box's inner width the typing tests use: wide enough that nothing they
    /// type wraps unless the test says so.
    const W: usize = 40;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_m(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// Type a string into a fresh box as if at the keyboard.
    fn typed(s: &str) -> InputState {
        let mut st = InputState::new();
        for c in s.chars() {
            st.handle_key(key(KeyCode::Char(c)), W);
        }
        st
    }

    /// Byte offset of the start of display row `k` at width `w`.
    fn start_of(rows: &[String], k: usize) -> usize {
        rows[..k].iter().map(|l| l.len()).sum()
    }

    /// The caret's (row, column) for `text` after running `keys` at width `w`.
    fn caret_of(s: &str, keys: &[KeyEvent], w: usize) -> (usize, usize) {
        let mut st = typed(s);
        for k in keys {
            st.handle_key(*k, w);
        }
        let rows = wrap_spans(&st.text, w);
        caret_cell(&st.text, &rows, st.cursor, w)
    }

    /// Tab is no longer invisible to the rest of the app: it produces a command,
    /// so a mode switch can tear down / bring up backends (looprs-05j).
    #[test]
    fn tab_reports_the_switch_it_made() {
        let mut s = InputState::new();
        assert_eq!(s.mode, TerminalType::Beeds);
        let Some(InputAction::SwitchMode { from, to }) = s.handle_key(key(KeyCode::Tab), W) else {
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
        s.handle_key(key(KeyCode::Char('h')), W);
        s.handle_key(key(KeyCode::Char('i')), W);
        let Some(InputAction::Submit { text, mode }) = s.handle_key(key(KeyCode::Enter), W) else {
            panic!("Enter must produce Submit");
        };
        assert_eq!(text, "hi");
        assert_eq!(mode, TerminalType::Beeds);
    }

    /// Submitting takes the caret's offset with the text. A box left holding a
    /// cursor past the end of the string it no longer has is the next panic
    /// report, and this box slices at that offset on every keystroke.
    #[test]
    fn submitting_resets_the_caret_with_the_text() {
        let mut s = typed("hello");
        assert!(s.handle_key(key(KeyCode::Enter), W).is_some());
        assert_eq!(s.text(), "");
        assert_eq!(s.cursor, 0);
        // …and the box still works afterwards.
        s.handle_key(key(KeyCode::Char('a')), W);
        assert_eq!(s.text(), "a");
    }

    // -------------------------------------------------------- caret movement

    /// The complaint this answers: the arrow keys did nothing, so a typo in the
    /// middle of a message meant deleting the whole message and re-typing it.
    #[test]
    fn arrows_move_the_caret_and_typing_lands_where_the_caret_is() {
        let mut s = typed("hello world");
        for _ in 0..5 {
            s.handle_key(key(KeyCode::Left), W);
        }
        // The caret now sits before the 'w'.
        s.handle_key(key(KeyCode::Char('b')), W);
        assert_eq!(s.text(), "hello bworld");
        for _ in 0..2 {
            s.handle_key(key(KeyCode::Right), W);
        }
        s.handle_key(key(KeyCode::Char('!')), W);
        assert_eq!(s.text(), "hello bwo!rld");
    }

    /// Arrows at the ends of the text stop. They do not wrap, do not crash and do
    /// not delete anything, and the caret is exactly where it was.
    #[test]
    fn arrows_at_the_ends_of_the_text_do_not_lose_anything() {
        let mut s = typed("abc");
        for _ in 0..10 {
            s.handle_key(key(KeyCode::Left), W);
        }
        assert_eq!(s.text(), "abc");
        s.handle_key(key(KeyCode::Char('Z')), W);
        assert_eq!(s.text(), "Zabc", "the caret was parked at the start");
        s.handle_key(key(KeyCode::Left), W);
        for _ in 0..10 {
            s.handle_key(key(KeyCode::Right), W);
        }
        assert_eq!(s.text(), "Zabc");
        s.handle_key(key(KeyCode::Char('Y')), W);
        assert_eq!(s.text(), "ZabcY", "…and the same holds at the end");
    }

    /// One press, one glyph — never half of one. A wide glyph is one character to
    /// the user; a caret allowed inside it is a slice on a non-char boundary and a
    /// panic on the next keystroke.
    #[test]
    fn left_and_right_move_whole_glyphs() {
        let mut s = typed("a✨b"); // 'a' is 1 byte, the emoji 3
        s.handle_key(key(KeyCode::Left), W); // back over the 'b'
        s.handle_key(key(KeyCode::Left), W); // and back over the whole emoji
        assert_eq!(
            s.cursor, 1,
            "one press crossed all three bytes of the glyph"
        );
        s.handle_key(key(KeyCode::Char('-')), W);
        assert_eq!(s.text(), "a-✨b");
        s.handle_key(key(KeyCode::Right), W); // forward over the emoji again
        assert_eq!(
            s.cursor,
            1 + "-".len() + "✨".len(),
            "the same glyph, crossed whole in the other direction"
        );
        s.handle_key(key(KeyCode::Char('!')), W);
        assert_eq!(s.text(), "a-✨!b");
    }

    /// Backspace deletes what is behind the *caret*, not the last character of
    /// the message. "It ate the end of my sentence" is the report this prevents.
    #[test]
    fn backspace_deletes_before_the_caret_not_before_the_end_of_the_text() {
        let mut s = typed("delete me");
        for _ in 0..6 {
            s.handle_key(key(KeyCode::Left), W);
        }
        s.handle_key(key(KeyCode::Backspace), W);
        assert_eq!(s.text(), "deete me", "the 'l', not the last 'e'");
    }

    /// Delete (Forward) eats the character under the caret, and is a no-op at the
    /// very end rather than at the very beginning.
    #[test]
    fn delete_eats_forward_from_the_caret() {
        let mut s = typed("abcdef");
        for _ in 0..3 {
            s.handle_key(key(KeyCode::Left), W);
        }
        s.handle_key(key(KeyCode::Delete), W);
        assert_eq!(s.text(), "abcef");
        for _ in 0..3 {
            s.handle_key(key(KeyCode::Right), W);
        }
        s.handle_key(key(KeyCode::Delete), W);
        assert_eq!(s.text(), "abcef", "nothing left to delete");
    }

    /// Ctrl-arrows hop words both ways, and never get stuck on punctuation — a
    /// caret sitting on a `/` in a path still moves, because a key that does
    /// nothing reads as a dropped keystroke.
    #[test]
    fn ctrl_arrows_hop_words_without_getting_stuck() {
        let mut s = typed("fix the bug please");
        s.handle_key(key_m(KeyCode::Left, KeyModifiers::CONTROL), W);
        assert_eq!(s.cursor, 12, "to the start of 'please'");
        s.handle_key(key_m(KeyCode::Left, KeyModifiers::CONTROL), W);
        assert_eq!(s.cursor, 8, "the space before 'bug' went with the word");
        s.handle_key(key_m(KeyCode::Right, KeyModifiers::CONTROL), W);
        assert_eq!(s.cursor, 11, "and back out to the end of 'bug'");

        let mut s = typed("/a/b/c");
        let before = s.cursor;
        s.handle_key(key_m(KeyCode::Right, KeyModifiers::CONTROL), W);
        assert_eq!(s.cursor, before, "already at the end of the text");
        s.handle_key(key_m(KeyCode::Left, KeyModifiers::CONTROL), W);
        assert!(
            s.cursor < before,
            "a non-word character moves one place rather than doing nothing"
        );
    }

    /// Up and Down move over the box's own wrapped rows, not over `\n`s — so a
    /// message the user never pressed anything in still navigates the way it looks.
    #[test]
    fn up_and_down_cross_a_soft_wrap() {
        // A 10-cell box: "aaaaaaaaaa" / "bbbbbbbbbb".
        let text = "a".repeat(10) + &"b".repeat(10);
        assert_eq!(wrap_display(&text, 10).len(), 2);
        // The caret after the last 'b' fills its row and sits one below it.
        assert_eq!(caret_of(&text, &[], 10), (2, 0));

        let mut s = typed(&text);
        // One Up: the head of the row it was on, not two rows up the text.
        s.handle_key(key(KeyCode::Up), 10);
        assert_eq!(s.cursor, 10, "start of the 'b' row");
        assert_eq!(caret_of(&text, &[key(KeyCode::Up)], 10), (1, 0));
        s.handle_key(key(KeyCode::Up), 10);
        assert_eq!(s.cursor, 0);
        s.handle_key(key(KeyCode::Down), 10);
        assert_eq!(s.cursor, 10);
    }

    /// The column the caret was aiming at survives the trip down. Without it every
    /// row after the first is entered at column 0 and the user spends the whole
    /// message typing their way back to where they were.
    #[test]
    fn vertical_moves_remember_the_column_they_were_aiming_at() {
        // Width 6: "aaa " / "bbb " / "ccc " / "ddd " / "eee".
        let text = "aaa bbb ccc ddd eee";
        let rows = wrap_display(text, 6);
        assert_eq!(rows, vec!["aaa ", "bbb ", "ccc ", "ddd ", "eee"]);
        let mut s = typed(text);
        s.handle_key(key_m(KeyCode::Home, KeyModifiers::CONTROL), 6);
        for _ in 0..4 {
            s.handle_key(key(KeyCode::Right), 6);
        }
        assert_eq!(s.cursor, 4, "column 4 of the first row");

        for row in 1..=3usize {
            s.handle_key(key(KeyCode::Down), 6);
            assert_eq!(
                s.cursor,
                start_of(&rows, row) + 4,
                "straight down column 4 to row {row}: {rows:?}"
            );
        }
        // The last row is three cells wide: the caret goes to its end but still
        // *wants* column 4, so the trip back up lands where it left off.
        s.handle_key(key(KeyCode::Down), 6);
        assert_eq!(s.cursor, text.len(), "clamped to the end of the short row");
        for row in (0..=3).rev() {
            s.handle_key(key(KeyCode::Up), 6);
            assert_eq!(
                s.cursor,
                start_of(&rows, row) + 4,
                "back up to row {row}, column 4 restored"
            );
        }
    }

    /// A caret clamped by a short row stays on that row. Landing it on the row
    /// *below* — the other thing a byte offset can mean at a boundary — would move
    /// the caret two rows down for one arrow press.
    #[test]
    fn a_short_row_does_not_forget_the_column_on_the_way_past_it() {
        // Width 10: "aaaaaaaaaa" / " b " / "ccccccccc".
        let text = "a".repeat(10) + " b c" + &"c".repeat(8);
        let rows = wrap_display(&text, 10);
        assert_eq!(rows, vec!["aaaaaaaaaa", " b ", "ccccccccc"]);

        let mut s = typed(&text);
        s.handle_key(key_m(KeyCode::Home, KeyModifiers::CONTROL), 10);
        for _ in 0..7 {
            s.handle_key(key(KeyCode::Right), 10);
        }
        assert_eq!(
            caret_of(&text, &[key_m(KeyCode::Home, KeyModifiers::CONTROL)], 10),
            (0, 0),
            "and the caret starts at the head of the first row"
        );

        s.handle_key(key(KeyCode::Down), 10);
        assert_eq!(
            s.cursor,
            start_of(&rows, 2),
            "column 7 cannot fit a 3-cell row: the caret sits at its end"
        );
        let spans = wrap_spans(&s.text, 10);
        assert_eq!(
            caret_cell(&s.text, &spans, s.cursor, 10),
            (1, 3),
            "on the short row, not dropped to the row below it"
        );

        s.handle_key(key(KeyCode::Down), 10);
        assert_eq!(
            s.cursor,
            start_of(&rows, 2) + 7,
            "column 7 restored on the long row below"
        );
    }

    /// `Home`/`End` are row-local — the rows are what the user can see — and Ctrl
    /// reaches the ends of the whole text. Both are idempotent: pressing a key
    /// twice means the same thing twice, not "the first press was eaten".
    #[test]
    fn home_and_end_are_row_local_and_ctrl_reaches_the_ends_of_the_text() {
        let text = "one two three four five six";
        let rows = wrap_display(text, 10);
        assert_eq!(rows, vec!["one two ", "three ", "four five ", "six"]);
        let mut s = typed(text);

        s.handle_key(key(KeyCode::Home), 10);
        assert_eq!(s.cursor, start_of(&rows, 3), "top of its own row");
        s.handle_key(key(KeyCode::Home), 10);
        assert_eq!(
            s.cursor,
            start_of(&rows, 3),
            "idempotent, not the top of the text"
        );

        s.handle_key(key(KeyCode::End), 10);
        assert_eq!(s.cursor, start_of(&rows, 4), "end of its own row");
        s.handle_key(key(KeyCode::End), 10);
        assert_eq!(s.cursor, start_of(&rows, 4), "idempotent here too");

        s.handle_key(key_m(KeyCode::Home, KeyModifiers::CONTROL), 10);
        assert_eq!(s.cursor, 0, "Ctrl-Home: the start of everything");
        s.handle_key(key(KeyCode::End), 10);
        assert_eq!(s.cursor, start_of(&rows, 1), "end of the first row only");
        s.handle_key(key_m(KeyCode::End, KeyModifiers::CONTROL), 10);
        assert_eq!(s.cursor, text.len(), "Ctrl-End: the end of everything");
    }

    /// `End` on a hard-broken row stops *before* the newline. The newline is not
    /// on that row, and a caret past it would be sitting on the next one.
    #[test]
    fn end_stops_before_a_typed_newline() {
        let mut s = typed("first\nsecond");
        s.handle_key(key(KeyCode::Home), 20);
        s.handle_key(key(KeyCode::Up), 20);
        assert_eq!(s.cursor, 0, "top of the first row");
        s.handle_key(key(KeyCode::End), 20);
        assert_eq!(s.cursor, 5, "end of \"first\", not of the text");
        s.handle_key(key(KeyCode::Right), 20);
        assert_eq!(s.text(), "first\nsecond", "moving right is not deleting");
    }

    // ------------------------------------------------------- new lines in the
    // ------------------------------------------------------------------ input

    /// Shift-Tab is a new line *inside* the same input. Tab still switches mode:
    /// the two keys are one key apart on the keyboard and must not do alike.
    #[test]
    fn shift_tab_is_a_new_line_in_the_input_not_a_mode_switch() {
        let mut s = typed("line one");
        let before = s.mode;
        let act = s.handle_key(key(KeyCode::BackTab), W);
        assert!(act.is_none(), "Shift-Tab is a newline, not a command");
        assert_eq!(s.mode, before, "still the same mode");
        s.handle_key(key(KeyCode::Char('l')), W);
        assert_eq!(s.text(), "line one\nl");
        assert_eq!(
            s.display_lines(20),
            vec!["line one", "l"],
            "and it draws as two lines"
        );
    }

    /// Shift-Enter is the same thing in the terminals that can report it as
    /// distinct from Enter. Most cannot — it arrives as a bare CR — which is
    /// exactly why Shift-Tab exists.
    #[test]
    fn shift_enter_is_a_new_line_too_where_the_terminal_can_report_it() {
        let mut s = typed("a");
        assert!(
            s.handle_key(key_m(KeyCode::Enter, KeyModifiers::SHIFT), W)
                .is_none()
        );
        s.handle_key(key(KeyCode::Char('b')), W);
        assert_eq!(s.text(), "a\nb");
    }

    /// A multi-line box is still *one* message on submit — that is the point of
    /// the key: a newline for a new line, a bare Enter to send.
    #[test]
    fn a_multi_line_input_submits_as_one_message() {
        let mut s = typed("first");
        s.handle_key(key(KeyCode::BackTab), W);
        s.handle_key(key(KeyCode::Char('s')), W);
        s.handle_key(key(KeyCode::BackTab), W);
        s.handle_key(key(KeyCode::Char('t')), W);
        let Some(InputAction::Submit { text, .. }) = s.handle_key(key(KeyCode::Enter), W) else {
            panic!("Enter submits what is in the box");
        };
        assert_eq!(text, "first\ns\nt");
    }

    /// A newline is an editable character like any other: the caret walks over it,
    /// Backspace un-does the join, and the rows merge back into one.
    #[test]
    fn a_typed_newline_is_editable_like_any_character() {
        let mut s = typed("ab\ncd");
        assert_eq!(s.display_lines(20), vec!["ab", "cd"]);
        s.handle_key(key(KeyCode::Up), 20);
        s.handle_key(key(KeyCode::End), 20);
        assert_eq!(s.cursor, 2, "end of the first row, before the newline");
        s.handle_key(key(KeyCode::Delete), 20);
        assert_eq!(s.text(), "abcd");
        assert_eq!(s.display_lines(20), vec!["abcd"]);
    }

    /// A blank line made with two Shift-Tabs stays blank. Eating it silently is how
    /// a box starts disagreeing with what was typed.
    #[test]
    fn a_blank_line_stays_a_blank_line() {
        let mut s = typed("a");
        s.handle_key(key(KeyCode::BackTab), W);
        s.handle_key(key(KeyCode::BackTab), W);
        s.handle_key(key(KeyCode::Char('b')), W);
        assert_eq!(s.text(), "a\n\nb");
        assert_eq!(s.display_lines(20), vec!["a", "", "b"]);
    }

    /// What `Esc` hands back keeps its newlines for the same reason the box makes
    /// them: it can show them, edit them and send them, so folding them to spaces
    /// would only be a worse copy of what was typed. The caret comes back at the
    /// end, ready to keep typing.
    #[test]
    fn set_text_keeps_newlines_and_leaves_the_caret_at_the_end() {
        let mut s = InputState::new();
        s.set_text("one\ntwo\nthree".into());
        assert_eq!(s.text(), "one\ntwo\nthree");
        assert_eq!(s.cursor, 13);
        s.handle_key(key(KeyCode::Home), 20);
        assert_eq!(s.cursor, 8, "Home: the start of 'three'");
        s.handle_key(key(KeyCode::Up), 20);
        assert_eq!(
            s.cursor, 4,
            "and up walks the hard-broken rows like any other"
        );
        s.handle_key(key(KeyCode::Up), 20);
        assert_eq!(s.cursor, 0, "to the top of the box");
        s.handle_key(key(KeyCode::Up), 20);
        assert_eq!(s.cursor, 0, "and no further");
        s.set_text("   padded".into());
        assert_eq!(s.text(), "padded", "leading blank is still trimmed");
    }

    // ------------------------------------------------------------- painting

    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::CellWidth;
    use ratatui::layout::Position;

    /// Paint the box over a whole `w x h` terminal and read the screen and the caret
    /// back. The caret is the thing under test: "I can't see what I'm typing" is a
    /// statement about where the caret went, not about the text.
    fn paint(text: &str, w: u16, h: u16) -> (Vec<String>, Position) {
        paint_at(text, text.len(), w, h)
    }

    /// …with the caret pushed to a given byte offset, so a caret *inside* the text
    /// can be painted and not just one dragged along behind it.
    fn paint_at(text: &str, cursor: usize, w: u16, h: u16) -> (Vec<String>, Position) {
        assert!(text.is_char_boundary(cursor), "test bug: mid-glyph offset");
        let mut s = InputState::new();
        s.set_text(text.into());
        s.cursor = cursor;
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
        let inner = inner_width(w);
        let mut s = InputState::new();
        s.set_text(text.into());
        crate::viewport::input_rows(s.display_lines(inner).len() as u16)
    }

    /// The caret is inside the box's inner area for *every* caret position of
    /// every text length, at the height the policy gives the box. "Somewhere in
    /// the middle of a long message, in a narrow box" is exactly the case that
    /// used to be able to put it on a border, off the screen, or nowhere.
    #[test]
    fn the_caret_is_inside_the_box_at_every_caret_position() {
        for text in [
            "x".repeat(80),
            "one two three four five six seven eight".repeat(3),
            "a\n".repeat(20),
            "word ✨ word ✨ word ".repeat(6),
        ] {
            for w in [12u16, 20, 40] {
                let h = box_h(&text, w);
                if h < 3 {
                    continue; // a box with no text row is the viewport's problem, not the caret's
                }
                for cursor in 0..=text.len() {
                    if !text.is_char_boundary(cursor) {
                        continue;
                    }
                    let (screen, caret) = paint_at(&text, cursor, w, h);
                    assert!(
                        caret.x >= 1 && caret.x <= w - 2,
                        "w={w} cursor {cursor}: column {} is outside 1..={}: {screen:?}",
                        caret.x,
                        w - 2
                    );
                    assert!(
                        caret.y >= 1 && caret.y <= h - 2,
                        "w={w} cursor {cursor}: row {} is outside 1..={}: {screen:?}",
                        caret.y,
                        h - 2
                    );
                }
            }
        }
    }

    /// The box follows the caret back up the text. A window pinned to the tail
    /// would leave the user moving a caret they cannot see — aiming at the second
    /// row of a message while the box shows the last six.
    #[test]
    fn the_box_follows_the_caret_back_up_the_text() {
        let text = (0..40).map(|i| format!("line{i:02} ")).collect::<String>();
        let h = box_h(&text, 20);
        let shown_rows = (h as usize) - 2;
        assert!(
            (3..40).contains(&shown_rows),
            "the box is shorter than the text, which is the point: {h} rows"
        );

        // The caret at the very top: the window shows the top of the message.
        let (screen, caret) = paint_at(&text, 0, 20, h);
        assert_eq!(caret.y, 1);
        assert!(screen[1].contains("line00"), "{screen:?}");

        // The caret back at the end: the tail is what shows.
        let (_, caret) = paint_at(&text, text.len(), 20, h);
        assert_eq!(caret.y, h - 2);

        // A caret that walks up to a row above the window drags the window with
        // it — by exactly as much as it needs, so the caret lands on the top row
        // of what is on screen instead of above it, off the screen.
        let deep = 150usize;
        let spans = wrap_spans(&text, inner_width(20));
        let caret_row = spans.iter().position(|&(_, e)| deep <= e).unwrap();
        assert!(
            caret_row > shown_rows,
            "test setup: that caret is not above the window: {caret_row}"
        );
        let (screen, caret) = paint_at(&text, deep, 20, h);
        assert_eq!(
            caret.y, 1,
            "the window came up to the caret instead of leaving it off the top"
        );
        let mine = text[spans[caret_row].0..spans[caret_row].1].trim_end();
        assert!(
            screen[1].contains(mine),
            "the top row on screen is not the caret's row {mine:?}: {screen:?}"
        );
    }

    /// The row drawn is the row the caret is on, byte for byte: shifting the
    /// window does not move the caret to a different character than it was on.
    #[test]
    fn the_window_shows_the_caret_row_not_a_row_it_split() {
        let text = "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc";
        // Five cells into the 'b' row, which starts at byte 11.
        let cursor = 16;
        assert_eq!(text[11..].chars().next(), Some('b'));
        let (_, caret) = paint_at(text, cursor, 12, 5);
        assert_eq!(
            caret.x, 6,
            "column 5 of its row, plus the border: {caret:?}"
        );
        assert_eq!(caret.y, 2);
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
            "the typed text vanished from the box: {screen:?}",
        );
        assert!(caret.x >= 1 && caret.x <= 10, "caret at {caret:?}");
        assert_eq!(caret.y, 1);
    }

    /// A hard break is drawn as a break: two lines typed are two lines shown, at
    /// a width that would otherwise have put them on one.
    #[test]
    fn a_typed_newline_breaks_the_row_at_any_width() {
        let (screen, caret) = paint("ab\ncd", 40, 4);
        assert!(screen[1].contains("ab"), "{screen:?}");
        assert!(!screen[1].contains("cd"), "{screen:?}");
        assert!(screen[2].contains("cd"), "{screen:?}");
        assert_eq!(caret, Position::new(3, 2));
    }

    /// The wrap is total: an empty box still has the row its caret sits on.
    #[test]
    fn an_empty_box_still_has_a_row_for_the_caret() {
        assert_eq!(wrap_display("", 20), vec![String::new()]);
        let (_, caret) = paint("", 20, 3);
        assert_eq!(caret, Position::new(1, 1));
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

    /// The only thing the wrap is allowed to consume is the newline it broke on.
    /// Stated as an invariant so a future tweak to the break logic cannot quietly
    /// eat a space, a glyph or a whole line.
    #[test]
    fn the_wrap_consumes_nothing_but_the_newlines_it_breaks_on() {
        let text = "a  b\n\nc long long long\n d\ne";
        for w in 1usize..=12 {
            let joined: String = wrap_display(text, w).concat();
            assert_eq!(
                joined,
                text.replace('\n', ""),
                "width {w}: the wrap lost something besides newlines"
            );
            let shown: usize = wrap_spans(text, w).iter().map(|&(s, e)| e - s).sum();
            assert_eq!(
                text.len() - shown,
                text.matches('\n').count(),
                "width {w}: the bytes that vanished are not exactly the newlines"
            );
        }
    }

    /// A hard break at the very end still leaves the row after it: the caret needs
    /// somewhere to sit and the blank line was typed on purpose.
    #[test]
    fn a_trailing_newline_opens_a_row_for_the_caret() {
        assert_eq!(wrap_display("ab\n", 10), vec!["ab", ""]);
        let mut s = typed("ab");
        s.handle_key(key(KeyCode::BackTab), 10);
        assert_eq!(s.display_lines(10), vec!["ab", ""]);
        assert_eq!(s.cursor, 3);
        // …and the caret goes on that new row, not back on the text row.
        let (_, caret) = paint_at("ab\n", 3, 12, 4);
        assert_eq!(caret, Position::new(1, 2));
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

    /// A caret that wants a cell in the middle of a wide glyph stops *before* it.
    /// It cannot sit in the glyph's second cell, and jumping past the glyph would
    /// consume a character the vertical move never aimed at.
    #[test]
    fn a_caret_inside_a_wide_glyph_lands_before_it() {
        let text = "a✨b";
        let row = wrap_spans(text, 20)[0];
        // Column 2 is the tail cell of the wide glyph.
        assert_eq!(offset_at_col(text, row, 2), 1, "before the ✨");
        // Columns that exist are hit exactly.
        assert_eq!(offset_at_col(text, row, 0), 0);
        assert_eq!(offset_at_col(text, row, 1), 1);
        assert_eq!(offset_at_col(text, row, 3), 4, "before the 'b'");
        assert_eq!(
            offset_at_col(text, row, 99),
            row.1,
            "clamped to the row end"
        );
    }
}
