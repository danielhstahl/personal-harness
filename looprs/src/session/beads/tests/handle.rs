//! //! The session handle: entering the mode, leaving it, and shutting down.
//!
//! `Tab` in, `Tab` out, and the exit path. The policy under test is
//! [`SwitchAway::DrainThenPark`](crate::session::SwitchAway) — a pass already
//! running is left to finish, a pass merely wanted is deferred — and the thing
//! that makes it testable is that a resume *sets* `pending` rather than
//! spawning, so the test can watch a second entry refuse to double-spawn.

use super::*;

/// Entering the beads mode is what self-starts the loop — the same "opens
/// working, not idling" behavior the app has always had, now expressed as
/// `set_active(true)` rather than a special call in main.
#[tokio::test]
async fn entering_the_mode_starts_one_pass_and_only_one() {
    let fakes = Fakes::new("enter", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut s, _rx) = beads(&fakes, 1);

    assert_eq!(
        s.status(),
        SessionStatus::NotStarted,
        "start spawns nothing"
    );

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the session task acked");
    assert_eq!(fakes.pi_spawns(), 1, "entering the mode ran one pass");
    assert_eq!(s.status(), SessionStatus::Running);

    // Entering again must not run a second pass while a worker is live.
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    assert_eq!(fakes.pi_spawns(), 1, "re-entering cannot double-spawn");
}

/// ADR-0002 Q3, DrainThenPark: a Tab away mid-run kills nothing and starts
/// nothing, and every settle that arrives off-screen is remembered as *one*
/// pass to run when the user comes back.
#[tokio::test]
async fn switching_away_drains_then_parks_and_resuming_never_double_spawns() {
    let fakes = Fakes::new(
        "park",
        PiFake::Started,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut s, _rx) = beads(&fakes, 2);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    let worker = fakes.pi_pids()[0];
    assert!(process_alive(worker), "worker is running");

    // Tab away mid-run.
    s.set_active(false).unwrap();
    assert!(s.quiesce().await);
    assert!(
        process_alive(worker),
        "a Tab must not kill the in-flight worker: that work is already paid for"
    );

    // Three settles arrive while hidden — the loop's own worker, tagged with
    // the pass that made them, on a ticket that has now closed so that none of
    // them is a stop condition (looprs-w7q). Three *un-closed* settles would
    // be a different test: the first one stops the loop.
    fakes.set_board(SECOND_BOARD);
    show_closed(&fakes, "looprs-26r");
    let pass = s.in_flight().expect("a pass is in flight");
    for _ in 0..3 {
        s.cmd
            .send(BeadsCmd::WorkerSettled { serial: pass })
            .unwrap();
    }
    assert!(s.quiesce().await);
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "no new pass may start while the mode is hidden"
    );
    assert!(process_alive(worker), "and still no kill");

    // Tab back: resumes, once.
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    assert_eq!(
        fakes.pi_spawns(),
        2,
        "three deferred settles must coalesce into exactly one resumed pass"
    );
    assert!(
        !process_alive(worker),
        "the resumed pass reaped the old worker at its own pass boundary"
    );
    assert_eq!(s.status(), SessionStatus::Running);
}

/// Shutdown reaps the worker, says so once, and leaves nothing behind.
#[tokio::test]
async fn shutdown_reaps_the_worker_and_reports_the_session_gone() {
    let fakes = Fakes::new(
        "shutdown-beads",
        PiFake::Started,
        BdFake::Ok,
        ONE_BEADED_BOARD,
    );
    let (mut s, mut rx) = beads(&fakes, 3);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    let worker = fakes.pi_pids()[0];
    assert!(process_alive(worker));

    s.shutdown().unwrap();
    let evs = collect_until_down(&mut rx).await;
    assert!(!process_alive(worker), "shutdown must reap the worker");
    assert_eq!(
        evs.iter()
            .filter(|e| matches!(e, SessionEvent::Exited { .. }))
            .count(),
        1,
        "exactly one Exited: {evs:?}"
    );
    assert_eq!(s.status(), SessionStatus::Dead);
}
