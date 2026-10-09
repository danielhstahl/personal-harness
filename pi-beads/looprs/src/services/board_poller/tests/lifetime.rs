//! //! The task's own life: what is running, what must not be, and what is left
//! //! behind when the owner goes.
//! //!
//! //! A poller dropped mid-read leaves no `bd` behind; a board turned off runs
//! //! nothing at all — not a probe, not a sweep, not a publish; and a board
//! //! nobody reads stops polling rather than publishing into an empty room. All
//! //! three are the same promise seen from the process table.

use super::*;

/// Dropping the poller mid-read leaves **no `bd` child**, and does not hang.
///
/// `kill_on_drop(true)` is set in `services::bd::run`; this is the test that
/// checks it covers the poll path. The drop lands while the fake is still
/// asleep, which is the only moment the question is interesting — after the
/// read has returned there is nothing left to kill.
#[tokio::test]
async fn dropping_the_poller_mid_read_leaves_no_bd_running() {
    let fakes = Fakes::new(
        "poll-dropped-mid-read",
        PiFake::Started,
        BdFake::Slow,
        BOARD_VARIETY,
    );
    let (poller, handle) = BoardPoller::spawn(BoardConfig {
        bin: fakes.bd_bin().to_string(),
        interval: Duration::from_millis(60),
        enabled: true,
        journal: sweep_every(Duration::from_millis(60)),
    });
    // Wait for a read that is *in flight* — started, and not yet finished.
    until("the fake bd to have a read in flight", WAIT, || {
        fakes.bd_starts() > fakes.bd_stops()
    })
    .await;
    let pid = fakes
        .last_bd_pid()
        .expect("a read is in flight, so it has a pid");
    assert!(
        process_alive(pid),
        "the read about to be interrupted is not running: {pid}"
    );

    // The drop *is* the shutdown, and it must not wait for the read.
    let dropped = Instant::now();
    drop(handle);
    drop(poller);
    assert!(
        dropped.elapsed() < Duration::from_secs(1),
        "dropping the poller waited on the read instead of ending it: {:?}",
        dropped.elapsed()
    );

    // And the child went with it: `kill_on_drop` fires on the drop and the
    // runtime reaps the body.
    until(
        &format!("bd child {pid} to die with its poller"),
        WAIT,
        || !process_alive(pid),
    )
    .await;
    assert!(!process_alive(pid), "bd child {pid} outlived its poller");
}

/// A board that is off spawns no task and runs no process. Off has to mean
/// *off*, not "polls quietly and hides the answer".
#[tokio::test]
async fn a_board_that_is_off_runs_nothing_at_all() {
    let fakes = Fakes::new("poll-off", PiFake::Started, BdFake::Ok, BOARD_VARIETY);
    let (poller, handle) = BoardPoller::spawn(BoardConfig {
        bin: fakes.bd_bin().to_string(),
        interval: Duration::from_millis(60),
        enabled: false,
        journal: sweep_every(Duration::from_millis(60)),
    });

    assert!(!handle.is_enabled(), "the handle knows too");
    assert!(
        poller.task.is_none(),
        "a disabled board must not be left with a task to leak"
    );
    // Give every chance of having ticked.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fakes.bd_calls(),
        0,
        "a disabled board ran `bd` anyway: {:?}",
        fakes.bd_log()
    );
    // …and the value it hands out is the never-loaded one — which nothing
    // draws, because `is_enabled()` says not to.
    assert_eq!(handle.borrow().read, BoardRead::Never);
}

/// A poller nobody reads stops polling: there is no point publishing news to
/// an empty room.
#[tokio::test]
async fn a_board_nobody_reads_stops_polling() {
    let fakes = Fakes::new(
        "poll-no-readers",
        PiFake::Started,
        BdFake::Ok,
        BOARD_VARIETY,
    );
    let (poller, mut handle) = BoardPoller::spawn(BoardConfig {
        bin: fakes.bd_bin().to_string(),
        interval: Duration::from_millis(40),
        enabled: true,
        journal: sweep_every(Duration::from_millis(40)),
    });
    wait_reads(&fakes, 1).await;
    // The poller itself stays alive the whole time, so the only thing that
    // can stop the task is this: nobody left to publish to.
    assert!(handle.changed().await, "the second read never arrived");
    drop(handle);
    // Let whatever tick was in flight when the reader left finish, then take
    // the baseline. The poller checks `receiver_count` at the top of every
    // tick, so this window — ten ticks' worth — is far more than the one it
    // needs to notice and return.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let before_reads = board_reads(&fakes).len();
    let before = fakes.bd_starts();
    // And now nothing: no probe, no read, no process. Six ticks' worth of
    // silence is the assertion — a poller that had not stopped would have
    // run several of each by now.
    tokio::time::sleep(Duration::from_millis(240)).await;
    assert_eq!(
        fakes.bd_starts(),
        before,
        "`bd` ran after the last reader left: {} -> {}",
        before,
        fakes.bd_starts()
    );
    assert_eq!(
        board_reads(&fakes).len(),
        before_reads,
        "a board read happened with nobody left to see it"
    );
    assert!(poller.task.is_some(), "a running board owns a task");
}
