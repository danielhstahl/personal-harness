//! //! Resize: rebuilding the store's rows, and counting what the rebuild cannot
//! //! reach.
//! //!
//! //! A rewrap makes the rows again rather than adding to them, and the tests here
//! //! are the ways that can go wrong at the seam: content cut out of the source
//! //! that was never counted as dropped, the newest slice cut off instead of the
//! //! oldest, content the flusher has not emitted yet being taken by the cut, a
//! //! skipped entry reported twice (once by the source cut, once by the buffer),
//! //! and a single entry bigger than the cap that still has to land on the store.

use crate::session::TerminalType;
use crate::state::scrollback::ROW_STRUCT_BYTES;
use crate::state::transcript::MessageKind;

use super::*;

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
