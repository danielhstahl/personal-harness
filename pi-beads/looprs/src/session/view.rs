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
//!
//! ## What is where (looprs-00u.18)
//!
//! This was one 3,670-line file with seven responsibilities in it. What is left
//! here is the view itself — the type, its construction, and the answers read
//! off it — with the six responsibilities it carried next door:
//!
//! * [`flush`](flush) — the one door every finalized line goes through, plus the
//!   record-push verbs and the journal write-side;
//! * [`rewrap`](rewrap) — rebuilding the store's rows at a new width out of the
//!   source slice the budget still covers;
//! * [`buffer`](buffer) — the retained-history ceiling and the two ways a view
//!   lets go when it runs over;
//! * [`shell_tail`](shell_tail) — the Bash tail: chunks in, sealed entries out;
//! * [`chord`](chord) — the keyboard contract's vocabulary: key, effect, owner;
//! * [`chord_table`](chord_table) — `CHORD_TABLE` itself, the keymap as rows;
//! * [`tests`](tests) — this suite, split along the banners already in it.
//!
//! Behaviour-preserving only: whole items moved, and every path that named
//! something here (`view::SessionView`, `view::Tokens`,
//! `view::RETAINED_BYTES_WORST_CASE`, `view::copy_chord_hint`,
//! `view::CHORD_TABLE`) still resolves out of this module.
use std::time::{Duration, Instant};

use ratatui::text::Line;

use super::{SessionId, TerminalType};
use crate::components::line_render::Flusher;
use crate::session::ActiveBead;
use crate::session::{BeadStep, SessionStatus};
use crate::state::scrollback::{DEFAULT_RETAINED_BYTES, Scrollback};
use crate::state::transcript::{MessageKind, Transcript};
use crate::theme::styles::{restyle, style_for};
use crate::utils::shelltext::LineResolver;

mod buffer;
mod chord;
mod chord_table;
mod flush;
mod rewrap;
mod shell_tail;

#[cfg(test)]
mod tests;

pub use buffer::{DEFAULT_VIEW_BUFFER, MAX_VIEWS, RETAINED_BYTES_WORST_CASE};
pub use chord::copy_chord_hint;
// The rest of the chord vocabulary is reached where it lives
// (`view::chord::{ChordState, ChordRow, Effect, KeySym, Owner}` and
// `view::chord_table::CHORD_TABLE`); a `pub use` here for a binary crate's own
// types would be a re-export with no reader, which is the thing this repo's
// dead-code rule refuses to write down.

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
    pub fn add(&mut self, u: &crate::wire::Usage) {
        self.input += u.input;
        self.output += u.output;
        self.cache += u.cache_read + u.cache_write;
    }

    /// Nothing reported yet — which is not the same fact as "zero spent", per
    /// [`crate::wire::Usage`]'s optionality. The row shows no segment for it.
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
    /// Bounded by [`DEFAULT_RETAINED_BYTES`] rendered bytes; the text it was
    /// rendered from is separately bounded by [`DEFAULT_VIEW_BUFFER`]. Both
    /// per-view halves, and the ceiling the three of them add up to, are on
    /// [`RETAINED_BYTES_WORST_CASE`].
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
            // The store's cap is passed in here rather than left to
            // `Scrollback::new`'s own default, so that both of this view's
            // caps — rendered rows and transcript text — are set on adjacent
            // lines and the arithmetic on `RETAINED_BYTES_WORST_CASE` can be
            // read off the code it describes.
            scrollback: Scrollback::with_cap(0, DEFAULT_RETAINED_BYTES),
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

    /// This view's scrollable store.
    pub fn scrollback(&self) -> &Scrollback {
        &self.scrollback
    }

    /// Mutable access to the scroll state (offset, pin) — the scroll keys drive
    /// the store through here so nothing else can move the view.
    pub fn scrollback_mut(&mut self) -> &mut Scrollback {
        &mut self.scrollback
    }

    /// How many bytes the live tail currently holds — the length of what a
    /// [`Self::preview`] renders, in bytes.
    ///
    /// Measurement seam (looprs-00u.14): `crate::measure` buckets the per-frame
    /// preview cost by exactly this number, because "how long is the live block"
    /// is the question the ticket's whole argument turns on and the answer is a
    /// cursor fact (`text[block..]`), not something a caller can re-derive
    /// without duplicating the flusher's rules.
    #[allow(dead_code)] // measurement seam: `measure.rs` buckets live-preview cost by this number
    pub fn preview_len(&self) -> usize {
        self.flusher
            .live_tail(&self.transcript)
            .map(|t| t.text.len())
            .unwrap_or(0)
    }

    /// `(hits, misses)` of this view's live-tail preview cache.
    ///
    /// The flusher's counters, asked through the door the frame uses, so a test
    /// of `SessionView::preview` can say "that second call was served from the
    /// cache" rather than inferring it from a stopwatch. See
    /// [`crate::components::line_render::Flusher::tail_cache_stats`].
    #[allow(dead_code)] // measurement + test seam: `measure.rs` reports the live-preview cache's hit rate
    pub fn preview_cache_stats(&self) -> (u64, u64) {
        self.flusher.tail_cache_stats()
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
            TerminalType::Pi | TerminalType::Beads => !self.status.is_busy(),
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
}
