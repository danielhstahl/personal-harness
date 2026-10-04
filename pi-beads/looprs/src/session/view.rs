//! The per-session scrollback: a [`Transcript`] welded to the [`Flusher`] that was
//! born with it (ADR-0002 Q5).
//!
//! The ADR rejects the single shared transcript. The reason is not aesthetics, it is
//! the `insert_before` invariant in `main.rs`: a `Flusher` is a *cursor into one
//! specific transcript*, and the moment two transcripts share one cursor (or one
//! transcript is rendered through two cursors) lines get dropped or duplicated in
//! the real terminal scrollback, which is the one place a bug is unrecoverable for
//! the user.
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

use super::SessionId;
use crate::components::scrollback::Flusher;
use crate::session::ActiveBead;
use crate::session::{BeadStep, SessionStatus};
use crate::state::transcript::{Entry, MessageKind, Transcript};
use crate::utils::render::ControlStripper;

/// Default cap on how much text an *inactive* view will hold.
///
/// A view that is on screen drains every frame, so this only binds for a session
/// nobody is looking at — which is exactly the unbounded case ADR-0002 names
/// ("a `yes | sleep 1000000`-style shell … grows its transcript forever"). When a
/// hidden session overruns the cap, old lines are dropped and one visible notice is
/// inserted, so the loss is honest rather than silent.
pub const DEFAULT_VIEW_BUFFER: usize = 256 * 1024;

/// Room held back out of the cap for the eviction notice itself, so the view ends
/// *at or under* the limit rather than limit-plus-a-line. Generous for any wording
/// under 64 bytes ("… 4,294,967,295 bytes dropped (buffer cap) …" fits).
const NOTICE_BUDGET: usize = 64;

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
}

impl ChatState {
    pub fn is_streaming(self) -> bool {
        !matches!(self, Self::Stopped)
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
    /// This session wants typed input. Drives the input box (was: `App::need_input`).
    ///
    /// `true` by default: a mode nobody has used yet should accept input, and the
    /// session will say otherwise the moment it starts work. Note the asymmetry that
    /// matters — `false` here only ever means "this session is busy", so it must
    /// never be set on the basis of the input mode.
    pub awaiting_user: bool,
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
    /// Dropped-line bookkeeping for the buffer cap (see [`DEFAULT_VIEW_BUFFER`]).
    dropped: usize,
    limit: usize,
    /// Escape-sequence stripper for this view's Bash output (see [`Self::push_bash`]).
    /// Per-view because a sequence can straddle two reads.
    bash_strip: ControlStripper,
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
            status: SessionStatus::NotStarted,
            run_started: None,
            last_error: None,
            chat: ChatState::Stopped,
            awaiting_user: true,
            step: None,
            active_bead: None,
            dropped: 0,
            limit,
            bash_strip: ControlStripper::default(),
        }
    }

    /// Shell output into the transcript: verbatim content, presentation stripped,
    /// **never** markdown and **never** re-wrapped (ADR-0001 rules 1 and 5).
    ///
    /// The strip is per-view because a colour sequence split across two reads must
    /// not come out as half-dropped, half-printed-garbage.
    pub fn push_bash(&mut self, chunk: &str) {
        let text = self.bash_strip.strip(chunk);
        if text.is_empty() {
            return;
        }
        self.transcript.push_delta(MessageKind::Bash, &text);
        self.enforce_buffer();
    }

    /// Lines that became final since the last call. Call once per frame, for the
    /// active view only, immediately before `insert_before` + `draw`.
    ///
    /// Invariant preserved here: monotonic. Every finalized line of this transcript
    /// is returned exactly once, ever.
    pub fn flush(&mut self, width: u16) -> Vec<Line<'static>> {
        self.flusher.drain(&self.transcript, width)
    }

    /// The not-yet-final tail, for the live preview region.
    pub fn preview(&self, width: u16) -> Vec<Line<'static>> {
        self.flusher.preview(&self.transcript, width)
    }

    /// Close whatever streamed entry is still open.
    ///
    /// MUST be called when the owning session dies ([`super::SessionEvent::Exited`])
    /// or is torn down. Without it the transcript keeps an entry that is `!done`
    /// forever: the flusher stalls on it, nothing that session already produced
    /// ever reaches the scrollback again, and the live preview shows a spinner on
    /// dead text forever. This is the seam that breaks the `insert_before`
    /// invariant, so it is part of the contract rather than a detail.
    pub fn seal(&mut self) {
        self.transcript.finish_last();
        // A half-parsed escape sequence belongs to a stream that is never going to
        // send the rest of it. Left as it is, the next Bash generation's first
        // bytes get eaten by the previous one's dangling `\x1b[`, which shows up
        // as the first line of a brand new shell silently missing its head.
        self.bash_strip.reset();
    }

    /// A finished, one-shot line (status notices, the mode-switch separator).
    pub fn push_note(&mut self, kind: MessageKind, text: String) {
        self.transcript.push_done(kind, text);
        self.enforce_buffer();
    }

    /// A streaming delta from this session's backend.
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        self.transcript.push_delta(kind, delta);
        self.enforce_buffer();
    }

    /// A session-level error: recorded for the status row and shown.
    pub fn push_error(&mut self, text: String) {
        self.last_error = Some(text.clone());
        self.transcript.push_done(MessageKind::Error, text);
        self.enforce_buffer();
    }

    /// Mirror of the owning session's liveness, plus the input gating that follows
    /// from it: a session with no child must not leave the input box hidden.
    ///
    /// Also the one place the run clock is wound: entering a busy state starts it if
    /// it is not already running, leaving one stops it. Idempotent in both
    /// directions, so a session that publishes `Running` twice (a mirror refresh,
    /// a duplicate edge) does not restart the age the row is showing.
    pub fn set_status(&mut self, status: SessionStatus, now: Instant) {
        let was_busy = self.status.is_busy();
        self.status = status;
        if status.is_busy() {
            if !was_busy {
                self.run_started = Some(now);
            }
        } else {
            self.run_started = None;
        }
        if !status.is_alive() {
            self.awaiting_user = true;
            self.chat = ChatState::Stopped;
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

    /// The beads machine moved: record the step (status row) and gate input on it.
    ///
    /// The gate is [`BeadStep::awaits_user`], the same predicate the beads session
    /// uses, so "the loop is waiting" and "the box may open" cannot drift apart
    /// even though two different types are asking.
    pub fn set_step(&mut self, step: BeadStep) {
        self.awaiting_user = step.awaits_user();
        self.step = Some(step);
    }

    /// How many bytes of this view's output were dropped by the cap (status row).
    ///
    /// The counter is what makes an eviction honest rather than invisible; the row
    /// shows it as `~N dropped` (looprs-guh) and the tests here assert both the
    /// count and the message that quotes it.
    pub fn dropped_bytes(&self) -> usize {
        self.dropped
    }

    /// Cap the buffered transcript while this view is *not* on screen.
    ///
    /// Lossy on purpose — that is what a cap is — so the two things it must get
    /// right are: say what was lost, and do not corrupt the render cursor. The
    /// notice goes in *at* the cursor rather than at the head of the transcript, so
    /// it is the next thing the terminal sees and the still-pending entries behind
    /// it keep their order. The cursor is then reseat-ed, because its per-entry
    /// state (scan/block/fence) belongs to whatever entry it was last reading.
    ///
    /// The notice's own size is reserved out of the cap: without that the view ends
    /// at `limit + notice` and the cap is a rounding error with an apology note.
    fn enforce_buffer(&mut self) {
        if self.limit == 0 || self.transcript.byte_len() <= self.limit {
            return;
        }
        let target = self.limit.saturating_sub(NOTICE_BUDGET);
        let first = self.flusher.consumed();
        let mut dropped = 0usize;
        let mut removed = 0usize;
        // Always keep one entry: an empty transcript with the cursor past the end is
        // a state nothing downstream is written to expect.
        while self.transcript.byte_len() > target && self.transcript.entries.len() > 1 {
            dropped += self.transcript.entries.remove(0).text.len();
            removed += 1;
        }
        if dropped == 0 {
            return;
        }
        self.dropped += dropped;
        // The eviction shifted the transcript; the cursor follows it, and the notice
        // takes the cursor's slot so it is what gets written out next.
        let at = first.saturating_sub(removed);
        self.transcript.entries.insert(
            at,
            Entry {
                kind: MessageKind::System,
                text: format!("… {} bytes dropped (buffer cap) …", self.dropped),
                done: true,
            },
        );
        self.flusher.reseat(at);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::TerminalType;

    fn view(mode: TerminalType) -> SessionView {
        SessionView::new(SessionId::new(mode, 0))
    }

    /// The flush invariant, stated as a test: monotonic. Note what the flusher's
    /// own contract is — a prose block is rendered when it *closes* (blank line or
    /// `done`), not on every newline — so these assertions are about the cursor,
    /// not about line counts.
    #[test]
    fn flush_is_monotonic_per_view() {
        let mut v = view(TerminalType::Pi);
        v.transcript.push_delta(MessageKind::Answer, "line one\n\n");
        let first = v.flush(60);
        assert!(!first.is_empty(), "a closed block must flush");

        assert!(
            v.flush(60).is_empty(),
            "a second drain with no new content must emit nothing"
        );

        v.transcript
            .push_delta(MessageKind::Answer, "second para\n\n");
        let second = v.flush(60);
        assert!(!second.is_empty(), "the new block must flush");

        // And nothing is re-emitted: two closed blocks in, then silence.
        assert!(v.flush(60).is_empty());
    }

    /// The stall this ticket is about: an open entry that is never sealed blocks the
    /// cursor forever — its text can never reach the scrollback, and the live
    /// preview spins over dead text. `seal()` is what unblocks it.
    #[test]
    fn an_unsealed_entry_stalls_the_flusher_and_seal_clears_it() {
        let mut v = view(TerminalType::Beeds);
        v.transcript
            .push_delta(MessageKind::Answer, "partial answer, no newline");
        assert!(
            v.flush(60).is_empty(),
            "an unterminated, undone entry is not flushed"
        );

        v.seal();
        let after = v.flush(60);
        assert!(
            after.iter().any(|l| !l.spans.is_empty()),
            "sealing must release the tail: {after:?}"
        );
        assert!(v.flush(60).is_empty(), "and only once");
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
        assert!(!pi_lines.is_empty(), "pi's closed block must appear");
        assert!(!beads_lines.is_empty(), "beads' system line must appear");
        assert!(pi.flush(60).is_empty());
        assert!(beads.flush(60).is_empty());

        // Pi keeps streaming; the beads view must not budge, and must not gain pi's text.
        pi.transcript.push_delta(MessageKind::Answer, "more pi\n\n");
        assert!(!pi.flush(60).is_empty());
        assert!(
            beads.flush(60).is_empty(),
            "the beads view consumed nothing it was not given"
        );
    }

    #[test]
    fn errors_are_recorded_for_the_status_row() {
        let mut v = view(TerminalType::Bash);
        v.push_error("shell exited (code 1)".into());
        assert_eq!(v.last_error.as_deref(), Some("shell exited (code 1)"));
    }

    /// Where `App::need_input` used to come from, now per session: the beads step
    /// gates the input box, and only for the view that owns the beads machine.
    #[test]
    fn a_busy_step_hides_input_only_for_its_own_view() {
        let mut beads = view(TerminalType::Beeds);
        let pi = view(TerminalType::Pi);
        assert!(beads.awaiting_user, "an untouched view accepts input");

        beads.set_step(BeadStep::WorkTickets);
        assert!(
            !beads.awaiting_user,
            "a working beads view must not take input"
        );
        assert!(
            pi.awaiting_user,
            "and that must not leak into the Pi view (that was the global-flag bug)"
        );

        beads.set_step(BeadStep::AwaitInput);
        assert!(beads.awaiting_user);
        assert_eq!(beads.step, Some(BeadStep::AwaitInput));
    }

    /// A dead session must not leave the input box hidden behind it.
    #[test]
    fn a_dead_view_gives_the_input_box_back() {
        let mut v = view(TerminalType::Pi);
        v.awaiting_user = false;
        v.chat = ChatState::Chat;

        v.set_status(SessionStatus::Dead, Instant::now());
        assert!(
            v.awaiting_user,
            "a dead session cannot answer, so ask the human"
        );
        assert_eq!(v.chat, ChatState::Stopped);

        // ...while a live-but-idle session keeps whatever gating it had.
        let mut w = view(TerminalType::Bash);
        w.awaiting_user = false;
        w.set_status(SessionStatus::Idle, Instant::now());
        assert!(!w.awaiting_user);
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
            emitted.extend(v.flush(60).iter().map(|l| l.to_string()));
        }
        assert!(
            v.transcript.byte_len() <= 128,
            "buffer must stay at or under the cap: {}",
            v.transcript.byte_len()
        );
        assert!(v.dropped_bytes() > 0, "the drop must be counted");
        assert!(
            emitted
                .iter()
                .any(|l| l.contains("bytes dropped") && l.contains(&v.dropped_bytes().to_string())),
            "and the notice must reach the scrollback, with the amount: {emitted:?}"
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
            seen.extend(v.flush(60).iter().map(|l| l.to_string()));
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
        let after: Vec<String> = v.flush(60).iter().map(|l| l.to_string()).collect();
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
        v.push_bash("first-line\u{1b}["); // ends mid-escape-sequence
        v.seal();
        let first = v.flush(60);

        v.push_bash("second-line\n"); // the next stream, which must be unaffected
        v.seal();
        let second = v.flush(60);

        let joined: String = second.iter().map(|l| l.to_string()).collect();
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
        let out = v.flush(60);
        assert!(
            out.iter()
                .any(|l| l.to_string().contains("after the eviction")),
            "post-eviction content still reaches the terminal: {out:?}"
        );
        assert!(v.flush(60).is_empty(), "and only once");
    }
}
