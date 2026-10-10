//! //! The read itself: the shape of what the poller asks `bd` for, and what the
//! //! snapshot owes for each answer.
//! //!
//! //! Two fixed reads and never a write; the binary injected so no test in this
//! //! suite can reach a real board. The load-bearing ones are the three ways an
//! //! answer can be misread as another: a board never read reported as *empty*, an
//! //! empty board reported as a *failure*, and a failed read replacing the last
//! //! good board — which is `bd is broken` rebuilt one layer up as "the board is
//! //! empty" (looprs-037).

use super::*;

// ───────────────────────── the read it runs ─────────────────────────

#[tokio::test]
async fn the_poller_runs_two_fixed_reads_and_never_a_write() {
    let r = running(
        "poll-once",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(60),
    );
    wait_reads(&r.fakes, 4).await;

    let lines = r.fakes.bd_log();
    assert!(lines.len() >= 4, "expected several reads: {lines:?}");
    for line in &lines {
        let is_board = line == BOARD_READ;
        let is_probe = line.starts_with("--readonly events tail --since ")
            && line.contains(" --limit ")
            && line.ends_with("--json");
        assert!(
            is_board || is_probe,
            "the poller ran {line:?}, which is neither the board read nor the journal probe: \
                 {lines:?}"
        );
    }
    // Both halves actually ran, or this passed by running only one of them.
    assert!(
        lines.iter().any(|l| l == BOARD_READ),
        "no board read at all: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("--readonly events tail")),
        "no journal probe at all: {lines:?}"
    );
    // The band cannot change the board — as a property of the command line.
    let joined = lines.join(" ");
    for forbidden in [
        "update", "claim", "create", "close", "ready", "delete", "label", "prune",
    ] {
        assert!(
            !joined.contains(forbidden),
            "the poller ran {forbidden:?}, which either writes or re-derives: {lines:?}"
        );
    }
}

/// Zero network, zero real board: the poller only ever runs the binary it
/// was handed. That the *fake* is the only thing that could have logged is
/// the assertion — a poller wired to a real `bd` would leave this log empty
/// while doing something much worse.
#[tokio::test]
async fn the_suite_never_touches_a_real_board_because_the_binary_is_injected() {
    let r = running(
        "poll-injected",
        BdFake::Ok,
        EMPTY_BOARD,
        Duration::from_millis(60),
    );
    wait_reads(&r.fakes, 2).await;
    assert!(
        r.fakes.bd_calls() >= 2,
        "the injected fake never ran, which would mean the poller found a real `bd` \
             somewhere instead: {:?}",
        r.fakes.bd_log()
    );
}

// ─────────────────── never_loaded / empty / broken ───────────────────
//
// looprs-037's rule one layer up, as three separate tests rather than one
// test of three branches: the entire point is that these are three
// different facts, so they get three independent ways to fail.

/// Before the first read lands the board is `never_loaded` — **not** empty,
/// and not an error. A slow fake is what makes this observable at all: with
/// a fast `bd` the window closes before the test can look.
#[tokio::test]
async fn a_board_that_has_not_been_read_yet_is_never_loaded_not_empty() {
    let fakes = Fakes::new(
        "poll-never-loaded",
        PiFake::Started,
        BdFake::Slow,
        EMPTY_BOARD,
    );
    let (poller, handle) = BoardPoller::spawn(BoardConfig {
        bin: fakes.bd_bin().to_string(),
        interval: Duration::from_millis(60),
        enabled: true,
        journal: sweep_every(Duration::from_millis(60)),
    });

    let snap = handle.borrow();
    assert_eq!(
        snap.read,
        BoardRead::Never,
        "before the first read the band says `reading the board…`"
    );
    assert_eq!(snap.total(), 0);
    assert_eq!(snap.age, None, "nothing read, so nothing has an age");
    assert_eq!(snap.fetched_at, None);
    // …and explicitly not the empty-board rendering.
    let empty = BoardSnapshot::from_beads(&[], Some(Duration::ZERO));
    assert_ne!(
        snap.read, empty.read,
        "not-loaded and empty are not one state"
    );
    assert!(
        !snap.read.is_error(),
        "waiting is not failing: {:?}",
        snap.read
    );
    for col in Column::ALL {
        assert_eq!(
            snap.header_count(col),
            "—",
            "{} is not-yet-counted, not zero-counted",
            col.name()
        );
    }
    drop(snap);
    drop(handle);
    drop(poller);
}

/// `Ok(vec![])` renders as three empty columns, because the board answered
/// and its answer was nothing.
#[tokio::test]
async fn an_empty_board_is_an_answer_and_not_a_failure() {
    let r = running(
        "poll-empty",
        BdFake::Ok,
        EMPTY_BOARD,
        Duration::from_millis(60),
    );
    wait_snapshot(&r.handle, "an Ok snapshot", |s| s.read.is_ok()).await;

    let snap = r.handle.borrow();
    assert_eq!(snap.read, BoardRead::Ok, "the board answered");
    assert!(!snap.read.is_error());
    assert_eq!(snap.total(), 0);
    assert_eq!(snap.deferred, 0);
    for col in Column::ALL {
        assert!(snap.beads_in(col).is_empty(), "{}", col.name());
        // A counted zero, not the `—` of "cannot say".
        assert_eq!(snap.header_count(col), "0", "{}", col.name());
    }
    assert!(
        snap.fetched_at.is_some(),
        "a board that answered has a timestamp"
    );
}

/// The third of the three: `Err` is neither of the above, and arrives
/// carrying which kind of broken it is.
#[tokio::test]
async fn a_board_that_cannot_be_read_is_not_a_board_with_nothing_on_it() {
    let r = running(
        "poll-broken-from-the-start",
        BdFake::Fails,
        EMPTY_BOARD,
        Duration::from_millis(60),
    );
    wait_snapshot(&r.handle, "a failed snapshot", |s| s.read.is_error()).await;

    let snap = r.handle.borrow();
    assert!(snap.read.is_error(), "{:?}", snap.read);
    // `BdFake::Fails` exits 3 without writing a single byte to stderr, which
    // makes this the sharpest form the looprs-037 rule can take: an error with
    // nothing to say is still an error, and never a board with nothing on it.
    assert_eq!(
        snap.read,
        BoardRead::Failed {
            code: Some(3),
            message: None
        },
        "the error arrives as its own variant, with its exit code intact"
    );
    // Never loaded, so nothing to be stale about — and neither of the other
    // two states this may be mistaken for.
    assert_eq!(snap.age, None);
    assert_ne!(snap.read, BoardRead::Never);
    assert_ne!(snap.read, BoardRead::Ok);
}

// ─────────────────────── the mapping it ships ───────────────────────

/// The snapshot on the wire carries the ADR's mapping and the **true
/// totals**, un-truncated: counts are the poller's job, truncation is the
/// frame's.
#[tokio::test]
async fn the_snapshot_carries_the_whole_read_and_its_true_totals() {
    let r = running(
        "poll-mapping",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(60),
    );
    wait_snapshot(&r.handle, "a fully counted board", |s| {
        s.read.is_ok() && s.total() == 6
    })
    .await;

    let snap = r.handle.borrow();
    assert_eq!(snap.read, BoardRead::Ok);
    // BOARD_VARIETY: 3 to-do (open, blocked, unknown), 1 in progress,
    // 2 complete, 1 deferred-and-not-a-row.
    assert_eq!(snap.header_count(Column::ToDo), "3");
    assert_eq!(snap.header_count(Column::InProgress), "1");
    assert_eq!(snap.header_count(Column::Complete), "2");
    assert_eq!(snap.deferred, 1, "deferred is counted, never a row");
    // Nothing truncated: every bead in the read is accounted for exactly
    // once, which is ADR-0007's invariant I1 arriving intact through the
    // poll path.
    assert_eq!(snap.total() + snap.deferred, 7);
    assert_eq!(
        snap.beads_in(Column::Complete)
            .iter()
            .map(|b| b.id.as_str())
            .collect::<Vec<_>>(),
        vec!["looprs-done-1", "looprs-done-2"]
    );
    assert_eq!(
        snap.beads_in(Column::ToDo)
            .iter()
            .filter(|b| b.marker.is_some())
            .count(),
        2,
        "the blocked and unknown beads arrive marked, as the ADR maps them"
    );
}

// ───────────────────── the error ladder, over time ─────────────────────

/// A failure keeps the last good rows and sets the error; a later success
/// clears it. Both directions are the whole "never lose the last good
/// snapshot" contract, and both have to be watched on one running poller
/// rather than asserted on a hand-built value.
#[tokio::test]
async fn a_failure_keeps_the_last_good_board_and_a_later_success_clears_it() {
    let r = running(
        "poll-recovers",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(60),
    );
    wait_snapshot(&r.handle, "a good board to lose", |s| {
        s.read.is_ok() && s.total() == 6
    })
    .await;
    let good_rows = r.handle.borrow().columns.clone();
    let good_at = r
        .handle
        .borrow()
        .fetched_at
        .expect("a good read stamps its clock");

    // Break the board *after* it was readable: the case a user actually
    // hits, hours into a run.
    r.fakes.fail_bd(true);
    until("the poller noticed the board broke", WAIT, || {
        r.handle.borrow().read.is_error()
    })
    .await;

    let after = r.handle.borrow();
    assert!(after.read.is_error(), "{:?}", after.read);
    assert!(
        matches!(
            &after.read,
            BoardRead::Failed {
                code: Some(3),
                message: Some(m)
            } if m.contains("fail marker set")
        ),
        "the error carries the `bd` side of its own story: {:?}",
        after.read
    );
    assert_eq!(
        after.columns, good_rows,
        "an error must not touch the last good rows"
    );
    assert_eq!(after.deferred, 1, "…or the deferred count");
    let stamp = after
        .fetched_at
        .expect("the stamp survives the error that follows it");
    assert!(
        stamp >= good_at,
        "the stamp dates the last good read, not the failed one"
    );
    let age1 = after
        .age
        .expect("the kept rows carry an age: the footer needs it to say how stale");
    // The header stops counting — this read did not answer, so nothing new
    // may be counted from it.
    for col in Column::ALL {
        assert_eq!(after.header_count(col), "—", "{}", col.name());
    }
    drop(after);

    // A second failed tick, so the stamp is shown not to drift: it dates the
    // last good read, so it cannot move, while the age — the whole content of
    // the footer's `stale Ns` — has to grow.
    until("a second failed read", WAIT, || {
        let s = r.handle.borrow();
        s.read.is_error() && s.age.is_some_and(|a| a > age1)
    })
    .await;
    let again = r.handle.borrow();
    assert_eq!(
        again.fetched_at,
        Some(stamp),
        "failed reads must not refresh the stamp"
    );
    assert!(
        again.age.is_some_and(|a| a > age1),
        "…and the stale marker has to get staler, or the footer is lying about \
             a board nobody has looked at"
    );
    drop(again);

    // …and the recovery clears the error instead of leaving it sticky.
    r.fakes.fail_bd(false);
    until("the poller saw the board come back", WAIT, || {
        r.handle.borrow().read.is_ok()
    })
    .await;
    let back = r.handle.borrow();
    assert_eq!(back.read, BoardRead::Ok);
    assert!(!back.read.is_error(), "{:?}", back.read);
    assert_eq!(back.total(), 6, "the board is counted again");
}

// ─────────────────── no overlap, no pile-up, no orphans ───────────────────

/// **Two reads in flight is impossible**, measured rather than argued.
///
/// The fake records its pid on the way in and out (see
/// `tests/fixtures/fake_bd.sh`), so "how many `bd` were alive at once" is
/// a fact read off the log and not a thing the poller says about itself. With
/// a 350 ms read against a 60 ms tick a poller that *could* overlap has an
/// enormous amount of room to do it, which is exactly what this test buys.
#[tokio::test]
async fn a_slow_board_never_gets_two_reads_in_flight_at_once() {
    let r = running(
        "poll-no-overlap",
        BdFake::Slow,
        BOARD_VARIETY,
        Duration::from_millis(60),
    );
    wait_reads(&r.fakes, 3).await;

    assert_eq!(
        r.fakes.bd_max_concurrency(),
        1,
        "two `bd` reads overlapped: {:?}",
        r.fakes.bd_trace()
    );
    // Every read that started has finished, except the one in flight right
    // now. A started-and-never-finished pile is the other way this breaks.
    let (starts, stops) = (r.fakes.bd_starts(), r.fakes.bd_stops());
    assert!(
        starts - stops <= 1,
        "{starts} reads started and only {stops} finished: {:?}",
        r.fakes.bd_trace()
    );
}

/// Reads never come faster than the interval the knob was set to — measured
/// from the **producer's own clock**, which is what makes it a test rather
/// than a load measurement.
///
/// Because the stamp is taken when a read *begins* (see `poll_task`), the
/// distance between two consecutive stamps is `max(interval, read)`: the
/// next read cannot start until its tick is due **and** the previous one has
/// come back. So the gap cannot be shorter than the interval, and a reader
/// that arrives late can skip whole snapshots but can never shorten the
/// distance between the two it did see. Measuring the same property with
/// the distance between the two it did see. Measuring the same property with
/// wall-clock *arrival* times cannot promise that: this suite runs several
/// hundred tests at once and has been seen to starve a single-threaded
/// runtime for a second, which is a measurement error of the same size as
/// the thing being measured. The producer's stamps are not subject to that,
/// and so this test is the schedule's, not the machine's.
///
/// The other half of "must not pile up" — no two reads in flight — is
/// structural and tested by
/// `a_slow_board_never_gets_two_reads_in_flight_at_once`.
#[tokio::test]
async fn reads_never_come_faster_than_the_configured_interval() {
    let interval = Duration::from_millis(300);
    let r = running("poll-cadence", BdFake::Ok, BOARD_VARIETY, interval);
    let stamps = distinct_stamps(&r.handle, 4).await;
    for pair in stamps.windows(2) {
        let gap = pair[1].duration_since(pair[0]);
        // The one-tenth of slack is clock granularity and nothing else: the
        // bound is `gap >= interval + read`, and a read is not negative.
        assert!(
            gap * 10 >= interval * 9,
            "two snapshots are {:?} apart on a {:?} interval — the poller is reading \
                 faster than it was told to",
            gap,
            interval
        );
    }
}
