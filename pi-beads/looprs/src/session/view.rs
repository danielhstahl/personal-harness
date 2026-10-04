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

use ratatui::text::Line;

use super::SessionId;
use crate::components::scrollback::Flusher;
use crate::session::SessionStatus;
use crate::state::state::{MessageKind, Transcript};

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
    /// Most recent error, kept so the status row can show it without digging
    /// through scrollback (looprs-guh).
    pub last_error: Option<String>,
}

impl SessionView {
    /// Transcript and Flusher are created together and only ever used together.
    pub fn new(session: SessionId) -> Self {
        Self {
            session,
            transcript: Transcript::new(),
            flusher: Flusher::new(),
            status: SessionStatus::NotStarted,
            last_error: None,
        }
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
    }

    /// A finished, one-shot line (status notices, the mode-switch separator).
    pub fn push_note(&mut self, kind: MessageKind, text: String) {
        self.transcript.push_done(kind, text);
    }

    /// A session-level error: recorded for the status row and shown.
    pub fn push_error(&mut self, text: String) {
        self.last_error = Some(text.clone());
        self.transcript.push_done(MessageKind::Error, text);
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
}
