//! //! The change detector: what the cheap probe buys, and every way it could lie.
//! //!
//! //! The whole point of the journal half of the poller is that a quiet journal
//! //! means no board read — so every test here is a shape of "quiet" that must
//! //! not be trusted: a probe that could not answer, a watermark `bd` has
//! //! pruned out from under it, a journal switched off (a quiet *lie*), a change
//! //! that landed while a read was already in flight, and an unjournaled change
//! //! that only the periodic sweep can see. The last two are the detector working
//! //! as designed: a big journal drains in bounded batches, and with the
//! //! detector off the poller reads the board every tick exactly as it used to.

use crate::services::board_poller::config::{CATCHUP_LIMIT, JournalConfig, PROBE_LIMIT};

use super::*;

// ═══════════════════ the change detector (ADR-0007 §7) ═══════════════════
//
// Everything below is the same claim from different sides: the journal is
// what the poller asks, the board is what it reads, and **no variation of
// "the probe said nothing" may ever leave a changed board unpainted**. The
// first two tests are the saving; the rest are the safety.

/// The saving this whole change is for: a quiet board costs a probe, not a
/// board read.
#[tokio::test]
async fn a_quiet_journal_costs_no_board_read_after_the_first_one() {
    let r = running_journal(
        "detector-quiet",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(40),
        never_sweep(),
    );
    wait_settled(&r, 3).await;
    // Several more ticks, and the board is still read exactly once — the
    // first one, which is not optional.
    tokio::time::sleep(Duration::from_millis(240)).await;
    assert_eq!(
        board_reads(&r.fakes).len(),
        1,
        "a quiet journal re-read the board: probes {:?}",
        probe_since(&r.fakes)
    );
    // …and that is not the skip-forever failure masquerading as saving: the
    // board *was* read, and is on the handle.
    assert_eq!(r.handle.borrow().total(), 6, "the first read landed");
    assert!(r.handle.borrow().read.is_ok());
    assert!(
        probes(&r.fakes).len() >= 5,
        "it stopped probing rather than stopping reading"
    );
}

/// One record in the journal buys **one** board read — not one per record,
/// and not one per tick afterwards.
#[tokio::test]
async fn one_journal_record_buys_exactly_one_board_read() {
    let r = running_journal(
        "detector-moved",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(40),
        never_sweep(),
    );
    wait_settled(&r, 2).await;
    let before = board_reads(&r.fakes).len();

    r.fakes.bump_journal(1);
    until("the read the journal asked for", WAIT, || {
        board_reads(&r.fakes).len() > before
    })
    .await;

    // And it settles: the read it just did covered the change, so the next
    // several ticks are quiet ticks again. A detector that re-read on every
    // tick after a single record is no detector at all.
    tokio::time::sleep(Duration::from_millis(240)).await;
    assert_eq!(
        board_reads(&r.fakes).len(),
        before + 1,
        "one record caused more than one read: probes {:?}",
        probe_since(&r.fakes)
    );
}

/// **The watermark rule, tested as a lost change would appear.**
///
/// The dangerous implementation is the poller that adopts the highest seq it
/// has ever *seen* — including one seen after the board read that is meant
/// to have covered it. That loses the change silently: the rows never
/// contain it, the watermark is past it, and the band never asks again. With
/// the safe rule (adopt only what the probe saw *before* the read) the same
/// race costs one extra read, which is what this asserts.
///
/// `BdFake::Slow` is what makes the window wide enough to matter: 350 ms of
/// read against a 20 ms tick is ~17 ticks of room to drop a change into.
#[tokio::test]
async fn a_change_that_lands_during_a_read_is_still_picked_up() {
    let r = running_journal(
        "detector-midread",
        BdFake::Slow,
        BOARD_VARIETY,
        Duration::from_millis(20),
        never_sweep(),
    );
    wait_settled(&r, 2).await;
    let before = board_reads(&r.fakes).len();

    // First change: this is the read that will be running when the second
    // one arrives.
    r.fakes.bump_journal(1);
    until("a `bd` in flight for the first change", WAIT, || {
        r.fakes.bd_starts() > r.fakes.bd_stops()
    })
    .await;
    // Second change, dropped into the middle of that read.
    r.fakes.bump_journal(1);

    until(
        "a second read, for the change that arrived mid-read",
        WAIT,
        || board_reads(&r.fakes).len() >= before + 2,
    )
    .await;
    // And the watermark moved past both records: the detector is not stuck
    // replaying them, and it is not ahead of them either.
    until("the watermark to pass seq 2", WAIT, || {
        probe_since(&r.fakes).last().copied().unwrap_or(0) >= 2
    })
    .await;
}

/// The sweep, and nothing else, is what can see a change the journal did not
/// record — `bd dolt pull`, `bd sql`, a workspace with the journal off.
#[tokio::test]
async fn an_unjournaled_change_lands_on_the_sweep_and_the_journal_never_claimed_it() {
    let sweep = Duration::from_millis(150);
    let r = running_journal(
        "detector-sweep",
        BdFake::Ok,
        ONE_BEADED_BOARD,
        Duration::from_millis(25),
        JournalConfig {
            enabled: true,
            reconcile: sweep,
        },
    );
    wait_settled(&r, 2).await;
    assert_eq!(r.handle.borrow().total(), 1);

    // Change the board **without** a journal record. Nothing the probe can
    // see has happened.
    r.fakes.set_board_unjournaled(BOARD_VARIETY);
    let watermark_at_change = *probe_since(&r.fakes).last().expect("a probe ran");

    until(
        "the sweep to notice what the journal could not",
        WAIT,
        || r.handle.borrow().total() == 6,
    )
    .await;
    assert_eq!(
        *probe_since(&r.fakes).last().expect("a probe ran"),
        watermark_at_change,
        "the watermark moved, so this was the journal taking credit for a change \
             it never saw"
    );
}

/// A probe that cannot be answered is never read as "nothing changed". The
/// board keeps getting read while the detector is down, and goes quiet again
/// when it comes back.
#[tokio::test]
async fn a_probe_that_cannot_answer_never_reads_as_a_quiet_board() {
    let r = running_journal(
        "detector-broken",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(40),
        never_sweep(),
    );
    wait_settled(&r, 2).await;
    let before = board_reads(&r.fakes).len();

    r.fakes.fail_journal(true);
    until("the board read that a broken probe owes", WAIT, || {
        board_reads(&r.fakes).len() > before
    })
    .await;
    // …every tick, not once: with the detector down the poller is back to
    // asking the board directly.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        board_reads(&r.fakes).len() >= before + 4,
        "a broken probe stopped the board being read: probes {:?}",
        probe_since(&r.fakes)
    );

    // Recovery is not sticky either.
    r.fakes.fail_journal(false);
    // Give the detector a few ticks to notice it can answer again; reads
    // stop the tick a quiet probe lands.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let settled = board_reads(&r.fakes).len();
    tokio::time::sleep(Duration::from_millis(240)).await;
    assert_eq!(
        board_reads(&r.fakes).len(),
        settled,
        "still reading after the detector recovered: {:?}",
        probe_since(&r.fakes)
    );
}

/// A watermark the retention floors pruned away is a **typed** answer with
/// an address in it, and the poller goes there: read the board, resume from
/// the head `bd` named, do not re-probe the pruned prefix forever.
#[tokio::test]
async fn a_pruned_watermark_rebaselines_at_the_head_bd_named() {
    let r = running_journal(
        "detector-truncated",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(40),
        never_sweep(),
    );
    wait_settled(&r, 2).await;
    assert_eq!(*probe_since(&r.fakes).last().unwrap(), 0, "starts at zero");

    r.fakes.truncate_journal(10, 42);
    until("a probe above the retention floor", WAIT, || {
        *probe_since(&r.fakes).last().unwrap() >= 42
    })
    .await;
    assert!(
        board_reads(&r.fakes).len() >= 2,
        "the re-baseline did not come with a fresh board read"
    );
    // And the truncation is not a permanent condition: it settled at the
    // head instead of looping on the refusal.
    let settled = board_reads(&r.fakes).len();
    tokio::time::sleep(Duration::from_millis(240)).await;
    assert_eq!(
        board_reads(&r.fakes).len(),
        settled,
        "looping on the truncation: {:?}",
        probe_since(&r.fakes)
    );
}

/// A workspace with the journal switched off answers the probe with *nothing*,
/// successfully, forever. That is the case the sweep is there for, and the
/// band must keep following the board.
#[tokio::test]
async fn a_disabled_journal_is_a_quiet_lie_the_sweep_covers() {
    let r = running_journal(
        "detector-disabled",
        BdFake::Ok,
        ONE_BEADED_BOARD,
        Duration::from_millis(25),
        JournalConfig {
            enabled: true,
            reconcile: Duration::from_millis(120),
        },
    );
    wait_settled(&r, 2).await;

    r.fakes.disable_journal(true);
    r.fakes.set_board_unjournaled(BOARD_VARIETY);
    until("a sweep past the disabled journal", WAIT, || {
        r.handle.borrow().total() == 6
    })
    .await;
    // The probe is still being asked (cheap), it just never says anything.
    assert!(!probes(&r.fakes).is_empty());
}

/// A journal with more in it than one batch can carry is **drained**, not
/// guessed at — and while it is being drained the poller reads the board
/// every tick, because it cannot know whether the records it has not read
/// touched a bead. That is the property that makes a big journal delay the
/// savings rather than cost more than the pre-detector poller ever did.
#[tokio::test]
async fn a_big_journal_drains_in_bounded_batches_and_then_starts_saving() {
    let fakes = Fakes::new(
        "detector-catchup",
        PiFake::Started,
        BdFake::Ok,
        BOARD_VARIETY,
    );
    let mut prefill = String::new();
    for seq in 1..=(CATCHUP_LIMIT * 2) {
        prefill.push_str(&format!(
            r#"{{"seq":{seq},"op":"update","issue_id":"looprs-old-{seq}"}}"#
        ));
        prefill.push('\n');
    }
    fakes.set_journal(&prefill);
    let (_poller, handle) = BoardPoller::spawn(BoardConfig {
        bin: fakes.bd_bin().to_string(),
        interval: Duration::from_millis(30),
        enabled: true,
        journal: never_sweep(),
    });

    until("the drain to reach the head", WAIT, || {
        probes(&fakes).len() >= 4 && probe_limit(&fakes).last() == Some(&PROBE_LIMIT)
    })
    .await;

    // Every batch was bounded: no single probe was asked to swallow the lot.
    let limits = probe_limit(&fakes);
    assert_eq!(limits[0], CATCHUP_LIMIT, "starts in catch-up");
    assert!(
        limits.iter().all(|l| *l <= CATCHUP_LIMIT),
        "an unbounded probe: {limits:?}"
    );
    // The watermark walked the journal in CATCHUP_LIMIT steps.
    let since = probe_since(&fakes);
    assert!(
        since.windows(2).all(|w| w[1] >= w[0]),
        "the watermark went backwards: {since:?}"
    );
    assert!(
        since.contains(&(CATCHUP_LIMIT * 2)),
        "never drained to the head: {since:?}"
    );
    // While draining, every tick that found records read the board: two
    // full batches in, two reads out. Skipping them would be the lost-change
    // bug wearing a different name.
    assert!(
        board_reads(&fakes).len() >= 2,
        "drained the journal without reading the board it was behind: reads {}, probes {:?}",
        board_reads(&fakes).len(),
        since
    );
    // And once drained, it stops.
    let settled = board_reads(&fakes).len();
    tokio::time::sleep(Duration::from_millis(240)).await;
    assert_eq!(
        board_reads(&fakes).len(),
        settled,
        "still reading after catching up: {:?}",
        probe_since(&fakes)
    );
    assert_eq!(handle.borrow().total(), 6);
}

/// `LOOPRS_KANBAN_EVENTS=0`: the pre-detector poller, exactly. Every tick a
/// board read, and the journal is never touched at all.
#[tokio::test]
async fn detector_off_reads_the_board_every_tick_and_never_the_journal() {
    let r = running_journal(
        "detector-off",
        BdFake::Ok,
        BOARD_VARIETY,
        Duration::from_millis(40),
        JournalConfig {
            enabled: false,
            reconcile: Duration::from_secs(3600),
        },
    );
    wait_reads(&r.fakes, 4).await;
    assert!(
        board_reads(&r.fakes).len() >= 4,
        "the board stopped being read every tick: {:?}",
        r.fakes.bd_log()
    );
    assert!(
        probes(&r.fakes).is_empty(),
        "LOOPRS_KANBAN_EVENTS=0 still probed the journal: {:?}",
        probes(&r.fakes)
    );
}
