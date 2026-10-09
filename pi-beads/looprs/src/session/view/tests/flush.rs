//! //! The flusher's contract: one cursor, each finalized line exactly once.
//! //!
//! //! These are the tests for the single-door invariant ADR-0002 Q5 is built on —
//! //! monotonicity across views, the seal that clears a stalled open entry and
//! //! closes cards left running, and the status row's error mirror — i.e. what
//! //! [`flush`](super::super::flush) promises the frame every time it is called.

use crate::session::TerminalType;
use crate::state::transcript::MessageKind;

use super::*;

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
