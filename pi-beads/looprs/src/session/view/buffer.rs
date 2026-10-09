//! The retained-history budget: what a view may keep, and the two ways it lets
//! go when it runs out.
//!
//! The constants are the ceiling a view is built to and the prices it counts
//! rows in; the two mechanisms below are what spend down to it.
//! [`SessionView::enforce_buffer`] drops whole oldest entries off the front of
//! an over-budget view, [`SessionView::trim_open_entry`] cuts the middle out
//! of a single entry longer by itself than the entire cap, and the trim is
//! counted in both directions ([`SessionView::take_trims`]) so the store can
//! say what vanished.
//!
//! Dropping is a policy here, not an accident: every path keeps the journal the
//! whole document (the file is what survives a trim; the store is what renders),
//! never re-emits what the flusher already sent, and leaves no row the
//! transcript cannot re-render. Those tests are why this is a module of its own
//! rather than four methods at the bottom of a 3,670-line file.
use crate::session::view::SessionView;

use super::TerminalType;

use crate::state::scrollback::{DEFAULT_RETAINED_BYTES, ROW_STRUCT_BYTES};

/// Default cap on how much text an *inactive* view will hold.
///
/// A view that is on screen drains every frame, so this only binds for a session
/// nobody is looking at — which is exactly the unbounded case ADR-0002 names
/// ("a `yes | sleep 1000000`-style shell … grows its transcript forever"). When a
/// hidden session overruns the cap, old entries are dropped, and the loss is
/// honest rather than silent — the visible half of that honesty is the store's
/// own [`TrimMarker`](crate::state::scrollback::RowKind) row, which is why no
/// notice line is inserted *here*: a notice in the transcript is content, gets
/// copied, gets journalled, and gets counted against the thing it is apologising
/// for.
///
/// This cap bounds **text held for a session nobody is looking at**. It is not
/// the same number, and cannot be, as the cap on what a seen session *renders*
/// ([`DEFAULT_RETAINED_BYTES`](crate::state::scrollback::DEFAULT_RETAINED_BYTES)):
/// rendered rows carry styling and a cell map and cost several times their text.
///
/// Also per-view, so the two caps together are [`RETAINED_BYTES_WORST_CASE`]'s
/// per-view term, multiplied by the number of modes the app runs.
pub const DEFAULT_VIEW_BUFFER: usize = 256 * 1024;

/// How many [`SessionView`]s the app can hold at once — one per mode.
///
/// [`App`](crate::app::App) keys its views by [`TerminalType`], so this is not
/// a knob of its own but a fact about the mode table, and it is *derived* from
/// [`TerminalType::ALL`] for the reason the paragraph on
/// [`RETAINED_BYTES_WORST_CASE`] gives: a number that has to be re-derived by
/// hand is a number that gets left behind.
pub const MAX_VIEWS: usize = TerminalType::ALL.len();

/// The ceiling on what the app **retains**, in one number and one honest sentence.
///
/// Every mode keeps one [`SessionView`], and every view holds two separately
/// capped things:
///
/// * the **rendered store** — [`DEFAULT_RETAINED_BYTES`], 32 MiB of
///   [`Scrollback`], which is where the band draws from;
/// * the **transcript text** it renders from — [`DEFAULT_VIEW_BUFFER`], 256 KiB
///   of [`Transcript`].
///
/// All three modes alive with both caps full is therefore **3 × 32 MiB of
/// rendered rows (~96 MiB) + 3 × 256 KiB of transcript text (~0.75 MiB) ≈ 97
/// MiB retained**, which is the figure to quote when someone asks how much
/// looprs holds. It was previously not stated anywhere as a single thing: each
/// cap documented itself per view, and adding up "how many views are there" was
/// left to the reader — which is exactly how a ~96 MiB ceiling stays invisible
/// while two 32 MiB-sized-looking numbers sit in two different modules.
///
/// Why a `const` rather than the sentence alone: the sentence depends on three
/// numbers that live in three places (the mode count, the store cap, the buffer
/// cap), and hand-written prose about them goes stale silently. This one moves
/// with its inputs, and the test that pins its human-readable form —
/// [`tests::the_retained_ceiling_is_the_sum_it_says_it_is`] — fails loudly when
/// a fourth mode or a retuned cap changes it, so the sentence has to be *said
/// again* on purpose rather than inherited by accident.
///
/// What it does **not** cover, all of it deliberate:
///
/// * **syntect's compiled regex state** — this line used to call it
///   "megabytes paid once on the first highlight" without saying how many, and
///   the number turns out to be worth knowing: warming the four languages the
///   real corpus fences in retains **26.5 MiB of Rust heap** and takes the
///   process to **44.8 MiB RSS** (`fancy`, release; the `md::highlighter()`
///   load itself is 430.6 KiB). It is one copy, process-wide, paid once by the
///   startup warm thread (`utils::md::spawn_warm_from_env`) rather than by a
///   frame, and it is not history — it does not grow with the session, does not
///   get evicted with the scrollback, and is not per-view. That is why it is
///   excluded here rather than added to the total: this constant answers "how
///   much retained **content** does the app hold", and a compiled regex table is
///   not content. Numbers, both backends, and the reason the engine stayed pure
///   Rust: [ADR-0009](../../docs/adr/0009-highlighter-warm-start-and-the-regex-backend.md);
/// * **transients** — the peak a rebuild passes *through* is larger than what
///   it retains, and is measured separately (`spikes/results/resize-transient.log`);
/// * **allocator slack** — freed is not the same as returned to the OS.
///
/// So: this is the retained-history ceiling, not a peak-RSS figure.
///
/// [`Transcript`]: crate::state::transcript::Transcript
pub const RETAINED_BYTES_WORST_CASE: usize =
    MAX_VIEWS * (DEFAULT_RETAINED_BYTES + DEFAULT_VIEW_BUFFER);

/// Floor on what [`SessionView::trim_open_entry`] leaves inside the entry it cuts.
///
/// Only binds for deliberately tiny caps. The default budget halves to 128 KiB,
/// which is thousands of times this, so the floor is invisible in a real run — it
/// exists so a 128-byte test cap still retains a line or two rather than cutting
/// the entry to nothing, which would be a trim with no content left for its own
/// marker to be the boundary of.
pub const MIN_OPEN_KEEP: usize = 64;

/// Floor on the bytes-per-row density used to price transcript text the store
/// has never rendered.
///
/// Only binds when there is no measured density to use — a store with no rows
/// has no opinion about how many rows a byte makes — and the alternative to a
/// floor is a divide by zero. A real store measures itself near 88 B/row on the
/// corpus behind [`crate::state::scrollback::ROW_STRUCT_BYTES`], so in a live
/// session this number is the degenerate case, not the working one.
pub(super) const MIN_BYTES_PER_ROW: usize = 4;

/// How much denser than the store's own average a piece of never-rendered text
/// is assumed to be when the rewrap budget prices it.
///
/// The store's measured density is the best available estimate of what a byte
/// of this view's content costs to hold, and the unseen tail is the one part of
/// a rebuild whose cost has never been measured — so the estimate carries a
/// factor rather than pretending to be a measurement. Two is the compromise:
/// enough that a stretch of short lines (a `git log --oneline`, a JSON dump)
/// cannot make the budget read the unseen tail as free and reach back for
/// history it will not hold, and small enough that a normal burst of ordinary
/// prose does not clear the retained scrollback off the store on the next
/// resize.
pub(super) const UNSEEN_ROW_SAFETY: usize = 2;

/// What a re-render costs the cap, with the row count already scaled by the new
/// content width.
///
/// [`crate::state::scrollback::row_charge`] is the price of one row in the cap's own
/// unit; a re-render is
/// priced against a row count that is a *width ratio times* an existing count,
/// i.e. fractional. Carrying the rows as `rows × new_cw` and the budget as
/// `cap × new_cw` is what keeps that ratio exact across a whole entry instead of
/// rounding a row up per entry — and a rounded-up row per entry is a row per
/// entry of scrollback the budget refuses to buy. Both pricing sites go through
/// here so "what a row costs" stays one number.
pub(super) fn price_scaled(rows_cw: usize, text: usize, new_cw: usize) -> usize {
    text.saturating_mul(new_cw)
        .saturating_add(rows_cw.saturating_mul(ROW_STRUCT_BYTES))
}

impl SessionView {
    /// Cap the buffered transcript while this view is *not* on screen.
    ///
    /// Lossy on purpose — that is what a cap is — so the two things it must get
    /// right are: say what was lost, and do not corrupt the render cursor. It
    /// says what was loss by telling the store, which puts it on the marker row
    /// the user will scroll to; it keeps the cursor honest by reseating the
    /// flusher to the entry that the eviction moved into the slot it was
    /// reading, because its per-entry state (scan/block/fence) belongs to
    /// whatever entry it was last on.
    ///
    /// It used to also insert a `… N bytes dropped (buffer cap) …` entry into
    /// the transcript itself. That is gone: the marker row says the same thing
    /// where the user can see it, and the entry said it in the middle of the
    /// content the copy and the journal carry.
    pub(super) fn enforce_buffer(&mut self) {
        // What the cap has to cover is everything this view holds, which is the
        // transcript *plus* the Bash line the resolver is still resolving: the
        // store cannot see that one, and it is exactly the thing a child that
        // never ends a line can grow without bound.
        let buffered = self.transcript.byte_len() + self.shell.pending_len();
        if self.limit == 0 || buffered <= self.limit {
            return;
        }
        let first = self.flusher.consumed();
        let mut dropped = 0usize;
        let mut removed = 0usize;
        let mut lines = 0usize;
        // The re-render-source skip is counted here too, as a lead: the frontmost
        // `source_skipped` entries of this list have *already* been reported as
        // lost by the rewrap that cut them out of the rebuild, because the
        // store dropped them by not making them again. The transcript still
        // held them, which is why the cap now takes them — but the marker has
        // said so once, and a number that counts the same loss twice is a
        // number nobody believes.
        let mut skipped = self.source_skipped;
        // `bytes` mirrors the transcript's running total for the duration of the
        // loop. Not for speed — `byte_len()` is O(1) now — but because this
        // loop's whole job is to move that number down, and a local mirror says
        // that in the code: k evictions are k steps, not k passes over n entries
        // the way it was when the condition re-summed the transcript each time it
        // asked (looprs-7m5).
        let mut bytes = self.transcript.byte_len();
        // Always keep one entry: an empty transcript with the cursor past the end is
        // a state nothing downstream is written to expect.
        while bytes > self.limit && self.transcript.entries.len() > 1 {
            let gone = self
                .transcript
                .evict_front()
                .expect("the length check above left at least one entry to take");
            bytes -= gone.text.len();
            dropped += gone.text.len();
            // The unit the marker speaks: lines of content, counted while the
            // entry is still here to count them. Rows are the store's business;
            // this store may never have rendered these ones at all.
            if skipped == 0 {
                lines += gone.text.lines().count();
            } else {
                skipped -= 1;
            }
            removed += 1;
        }
        if removed > 0 {
            // The skipped prefix followed the list down; it never grows here.
            self.source_skipped = skipped;
            self.dropped += dropped;
            // The eviction shifted the transcript; the render cursor follows it.
            self.flusher.reseat(first.saturating_sub(removed));
            // …and so does the journal cursor, for the same reason: it addresses the
            // same list by index. It never points *below* zero (`saturating_sub`),
            // and if it had not caught up to the eviction it means the journal is
            // behind by the entries the cap took — which is why `after_write`
            // journals first.
            let was = self.journalled;
            self.journalled = self.journalled.saturating_sub(removed);
            // A *saturated* decrement means the entry the walk was standing on was
            // itself in the evicted range. If that entry had a head already in the
            // file (`journal_continued`), the flag now describes an entry that is
            // gone, so the next chunk must go back to the ordinary join rather
            // than continue a sentence whose entry nobody has any more.
            if was < removed {
                self.journal_continued = false;
            }
            // The store indexes rows by entry, and the entries just moved underneath
            // it. Rows rendered from a gone entry cannot be re-rendered, so they go
            // now rather than vanishing on the next resize; the survivors get
            // renumbered so `entry` keeps naming the right thing.
            self.scrollback.entries_evicted(removed, lines);
            self.pending_trims.push(removed);
            tracing::debug!(
                "view buffer cap: {dropped} bytes over {removed} entries dropped ({} lines)",
                lines
            );
        }
        // The loop above cannot reach the shape that actually overflows: one
        // long-running stream inside a single still-open entry. Ask that question
        // here, whether or not the loop managed to remove anything — "removed
        // nothing because only one entry exists" *is* the case.
        self.trim_open_entry();
    }

    /// Cap a single **open** entry, which whole-entry eviction cannot reach.
    ///
    /// Why this is a second question rather than more of the first: the loop's
    /// unit is the entry, and it deliberately keeps one entry so that an empty
    /// transcript with the cursor past the end is never handed downstream. But
    /// every append path writes into the *tail* entry
    /// ([`Transcript::push_delta`], [`Transcript::push_shell_lines`]), and
    /// [`Self::seal_shell_output`] closes the block at **submit** only — so one
    /// running command *is* one open entry for its whole life. `tail -f`, `watch`,
    /// `npm run dev`, a big build. Measured before this existed: a 4 KiB cap held
    /// 1.2 MB of transcript and gave the journal none of it.
    ///
    /// The cut is made at a **line boundary** — never mid-line, mid style-run or
    /// mid UTF-8 character — and down to half the remaining budget rather than
    /// exactly to it, because cutting at the cap would re-enter this function on
    /// the next write, forever.
    ///
    /// Four things the cut keeps straight, because each one addresses the bytes
    /// being thrown away:
    ///
    /// * **the journal** gets the head before memory loses it, and only while the
    ///   walk stands on this very entry — writing a later entry's piece ahead of
    ///   an earlier one would reorder the document it is meant to be;
    /// * **the style runs** are byte ranges into this entry's own text, so they
    ///   are dropped or re-based against the cut, never left pointing into bytes
    ///   that are no longer there;
    /// * **the flusher** is adjusted in place and never *reseated* — a reseat
    ///   rewinds to the head of the entry and would re-emit lines the store
    ///   already has. `preview` slices `text[scan..]`, so a stale cursor here is
    ///   not a stale render, it is an out-of-range slice;
    /// * **entry indices do not move.** That is why neither the store nor a
    ///   standing drag selection needs renumbering from this path. What can be
    ///   eaten is a selection's `(logical, byte)` anchor *inside* this entry, on
    ///   the next re-wrap — the same fail-safe an ordinary trim already has: the
    ///   anchor does not resolve, the selection clears, and no wrong text gets
    ///   copied.
    fn trim_open_entry(&mut self) {
        let Some(idx) = self.transcript.entries.len().checked_sub(1) else {
            return;
        };
        // Only the tail entry can grow, and only while it is open. A closed entry
        // is the whole-entry loop's business, not this one's.
        if self.transcript.entries[idx].done {
            return;
        }
        let tail_len = self.transcript.entries[idx].text.len();
        // Cut against the *view's* budget, not the entry's share of it: the rest
        // of the transcript is still here and still counted.
        let target = self
            .limit
            .saturating_sub(self.transcript.byte_len() - tail_len);
        if target == 0 || tail_len <= target {
            return;
        }
        // Half the budget, so the next cut is a whole budget of growth away. The
        // floor is for test-sized caps: a trim that cut an entry to nothing would
        // leave nothing for the marker to be the boundary *of*.
        let keep = (target / 2).max(MIN_OPEN_KEEP);
        let want = tail_len.saturating_sub(keep);
        // Snap *forward* to the end of the next complete line. Never backward: a
        // retained row that starts halfway through a line is a paragraph with its
        // head missing, and no marker apologises for that properly.
        let cut = match self.transcript.entries[idx].text[want..].find('\n') {
            Some(nl) => want + nl + 1,
            // One unfinished line left: there is no cut to make that is a line.
            None => return,
        };
        if cut >= tail_len {
            return; // the retained remainder would be empty
        }
        // Take the head before anything below moves the bytes under it.
        let head = self.transcript.entries[idx].text[..cut].to_string();

        // Journal the head before memory loses it — the same rule the whole-entry
        // path runs on, for the same reason.
        //
        // By the time a cut is possible the walk is always standing on this very
        // entry: the eviction loop above gets under the cap by taking everything
        // else off the front, which leaves the open tail as the only entry and the
        // cursor on it. Asserted rather than assumed, because the alternative —
        // writing this piece somewhere else in the file — is not an option: the
        // document's order is the whole reason it can be read.
        debug_assert_eq!(
            self.journalled, idx,
            "open-entry trim of entry {idx} with the walk at {}: the head cannot \
             be journalled in order",
            self.journalled
        );
        if self.journalled == idx {
            let mut chunk = String::with_capacity(head.len() + 2);
            // The first piece of an entry gets the ordinary between-entries join;
            // a later piece of an entry the file has already started gets none,
            // so head + retained is byte-for-byte the entry the uncapped journal
            // would have written.
            if self.journalled_any && !self.journal_continued {
                chunk.push_str("\n\n");
            }
            chunk.push_str(&head);
            self.journal.append(self.session.mode.label(), chunk);
            self.journal_continued = true;
        } else {
            // Reachable only if the eviction invariant above stops holding. The
            // bytes still have to go — memory is the emergency — but not quietly:
            // a cap that eats content and says nothing is the failure ADR-0004 R2
            // was written against.
            tracing::error!(
                "open-entry trim: {cut} bytes cut from entry {idx} could not be \
                 journalled in order (walk held at {}) — they are gone from the \
                 file as well as from memory",
                self.journalled
            );
        }

        // Take the bytes out and re-base the presentation in one move: text and
        // style runs are one fact, and the running total is the debit against it,
        // so neither belongs at this call site. The returned count is what
        // actually left — `cut` clamped to the entry's length — and everything
        // downstream that speaks bytes follows *that*, not the amount asked for.
        let cut_bytes = self.transcript.cut_entry_head(idx, cut);
        // The flusher reads this entry from a byte offset; follow the bytes down.
        if self.flusher.consumed() == idx {
            self.flusher.cut_front(cut_bytes);
        }
        self.dropped += cut_bytes;
        tracing::debug!(
            "open-entry cap: {cut} bytes cut from entry {idx}, {} retained against a {target} budget",
            self.transcript.entries[idx].text.len()
        );
    }

    /// Drain the trims applied since the last call, each one the number of
    /// transcript entries that came off the front, in the order they happened.
    ///
    /// The consumer is the drag selection, which addresses rows by entry and so
    /// must be renumbered by the same eviction the store was renumbered by
    /// (looprs-pdl.9). Both calls take the same number from the same place,
    /// which is the only thing keeping "what the row is numbered" and "what the
    /// selection thinks it is numbered" one fact rather than two.
    pub fn take_trims(&mut self) -> Vec<usize> {
        std::mem::take(&mut self.pending_trims)
    }

    /// Set the byte cap. A test seam in the shipped shape: eviction is
    /// otherwise reachable only by streaming [`DEFAULT_VIEW_BUFFER`] bytes, and
    /// a test that does that is a slow test that nobody runs.
    #[allow(dead_code)] // test seam: `app::tests` trips the buffer cap through this to test the selection's trim re-base
    pub fn set_buffer_limit(&mut self, limit: usize) {
        self.limit = limit;
    }
}
