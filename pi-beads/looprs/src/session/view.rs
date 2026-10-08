//! The per-session scrollback: a [`Transcript`] welded to the [`Flusher`] that was
//! born with it (ADR-0002 Q5).
//!
//! The ADR rejects the single shared transcript. The reason is not aesthetics, it is
//! the flusher's own invariant: a `Flusher` is a *cursor into one specific
//! transcript*, and the moment two transcripts share one cursor (or one
//! transcript is rendered through two cursors) lines get dropped or duplicated.
//! When the pane printed into the terminal's scrollback that was the one place a
//! bug was unrecoverable for the user; since looprs-pdl.4 the lines land in the
//! frame's transcript band instead, and the invariant is the same one for a
//! different surface — the display cache this view keeps is written only by
//! [`SessionView::flush`].
//!
//! So the pairing is the type. `SessionView` owns both halves, its `flusher` field
//! is private, and there is no way to point it at somebody else's transcript.
//!
//! Everything the App used to keep globally — "is the live region streaming", "is a
//! bead waiting on me", "what is this mode's last error" — lives here instead, per
//! session. `App::need_input` and `App::chat_state` are *derived from the active
//! view*, never from `input.mode`: a Beads pass that is running off-screen must not
//! be able to hide the Pi input box, and a Pi answer must not be able to pause the
//! beads loop.

use std::time::{Duration, Instant};

use ratatui::text::Line;

use super::{SessionId, TerminalType};
use crate::components::scrollback::Flusher;
use crate::session::ActiveBead;
use crate::session::{BeadStep, SessionStatus};
use crate::state::scrollback::{ROW_STRUCT_BYTES, Scrollback};
use crate::state::transcript::{MessageKind, Transcript};
use crate::theme::styles::{content_width, restyle, style_for};
use crate::utils::shelltext::LineResolver;

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
pub const DEFAULT_VIEW_BUFFER: usize = 256 * 1024;

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
const MIN_BYTES_PER_ROW: usize = 4;

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
const UNSEEN_ROW_SAFETY: usize = 2;

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
fn price_scaled(rows_cw: usize, text: usize, new_cw: usize) -> usize {
    text.saturating_mul(new_cw)
        .saturating_add(rows_cw.saturating_mul(ROW_STRUCT_BYTES))
}

/// What the live (not-yet-final) region of one session is showing.
///
/// Per session rather than global: this decides whether the preview renders and
/// whether the spinner ticks, and those are properties of *this* session's stream,
/// not of whichever mode happens to be on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChatState {
    /// Nothing streaming.
    #[default]
    Stopped,
    /// Prose streaming into the preview.
    Chat,
    /// A tool run is live.
    Tool,
    /// A context compaction is live.
    ///
    /// Its own state rather than reusing `Tool`: it is not a tool, and the thing
    /// the state is *for* is deciding what the live region shows and whether the
    /// spinner turns. Compaction wants the prose preview held back (the card owns
    /// the row) and the spinner turning (a compaction is a real, slow, paid-for
    /// call, and a still transcript over a working child is the "is it hung?"
    /// question this card was added to answer) — which is what
    /// [`Self::is_streaming`] and `main::view`'s `Chat`-only text draw give it.
    Compacting,
}

impl ChatState {
    pub fn is_streaming(self) -> bool {
        !matches!(self, Self::Stopped)
    }
}

/// What this view's token window has spent.
///
/// The *window* is what differs by mode, because the question differs — see
/// [`SessionView::tokens`]. The numbers come straight off pi's `usage` record,
/// folded in once per assistant message.
///
/// Cache tokens are carried separately rather than folded into `input`, because on
/// Anthropic-style accounting `input` **excludes** them — and on a long run the
/// cache buckets are where most of the tokens go. Fold them in and the row reports
/// a number quietly smaller than the work that was done; leave them out and a run
/// that is 90% cache reads looks free. They get their own segment, which is also
/// the first one the width ladder gives back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tokens {
    pub input: u64,
    pub output: u64,
    /// `cacheRead` + `cacheWrite`, summed at the door: the row shows them as one
    /// number and nothing upstream wants the split.
    pub cache: u64,
}

impl Tokens {
    /// Fold one assistant message's usage into the window.
    ///
    /// Called from the *authoritative* `message_end` only. pi also reports `usage`
    /// on every `message_update`, and that figure is **cumulative for the message
    /// still streaming** — folding it in per delta grows the total with the square
    /// of the message length, and it looks plausible right up until it doesn't.
    /// `message_end` lands once per API call, which is every bit as live as the row
    /// needs and impossible to double-count.
    pub fn add(&mut self, u: &crate::app::Usage) {
        self.input += u.input;
        self.output += u.output;
        self.cache += u.cache_read + u.cache_write;
    }

    /// Nothing reported yet — which is not the same fact as "zero spent", per
    /// [`crate::app::Usage`]'s optionality. The row shows no segment for it.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// One terminal state's rendered history, plus the render cursor over it.
///
/// `App` holds `HashMap<TerminalType, SessionView>`. Events are applied to the
/// view named by `Msg::*{ session }` — **never** to the view currently on screen.
/// Rendering picks the active view. Those two selections are independent, and that
/// independence is the fix for the whole class of "I tabbed away and the other
/// session's output corrupted this one" bugs.
pub struct SessionView {
    pub session: SessionId,
    pub transcript: Transcript,
    /// Private on purpose: this cursor is only valid for `self.transcript`.
    flusher: Flusher,
    /// The scrollable store: every rendered row this view has produced, plus the
    /// offset, the pin and the "N new" count over them.
    ///
    /// Written only through [`Self::flush`] and [`Self::rewrap`], which are the
    /// only two things that have seen the flusher's output — so "a line appears
    /// in the band exactly once" still follows from the monotonic-cursor property
    /// that made "a line reaches the scrollback exactly once" true, and a resize
    /// cannot leave the store wrapped for two different widths.
    ///
    /// Bounded by [`crate::state::scrollback::DEFAULT_MAX_ROWS`] rendered rows;
    /// the text it was rendered from is separately bounded by
    /// [`DEFAULT_VIEW_BUFFER`].
    scrollback: Scrollback,
    /// Liveness mirror of the owning session, for the status row (looprs-guh).
    pub status: SessionStatus,
    /// When the **current run** started, for `run_elapsed`.
    ///
    /// Set on the way *into* a busy state and cleared on the way out, which makes
    /// it "this run", not "this session": a session that goes idle and busy again
    /// starts a new clock rather than inheriting an age hours old. The instant is
    /// handed in rather than read here so the view stays a set of functions of
    /// state that a test can drive without waiting for anything.
    pub run_started: Option<Instant>,
    /// Most recent error, kept so the status row can show it without digging
    /// through scrollback (looprs-guh).
    pub last_error: Option<String>,
    /// What the live region shows for this session (was: a single global on `App`).
    pub chat: ChatState,
    /// The ticket the beads loop holds right now (looprs-w7q). `None` for every
    /// other mode, and for beads between passes.
    ///
    /// Set from the session's own `SessionEvent::ActiveBead` — the loop publishes
    /// its claim, it does not have it inferred. Consumer: the status row
    /// (looprs-guh), which cannot name the active ticket without this.
    pub active_bead: Option<ActiveBead>,
    /// The beads machine's step, for the status row. `None` for every other mode.
    /// Rendered, never re-derived (looprs-msj).
    pub step: Option<BeadStep>,
    /// Tokens this view's window has spent, for the status row.
    ///
    /// **Pi chat: the whole session** — every turn this warm child has answered.
    /// **Beads: the current ticket** — [`App`](crate::app::App) clears it when a
    /// new claim is published, so the row answers "what is this bead costing",
    /// which is the question a loop that spends money unwatched actually wants
    /// answered.
    ///
    /// Three things the window deliberately does *not* do:
    ///
    /// * **it does not roll back on `Esc`.** A cancelled pass spent what it spent;
    ///   cancelling does not un-buy it, and a number that went down on a keystroke
    ///   would be a number nobody could trust;
    /// * **it does not clear when a claim is released** (`ActiveBead: None`), so
    ///   the row keeps showing what that ticket cost after its pass ended, until
    ///   the next claim opens a fresh window;
    /// * **it does not zero out the planner.** A planner pass holds no claim, so
    ///   its spend lands in the window of the first bead that follows it — which
    ///   is the work it planned, and beats throwing away a pass that cost money.
    pub tokens: Tokens,
    /// Dropped-line bookkeeping for the buffer cap (see [`DEFAULT_VIEW_BUFFER`]).
    ///
    /// Bytes, because that is the unit the cap is a cap in. The *visible* notice
    /// of a trim is not here: it is the store's
    /// [`TrimMarker`](crate::state::scrollback::RowKind) row, and it is the
    /// store's because a notice that lives in the transcript is a notice that
    /// gets copied, journalled and counted as content. This number feeds the log
    /// and the store's line count.
    dropped: usize,
    /// How many of this view's transcript entries the last rewrap cut off the
    /// **front of the re-render source** rather than rendering and trimming.
    ///
    /// A count of entries in current transcript index space, so always at most
    /// `entries.len() - 1`: the newest entry is never in it, which is what
    /// makes [`Self::trim_open_entry`] unable to be looking at content this
    /// already reported.
    ///
    /// It exists so a loss is counted **once**. A skipped entry is content that
    /// leaves the store by never being made again, and the transcript entry
    /// itself stays put — until the buffer cap evicts it, which is a path that
    /// counts lines of loss too. Without this marker the same paragraph would be
    /// reported by the rewrap and then reported again by the eviction, and a
    /// marker number that double-counts is a marker nobody believes.
    /// [`Self::enforce_buffer`] walks it down as it evicts.
    source_skipped: usize,
    /// The byte cap. See [`DEFAULT_VIEW_BUFFER`].
    limit: usize,
    /// Trims this view has applied that the app has not yet fed to the other
    /// things that address the same store.
    ///
    /// [`Scrollback::entries_evicted`] renumbers rows by entry, and the drag
    /// selection (looprs-pdl.9) speaks the same addresses — so it has to hear
    /// about the trim in the same breath, or an eviction moves the entries out
    /// from under a standing selection and the highlight silently starts
    /// pointing at the message *after* the one the user selected. Each entry is
    /// the number of transcript entries that came off the front, exactly as
    /// given to the store, and [`Self::take_trims`] is how the App drains them.
    pending_trims: Vec<usize>,
    /// Whether the journal file already holds the **beginning** of the entry the
    /// walk is standing on.
    ///
    /// `false` is every ordinary case: entries go to the file whole, so the walk
    /// is always at an entry boundary. The one thing that sets it is
    /// [`Self::trim_open_entry`] cutting the head off a still-open entry: the
    /// head is written, the entry stays open, and "did the file already start
    /// this entry?" stops being answerable from the entry index alone.
    ///
    /// It is a flag rather than a byte count because the cut *removes* those bytes
    /// from the entry — the retained text is entirely not-yet-journalled — so the
    /// only fact worth keeping is whether the next piece continues a sentence the
    /// file has already begun. A continuation must not get the between-entries
    /// blank line; that is what makes `head + retained` the same bytes the
    /// uncapped journal would have written for that entry.
    journal_continued: bool,
    /// The journal cursor: the index of the first transcript entry that has not
    /// been handed to the journal yet.
    ///
    /// A *prefix* cursor, which is what makes the journal ordered by transcript
    /// position rather than by finalisation: the walk stops at the first entry
    /// that is still open, so an answer that finalised behind an open tool card
    /// waits for that card rather than jumping the queue. The wait is bounded by
    /// the card's own runtime, and [`Self::seal`] (death, teardown, cancel)
    /// closes everything and drains the lot.
    journalled: usize,
    /// Whether the journal has any entry on record from this view. Decides
    /// whether a blank line goes before the next chunk, which is what makes the
    /// journal byte-identical to [`Transcript::plain_text`] over the same
    /// entries — "the journal *is* select-all-and-copy" (ADR-0004 R2).
    journalled_any: bool,
    /// Where the transcript goes as it finalises (looprs-pdl.7, ADR-0004 R2).
    ///
    /// Injected, exactly like the clipboard and the dump sink, for the same three
    /// reasons: it touches a filesystem, it can fail in ways the UI must report
    /// rather than handle, and `main` being the only place the real one is built
    /// is what keeps the test suite off the disk by construction. The default is
    /// the disabled journal, never a real writer.
    ///
    /// Held per view because the journal is per session: each mode's transcript
    /// is its own file, and the sink takes the mode name with every chunk so the
    /// one writer can keep them apart and never interleave two sessions into one
    /// document.
    journal: std::sync::Arc<dyn crate::services::journal::Journal>,
    /// The resolver for this view's Bash output (see [`Self::push_bash`]).
    ///
    /// Per-view, and per-stream: shell output is not a string, it is a *stream*
    /// with state — an escape sequence straddles two reads, an SGR style carries
    /// from one line to the next, and the cell a `\r` returns to is the cell
    /// earlier bytes of *this* shell wrote. ADR-0005 is the decision; this field
    /// is where that state lives, and there is exactly one of them per Bash
    /// session, which is what makes "who owns the resolution" a answered
    /// question rather than a rumour.
    shell: LineResolver,
}

impl SessionView {
    /// Transcript and Flusher are created together and only ever used together.
    pub fn new(session: SessionId) -> Self {
        Self::with_buffer(session, DEFAULT_VIEW_BUFFER)
    }

    /// As [`SessionView::new`], with an explicit buffer cap (0 = unbounded). Tests
    /// use the small-cap form to exercise eviction without streaming 256 KiB.
    pub fn with_buffer(session: SessionId, limit: usize) -> Self {
        Self {
            session,
            transcript: Transcript::new(),
            flusher: Flusher::new(),
            scrollback: Scrollback::new(0),
            status: SessionStatus::NotStarted,
            run_started: None,
            last_error: None,
            chat: ChatState::Stopped,
            step: None,
            active_bead: None,
            tokens: Tokens::default(),
            dropped: 0,
            source_skipped: 0,
            journal_continued: false,
            limit,
            pending_trims: Vec::new(),
            journalled: 0,
            journalled_any: false,
            journal: std::sync::Arc::new(crate::services::journal::Disabled),
            shell: LineResolver::new(),
        }
    }

    /// Replace this view's journal.
    ///
    /// Called by the App for every view it creates, from the one handle the App
    /// was given at startup. Setting it after content exists is fine and is what
    /// the App does: the cursor replays nothing, so content journalled before the
    /// swap stays journalled and content after it goes to the new sink.
    pub fn set_journal(&mut self, journal: std::sync::Arc<dyn crate::services::journal::Journal>) {
        self.journal = journal.clone();
        // The trim marker names this session's journal when it has one, so the
        // row that says "N earlier lines dropped" also says where they can still
        // be read. `None` renders the short form rather than pointing at
        // nothing.
        let hint = journal.display_path(self.session.mode.label());
        self.scrollback.set_trim_hint(hint);
    }

    /// Change this view's rendered-store cap (0 = unbounded).
    ///
    /// The measurement/test seam: reaching the default by ordinary means means
    /// rendering a whole long pass, and a test that does that is a slow test
    /// nobody runs.
    #[allow(dead_code)] // consumer: crate::measure (the cap is measured, not tuned at runtime)
    pub fn set_store_cap(&mut self, bytes: usize) {
        self.scrollback_mut().set_cap(bytes);
    }

    /// Shell output into the transcript (ADR-0005, superseding the "strip the
    /// presentation" reading of ADR-0001 rule 5).
    ///
    /// The bytes are *resolved* — SGR becomes a style, `\r`/`\b`/`\t`/`EL` are
    /// applied inside the line, clusters keep their cell count — and the finished
    /// lines go to [`Transcript::push_shell_lines`] with their styles attached.
    /// What is still guaranteed by ADR-0001 rule 1 is unchanged: no markdown, and
    /// no re-wrap by us at any point between the child and the scrollback.
    ///
    /// `term_width` is the width the child is writing for, in cells, and it must
    /// be the **same value that was forwarded to the pty**. That is what makes a
    /// `\r` here land on the row the child believed it was on rather than on the
    /// head of the whole logical line (ADR-0005 Q2).
    pub fn push_bash(&mut self, chunk: &str, term_width: u16) {
        self.shell.set_wrap_width(term_width as usize);
        let lines = self.shell.feed(chunk);
        if lines.is_empty() {
            return;
        }
        self.transcript.push_shell_lines(&lines);
        self.after_write();
    }

    /// Move the line the resolver still has open into the transcript, ended.
    ///
    /// The open line lives in the resolver rather than in the entry so that it can
    /// still be overwritten by the next `\r` — which is the whole reason the
    /// store can stay an append-only list of finished lines. That only works if
    /// "the stream ended" is answered by *somebody*, and every path that ends one
    /// (a different kind of entry opening, the seal, teardown) calls this first.
    /// Nothing typed into a shell is allowed to evaporate because a newline
    /// never arrived before the prompt changed hands.
    fn flush_shell_pending(&mut self) {
        if let Some(line) = self.shell.take_pending() {
            self.transcript
                .push_shell_lines(std::slice::from_ref(&line));
        }
    }

    /// Close off the shell output gathered so far, so the command about to run
    /// starts a fresh transcript entry (looprs-pdl.13).
    ///
    /// This is the *making* of the command boundary that
    /// [`Transcript::last_command_output`] reads. Without it a Bash session's
    /// whole life is one entry, because a stream of one kind is one entry by
    /// design, and "copy the last command's output" would silently mean
    /// "everything since the shell started" — a copy that widens itself is worse
    /// than one that refuses.
    ///
    /// The pending resolver line is flushed first and deliberately: it is almost
    /// always the prompt the shell came back to, which belongs to the block that
    /// just finished. Leaving it in the resolver and sealing underneath would let
    /// it land at the *front* of the next command's entry, which is the same
    /// bytes in the wrong place — the one mistake this file's whole
    /// text-and-styles-travel-together rule exists to prevent.
    ///
    /// Called at submit rather than at command completion because submit is the
    /// moment the boundary is known. The shell itself does not report where one
    /// command ends and the next begins, and inferring it from a prompt pattern
    /// would break on every shell that is not bash.
    pub fn seal_shell_output(&mut self) {
        self.flush_shell_pending();
        self.transcript.seal_command();
        // The seal finalises the block it just closed, so it is a write as far as
        // the journal and the cap are concerned: sealed and not journalled is the
        // same loss as never sealed, and the seal is the moment that block is
        // known to be finished.
        self.after_write();
    }

    /// Rows that became final since the last call. Call once per frame, for the
    /// active view only; each one is also appended to
    /// [`Self::scrollback`](SessionView::scrollback), which is what the frame's
    /// transcript band draws.
    ///
    /// Invariant preserved here: monotonic. Every finalized line of this transcript
    /// is returned exactly once, ever — and, for the same reason, appears in the
    /// store exactly once. The flusher is the only door to either, which is what
    /// makes "the band shows each line exactly once" a property of the type rather
    /// than of the caller.
    ///
    /// The width is not an incidental argument: it is the geometry the stored rows
    /// are wrapped for. A call at a new width re-wraps the whole store first
    /// ([`Self::rewrap`]) before anything new goes in, because a store that is
    /// half one width and half another renders as text that stops making sense at
    /// the seam — which is why this is one door and not two.
    ///
    /// How many rows **became final** on this call and went into the store.
    ///
    /// It used to hand back the rows themselves, cloned. That clone was the whole
    /// batch, every frame, at the store's own measured 5,760 bytes of heap per
    /// row — and nothing on the display ever read it: the band draws from
    /// [`App::transcript_window`], which is the store's own window, and the run
    /// loop calls this for the side effect and drops the value. A count carries
    /// the same fact ("N rows arrived") at no cost, and a caller that wants the
    /// text reads the store that already owns it.
    ///
    /// On a width change the store is *rebuilt* rather than extended; the count
    /// stays "what the flusher finalized this call", which is the question the
    /// caller asks, not "how big is the store now".
    pub fn flush(&mut self, width: u16) -> usize {
        if self.scrollback.width() != width {
            // Every stored row is wrapped for a window that no longer exists. The
            // rows that would have been pushed here are already part of what the
            // rebuild makes again, so the rebuild is the one drain — it used to
            // be two, one of them for a return value nobody read.
            return self.rewrap(width);
        }
        let rows = self.flusher.drain_rows(&self.transcript, width);
        let added = rows.len();
        self.scrollback.push(rows);
        added
    }

    /// This view's scrollable store.
    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    /// Mutable access to the scroll state (offset, pin) — the scroll keys drive
    /// the store through here so nothing else can move the view.
    pub fn scrollback_mut(&mut self) -> &mut Scrollback {
        &mut self.scrollback
    }

    /// Re-make the store at a new width, from the **newest slice of the source
    /// that can still fit the cap**, and report how many of the rows it produced
    /// were *not yet emitted* before the rebuild.
    ///
    /// The rows are a *projection* of the entries, and the only honest way to
    /// re-wrap a projection is to make it again from what it projects: the
    /// flusher is reseated and drained at the new width, which leaves its cursor
    /// exactly where the per-frame drains would have left it, so nothing is
    /// emitted twice and nothing is dropped that the store is keeping.
    ///
    /// What it does *not* do any more is re-render the whole transcript and let
    /// the store trim afterwards. That shape cost one full markdown pass over
    /// every entry the view holds to retain the newest slice of them: measured
    /// on a long pass, 30,000 rows materialised (~164 MiB at the code's own
    /// [`ROW_STRUCT_BYTES`] charge) to keep 5,787 of them (32 MiB), for one
    /// column of window drag. The source is now cut at
    /// [`Self::rewrap_source_start`] first, so the transient is O(cap) plus
    /// whatever the drain owed this frame anyway, instead of O(transcript).
    ///
    /// A resize is a drag in practice, and paying one bounded re-render per
    /// frame for a scrollback that stays where the user was looking is the trade
    /// ADR-0004 signed up for when it made the transcript ours to scroll.
    ///
    /// The entries cut off the front of the source leave the store by *never
    /// being made again* rather than by a trim, so they are counted as dropped
    /// here — before the swap, while the rows the store has for them are still
    /// here to count. A trim the user cannot see is the failure this store's
    /// whole marker exists to prevent.
    ///
    /// "Not yet emitted" is taken from the flusher's own position *before* it
    /// is reseated: everything it had already rendered stays rendered, and the
    /// rest is what this call newly finalised. Counting it after the fact is not
    /// possible — the rebuild trims as it goes, so the store's length says what
    /// was kept, not what arrived.
    pub fn rewrap(&mut self, width: u16) -> usize {
        if self.scrollback.width() == width {
            return 0;
        }
        let unseen_from = self.flusher.consumed();
        let start = self.rewrap_source_start(width);
        // Counted off the transcript before the drain, because it is the
        // entries themselves that carry the line count and the drain is where
        // memory gets tight.
        let skipped_lines = self.skipped_source_lines(start);
        self.flusher.reseat(start);
        let rows = self.flusher.drain_rows(&self.transcript, width);
        let added = rows.iter().filter(|r| r.entry >= unseen_from).count();
        self.scrollback.note_source_dropped(start, skipped_lines);
        self.scrollback.rewrap(width, rows);
        self.source_skipped = start;
        added
    }

    /// The oldest transcript entry a re-render at `width` has to cover.
    ///
    /// Everything older is content this store cannot hold at *any* width, so
    /// rendering it is work whose entire output is thrown away.
    ///
    /// The slice is chosen from the back with the store's own measurements as
    /// the price list ([`Scrollback::entry_shapes`]): a row that has been
    /// rendered once before has a *measured* cost at a known width, and
    /// re-rendering it at a new width costs that, scaled by the width ratio,
    /// plus at most one row per logical line for the rounding the narrower band
    /// introduces. Text that has never been rendered has no measurement, so it
    /// is priced at the density the store has already achieved times
    /// [`UNSEEN_ROW_SAFETY`] — an estimate carrying its own margin, not a
    /// number pretending to be a measurement.
    ///
    /// Three rules keep the *tail* safe, because this cuts the source and the
    /// render then runs forward from the cut:
    ///
    /// * everything the flusher has not emitted yet is inside the slice,
    ///   always. Cutting it would be dropping content the frame has never shown,
    ///   and it is what the drain this rebuild replaces was going to render
    ///   anyway;
    /// * the newest entry is inside the slice even when it alone is over the cap
    ///   — the store's own "keep the newest row anyway" rule from
    ///   [`Scrollback::trim`], applied to the source;
    /// * an entry the store holds no rows for ends the walk rather than being
    ///   guessed at. It was trimmed, and everything older with it: it could not
    ///   have come back inside the cap either, so there is nothing to price.
    ///
    /// An *under*-estimate of any entry's cost is not a correctness problem: the
    /// store trims whatever the rebuild leaves over the cap, exactly as it did
    /// when the rebuild covered everything, and counts it.
    ///
    /// What the bound costs is the mirror image: the store fills to a fraction of
    /// its cap after a resize rather than to exactly the cap, because the safe
    /// price of a re-render is above what it will actually cost. Measured on the
    /// pdl.7 corpus a rebuild holds 28–77% of its cap, depending on which way
    /// the window moved and the shape of the text
    /// (`spikes/results/resize-transient.log`) — and the direction it is worst
    /// in is *widening*, where the strict upper price is the row count the entry
    /// already has, so a wider window buys no more history than the narrow one
    /// held. Nothing the user had is lost by that; it just does not reach further
    /// back into what an earlier resize already trimmed. Bought against that is
    /// the whole point of the ticket: 9–25 MiB of transient across a sweep of
    /// widths where rendering the whole source for the same resizes cost
    /// 133–151 MiB, and a figure that stopped growing with the length of the
    /// transcript.
    fn rewrap_source_start(&self, width: u16) -> usize {
        let cap = self.scrollback.cap_bytes();
        let n = self.transcript.entries.len();
        // Unbounded store: nothing to cut back to, and no transient to bound.
        if cap == 0 || n == 0 {
            return 0;
        }
        let unseen = self.flusher.consumed().min(n);
        let shapes = self.scrollback.entry_shapes();
        let old_cw = content_width(self.scrollback.width()).max(1) as usize;
        let new_cw = content_width(width).max(1) as usize;

        // What one byte of never-rendered text costs here, by this store's own
        // achieved density. `MIN_BYTES_PER_ROW` covers the store that has no
        // rows yet and therefore no opinion.
        let rows_total: usize = shapes.iter().map(|s| s.rows).sum();
        let text_total: usize = shapes.iter().map(|s| s.text).sum();
        let bytes_per_row = (text_total / rows_total.max(1)).max(MIN_BYTES_PER_ROW);

        // Everything below is accumulated in **bytes scaled by `new_cw`**, so
        // the width ratio is never rounded — not per entry, not per candidate.
        // Rounding a row up per entry is a row per entry of history the budget
        // refuses to buy, and over sixty entries that is a tenth of the cap
        // thrown away on arithmetic. One rounding at the end, against
        // `cap × new_cw`, is the same decision without the tax.
        let scaled_cap = cap.saturating_mul(new_cw);

        // (1) The unseen tail: kept unconditionally, and its price is spent out
        // of the cap before anything older is considered — it is going to be on
        // the store either way, and the older entries can only have what is left.
        let mut spent: usize = (unseen..n)
            .map(|e| {
                let text = self.transcript.entries[e].text.len();
                let rows = (text / bytes_per_row).max(1) * UNSEEN_ROW_SAFETY;
                price_scaled(rows.saturating_mul(new_cw), text, new_cw)
            })
            .sum();

        // (2) Back over the entries the store already paid for, while they fit.
        let mut start = unseen;
        let mut si = shapes.len();
        for e in (0..unseen).rev() {
            while si > 0 && shapes[si - 1].entry > e {
                si -= 1;
            }
            let Some(shape) = si.checked_sub(1).and_then(|i| shapes.get(i)) else {
                break;
            };
            if shape.entry != e {
                // Rule three: no rows for this entry means it was trimmed away.
                break;
            }
            // `rows × new_cw` for what the entry will make at the new width.
            let rows_scaled = if new_cw >= old_cw {
                // Widening never adds a row to a logical line: the rows it has
                // are the rows it will make again.
                shape.rows.saturating_mul(new_cw)
            } else {
                // Three parts, because an entry's rows are not one thing. The
                // text rows re-flow with the band — `content × old_cw / new_cw`,
                // carried here as `content × old_cw` so the division stays
                // exact. The blank separators do not re-flow: one row at any
                // width. And the rounding the ratio leaves behind is worth at
                // most one extra row per non-empty logical line, which is the
                // `lines` term — the worst case of that rounding, not the
                // ordinary one.
                let content = shape.rows.saturating_sub(shape.blank);
                content
                    .saturating_mul(old_cw)
                    .saturating_add((shape.lines + shape.blank).saturating_mul(new_cw))
            };
            let charge = price_scaled(rows_scaled, shape.text, new_cw);
            if spent.saturating_add(charge) > scaled_cap {
                break;
            }
            spent += charge;
            start = e;
        }
        // Rule two: the newest entry is always in the slice, even if the walk
        // above could not afford it. `start == n` would re-render nothing at all
        // and empty the store on a resize.
        start.min(n - 1)
    }

    /// Content lines in the transcript entries the next rewrap cuts off the
    /// source, counting only the ones no other path has reported.
    ///
    /// `self.source_skipped` is where the previous rewrap left off, so the same
    /// paragraph is not counted by this *and* by the buffer-cap eviction that
    /// later takes the same entries off the front of the transcript.
    fn skipped_source_lines(&self, to_entry: usize) -> usize {
        let from = self.source_skipped.min(to_entry);
        (from..to_entry)
            .filter_map(|e| self.transcript.entries.get(e))
            .map(|e| e.text.lines().count())
            .sum()
    }

    /// The not-yet-final tail, for the live preview region.
    pub fn preview(&self, width: u16) -> Vec<Line<'static>> {
        // A Bash view's live tail is in the resolver, not in the entry: the entry
        // only ever holds lines the resolver *finished*, so the flusher would
        // honestly answer "nothing open" while the shell is mid-line.
        //
        // The pending line needs no guard against "but something else is live":
        // every path that writes a different kind through this view calls
        // `flush_shell_pending` first, so a pending line is, by construction, the
        // newest thing this session said.
        if let Some(line) = self.shell.pending() {
            return vec![restyle(line.to_line(), style_for(&MessageKind::Bash))];
        }
        self.flusher.preview(&self.transcript, width)
    }

    /// Close whatever streamed entry is still open.
    ///
    /// MUST be called when the owning session dies ([`super::SessionEvent::Exited`])
    /// or is torn down. Without it the transcript keeps an entry that is `!done`
    /// forever: the flusher stalls on it, nothing that session already produced
    /// ever reaches the transcript band again, and the live preview shows a
    /// spinner on dead text forever. This is the seam that breaks the one-door
    /// invariant, so it is part of the contract rather than a detail.
    pub fn seal(&mut self) {
        // The line the resolver still had open is part of this tail, and it goes
        // in *before* anything is closed: pushed into the still-open Bash entry,
        // then closed with it. After `finish_last` the entry is done, and the tail
        // would start a second entry — same words, wrong shape.
        self.flush_shell_pending();
        self.transcript.finish_last();
        // ...and so are any cards still open in it. `finish_last` deliberately
        // leaves running tools alone (parallel tools must survive a delta
        // arriving), so without this a tool or compaction that was in flight when
        // the child died holds the flush cursor forever: everything the session
        // said *after* that point — which is most of what the exit drain exists
        // to collect — never reaches the scrollback.
        self.transcript.abandon_open_cards();
        // A half-parsed escape sequence belongs to a stream that is never going to
        // send the rest of it. Left as it is, the next Bash generation's first
        // bytes get eaten by the previous one's dangling `\x1b[`, which shows up
        // as the first line of a brand new shell silently missing its head.
        self.shell.reset();
        // The last chance this session gets to say everything it said: sealing
        // closes the entries the journal's prefix walk was waiting on, so the
        // tail of a dead or torn-down session reaches the file rather than staying
        // behind an open card. This is not the journal's mechanism — R2 is
        // append-as-you-finalise, and by the time we get here the normal case has
        // been flushed entry by entry — it is the drain of work the walk could
        // not finish on its own.
        self.after_write();
    }

    /// A finished, one-shot line (status notices, the mode-switch separator).
    pub fn push_note(&mut self, kind: MessageKind, text: String) {
        self.flush_shell_pending();
        self.transcript.push_done(kind, text);
        self.after_write();
    }

    /// A streaming delta from this session's backend.
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        self.flush_shell_pending();
        self.transcript.push_delta(kind, delta);
        self.after_write();
    }

    /// A session-level error: recorded for the status row and shown.
    pub fn push_error(&mut self, text: String) {
        self.last_error = Some(text.clone());
        self.flush_shell_pending();
        self.transcript.push_done(MessageKind::Error, text);
        self.after_write();
    }

    // ─────────────── the card kinds, through the same door ───────────────
    //
    // A tool card and a compaction card used to be written straight into
    // `view.transcript` by the App, which meant the two things `after_write`
    // exists to do did not happen for them: no cap, and no journal. Tool
    // results are the **biggest content in a beads pass** — a `bd show`, a
    // file read, a `git diff` — so the cap that was supposed to bound a
    // session's memory did not bound the part of it that actually costs
    // anything, and the journal that was supposed to survive a crash never saw
    // the part worth reading. Both are fixed by the same move: the App asks the
    // view to write the card, and the view runs the edge.

    /// A tool started: opens the card entry.
    pub fn start_tool(&mut self, id: String, name: String, input: String) {
        self.flush_shell_pending();
        self.transcript.start_tool(id, name, input);
        self.after_write();
    }

    /// A tool came back: fills the card in and finalises it.
    ///
    /// Note this finalises an entry that is *not* the last one — a tool that
    /// reported back behind an open sibling, or behind streamed prose, lands in
    /// the middle of the transcript. That is exactly what the journal's prefix
    /// walk is built for: the entry is journalled when the walk reaches it, and
    /// the walk cannot pass an entry that is still open, so file order is
    /// transcript order.
    pub fn finish_tool(&mut self, id: String, summary: String, is_error: bool) {
        self.transcript.finish_tool(id, summary, is_error);
        self.after_write();
    }

    /// pi is pausing the run to compact the context.
    pub fn start_compaction(&mut self, reason: String) {
        self.flush_shell_pending();
        self.transcript.start_compaction(reason);
        self.after_write();
    }

    /// The compaction finished, one way or another.
    ///
    /// Returns whether there was a card to close; `false` is the caller's cue to
    /// record the event itself (see the App's arm), and this records nothing.
    pub fn finish_compaction(
        &mut self,
        state: crate::components::compaction::CompactionState,
        detail: String,
    ) -> bool {
        let closed = self.transcript.finish_compaction(state, detail);
        self.after_write();
        closed
    }

    /// Close the streamed entry that is open, if any: the point a turn's prose
    /// stops being live and starts being transcript.
    ///
    /// The `message_end` arm of the App's event handling, with the edge that goes
    /// with it — an answer that ended is an answer the journal should have.
    pub fn finish_stream(&mut self) {
        self.transcript.finish_last();
        self.after_write();
    }

    /// Mirror the owning session's liveness, and wind the run clock.
    ///
    /// The run clock is *this run*, not *this session*: the clock starts on the way
    /// into a busy state and is cleared on the way out, so a session that goes idle
    /// and busy again starts a new clock rather than inheriting an age hours old.
    /// Writing it only when busy-ness actually *changes* is what makes both halves
    /// idempotent — a session that publishes `Running` twice (a mirror refresh, a
    /// duplicate edge) leaves the age the row is showing alone, and `Idle → Dead`
    /// has nothing left to clear.
    ///
    /// This no longer touches the input box. Input availability is *derived* in
    /// [`Self::accepts_input`] from the mode plus this same status, so a liveness
    /// edge carries no opinion about the keyboard and there is nothing here left to
    /// keep in step with anything.
    pub fn set_status(&mut self, status: SessionStatus, now: Instant) {
        if status.is_busy() != self.status.is_busy() {
            self.run_started = status.is_busy().then_some(now);
        }
        // A child that is gone cannot still be streaming into the live region, so
        // stop the preview rather than spinning a spinner over dead text.
        // (`seal()` closes the transcript itself; this closes the *preview* of it.)
        if !status.is_alive() {
            self.chat = ChatState::Stopped;
        }
        self.status = status;
    }

    /// May this mode take typed input right now?
    ///
    /// One rule, and it is the whole rule: **an agentic mode owns the keyboard
    /// while it works; Bash never owns it.** So during a pi run or a beads pass
    /// the user still has a terminal — exactly one of them, the Bash one — and no
    /// agent ever has a box competing with the run it is in the middle of.
    ///
    /// Derived rather than stored, and that is the point. This used to be a field
    /// written from three places — `set_status` on death, `set_step` on every
    /// beads transition, and the boot seed in `main.rs` — which meant "can I type
    /// here?" had three potential answers depending on which message landed last,
    /// and `set_status` had to carry the arithmetic to reconcile them. Computed
    /// here, there is one source of truth, and two useful consequences come free
    /// with no special case: a session that never started, and a session whose
    /// child died, are both "not busy", so neither can hold the keyboard hostage.
    pub fn accepts_input(&self) -> bool {
        match self.session.mode {
            // A shell is never mid-turn from our point of view. Its command may
            // have been running for an hour and the user can still type into it
            // (Ctrl-C is the shell's, not ours); this is the mode that stays open
            // while the agents are working.
            TerminalType::Bash => true,
            // The agentic modes own the keyboard for the duration of a turn —
            // including the `Aborting` window, where the turn is still unwinding
            // and is still an agentic workflow occurring.
            TerminalType::Pi | TerminalType::Beeds => !self.status.is_busy(),
        }
    }

    /// How long this session's current run has been going, measured at `now`.
    ///
    /// `None` unless a run is live: "idle for 4 minutes" is not a thing the row
    /// should imply, and an age that keeps counting after the run ended is worse
    /// than no age at all. `now` comes from the caller's tick, never from here, so
    /// the render path reads no clock.
    pub fn run_elapsed(&self, now: Instant) -> Option<Duration> {
        if !self.status.is_busy() {
            return None;
        }
        Some(
            self.run_started
                .map(|s| now.saturating_duration_since(s))
                .unwrap_or_default(),
        )
    }

    /// The beads machine moved: record the step, for the status row.
    ///
    /// This used to gate the input box as well, on [`BeadStep::awaits_user`]. That
    /// second gate is gone because it was a copy of a fact already on the wire: the
    /// beads loop publishes `Idle` for precisely the state `AwaitInput` describes
    /// — "started, no worker up, waiting for a human" (`BeadsTask::status`) — so
    /// the step and the liveness mirror are one sentence said twice, and
    /// [`Self::accepts_input`] reads the copy every mode has rather than keeping a
    /// second lock that can rust out of sync with the first.
    ///
    /// The step still lives here because the row renders it and must not re-derive
    /// it (looprs-msj). It is a label now, not a lock.
    pub fn set_step(&mut self, step: BeadStep) {
        self.step = Some(step);
    }

    /// How many bytes of this view's output the buffer cap has dropped.
    ///
    /// Bytes, because that is the unit the cap is a cap in. The *visible* record
    /// of a trim is not here — it is the store's
    /// [`TrimMarker`](crate::state::scrollback::RowKind) row, which says the
    /// loss in lines and sits at the head of what was kept. Keeping the notice
    /// out of the transcript is deliberate: a notice written into the
    /// transcript is a notice that gets copied (`Ctrl-S a`, `plain_text`) and
    /// journalled, which makes the app's own bookkeeping part of the user's
    /// content. This number is for the log.
    #[allow(dead_code)] // measurement/log seam: `view::tests` asserts the count against the corpus it streamed
    pub fn dropped_bytes(&self) -> usize {
        self.dropped
    }

    /// Every write to this view's transcript ends here, and only here.
    ///
    /// Two things happen on this edge, in this order:
    ///
    /// 1. [`Self::journal_finalised`] hands whatever just became final to the
    ///    journal, and
    /// 2. [`Self::enforce_buffer`] trims what has grown past the cap.
    ///
    /// The order is load-bearing and it is the reason the two are one function
    /// rather than two calls at every call site: journal **before** trim, so
    /// content that is about to leave memory has already been handed to the
    /// file. Done the other way round, the cap eats the history the journal
    /// exists to keep, which is the exact failure ADR-0004 R2 was written to
    /// close — and it is silent, because by the time anyone looks, the bytes are
    /// neither in memory nor on disk.
    fn after_write(&mut self) {
        self.journal_finalised();
        self.enforce_buffer();
    }

    /// Push every entry that has become final, and only those, to the journal.
    ///
    /// The chunk is built to the same join as [`Transcript::plain_text`] — entry
    /// text with trailing newlines off, entries separated by a blank line — so
    /// the journal of a whole run is byte-for-byte the same document the
    /// `Ctrl-S a` dump of that run writes (ADR-0004 R2). The rule lives in both
    /// places on purpose: this one appends it as it finalises, that one answers
    /// for a transcript in one go, and the claim that they agree is what
    /// `the_journal_is_what_select_all_and_copy_would_have_given` checks. If
    /// `plain_text`'s join ever changes, that test fails and this one follows it
    /// rather than the journal quietly drifting to a different document.
    ///
    /// Entries with no text are skipped without being journalled, and the cursor
    /// still moves past them: an empty tool summary is not a missing paragraph.
    fn journal_finalised(&mut self) {
        let mut chunk = String::new();
        while self.journalled < self.transcript.entries.len() {
            let e = &self.transcript.entries[self.journalled];
            // The prefix walk stops at the first still-open entry. Ordering in
            // the file is transcript order, not finalisation order; a card that
            // has not come back holds up what arrived behind it for as long as it
            // takes, and `seal` empties the pipe when the stream ends.
            if !e.done {
                break;
            }
            let body = e.text.trim_end_matches('\n');
            self.journalled += 1;
            // The entry is in the file now, whole or finish-it-off: whatever comes
            // next starts a new entry and gets the ordinary between-entries join.
            self.journal_continued = false;
            if body.is_empty() {
                continue;
            }
            // No separator on a *continuation*: this entry already began in the
            // file as the head of a cut that `trim_open_entry` made, and putting
            // a blank line in the middle of it would make the journal a
            // different document from the transcript it is meant to be.
            if self.journalled_any && !self.journal_continued {
                // Two newlines, because that is the join `plain_text` uses: the
                // entry's own line ending plus a blank line between entries.
                // Matching it exactly is the point — the journal of a whole run
                // and the `Ctrl-S a` dump of that run are then the same bytes,
                // which is what makes either one safe to hand to someone.
                chunk.push_str("\n\n");
            }
            chunk.push_str(body);
            chunk.push('\n');
            self.journalled_any = true;
        }
        if chunk.is_empty() {
            return;
        }
        self.journal.append(self.session.mode.label(), chunk);
    }

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
    fn enforce_buffer(&mut self) {
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
        // Always keep one entry: an empty transcript with the cursor past the end is
        // a state nothing downstream is written to expect.
        while self.transcript.byte_len() > self.limit && self.transcript.entries.len() > 1 {
            let gone = self.transcript.entries.remove(0);
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

        // Re-base the presentation against the bytes going away. A run wholly
        // inside the head goes; a run straddling the cut keeps the part that is
        // still here, which is exactly what saturating the start does.
        let e = &mut self.transcript.entries[idx];
        e.text.drain(..cut);
        if !e.styles.is_empty() {
            e.styles.retain(|s| s.end > cut);
            for s in e.styles.iter_mut() {
                s.start = s.start.saturating_sub(cut);
                s.end -= cut;
            }
        }
        // The flusher reads this entry from a byte offset; follow the bytes down.
        if self.flusher.consumed() == idx {
            self.flusher.cut_front(cut);
        }
        self.dropped += cut;
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

// ─────────────────── the chord table (looprs-pdl.13) ───────────────────
//
/// Every key the app claims, in every mode, in every state of the screen, with
/// exactly one owner each. This is the table the ticket asks for, and it is data
/// rather than prose for one reason: the rules written against it are tests, and a
/// rule checked against a comment is a rule checked by nobody.
///
/// **The rows are the order of decision**, and [`App::on_key`] executes them in
/// that same order: the control chords first (`Ctrl-C`, `Ctrl-Q`, `Ctrl-S`, all
/// three claimed before anything else because all three have to survive every
/// other state), then the armed chord's second key, then the passthrough handover,
/// then `Esc`-clears-the-selection, then the scroll keys, then the input box.
/// A row appearing above another row is the same statement as "handles the key
/// first", which is what makes the no-shadowing audit mean something.
///
/// ```text
/// MODE        KEY              STATE          OWNER     EFFECT
/// Pi, Beads   Ctrl-C         plain          App       quit (no Cancel worth the name yet)
/// Bash        Ctrl-C         plain          Shell     0x03 → SIGINT the foreground group
/// all         Ctrl-Q         plain          App       quit
/// all         Ctrl-S         plain          App       arm the copy chord (help toast is the window)
/// all         a              armed          App       copy the last answer
/// all         o              armed          App       copy the last command's output
/// all         s              armed          App       copy the live selection
/// all         t              armed          App       write the whole transcript to a file
/// all         ?              armed          App       show the chord help
/// all         Esc            armed          App       cancel the chord, and nothing else
/// Bash        Ctrl-C         child holds    Shell     0x03 → SIGINT
/// Bash        Ctrl-Q         child holds    App       quit
/// Bash        Ctrl-S         child holds    App       swallowed: XOFF is never forwarded
/// Bash        any other      child holds    Child     forwarded as the bytes the terminal sent
/// all         Esc            selection live App       clear the selection, nothing sent
/// all         Esc            plain          session   the mode's cancel (ADR-0003)
/// all         Tab            plain          App       switch mode (clears selection, chord, throttle)
/// all         Shift-Tab      plain          Box       newline
/// all         Shift-Enter    plain          Box       newline
/// all         Enter          plain          Box       submit
/// all         PageUp         plain          App       one page up (unpins)
/// all         PageDown       plain          App       one page down (re-pins at the tail)
/// all         Home           plain          App       top of the transcript
/// all         End            plain          App       bottom, and re-pin
/// all         anything else  plain          Box       typing
/// ```
///
/// The three rules the audit below runs against it are the ticket's:
///
/// 1. **`Ctrl-C` is not copy.** In Bash mode it is SIGINT and stays SIGINT. No
///    row in this table may pair a `Ctrl-C` with a copy or a dump, in any mode.
/// 2. **Nothing added may shadow an existing binding in the mode it is added
///    to**, and the audit covers the modes' *differences*: the same key must
///    have exactly one owner per (mode, state), which is why `Ctrl-C` gets two
///    rows and why "the child holds the screen" is a state at all rather than a
///    footnote.
/// 3. **`Esc`'s branch is written down per state**, with the "was a selection
///    live?" question explicit — and, since looprs-pdl.13 added one, with the
///    pending-chord state ahead of it: `Esc` undoes the most recent thing the
///    user gave us, never something older and louder.
///
/// The state of the screen a chord is being decided in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChordState {
    /// We hold the screen, nothing is outstanding, no selection is live.
    Plain,
    /// A selection is live (made by the mouse, or by any other means).
    SelectionLive,
    /// `Ctrl-S` is outstanding and the next key is the target.
    ChordArmed,
    /// A full-screen child holds the real terminal (ADR-0001 Q2).
    ChildHolds,
}

/// Who ends up with the keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// looprs's own chord/scroll layer.
    App,
    /// The shell, by way of bytes down the pty.
    Shell,
    /// The mode's session (the cancel that ADR-0003 owns).
    Session,
    /// The input box.
    Box,
    /// A full-screen child program, forwarded verbatim.
    Child,
}

impl Effect {
    /// The noun `copy_chord_hint` pairs with this effect's sub-key: what arming
    /// the prefix and pressing this target gets you. Only the copy family has a
    /// hint; everything else in the table is not a thing the chord offers.
    fn hint(self) -> Option<&'static str> {
        match self {
            Effect::CopyAnswer => Some("answer"),
            Effect::CopyLastOutput => Some("last output"),
            Effect::CopySelection => Some("selection"),
            Effect::DumpTranscriptFile => Some("transcript to file"),
            Effect::CancelChord => Some("cancel"),
            _ => None,
        }
    }
}

/// What the app does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// `0x03` to the pty master: SIGINT to the foreground process group.
    SigInt,
    /// The mode's own cancel.
    CancelRun,
    ClearSelection,
    ArmChord,
    CancelChord,
    CopyAnswer,
    CopyLastOutput,
    CopySelection,
    DumpTranscriptFile,
    ChordHelp,
    ScrollUp,
    ScrollDown,
    Top,
    Tail,
    SwitchMode,
    Newline,
    Submit,
    Typing,
    /// Forwarded raw to a full-screen child.
    Forwarded,
    /// Deliberately thrown away. The only one in the table is XOFF: we own
    /// `Ctrl-Q`, so a `Ctrl-S` we forwarded could stop a child the user then had
    /// no chord left to restart.
    Swallowed,
}

/// A key, spelled the way the table spells it, and constructible into the real
/// keystroke the driving tests send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySym {
    CtrlC,
    CtrlQ,
    CtrlS,
    /// The chord's second keys.
    TargetAnswer,
    TargetOutput,
    TargetSelection,
    TargetTranscript,
    Help,
    Esc,
    Tab,
    ShiftTab,
    Enter,
    ShiftEnter,
    PageUp,
    PageDown,
    Home,
    End,
    /// Any key not named above.
    AnyOther,
}

impl KeySym {
    /// The real keystroke this row talks about.
    ///
    /// #[allow(dead_code)] is below: the shipped binary never needs to turn a row
    /// back into a KeyEvent — the driving tests do, which is how the table is
    /// checked against the real handler instead of against a description of it.
    #[allow(dead_code)] // test seam: driven tests replay the table's chords
    /// For [`KeySym::AnyOther`] this is a plain `x`: the row's *meaning* is
    /// "anything unnamed", and `x` is the representative the driving tests use —
    /// chosen because nothing in the table names it, so a test that sends it is
    /// really sending the fallback and not a chord that happens to match.
    pub fn event(self) -> crossterm::event::KeyEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
        match self {
            KeySym::CtrlC => KeyEvent::new(KeyCode::Char('c'), M::CONTROL),
            KeySym::CtrlQ => KeyEvent::new(KeyCode::Char('q'), M::CONTROL),
            KeySym::CtrlS => KeyEvent::new(KeyCode::Char('s'), M::CONTROL),
            KeySym::TargetAnswer => KeyEvent::new(KeyCode::Char('a'), M::NONE),
            KeySym::TargetOutput => KeyEvent::new(KeyCode::Char('o'), M::NONE),
            KeySym::TargetSelection => KeyEvent::new(KeyCode::Char('s'), M::NONE),
            KeySym::TargetTranscript => KeyEvent::new(KeyCode::Char('t'), M::NONE),
            KeySym::Help => KeyEvent::new(KeyCode::Char('?'), M::NONE),
            KeySym::Esc => KeyEvent::new(KeyCode::Esc, M::NONE),
            KeySym::Tab => KeyEvent::new(KeyCode::Tab, M::NONE),
            KeySym::ShiftTab => KeyEvent::new(KeyCode::BackTab, M::SHIFT),
            KeySym::Enter => KeyEvent::new(KeyCode::Enter, M::NONE),
            KeySym::ShiftEnter => KeyEvent::new(KeyCode::Enter, M::SHIFT),
            KeySym::PageUp => KeyEvent::new(KeyCode::PageUp, M::NONE),
            KeySym::PageDown => KeyEvent::new(KeyCode::PageDown, M::NONE),
            KeySym::Home => KeyEvent::new(KeyCode::Home, M::NONE),
            KeySym::End => KeyEvent::new(KeyCode::End, M::NONE),
            KeySym::AnyOther => KeyEvent::new(KeyCode::Char('x'), M::NONE),
        }
    }

    /// Is this a *copy* chord? Rule 1 is stated in terms of this: nothing whose
    /// effect moves text out of the app may be reachable on `Ctrl-C`.
    #[allow(dead_code)] // audit-only: rule 1 of the table audit is stated over this
    pub fn is_copy(self) -> bool {
        matches!(
            self,
            KeySym::TargetAnswer
                | KeySym::TargetOutput
                | KeySym::TargetSelection
                | KeySym::TargetTranscript
        )
    }
}

/// One row of [`CHORD_TABLE`].
///
/// The shipped binary reads `state`, `keys` and `does` — that is what
/// [`copy_chord_hint`] is built from. `mode`, `key`, `owner` and `note` exist so
/// the audit in `tests` can state the rules over the whole row; the table is the
/// artifact that gets checked, not a structure the key path walks.
#[allow(dead_code)] // four of the seven columns are the audit's, three are production's
pub struct ChordRow {
    /// The mode this row applies to. Every row names one mode explicitly: a row
    /// that said "all modes" would hide the fact that `Ctrl-C` does not mean the
    /// same thing in all of them.
    pub mode: TerminalType,
    /// The key.
    pub key: KeySym,
    /// How it is spelled in the table above, and in a failure message.
    pub keys: &'static str,
    /// The state this row applies in.
    pub state: ChordState,
    /// Who ends up with it.
    pub owner: Owner,
    /// What happens.
    pub does: Effect,
    /// Why, in one line — the part a reader of the code needs and a reader of
    /// the enum cannot carry.
    pub note: &'static str,
}

/// The table. See the comment above it.
pub const CHORD_TABLE: &[ChordRow] = &[
    // ── the control chords, claimed before anything else ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::Plain,
        owner: Owner::Shell,
        does: Effect::SigInt,
        note: "the shell's own key: 0x03 to the pty master, the foreground group gets SIGINT, \
               the app stays up — and it is never, in any mode, a copy",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit for now: Pi mode has no Cancel worth the name until looprs-5g7, and a copy \
               chord is not what Ctrl-C becomes",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "same as Pi",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit without touching the shell; this is also why Ctrl-S cannot be forwarded \u{2014} \u{2014} \u{2018} is ours",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit in every mode",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Quit,
        note: "quit in every mode",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ArmChord,
        note: "the copy prefix: a prefix rather than four top-level chords, because the chord budget is the whole difficulty here",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ArmChord,
        note: "the copy prefix, armed",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ArmChord,
        note: "the copy prefix, armed",
    },
    // ── the chord's second key, armed only ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetAnswer,
        keys: "Ctrl-S a",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyAnswer,
        note: "in Bash this *refuses* (there are no answers here) and names the chord that works",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetAnswer,
        keys: "Ctrl-S a",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyAnswer,
        note: "the last answer, whole, through the looprs-pdl.10 sink and toast",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetAnswer,
        keys: "Ctrl-S a",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyAnswer,
        note: "the last answer, whole, through the looprs-pdl.10 sink and toast",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetOutput,
        keys: "Ctrl-S o",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyLastOutput,
        note: "the last sealed shell block: this command's echo, output and prompt, and nothing before it",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetOutput,
        keys: "Ctrl-S o",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyLastOutput,
        note: "the agentic modes' answer to the same question: the last finished tool card",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetOutput,
        keys: "Ctrl-S o",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopyLastOutput,
        note: "the agentic modes' answer to the same question: the last finished tool card",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetSelection,
        keys: "Ctrl-S s",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopySelection,
        note: "whatever selection is live; a keyboard-driven selection was *not* one of the things that landed in this ticket, and this target is ready for it",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetSelection,
        keys: "Ctrl-S s",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopySelection,
        note: "whatever selection is live",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetSelection,
        keys: "Ctrl-S s",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CopySelection,
        note: "whatever selection is live",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::TargetTranscript,
        keys: "Ctrl-S t",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::DumpTranscriptFile,
        note: "the escape hatch: the whole transcript to a timestamped file, through an injected sink like the clipboard's",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::TargetTranscript,
        keys: "Ctrl-S t",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::DumpTranscriptFile,
        note: "the escape hatch",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::TargetTranscript,
        keys: "Ctrl-S t",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::DumpTranscriptFile,
        note: "the escape hatch",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Help,
        keys: "Ctrl-S ?",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::ChordHelp,
        note: "the family, listed in a toast: the chords have to be reachable from inside the app",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Help,
        keys: "Ctrl-S ?",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::ChordHelp,
        note: "the family, listed in a toast",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Help,
        keys: "Ctrl-S ?",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::ChordHelp,
        note: "the family, listed in a toast",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CancelChord,
        note: "undoes the prefix and nothing else \u{2014} it is deliberately not the mode's cancel",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CancelChord,
        note: "undoes the prefix and nothing else",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::ChordArmed,
        owner: Owner::App,
        does: Effect::CancelChord,
        note: "undoes the prefix and nothing else",
    },
    // ── the child holds the screen ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlC,
        keys: "Ctrl-C",
        state: ChordState::ChildHolds,
        owner: Owner::Shell,
        does: Effect::SigInt,
        note: "still SIGINT: a full-screen child does not take Ctrl-C away from the shell it is already in",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlQ,
        keys: "Ctrl-Q",
        state: ChordState::ChildHolds,
        owner: Owner::App,
        does: Effect::Quit,
        note: "ours in every state; the child is killed on the way out",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::CtrlS,
        keys: "Ctrl-S",
        state: ChordState::ChildHolds,
        owner: Owner::App,
        does: Effect::Swallowed,
        note: "never forwarded: XOFF into a pty whose XON (Ctrl-Q) we own is a freeze the user cannot undo",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::ChildHolds,
        owner: Owner::Child,
        does: Effect::Forwarded,
        note: "a program that owns the screen owns the keyboard (ADR-0001 Q2) \u{2014} Esc included, so vim leaves insert mode",
    },
    // ── the `Esc` branch, per mode, with the question the ticket asks shown ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::SelectionLive,
        owner: Owner::App,
        does: Effect::ClearSelection,
        note: "was a selection live? yes \u{2014} the first Esc unselects and sends nothing",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::SelectionLive,
        owner: Owner::App,
        does: Effect::ClearSelection,
        note: "was a selection live? yes \u{2014} the first Esc unselects and sends nothing",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::SelectionLive,
        owner: Owner::App,
        does: Effect::ClearSelection,
        note: "was a selection live? yes \u{2014} the first Esc unselects and sends nothing",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::Plain,
        owner: Owner::Session,
        does: Effect::CancelRun,
        note: "was a selection live? no \u{2014} the Esc is the cancel the mode table already describes",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::Plain,
        owner: Owner::Session,
        does: Effect::CancelRun,
        note: "was a selection live? no \u{2014} the cancel, and the queued text comes back to the box",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Esc,
        keys: "Esc",
        state: ChordState::Plain,
        owner: Owner::Session,
        does: Effect::CancelRun,
        note: "was a selection live? no \u{2014} the cancel",
    },
    // ── the rest of the table, uniform across modes ──
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Tab,
        keys: "Tab",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::SwitchMode,
        note: "switch mode; the selection, the chord and the wheel throttle all clear with it",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Tab,
        keys: "Tab",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::SwitchMode,
        note: "switch mode",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Tab,
        keys: "Tab",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::SwitchMode,
        note: "switch mode",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::ShiftTab,
        keys: "Shift-Tab",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "newline in the input box — Shift-Enter never sends a shell command",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::ShiftTab,
        keys: "Shift-Tab",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "newline in the input box",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::ShiftTab,
        keys: "Shift-Tab",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "newline in the input box",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::ShiftEnter,
        keys: "Shift-Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "a newline in the box, not a submit",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::ShiftEnter,
        keys: "Shift-Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "a newline in the box",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::ShiftEnter,
        keys: "Shift-Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Newline,
        note: "a newline in the box",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Enter,
        keys: "Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Submit,
        note: "submit; in Bash this is also where the command boundary is sealed for Ctrl-S o",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Enter,
        keys: "Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Submit,
        note: "submit",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Enter,
        keys: "Enter",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Submit,
        note: "submit",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::PageUp,
        keys: "PageUp",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollUp,
        note: "one page up, unpinning the tail (looprs-pdl.8's semantics, the same store)",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::PageUp,
        keys: "PageUp",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollUp,
        note: "one page up, unpinning the tail",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::PageUp,
        keys: "PageUp",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollUp,
        note: "one page up, unpinning the tail",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::PageDown,
        keys: "PageDown",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollDown,
        note: "one page down; reaching the bottom re-pins",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::PageDown,
        keys: "PageDown",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollDown,
        note: "one page down; reaching the bottom re-pins",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::PageDown,
        keys: "PageDown",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::ScrollDown,
        note: "one page down; reaching the bottom re-pins",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::Home,
        keys: "Home",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Top,
        note: "top of the transcript",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::Home,
        keys: "Home",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Top,
        note: "top of the transcript",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::Home,
        keys: "Home",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Top,
        note: "top of the transcript",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::End,
        keys: "End",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Tail,
        note: "bottom, and re-pin: what the \u{201c}N new\u{201d} affordance names",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::End,
        keys: "End",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Tail,
        note: "bottom, and re-pin",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::End,
        keys: "End",
        state: ChordState::Plain,
        owner: Owner::App,
        does: Effect::Tail,
        note: "bottom, and re-pin",
    },
    ChordRow {
        mode: TerminalType::Bash,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Typing,
        note: "typing, to whichever mode's box is up",
    },
    ChordRow {
        mode: TerminalType::Pi,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Typing,
        note: "typing",
    },
    ChordRow {
        mode: TerminalType::Beeds,
        key: KeySym::AnyOther,
        keys: "anything else",
        state: ChordState::Plain,
        owner: Owner::Box,
        does: Effect::Typing,
        note: "typing",
    },
];

/// The hint shown while the copy chord is armed, and what `Ctrl-S ?` prints.
///
/// Read out of the table rather than written again: the string the user reads and
/// the table the audit checks are then the same data. A target added to the table
/// shows up in the hint by itself; a target that is not in the table cannot be
/// advertised.
pub fn copy_chord_hint() -> String {
    let mut parts: Vec<String> = Vec::new();
    for row in CHORD_TABLE
        .iter()
        .filter(|r| r.state == ChordState::ChordArmed)
    {
        let Some(noun) = row.does.hint() else {
            continue;
        };
        // The table spells each chord out in full (`"Ctrl-S a"`); the hint wants
        // the sub-key by itself next to the noun it gets you.
        let sub = row.keys.rsplit(' ').next().unwrap_or(row.keys);
        let item = format!("{sub} {noun}");
        if !parts.contains(&item) {
            parts.push(item);
        }
    }
    format!("Copy: {}", parts.join(" \u{b7} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mode: TerminalType) -> SessionView {
        SessionView::new(SessionId::new(mode, 0))
    }

    use std::sync::{Arc, Mutex};

    /// A journal that remembers what it was handed, for the tests that care about
    /// what reached the file rather than what is on disk. The real writer has its
    /// own tests (`services::journal`); duplicating its thread here would test
    /// the scheduler.
    #[derive(Default, Debug)]
    struct RecordingJournal {
        got: Mutex<Vec<(&'static str, String)>>,
    }

    impl crate::services::journal::Journal for RecordingJournal {
        fn append(&self, mode: &'static str, text: String) {
            self.got.lock().unwrap().push((mode, text));
        }
        fn display_path(&self, mode: &str) -> Option<String> {
            Some(format!("/tmp/last-{mode}"))
        }
        fn close(&self) {}
        fn describe(&self) -> &'static str {
            "recording"
        }
    }

    impl RecordingJournal {
        fn text(&self) -> String {
            self.got
                .lock()
                .unwrap()
                .iter()
                .map(|(_, t)| t.as_str())
                .collect::<String>()
        }
    }

    fn with_journal(v: &mut SessionView, j: Arc<RecordingJournal>) {
        v.set_journal(j);
    }

    // ─────────────── the journal is the escape hatch (looprs-pdl.7) ───────────────

    /// **The cap does not get to be the reason the transcript is gone.** The
    /// whole design of a bounded scrollback rests on this ordering: what
    /// finalised goes to the file before the cap takes it out of memory. Run it
    /// the other way round and the journal holds the same truncated thing the
    /// store does, which is a worse copy of a buffer, not an escape hatch.
    #[test]
    fn the_cap_takes_it_out_of_memory_and_the_file_still_has_it() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        let j = Arc::new(RecordingJournal::default());
        with_journal(&mut v, j.clone());

        for i in 0..12 {
            v.push_note(MessageKind::Answer, format!("line {i} {}", "y".repeat(24)));
            let _ = v.flush(60);
        }
        assert!(
            v.dropped_bytes() > 0,
            "the cap bit into the transcript: {}",
            v.dropped_bytes()
        );
        assert!(
            v.transcript.byte_len() <= 128,
            "and the buffer stayed under it: {}",
            v.transcript.byte_len()
        );
        let journalled = j.text();
        for i in 0..12 {
            assert!(
                journalled.contains(&format!("line {i} ")),
                "entry {i} went missing from the journal, which is the whole \
                 point of having one: {journalled:?}"
            );
        }
        assert!(
            journalled.len() > v.transcript.plain_text().len(),
            "the file holds more than memory does — that is the inequality the \
             marker row is promising: file {} vs memory {}",
            journalled.len(),
            v.transcript.plain_text().len()
        );
    }

    /// **Order is transcript order, not finalisation order.** An answer that
    /// finalised behind an open tool card waits for the card rather than jumping
    /// the queue, so reading the file top to bottom reads like the session
    /// happened.
    #[test]
    fn an_open_entry_holds_the_line_behind_it_rather_than_being_jumped() {
        let mut v = view(TerminalType::Pi);
        let j = Arc::new(RecordingJournal::default());
        with_journal(&mut v, j.clone());

        v.push_note(MessageKind::Answer, "first".into());
        v.start_tool("t1".into(), "a tool still running".into(), String::new());
        v.push_note(MessageKind::Answer, "behind the card".into());
        let _ = v.flush(60);
        let before = j.text();
        assert!(
            before.contains("first"),
            "the closed entry went: {before:?}"
        );
        assert!(
            !before.contains("behind the card"),
            "the entry behind an open card waits: {before:?}"
        );

        v.finish_tool("t1".into(), "the card came back".into(), false);
        v.seal();
        let after = j.text();
        assert!(
            after.contains("first") && after.contains("behind the card"),
            "sealing empties the pipe: {after:?}"
        );
        assert!(
            after.find("first") < after.find("behind the card"),
            "and in the order they were said: {after:?}"
        );
    }

    /// **The journal of a run is what select-all-and-copy would have given**
    /// (ADR-0004 R2). Both are the same rule over the same entries; this is the
    /// test that keeps the two implementations of that rule from drifting.
    #[test]
    fn the_journal_is_what_select_all_and_copy_would_have_given() {
        let mut v = view(TerminalType::Bash);
        let j = Arc::new(RecordingJournal::default());
        with_journal(&mut v, j.clone());
        v.push_note(MessageKind::Answer, "one\n".into());
        v.push_note(MessageKind::System, "two\n\n".into());
        v.push_note(MessageKind::Answer, "three".into());
        v.seal();
        assert_eq!(
            j.text(),
            v.transcript.plain_text(),
            "the file and the copy must be the same document"
        );
    }

    /// Nothing on the exit path invents transcript-shaped bytes (ADR-0004 R3):
    /// an entry that never finalised never reaches the file, and a `close` adds
    /// nothing either.
    #[test]
    fn what_never_finalised_never_reaches_the_file() {
        let mut v = view(TerminalType::Pi);
        let j = Arc::new(RecordingJournal::default());
        with_journal(&mut v, j.clone());
        v.push_note(MessageKind::Answer, "done and journalled".into());
        v.seal();
        let after_seal = j.text();
        assert!(after_seal.contains("done and journalled"));
        crate::services::journal::Journal::close(&*j);
        assert_eq!(
            j.text(),
            after_seal,
            "close is a drain, not a second pass over the transcript"
        );
    }

    /// The marker row names the file, because a marker that says "16,834 lines
    /// dropped" with nowhere to go is a dead end. The hint is taken when the
    /// journal is installed, so a view created before the app had a journal is
    /// not left pointing at nothing.
    #[test]
    fn the_marker_names_the_journal_it_can_send_you_to() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        let j = Arc::new(RecordingJournal::default());
        with_journal(&mut v, j.clone());
        for i in 0..12 {
            v.push_note(MessageKind::Answer, format!("line {i} {}", "y".repeat(24)));
            let _ = v.flush(60);
        }
        let marker = v.scrollback().rows()[0].to_string();
        assert!(marker.contains("scrollback trimmed"), "{marker:?}");
        assert!(
            marker.contains("/tmp/last-"),
            "the marker carries the journal path so the reader has somewhere to go: \
             {marker:?}"
        );
    }

    // ─────────────── the chord table audit (looprs-pdl.13) ───────────────
    //
    // These are the ticket's three rules, run against `CHORD_TABLE` as data. What
    // they cannot do is prove the *code* matches the table — that is what
    // `app::tests`' driven chord tests are for, and each rule names the test that
    // drives it. What they can do is make a hole in the table a build failure
    // rather than a surprise at 1 a.m.

    /// Rule 1: `Ctrl-C` is not copy, in any mode.
    #[test]
    fn ctrl_c_is_never_a_copy_in_any_mode() {
        for row in CHORD_TABLE {
            if row.key == KeySym::CtrlC {
                assert!(
                    !row.key.is_copy(),
                    "{}/{}: a copy chord spelled as Ctrl-C in {}",
                    row.mode.label(),
                    row.keys,
                    row.keys
                );
                assert!(
                    matches!(row.does, Effect::SigInt | Effect::Quit),
                    "{}: Ctrl-C must be SIGINT or quit, not {:?}",
                    row.keys,
                    row.does
                );
            }
        }
        // And positively: Bash's Ctrl-C is SIGINT, and stays SIGINT whatever else
        // this table grows.
        let bash = CHORD_TABLE
            .iter()
            .filter(|r| r.mode == TerminalType::Bash && r.key == KeySym::CtrlC);
        assert!(bash.clone().count() >= 1, "Bash must have a Ctrl-C row");
        for row in bash {
            assert_eq!(
                row.does,
                Effect::SigInt,
                "Bash Ctrl-C in state {:?} must be SIGINT",
                row.state
            );
        }
    }

    /// Rule 2: no key has two owners in the same mode and state — which is what
    /// "shadowing" is, spelled as a table constraint.
    ///
    /// The mode *differences* are covered by the fact that the triple includes
    /// the mode: `Ctrl-C` is allowed to mean two different things because it is
    /// two rows in two modes, and is not allowed to mean two things in one mode
    /// because that would be two rows with the same triple.
    #[test]
    fn no_key_has_two_owners_in_the_same_mode_and_state() {
        let mut seen: std::collections::HashMap<(TerminalType, KeySym, ChordState), (Owner, &str)> =
            std::collections::HashMap::new();
        for row in CHORD_TABLE {
            let k = (row.mode, row.key, row.state);
            if let Some(prev) = seen.insert(k, (row.owner, row.keys)) {
                assert_eq!(
                    prev,
                    (row.owner, row.keys),
                    "{:?} in {:?} is claimed twice: {:?} and {:?}",
                    row.mode,
                    row.state,
                    prev,
                    (row.owner, row.keys)
                );
            }
        }
    }

    /// Rule 2, the other half: every mode must say something about every key in
    /// the family the audit cares about. A missing cell is the failure mode a
    /// table exists to catch — the binding nobody thought about, which is the
    /// same thing as a binding nobody can find.
    #[test]
    fn every_mode_has_a_row_for_every_key_that_matters() {
        let keys = [
            KeySym::CtrlC,
            KeySym::CtrlQ,
            KeySym::CtrlS,
            KeySym::TargetAnswer,
            KeySym::TargetOutput,
            KeySym::TargetSelection,
            KeySym::TargetTranscript,
            KeySym::Help,
            KeySym::Esc,
            KeySym::Tab,
            KeySym::Enter,
            KeySym::ShiftEnter,
            KeySym::PageUp,
            KeySym::PageDown,
            KeySym::Home,
            KeySym::End,
            KeySym::AnyOther,
        ];
        for mode in TerminalType::ALL {
            for key in keys {
                let rows: Vec<_> = CHORD_TABLE
                    .iter()
                    .filter(|r| r.mode == mode && r.key == key)
                    .collect();
                assert!(
                    !rows.is_empty(),
                    "{} mode has no row for {:?}: an undocumented key is an unaudited key",
                    mode.label(),
                    key
                );
            }
        }
    }

    /// Rule 3: `Esc`'s branch is written down per mode, with the "was a
    /// selection live?" question explicit — and with the pending chord ahead of
    /// it, because looprs-pdl.13 put a new "most recent thing" in front of both.
    #[test]
    fn the_esc_branch_is_written_down_per_mode() {
        for mode in TerminalType::ALL {
            let armed = CHORD_TABLE.iter().find(|r| {
                r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::ChordArmed
            });
            let live = CHORD_TABLE.iter().find(|r| {
                r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::SelectionLive
            });
            let plain = CHORD_TABLE
                .iter()
                .find(|r| r.mode == mode && r.key == KeySym::Esc && r.state == ChordState::Plain);
            assert!(
                armed.is_some_and(|r| r.does == Effect::CancelChord),
                "{}: Esc with a chord armed must cancel the chord",
                mode.label()
            );
            assert!(
                live.is_some_and(|r| r.does == Effect::ClearSelection),
                "{}: Esc with a live selection must clear the selection",
                mode.label()
            );
            assert!(
                plain.is_some_and(|r| r.does == Effect::CancelRun),
                "{}: Esc with nothing live must be the mode's cancel",
                mode.label()
            );
        }
        // Bash adds the fourth state: the child holds Esc too.
        let held = CHORD_TABLE
            .iter()
            .find(|r| r.mode == TerminalType::Bash && r.state == ChordState::ChildHolds);
        assert!(
            held.is_some_and(|r| r.owner == Owner::Child
                || r.owner == Owner::Shell
                || r.owner == Owner::App),
            "Bash must say what happens to keys while a full-screen child holds the screen"
        );
    }

    /// The table's own arithmetic: three modes, and every row names one of them.
    #[test]
    fn the_table_is_a_table() {
        for row in CHORD_TABLE {
            assert!(
                TerminalType::ALL.contains(&row.mode),
                "{:?} is not a mode",
                row.mode
            );
            assert!(!row.keys.is_empty() && !row.note.is_empty());
        }
        // The copy family is exactly the four targets plus help, and each is in
        // all three modes.
        for key in [
            KeySym::TargetAnswer,
            KeySym::TargetOutput,
            KeySym::TargetSelection,
            KeySym::TargetTranscript,
        ] {
            assert!(key.is_copy(), "{:?} must be a copy key", key);
            assert_eq!(
                CHORD_TABLE.iter().filter(|r| r.key == key).count(),
                3,
                "{:?} must be in all three modes",
                key
            );
        }
    }

    /// The flush invariant, stated as a test: monotonic. Note what the flusher's
    /// own contract is — a prose block is rendered when it *closes* (blank line or
    /// `done`), not on every newline — so these assertions are about the cursor,
    /// not about line counts.
    /// Flush and return **exactly the rows this flush appended**, as text.
    ///
    /// `flush` hands back a count now rather than the batch: the batch was cloned
    /// every real frame for a caller that did not exist (the band reads the
    /// store), so the only reader of those rows was the test suite. The count
    /// names the same rows precisely — the store's tail of that length *is* what
    /// was just pushed, since the trim marker, if there is one, sits at the head
    /// and never at the tail.
    fn flush_new(v: &mut SessionView, w: u16) -> Vec<String> {
        let added = v.flush(w);
        let rows = v.scrollback().rows();
        rows[rows.len() - added..]
            .iter()
            .map(|r| r.to_string())
            .collect()
    }

    /// Every row the store holds, as text — for assertions that do not care
    /// which flush produced the row.
    fn store_text(v: &SessionView) -> String {
        v.scrollback()
            .rows()
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn flush_is_monotonic_per_view() {
        let mut v = view(TerminalType::Pi);
        v.transcript.push_delta(MessageKind::Answer, "line one\n\n");
        let first = v.flush(60);
        assert_ne!(first, 0, "a closed block must flush");

        assert_eq!(
            v.flush(60),
            0,
            "a second drain with no new content must emit nothing"
        );

        v.transcript
            .push_delta(MessageKind::Answer, "second para\n\n");
        let second = v.flush(60);
        assert_ne!(second, 0, "the new block must flush");

        // And nothing is re-emitted: two closed blocks in, then silence.
        assert_eq!(v.flush(60), 0);
    }

    /// The stall this ticket is about: an open entry that is never sealed blocks the
    /// cursor forever — its text can never reach the scrollback, and the live
    /// preview spins over dead text. `seal()` is what unblocks it.
    #[test]
    fn an_unsealed_entry_stalls_the_flusher_and_seal_clears_it() {
        let mut v = view(TerminalType::Beeds);
        v.transcript
            .push_delta(MessageKind::Answer, "partial answer, no newline");
        assert_eq!(
            v.flush(60),
            0,
            "an unterminated, undone entry is not flushed"
        );

        v.seal();
        let after = v.flush(60);
        assert!(
            after > 0
                && v.scrollback()
                    .rows()
                    .iter()
                    .any(|l| !l.line.spans.is_empty()),
            "sealing must release the tail: {after} rows / {}",
            store_text(&v)
        );
        assert_eq!(v.flush(60), 0, "and only once");
    }

    /// The same stall from a card rather than from prose — and a card is where it
    /// bites hardest, because a running tool or compaction is *deliberately* not
    /// closed by whatever streams next (parallel tools). So `seal` closes them
    /// itself, as aborted: nothing is coming to report them, and a frozen spinner
    /// in a transcript whose process is gone is a lie that outlives its subject.
    #[test]
    fn sealing_closes_cards_left_running_so_the_transcript_keeps_flushing() {
        let mut v = view(TerminalType::Pi);
        v.transcript
            .start_tool("t1".into(), "bash".into(), "sleep 100".into());
        v.transcript.start_compaction("threshold".into());
        v.transcript
            .push_delta(MessageKind::Answer, "said after both\n");

        assert_eq!(
            v.flush(60),
            0,
            "the open cards hold the cursor, as they should while the session lives"
        );

        v.seal();
        let _ = v.flush(60);
        let out = store_text(&v);
        assert!(out.contains("said after both"), "{out:?}");
        assert!(
            out.contains("compaction aborted"),
            "the compaction row says how it ended: {out:?}"
        );
        assert!(
            out.contains("sleep 100"),
            "the tool row is not lost: {out:?}"
        );
        for frame in crate::utils::render::FRAMES {
            assert!(
                !out.contains(frame),
                "a dead session's card is still spinning ({frame}): {out:?}"
            );
        }
    }

    /// Two views never share a cursor: draining one cannot consume the other's
    /// lines, and one view streaming cannot advance the other's position.
    #[test]
    fn views_do_not_interfere() {
        let mut pi = view(TerminalType::Pi);
        let mut beads = view(TerminalType::Beeds);

        pi.transcript
            .push_delta(MessageKind::Answer, "pi says hi\n\n");
        beads
            .transcript
            .push_done(MessageKind::System, "working looprs-1".into());

        let pi_lines = pi.flush(60);
        let beads_lines = beads.flush(60);
        assert_ne!(pi_lines, 0, "pi's closed block must appear");
        assert_ne!(beads_lines, 0, "beads' system line must appear");
        assert_eq!(pi.flush(60), 0);
        assert_eq!(beads.flush(60), 0);

        // Pi keeps streaming; the beads view must not budge, and must not gain pi's text.
        pi.transcript.push_delta(MessageKind::Answer, "more pi\n\n");
        assert_ne!(pi.flush(60), 0);
        assert_eq!(
            beads.flush(60),
            0,
            "the beads view consumed nothing it was not given"
        );
    }

    #[test]
    fn errors_are_recorded_for_the_status_row() {
        let mut v = view(TerminalType::Bash);
        v.push_error("shell exited (code 1)".into());
        assert_eq!(v.last_error.as_deref(), Some("shell exited (code 1)"));
    }

    /// **The whole keyboard table**: mode x liveness -> may the user type here?
    ///
    /// Enumerated rather than spot-checked, because this is the one place the
    /// "only Bash while the agents run" rule lives and a table is the only form
    /// that shows a missing cell. Read the Bash column as the reason the mode
    /// exists: the terminal is never taken away.
    #[test]
    fn the_keyboard_table_is_mode_times_liveness() {
        use SessionStatus::*;
        let table = [
            //  mode,            liveness,     accepts input
            (TerminalType::Bash, NotStarted, true),
            (TerminalType::Bash, Idle, true),
            (TerminalType::Bash, Running, true),
            (TerminalType::Bash, Aborting, true),
            (TerminalType::Bash, Dead, true),
            (TerminalType::Pi, NotStarted, true),
            (TerminalType::Pi, Idle, true),
            (TerminalType::Pi, Running, false),
            (TerminalType::Pi, Aborting, false),
            (TerminalType::Pi, Dead, true),
            (TerminalType::Beeds, NotStarted, true),
            (TerminalType::Beeds, Idle, true),
            (TerminalType::Beeds, Running, false),
            (TerminalType::Beeds, Aborting, false),
            (TerminalType::Beeds, Dead, true),
        ];

        assert_eq!(
            table.len(),
            TerminalType::ALL.len() * 5,
            "one row per mode per liveness state"
        );
        for (mode, status, want) in table {
            let mut v = view(mode);
            v.set_status(status, Instant::now());
            assert_eq!(v.accepts_input(), want, "{mode:?} while {status:?}");
        }
    }

    /// The scenario the rule was written for, end to end: the pi run and the beads
    /// pass are both going, the user is not locked out — they are in Bash.
    #[test]
    fn with_pi_and_beads_both_running_bash_is_still_there() {
        let now = Instant::now();
        let mut pi = view(TerminalType::Pi);
        let mut beads = view(TerminalType::Beeds);
        let mut bash = view(TerminalType::Bash);

        pi.set_status(SessionStatus::Running, now);
        beads.set_step(BeadStep::WorkTickets);
        beads.set_status(SessionStatus::Running, now);
        bash.set_status(SessionStatus::Running, now); // `make test` in the other pane

        assert!(!pi.accepts_input() && !beads.accepts_input());
        assert!(
            bash.accepts_input(),
            "the agentic modes are locked, but the shell never is"
        );
    }

    /// The beads step no longer takes the keyboard either way. It is render-only:
    /// the loop's `Idle` *is* "waiting for a human", so gating on the step as
    /// well was a second lock on the same door — and a second lock is a second
    /// thing that can be left engaged after the door opens.
    #[test]
    fn the_step_is_a_label_not_a_lock() {
        let mut beads = view(TerminalType::Beeds);

        beads.set_step(BeadStep::WorkTickets);
        assert_eq!(
            beads.step,
            Some(BeadStep::WorkTickets),
            "the row still gets the step it renders"
        );
        assert!(
            beads.accepts_input(),
            "a step with no busy liveness behind it is not a run"
        );

        beads.set_status(SessionStatus::Idle, Instant::now());
        beads.set_step(BeadStep::AwaitInput);
        assert!(beads.accepts_input(), "and the human's turn still opens it");
    }

    /// A dead session must not leave the input box hidden behind it — the case the
    /// old `if !status.is_alive() { awaiting_user = true }` existed for. It needs
    /// no special case now: `Dead` is not `is_busy()`, so the derived rule opens
    /// the box on its own. What death *does* still have to do is stop the preview.
    #[test]
    fn a_dead_view_gives_the_input_box_back() {
        let mut v = view(TerminalType::Pi);
        v.set_status(SessionStatus::Running, Instant::now());
        v.chat = ChatState::Chat;
        assert!(!v.accepts_input(), "mid-run the box is closed");

        v.set_status(SessionStatus::Dead, Instant::now());
        assert!(
            v.accepts_input(),
            "a dead session cannot answer, so ask the human"
        );
        assert_eq!(
            v.chat,
            ChatState::Stopped,
            "and the live preview stops with the child, not after it"
        );
    }

    /// The run clock is *this run*. Entering busy starts it, leaving clears it, and
    /// a repeated edge changes neither — otherwise a duplicate `Running` publish
    /// would silently restart the age the status row is showing.
    #[test]
    fn the_run_clock_winds_on_the_edge_not_on_the_value() {
        let t0 = Instant::now();
        let later = t0 + Duration::from_secs(30);
        let mut v = view(TerminalType::Pi);

        v.set_status(SessionStatus::Running, t0);
        assert_eq!(v.run_elapsed(later), Some(Duration::from_secs(30)));

        // Same state again, later: the age does not restart.
        v.set_status(SessionStatus::Running, later);
        assert_eq!(v.run_elapsed(later), Some(Duration::from_secs(30)));

        // Idle is not a run at all, and leaves no clock behind for the next one.
        v.set_status(SessionStatus::Idle, later);
        assert_eq!(v.run_elapsed(later), None);

        // And the next run starts from its own instant, not from t0.
        let t1 = later + Duration::from_secs(60);
        v.set_status(SessionStatus::Running, t1);
        assert_eq!(v.run_elapsed(t1), Some(Duration::ZERO));

        // Through Aborting (busy both sides) the clock is held, not reset.
        let t2 = t1 + Duration::from_secs(5);
        v.set_status(SessionStatus::Aborting, t2);
        assert_eq!(v.run_elapsed(t2), Some(Duration::from_secs(5)));
    }

    /// ADR-0002 "Consequences": buffered output while hidden must be capped, and
    /// the loss must be visible rather than silent.
    #[test]
    fn an_inactive_view_caps_its_buffer_and_says_so() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        let mut emitted: Vec<String> = Vec::new();
        for i in 0..8 {
            v.push_note(MessageKind::System, format!("line {i} {}", "x".repeat(24)));
            // Flushed every round, i.e. these are lines the terminal already has.
            emitted.extend(flush_new(&mut v, 60));
        }
        assert!(
            v.transcript.byte_len() <= 128,
            "buffer must stay at or under the cap: {}",
            v.transcript.byte_len()
        );
        assert!(v.dropped_bytes() > 0, "the drop must be counted");
        // The notice the user sees is the store's marker row, not a line of
        // transcript: a notice in the transcript gets copied, journalled and
        // counted as content. So the assertion is on the marker, and the
        // marker is only in the emitted rows if it was rendered while this
        // view was on screen — which it is here, because we flushed every round.
        // The notice the user sees is the store's marker row at the head of the
        // window, not a line of transcript: a notice in the transcript would be
        // copied, journalled and counted as content. `flush` hands the frame new
        // rows and the frame paints the whole window, so what the marker has to
        // be true about is the store, not the emitted batch.
        let marker = &v.scrollback().rows()[0];
        assert!(
            marker.is_trim_marker(),
            "a trimmed store leads with its marker, not with a page cut mid-way"
        );
        let marker = marker.to_string();
        assert!(marker.contains("scrollback trimmed"), "{marker:?}");
        assert!(
            marker.contains(&v.scrollback().dropped_lines().to_string())
                || marker.contains(
                    &crate::services::clipboard::thousands(v.scrollback().dropped_lines())
                        .replace(',', "")
                ),
            "the marker states the loss: {marker} / dropped {}",
            v.scrollback().dropped_lines()
        );
    }

    /// The eviction moves the render cursor, and a cursor moved without being reset
    /// re-emits or garbles output — which in a real terminal is unrecoverable. So:
    /// after the cap kicks in, nothing already written comes back.
    #[test]
    fn eviction_never_re_emits_what_was_already_written() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        let mut seen: Vec<String> = Vec::new();
        for i in 0..24 {
            v.push_note(MessageKind::System, format!("unique line {i}"));
            seen.extend(flush_new(&mut v, 60));
        }
        assert!(
            v.dropped_bytes() > 0,
            "this test is about eviction happening"
        );

        let remaining: Vec<String> = v
            .transcript
            .entries
            .iter()
            .map(|e| e.text.clone())
            .collect();
        let after: Vec<String> = flush_new(&mut v, 60);
        for line in &after {
            let t = line.trim();
            if t.is_empty() || t.contains("bytes dropped") {
                continue;
            }
            assert!(
                !seen.iter().any(|s| s.contains(t)),
                "re-emitted a line the terminal already printed: {line:?}"
            );
            assert!(
                remaining.iter().any(|e| e.contains(t)),
                "emitted a line the view does not hold: {line:?}"
            );
        }
    }

    /// Sealing drops a half-parsed escape sequence along with the open entry. The
    /// bytes a dangling `\x1b[` is holding belong to a stream that will never
    /// finish, and if they stay they get charged to whatever streams next.
    #[test]
    fn sealing_drops_a_dangling_escape_sequence_too() {
        let mut v = view(TerminalType::Bash);
        v.push_bash("first-line\u{1b}[", 60); // ends mid-escape-sequence
        v.seal();
        let _ = v.flush(60);
        let first: Vec<String> = v
            .scrollback()
            .rows()
            .iter()
            .map(|r| r.to_string())
            .collect();

        v.push_bash("second-line\n", 60); // the next stream, which must be unaffected
        v.seal();
        let second: Vec<String> = flush_new(&mut v, 60);

        let joined: String = second.join("");
        assert!(
            joined.contains("second-line"),
            "the next stream arrived intact: {joined:?}"
        );
        for line in first.iter().chain(second.iter()) {
            assert!(
                !line.to_string().contains('\u{1b}'),
                "a raw escape byte reached the scrollback: {line:?}"
            );
        }
    }

    /// Losing bytes is fine; losing the ability to flush at all is not.
    #[test]
    fn the_view_keeps_flushing_after_eviction() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 128);
        for i in 0..12 {
            v.push_note(
                MessageKind::System,
                format!("filler {i} {}", "y".repeat(20)),
            );
            let _ = v.flush(60);
        }
        assert!(v.dropped_bytes() > 0);
        v.push_note(MessageKind::System, "after the eviction".into());
        let out = flush_new(&mut v, 60).join("\n");
        assert!(
            out.contains("after the eviction"),
            "post-eviction content still reaches the terminal: {out:?}"
        );
        assert_eq!(v.flush(60), 0, "and only once");
    }

    // ─────────── the store behind the scrollback (looprs-pdl.6) ───────────

    /// **Provenance stays truthful across a trim.** Every row on the screen has
    /// to be re-renderable from the entry it names. If the byte trim cut
    /// entries out from under the store, rows would address the wrong entry —
    /// the scrollback showing text no entry can produce, which is the sort of
    /// corruption that only appears after a long session.
    #[test]
    fn eviction_leaves_no_row_the_transcript_cannot_re_render() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 256);
        for i in 0..40 {
            v.push_note(
                MessageKind::System,
                format!("line {i} of a long transcript that will be trimmed away"),
            );
            v.flush(60);
        }
        assert!(v.dropped_bytes() > 0, "the trim ran");
        let entries = &v.transcript.entries;
        assert!(
            entries.len() < 40,
            "some entries were dropped: {}",
            entries.len()
        );
        for row in v.scrollback().rows() {
            if row.is_trim_marker() {
                continue; // chrome names no entry, by construction
            }
            assert!(
                row.entry < entries.len(),
                "a row outlives the entry it names: {:?}",
                row.line
            );
            let text = row.to_string();
            let body = text.trim_start_matches(['•', '●', '◌', ' ']).trim();
            if body.is_empty() {
                continue;
            }
            assert!(
                entries[row.entry].text.contains(body),
                "row {text:?} is not text the entry it names can re-render: {:?}",
                entries[row.entry].text
            );
        }
    }

    /// **A re-wrap makes the rows again.** The dangerous reading of "the store
    /// is keyed to a width" is that a resize appends the re-wrapped copy on top
    /// of the old one; the whole transcript would then be doubled.
    #[test]
    fn a_rewrap_makes_the_rows_again_rather_than_adding_to_them() {
        let mut v = view(TerminalType::Pi);
        v.push_note(
            MessageKind::Answer,
            "MARKER the answer is long enough to wrap in a narrow window and so takes several rows at forty columns but noticeably fewer at eighty columns".into(),
        );
        assert_ne!(v.flush(80), 0, "the answer rendered");
        let at_wide = v.scrollback().len();

        // A resize with nothing new to say: the store is made again at 40.
        assert_eq!(v.flush(40), 0, "nothing was finalised since the last flush");
        let at_narrow = v.scrollback().len();
        assert_eq!(v.scrollback().width(), 40, "the store is wrapped for 40");
        assert!(
            at_narrow > at_wide,
            "a narrower window takes more rows: {at_wide} -> {at_narrow}"
        );

        let all: String = v
            .scrollback()
            .rows()
            .iter()
            .map(|r| r.to_string())
            .collect();
        assert_eq!(
            all.matches("MARKER").count(),
            1,
            "the re-wrap left a second copy behind: {all:?}"
        );
    }

    /// The store lags the tail exactly as far as the user scrolled, and no
    /// further: `pending` is the count of rows that arrived since, not the
    /// distance to the bottom.
    #[test]
    fn rows_that_arrive_off_the_tail_count_themselves_and_do_not_move_the_view() {
        let mut v = view(TerminalType::Pi);
        for i in 0..30 {
            v.push_note(MessageKind::System, format!("settled {i}"));
            v.flush(60);
        }
        let band = 10usize;
        v.scrollback_mut().scroll_by(-(band as isize), band);
        let held = v
            .scrollback()
            .window(band)
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>();
        v.push_note(MessageKind::System, "late".into());
        v.flush(60);
        assert_eq!(
            v.scrollback()
                .window(band)
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>(),
            held,
            "the view held while the tail moved away"
        );
        assert_eq!(v.scrollback().pending(), 2, "the line and its separator");
        assert_eq!(
            v.scrollback().offset(),
            band + 2,
            "and the gap grew by that"
        );
    }

    // ──────── the one entry that never closes is the shape that overflows ────────
    //
    // Everything above caps a transcript of *closed* entries: whole entries come
    // off the front, the journal already had them, everybody goes home. The shape
    // that actually overflows a running app is a **single entry that never
    // closes** — `tail -f`, `watch`, `npm run dev`, a long build — because the
    // Bash command boundary is made at *submit*, not at completion, so one
    // running command *is* one open entry for its whole life. Measured before
    // `trim_open_entry` existed: a 4 KiB cap held 1.2 MB of transcript, and the
    // journal got **0 bytes** of it, because the prefix walk stops at the first
    // `!done` entry. Both halves of that matter — the OOM, and the escape hatch
    // missing exactly the case that needed it.

    /// **Memory is bounded while the stream never ends — and the file is still the
    /// whole document.**
    ///
    /// Deliberately no `flush` anywhere in this test: a view that is not on screen
    /// is never flushed, and "cap the buffered output while hidden" is the exact
    /// consequence ADR-0002 is asking for. The strongest available assertion is
    /// used for the file half — not "contains the lines" but *the exact bytes an
    /// uncapped run would have written*, cut boundaries and all.
    #[test]
    fn an_endless_open_entry_is_capped_and_the_journal_is_still_the_whole_document() {
        const CAP: usize = 4096;
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), CAP);
        let j = Arc::new(RecordingJournal::default());
        with_journal(&mut v, j.clone());

        // One command boundary, then output forever. Nothing closes it until the
        // very end, which is what a running command actually looks like.
        v.seal_shell_output();
        let mut expected = String::new();
        let mut peak = 0usize;
        for i in 0..600 {
            let line = format!("tail -f  line {i:04} of a stream that never ends");
            expected.push_str(&line);
            expected.push('\n');
            v.push_bash(&format!("{line}\n"), 80);
            peak = peak.max(v.transcript.byte_len());
        }

        // 600 lines x ~45 bytes is ~27 KB against a 4 KiB cap. Uncapped it rides
        // straight through, so the ceiling on the peak is the whole test.
        assert!(
            peak <= CAP + 512,
            "an open entry rode through the cap: peaked at {peak} against a {CAP} cap"
        );
        assert!(
            v.dropped_bytes() > 0,
            "and the trim counted what it took out of memory"
        );

        v.seal_shell_output();
        assert_eq!(
            j.text(),
            expected,
            "the journal must be the whole transcript across every cut boundary \
             (memory holds {} bytes, the file holds {})",
            v.transcript.plain_text().len(),
            j.text().len()
        );
    }

    /// **The cut moves the render cursor down instead of leaving it running past
    /// the end, and never re-emits what it removed.**
    ///
    /// Two failure shapes live here and this catches both: a *reseat* would rewind
    /// the entry to its head and duplicate every row the store already has, and a
    /// cursor left high against shortened text is an out-of-range slice in the
    /// middle of a frame. Flushing every round while the cap cuts underneath is
    /// how both get exercised rather than argued about.
    #[test]
    fn a_cut_open_entry_is_never_re_emitted_and_its_cursor_stays_in_range() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 512);
        v.seal_shell_output();

        let mut seen: Vec<String> = Vec::new();
        for i in 0..300 {
            v.push_bash(&format!("line {i:04} of a stream that never ends\n"), 80);
            seen.extend(flush_new(&mut v, 80));
            let text = v.transcript.entries.last().unwrap().text.len();
            assert!(
                v.flusher.emitted() <= text,
                "the flusher cursor ({}) is past the end of the text it reads ({text})",
                v.flusher.emitted()
            );
        }

        let joined = seen.join("\n");
        for i in (0..300).step_by(7) {
            let needle = format!("line {i:04}");
            assert_eq!(
                joined.matches(&needle).count(),
                1,
                "each line reaches the store exactly once: {needle}"
            );
        }
        assert!(
            v.dropped_bytes() > 0,
            "the cap was biting throughout, which is what this is testing"
        );
    }

    /// **Styles are byte ranges into their own entry, and they move with the cut.**
    ///
    /// Not a cosmetic concern: every colour downstream is read out of those
    /// ranges, so an un-rebased run paints the style of a line that has left the
    /// building onto whatever characters slid into its place.
    #[test]
    fn styles_are_rebased_onto_the_text_that_is_left() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 256);
        v.seal_shell_output();
        for i in 0..200 {
            v.push_bash(
                &format!("\u{1b}[31mred {i:04} padding padding padding\u{1b}[0m\n"),
                80,
            );
        }

        let e = v.transcript.entries.last().unwrap();
        assert!(
            !e.styles.is_empty(),
            "the stream carried styles at all, or this test proves nothing"
        );
        assert!(
            e.text.len() < 1200,
            "the entry was capped, not left to grow: {} bytes",
            e.text.len()
        );
        for s in &e.styles {
            assert!(
                s.end <= e.text.len(),
                "a style run points past the end of the text it belongs to: {:?} of {}",
                s,
                e.text.len()
            );
            assert!(s.start < s.end, "a style run collapsed to nothing: {s:?}");
            assert!(
                e.text[s.start..s.end].contains("red"),
                "the run now styles something that was never styled: {:?} -> {:?}",
                s,
                &e.text[s.start..s.end]
            );
        }
    }

    /// `preview` reads `text[scan..]` of the open entry on every frame the view is
    /// live. After the front of that entry is gone, a cursor that did not move
    /// with it is an out-of-range panic in the draw path — so this just asks for
    /// the preview a lot, with the cutting happening underneath.
    #[test]
    fn a_cut_open_entry_still_previews_without_slicing_past_its_end() {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), 256);
        v.seal_shell_output();
        for i in 0..150 {
            v.push_bash(&format!("tail {i:03} of a stream that never ends\n"), 60);
            let p = v.preview(60);
            assert!(
                p.len() <= 2,
                "the live tail stayed a live tail: {} rows",
                p.len()
            );
            let _ = v.flush(60);
        }
        assert!(
            v.transcript.byte_len() <= 512,
            "and it stayed capped while previewing: {}",
            v.transcript.byte_len()
        );
    }
    // ────────── the re-render source is bounded by the cap (looprs-zie) ──────────
    //
    // A resize used to re-render every entry the view holds and let the store
    // trim afterwards: 30,000 rows materialised to keep 5,787. The tests here
    // are the shape of that difference at a size a unit test can hold — a cap of
    // tens of rows, a transcript of hundreds — with the transcript buffer off
    // (`with_buffer(.., 0)`) so the store cap is the only cap in the room.

    /// A view of `entries` answers, each a few lines of prose, flushed at 80
    /// against a store capped at `cap_rows` rows' worth of charge.
    fn filled(cap_rows: usize, entries: usize) -> SessionView {
        let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Pi, 0), 0);
        v.set_store_cap(cap_rows * ROW_STRUCT_BYTES);
        for i in 0..entries {
            v.push_note(
                MessageKind::Answer,
                format!(
                    "answer {i:03}: the first line is long enough to be a whole row of prose at eighty columns.\n\
                     the second line keeps the paragraph going with ordinary words.\n\
                     and the third one closes it out."
                ),
            );
        }
        v
    }

    fn all_text(v: &SessionView) -> String {
        v.scrollback()
            .rows()
            .iter()
            .map(|r| r.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// **The source is cut.** A store that cannot hold the transcript must not
    /// pay to render the part of it it cannot hold.
    #[test]
    fn a_rewrap_cuts_the_source_at_the_newest_slice_that_fits() {
        let mut v = filled(40, 80);
        assert_ne!(v.flush(80), 0, "the transcript rendered once");
        assert!(
            v.scrollback().retained_bytes() <= 40 * ROW_STRUCT_BYTES,
            "and the store is at its cap: {}",
            v.scrollback().retained_bytes()
        );

        let start = v.rewrap_source_start(60);
        assert!(
            start > 0,
            "the rebuild starts partway into the transcript, not at entry 0: {start}"
        );
        assert!(
            start < v.transcript.entries.len(),
            "and it still covers the tail: {start} of {}",
            v.transcript.entries.len()
        );
    }

    /// **The tail is never what gets cut.** The cut is in the source and the
    /// render runs forward from it, so everything from the cut to the newest
    /// entry is rendered.
    #[test]
    fn the_source_cut_never_cuts_the_newest_content_off() {
        let mut v = filled(40, 80);
        let _ = v.flush(80);
        let newest = format!("answer {:03}", v.transcript.entries.len() - 1);
        let _ = v.flush(60);
        let all = all_text(&v);
        assert!(
            all.contains(newest.as_str()),
            "the newest answer ({newest}) is on the store after the rebuild: {all}"
        );
        assert!(
            !all.contains("answer 000:"),
            "and the oldest one is not: that is what a cut means"
        );
    }

    /// **A loss that happens by not being re-rendered is still a loss, and the
    /// marker says so.** Nothing in the store's own `trim` ever sees these rows.
    #[test]
    fn content_cut_out_of_the_source_is_counted_as_dropped() {
        let mut v = filled(40, 80);
        let _ = v.flush(80);
        let before = v.scrollback().dropped_lines();
        let _ = v.flush(60);
        let after = v.scrollback().dropped_lines();
        assert!(
            after > before,
            "the source cut reported its lines: {before} -> {after}"
        );
        let marker = v
            .scrollback()
            .rows()
            .first()
            .map(|r| r.to_string())
            .unwrap_or_default();
        assert!(
            marker.contains("scrollback trimmed"),
            "and the marker row is there to say it: {marker:?}"
        );
    }

    /// **Content that has not reached the store yet is inside the slice, always.**
    /// Dropping it would be dropping the first copy the user ever saw.
    #[test]
    fn content_the_flusher_has_not_emitted_yet_is_never_cut() {
        let mut v = filled(40, 80);
        let _ = v.flush(80);
        v.push_note(
            MessageKind::Answer,
            "ARRIVED AFTER THE LAST FLUSH, unseen.".into(),
        );
        let unseen = v.flusher.consumed();
        let start = v.rewrap_source_start(60);
        assert!(
            start <= unseen,
            "the unseen entry at {unseen} is inside the slice starting at {start}"
        );
        let _ = v.flush(60);
        assert!(
            all_text(&v).contains("ARRIVED AFTER THE LAST FLUSH"),
            "and it landed on the store"
        );
    }

    /// **A skipped entry is reported once.** The transcript keeps skipped entries
    /// until the buffer cap evicts them, and that path counts lines too.
    #[test]
    fn a_skipped_entry_is_not_reported_twice_when_the_buffer_evicts_it() {
        let mut v = filled(40, 80);
        let _ = v.flush(80);
        let _ = v.flush(60);
        let skipped = v.source_skipped;
        assert!(skipped > 1, "the rewrap skipped {skipped} entries");
        let reported = v.scrollback().dropped_lines();
        assert!(reported > 0);

        // Now let the buffer cap take entries that were *already* reported by
        // the source cut. The marker number must not move for them.
        v.set_buffer_limit(v.transcript.byte_len() / 2);
        v.push_note(
            MessageKind::Answer,
            "one more, which tips the buffer over".into(),
        );
        let evicted = 81 - v.transcript.entries.len() + 1;
        let after = v.scrollback().dropped_lines();
        assert!(
            evicted <= skipped,
            "the eviction ({evicted}) stayed inside the skipped prefix ({skipped})"
        );
        assert_eq!(
            after, reported,
            "and nothing was reported a second time for the same content"
        );
    }

    /// **Rule two of the cut: the newest entry is never the one that gets away.**
    /// The walk could not afford a 300-line answer against a 40-row cap, so it
    /// stopped before the newest entry — and if `start` had been allowed to land
    /// past it, the rebuild would have rendered *nothing* and emptied the store
    /// on a resize. The store's own "keep the newest row anyway" rule, applied
    /// where the source is cut.
    #[test]
    fn one_newest_entry_bigger_than_the_cap_is_still_put_on_the_store() {
        let mut v = filled(40, 20);
        let _ = v.flush(80);
        // One enormous answer, never flushed: nothing about it fits.
        let mut big = String::new();
        for i in 0..300 {
            big.push_str(&format!(
                "the one enormous answer line {i:03}, over the cap by itself\n"
            ));
        }
        v.push_note(MessageKind::Answer, big);
        let n = v.transcript.entries.len();
        assert!(
            v.rewrap_source_start(60) >= n - 1,
            "the walk cannot afford it and stops at the newest entry"
        );
        let _ = v.flush(60);
        let all = all_text(&v);
        assert!(
            all.contains("enormous answer line 299"),
            "the newest line of it is on the store"
        );
        assert!(
            v.scrollback().retained_bytes() <= 40 * ROW_STRUCT_BYTES,
            "and the store is back under its cap: {}",
            v.scrollback().retained_bytes()
        );
    }

    /// **A cut in the source is not a blank stare.** With the store cap well
    /// below the transcript, every rebuild drops older content that no `trim`
    /// ever sees; the marker has to say it from the first resize and keep saying
    /// the whole of it.
    #[test]
    fn the_marker_tells_the_whole_loss_across_repeated_resizes() {
        let mut v = filled(40, 120);
        let _ = v.flush(80);
        let mut last = v.scrollback().dropped_lines();
        assert!(last > 0, "the first fill already trimmed to the cap");
        for w in [70u16, 60, 50, 40, 35, 30] {
            let _ = v.flush(w);
            let now = v.scrollback().dropped_lines();
            assert!(
                now >= last,
                "the count never goes backwards: {last} -> {now}"
            );
            let marker = v
                .scrollback()
                .rows()
                .first()
                .map(|r| r.to_string())
                .unwrap_or_default();
            assert!(
                marker.contains("scrollback trimmed"),
                "and the row that says it is at the head at w={w}: {marker:?}"
            );
            last = now;
        }
    }
}
