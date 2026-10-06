//! What shell output may contain, and how it becomes a transcript line.
//!
//! **ADR-0005 is the decision; this file is that decision executable.** Read the
//! ADR for the reasoning and the priced losses; read this file for the rules in
//! their only enforceable form.
//!
//! The reason this module exists at all: ADR-0001 rule 1 promised shell output
//! reaches the user as the exact lines the child wrote — no markdown, no
//! re-wrap. That was cheap while the *terminal* resolved the child's control
//! sequences. Once looprs owns the scrollback it owns the resolution, and
//! "verbatim" without resolution means either a string full of escape bytes (which
//! paint garbage into a cell grid we control) or a string with them deleted (which
//! is what `ControlStripper` did, and which fuses every frame of a progress bar
//! into one long line). Both are wrong in a different way. So the bytes are
//! *resolved*, once, here, into lines:
//!
//! * **SGR keeps colour.** `CSI … m` is decoded into a [`ratatui::style::Style`]
//!   and rides along on the text as [`StyleRun`]s. Colour is the one thing shell
//!   output has that is worth the work.
//! * **Cursor motion resolves inside the line.** `\r`, `\b`, `\t` and `EL` are
//!   applied to a cell-addressed line buffer, so `curl`'s bar becomes its last
//!   frame instead of 200 concatenated frames.
//! * **Wide characters are cells.** A cluster occupies 1 or 2 cells and the cell
//!   count is measured with the same function ratatui measures with, so the cells
//!   we laid out in and the width the renderer sees cannot disagree.
//! * **What comes out is control-free.** No `ESC`, no C0 controls, no tabs. The
//!   copy path therefore gets plain text for free: what the user pastes into an
//!   editor is what they saw, minus the presentation (ADR-0005 Q4).
//!
//! Two paths, one value:
//!
//! ```text
//! child bytes ──► LineResolver ──► StyledLine { text: plain, runs: Vec<StyleRun> }
//!                                     │                    │
//!                              what we render         what we copy
//!                              (text + runs)         (text alone)
//! ```

use ratatui::style::{Color, Modifier, Style};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Tab stops every 8 cells — the VT100 default, and what every shell program
/// written against a `tabsize=8` terminal expects (ADR-0005 Q3).
pub const TAB_STOP: usize = 8;

/// A span of a line's plain text that shares one style.
///
/// `start`/`end` are **byte offsets into the owning line's `text`**, always on
/// `char` boundaries: they are produced by cutting that same string at the cell
/// boundaries where the SGR state changed, so slicing them out can never split a
/// character, a cluster, or a style.
#[derive(Clone, Debug, PartialEq)]
pub struct StyleRun {
    pub start: usize,
    pub end: usize,
    pub style: Style,
}

/// One resolved line of shell output.
///
/// `text` is the *content*: escape-free, tab-free, with every `\r`/`\b` overwrite
/// already applied. `runs` is the *presentation*, and is allowed to say things
/// `text` cannot (colour, bold). Keeping them separate rather than storing the
/// child's original bytes is what lets the copy path be a field access instead of
/// a second decoder — see ADR-0005 Q4 for why "what you see" and "what you
/// paste" are allowed to differ, and why only one of them is stored.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StyledLine {
    pub text: String,
    /// Sorted, non-overlapping, contiguous where styles match.
    pub runs: Vec<StyleRun>,
    /// How many terminal cells `text` occupies — the width it was *laid out* in,
    /// and (by the invariant below) the width the renderer measures for it.
    ///
    /// This is the cell side of the mapping ADR-0005 Q4 says the store has to
    /// keep: wrapping counts cells, selection works in characters, and the two
    /// only agree if the line carries its cell count rather than guessing it
    /// from its byte length.
    pub cells: usize,
}

impl StyledLine {
    /// The line as it should be pasted: the content, trailing blanks removed.
    ///
    /// ADR-0005 Q4: a resolved line carries blanks it did not start with — the
    /// padding a tab expanded into, the blank left where a half-overwritten wide
    /// glyph used to be. Those are layout, and a paste of layout is noise.
    ///
    /// No caller in the shipped binary yet: select-to-copy (`looprs-pdl.10`) is
    /// the consumer. The trim rule lives here rather than in that ticket so that
    /// "what you copy" has exactly one definition in the tree.
    #[allow(dead_code)] // consumer: looprs-pdl.10 (select-to-copy)
    pub fn copy_text(&self) -> &str {
        self.text.trim_end_matches([' ', '\t'])
    }

    /// This line as a ratatui `Line`, styles and all.
    pub fn to_line(&self) -> ratatui::text::Line<'static> {
        spanned(&self.text, &self.runs, 0, self.text.len())
    }
}

/// A `Line` for `text[start..end]` with `runs` clipped to that range.
///
/// Gaps between runs are raw text: a run list that says nothing is a run list
/// that means "default style", not one that means "print nothing". That matters
/// because most shell output has no styles at all, and the empty-`runs` path is
/// the common one, not the degenerate one.
pub fn spanned(
    text: &str,
    runs: &[StyleRun],
    start: usize,
    end: usize,
) -> ratatui::text::Line<'static> {
    use ratatui::text::{Line, Span};
    let mut spans = Vec::new();
    let mut pos = start;
    for run in runs {
        let s = run.start.max(start);
        let e = run.end.min(end);
        if s >= e {
            continue;
        }
        if s > pos {
            spans.push(Span::raw(slice(text, pos, s)));
        }
        spans.push(Span::styled(slice(text, s, e), run.style));
        pos = e;
    }
    if pos < end {
        spans.push(Span::raw(slice(text, pos, end)));
    }
    Line::from(spans)
}

/// `text[start..end]` owned. Byte ranges in this module are always `char`
/// boundaries; this never splits a character, and if it somehow were handed one it
/// yields the empty string rather than panicking in a render path.
fn slice(text: &str, start: usize, end: usize) -> String {
    if start >= end {
        return String::new();
    }
    text.get(start..end).unwrap_or_default().to_string()
}

/// One cell slot of the line being composed.
///
/// Slots are addressed by **cell column**, one slot per cell. A double-width
/// cluster takes two slots: the cluster itself in its lead slot and a
/// [`Slot::continuation`] placeholder in the slot to its right, which is how a
/// write at an arbitrary column can find, in O(1), whether it landed in the
/// middle of a wide glyph (ADR-0005 Q4 "wide characters partially overwritten").
#[derive(Clone, Debug, PartialEq)]
struct Slot {
    /// The character cluster in this cell. Empty only for a continuation slot.
    text: String,
    /// Cells this cluster occupies: 1 or 2. A continuation slot carries 1 here
    /// and is never measured.
    cells: u8,
    style: Style,
    continuation: bool,
}

impl Slot {
    fn blank(style: Style) -> Self {
        Self {
            text: " ".to_string(),
            cells: 1,
            style,
            continuation: false,
        }
    }

    /// The cell a multi-cell cluster's right half sits in. Carries no text: the
    /// cluster's characters all live in its lead slot, which is what keeps the
    /// line's `text` a faithful copy of what was written while `slots` stays a
    /// one-slot-per-cell grid.
    fn cont(style: Style) -> Self {
        Self {
            text: String::new(),
            cells: 1,
            style,
            continuation: true,
        }
    }
}

/// Where the resolver is inside an escape sequence. Same shape as
/// [`ControlStripper`](crate::utils::render::ControlStripper)'s, for the same
/// reason: a sequence can straddle two reads, and half of it must never be
/// shown as text.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum State {
    #[default]
    Text,
    Escaped,
    /// `CSI … ` — accumulating parameter bytes until the final byte in `@..~`.
    Csi,
    /// `OSC …` until BEL or ST.
    Osc,
    /// Inside an OSC and just saw the `ESC` of its `ESC \` terminator. Its own
    /// state because the terminator can straddle a read boundary: if the `ESC`
    /// arrives with nothing after it, a peek at the next byte sees nothing, and
    /// "OSC is over" and "OSC is still going" then look identical. Which is to
    /// say: without this state, terminating on a chunk boundary swallows the rest
    /// of the stream — and it does so silently, which is the bad kind of wrong.
    OscEsc,
}

/// Resolves a child's line-mode byte stream into [`StyledLine`]s.
///
/// Stateful and **per stream** (one per [`SessionView`](crate::session::view::SessionView)),
/// because all four of the things it does depend on history: the SGR state carries
/// across lines the way a terminal's does, an escape sequence carries across
/// reads, and the cell it is about to overwrite was put there by an earlier one.
#[derive(Default)]
pub struct LineResolver {
    slots: Vec<Slot>,
    /// The cell column the next write lands at.
    col: usize,
    /// The cluster the cursor is still writing into, as its lead slot index — the
    /// one a following combining mark, variation selector or ZWJ may extend.
    /// Cleared by anything that moves the cursor ([`Self::commit_cluster`]).
    open_cluster: Option<usize>,
    /// A `ZWJ` has been written and is waiting to pull the next base into the
    /// open cluster.
    join_pending: bool,
    /// The SGR state, carried across lines exactly as a terminal's SGR state is
    /// carried: nothing resets it but another SGR sequence.
    style: Style,
    /// Width the child is wrapping at (the pty's width), used for the
    /// row-relative meaning of `\r`, `\b` and `\t`. `None` = no wrap emulation.
    wrap: Option<usize>,
    state: State,
    /// Parameter bytes of the `CSI` sequence being read.
    params: String,
    /// `true` once a parameter byte of the sequence currently in `params` says the
    /// sequence is uninterpretable (a `:` sub-parameter), so the final byte must
    /// not act on it.
    params_bad: bool,
}

impl LineResolver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Tell the resolver the width the child writes for, in cells.
    ///
    /// This must be the **same value that is forwarded to the pty**, so a `\r`
    /// here lands where the child believed row 0 of its own wrapped line was
    /// (ADR-0005 Q2). `0` disables the emulation, which makes `\r` go to the
    /// start of the whole logical line.
    pub fn set_wrap_width(&mut self, cols: usize) {
        self.wrap = if cols >= 2 { Some(cols) } else { None };
    }

    /// Feed the next chunk of the stream; return the lines it *completed*.
    ///
    /// A line is completed by `\n` only — the child's own line ending, not our
    /// width. Long lines stay open until the child says so, which is what keeps
    /// our wrap positions and the child's the same value
    /// ([`set_wrap_width`](Self::set_wrap_width)).
    pub fn feed(&mut self, chunk: &str) -> Vec<StyledLine> {
        let mut out = Vec::new();
        for c in chunk.chars() {
            match self.state {
                State::Text => match c {
                    '\n' => {
                        self.commit_cluster();
                        out.push(self.finish_line())
                    }
                    '\r' => {
                        self.commit_cluster();
                        self.col = self.row_start();
                    }
                    // Backspace: one cell left, and no further than the start of
                    // the row — which is what a terminal with backspace-at-column-0
                    // does, and why a wrapped line cannot be edited backwards past
                    // its own left edge (ADR-0005 Q2).
                    '\u{8}' => {
                        self.commit_cluster();
                        self.backspace()
                    }
                    '\t' => {
                        self.commit_cluster();
                        self.tab()
                    }
                    '\u{1b}' => self.state = State::Escaped,
                    // Every other C0 control, and DEL: the child's business, not
                    // content. Same verdict as the old stripper, stated so.
                    c if (c as u32) < 0x20 || c == '\u{7f}' => {}
                    c => self.put_char(c),
                },
                State::Escaped => match c {
                    '[' => {
                        self.params.clear();
                        self.params_bad = false;
                        self.state = State::Csi;
                    }
                    ']' => self.state = State::Osc,
                    // `ESC <char>` is a complete (if archaic) sequence.
                    _ => self.state = State::Text,
                },
                State::Csi => {
                    if c.is_ascii_digit() || c == ';' {
                        self.params.push(c);
                    } else if c == ':' {
                        // Sub-parameters (colon-separated) are not in the set we
                        // interpret. Keeping the digits we have and letting the
                        // final byte act on them would be a *guess* at a
                        // sequence's meaning, which is how colour turns into
                        // noise, so the whole sequence is dropped instead.
                        self.params_bad = true;
                    } else if ('\u{40}'..='\u{7e}').contains(&c) {
                        // A final byte: the sequence ends here.
                        if !self.params_bad {
                            self.apply_csi(c);
                        }
                        self.state = State::Text;
                    }
                    // `?`, `>`, `<`, spaces: private prefixes and intermediates.
                    // Read past them; the final byte still terminates.
                }
                State::Osc => {
                    if c == '\u{7}' {
                        self.state = State::Text;
                    } else if c == '\u{1b}' {
                        self.state = State::OscEsc;
                    }
                }
                State::OscEsc => {
                    // `ESC \` is the OSC terminator (ST). Anything else is a
                    // new sequence starting inside the old one, read as such.
                    self.state = match c {
                        '\\' => State::Text,
                        ']' => State::Osc,
                        '[' => {
                            self.params.clear();
                            self.params_bad = false;
                            State::Csi
                        }
                        _ => State::Text,
                    };
                }
            }
        }
        out
    }

    /// The line currently open, for the live region. `None` when nothing is open.
    pub fn pending(&self) -> Option<StyledLine> {
        if self.slots.is_empty() {
            return None;
        }
        Some(self.compose())
    }

    /// As [`Self::pending`], but the line ends here: it will not be seen again.
    pub fn take_pending(&mut self) -> Option<StyledLine> {
        let line = self.pending()?;
        self.commit_cluster();
        self.slots.clear();
        self.col = 0;
        Some(line)
    }

    /// Bytes sitting in the open line, for the caller's memory accounting.
    ///
    /// The store cannot see the line still being resolved, so without this the
    /// view's buffer cap under-counts by exactly the thing a pathological child
    /// can make arbitrarily large: one line, never terminated.
    pub fn pending_len(&self) -> usize {
        self.slots.iter().map(|s| s.text.len()).sum()
    }

    /// Drop the open line and the SGR state.
    ///
    /// For a stream that is over: a dangling `\x1b[` belongs to a child that will
    /// never send the rest of it, and SGR left on by a killed program must not be
    /// charged to the next shell's first line.
    pub fn reset(&mut self) {
        self.slots.clear();
        self.col = 0;
        self.commit_cluster();
        self.style = Style::default();
        self.state = State::Text;
        self.params.clear();
        self.params_bad = false;
    }

    // ─────────────────────────── the cell grid ───────────────────────────

    fn row_start(&self) -> usize {
        match self.wrap {
            Some(w) => self.col - (self.col % w),
            None => 0,
        }
    }

    /// Cells already used in the current row, i.e. the screen column of `col`.
    fn screen_col(&self) -> usize {
        match self.wrap {
            Some(w) => self.col % w,
            None => self.col,
        }
    }

    fn backspace(&mut self) {
        let row = self.row_start();
        if self.col > row {
            self.col -= 1;
        }
    }

    /// A tab advances to the next stop, in **screen** columns, and no further
    /// than the end of the row (ADR-0005 Q3).
    fn tab(&mut self) {
        let screen = self.screen_col();
        let mut advance = TAB_STOP - (screen % TAB_STOP);
        if let Some(w) = self.wrap {
            advance = advance.min(w - screen);
        }
        self.fill(advance);
    }

    /// `n` blanks at the cursor, in the current style.
    fn fill(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.commit_cluster();
        self.blank_lead_at(self.col);
        self.ensure(self.col + n);
        for slot in &mut self.slots[self.col..self.col + n] {
            *slot = Slot::blank(self.style);
        }
        self.col += n;
    }

    /// Blank the lead cell of the wide cluster whose right half is at `at`, if
    /// there is one.
    ///
    /// Anything that paints over a continuation cell takes the whole cluster: the
    /// alternative is a grid holding a wide lead with a foreign cell to its right,
    /// which repaints as a broken glyph forever. This is the same rule the
    /// terminal applied before we owned the cells (ADR-0005 Q4).
    fn blank_lead_at(&mut self, at: usize) {
        if at > 0 && self.slots.get(at).is_some_and(|s| s.continuation) {
            self.slots[at - 1] = Slot::blank(self.style);
        }
    }

    fn ensure(&mut self, len: usize) {
        while self.slots.len() < len {
            self.slots.push(Slot::blank(self.style));
        }
    }

    /// Put one codepoint of content into the line.
    ///
    /// A codepoint is either a **base** (it can start a cluster) or a **joiner**
    /// — a zero-width mark, a variation selector, a tag, a skin-tone modifier, or
    /// `ZWJ`. A joiner extends the cluster to its left instead of taking a cell,
    /// and `ZWJ` additionally pulls the *next* base into that same cluster. That
    /// is what makes 👩‍👩‍👦 one cluster of two cells rather than three clusters of
    /// six — i.e. the same shape the renderer measures, which is the whole point
    /// of measuring with `unicode_width` (ADR-0005 Q4).
    fn put_char(&mut self, c: char) {
        if self.join_pending {
            self.join_pending = false;
            // If the ZWJ had nothing left to pull into, the base stands alone.
            if let Some(lead) = self.open_cluster {
                self.extend_cluster(lead, c);
                return;
            }
        }
        if is_joiner(c) {
            if let Some(lead) = self.open_cluster {
                self.extend_cluster(lead, c);
                self.join_pending = c == ZWJ;
                return;
            }
            // A joiner with nothing to join. A bare combining mark cannot take a
            // cell without breaking the invariant that the cells laid out *are*
            // the width the renderer measures (see `finish_line`), and a cell of
            // loose accent carries no information. This is the one thing a
            // resolved line can lose, and ADR-0005 Q4 prices it in the open.
            tracing::trace!(
                "dropped orphan joiner U+{:04X}: no cluster to join",
                c as u32
            );
            return;
        }
        self.place_cluster(c.to_string());
    }

    /// Start a new cluster at the cursor, taking `cluster_cells(&text)` cells.
    fn place_cluster(&mut self, text: String) {
        let cells = cluster_cells(&text);
        self.ensure(self.col + cells);
        self.blank_lead_at(self.col);
        self.slots[self.col] = Slot {
            text,
            cells: cells as u8,
            style: self.style,
            continuation: false,
        };
        for k in 1..cells {
            self.slots[self.col + k] = Slot::cont(self.style);
        }
        // A cluster narrower than the one it replaced can leave that cluster's
        // tail stranded. A continuation with no lead in front of it is a glyph
        // that repaints broken for the rest of the line's life, so the stranded
        // cells become blanks.
        let mut i = self.col + cells;
        while self.slots.get(i).is_some_and(|s| s.continuation) {
            self.slots[i] = Slot::blank(self.style);
            i += 1;
        }
        self.col += cells;
        self.open_cluster = Some(self.col - cells);
    }

    /// Add one codepoint to the cluster started at `lead`, and re-measure it.
    ///
    /// Re-measuring can change the cluster's cell count — `☺` is one cell and
    /// `☺️` is two — so the cluster's continuation cells are resized with it.
    /// The cursor follows when it was sitting at the cluster's end, because a
    /// mark typed after a character is typed *at* that character, not after it.
    fn extend_cluster(&mut self, lead: usize, c: char) {
        let Some(slot) = self.slots.get_mut(lead) else {
            return;
        };
        slot.text.push(c);
        let new_cells = cluster_cells(&slot.text);
        let old_cells = slot.cells as usize;
        if new_cells == old_cells {
            return;
        }
        slot.cells = new_cells as u8;
        let delta = new_cells as isize - old_cells as isize;
        if delta > 0 {
            let at = lead + old_cells;
            for k in 0..delta as usize {
                self.slots.insert(at + k, Slot::cont(self.style));
            }
        } else {
            let at = lead + new_cells;
            self.slots.drain(at..at + (-delta) as usize);
        }
        if self.col >= lead + old_cells {
            self.col = (self.col as isize + delta) as usize;
        }
    }

    /// The cluster the cursor is still writing into is finished: later joiners may
    /// not reach back into it.
    ///
    /// Called by every operation that moves the cursor. A terminal applies a
    /// combining mark to the character just written; once the cursor has been
    /// put elsewhere, "the character just written" is no longer a thing, and a
    /// mark that arrives then has nowhere honest to go.
    fn commit_cluster(&mut self) {
        self.open_cluster = None;
        self.join_pending = false;
    }

    // ───────────────────────── control sequence effects ─────────────────────────

    /// The CSI sequences the transcript honours, and only these.
    ///
    /// Everything else in the alphabet is a *screen* operation (cursor addressing,
    /// scrolling, erasing the display) or a device control we do not own. A
    /// transcript stores lines; a screen operation has no line-level meaning, so
    /// it is dropped and its text kept. ADR-0005 Q1/Q2 lists what that costs.
    fn apply_csi(&mut self, final_byte: char) {
        let p = params(&self.params);
        match final_byte {
            'm' => self.apply_sgr(&p),
            // EL — erase in line. The one screen-side operation that *is*
            // meaningful inside a line, and that every progress bar uses to keep a
            // shorter repaint from leaving the longer one's tail behind.
            'K' => match p.first().copied().unwrap_or(0) {
                0 => self.erase_from(self.col),
                1 => {
                    let upto = self.col + 1;
                    self.erase_before(upto)
                }
                _ => {
                    let col = self.col;
                    self.slots.clear();
                    self.col = 0;
                    // Blanks out to where the cursor was, and stops there: the
                    // line is empty, not ended.
                    self.fill(col);
                }
            },
            // ICH (insert blanks) and DCH (delete cells) are cell surgery in the
            // middle of a line, and they only ever appear in output aimed at a
            // screen we are not painting — full-screen children are teed straight
            // to the terminal and never reach here. Dropping them keeps the grid
            // consistent for the price of a sequence that does nothing.
            _ => {}
        }
    }

    fn erase_from(&mut self, from: usize) {
        self.commit_cluster();
        self.blank_lead_at(from);
        self.slots.truncate(from);
    }

    fn erase_before(&mut self, upto: usize) {
        self.commit_cluster();
        let upto = upto.min(self.slots.len());
        for i in 0..upto {
            if self.slots[i].continuation && i > 0 {
                self.slots[i - 1] = Slot::blank(self.style);
            }
            self.slots[i] = Slot::blank(self.style);
        }
    }

    // ─────────────────────────────── SGR ───────────────────────────────

    /// Decode `CSI … m` into the running style (ADR-0005 Q1).
    ///
    /// A closed, tested parameter set: reset, the nine attributes and their
    /// off-switches, the 8+8 ANSI palette in both spellings, and 256-colour /
    /// truecolour via `38`/`48`. Anything not in the table is ignored *without
    /// losing the text it was wrapped around*, which is the property that
    /// separates this from a regex.
    ///
    /// Colours are kept **indexed**, not mapped to ratatui's named colours, so the
    /// user's own terminal palette paints them: `SGR 31` is *their* red, which is
    /// the only promise worth making about colour.
    fn apply_sgr(&mut self, p: &[u32]) {
        if p.is_empty() {
            self.style = Style::default();
            return;
        }
        let mut style = self.style;
        let mut i = 0;
        while i < p.len() {
            match p[i] {
                0 => style = Style::default(),
                1 => style = style.add_modifier(Modifier::BOLD),
                2 => style = style.add_modifier(Modifier::DIM),
                3 => style = style.add_modifier(Modifier::ITALIC),
                4 => style = style.add_modifier(Modifier::UNDERLINED),
                5 => style = style.add_modifier(Modifier::SLOW_BLINK),
                6 => style = style.add_modifier(Modifier::RAPID_BLINK),
                7 => style = style.add_modifier(Modifier::REVERSED),
                8 => style = style.add_modifier(Modifier::HIDDEN),
                9 => style = style.add_modifier(Modifier::CROSSED_OUT),
                // ECMA-48 says 22 turns off both intensities; terminals disagree
                // about 21 (double underline in xterm, "bold off" elsewhere).
                // Underline is the visible reading and the one git uses.
                21 => style = style.add_modifier(Modifier::UNDERLINED),
                22 => style = style.remove_modifier(Modifier::BOLD | Modifier::DIM),
                23 => style = style.remove_modifier(Modifier::ITALIC),
                24 => style = style.remove_modifier(Modifier::UNDERLINED),
                25 => style = style.remove_modifier(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK),
                27 => style = style.remove_modifier(Modifier::REVERSED),
                28 => style = style.remove_modifier(Modifier::HIDDEN),
                29 => style = style.remove_modifier(Modifier::CROSSED_OUT),
                30..=37 => style = style.fg(Color::Indexed((p[i] - 30) as u8)),
                38 => {
                    if let Some((color, next)) = extended_color(p, i) {
                        style = style.fg(color);
                        i = next;
                        continue;
                    }
                }
                39 => style = style.fg(Color::Reset),
                40..=47 => style = style.bg(Color::Indexed((p[i] - 40) as u8)),
                48 => {
                    if let Some((color, next)) = extended_color(p, i) {
                        style = style.bg(color);
                        i = next;
                        continue;
                    }
                }
                49 => style = style.bg(Color::Reset),
                // 58/59 are the *underline* colour. They are parsed for their
                // shape and thrown away — but they must be consumed, or their
                // `;5;196` tail comes back round the loop and 5 reads as
                // "slow blink". That is the failure mode this whole branch is
                // here to prevent, and a test watches it.
                58 | 59 => {
                    if let Some((_, next)) = extended_color(p, i) {
                        i = next;
                        continue;
                    }
                }
                90..=97 => style = style.fg(Color::Indexed((p[i] - 82) as u8)),
                100..=107 => style = style.bg(Color::Indexed((p[i] - 92) as u8)),
                // Anything else (58/59 underline colour, 53 framed, …) is
                // recognised-as-unsupported rather than misapplied.
                _ => {}
            }
            i += 1;
        }
        self.style = style;
    }

    // ───────────────────────────── composing ─────────────────────────────

    fn finish_line(&mut self) -> StyledLine {
        let line = self.compose();
        self.slots.clear();
        self.col = 0;
        line
    }

    /// Slots → text + runs. Continuation slots contribute no text: the cells of
    /// a wide cluster are already paid for by the characters in its lead slot,
    /// which is what makes `text` both the copy value and a faithful record of
    /// what the child wrote.
    fn compose(&self) -> StyledLine {
        let mut text = String::new();
        let mut runs: Vec<StyleRun> = Vec::new();
        for slot in self.slots.iter().filter(|s| !s.continuation) {
            if slot.text.is_empty() {
                continue;
            }
            let start = text.len();
            text.push_str(&slot.text);
            push_run(&mut runs, slot.style, start, text.len());
        }
        let cells = self.slots.len();
        // ADR-0005 Q4, checked on every line the resolver finishes or reports:
        // the cells we laid out are the width the renderer measures. Both sides
        // get that number from `unicode_width` — ours at cluster width, the
        // renderer's over the same text — so a wrap done in cells and a wrap
        // done on `text` cannot drift apart. If this ever fires, the cluster
        // rules in `is_joiner`/`cluster_cells` and the measure have diverged.
        debug_assert_eq!(
            cells,
            text.width(),
            "cells laid out ({}) != measured width of the resolved text ({}) for {:?}",
            cells,
            text.width(),
            text
        );
        debug_assert!(
            is_control_free(&text),
            "a control character survived resolution: {text:?}"
        );
        StyledLine { text, runs, cells }
    }
}

/// Append a run, merging with the previous one when the style is the same.
///
/// Merging is not an optimisation: `git diff` closes and reopens the *same* green
/// around every token (`\e[32m+\e[m\e[32m+text\e[m`), and without merging a
/// single added line arrives as a dozen one-token spans that make selection,
/// equality and the render pass all worse for nothing.
///
/// A run of default style is not recorded at all: "no style said anything" is
/// what an empty `runs` list means, and most shell output is exactly that. The
/// reader fills the gap ([`spanned`]), so nothing is lost and the common path
/// carries no run per character.
fn push_run(runs: &mut Vec<StyleRun>, style: Style, start: usize, end: usize) {
    if style == Style::default() {
        return;
    }
    if let Some(last) = runs.last_mut()
        && last.style == style
        && last.end == start
    {
        last.end = end;
        return;
    }
    runs.push(StyleRun { start, end, style });
}

/// `38;5;n` / `48;5;n` / `38;2;r:g:b` / `48;2;r:g:b` → colour + next index.
///
/// `None` when the tail is not a well-formed extended colour: the parameter is
/// then treated as an unknown code rather than as a truncated colour, so the
/// parameters after it still get their own chance.
fn extended_color(p: &[u32], i: usize) -> Option<(Color, usize)> {
    match p.get(i + 1)? {
        5 => {
            let n = *p.get(i + 2)?;
            Some((Color::Indexed(n.min(255) as u8), i + 3))
        }
        2 => {
            let r = *p.get(i + 2)?;
            let g = *p.get(i + 3)?;
            let b = *p.get(i + 4)?;
            Some((
                Color::Rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8),
                i + 5,
            ))
        }
        _ => None,
    }
}

/// `ZWJ` — the joiner that also pulls the *next* base into the cluster.
const ZWJ: char = '\u{200d}';

/// Does this codepoint join the cluster to its left rather than starting one?
///
/// Zero-width covers the combining marks, the variation selectors, the tag
/// characters of flag sequences and `ZWJ` itself. The emoji skin-tone modifiers
/// are the exception worth naming: `unicode-width` calls `U+1F3FD` two cells wide
/// **on its own**, and it is — but typed after 👩 it joins it, and the joined
/// cluster `👩🏽` measures two. Treating the modifier as a base would lay that
/// out as four cells and put us a whole glyph out of step with the renderer.
fn is_joiner(c: char) -> bool {
    c.width().unwrap_or(0) == 0 || matches!(c, '\u{1F3FB}'..='\u{1F3FF}')
}

/// How many cells a cluster takes: exactly what the renderer measures for that
/// cluster's text, with a floor of one (a cluster that occupies no cell cannot be
/// laid out in a grid, and measuring zero is not information).
///
/// Not clamped at two. An exotic joined sequence may measure more, and clamping
/// it would make the cell grid disagree with the wrap — the one disagreement that
/// corrupts layout rather than merely looking odd.
fn cluster_cells(text: &str) -> usize {
    text.width().max(1)
}

/// `"38;5;208"` → `[38, 5, 208]`. An empty parameter list is `[0]`, which is
/// what `CSI m` means (reset) and what `git` emits at the end of every span.
fn params(raw: &str) -> Vec<u32> {
    if raw.is_empty() {
        return vec![0];
    }
    raw.split(';')
        .map(|s| s.parse::<u32>().unwrap_or(0))
        .collect()
}

/// The ADR-0005 Q4 invariant, checked on demand: the content of a resolved line
/// contains no escape byte, no C0 control other than the `\n` separators the
/// *caller* adds, and no tab. This is the property the copy path is built on: if
/// a paste can ever carry ANSI noise, this function says so before the store
/// does. [`LineResolver::finish_line`] asserts it on every completed line.
pub fn is_control_free(text: &str) -> bool {
    !text
        .chars()
        .any(|c| c == '\u{1b}' || ((c as u32) < 0x20 && c != '\n') || c == '\u{7f}' || c == '\t')
}

// ───────────────────────────────── tests ─────────────────────────────────
//
// Every rule ADR-0005 states is a test here, in the same order the ADR asks the
// four questions. `feed` is a stateful stream, so every test also has to say what
// happens when the stream is cut at an inconvenient byte — the
// `one_char_at_a_time` test is the one that does that for the whole corpus.

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolve a whole stream, including its unterminated tail.
    fn resolve(chunks: &[&str]) -> Vec<StyledLine> {
        let mut r = LineResolver::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(r.feed(c));
        }
        out.extend(r.take_pending());
        out
    }

    fn texts(stream: &str) -> Vec<String> {
        resolve(&[stream]).iter().map(|l| l.text.clone()).collect()
    }

    fn one(stream: &str) -> StyledLine {
        let mut l = resolve(&[stream]);
        assert_eq!(l.len(), 1, "expected exactly one line from {stream:?}");
        l.remove(0)
    }

    /// Resolve with a wrap width — the thing that makes `\r` / `\b` / `\t` mean
    /// "of this row" rather than "of this line".
    fn wrapped(wrap: usize, stream: &str) -> Vec<String> {
        let mut r = LineResolver::new();
        r.set_wrap_width(wrap);
        let mut out: Vec<String> = r.feed(stream).iter().map(|l| l.text.clone()).collect();
        out.extend(r.take_pending().iter().map(|l| l.text.clone()));
        out
    }

    /// The line's runs as `(covered text, foreground)` pairs, so an expectation
    /// reads like a table.
    fn runs(line: &StyledLine) -> Vec<(&str, Option<Color>)> {
        line.runs
            .iter()
            .map(|r| (&line.text[r.start..r.end], r.style.fg))
            .collect()
    }

    /// Sequences deliberately aimed at the places a chunk boundary can land:
    /// mid-escape, mid-cluster, mid-overwrite.
    const CORPUS: &[&str] = &[
        "plain\n",
        "\u{1b}[32mgreen\u{1b}[0m\n",
        "\u{1b}[1;38;5;208morange\u{1b}[m\n",
        "abc\rXY\n",
        "abc\u{8}X\n",
        "a\tb\tc\n",
        "漢字 Wide\n",
        "e\u{301} combining\n",
        "\u{1f469}\u{200d}\u{1f466} family\n",
        "\u{263a}\u{fe0f} vs16 and \u{1f469}\u{1f3fd} skin\n",
        "a\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e0067}\u{e007f}b flag\n",
        "1\u{fe0f}\u{20e3} keycap\n",
        "\r\u{1b}[2K\n",
        "AAAA\rBB\u{1b}[Kdone\n",
        "\u{1b}]0;window title\u{7}after osc\n",
        "\u{1b}]0;title\u{1b}\\after st\n",
        "\u{1b}[?1049h\u{1b}[2Jscreen\u{1b}[?1049l\n",
        "a\u{1}\u{7}\u{7f}b\n",
        "\u{1b}[?25lhidden cursor\u{1b}[?25h\n",
        "a\u{1b}[2Ab\n",
        "\u{1b}[38:5:1mcolon\u{1b}[0m\n",
        "\u{1b}[38;2;16;160;240mrgb\u{1b}[49m x\n",
    ];

    // ─────────── Q1: SGR keeps colour; everything else keeps its text ───────────

    #[test]
    fn plain_text_and_its_newlines_survive() {
        assert_eq!(texts("hello\nworld\n"), ["hello", "world"]);
        assert_eq!(one("no trailing newline").text, "no trailing newline");
    }

    #[test]
    fn sgr_colour_becomes_a_style_on_the_text_it_covered() {
        let l = one("\u{1b}[31mred\u{1b}[39m plain");
        assert_eq!(l.text, "red plain");
        assert_eq!(
            runs(&l),
            [
                ("red", Some(Color::Indexed(1))),
                // `39` is an explicit "back to the terminal's default", which is
                // a thing the line says, so it is a run — a Reset is not the
                // same fact as "nothing was said", and only the second of these
                // lets a base style show through.
                (" plain", Some(Color::Reset)),
            ]
        );
    }

    #[test]
    fn background_colour_is_a_separate_channel() {
        let l = one("\u{1b}[44mblock\u{1b}[49m");
        assert_eq!(runs(&l), [("block", None)]);
        assert_eq!(l.runs[0].style.bg, Some(Color::Indexed(4)));
        assert_eq!(l.runs[0].style.fg, None, "fg was never mentioned");
    }

    #[test]
    fn the_nine_attributes_are_their_own_modifiers() {
        let cases: &[(&str, Modifier)] = &[
            ("1", Modifier::BOLD),
            ("2", Modifier::DIM),
            ("3", Modifier::ITALIC),
            ("4", Modifier::UNDERLINED),
            ("5", Modifier::SLOW_BLINK),
            ("6", Modifier::RAPID_BLINK),
            ("7", Modifier::REVERSED),
            ("8", Modifier::HIDDEN),
            ("9", Modifier::CROSSED_OUT),
        ];
        for (code, m) in cases {
            let l = one(&format!("\u{1b}[{code}mX\u{1b}[0m"));
            assert!(
                l.runs[0].style.add_modifier.contains(*m),
                "SGR {code} must set {m:?}, got {:?}",
                l.runs[0].style
            );
        }
    }

    #[test]
    fn an_off_switch_removes_the_one_thing_it_names() {
        let l = one("\u{1b}[1;31mX\u{1b}[22mY\u{1b}[0m");
        assert_eq!(l.text, "XY");
        assert!(l.runs[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(
            !l.runs[1].style.add_modifier.contains(Modifier::BOLD),
            "22 turns bold off"
        );
        assert_eq!(
            l.runs[1].style.fg,
            Some(Color::Indexed(1)),
            "…and leaves the colour it was not told to touch"
        );
    }

    #[test]
    fn the_bright_palette_is_indexed_8_to_15_in_both_spellings() {
        let l = one("\u{1b}[90mbright-black\u{1b}[107mX");
        assert_eq!(l.runs[0].style.fg, Some(Color::Indexed(8)));
        assert_eq!(l.runs[1].style.bg, Some(Color::Indexed(15)));
    }

    #[test]
    fn colour_stays_indexed_so_the_users_palette_paints_it() {
        // SGR 1 is *their* red, not ratatui's `Color::Red`. Asserting the
        // absence of named colours is what stops a "helpful" mapping being added
        // later and quietly overriding the terminal.
        for code in 30..=37 {
            let l = one(&format!("\u{1b}[{code}mX\u{1b}[0m"));
            assert!(
                matches!(
                    l.runs[0].style.fg,
                    Some(Color::Indexed(_)) | Some(Color::Rgb(..))
                ),
                "SGR {code} must not resolve to a named colour"
            );
        }
    }

    #[test]
    fn extended_colour_256_and_truecolour() {
        let l = one("\u{1b}[38;5;208morange\u{1b}[48;5;21mX\u{1b}[0m");
        assert_eq!(l.runs[0].style.fg, Some(Color::Indexed(208)));
        assert_eq!(l.runs[1].style.bg, Some(Color::Indexed(21)));
        let l = one("\u{1b}[38;2;16;160;240mrgb\u{1b}[48;2;1;2;3mX");
        assert_eq!(l.runs[0].style.fg, Some(Color::Rgb(16, 160, 240)));
        assert_eq!(l.runs[1].style.bg, Some(Color::Rgb(1, 2, 3)));
    }

    #[test]
    fn an_empty_sgr_parameter_list_is_a_reset() {
        // `\e[m` — what `git` closes every span with — means SGR 0.
        let l = one("\u{1b}[32mgreen\u{1b}[m plain");
        assert_eq!(l.text, "green plain");
        assert_eq!(runs(&l), [("green", Some(Color::Indexed(2)))]);
    }

    #[test]
    fn an_unsupported_sgr_code_is_ignored_without_losing_its_text() {
        let l = one("\u{1b}[53mframed\u{1b}[55m after");
        assert_eq!(l.text, "framed after", "the content is never the casualty");
        assert!(l.runs.is_empty(), "and nothing was invented for it");
    }

    #[test]
    fn underline_colour_does_not_become_foreground_colour() {
        // 58 is "underline colour". Reading it as 38 would tint the text — a
        // wrong answer delivered in the right place, and the hardest kind to spot.
        let l = one("\u{1b}[58;5;196mX");
        assert_eq!(l.text, "X");
        assert!(l.runs.is_empty());
    }

    #[test]
    fn colon_subparameters_are_dropped_rather_than_guessed_at() {
        // `:` introduces sub-parameters we do not interpret. Applying the
        // leading `38` as if the tail were a normal `;` list is a guess at a
        // sequence we just admitted we cannot read.
        let l = one("a\u{1b}[38:5:123mb");
        assert_eq!(l.text, "ab");
        assert!(l.runs.is_empty());
    }

    #[test]
    fn sgr_state_carries_across_a_newline_the_way_a_terminals_does() {
        let ls = resolve(&["\u{1b}[32mpart one\npart two\u{1b}[0m\n"]);
        assert_eq!(
            ls[1].runs[0].style.fg,
            Some(Color::Indexed(2)),
            "nothing reset the style at the line break, so line two is still green"
        );
    }

    #[test]
    fn git_diffs_reopened_green_merges_into_one_run_per_span() {
        // The shape of `git diff --color` (fixtures/shell_output/git-diff.raw):
        // the same green closed and reopened around every token. Un-merged, one
        // added line arrives as a dozen spans.
        let l = one("\u{1b}[32m+\u{1b}[0m \u{1b}[32m+added line\u{1b}[0m");
        assert_eq!(l.text, "+ +added line");
        assert_eq!(
            runs(&l),
            [
                ("+", Some(Color::Indexed(2))),
                ("+added line", Some(Color::Indexed(2))),
            ],
            "the unstyled middle is real, so two runs — but each one merged"
        );
    }

    #[test]
    fn adjacent_runs_of_equal_style_merge() {
        let mut r = Vec::new();
        push_run(&mut r, Style::new().red(), 0, 3);
        push_run(&mut r, Style::new().red(), 3, 6);
        assert_eq!(r.len(), 1, "one run, not two that mean the same thing");
        assert_eq!(r[0].end, 6);
    }

    #[test]
    fn a_default_style_is_never_recorded_as_a_run() {
        assert!(one("just text").runs.is_empty(), "no style said anything");
    }

    // ─────────── Q2: `\r` and `\b` resolve inside the line ───────────

    #[test]
    fn carriage_return_returns_to_the_start_and_overwrites_in_place() {
        assert_eq!(one("abc\rXY").text, "XYc", "\r is not end-of-line");
    }

    #[test]
    fn a_repaint_shorter_than_what_it_replaces_keeps_the_tail_it_did_not_erase() {
        // Exactly what the terminal did. `EL` (next test) is how a program says
        // otherwise; we are not allowed to tidy up behind one that did not ask.
        assert_eq!(one("AAAA\rBB").text, "BBAA");
    }

    #[test]
    fn erase_in_line_covers_the_tail_a_repaint_left() {
        assert_eq!(one("AAAA\r\u{1b}[KBB").text, "BB", "EL 0: cursor → end");
        assert_eq!(
            one("AA\u{1b}[1Kx").text,
            "  x",
            "EL 1 clears start..=cursor, and the cursor was at column 2, so the write lands at column 2"
        );
        assert_eq!(one("AAB\u{8}\u{1b}[Kz").text, "AAz");
    }

    #[test]
    fn erase_whole_line_clears_it_without_ending_it() {
        let mut r = LineResolver::new();
        r.feed("abcde");
        r.feed("\u{1b}[2K");
        let pending = r
            .pending()
            .expect("EL 2 clears the line, it does not end it");
        assert_eq!(
            pending.text, "     ",
            "cleared to blanks, cursor still at 5"
        );
        r.feed("X");
        assert_eq!(r.take_pending().unwrap().text, "     X");
    }

    #[test]
    fn a_curl_style_bar_becomes_its_last_frame_instead_of_a_hundred_frames() {
        // The failure mode this ticket exists for: with escapes deleted rather
        // than resolved, every frame of a progress bar concatenates into one
        // long ###---###---###--- line.
        let mut s = String::new();
        for p in 0..=10 {
            s.push('\r');
            s.push_str(&format!("[{:#<8}] {:>4}%", "#".repeat(p), p * 10));
        }
        s.push_str("\r\u{1b}[Kdone\n");
        let ls = texts(&s);
        assert_eq!(ls.len(), 1, "one line, because the child wrote one line");
        assert_eq!(ls[0], "done", "and it ended as the last thing it said");
    }

    #[test]
    fn backspace_lands_on_the_cell_to_the_left_and_overwrites_it() {
        assert_eq!(one("abc\u{8}X").text, "abX");
        assert_eq!(one("abc\u{8}\u{8}X").text, "aXc");
    }

    #[test]
    fn backspace_cannot_walk_off_the_left_edge() {
        assert_eq!(one("abc\u{8}\u{8}\u{8}\u{8}\u{8}X").text, "Xbc");
    }

    #[test]
    fn with_a_wrap_width_carriage_return_means_this_row_not_this_line() {
        // wrap=10: the child wrapped, so `\r` is the terminal's "start of this
        // row", and the overwrite lands at cell 10 — not at cell 0.
        assert_eq!(wrapped(10, "0123456789ab\rX"), ["0123456789Xb"]);
    }

    #[test]
    fn with_a_wrap_width_backspace_stops_at_the_left_edge_of_its_row() {
        // The cursor is at cell 10 — the left edge of row 2 — and backspace has
        // nowhere to go from there, exactly as it has nowhere to go at column 0.
        // The `X` lands where the cursor is; the previous row is untouched.
        assert_eq!(wrapped(10, "0123456789\u{8}X"), ["0123456789X"]);
    }

    #[test]
    fn without_a_wrap_width_carriage_return_means_the_whole_line() {
        // The fallback is deliberately the boring one: no width, no row maths.
        assert_eq!(texts("0123456789ab\rX"), ["X123456789ab"]);
    }

    // ─────────── Q3: tabs expand, in cells, to a stop ───────────

    #[test]
    fn a_tab_advances_to_the_next_stop_of_eight() {
        assert_eq!(one("a\tb").text, format!("a{}b", " ".repeat(7)));
        assert_eq!(
            one("12345678\tX").text,
            format!("12345678{}X", " ".repeat(8))
        );
        assert_eq!(
            one("1234567\tX").text,
            "1234567 X",
            "just past a stop: one blank"
        );
    }

    #[test]
    fn a_tab_leaves_blanks_not_a_tab_character() {
        // The stored line is control-free, so nothing downstream has to know
        // what a tab stop is, and a paste cannot re-flow the author's alignment
        // into somebody else's tab width.
        let l = one("\tone\ttwo");
        assert!(!l.text.contains('\t'), "{:?}", l.text);
        assert!(is_control_free(&l.text));
    }

    #[test]
    fn a_tab_never_runs_past_the_end_of_its_row() {
        // A 5-cell row, cursor at 3: the stop is 8, the row has 2 left, so a
        // tab buys 2 and stops.
        assert_eq!(wrapped(5, "abc\tX"), ["abc  X"]);
    }

    // ─────────── Q4: wide clusters, marks, and the two mappings ───────────

    #[test]
    fn a_wide_cluster_takes_two_cells_and_measures_two() {
        let l = one("漢字");
        assert_eq!(l.text, "漢字");
        assert_eq!(l.text.width(), 4, "two clusters, two cells each");
    }

    #[test]
    fn a_combining_mark_joins_the_cluster_to_its_left() {
        let l = one("e\u{301} done");
        assert_eq!(l.text, "e\u{301} done", "the mark is kept exactly");
        assert_eq!(l.text.width(), 6, "and takes no cell of its own");
    }

    #[test]
    fn a_zwj_sequence_is_one_cluster_and_measures_like_one() {
        // 👩‍👩‍👦 is a single joined cluster, not three emoji: `unicode_width`
        // measures the whole sequence as one 2-cell glyph, and because the
        // resolver measures with the same function the cluster it lays out is
        // the same 2 cells the renderer will count. The characters are exact,
        // and so is the layout.
        let seq = "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}";
        let l = one(&format!("{seq} tail"));
        assert!(l.text.starts_with(seq), "kept character for character");
        assert_eq!(l.cells, seq.width() + 5);
        assert_eq!(l.cells, 7, "the family is two cells, ' tail' five");
    }

    #[test]
    fn a_variation_selector_widens_the_cluster_it_joined() {
        // `☺` is one cell; `☺️` (with VS16) is two. The cluster is re-measured
        // when the selector arrives, and the cell it gained is paid for at the
        // cluster's own end — not borrowed from whatever came after.
        assert_eq!(one("\u{263a}").cells, 1);
        let l = one("\u{263a}\u{fe0f}");
        assert_eq!(l.text, "\u{263a}\u{fe0f}");
        assert_eq!(l.cells, 2);
        assert_eq!(one("x\u{263a}\u{fe0f}y").cells, 4);
    }

    #[test]
    fn a_skin_tone_modifier_joins_its_base_instead_of_standing_alone() {
        let l = one("\u{1f469}\u{1f3fd}");
        assert_eq!(l.text, "\u{1f469}\u{1f3fd}", "both codepoints kept");
        assert_eq!(l.cells, 2, "one emoji, two cells — not four");
    }

    #[test]
    fn a_flag_tag_sequence_joins_into_one_cluster() {
        // 🏴󠁧󠁢󠁥󠁮󠁧� is Wales: the flag glyph plus five tag codepoints.
        let flag = "\u{1f3f4}\u{e0067}\u{e0062}\u{e0065}\u{e0067}\u{e007f}";
        let l = one(flag);
        assert_eq!(l.text, flag);
        assert_eq!(l.cells, 2);
    }

    #[test]
    fn replacing_a_narrower_cluster_does_not_leave_the_old_cells_stranded() {
        // ☺️ took two cells; writing 'x' over its lead must not leave the second
        // cell as an orphaned continuation.
        let l = one("\u{263a}\u{fe0f}\rX");
        assert_eq!(l.text, "X ");
        assert_eq!(l.cells, 2);
    }

    #[test]
    fn overwriting_the_right_half_of_a_wide_cluster_blanks_its_lead() {
        let l = one("漢\u{8}X");
        assert_eq!(l.text, " X");
        assert_eq!(l.text.width(), 2, "the cell count is undisturbed");
    }

    #[test]
    fn replacing_a_wide_clusters_lead_with_a_narrow_one_blanks_its_tail() {
        assert_eq!(one("漢\rX").text, "X ");
        assert_eq!(one("漢字\rab").text, "ab字");
        assert_eq!(one("漢字\rab").text.width(), 4);
    }

    #[test]
    fn a_wide_cluster_overwrites_a_wide_cluster_exactly() {
        assert_eq!(one("漢\r字").text, "字");
    }

    #[test]
    fn a_zero_width_cluster_with_no_base_is_the_one_thing_a_line_can_lose() {
        // Priced in ADR-0005 Q4: a bare combining mark at column 0 cannot take
        // a cell without breaking the cells/measured-width invariant, and a cell
        // of bare accent carries no information.
        assert_eq!(one("\u{301}abc").text, "abc");
    }

    #[test]
    fn the_cells_a_line_takes_are_the_width_the_renderer_measures() {
        // The invariant, from outside the resolver, over the whole corpus. It is
        // what lets a wrap done in cells and a wrap done on `text` be the same
        // wrap: `unicode_width` is the only width function either side uses.
        for s in CORPUS {
            let l = one(s);
            assert_eq!(l.cells, l.text.width(), "cells vs measured width for {s:?}");
        }
    }

    // ─────────── the read boundary is not allowed to matter ───────────

    #[test]
    fn cutting_the_stream_one_character_at_a_time_changes_nothing() {
        for s in CORPUS {
            let whole = texts(s);
            let owned: Vec<String> = s.chars().map(|c| c.to_string()).collect();
            let chars: Vec<&str> = owned.iter().map(String::as_str).collect();
            let dribble = resolve(&chars)
                .iter()
                .map(|l| l.text.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                whole, dribble,
                "stream granularity changed the answer for {s:?}"
            );
        }
    }

    #[test]
    fn no_line_of_the_corpus_carries_a_control_character() {
        for s in CORPUS {
            for l in texts(s) {
                assert!(is_control_free(&l), "control character in {s:?} → {l:?}");
            }
        }
    }

    #[test]
    fn a_dangling_escape_sequence_holds_its_bytes_and_releases_them_whole() {
        let mut r = LineResolver::new();
        assert!(
            r.feed("abc\u{1b}[").is_empty(),
            "half a sequence is not a line, and `abc` is still open"
        );
        let lines = r.feed("31mred\u{1b}[0m\n");
        assert_eq!(lines[0].text, "abcred");
        assert_eq!(runs(&lines[0]), [("red", Some(Color::Indexed(1)))]);
    }

    #[test]
    fn reset_forgets_the_open_line_and_the_style_too() {
        let mut r = LineResolver::new();
        r.feed("\u{1b}[31mpartial");
        r.reset();
        assert!(r.pending().is_none(), "the dropped line stays dropped");
        let lines = r.feed("clean\n");
        assert_eq!(lines[0].text, "clean");
        assert!(
            lines[0].runs.is_empty(),
            "and the red from the dead stream went with it"
        );
    }

    // ─────────── the copy projection (ADR-0005 Q4) ───────────

    #[test]
    fn the_copy_text_is_the_line_without_its_layout_blanks() {
        // The blanks a tab expanded into, and the blank a half-overwritten wide
        // cluster leaves, are layout. What pastes is what the content said.
        assert_eq!(one("name\tvalue   ").copy_text(), "name    value");
        assert_eq!(one("tail\t").copy_text(), "tail");
        assert_eq!(
            one("漢\u{8}X").copy_text(),
            " X",
            "a *leading* blank is what the line says, so it is copied"
        );
    }

    #[test]
    fn styles_clip_into_spans_without_losing_the_text_between_them() {
        let l = one("a\u{1b}[31mb\u{1b}[0mc");
        let line = l.to_line();
        let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(joined, "abc", "every character is in exactly one span");
        assert_eq!(line.spans.len(), 3, "raw, styled, raw");
        assert_eq!(line.spans[1].style.fg, Some(Color::Indexed(1)));
    }

    // ─────────── real output, captured from real programs ───────────
    //
    // fixtures/shell_output/*.raw are committed byte-for-byte captures
    // (`git diff --color`, `ls --color=always`, a curl-shaped bar, tabs, CJK,
    // ZWJ emoji). They are here so the rules are tested against what programs
    // actually emit rather than against what this file's author imagined.

    fn fixture(name: &str) -> String {
        let path = format!(
            "{}/tests/fixtures/shell_output/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        // What the Bash session's reader hands up: bytes, lossy-decoded.
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn resolve_at(wrap: usize, raw: &str) -> Vec<StyledLine> {
        let mut r = LineResolver::new();
        r.set_wrap_width(wrap);
        let mut out = r.feed(raw);
        out.extend(r.take_pending());
        out
    }

    #[test]
    fn real_git_diff_keeps_its_green() {
        let lines = resolve_at(80, &fixture("git-diff.raw"));
        assert!(lines.len() > 4, "the fixture resolved to lines");
        let green_added = lines
            .iter()
            .filter(|l| l.text.starts_with('+'))
            .filter(|l| {
                l.runs
                    .iter()
                    .any(|r| r.style.fg == Some(Color::Indexed(2)) && r.end > r.start)
            })
            .count();
        assert!(
            green_added >= 1,
            "an added line kept its green: {:?}",
            lines
                .iter()
                .map(|l| (l.text.clone(), l.runs.len()))
                .collect::<Vec<_>>()
        );
        assert!(
            lines.iter().all(|l| is_control_free(&l.text)),
            "no escape bytes survived git's own output"
        );
    }

    #[test]
    fn real_ls_color_marks_its_directories_blue() {
        let lines = resolve_at(80, &fixture("ls-color.raw"));
        assert!(
            lines
                .iter()
                .any(|l| l.runs.iter().any(|r| r.style.fg == Some(Color::Indexed(4)))),
            "a directory kept its blue: {:?}",
            lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>()
        );
    }

    #[test]
    fn real_progress_frames_collapse_to_what_the_line_said_last() {
        let lines = resolve_at(80, &fixture("progress-wides.raw"));
        let done = lines
            .iter()
            .map(|l| l.text.clone())
            .filter(|t| t.contains("done"))
            .collect::<Vec<_>>();
        assert_eq!(done, ["done"], "no ###--- residue on the line");
    }

    #[test]
    fn real_tabs_land_on_the_stops_they_were_aligned_to() {
        let lines = resolve_at(80, &fixture("progress-wides.raw"));
        let named = lines
            .iter()
            .find(|l| l.text.starts_with("name"))
            .expect("the tab fixture line");
        assert_eq!(named.text, format!("name{}value", " ".repeat(4)));
        let long = lines
            .iter()
            .find(|l| l.text.starts_with("long-name"))
            .expect("the long-key tab fixture line");
        assert_eq!(long.text, format!("long-name{}x", " ".repeat(7)));
        let indented = lines
            .iter()
            .find(|l| l.text.starts_with(&" ".repeat(8)))
            .expect("the leading-tab fixture line");
        assert_eq!(indented.text, format!("{}indented", " ".repeat(8)));
    }

    #[test]
    fn real_wide_and_combined_text_is_kept_and_measured_right() {
        let lines = resolve_at(80, &fixture("progress-wides.raw"));
        let wide = lines
            .iter()
            .find(|l| l.text.contains("漢字"))
            .expect("the wide fixture line");
        assert!(wide.text.contains("組合せ"));
        assert_eq!(wide.cells, wide.text.width());
        let emoji = lines
            .iter()
            .find(|l| l.text.contains("\u{1f469}"))
            .expect("the emoji fixture line");
        assert!(
            emoji.text.contains("\u{1f469}\u{200d}\u{1f469}"),
            "the ZWJ sequence survived character for character"
        );
        assert!(
            emoji.text.contains("e\u{301}"),
            "and so did the decomposed e-acute"
        );
        assert_eq!(emoji.cells, emoji.text.width());
    }

    #[test]
    fn every_fixture_resolves_the_same_whole_or_one_character_at_a_time() {
        for name in ["git-diff.raw", "ls-color.raw", "progress-wides.raw"] {
            let raw = fixture(name);
            let whole = resolve_at(80, &raw);
            let dribble = {
                let mut r = LineResolver::new();
                r.set_wrap_width(80);
                let mut out = Vec::new();
                for c in raw.chars() {
                    out.extend(r.feed(&c.to_string()));
                }
                out.extend(r.take_pending());
                out
            };
            assert_eq!(
                whole.iter().map(|l| l.text.clone()).collect::<Vec<_>>(),
                dribble.iter().map(|l| l.text.clone()).collect::<Vec<_>>(),
                "{name}: the read boundary changed the text"
            );
            assert_eq!(whole, dribble, "{name}: …or the styles");
        }
    }
}
