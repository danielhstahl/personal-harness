//! //! Getting out: the reap that never runs alone (looprs-2ck).
//! //!
//! //! The task that owns the pty is also the only drainer of the reader thread's
//! //! bounded byte lane, so a shutdown that blocks on the child without emptying
//! //! the lane deadlocks the task, the reader thread and the child's own
//! //! `exit(2)` at once. These tests put the lane full and the reader parked, and
//! //! require the shutdown to return anyway — and the stubborn shell that ignores
//! //! the exit to be killed inside the bound rather than waited on forever.

use crate::session::bash::pty::{EXIT_ASK, KILL_REAP};
use std::time::Instant;

use super::*;

/// Shutdown kills the shell: no bash left behind.
#[tokio::test]
async fn shutdown_leaves_no_shell_running() {
    let (mut s, mut rx) = bash(12);
    s.send_text("echo hi".to_string()).unwrap();
    run_command(&mut rx).await;
    assert_eq!(s.status(), SessionStatus::Idle);

    s.shutdown().unwrap();
    let mut down = false;
    for _ in 0..50 {
        let line = next_event(&mut rx).await;
        if line.starts_with("down ") {
            down = true;
            break;
        }
    }
    assert!(down, "shutdown did not report the session gone");
    assert_eq!(s.status(), SessionStatus::Dead);
}

/// **Acceptance: shutdown returns while the byte lane is full and the reader
/// is parked on it** (looprs-2ck).
///
/// `yes` is the worst case the exit path can be handed, for three reasons
/// at once, and the bug needed all three: it never reads the `exit` we send
/// (a foreground job owns the shell), so the polite phase always runs out;
/// it outruns the lane immediately, so the reader thread is parked in
/// `blocking_send` instead of in `read(2)`; and it leaves the pty's own
/// kernel buffer full of bytes nobody has taken, which is what a dying
/// child blocks on inside `exit(2)`.
///
/// The old code put the session task into `wait4` there and the three of
/// them held each other down: task waiting on child, reader waiting on lane,
/// child waiting on the tty the reader had stopped emptying. What is
/// asserted is the ticket's two properties rather than the mechanism that
/// now provides them — **the session reports itself gone** (the `down` line
/// is emitted only after `BashTask::shutdown` returns) and **the reader
/// thread ends** (`readers_live` reaches 0, which needs the parked send to
/// fail against a dropped receiver).
#[tokio::test]
async fn shutdown_returns_while_the_lane_is_full_and_the_reader_is_parked() {
    let (mut s, mut rx) = bash(32);
    warm_shell(&mut s, &mut rx).await;
    in_flight(
        &mut s,
        &mut rx,
        "bash -c 'echo looprs-start''ed; exec yes'",
        CHILD_STARTED,
    )
    .await;
    // Let the producer get far enough ahead that the reader has something
    // in hand and nowhere to put it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        s.readers_live(),
        1,
        "the shell we are about to shut down should have exactly one reader thread"
    );

    let started = Instant::now();
    s.shutdown().unwrap();

    let seen = until_event(&mut rx, |l| l.starts_with("down "), "the shutdown notice").await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < SHUTDOWN_RETURNED_WITHIN,
        "shutdown took {elapsed:?} to report the session gone, past the {SHUTDOWN_RETURNED_WITHIN:?} \
             the whole reap is budgeted for: {seen:?}"
    );
    assert_eq!(
        s.status(),
        SessionStatus::Dead,
        "the session said it was down without making itself dead"
    );

    // The second property, and the one that used to be unverifiable: a
    // reader parked on a lane that has stopped draining is the failure, so
    // watch the count rather than the bytes.
    let readers = readers_gone(&s, Duration::from_secs(5)).await;
    assert_eq!(
        readers, 0,
        "a reader thread is still parked on this session's byte lane after {elapsed:?}"
    );
}

/// **Acceptance: a shell that will not take the `exit` gets killed, and the
/// kill is bounded too** (looprs-2ck).
///
/// The quiet half of the case above: `sleep 30` fills nothing, so the only
/// thing that can go wrong here is the wait itself. The assertion that this
/// really travelled the kill path rather than exiting politely is timing in
/// the *only* direction load cannot bend: it took **at least** `EXIT_ASK`,
/// because the ask deadline is armed before the shell is even asked and
/// nothing can make it elapse sooner.
#[tokio::test]
async fn a_shell_that_ignores_the_exit_is_killed_within_the_bound() {
    let (mut s, mut rx) = bash(33);
    warm_shell(&mut s, &mut rx).await;
    in_flight(
        &mut s,
        &mut rx,
        "bash -c 'echo looprs-start''ed; exec sleep 30'",
        CHILD_STARTED,
    )
    .await;

    let started = Instant::now();
    s.shutdown().unwrap();
    let seen = until_event(&mut rx, |l| l.starts_with("down "), "the shutdown notice").await;
    let elapsed = started.elapsed();

    assert!(
        elapsed >= EXIT_ASK,
        "this shutdown finished in {elapsed:?}, before the {EXIT_ASK:?} ask was even over, so it \
             never reached the kill it was meant to test: {seen:?}"
    );
    assert!(
        elapsed < EXIT_ASK + KILL_REAP * 4,
        "the kill-and-reap phase outstayed its budget ({elapsed:?} total, {KILL_REAP:?} budgeted): \
             {seen:?}"
    );
    assert_eq!(
        readers_gone(&s, Duration::from_secs(5)).await,
        0,
        "the reader thread outlived the shell it was reading"
    );
}
