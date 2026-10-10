//! //! What the view keeps, and what the journal keeps, tested against each other.
//! //!
//! //! The store is a window; the journal is the document. Every test here asserts
//! //! one of the pairs the design is sold on: the retained ceiling is the sum of
//! //! its stated halves, a capped view costs RAM but not the file, an open entry
//! //! holds the line behind it instead of being jumped, and what was never
//! //! finalised never reaches the file at all.

use crate::session::view::SessionView;
use crate::session::view::buffer::{DEFAULT_VIEW_BUFFER, MAX_VIEWS, RETAINED_BYTES_WORST_CASE};
use crate::session::{SessionId, TerminalType};
use crate::state::scrollback::DEFAULT_RETAINED_BYTES;
use crate::state::transcript::MessageKind;

use super::*;

// ───────────── the retained ceiling is stated where it is set (looprs-di9) ─────────────

/// The "≈ 97 MiB retained" in [`RETAINED_BYTES_WORST_CASE`]'s doc is a
/// claim about three numbers that live in three places: the mode count, the
/// rendered-store cap and the transcript-buffer cap. Prose about numbers in
/// other modules goes stale silently, so this is the tripwire: change any
/// one of them and this fails, and the failure message is the sentence that
/// has to be re-written on purpose.
#[test]
fn the_retained_ceiling_is_the_sum_it_says_it_is() {
    const KIB: usize = 1024;
    const MIB: usize = 1024 * KIB;

    // One view per mode, and the mode table is the only thing that decides
    // it. A fourth `TerminalType` is a fourth store, and this is where that
    // gets noticed — not in a comment that still says "three".
    assert_eq!(
        MAX_VIEWS,
        3,
        "one view per mode: {:?}",
        TerminalType::ALL.map(|m| m.label())
    );
    // The two per-view halves, as the doc describes them.
    assert_eq!(DEFAULT_RETAINED_BYTES, 32 * MIB, "the store cap moved");
    assert_eq!(DEFAULT_VIEW_BUFFER, 256 * KIB, "the buffer cap moved");
    // And the total is what the sentence claims it is.
    assert_eq!(
        RETAINED_BYTES_WORST_CASE,
        MAX_VIEWS * DEFAULT_RETAINED_BYTES + MAX_VIEWS * DEFAULT_VIEW_BUFFER,
        "the ceiling is not the sum of its halves"
    );
    assert!(
        (96 * MIB..98 * MIB).contains(&RETAINED_BYTES_WORST_CASE),
        "the doc quotes ~97 MiB; the numbers now add up to {} MiB — re-write \
             the sentence (and re-check that number is still acceptable)",
        RETAINED_BYTES_WORST_CASE as f64 / MIB as f64,
    );
}

/// The ceiling only describes the app if the views the app *builds* are
/// built to it. A `SessionView::new` that quietly picked a different store
/// cap would leave [`RETAINED_BYTES_WORST_CASE`] documenting the defaults
/// instead of the running thing.
#[test]
fn a_fresh_view_is_built_to_both_halves_of_the_ceiling() {
    for mode in TerminalType::ALL {
        let v = view(mode);
        assert_eq!(
            v.scrollback().cap_bytes(),
            DEFAULT_RETAINED_BYTES,
            "{}'s store is not capped at the stated figure",
            mode.label()
        );
        assert_eq!(
            v.limit,
            DEFAULT_VIEW_BUFFER,
            "{}'s transcript buffer is not capped at the stated figure",
            mode.label()
        );
    }
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
