//! The scrollable transcript store (looprs-pdl.6).
//!
//! Until this ticket the finalized lines were pushed *out* — `insert_before`
//! wrote them into the terminal's own scrollback above the pane and the app kept
//! only what was live. In the alternate screen there is no "out" any more
//! (ADR-0004 R1), so the transcript becomes the thing that is scrolled, and the
//! thing that is scrolled needs a store under it: the rendered rows, an offset,
//! and a follow-the-tail rule.
//!
//! # The shape
//!
//! [`Scrollback`] is a `Vec<DisplayRow>` plus scroll state. The rows come from
//! the existing [`Flusher`](crate::components::scrollback::Flusher) — the
//! markdown/fence/raw rendering stays exactly where it is — and this module adds
//! the three things the renderer knows but had nowhere to put:
//!
//! * **which entry** a row came from ([`DisplayRow::entry`]), so a selection can
//!   refuse to cross a mode boundary or select a card's chrome (looprs-pdl.9);
//! * **hard newline or our soft wrap** ([`RowEnd`]), so joining rows back into
//!   text inserts nothing for a continuation and exactly one `\n` between two
//!   logical lines (ADR-0004 R14) — see [`paste_text`];
//! * **the cell→character map** of the row ([`CellMap`]), so a cell range can be
//!   snapped to whole clusters and never emits half a CJK glyph or half a
//!   joined emoji sequence.
//!
//! The row keeps its styles rather than stripping them (ADR-0005 chose *decode
//! SGR*, not *strip*), which is why a row carries a `Line` and not a `String`;
//! the copy path drops the presentation on the way out, exactly as ADR-0004 R14
//! says it may.
//!
//! # Scroll state
//!
//! Three fields and one rule, all of them user-visible:
//!
//! * `offset` — rows hanging between the bottom of the view and the tail. `0`
//!   *is* pinned; there is no second "pinned" notion to keep in step with it,
//!   only the cached boolean that [`Scrollback::set_offset`] derives from it.
//! * `pinned` — new output appends and the view follows.
//! * `pending` — rows that arrived while unpinned, so the "N new" affordance can
//!   say so out loud rather than let the user take a held view for the tail.
//!
//! # Re-wrapping on resize
//!
//! A resize re-renders the transcript at the new width and *replaces* the rows
//! ([`Scrollback::rewrap`]). What makes that usable instead of unusable is that
//! the view is anchored to **content**: [`ContentAnchor`] addresses the row the
//! view rests on by `(entry, logical line, byte)`, which means the same thing
//! before and after the re-wrap, while a row index means nothing across it.
//! See [`Scrollback::resting_anchor`] for why the anchor is the bottom-most
//! visible row rather than the top-most.

use std::fmt;

use ratatui::text::Line;

use crate::components::scrollback::RenderedRow;
use crate::utils::shelltext;

/// How the text after this row continues.
///
/// The one bit a wrapped display row cannot be without: it is what tells the
/// copy path (ADR-0004 R14) that two soft-wrapped rows are one line and that
/// two hard-ended rows are two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowEnd {
    /// The source had a newline here. Joining to the next row inserts exactly
    /// one `\n`.
    Hard,
    /// A continuation of **our** soft wrap. Joining to the next row inserts
    /// nothing — the space the wrap broke on is already gone, and inventing one
    /// welds two words together.
    Soft,
}

/// How many display rows a view keeps by default.
///
/// "Several screens' worth at every window size we support", which is about the
/// most a human scrolls through in one sitting and small enough that the
/// rendered form of a long run is measured in hundreds of kilobytes rather than
/// in megabytes. The *text* is capped separately, per view, by the buffer cap in
/// [`crate::session::view`]; this caps the rendered rows, which are worth
/// several allocations per cell of text.
pub const DEFAULT_MAX_ROWS: usize = 4096;

/// One row of the transcript as it is displayed.
#[derive(Clone, Debug)]
pub struct DisplayRow {
    /// Index of the transcript entry this row was rendered from.
    ///
    /// Meaningful against the transcript the row was rendered *from*, and
    /// re-based when that transcript is compacted
    /// ([`Scrollback::entries_evicted`]) — which is the one way it can go
    /// stale, and the one way it is kept honest.
    pub entry: usize,
    /// Which logical line within that entry (0-based).
    ///
    /// A logical line is a run of rows ending in [`RowEnd::Hard`]: the unit the
    /// source wrote, as opposed to the rows our wrap cut it into.
    pub logical: usize,
    /// Byte offset, within that logical line's rendered text, of this row's
    /// first character. `0` for the first row of a logical line.
    pub start: usize,
    /// Hard newline vs. our soft wrap.
    ///
    /// The flag ADR-0004 R14 is stated in: joining two soft rows must insert
    /// nothing and two logical lines exactly one `\n`.
    #[allow(dead_code)]
    // consumer: `paste_text` below (select-to-copy, looprs-pdl.10); nothing in this ticket's draw path reads it
    pub end: RowEnd,
    /// The rendered line, styles included.
    pub line: Line<'static>,
    /// Where this row's clusters land in cells.
    #[allow(dead_code)] // consumer: the drag hit-test (looprs-pdl.9); see `CellMap`
    pub cells: CellMap,
}

impl DisplayRow {
    fn new(entry: usize, logical: usize, start: usize, end: RowEnd, line: Line<'static>) -> Self {
        let cells = CellMap::of(&plain(&line));
        Self {
            entry,
            logical,
            start,
            end,
            line,
            cells,
        }
    }

    /// Does the given row come from a different entry than this one?
    ///
    /// The test a drag selection needs first: a selection that crosses an entry
    /// boundary crosses a mode boundary, and ADR-0004 R16 says it does not
    /// happen.
    #[allow(dead_code)] // consumer: the drag hit-test (looprs-pdl.9), which is the reason the row carries a cell map at all
    pub fn crosses(&self, other: &DisplayRow) -> bool {
        self.entry != other.entry
    }

    /// This row as content-addressed scroll state.
    pub fn anchor(&self) -> ContentAnchor {
        ContentAnchor {
            entry: self.entry,
            logical: self.logical,
            byte: self.start,
        }
    }
}

/// `Display` is the row's plain text — the string ratatui would have printed,
/// minus the styling. Callers that want the copy value use [`paste_text`], which
/// is the rule; this is the plumbing that several tests read the row through.
impl fmt::Display for DisplayRow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for span in &self.line.spans {
            f.write_str(&span.content)?;
        }
        Ok(())
    }
}

/// The value of a run of rows when it is pasted (ADR-0004 R14, R15).
///
/// Soft-wrapped rows join with **nothing**; a hard end contributes **exactly one
/// `\n`**; the trailing newline is dropped, because a paste that ends in a blank
/// line is a paste that pressed Enter for you.
///
/// The wrap never reaches the paste. Neither do styles: they live on the row's
/// `Line` and are simply not read here — the "the copy path drops them on the
/// way out" half of the ADR-0005 decision — and ADR-0005 rule 6 is what makes
/// "no control bytes can reach the paste" true rather than hoped for.
#[allow(dead_code)] // consumer: select-to-copy (looprs-pdl.10); the rule lives here so "what you copy" has exactly one definition in the tree, as `StyledLine::copy_text` does
pub fn paste_text<'a>(rows: impl IntoIterator<Item = &'a DisplayRow>) -> String {
    let mut out = String::new();
    for row in rows {
        for span in &row.line.spans {
            out.push_str(&span.content);
        }
        if row.end == RowEnd::Hard {
            out.push('\n');
        }
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// The content the view rests on, addressed so that it survives a re-wrap.
///
/// A row *index* cannot be the anchor: the whole point of re-wrapping is that
/// the row a piece of content lands on changes. `(entry, logical line, byte)`
/// says "this character", which says the same thing at every width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentAnchor {
    pub entry: usize,
    pub logical: usize,
    pub byte: usize,
}

/// One cluster: its cell span and its byte span in the row's text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellSpan {
    pub cell: usize,
    pub cells: usize,
    pub start: usize,
    pub end: usize,
}

/// Where every cluster of a display row sits, in cells.
///
/// Built from the row's own text with [`shelltext::clusters`] — the resolver's
/// cluster rule, deliberately reused rather than re-implemented, because two
/// rules is how a selection ends up cutting a glyph in half: the wrap lays the
/// row out with one idea of what a character is and the hit-test uses another.
///
/// The map is total over the row's text (its clusters partition it), and every
/// lookup answers with a *whole* cluster: cell 1 of a double-width glyph answers
/// with that glyph, not with its right-hand half.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellMap {
    spans: Vec<CellSpan>,
    cells: usize,
}

impl CellMap {
    /// The cell map of one row's text.
    pub fn of(text: &str) -> Self {
        let mut spans = Vec::new();
        let mut cell = 0usize;
        for c in shelltext::clusters(text) {
            spans.push(CellSpan {
                cell,
                cells: c.cells,
                start: c.start,
                end: c.end,
            });
            cell += c.cells;
        }
        Self { spans, cells: cell }
    }

    /// How many cells the row occupies.
    #[allow(dead_code)] // consumer: looprs-pdl.9 (a drag cannot extend past the row's last cell); read by the cell-map tests meanwhile
    pub fn cells(&self) -> usize {
        self.cells
    }

    #[allow(dead_code)] // consumer: looprs-pdl.9; a row with no clusters occupies no cell, which is the difference between "empty row" and "row of one blank"
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The cluster that owns `cell`, or `None` past the end of the row.
    #[allow(dead_code)] // consumer: looprs-pdl.9; also the body under `bytes_at`/`snap_bytes` below
    pub fn at(&self, cell: usize) -> Option<&CellSpan> {
        // Clusters are contiguous in cells, so the answer is the last cluster
        // starting at or before `cell`, and it owns `cell` by construction.
        let i = self.spans.partition_point(|s| s.cell <= cell);
        let s = i.checked_sub(1).map(|i| &self.spans[i])?;
        (cell < s.cell + s.cells).then_some(s)
    }

    /// Byte range of the whole cluster under `cell`.
    #[allow(dead_code)] // consumer: looprs-pdl.9 (a click selects a whole character, never half a wide glyph)
    pub fn bytes_at(&self, cell: usize) -> Option<(usize, usize)> {
        self.at(cell).map(|s| (s.start, s.end))
    }

    /// Snap the cell range `[from, to)` out to whole clusters and return the
    /// bytes that covers.
    ///
    /// The snapping is this type's whole function: a drag that starts on the
    /// second cell of a wide glyph starts at that glyph's *first* byte, and one
    /// that ends inside a ZWJ sequence ends at the sequence's end. An
    /// un-snapped range is not a slightly worse selection, it is a corrupted
    /// string.
    #[allow(dead_code)] // consumer: looprs-pdl.9 (a cell range snapped to whole clusters, so no ZWJ sequence is ever cut in half)
    pub fn snap_bytes(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        if to <= from {
            return None;
        }
        let start = self.at(from)?.start;
        let end = self.at(to.saturating_sub(1))?.end;
        Some((start, end))
    }

    /// The cell the given byte offset is drawn in.
    ///
    /// The inverse of [`Self::bytes_at`]: the store needs both directions to keep
    /// a re-wrap honest — content addressing walks byte→row, and a
    /// column-preserving anchor needs byte→cell.
    #[allow(dead_code)] // consumer: looprs-pdl.9; the round trip is checked by the cell-map tests meanwhile
    pub fn cell_of_byte(&self, byte: usize) -> Option<usize> {
        let i = self.spans.partition_point(|s| s.start <= byte);
        let s = i.checked_sub(1).map(|i| &self.spans[i])?;
        (byte < s.end).then_some(s.cell)
    }
}

/// The store: rendered rows plus the scroll state over them.
#[derive(Debug)]
pub struct Scrollback {
    rows: Vec<DisplayRow>,
    /// The width these rows were wrapped at.
    width: u16,
    /// Rows hanging between the bottom of the view and the tail. `0` is pinned.
    offset: usize,
    /// `offset == 0`, cached. Written only by [`Self::set_offset`], so the two
    /// cannot disagree.
    pinned: bool,
    /// Rows that arrived while unpinned.
    pending: usize,
    max_rows: usize,
    /// Rows this cap has thrown away, so the trim is a counted thing rather than
    /// an invisible one (looprs-pdl.7 reads it).
    dropped: usize,
}

impl Scrollback {
    pub fn new(width: u16) -> Self {
        Self::with_rows(width, DEFAULT_MAX_ROWS)
    }

    /// As [`Self::new`], with an explicit row cap (`0` = unbounded — the same
    /// convention the byte cap in [`crate::session::view`] uses).
    pub fn with_rows(width: u16, max_rows: usize) -> Self {
        Self {
            rows: Vec::new(),
            width,
            offset: 0,
            pinned: true,
            pending: 0,
            max_rows,
            dropped: 0,
        }
    }

    /// The width the current rows were wrapped at. Differing from the window's
    /// is the signal that a [`Self::rewrap`] is owed.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Every row in the store, oldest first.
    #[allow(dead_code)] // consumer: looprs-pdl.9 (a selection spans rows); the store's own tests read it meanwhile
    pub fn rows(&self) -> &[DisplayRow] {
        &self.rows
    }

    #[allow(dead_code)] // companion to `is_empty`; see `rows`
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[allow(dead_code)] // companion to `len`; a view with no rows cannot be scrolled, only followed
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Is the view following the tail?
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Rows between the bottom of the view and the tail.
    #[allow(dead_code)] // consumer: looprs-pdl.8 (a wheel step is a delta against this); `resting_anchor` and the tests read it meanwhile
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// Rows that arrived since the view last looked at the tail.
    pub fn pending(&self) -> usize {
        self.pending
    }

    /// Is there new content the user is not looking at? The "N new" affordance
    /// reads exactly this.
    pub fn shows_new(&self) -> bool {
        !self.pinned && self.pending > 0
    }

    /// Rows the [`Self::max_rows`] cap has dropped.
    #[allow(dead_code)] // consumer: looprs-pdl.7 (bounded scrollback reports what it trimmed); the row-cap test reads it meanwhile
    pub fn dropped_rows(&self) -> usize {
        self.dropped
    }

    /// Append rows that just became final.
    ///
    /// Pinned: the view follows, because the tail moved and `offset` did not.
    /// Unpinned: the view holds, for the same reason — `offset` is measured
    /// from the tail, so holding it holds the *content* — and the rows are
    /// counted, so the affordance can tell the user a held view is not up to
    /// date.
    pub fn push(&mut self, rows: Vec<RenderedRow>) {
        if rows.is_empty() {
            return;
        }
        if !self.pinned {
            self.pending += rows.len();
            // The new rows hang *below* the view, so holding the view means the
            // offset grows with them. Hold the offset instead and the view rides
            // the tail, which is exactly the thing the user said they did not
            // want when they scrolled up.
            self.offset += rows.len();
        }
        self.rows.extend(rows_from_rendered(rows));
        self.trim();
    }

    /// Replace every row with the same content re-wrapped at `width`.
    ///
    /// The view stays where the *content* was, not where the row index was:
    /// [`Self::resting_anchor`] is taken before the swap and put back after it.
    /// A pinned view has nothing to anchor — it is pinned to the tail, and the
    /// tail is wherever the content ends.
    pub fn rewrap(&mut self, width: u16, rendered: Vec<RenderedRow>) {
        let anchor = self.resting_anchor();
        self.width = width;
        self.rows = rows_from_rendered(rendered);
        self.trim();
        if self.pinned {
            self.offset = 0;
            return;
        }
        match anchor.and_then(|a| self.index_of_anchor(&a)) {
            Some(i) => self.set_offset(self.rows.len().saturating_sub(i + 1)),
            // The anchored content is gone — its entry was evicted while the
            // user was looking away. There is no honest way to "keep" a position
            // whose content left, so hold as close as the new list allows rather
            // than teleport to the tail, which is the one thing not asked for.
            None => self.set_offset(self.offset),
        }
    }

    /// The content the view is resting on: the **bottom-most visible row**.
    ///
    /// Bottom-most rather than top-most because it is the anchor that needs
    /// nothing else remembered. `offset` counts rows hanging *below* the view,
    /// so the row the view ends on is `len - offset - 1`; anchoring the top row
    /// would need the band height, which lives in the frame and not here.
    ///
    /// It is also the anchor that behaves while a window is being dragged: the
    /// rows a narrowing re-wrap inserts appear *above* the anchor, so the
    /// content slides up out of the top of the band exactly as it would have
    /// anyway, and nothing moves out from under the bottom edge — the edge the
    /// "N new" affordance and the chrome are measured from.
    pub fn resting_anchor(&self) -> Option<ContentAnchor> {
        if self.pinned || self.rows.is_empty() {
            return None;
        }
        let end = self.rows.len().saturating_sub(self.offset);
        let idx = end.saturating_sub(1).min(self.rows.len() - 1);
        Some(self.rows[idx].anchor())
    }

    /// The row index a content anchor points at now, at this width.
    fn index_of_anchor(&self, a: &ContentAnchor) -> Option<usize> {
        // The last row of that logical line that starts at or before the
        // anchored byte: that is the row the anchored character is drawn on.
        let mut first = None;
        let mut last = None;
        for (i, r) in self.rows.iter().enumerate() {
            if r.entry != a.entry || r.logical != a.logical {
                continue;
            }
            if first.is_none() {
                first = Some(i);
            }
            if r.start <= a.byte {
                last = Some(i);
            }
        }
        last.or(first)
    }

    /// Move the view by `rows`: **positive toward the tail** (down), **negative
    /// into history** (up).
    ///
    /// The sign is the direction the *view* travels, not the direction `offset`
    /// moves — a page down is `+page`, a wheel-toward-the-past is negative.
    /// `visible` is the height of the band being scrolled; it sets how far up
    /// there is anything to see.
    pub fn scroll_by(&mut self, rows: isize, visible: usize) {
        let max = self.max_scroll(visible);
        let cur = self.offset.min(max) as isize;
        self.set_offset((cur - rows).clamp(0, max as isize) as usize);
    }

    /// The very bottom. Always legal, always pins.
    pub fn scroll_to_tail(&mut self) {
        self.set_offset(0);
    }

    /// As far up as there is content.
    pub fn scroll_to_top(&mut self, visible: usize) {
        let max = self.max_scroll(visible);
        self.set_offset(max);
    }

    /// The largest offset that still shows something.
    pub fn max_scroll(&self, visible: usize) -> usize {
        self.rows.len().saturating_sub(visible.max(1))
    }

    fn set_offset(&mut self, offset: usize) {
        // Never past the head of the content: an offset of `len` shows nothing at
        // all, and "nothing at all" is not a place a view can be.
        self.offset = offset.min(self.rows.len().saturating_sub(1));
        // One rule for both halves of the interaction: being at the very bottom
        // *is* being pinned, and being off it *is* not. Scrolling up one row
        // unpins, and a keystroke at the bottom re-pins, because both come
        // through here — not because two places each remembered their half.
        self.pinned = self.offset == 0;
        if self.pinned {
            self.pending = 0;
        }
    }

    /// The rows to draw for a band `visible` rows tall.
    ///
    /// The slice is `visible` rows long whenever there are that many above the
    /// offset, and shorter only because the transcript ran out — the frame pads
    /// the rest; it does not scroll past the top of the content.
    pub fn window(&self, visible: usize) -> &[DisplayRow] {
        if visible == 0 || self.rows.is_empty() {
            return &[];
        }
        let end = self.rows.len().saturating_sub(self.offset);
        if end == 0 {
            return &[];
        }
        let start = end.saturating_sub(visible);
        &self.rows[start..end]
    }

    /// The transcript was compacted: `removed` entries came off the front and a
    /// one-line notice was inserted at `notice_at`. Follow the indices.
    ///
    /// A row whose entry is gone cannot be re-rendered, and a store that cannot
    /// re-render a row must not keep showing it: the next resize would drop it
    /// silently, and a scrollback that loses content only when the window moves
    /// is the worst kind of scrollback. So those rows go here, visibly, at the
    /// same moment their entries do — and the rows that survive are renumbered,
    /// so `entry` still means *this* entry and not whatever now occupies the
    /// number it used to have.
    pub fn entries_evicted(&mut self, removed: usize, notice_at: usize) {
        if removed == 0 {
            return;
        }
        let map = |old: usize| -> Option<usize> {
            if old < removed {
                return None;
            }
            let shifted = old - removed;
            // The notice took a slot at `notice_at`, so everything from there on
            // is one further along than the bare shift says.
            Some(if shifted >= notice_at {
                shifted + 1
            } else {
                shifted
            })
        };
        // Rows are in entry order, so the rows to drop are exactly the leading
        // run whose entries are gone.
        let cut = self
            .rows
            .iter()
            .take_while(|r| map(r.entry).is_none())
            .count();
        self.rows.drain(..cut);
        for r in self.rows.iter_mut() {
            if let Some(n) = map(r.entry) {
                r.entry = n;
            }
        }
    }

    fn trim(&mut self) {
        if self.max_rows == 0 || self.rows.len() <= self.max_rows {
            return;
        }
        let over = self.rows.len() - self.max_rows;
        self.rows.drain(..over);
        self.dropped += over;
        // Nothing to do to `offset`: measured from the tail, so dropping rows off
        // the front moves the content and leaves the view alone. A view anchored
        // on a row that was just trimmed is re-found by `index_of_anchor` on the
        // next re-wrap, which fails gently — see [`Self::rewrap`].
    }
}

/// Turn the flusher's lines into display rows.
///
/// `logical` and `start` fall straight out of the hard/soft chain the renderer
/// already set: a row that is *not* a soft continuation starts a logical line,
/// so counting hard ends across an entry counts its logical lines, and the bytes
/// in between are the offsets inside each one. Nothing extra is asked of the
/// renderer, which is the point — a second bookkeeping of the wrap would be a
/// second opinion about the wrap.
pub fn rows_from_rendered(rendered: Vec<RenderedRow>) -> Vec<DisplayRow> {
    let mut out = Vec::with_capacity(rendered.len());
    let mut entry = usize::MAX;
    let mut logical = 0usize;
    let mut start = 0usize;
    for r in rendered {
        if r.entry != entry {
            entry = r.entry;
            logical = 0;
            start = 0;
        }
        // The row's own text, measured once: the cell map is built from it and
        // the running byte offset has to agree with what the cell map saw.
        let len = plain(&r.line).len();
        let row = DisplayRow::new(entry, logical, start, r.end, r.line);
        match r.end {
            RowEnd::Hard => {
                logical += 1;
                start = 0;
            }
            RowEnd::Soft => start += len,
        }
        out.push(row);
    }
    out
}

/// The row's text, styling dropped — see [`crate::utils::render::plain`], which
/// is the one definition both this type and `RenderedRow` print through.
fn plain(line: &Line<'_>) -> String {
    crate::utils::render::plain(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn row(entry: usize, end: RowEnd, text: &str) -> RenderedRow {
        RenderedRow {
            entry,
            end,
            line: Line::from(text.to_string()),
        }
    }

    fn hard(entry: usize, text: &str) -> RenderedRow {
        row(entry, RowEnd::Hard, text)
    }

    fn soft(entry: usize, text: &str) -> RenderedRow {
        row(entry, RowEnd::Soft, text)
    }

    fn lines(rows: &[DisplayRow]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    /// The default state, and the one that must not be surprising: you open on
    /// the tail and you stay there while the session talks.
    #[test]
    fn pinned_is_the_default_and_new_output_follows() {
        let mut s = Scrollback::new(40);
        assert!(s.is_pinned(), "the app opens pinned to the tail");
        s.push(vec![hard(0, "one"), hard(0, "two")]);
        assert!(s.is_pinned(), "and stays pinned while output appends");
        assert_eq!(s.pending(), 0, "nothing is pending while pinned");
        assert_eq!(lines(s.window(2)), vec!["one", "two"]);

        s.push(vec![hard(1, "three")]);
        assert_eq!(
            lines(s.window(2)),
            vec!["two", "three"],
            "and followed again"
        );
    }

    /// The user said "I am reading something else" by moving the view by a
    /// single row. That is the whole interaction, so it is a test.
    #[test]
    fn scrolling_up_one_row_unpins() {
        let mut s = Scrollback::new(40);
        s.push((0..10).map(|i| hard(0, &format!("line {i}"))).collect());
        assert!(s.is_pinned());
        s.scroll_by(-1, 4);
        assert!(!s.is_pinned(), "one row up is enough to unpin");
        assert_eq!(s.offset(), 1);
    }

    /// The property the unpinned half exists for: the view holds its *content*
    /// while the tail runs away from it, and says how far away.
    #[test]
    fn unpinned_holds_its_content_and_counts_what_arrived() {
        let mut s = Scrollback::new(40);
        s.push((0..6).map(|i| hard(0, &format!("old {i}"))).collect());
        s.scroll_by(-3, 2);
        let before = lines(s.window(2));
        assert_eq!(before, vec!["old 1", "old 2"]);

        s.push(vec![hard(1, "new 1"), hard(1, "new 2")]);
        assert_eq!(
            lines(s.window(2)),
            before,
            "the view held exactly where it was looking"
        );
        assert_eq!(s.pending(), 2, "and knows how much went by");
        assert!(s.shows_new(), "and is ready to say so");
    }

    /// Both halves of the tail rule, in one place: reaching the bottom re-pins,
    /// and re-pinning clears the count, because there is nothing left unseen.
    #[test]
    fn reaching_the_very_bottom_re_pins_and_clears_the_count() {
        let mut s = Scrollback::new(40);
        s.push((0..8).map(|i| hard(0, &format!("a{i}"))).collect());
        s.scroll_by(-4, 2);
        assert_eq!(lines(s.window(2)), ["a2", "a3"]);

        s.push(vec![hard(1, "b")]);
        assert_eq!(
            lines(s.window(2)),
            ["a2", "a3"],
            "a row arriving at the tail does not move what the user is reading"
        );
        assert_eq!(s.pending(), 1);

        s.scroll_by(1, 2);
        assert_eq!(lines(s.window(2)), ["a3", "a4"], "one row toward the tail");
        assert_eq!(
            s.pending(),
            1,
            "still off the tail: still holding the count"
        );

        s.scroll_to_tail();
        assert!(s.is_pinned(), "the tail re-pins");
        assert_eq!(s.pending(), 0, "and the count is answered");
        assert_eq!(lines(s.window(2)), ["a7", "b"]);
    }

    /// You cannot scroll into blank space above the transcript: the top of the
    /// content is the top of the range.
    #[test]
    fn cannot_scroll_past_the_top_of_the_content() {
        let mut s = Scrollback::new(40);
        s.push((0..10).map(|i| hard(0, &format!("l{i}"))).collect());
        s.scroll_by(-1000, 4);
        assert_eq!(s.offset(), 6, "10 rows, 4 visible, 6 rows of scrollback");
        assert_eq!(lines(s.window(4)), ["l0", "l1", "l2", "l3"]);

        // A transcript shorter than the band cannot be scrolled at all.
        let mut small = Scrollback::new(40);
        small.push(vec![hard(0, "only")]);
        small.scroll_by(-5, 10);
        assert_eq!(small.offset(), 0, "nothing above to scroll to");
        assert!(small.is_pinned(), "and so it never left the tail");
    }

    /// Provenance per row: entry, logical line within the entry, and the byte
    /// offset inside that logical line. Three later tickets ask all three
    /// questions, and none of them can reconstruct the answers afterwards.
    #[test]
    fn every_row_carries_its_provenance() {
        let mut s = Scrollback::new(40);
        s.push(vec![
            soft(0, "first logical,"),
            hard(0, "wrapped twice"),
            hard(0, "second line"),
            hard(1, "another entry"),
        ]);
        let r = s.rows();
        assert_eq!(
            r.iter()
                .map(|r| (r.entry, r.logical, r.start))
                .collect::<Vec<_>>(),
            vec![(0, 0, 0), (0, 0, 14), (0, 1, 0), (1, 0, 0)],
            "the soft continuation is the second half of logical 0; the next hard \
             row is a new logical line; the entry boundary is honoured"
        );
        assert!(r[2].crosses(&r[3]), "the entry boundary is visible");
        assert!(!r[0].crosses(&r[1]), "a soft wrap is not a boundary");
    }

    /// ADR-0004 R14, spelled as the trap it exists to close.
    #[test]
    fn joining_rows_follows_the_hard_soft_rule() {
        let rows = rows_from_rendered(vec![
            soft(0, "The quick brown fox "),
            hard(0, "jumps over"),
            hard(0, "the lazy dog"),
        ]);
        assert_eq!(
            paste_text(&rows),
            "The quick brown fox jumps over\nthe lazy dog",
            "soft rows join with nothing, a hard end adds exactly one \\n, and the \
             trailing newline is not pasted"
        );
        // The wrong join, which is what "glue the display rows together"
        // produces, is not what this returns.
        let welded: String = rows.iter().map(|r| r.to_string()).collect();
        assert_eq!(welded, "The quick brown fox jumps overthe lazy dog");
    }

    /// The cell-to-character mapping: a cell is never the half of something.
    #[test]
    fn the_cell_map_never_splits_a_cluster() {
        // 日本語: three clusters, two cells each.
        let m = CellMap::of("日本語");
        assert_eq!(m.cells(), 6);
        // Cell 1 is the *right half* of 日. There is no such thing to select.
        assert_eq!(m.bytes_at(1), Some((0, 3)));
        assert_eq!(m.bytes_at(0), Some((0, 3)));
        assert_eq!(m.bytes_at(2), Some((3, 6)));
        assert_eq!(m.bytes_at(6), None, "past the end of the row");

        assert_eq!(m.snap_bytes(1, 5), Some((0, 9)));
        assert_eq!(m.snap_bytes(3, 4), Some((3, 6)), "exactly one glyph");
        assert_eq!(
            m.snap_bytes(2, 2),
            None,
            "an empty cell range selects nothing"
        );

        // A ZWJ sequence is ONE cluster: 5 chars, 2 cells, 18 bytes.
        let fam = CellMap::of("\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466} tail");
        assert_eq!(fam.cells(), 7, "the family is 2 cells, ' tail' is 5");
        assert_eq!(
            fam.bytes_at(1),
            Some((0, 18)),
            "the second cell of the family is still the whole family"
        );
        assert_eq!(fam.bytes_at(2), Some((18, 19)));
        assert_eq!(fam.snap_bytes(0, 2), Some((0, 18)));
        assert_eq!(fam.snap_bytes(1, 3), Some((0, 19)));
    }

    /// A combining mark is part of the character it follows, not its own cell.
    #[test]
    fn a_combining_mark_belongs_to_its_base() {
        let m = CellMap::of("e\u{301}x");
        assert_eq!(m.cells(), 2);
        assert_eq!(m.bytes_at(0), Some((0, 3)));
        assert_eq!(m.bytes_at(1), Some((3, 4)));
        assert_eq!(m.snap_bytes(0, 1), Some((0, 3)));
        assert_eq!(m.cell_of_byte(2), Some(0), "the mark lives in cell 0");
        assert_eq!(m.cell_of_byte(3), Some(1));
    }

    #[test]
    fn a_row_of_nothing_has_no_cells() {
        let m = CellMap::of("");
        assert!(m.is_empty());
        assert_eq!(m.cells(), 0);
        assert_eq!(m.at(0), None);
    }

    /// Styles ride on the row and never reach the paste.
    #[test]
    fn styles_render_and_are_never_copied() {
        let rows = rows_from_rendered(vec![RenderedRow {
            entry: 0,
            end: RowEnd::Hard,
            line: Line::from(vec![
                Span::styled("red".to_string(), Style::new().fg(Color::Red)),
                Span::raw(" plain".to_string()),
            ]),
        }]);
        assert_eq!(rows[0].line.spans.len(), 2, "the style is kept on the row");
        assert_eq!(paste_text(&rows), "red plain", "and dropped at the door");
        assert_eq!(rows[0].cells.cells(), 9, "cells count text, not styles");
    }

    // ───────────────────────────── re-wrap ─────────────────────────────

    /// A pinned view has nothing to anchor to: it follows the tail through a
    /// resize as it follows it through an append.
    #[test]
    fn rewrap_keeps_a_pinned_view_pinned() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "a"), hard(0, "b"), hard(0, "c")]);
        s.rewrap(
            20,
            vec![hard(0, "a1"), hard(0, "a2"), hard(0, "b"), hard(0, "c")],
        );
        assert!(s.is_pinned(), "a resize did not unpin a following view");
        assert_eq!(s.offset(), 0);
        assert_eq!(s.width(), 20);
        assert_eq!(s.window(1)[0].to_string(), "c");
    }

    /// **The** resize property: the view follows the content, not the row index.
    #[test]
    fn rewrap_anchors_on_content_not_on_row_index() {
        let mut s = Scrollback::new(20);
        s.push(vec![
            soft(0, "narrow top"),
            hard(0, "wrapped part"),
            hard(0, "ANCHOR"),
            hard(0, "below it"),
        ]);
        // A one-row band at offset 1 has exactly one row hanging below it, so it
        // shows ANCHOR and "below it" is off the bottom.
        s.scroll_by(-1, 1);
        assert_eq!(s.window(1)[0].to_string(), "ANCHOR");
        let anchor = s
            .resting_anchor()
            .expect("an unpinned view has a resting anchor");
        assert_eq!(
            anchor,
            ContentAnchor {
                entry: 0,
                logical: 1,
                byte: 0
            },
            "the resting row, addressed by content"
        );

        // A wider window merged the top logical line into one row: every index
        // below it shifted by one.
        s.rewrap(
            40,
            vec![
                hard(0, "narrow top wrapped part"),
                hard(0, "ANCHOR"),
                hard(0, "below it"),
            ],
        );

        assert_eq!(
            s.resting_anchor(),
            Some(anchor),
            "the same content is still what the view rests on"
        );
        assert_eq!(
            s.window(1)[0].to_string(),
            "ANCHOR",
            "…and that is what the band shows"
        );
        assert_eq!(
            s.offset(),
            1,
            "the view did not jump to the row its content used to occupy"
        );
        // The row an index-preserving resize would have shown:
        assert_eq!(s.rows()[2].to_string(), "below it");
    }

    /// When the anchored content has left the transcript, the view holds as near
    /// as it can rather than teleporting to the tail — the user did not ask to
    /// be at the tail.
    #[test]
    fn rewrap_with_lost_content_holds_rather_than_inventing_a_position() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "a"), hard(0, "b"), hard(0, "c"), hard(0, "d")]);
        s.scroll_by(-3, 1);
        assert_eq!(s.window(1)[0].to_string(), "a");

        // Re-wrap without entry 0 at all: it was evicted while we looked away.
        s.rewrap(20, vec![hard(1, "x"), hard(1, "y")]);
        assert_eq!(s.offset(), 1, "clamped to the head of what still exists");
        assert!(!s.is_pinned(), "and not quietly moved to the tail");
        assert_eq!(
            lines(s.window(1)),
            ["x"],
            "the top of what is left is what the view shows, not the tail"
        );
    }

    /// Re-wrapping is not new content: nothing that was pending becomes anything
    /// else, and the count is not re-armed.
    #[test]
    fn rewrap_is_not_new_content() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "one"), hard(0, "two")]);
        s.scroll_by(-2, 1);
        s.push(vec![hard(1, "three")]);
        assert_eq!(s.pending(), 1);
        s.rewrap(30, vec![hard(0, "one"), hard(0, "two"), hard(1, "three")]);
        assert_eq!(s.pending(), 1, "the same unseen rows are still unseen");
    }

    // ───────────────────────────── caps ─────────────────────────────

    /// The row cap trims the *oldest* and counts it.
    #[test]
    fn the_row_cap_drops_the_oldest_and_says_so() {
        let mut s = Scrollback::with_rows(20, 3);
        s.push((0..5).map(|i| hard(0, &format!("l{i}"))).collect());
        assert_eq!(s.len(), 3);
        assert_eq!(s.dropped_rows(), 2);
        assert_eq!(lines(s.rows()), ["l2", "l3", "l4"]);
        assert!(s.is_pinned(), "trimming the front does not move the view");

        // `0` means unbounded, the same convention the byte cap uses.
        let mut open = Scrollback::with_rows(20, 0);
        open.push((0..50).map(|_| hard(0, "x")).collect());
        assert_eq!(open.len(), 50);
    }

    /// After the transcript itself is compacted, `entry` must still mean the
    /// entry it meant — or the row has to go.
    #[test]
    fn eviction_drops_gone_entries_and_renumbers_the_rest() {
        let mut s = Scrollback::new(20);
        s.push(vec![
            hard(0, "from entry 0"),
            hard(1, "from entry 1"),
            hard(2, "from entry 2"),
            hard(3, "from entry 3"),
        ]);
        // Entries 0 and 1 came off the front and the notice went in at index 1,
        // so the transcript now reads [2, notice, 3].
        s.entries_evicted(2, 1);

        let got: Vec<(usize, String)> = s.rows().iter().map(|r| (r.entry, r.to_string())).collect();
        assert_eq!(
            got,
            vec![
                (0, "from entry 2".to_string()),
                (2, "from entry 3".to_string())
            ],
            "entry 2 is index 0 now, and entry 3 is index 2 because the notice \
             took index 1"
        );
        assert_eq!(paste_text(s.rows()), "from entry 2\nfrom entry 3");
    }

    #[test]
    fn eviction_of_nothing_changes_nothing() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "a")]);
        s.entries_evicted(0, 0);
        assert_eq!(s.len(), 1);
        assert_eq!(s.rows()[0].entry, 0);
    }

    /// Every `window` call is in range for every shape of store: the frame hands
    /// this a band height taken from the window, so a store shorter than the
    /// band, a band of one row and an empty store all have to be answerable.
    #[test]
    fn the_window_is_always_a_valid_slice() {
        for n in 0..12usize {
            for visible in 0..8usize {
                for scroll in 0..=n {
                    let mut s = Scrollback::new(20);
                    s.push((0..n).map(|i| hard(0, &format!("l{i}"))).collect());
                    s.scroll_by(-(scroll as isize), visible);
                    let w = s.window(visible);
                    assert!(w.len() <= visible.max(1), "n={n} visible={visible}");
                    assert!(w.len() <= n);
                    if n > 0 && visible > 0 {
                        assert!(!w.is_empty(), "n={n} visible={visible} scroll={scroll}");
                    }
                    // And never anything but a contiguous run of the store.
                    if let Some(first) = w.first() {
                        let text = first.to_string();
                        assert!(s.rows().iter().any(|r| r.to_string() == text));
                    }
                }
            }
        }
    }

    /// A pinned store of fewer rows than the band shows what there is, and no
    /// more: the caller pads.
    #[test]
    fn a_short_transcript_shows_what_exists() {
        let mut s = Scrollback::new(20);
        s.push(vec![hard(0, "x"), hard(0, "y")]);
        assert_eq!(lines(s.window(10)), ["x", "y"]);
        assert_eq!(s.window(0).len(), 0, "a zero-height band gets nothing");
        let empty = Scrollback::new(20);
        assert!(empty.window(10).is_empty());
    }
}
