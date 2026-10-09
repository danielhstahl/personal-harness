//! //! Over budget: what a view drops, and what it must never do while dropping it.
//! //!
//! //! The cap is the thing a hidden `yes | sleep 1000000` exists to be caught by,
//! //! so every test here is a way the drop could be a lie: re-emitting a line the
//! //! flusher already sent, leaving a row the store cannot re-render, losing the
//! //! journal's whole-document property on an endless open entry, cutting a line's
//! //! styles out from under the text that survived, or stopping the flush after
//! //! eviction so the seen half never arrives.

use crate::session::view::SessionView;
use crate::session::{SessionId, TerminalType};
use crate::state::transcript::MessageKind;

use super::*;

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
        let text = v.transcript.entries.back().unwrap().text.len();
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

    let e = v.transcript.entries.back().unwrap();
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

/// **The cap's decision and the counter's number are one fact, all the way
/// down the eviction loop.**
///
/// `enforce_buffer` decides with a running total, not with a fresh sum, so
/// the two directions a wrong counter can fail are both worth naming: an
/// eviction that **under**-debit leaves the loop cutting entries the cap had
/// already covered (content lost that nobody had to lose), and one that
/// **over**-debit stops the loop with the transcript still over its limit — a
/// leak that reports itself as a cap that works. Both are checked every round
/// of a stream that keeps the loop biting, against `recount_bytes`, which is
/// the sum the counter stands for rather than a second opinion of the same
/// arithmetic.
#[test]
fn the_cap_stops_exactly_where_the_counter_says_it_should() {
    const CAP: usize = 512;
    let mut v = SessionView::with_buffer(SessionId::new(TerminalType::Bash, 0), CAP);

    for i in 0..200 {
        // One-shot entries, so this is the whole-entry eviction path and not
        // the open-entry cut next door: entries come off the front whole.
        v.push_note(
            MessageKind::Answer,
            format!("answer {i:03}, long enough that the cap is always biting"),
        );
        let bytes = v.transcript.byte_len();
        assert_eq!(
            bytes,
            v.transcript.recount_bytes(),
            "the counter and the entries disagreed mid-stream at round {i}"
        );
        assert!(
            bytes <= CAP,
            "the cap stopped at {bytes} against a {CAP} cap (round {i})"
        );
    }

    assert!(
        v.dropped_bytes() >= 200 * 40 - CAP,
        "and it had plenty to drop, or the loop above proved nothing: {}",
        v.dropped_bytes()
    );
}
