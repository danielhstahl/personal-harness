//! The shape of a drag selection (looprs-pdl.9).
//!
//! The ticket draws the line for this module sharply, and it is worth stating
//! before the types: **the copy is looprs-pdl.10, this is the shape**. What
//! lives here is the answer to "what did the user just select" — as a range of
//! *characters*, addressed so that it survives a resize, a scroll and a trim —
//! and nothing else. No clipboard, no toast, no transport.
//!
//! # Why the selection is content-addressed and not a cell rectangle
//!
//! The obvious model is the one every text widget uses: remember the (row, col)
//! you pressed on and the (row, col) you are on, and draw between them. That
//! model is wrong here twice over, and both times the ticket names it:
//!
//! * **a resize** re-wraps the transcript, so the same characters land on
//!   different cells. A kept cell rectangle does not "point at slightly wrong
//!   text" after a resize — it points at *different text*, confidently, and the
//!   user has no way to see that the box they are looking at is no longer the
//!   box they dragged. *"Re-derive the cells from the character range; do not
//!   keep the cell rectangle and hope."*
//! * **a scroll** moves the content under the same rectangle, which is the
//!   same lie told by a different gesture.
//!
//! So the selection stores what the two ends of the drag *are*, not where they
//! were: [`CharRef`], which is `(entry, logical line, byte, byte end)` — the
//! same address [`ContentAnchor`] uses, widened by the end of the character so
//! that each end is a whole character by construction. Cells are derived from
//! it, per frame, against whatever the store currently looks like
//! ([`Selection::cells`]). The rectangle is a *projection*, recomputed every
//! time, which is exactly what a projection is for.
//!
//! # The drag is a state machine, not a boolean
//!
//! [`Selection`] has three states and the ticket's sentence writes them out:
//! *press* sets the anchor and starts a **pending** selection ([`Selection::Dragging`]),
//! *drag* events move the focus, *release* **commits** it
//! ([`Selection::Selected`]) — and a press with no movement is a **click**,
//! which clears. The third case is why `Dragging` and `Selected` cannot be one
//! state with a `moved: bool`: "a click clears" means the release *destroys*
//! the pending selection, and that is a transition out of a state rather than
//! an attribute of one.
//!
//! # The two ends are characters, so half a character cannot exist
//!
//! A drag that starts on the trailing half of a CJK glyph, or that ends inside
//! a ZWJ family, is not a slightly worse selection — it is a corrupted
//! character on the clipboard. The snapping is not done here and is not done in
//! the renderer either; it is done at the one place that both of them read, the
//! store's [`CellMap`](crate::state::scrollback::CellMap) (looprs-pdl.6), which answers every cell lookup with a
//! *whole* cluster. [`hit`] asks that map for the cell the pointer is on and
//! copies the cluster's byte span into the `CharRef`. There is therefore no
//! code path in this module that could produce half a glyph, rather than a path
//! that is careful not to.
//!
//! # What the range *means* across a wrap
//!
//! The user drags from the middle of a wrapped paragraph, over the fold, into
//! the next one. The selection **renders** over the rows they touched and
//! **means** a range of characters across logical lines.
//! [`Selection::resolve`] does the turning of one into the other: it walks the
//! store's rows, cuts the byte range each row contributes, and hands the
//! result to [`paste_slices`], which is ADR-0004 R14 with the join rule
//! already in it — a soft continuation joins with nothing, a hard end with
//! exactly one `\n`. The rows are the projection; the character range is the
//! thing; and the paste is computed from the thing.
//!
//! Note that `resolve` walks the **whole store**, not the visible window. A
//! selection that silently stopped at the screen edge would be a lie about its
//! own extent, and the lie would be paid for at copy time.
//!
//! # Chrome is not transcript
//!
//! The status row, the tool-card band and the input box are not transcript and
//! cannot be selected (ADR-0004 R16). This module enforces that by
//! *only ever* answering hit tests against store rows: chrome is not in the
//! store, so there is nothing there to hit. A drag that crosses chrome keeps
//! its focus where the transcript last was and selects the transcript rows in
//! the vertical span it covers — which, because the range is content and not
//! pixels, needs no "skip the chrome" special case at all. The chrome is not in
//! the range. It cannot be.
//!
//!
//! # Auto-scroll
//!
//! Dragging off the top or bottom edge of the band scrolls rather than
//! stopping, on a throttle ([`AUTO_SCROLL_INTERVAL`]). The throttle is the
//! whole design: the wire can deliver a hundred motion events a second
//! (measured in ADR-0004 as 4.5×10⁵/s, which is why there is no event-rate
//! concern anywhere in this epic), and one row per event would move the text
//! several times faster than the hand moving the mouse — the selection would
//! outrun the drag and land somewhere the user was never pointing at. One row
//! per [`AUTO_SCROLL_INTERVAL`] is about ten rows a second, which is faster
//! than a careful drag and slower than the pointer can travel.
//!
//! The scroll and the focus are separate calls on purpose, and the caller must
//! scroll **before** it re-reads the pointer: the content under the cursor is
//! what the band shows *after* the scroll, not before it. See
//! See `App::on_mouse` for that ordering.
//!
//! # Clearing
//!
//! Cleared by a click without a drag ([`Selection::release`]), by
//! [`Selection::clear_if_live`] (the Esc rule), and by a mode switch (the
//! caller's, in [`crate::App`]). *Not* cleared by a repaint, a tick, new
//! output arriving, or a resize — which needs no code here, because none of
//! those can change what a character address points at. A trim that ate the
//! anchored content does not clear the state either: [`Selection::resolve`]
//! clamps to the rows that still exist, so the selection shrinks to what is
//! left rather than pointing at nothing with confidence.
//!
//! # Esc ordering
//!
//! [`Selection::clear_if_live`] is the whole rule and it is stated here rather
//! than in the key handler because it is a property of the selection, not of
//! the keyboard: **if a selection is live, the first Esc clears the selection
//! and does nothing else; the next Esc is the cancel the mode table already
//! describes** (ADR-0003). Cancelling a running model call because the user
//! wanted to unselect text is the class of surprise that decision exists to
//! prevent, and it is not allowed to be discovered by accident.

use std::cmp::Ordering;
use std::time::{Duration, Instant};

use ratatui::layout::Rect;

use crate::state::scrollback::{ContentAnchor, DisplayRow, RowSlice, Scrollback, paste_slices};

/// How far one edge-drag scrolls, and how often.
///
/// See the module doc's auto-scroll section: the rate is a throttle against the
/// event stream, not a guess at a speed. One row per event would outrun the
/// hand; one row per 100 ms does not, and about ten rows a second is fast
/// enough that no one ever notices it is not faster.
pub const AUTO_SCROLL_ROWS: isize = 1;
/// The minimum spacing between two auto-scroll steps.
pub const AUTO_SCROLL_INTERVAL: Duration = Duration::from_millis(100);

/// One character of the transcript, addressed by content.
///
/// `(entry, logical, start)` is [`ContentAnchor`]'s address — the one pdl.6
/// put in the store precisely so that a selection could be written in it. The
/// extra field, `end`, is the byte just past this character, and it is what
/// makes each *end of a drag* a whole character: the range is the union of two
/// complete characters and everything between them, so "half a CJK glyph" is
/// not a state this type can be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharRef {
    /// Which transcript entry the character belongs to.
    pub entry: usize,
    /// Which logical line within that entry.
    pub logical: usize,
    /// Byte offset of the character's first byte within that logical line.
    pub start: usize,
    /// Byte just past the character (still within the logical line).
    pub end: usize,
}

impl CharRef {
    /// The same address without the width — for anything that only needs to know
    /// *which character*, not how many bytes it is.
    ///
    /// This is the seam between the two addressings in this module: the ends of a
    /// drag are `CharRef`s (whole characters), and the store speaks
    /// `ContentAnchor`s. Nothing in today's draw path needs the narrower form,
    /// so it is asserted rather than used — see
    /// `a_selection_end_is_a_store_address`, which round-trips it through
    /// `Scrollback::index_of`.
    #[allow(dead_code)] // consumer: anything that has to hand a selection end back to the store (pdl.7's trim hooks, pdl.13's keyboard selection); proven round-trip today
    pub fn anchor(&self) -> ContentAnchor {
        ContentAnchor {
            entry: self.entry,
            logical: self.logical,
            byte: self.start,
        }
    }

    /// The ordering key: a character's position in reading order.
    ///
    /// Deliberately not `end` — two `CharRef`s naming the same character agree
    /// about where it is whatever their byte lengths differ by, and reading
    /// order is a statement about position.
    fn pos(&self) -> (usize, usize, usize) {
        (self.entry, self.logical, self.start)
    }
}

impl PartialOrd for CharRef {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CharRef {
    fn cmp(&self, other: &Self) -> Ordering {
        self.pos().cmp(&other.pos())
    }
}

/// A range of whole characters, ordered.
///
/// "Ordered" is the direction-independence the ticket asks for: the range is
/// normalised at construction from the two ends of the drag, so a drag that
/// went right-to-left selects exactly what the same span dragged
/// left-to-right selects. Everything downstream — the highlight, the paste,
/// the count — reads this and never learns which way the mouse moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CharRange {
    /// The earlier character, inclusive.
    pub start: CharRef,
    /// The later character, inclusive.
    pub end: CharRef,
}

impl CharRange {
    /// Normalise two ends into one reading-order range.
    ///
    /// Both ends are *inclusive* whole characters, so this picks the earlier
    /// one's start and the later one's end — which is why each end carries both
    /// of its own boundaries rather than one: an end that stored only a byte
    /// offset would be either the start of the character pointed at (and then
    /// a leftwards drag loses the character it started on) or the end of it
    /// (and then a rightwards drag overshoots by one). With both, the earlier
    /// end contributes its start, the later end contributes its end, and
    /// neither direction can lose or gain a character.
    pub fn new(a: CharRef, b: CharRef) -> Self {
        if a.pos() <= b.pos() {
            Self { start: a, end: b }
        } else {
            Self { start: b, end: a }
        }
    }
}

/// What a trim did to a live selection. See
/// [`Selection::entries_evicted`] for what each one means on the screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrimEffect {
    /// The selection still has everything it had.
    Untouched,
    /// Its earlier end was trimmed; the selection now starts at the oldest
    /// content that survives, and is shorter than the drag that made it.
    Clamped,
    /// All of it went, and there is no selection to clamp.
    Dropped,
}

/// Which edge of the transcript band a pointer position is on.
///
/// The *band*, not the drawn rows: the edge that matters is the edge of the
/// space the user can see, and whether the band happens to be full of rows is a
/// different question.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Edge {
    /// Nowhere near an edge.
    #[default]
    None,
    /// The top row of the band (or above it).
    Top,
    /// The bottom row of the band (or below it).
    Bottom,
}

/// The live drag selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Selection {
    /// Nothing is selected and no button is down.
    #[default]
    None,
    /// The left button is down over transcript text. The range may still be
    /// empty (nothing has moved yet) and nothing has been committed.
    Dragging {
        /// Where the press landed.
        anchor: CharRef,
        /// Where the drag has got to. Equal to `anchor` until a drag event
        /// moves it — which is exactly the difference between a click and a
        /// selection.
        focus: CharRef,
        /// When this drag last auto-scrolled. `None` until it does. The
        /// throttle lives *in the drag* because it is a property of one
        /// gesture: a new press starts a new throttle rather than inheriting
        /// a stale timestamp from the last one.
        last_scroll: Option<Instant>,
    },
    /// Released with the focus somewhere other than the anchor: this is a
    /// selection, and it is the range pdl.10 copies.
    Selected(CharRange),
}

impl Selection {
    /// Is there a selection the user can see — dragged right now, or committed
    /// and still standing?
    pub fn is_live(&self) -> bool {
        !matches!(self, Selection::None)
    }

    /// Is a drag in progress (button down)?
    pub fn is_dragging(&self) -> bool {
        matches!(self, Selection::Dragging { .. })
    }

    /// The live range, if there is one. While dragging with no movement there
    /// isn't: the anchor alone is not a selection, and nothing should draw or
    /// count it.
    pub fn range(&self) -> Option<CharRange> {
        match self {
            Selection::None => None,
            Selection::Selected(r) => Some(*r),
            Selection::Dragging { anchor, focus, .. } => {
                (anchor != focus).then(|| CharRange::new(*anchor, *focus))
            }
        }
    }

    /// The press. Only ever called with a character the transcript actually has
    /// (see [`hit`]), so pressing on chrome, on blank padding or past the tail
    /// starts nothing — and leaves any selection that was already standing
    /// alone, because a press that cannot select is not a statement about the
    /// selection.
    pub fn press(&mut self, at: CharRef) {
        *self = Self::Dragging {
            anchor: at,
            focus: at,
            last_scroll: None,
        };
    }

    /// A motion event moved the focus. Returns whether anything changed, which
    /// is what the caller uses to decide if a repaint is owed.
    pub fn drag(&mut self, focus: CharRef) -> bool {
        let Self::Dragging { focus: cur, .. } = self else {
            return false;
        };
        if *cur == focus {
            return false;
        }
        *cur = focus;
        true
    }

    /// The release.
    ///
    /// Returns the committed range, or `None` when the press never moved — a
    /// click, which clears. `self` is left in the state the caller must draw:
    /// `Selected` or `None`, never `Dragging`.
    pub fn release(&mut self) -> Option<CharRange> {
        let Self::Dragging { anchor, focus, .. } = *self else {
            return None;
        };
        if anchor == focus {
            *self = Self::None;
            return None;
        }
        let r = CharRange::new(anchor, focus);
        *self = Self::Selected(r);
        Some(r)
    }

    /// Esc's first claim on itself: if a selection is live, drop it and say so.
    ///
    /// The `true` return is the whole ordering rule (see the module doc): the
    /// Esc was spent here and must not go on to cancel anything else.
    pub fn clear_if_live(&mut self) -> bool {
        if !self.is_live() {
            return false;
        }
        *self = Self::None;
        true
    }

    /// Forget the selection whatever it was. The mode switch's entry point.
    pub fn clear(&mut self) {
        *self = Self::None;
    }

    /// The transcript was compacted: follow the entry renumbering, the same
    /// mapping [`Scrollback::entries_evicted`] applies to the rows.
    ///
    /// **This is where a trim meets a live selection, and the ticket is explicit
    /// about what has to happen: it clamps, and it does not lie.** Three
    /// outcomes, returned so the caller (and the test) can tell them apart:
    ///
    /// * [`TrimEffect::Untouched`] — nothing this selection is made of went
    ///   anywhere; the entries just moved, and the mapping keeps them moved the
    ///   same way the store moved its rows.
    /// * [`TrimEffect::Clamped`] — one end of the selection was trimmed away.
    ///   That end can only be the **earlier** one (trims come off the old end,
    ///   and the ends are ordered by content), so it is set to the oldest thing
    ///   that still exists and the selection goes on living, shorter than the
    ///   drag that made it. Two things make that honest rather than a shrink
    ///   behind the user's back: the highlight is re-derived from the range
    ///   every frame ([`Selection::cells`]), so what the user sees selected *is*
    ///   what will be copied; and the count on the copy toast is the count of
    ///   the text that resolved, not the count of the drag, so the number that
    ///   gets reported is the number that got copied. A selection that shrank
    ///   *silently* between drag and paste is a lie about the clipboard, and
    ///   that is the failure this branch exists to close.
    /// * [`TrimEffect::Dropped`] — both ends went. There is no range left to
    ///   clamp to, and a "selection" of content the user never pointed at is
    ///   not a shorter selection, it is a wrong one. The state is cleared.
    ///
    /// Called with the same number the store got, from the same event — the two
    /// mappings must be the same mapping, or the selection points at entries by
    /// one numbering and the rows carry another.
    pub fn entries_evicted(&mut self, removed: usize) -> TrimEffect {
        if removed == 0 || !self.is_live() {
            return TrimEffect::Untouched;
        }
        // The same mapping the store applied, and nothing else: after `removed`
        // entries come off the front, the oldest surviving entry is index 0, so
        // the head of what is left is a fixed address in the new numbering.
        let map = |old: usize| old.checked_sub(removed);
        let head = CharRef {
            entry: 0,
            logical: 0,
            start: 0,
            end: 0,
        };
        match self {
            Selection::None => TrimEffect::Untouched,
            Selection::Dragging { anchor, focus, .. } => {
                match (map(anchor.entry), map(focus.entry)) {
                    (Some(a), Some(f)) => {
                        anchor.entry = a;
                        focus.entry = f;
                        TrimEffect::Untouched
                    }
                    // Exactly one end went, so that end becomes the head of what
                    // is left — and the *surviving* end still has to be
                    // renumbered in the same breath, or the clamped range pairs a
                    // new head with an old address and copies the wrong text.
                    // Neither arm assumes which end is the earlier one: a drag
                    // upward puts the focus behind the anchor, and the clamp
                    // follows whichever end actually went.
                    (Some(a), None) => {
                        anchor.entry = a;
                        *focus = head;
                        TrimEffect::Clamped
                    }
                    (None, Some(f)) => {
                        focus.entry = f;
                        *anchor = head;
                        TrimEffect::Clamped
                    }
                    (None, None) => {
                        *self = Self::None;
                        TrimEffect::Dropped
                    }
                }
            }
            Selection::Selected(r) => {
                match (map(r.start.entry), map(r.end.entry)) {
                    (Some(a), Some(b)) => {
                        r.start.entry = a;
                        r.end.entry = b;
                        TrimEffect::Untouched
                    }
                    (None, Some(b)) => {
                        r.start = head;
                        r.end.entry = b;
                        TrimEffect::Clamped
                    }
                    // Not reachable through a front-trimming store (the later end
                    // cannot go before the earlier one), but answered rather than
                    // left to a `reachable!`: dropping is the honest answer if a
                    // future trim ever works from the other end.
                    (Some(_), None) | (None, None) => {
                        *self = Self::None;
                        TrimEffect::Dropped
                    }
                }
            }
        }
    }

    /// Should the view scroll because this drag is sitting on an edge, and by
    /// how much (positive toward the tail, matching
    /// [`Scrollback::scroll_by`])?
    ///
    /// Returns `0` when there is no drag, when the pointer is not on an edge,
    /// and when the last step was less than [`AUTO_SCROLL_INTERVAL`] ago. It
    /// does **not** scroll itself: the caller owns the store, and the caller
    /// must scroll *before* it re-reads the pointer position, or the focus
    /// lands on the content the user was looking at before the scroll rather
    /// than the content now under the cursor.
    pub fn auto_scroll(&mut self, edge: Edge, now: Instant) -> isize {
        if edge == Edge::None {
            return 0;
        }
        let Self::Dragging { last_scroll, .. } = self else {
            return 0;
        };
        if let Some(at) = *last_scroll
            && now.saturating_duration_since(at) < AUTO_SCROLL_INTERVAL
        {
            return 0;
        }
        *last_scroll = Some(now);
        match edge {
            // Toward history at the top, toward the tail at the bottom: the
            // sign is the direction the *view* travels, per `scroll_by`.
            Edge::Top => -AUTO_SCROLL_ROWS,
            Edge::Bottom => AUTO_SCROLL_ROWS,
            Edge::None => 0,
        }
    }

    /// The rows and byte ranges this selection covers, in reading order,
    /// clamped to what the store still has.
    ///
    /// Walks the **whole store**, not the visible window: the extent of a
    /// selection is a fact about content, and rows that scrolled off the top
    /// are still inside the thing the user dragged across.
    ///
    /// The clamp is the trim story: when the anchored rows are gone, the rows
    /// that remain inside the range are still selected, and nothing outside it
    /// is. A selection whose content has all been trimmed resolves to nothing,
    /// which is the honest answer and costs the caller no special case — see
    /// the module doc's "Clearing".
    pub fn resolve<'a>(&self, rows: &'a [DisplayRow]) -> Vec<RowSlice<'a>> {
        let Some(r) = self.range() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for row in rows {
            if let Some((from, to)) = span_on(row, &r) {
                out.push(RowSlice { row, from, to });
            }
        }
        out
    }

    /// What this selection would paste (ADR-0004 R14, R15).
    ///
    /// A convenience over [`Self::resolve`] + [`paste_slices`], and the shape
    /// pdl.10 wants. Empty when nothing is selected, and empty for a selection
    /// of blanks — which R13 says must copy nothing and say nothing.
    pub fn paste(&self, rows: &[DisplayRow]) -> String {
        paste_slices(self.resolve(rows))
    }

    /// The cells this selection paints, as `(index into rows, first cell, one
    /// past last cell)`.
    ///
    /// This is the **only** thing the draw path reads, which is the point of
    /// the split: the highlight is derived from the character range against
    /// the rows currently on screen, every frame, so a resize cannot leave a
    /// stale box pointing at stale text. Rows the range does not touch are not
    /// in the list at all, and a row whose clusters cannot be split at the
    /// range's edge is not split — the range's ends are whole clusters to begin
    /// with, so this never has to invent one.
    pub fn cells(&self, rows: &[DisplayRow]) -> Vec<(usize, u16, u16)> {
        let Some(r) = self.range() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (i, row) in rows.iter().enumerate() {
            let Some((from, to)) = span_on(row, &r) else {
                continue;
            };
            let total = row.cells.cells();
            // `from` is inside the row's text, so it is inside some cluster,
            // and that cluster's cell is where the highlight starts — even if
            // `from` is the *second* byte of a wide character's cluster, in
            // which case the whole character lights up, because half a
            // character is not a thing that can be lit.
            let Some(cell_from) = row.cells.cell_of_byte(from) else {
                continue;
            };
            // The cell just past the selection: `to` is a cluster boundary, so
            // the byte at `to` (if any) belongs to the *next* cluster, and its
            // cell is one past the last selected one.
            let cell_to = if to >= row.cells.text_len() {
                total
            } else {
                row.cells.cell_of_byte(to).unwrap_or(total)
            };
            if cell_from < cell_to {
                out.push((i, cell_from as u16, cell_to as u16));
            }
        }
        out
    }
}

/// The byte span the range puts on one row, relative to **that row's own
/// text**, or `None` if it puts nothing on it.
///
/// The one place "does this row fall inside the range" is answered, so
/// [`Selection::resolve`] and [`Selection::cells`] cannot drift apart on it.
///
/// The endpoint clamps are applied **only** on the rows the endpoints are in:
/// a row in the middle of the range takes its whole span, because a byte
/// offset belonging to some other logical line has no business clipping it.
/// (The two coordinates live in different spaces — the range is addressed in
/// bytes of the *logical line*, the answer is a slice of the *row* — and
/// mixing them up is the difference between "the middle rows are whole" and
/// "the middle rows start at byte 43 of themselves", which is nothing at all.)
fn span_on(row: &DisplayRow, r: &CharRange) -> Option<(usize, usize)> {
    // The trim marker is chrome that happens to live in the store, and chrome is
    // not transcript (ADR-0004 R16). Selecting it would put
    // "scrollback trimmed: …" on the clipboard in the middle of the user's
    // text — the one thing in the store that is not content, and so the one
    // thing that must answer "nothing here" to both resolve and highlight.
    if row.is_trim_marker() {
        return None;
    }
    let key = (row.entry, row.logical);
    let start_key = (r.start.entry, r.start.logical);
    let end_key = (r.end.entry, r.end.logical);
    if key < start_key || key > end_key {
        return None;
    }
    // The row's own span in logical-line bytes.
    let row_from = row.start;
    let row_to = row.start + row.cells.text_len();
    let from = if key == start_key {
        r.start.start.max(row_from)
    } else {
        row_from
    };
    let to = if key == end_key {
        r.end.end.min(row_to)
    } else {
        row_to
    };
    if from >= to {
        return None;
    }
    Some((from - row_from, to - row_from))
}

/// The character under a cell of a display row, snapped to the whole cluster.
///
/// This is the one place a pointer position becomes content, and it delegates
/// the hard part: [`CellMap::at`](crate::state::scrollback::CellMap::at) answers any cell with the *whole* cluster
/// that owns it, so a cell on the trailing half of `日` answers with all of
/// `日`, and a cell inside a ZWJ family answers with the family. A row with no
/// clusters (a blank separator) has nothing to hit and answers `None`.
///
/// A cell past the end of the row's text is clamped to the last cluster: a
/// press in the blank tail of a row means "at the end of this row", which in
/// the drag is what the user's hand is aiming at, and for the *end* of a drag
/// it means "through the end of the row" — not "nothing".
pub fn hit(row: &DisplayRow, cell: usize) -> Option<CharRef> {
    // Nothing to hit on the marker row, for the same reason there is nothing to
    // hit on the status row: it is not content. A press there starts no
    // selection, which leaves whatever was standing already standing alone —
    // the same rule as a press on chrome.
    if row.is_trim_marker() {
        return None;
    }
    let total = row.cells.cells();
    if total == 0 {
        return None;
    }
    let cell = cell.min(total - 1);
    let span = row.cells.at(cell)?;
    Some(CharRef {
        entry: row.entry,
        logical: row.logical,
        start: row.start + span.start,
        end: row.start + span.end,
    })
}

/// Where the pointer **rests** in a run of rows, as opposed to what one row's
/// cell contains.
///
/// [`hit`] answers `None` for a blank row — there is no cluster there to hit
/// — and a drag that froze on every blank line would be unusable: half the
/// transcript's rows are the blank separators the flusher puts between
/// entries, so "press on a line and drag down one row" would land on a
/// blank, resolve to nothing, and be read back as a click that throws the
/// whole selection away.
///
/// The rule the walk implements: a pointer resting on a blank line is the
/// user pointing at **the end of the line above**, which is where the text
/// ran out. Walking up for the nearest row that has clusters, and taking its
/// last one, says that in one line of code. Past the head of the transcript
/// there is nothing above to rest on, and the answer is `None` — the same
/// answer chrome gives, and for the same reason: there is no content there.
///
/// A side effect worth naming: selecting *through* blank lines works, but a
/// drag cannot end exactly on one; it ends at the end of the text above it.
/// The trailing blank lines are left out of the selection. They are the
/// separator, not the message, and the alternative — a range that can end
/// on a line with no characters in it — needs a boundary in the model that
/// buys nothing the user can see.
pub fn hit_resting(rows: &[DisplayRow], idx: usize, cell: usize) -> Option<CharRef> {
    if let Some(at) = hit(rows.get(idx)?, cell) {
        return Some(at);
    }
    let above = rows[..idx].iter().rev().find(|r| r.cells.cells() > 0)?;
    hit(above, usize::MAX)
}

/// What the last drawn frame put in the transcript band.
///
/// **Why the selection needs a snapshot of the pixels.** A pointer position is
/// a claim about the screen, and the only honest map from a claim about the
/// screen to a claim about content is the one made by the code that painted
/// the screen. Re-deriving the band layout at mouse time would mean either
/// re-rendering the live tail (a markdown re-parse per motion event, and drag
/// events come at a hundred a second) or guessing at how many live rows were
/// in the frame that produced the pixels under the cursor — and a guess that
/// is one row out does not produce an off-by-one, it produces a selection of
/// the wrong paragraph, with a box drawn where the user aimed and characters
/// picked from where the guess was.
///
/// So `view` publishes what it drew, and the hit test reads that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BandSnapshot {
    /// The band's area: `x` is where cell 0 is, `height` is the visible span
    /// the edges are measured from.
    pub area: Rect,
    /// Screen row of the first store row the frame drew.
    pub first_row_y: u16,
    /// How many store rows it drew.
    pub rows: usize,
    /// The content address of that first drawn row, so the index below is
    /// resolved against *content* and not against a row number that a trim or
    /// a re-wrap has since invalidated. `None` when the frame drew no store
    /// rows at all — in which case there is nothing anywhere to select.
    pub first: Option<ContentAnchor>,
}

impl BandSnapshot {
    pub fn new(area: Rect, first_row_y: u16, rows: usize, first: Option<ContentAnchor>) -> Self {
        Self {
            area,
            first_row_y,
            rows,
            first,
        }
    }

    /// The band-local cell column of a screen column (`0` at the band's left
    /// edge). A column left of the band clamps to `0`, which is harmless: the
    /// row underneath still answers, and there is no other sane reading of
    /// "half a character to the left of the first one".
    pub fn cell_of(&self, column: u16) -> usize {
        column.saturating_sub(self.area.x) as usize
    }

    /// Is this screen row inside the band the frame drew — i.e. is it
    /// transcript rather than chrome?
    ///
    /// The wheel's first question (looprs-pdl.8), and answered here rather
    /// than at the call site because the band's extent is the frame's
    /// knowledge: it is the same `area` the band drew itself into, so a report
    /// over the input box, the status row or the card band cannot be
    /// mis-classified as transcript by a second guess at the geometry. The
    /// columns are not asked about: the band spans the window's width, and
    /// "over the transcript, in some column" is the whole of the question a
    /// scroll report needs answered.
    pub fn contains_row(&self, row: u16) -> bool {
        row >= self.area.top() && row < self.area.bottom()
    }

    /// Which edge of the band this screen row is on, for auto-scroll.
    pub fn edge_at(&self, row: u16) -> Edge {
        if self.area.height == 0 {
            return Edge::None;
        }
        if row <= self.area.top() {
            Edge::Top
        } else if row >= self.area.bottom() - 1 {
            Edge::Bottom
        } else {
            Edge::None
        }
    }

    /// The store index of the row drawn at screen row `y`, if anything was.
    ///
    /// Resolved through [`Scrollback::index_of`] from the published anchor, so
    /// the answer follows the content: if the store shifted between the draw
    /// and this event (a re-wrap landed, a trim ran), the count starts from
    /// where that content *now* is rather than from where the row number used
    /// to be. When the published content has itself gone, the anchor lookup
    /// fails and the answer is "nothing was hit" — the same clamped, hold-
    /// rather-than-teleport behaviour the store's own re-wrap uses
    /// ([`Scrollback::rewrap`]).
    pub fn row_index(&self, store: &Scrollback, y: u16) -> Option<usize> {
        if self.rows == 0 {
            return None;
        }
        let offset = y.checked_sub(self.first_row_y)?;
        if offset >= self.rows as u16 {
            return None;
        }
        let base = store.index_of(self.first?)?;
        Some(base + offset as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::scrollback::RenderedRow;
    use crate::state::scrollback::{RowEnd, rows_from_rendered};
    use ratatui::text::Line;

    fn rendered(entry: usize, end: RowEnd, text: &str) -> RenderedRow {
        RenderedRow {
            entry,
            end,
            line: Line::from(text.to_string()),
        }
    }

    fn hard(entry: usize, text: &str) -> RenderedRow {
        rendered(entry, RowEnd::Hard, text)
    }

    fn soft(entry: usize, text: &str) -> RenderedRow {
        rendered(entry, RowEnd::Soft, text)
    }

    /// One entry laid out as `parts`: every row but the last is a soft
    /// continuation, so this is the shape a real wrap leaves behind.
    ///
    /// **The space at a fold stays in the row it was broken from.** That is not
    /// a detail of the fixture, it is the convention the whole soft-wrap story
    /// rests on (pdl.6, ADR-0004 R14): a soft row joining with *nothing* is
    /// only correct because the space it broke on is already sitting at the end
    /// of the earlier row. A fixture that dropped it would be testing the weld.
    fn laid(entry: usize, parts: &[&str]) -> Vec<RenderedRow> {
        let n = parts.len();
        parts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                if i + 1 == n {
                    hard(entry, t)
                } else {
                    soft(entry, t)
                }
            })
            .collect()
    }

    fn entry(entry: usize, parts: &[&str]) -> Vec<DisplayRow> {
        rows_from_rendered(laid(entry, parts))
    }

    fn hit_at(rows: &[DisplayRow], row: usize, cell: usize) -> CharRef {
        hit(&rows[row], cell).expect("the row has a character there")
    }

    // ───────────────────── the state machine ─────────────────────

    /// Press starts it pending, drag moves the focus, release commits. The
    /// ticket's sentence, as a test.
    #[test]
    fn press_then_drag_then_release_commits_a_range() {
        let rows = entry(0, &["hello world"]);
        let a = hit_at(&rows, 0, 0); // 'h'
        let b = hit_at(&rows, 0, 4); // 'o'

        let mut sel = Selection::default();
        assert!(!sel.is_live());

        sel.press(a);
        assert!(sel.is_dragging());
        assert!(
            sel.range().is_none(),
            "a press alone is not a selection: nothing has moved"
        );

        assert!(sel.drag(b), "the focus moved");
        assert_eq!(
            sel.paste(&rows),
            "hello",
            "while the button is still down the range already means what it will copy"
        );

        let committed = sel.release();
        assert_eq!(committed, Some(CharRange::new(a, b)));
        assert!(!sel.is_dragging());
        assert!(sel.is_live(), "committed, and still standing");
        assert_eq!(sel.paste(&rows), "hello");
    }

    /// **A press with no movement is a click, not a selection, and clears** —
    /// including when it lands on top of a selection that was already standing.
    #[test]
    fn a_press_with_no_movement_is_a_click_and_clears() {
        let rows = entry(0, &["hello world"]);
        let a = hit_at(&rows, 0, 2);
        let b = hit_at(&rows, 0, 6);

        let mut sel = Selection::default();
        sel.press(a);
        assert_eq!(sel.release(), None, "a click commits nothing");
        assert_eq!(sel, Selection::None, "and clears, rather than going empty");

        sel.press(a);
        sel.drag(b);
        assert!(sel.is_live());
        sel.press(b);
        assert_eq!(sel.release(), None);
        assert!(!sel.is_live(), "the click cleared the standing selection");
    }

    /// A drag that comes back to where it started *is* a click again, and that
    /// is the right shape rather than a loose end: two ends on the same
    /// character is not a selection of zero length, it is no selection. A
    /// "live" empty selection would be something Esc has to clear and the copy
    /// path has to refuse, and the user would be holding a selection that is
    /// nothing. The range follows the pointer, and the pointer is where it
    /// began.
    #[test]
    fn a_drag_back_to_the_anchor_is_no_selection() {
        let rows = entry(0, &["abcdef"]);
        let a = hit_at(&rows, 0, 0);
        let b = hit_at(&rows, 0, 3);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        assert_eq!(sel.paste(&rows), "abcd");
        sel.drag(a);
        assert_eq!(sel.range(), None, "back home, so nothing is selected");
        assert_eq!(sel.release(), None, "and the release says click");
        assert_eq!(sel, Selection::None);
    }

    /// **Direction independence.** The range is a span of characters, not a
    /// trace of the mouse path.
    #[test]
    fn the_range_does_not_care_which_way_the_mouse_went() {
        let rows = entry(0, &["hello world"]);
        let a = hit_at(&rows, 0, 1); // 'e'
        let b = hit_at(&rows, 0, 8); // 'r'

        let mut forwards = Selection::default();
        forwards.press(a);
        forwards.drag(b);
        forwards.release();

        let mut backwards = Selection::default();
        backwards.press(b);
        backwards.drag(a);
        backwards.release();

        assert_eq!(
            forwards, backwards,
            "left-to-right and right-to-left over the same span are one selection"
        );
        assert_eq!(forwards.paste(&rows), "ello wor");
    }

    // ─────────────────── whole characters only ───────────────────

    /// **Half a wide character is not a selection.** The snapping is the
    /// store's cell map, read at the one place a pointer becomes content, so
    /// there is no code path here that could emit half a glyph rather than a
    /// path that is careful not to.
    #[test]
    fn a_drag_never_starts_or_ends_on_half_a_wide_glyph() {
        // 日本語: cells 0..1 are 日, 2..3 are 本, 4..5 are 語.
        let rows = entry(0, &["日本語"]);
        assert_eq!(
            hit_at(&rows, 0, 1).start,
            0,
            "cell 1 is the *right half* of 日, and answers with all of 日"
        );
        assert_eq!(hit_at(&rows, 0, 2).start, 3, "cell 2 starts 本");

        let a = hit_at(&rows, 0, 1);
        let b = hit_at(&rows, 0, 4);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        assert_eq!(sel.paste(&rows), "日本語");
        assert_eq!(sel.cells(&rows), vec![(0, 0, 6)]);

        // A drag *within* one glyph snaps both ends to the same character, so
        // it is a click and not a half-glyph selection: the same rule as
        // "a press with no movement is a click", arriving by a different road.
        let mut within = Selection::default();
        within.press(hit_at(&rows, 0, 2));
        within.drag(hit_at(&rows, 0, 3));
        assert_eq!(
            within.range(),
            None,
            "cells 2 and 3 are both 本, so the two ends are one character"
        );
        assert_eq!(within.release(), None);

        // One glyph *is* selectable when the drag crosses a boundary.
        let mut one = Selection::default();
        one.press(hit_at(&rows, 0, 2));
        one.drag(hit_at(&rows, 0, 4));
        assert_eq!(one.paste(&rows), "本語");
        assert_eq!(one.cells(&rows), vec![(0, 2, 6)]);
    }

    /// The ZWJ case from the ticket: the family is one cluster, and a range
    /// that ends "inside" it ends after all of it.
    #[test]
    fn a_zwj_sequence_is_never_cut_in_half() {
        let fam = "\u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f466}";
        let rows = entry(0, &[&format!("{fam} tail")]);
        // cells: 0..1 the family, 2 ' ', 3 't', 4 'a', 5 'i', 6 'l'.
        assert_eq!(rows[0].cells.cells(), 7, "2 cells of family + 5 of ' tail'");

        let a = hit_at(&rows, 0, 1);
        let b = hit_at(&rows, 0, 5);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        let text = sel.paste(&rows);
        assert_eq!(text, format!("{fam} tai"));
        assert_eq!(
            text.chars().count(),
            9,
            "family(5 code points) + ' tai'(4) — 5 characters is 2 code points short"
        );
        assert_eq!(
            text.matches('\u{200d}').count(),
            2,
            "both joiners survived, so the family is one sequence and not three stray glyphs"
        );

        // And starting just after the family never picks up a dangling joiner.
        assert_eq!(
            hit_at(&rows, 0, 2).start,
            fam.len(),
            "cell 2 is the space, not the tail of the family"
        );
    }

    // ─────────────────── wrap is not content ───────────────────

    /// **Soft wrap vs hard newline**: the selection *renders* over the rows it
    /// touched and *means* a range of characters in one logical line.
    #[test]
    fn a_drag_over_a_fold_copies_the_logical_line_not_the_rows() {
        let rows = entry(0, &["The quick brown fox ", "jumps over the lazy dog"]);
        let a = hit_at(&rows, 0, 0); // 'T'
        let b = hit_at(&rows, 1, 9); // the 'r' of "over"
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        assert_eq!(
            sel.paste(&rows),
            "The quick brown fox jumps over",
            "the soft fold contributes NO newline and NO space: the wrap is not \
             content (ADR-0004 R15) and the space it broke on is already in row 0"
        );
        // The wrong answer, spelled out: what gluing the painted rows back
        // together with newlines would produce.
        assert_ne!(
            sel.paste(&rows),
            "The quick brown fox\njumps over",
            "that would be a hard break the source never had"
        );
    }

    /// …and a hard end contributes **exactly one** `\n`, including across an
    /// entry boundary, which in the paste is just another line break.
    #[test]
    fn a_drag_across_logical_lines_contributes_exactly_one_newline_each() {
        // One store, three entries — and built in a *single*
        // `rows_from_rendered` call on purpose. `logical` and the running byte
        // offset are per-entry state that call owns, so stitching two calls
        // together hands two rows the same `(entry, logical)` key and the
        // range reads them as one logical line.
        let rows = rows_from_rendered(vec![
            hard(0, "aaa"),
            hard(0, "bbb"),
            hard(1, "ccc"),
            hard(2, "ddd"),
        ]);
        let a = hit_at(&rows, 0, 1);
        let b = hit_at(&rows, 2, 1);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        assert_eq!(
            sel.paste(&rows),
            "aa\nbbb\ncc",
            "one \\n per hard end, and the entry boundary costs no extra"
        );
    }

    /// The extent of a selection is a fact about content, not about what was
    /// on screen when the button came up.
    #[test]
    fn a_selection_extends_past_the_visible_window() {
        let rows = entry(0, &["aa ", "bb ", "cc ", "dd"]);
        let a = hit_at(&rows, 0, 0);
        let b = hit_at(&rows, 3, 1);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        // Resolved against the whole store, though a real band shows two of
        // these four rows at a time.
        assert_eq!(sel.resolve(&rows).len(), 4, "all four rows are in it");
        assert_eq!(sel.paste(&rows), "aa bb cc dd");
        // The *highlight* is the separate question it needs to be, answered
        // against whatever slice the caller hands it.
        assert_eq!(
            sel.cells(&rows[1..3]),
            vec![(0, 0, 3), (1, 0, 3)],
            "index 0 is the first row of the slice handed in, not of the store"
        );
    }

    /// **The resize property.** The range keeps pointing at the same
    /// characters; the cells they land on are re-derived, and they differ.
    #[test]
    fn the_range_survives_a_rewrap_and_the_cells_are_re_derived() {
        let wide = entry(0, &["one two three"]);
        let narrow = entry(0, &["one", " two", " three"]);

        // Select "two" at the wide wrap: bytes 4..7 of logical line 0.
        let a = hit_at(&wide, 0, 4);
        let b = hit_at(&wide, 0, 6);
        assert_eq!(
            (a.start, b.end),
            (4, 7),
            "setup: the substring \"two\" of \"one two three\""
        );
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        sel.release().expect("it moved, so it committed");

        let cells_wide = sel.cells(&wide);
        let cells_narrow = sel.cells(&narrow);
        assert_eq!(cells_wide, vec![(0, 4, 7)], "one row, cells 4..7");
        assert_eq!(
            cells_narrow,
            vec![(1, 1, 4)],
            "the same three characters are on the second row now, at other cells"
        );
        assert_ne!(cells_wide, cells_narrow);
        assert_eq!(sel.paste(&wide), "two");
        assert_eq!(
            sel.paste(&narrow),
            "two",
            "…and the paste is unchanged, which is the whole point of addressing it by content"
        );
    }

    // ─────────────────────── auto-scroll ───────────────────────

    /// **Auto-scroll is a rate, not a reaction.** The throttle is the design:
    /// a burst of edge events moves one row, not a hundred, because a
    /// selection that outruns the drag lands somewhere the user was never
    /// pointing.
    #[test]
    fn an_edge_drag_scrolls_at_a_throttled_rate() {
        let t0 = Instant::now();
        let rows = entry(0, &["alpha"]);
        let mut sel = Selection::default();
        sel.press(hit_at(&rows, 0, 0));

        for _ in 0..50 {
            assert_eq!(sel.auto_scroll(Edge::None, t0), 0, "not on an edge");
        }

        assert_eq!(sel.auto_scroll(Edge::Top, t0), -1, "toward history");
        for i in 1..100 {
            assert_eq!(
                sel.auto_scroll(Edge::Top, t0 + Duration::from_millis(i)),
                0,
                "inside the interval at {i}ms: the drag does not outrun the hand"
            );
        }
        assert_eq!(
            sel.auto_scroll(Edge::Top, t0 + AUTO_SCROLL_INTERVAL),
            -1,
            "one row per interval, no more"
        );
        assert_eq!(
            sel.auto_scroll(Edge::Bottom, t0 + AUTO_SCROLL_INTERVAL * 2),
            AUTO_SCROLL_ROWS,
            "and the other edge goes the other way"
        );
    }

    /// The throttle belongs to the gesture, not to the module: a fresh press
    /// is not grounded by the last one's timestamp.
    #[test]
    fn a_new_press_starts_a_fresh_auto_scroll_throttle() {
        let t0 = Instant::now();
        let rows = entry(0, &["alpha"]);
        let a = hit_at(&rows, 0, 0);
        let mut sel = Selection::default();
        sel.press(a);
        assert_eq!(sel.auto_scroll(Edge::Top, t0), -1);
        sel.release();

        sel.press(a);
        assert_eq!(
            sel.auto_scroll(Edge::Top, t0 + Duration::from_millis(1)),
            -1,
            "a new drag can scroll immediately"
        );
    }

    /// Auto-scroll is inert without a button down.
    #[test]
    fn auto_scroll_is_inert_without_a_drag() {
        let mut sel = Selection::default();
        assert_eq!(sel.auto_scroll(Edge::Top, Instant::now()), 0);

        sel.press(CharRef {
            entry: 0,
            logical: 0,
            start: 0,
            end: 1,
        });
        sel.drag(CharRef {
            entry: 0,
            logical: 0,
            start: 2,
            end: 3,
        });
        sel.release();
        assert!(sel.is_live());
        assert_eq!(
            sel.auto_scroll(Edge::Bottom, Instant::now()),
            0,
            "committed is not held-down: nothing to scroll"
        );
    }

    // ───────────────────── chrome is not transcript ─────────────────────

    /// A blank row has no cluster to hit, so a press on the transcript's own
    /// blank separator starts nothing — and cannot select "half a blank".
    #[test]
    fn a_blank_row_has_nothing_to_hit() {
        let rows = rows_from_rendered(vec![hard(0, "text"), hard(0, "")]);
        assert!(
            hit(&rows[1], 0).is_none(),
            "a blank row has no character to start a selection on"
        );
        assert!(hit(&rows[0], 0).is_some(), "and the real row does");
    }

    /// **A drag does not freeze on a blank line.** Half the transcript's rows
    /// are the blank separators between entries, so a hit that answered `None`
    /// on all of them would make "press on a line, drag down one row" read
    /// back as a click and throw the selection away. A pointer resting on a
    /// blank line rests at the end of the line above it.
    #[test]
    fn a_pointer_on_a_blank_line_rests_at_the_end_of_the_line_above() {
        let rows = rows_from_rendered(vec![hard(0, "the line"), hard(1, ""), hard(2, "the next")]);
        let resting = hit_resting(&rows, 1, 3).expect("a blank line is still a place to rest");
        assert_eq!(
            resting,
            hit(&rows[0], usize::MAX).unwrap(),
            "the end of the line above, which is where the text ran out"
        );
        assert_eq!(
            hit_resting(&rows, 2, 2),
            hit(&rows[2], 2),
            "and a row with text in it is hit normally"
        );

        // A long blank stretch walks all the way back to the text.
        let long = rows_from_rendered(vec![hard(0, "top"), hard(1, ""), hard(2, ""), hard(3, "")]);
        assert_eq!(
            hit_resting(&long, 3, 40),
            hit(&long[0], usize::MAX),
            "however far down the blanks go"
        );

        // Above the head of the transcript there is nothing to rest on.
        let head = rows_from_rendered(vec![hard(0, ""), hard(1, "text")]);
        assert_eq!(
            hit_resting(&head, 0, 0),
            None,
            "a blank with nothing above it is not a place to start"
        );
    }

    /// The consequence that matters for the gesture: a press on a line and a
    /// release on the blank line under it is a drag, not a click, and it
    /// selects the line the pointer came from.
    #[test]
    fn dragging_onto_the_blank_line_below_selects_the_line_above() {
        let rows = rows_from_rendered(vec![hard(0, "hello there"), hard(1, "")]);
        let mut sel = Selection::default();
        sel.press(hit_resting(&rows, 0, 0).unwrap());
        sel.drag(hit_resting(&rows, 1, 3).unwrap());
        sel.release();
        assert_eq!(
            sel.paste(&rows),
            "hello there",
            "press at the start, drag to the blank below, get the line"
        );
    }

    /// Chrome is not in the store, so there is no row to hit and no answer to
    /// give. This is the whole enforcement of "a drag cannot select chrome":
    /// the caller cannot even build a `CharRef` for the status row.
    #[test]
    fn chrome_cannot_be_hit_because_it_is_not_transcript() {
        let rows = entry(0, &["only this row"]);
        // Out of range of the store's rows entirely: no row, no hit.
        assert!(rows.get(1).and_then(|r| hit(r, 0)).is_none());
        // And a selection built from what *is* there resolves to nothing at all
        // outside it, which is what "skips the chrome" means in a paste.
        let mut sel = Selection::default();
        sel.press(hit_at(&rows, 0, 0));
        sel.drag(hit_at(&rows, 0, 3));
        sel.release();
        assert_eq!(sel.resolve(&rows).len(), 1);
        assert_eq!(sel.paste(&[] as &[DisplayRow]), "", "no rows, no paste");
    }

    /// A press past the end of a row means "at the end of this row", which is
    /// what the hand is aiming at, and makes drag-to-the-margin select to the
    /// end rather than select nothing.
    #[test]
    fn a_press_past_the_end_of_a_row_lands_on_its_last_character() {
        let rows = entry(0, &["six chars"]);
        let last = hit_at(&rows, 0, 400);
        assert_eq!(last.start, 8, "'s' is the last character");
        assert_eq!(
            hit_at(&rows, 0, 8),
            last,
            "clamped to the last cluster, not lost off the end"
        );

        let mut sel = Selection::default();
        sel.press(hit_at(&rows, 0, 0));
        sel.drag(last);
        assert_eq!(sel.paste(&rows), "six chars", "the whole row, as asked");
    }

    // ─────────────────────── the trim ───────────────────────

    /// **A trim that ate the earlier end clamps it** (looprs-pdl.7): the
    /// selection does not disappear because its left end got eaten, it starts
    /// now at the oldest thing still there — and what it resolves to is exactly
    /// that, which is what the copy reports. The test that matters is the pair:
    /// the paste got shorter, and the *count of the paste* is the count of
    /// what is in it, not the count of the drag.
    #[test]
    fn a_trim_that_ate_the_earlier_end_clamps_it_and_the_paste_tells_the_truth() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "aaaa"), hard(1, "bbbb"), hard(2, "cccc")]);
        let a = hit_at(s.rows(), 0, 2);
        let b = hit_at(s.rows(), 2, 1);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        let before = sel.paste(s.rows());
        assert_eq!(before, "aa\nbbbb\ncc");

        // Entry 0 goes. The store's mapping and the selection's are the same one
        // (`removed` in, `old - removed` out), so the surviving end still names
        // the same content it named before the eviction.
        s.entries_evicted(1, 1);
        assert_eq!(
            sel.entries_evicted(1),
            TrimEffect::Clamped,
            "the earlier end went, and the selection says so rather than clearing"
        );
        assert!(sel.is_live(), "a clamped selection is still a selection");
        let after = sel.paste(s.rows());
        assert_eq!(
            after, "bbbb\ncc",
            "what is left of it is exactly what is still on screen"
        );
        assert!(
            after.chars().count() < before.chars().count(),
            "shorter than the drag: the trim is visible in the copy, not hidden from it"
        );
        // The address it clamped to is the head of what the store still has.
        assert_eq!(
            sel.range()
                .map(|r| (r.start.entry, r.start.logical, r.start.start)),
            Some((0, 0, 0)),
            "entry 0 of the *new* numbering — the oldest thing kept"
        );
    }

    /// …and a trim that only ate rows *above* the selection leaves it alone
    /// beyond the renumbering: those bytes were never part of what the user
    /// selected.
    #[test]
    fn a_trim_above_the_selection_keeps_it_and_renumbers_it() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "gone"), hard(1, "keep me"), hard(2, "and me")]);
        let a = hit_at(s.rows(), 1, 0);
        let b = hit_at(s.rows(), 2, 2);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        assert_eq!(sel.paste(s.rows()), "keep me\nand");

        // Entry 0 off the front: the store's mapping is 1 → 0 and 2 → 1, and
        // the selection is given the same one number.
        s.entries_evicted(1, 1);
        assert_eq!(
            sel.entries_evicted(1),
            TrimEffect::Untouched,
            "nothing of the selection was eaten"
        );
        assert!(sel.is_live());
        // The store's rows and the selection's addresses moved together, so
        // the paste is still the same text against the renumbered store.
        let kept: Vec<(usize, String)> = s
            .rows()
            .iter()
            .filter(|r| !r.is_trim_marker())
            .map(|r| (r.entry, r.to_string()))
            .collect();
        assert_eq!(
            kept,
            vec![(0, "keep me".to_string()), (1, "and me".to_string())],
            "the store renumbered the survivors the same way the selection did"
        );
        assert_eq!(sel.paste(s.rows()), "keep me\nand");
    }

    /// Both ends gone is not a short selection, it is no selection: clamping
    /// both to the head would manufacture a range over content the user never
    /// pointed at, which is the one thing worse than losing the selection.
    #[test]
    fn a_trim_that_ate_all_of_it_drops_the_selection() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "one"), hard(1, "two"), hard(2, "three")]);
        let mut sel = Selection::default();
        sel.press(hit_at(s.rows(), 0, 0));
        sel.drag(hit_at(s.rows(), 1, 1));

        s.entries_evicted(2, 2);
        assert_eq!(sel.entries_evicted(2), TrimEffect::Dropped);
        assert_eq!(sel, Selection::None);
        assert_eq!(sel.paste(s.rows()), "");
    }

    /// A drag can point its *earlier* end with the focus rather than the anchor
    /// — dragging up and to the left. The clamp follows reading order, not the
    /// order the two ends happened to be recorded in.
    #[test]
    fn a_drag_upward_into_trimmed_content_clamps_its_earlier_end() {
        let mut s = Scrollback::new(40);
        s.push(vec![hard(0, "gone"), hard(1, "kept"), hard(2, "also kept")]);
        let mut sel = Selection::default();
        // press on entry 2, drag back up into entry 0: the focus is the earlier end.
        sel.press(hit_at(s.rows(), 2, 2));
        sel.drag(hit_at(s.rows(), 0, 1));
        assert_eq!(sel.paste(s.rows()), "one\nkept\nals");

        s.entries_evicted(1, 1);
        assert_eq!(sel.entries_evicted(1), TrimEffect::Clamped);
        // The focus is what went, so the focus is what is set to the head, and
        // the range the copy resolves is the kept part — not a mirrored one.
        let r = sel.range().expect("still selected");
        assert_eq!((r.start.entry, r.start.logical, r.start.start), (0, 0, 0));
        assert_eq!(r.end.entry, 1, "the press point renumbered 2 → 1");
        assert_eq!(sel.paste(s.rows()), "kept\nals");
    }

    /// The trim marker is chrome, and chrome is not transcript (ADR-0004 R16):
    /// a range that spans it must not put the marker's own sentence on the
    /// clipboard, and must not light it up as if it were content.
    #[test]
    fn the_trim_marker_is_not_selectable() {
        // Room for the last two entries only, so the store ends up as
        // [marker, "kept one", "kept two"] and a range can span the marker.
        let cap = 2 * ("kept one".len() + crate::state::scrollback::ROW_STRUCT_BYTES) + 1;
        let mut s = Scrollback::with_cap(40, cap);
        s.push(vec![
            hard(0, "a dropped line that was long"),
            hard(1, "kept one"),
        ]);
        s.push(vec![hard(2, "kept two")]);
        assert!(
            s.rows()[0].is_trim_marker(),
            "the trim put the marker at the head"
        );
        // A press on the marker hits nothing, so nothing starts there.
        assert_eq!(hit(&s.rows()[0], 0), None);
        assert_eq!(
            hit_resting(s.rows(), 0, 0),
            None,
            "nothing above to rest on"
        );

        // And a range whose span *covers* the marker row contributes nothing
        // from it: the paste is the content either side of it, never its text.
        let mut sel = Selection::default();
        sel.press(hit_at(s.rows(), 1, 0));
        sel.drag(hit_at(s.rows(), 2, 3));
        let text = sel.paste(s.rows());
        assert_eq!(
            text, "kept one\nkept",
            "the end lands on the cell it landed on, and nothing of the marker"
        );
        assert!(!text.contains("scrollback trimmed"), "{text:?}");
        let cells = sel.cells(s.rows());
        assert!(
            !cells.iter().any(|(i, _, _)| *i == 0),
            "the highlight never lands on the marker row: {cells:?}"
        );
    }

    // ─────────────────────── the Esc rule ───────────────────────

    /// **Esc ordering.** A live selection takes the first Esc; nothing else
    /// does, which is what leaves the *next* Esc free to be the cancel the
    /// mode table already describes (ADR-0003).
    #[test]
    fn esc_takes_a_live_selection_first_and_only_then_leaves() {
        let rows = entry(0, &["hello world"]);
        let mut sel = Selection::default();
        assert!(
            !sel.clear_if_live(),
            "with nothing live the Esc is NOT consumed here"
        );

        sel.press(hit_at(&rows, 0, 0));
        sel.drag(hit_at(&rows, 0, 4));
        assert!(sel.clear_if_live(), "a dragged selection takes the Esc");
        assert_eq!(sel, Selection::None);
        assert!(
            !sel.clear_if_live(),
            "and the next Esc is not consumed here either, which is what makes it a cancel"
        );
    }

    /// A press alone is live enough for Esc: it is a button the user is
    /// holding on our transcript, and Esc while holding it must not cancel a
    /// model run.
    #[test]
    fn a_press_alone_is_live_enough_for_esc() {
        let rows = entry(0, &["hello"]);
        let mut sel = Selection::default();
        sel.press(hit_at(&rows, 0, 1));
        assert!(
            sel.is_live(),
            "the drag is live even though the range is empty"
        );
        assert!(sel.clear_if_live());
    }

    // ───────────────────── the highlight's shape ─────────────────────

    /// One cell-run per touched row, in that row's own cell columns, and an
    /// index into whatever slice the caller handed — because that is how the
    /// band turns it back into a screen row.
    #[test]
    fn the_highlight_is_one_cell_run_per_touched_row() {
        let mut rows = entry(0, &["012345678"]);
        rows.extend(entry(1, &["abcdefgh"]));
        let a = hit_at(&rows, 0, 2);
        let b = hit_at(&rows, 1, 3);
        let mut sel = Selection::default();
        sel.press(a);
        sel.drag(b);
        assert_eq!(
            sel.cells(&rows),
            vec![(0, 2, 9), (1, 0, 4)],
            "to the end of the first row, from the start of the second"
        );
        assert_eq!(
            sel.cells(&rows[1..]),
            vec![(0, 0, 4)],
            "the index is relative to the slice handed in"
        );
    }

    /// A run confined to one row touches exactly one row, and a range with no
    /// rows produces nothing rather than a degenerate somewhere.
    #[test]
    fn a_range_over_nothing_highlights_nothing() {
        let rows = entry(0, &["some text"]);
        let sel = Selection::None;
        assert!(sel.cells(&rows).is_empty());
        assert!(sel.resolve(&rows).is_empty());
        assert_eq!(sel.paste(&rows), "");

        let mut one = Selection::default();
        one.press(hit_at(&rows, 0, 2));
        one.drag(hit_at(&rows, 0, 6));
        assert_eq!(one.cells(&rows), vec![(0, 2, 7)]);
        assert_eq!(
            one.cells(&rows[1..]).len(),
            0,
            "that row is not in the slice"
        );
    }

    /// An unmoved press draws nothing; the first movement draws one run.
    #[test]
    fn an_unmoved_drag_highlights_nothing() {
        let rows = entry(0, &["abcdef"]);
        let mut sel = Selection::default();
        sel.press(hit_at(&rows, 0, 0));
        assert_eq!(sel.cells(&rows).len(), 0, "nothing is drawn yet");
        sel.drag(hit_at(&rows, 0, 1));
        assert_eq!(sel.cells(&rows), vec![(0, 0, 2)]);
    }

    /// The highlight never exceeds the row: a range whose end is past the row's
    /// text ends at the row's last cell, not past the band.
    #[test]
    fn the_highlight_never_runs_past_the_row_that_has_it() {
        let rows = entry(0, &["short"]);
        let mut sel = Selection::default();
        sel.press(hit_at(&rows, 0, 0));
        // An end far past the row: only reachable by hand, which is the point —
        // the painter must still be safe.
        sel.drag(CharRef {
            entry: 0,
            logical: 0,
            start: 999,
            end: 1000,
        });
        let cells = sel.cells(&rows);
        assert_eq!(cells, vec![(0, 0, 5)]);
        for (_, from, to) in cells {
            assert!(to <= rows[0].cells.cells() as u16, "ran off the row");
            assert!(from < to);
        }
    }

    // ─────────────────── the band snapshot (hit test) ───────────────────

    /// The snapshot maps a screen row to a store row *through content*: the
    /// index is counted from where the published row now is, not from where a
    /// row number used to be.
    #[test]
    fn the_snapshot_maps_screen_rows_to_store_rows_through_content() {
        let mut s = Scrollback::new(40);
        s.push(vec![
            hard(0, "row zero"),
            hard(1, "row one"),
            hard(2, "row two"),
        ]);
        // The frame drew store rows 1..=2 starting at screen row 5.
        let snap = BandSnapshot::new(Rect::new(0, 0, 40, 10), 5, 2, Some(s.rows()[1].anchor()));
        assert_eq!(snap.row_index(&s, 5), Some(1));
        assert_eq!(snap.row_index(&s, 6), Some(2));
        assert_eq!(snap.row_index(&s, 4), None, "above what was drawn");
        assert_eq!(snap.row_index(&s, 7), None, "below what was drawn");
        assert_eq!(snap.cell_of(0), 0);
        assert_eq!(snap.cell_of(39), 39);
        assert_eq!(snap.cell_of(0) + 1, 1, "cell arithmetic is band-local");

        // The edge is the *band's* edge, whether or not the rows reach it.
        assert_eq!(snap.edge_at(0), Edge::Top);
        assert_eq!(snap.edge_at(9), Edge::Bottom);
        assert_eq!(
            snap.edge_at(5),
            Edge::None,
            "a transcript row is not an edge"
        );
        assert_eq!(snap.edge_at(8), Edge::None);
        assert_eq!(
            snap.edge_at(4),
            Edge::None,
            "padding above the rows inside the band is not the band's top row"
        );

        // With the published content trimmed away, the answer is "nothing was
        // hit" rather than a guessed row — the store's own re-wrap rule.
        s.entries_evicted(2, 2);
        assert_eq!(
            snap.row_index(&s, 5),
            None,
            "the content the frame anchored to is gone: do not invent a position"
        );
    }

    /// A band with no settled rows (empty transcript, live tail only) has
    /// nothing anywhere for a drag to start on — but still has edges.
    #[test]
    fn a_band_with_no_settled_rows_has_nothing_to_select() {
        let s = Scrollback::new(40);
        let snap = BandSnapshot::new(Rect::new(0, 0, 40, 10), 9, 0, None);
        assert_eq!(snap.row_index(&s, 9), None);
        assert_eq!(snap.edge_at(9), Edge::Bottom);
    }

    /// A zero-height band reports no edge: nothing can be on an edge of nothing.
    #[test]
    fn a_zero_height_band_has_no_edges() {
        let snap = BandSnapshot::new(Rect::new(0, 0, 40, 0), 0, 1, None);
        assert_eq!(snap.edge_at(0), Edge::None);
    }

    /// `CharRange::new`, in isolation: the two ends are inclusive, and the
    /// earlier one's start and the later one's end is the whole rule.
    #[test]
    fn the_range_normalises_its_two_ends() {
        let a = CharRef {
            entry: 0,
            logical: 0,
            start: 3,
            end: 4,
        };
        let b = CharRef {
            entry: 0,
            logical: 2,
            start: 0,
            end: 1,
        };
        assert_eq!(CharRange::new(a, b).start, a);
        assert_eq!(CharRange::new(b, a).start, a, "same range either way");
        assert_eq!(CharRange::new(a, b).end, b);
        assert_eq!(CharRange::new(b, a).end, b);
        // Entries order before logical lines, which is the transcript's order.
        let later_entry = CharRef {
            entry: 1,
            logical: 0,
            start: 0,
            end: 1,
        };
        assert!(later_entry > b);
        // And `anchor()` is the same address without the width.
        assert_eq!(
            a.anchor(),
            ContentAnchor {
                entry: 0,
                logical: 0,
                byte: 3
            }
        );
    }

    /// `CharRef::anchor` round-trips into the store's own addressing, which is
    /// what lets a selection's end be found again by `Scrollback::index_of`.
    #[test]
    fn a_selection_end_is_a_store_address() {
        let rows = entry(0, &["one", " two"]);
        let c = hit_at(&rows, 1, 1);
        assert_eq!(
            c.anchor(),
            ContentAnchor {
                entry: 0,
                logical: 0,
                byte: 4
            },
            "the second row starts at byte 3, so its second character is byte 4"
        );
        let mut s = Scrollback::new(40);
        s.push(laid(0, &["one", " two"]));
        assert_eq!(
            s.index_of(c.anchor()),
            Some(1),
            "the store finds the row this character is drawn on"
        );
    }
}
