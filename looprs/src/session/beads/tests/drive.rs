//! //! Who drives the loop (looprs-msj): the worker's own settle, and nothing else.
//!
//! None of these tests has an App, an input mode, or a `UiCommand` in it, which
//! is the point — the transition is answered inside the session, off the child
//! that made the event. The abort half lives here too, because `Esc` and a
//! settle are the same question asked at different moments: is this the pass I
//! am still running, and may it buy another?

use super::*;

// ------------- who drives the loop? (looprs-msj) -------------
//
// Four tests, one claim each: the transition happens inside this session, off
// this session's own worker, and off nothing else. None of them has an App, an
// input mode, or a `UiCommand` in it, which is the point — the App cannot be
// part of the path any more because there is nothing left for it to send.

/// **The acceptance case, run end to end with real processes.** A beads worker
/// settles, and the next pass starts. There is no UI in this test in any form:
/// the only things that ever touched this session were one `set_active` and the
/// fake worker's own stdout, so the advance cannot have come from anywhere but
/// the bead's side of the boundary.
#[tokio::test]
async fn the_loop_takes_its_next_pass_from_its_own_workers_settle() {
    let fakes = Fakes::new(
        "settle-drives-loop",
        PiFake::Chat,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    // `quiesce`, not a log poll: the fake logs the prompt *before* it answers,
    // so a status asserted against the log races the answer. Quiesce is FIFO on
    // the session's own mailbox — when it returns, the pass has started and the
    // status mirror has been published behind it.
    assert!(s.quiesce().await, "the entry command was handled");
    assert_eq!(fakes.pi_spawns(), 1, "one pass, from entering the mode");
    assert_eq!(s.in_flight(), Some(1));
    // The harness claimed the ticket before it bought the worker, so the id it
    // claimed is the id the worker was prompted with and the id the settle is
    // about to be checked against (looprs-w7q).
    assert!(fakes.claimed("looprs-26r"), "{:?}", fakes.bd_log());

    // The worker did its job: the ticket is closed, and the next ready bead is
    // a *different* one. Both halves are load-bearing — the loop advances on a
    // closed ticket, and an un-closed one stops it dead rather than buying a
    // second pass on the same thing.
    fakes.set_board(SECOND_BOARD);
    show_closed(&fakes, "looprs-26r");
    fakes.settle();
    fakes.wait_for_pi_spawns(2).await;

    assert_eq!(
        fakes.pi_spawns(),
        2,
        "the settle drove the next pass — no command told anyone to"
    );
    assert_eq!(
        fakes.pi_prompts().len(),
        2,
        "and the new pass was prompted, not left idle the way this loop used to start"
    );
    // `wait_for_pi_spawns` returns the moment the fake logs the spawn, which is
    // *inside* the session task's still-running settle handler — before that
    // handler returns and republishes the `in_flight` mirror. This assertion
    // raced that gap (a flake it had been carrying since looprs-msj); the seam
    // is the only honest way to read a mirror written by another task.
    assert!(
        s.quiesce().await,
        "the settle-driven pass finished starting and published its serial"
    );
    assert_eq!(s.in_flight(), Some(2), "the new pass is the live one");
    assert!(
        drain(&mut rx).contains(&"step:work".to_string()),
        "and the UI was told, as a render, not asked, as a command"
    );

    // Nothing drives a *third* pass: the loop moves when a pass settles and
    // nowhere else, so it sits waiting rather than running the board by itself.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fakes.pi_spawns(), 2, "no settle, no pass");
}

/// A settle only moves the loop when it names the pass that is actually in
/// flight. A late or duplicated tail from an already-retired worker looks
/// identical on the wire otherwise, and acting on it would kill the live pass
/// — work already paid for — to start a pass nobody asked for.
#[tokio::test]
async fn a_settle_from_a_pass_this_loop_does_not_own_moves_nothing() {
    let fakes = Fakes::new(
        "stale-settle",
        PiFake::Started,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut s, _rx) = beads(&fakes, 1);

    // (a) Nothing is running: a settle cannot conjure work out of nowhere.
    s.cmd.send(BeadsCmd::WorkerSettled { serial: 1 }).unwrap();
    assert!(s.quiesce().await);
    assert_eq!(
        fakes.pi_spawns(),
        0,
        "a settle with no pass behind it starts nothing"
    );
    assert_eq!(s.status(), SessionStatus::NotStarted);

    // (b) A real pass, retired by a real settle, which then arrives late again.
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    let first = s.in_flight().expect("the first pass is live");
    // The ticket finished and the board moved on, so the pass below is retired
    // by a *successful* settle rather than stopped by the un-closed-ticket
    // guard — which would halt the loop for a reason that has nothing to do
    // with what this test is about.
    fakes.set_board(SECOND_BOARD);
    show_closed(&fakes, "looprs-26r");
    s.cmd
        .send(BeadsCmd::WorkerSettled { serial: first })
        .unwrap();
    assert!(s.quiesce().await);
    assert_eq!(fakes.pi_spawns(), 2, "the real settle drove the next pass");
    let second = s.in_flight().expect("the second pass is live");
    assert_ne!(first, second, "each pass gets its own serial");

    s.cmd
        .send(BeadsCmd::WorkerSettled { serial: first })
        .unwrap();
    assert!(s.quiesce().await);
    assert_eq!(
        fakes.pi_spawns(),
        2,
        "the late echo of a retired pass must not come over the top of the live one"
    );
    assert!(
        process_alive(fakes.pi_pids()[1]),
        "the live worker was not touched"
    );
    assert_eq!(s.in_flight(), Some(second));
}

/// **ADR-0002 Q3, the rule that makes the settle path not a tautology**: a
/// settle means "a pass finished" only when nobody cancelled it. An aborted
/// worker settles on the way out — that is how pi unwinds — and reading that as
/// "next bead" would spend a turn on the next ticket *because* the user pressed
/// Esc, which is the exact opposite of what Esc is for.
#[tokio::test]
async fn an_aborted_pass_parks_instead_of_advancing() {
    let fakes = Fakes::new("abort-parks", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the entry command was handled");
    let worker = fakes.pi_pids()[0];
    assert!(process_alive(worker), "a pass is running");

    s.abort().unwrap();
    let msgs = drain_until_parked(&mut rx).await;

    assert_eq!(
        fakes.pi_spawns(),
        1,
        "the abort parked the loop; it did not take the next bead: {msgs:?}"
    );
    assert!(
        !process_alive(worker),
        "a parked beads loop holds no warm child — the mode is cold by policy"
    );
    assert!(
        msgs.iter().any(|m| m.contains("cancel")),
        "the park says why, rather than looking like the board ran dry: {msgs:?}"
    );
    assert_eq!(s.status(), SessionStatus::Idle, "waiting on a human");
    assert_eq!(s.in_flight(), None, "nothing is in flight any more");
    assert_eq!(
        fakes.pi_verbs().iter().filter(|v| *v == "prompt").count(),
        1,
        "exactly one prompt ever went out: {:?}",
        fakes.pi_verbs()
    );
}

/// **Acceptance: Esc during a beads worker names the bead it is cancelling.**
///
/// "cancelling…" about a faceless pass is not much of an answer: the thing the
/// user is stopping is a *bead*, and the only way to be sure you cancelled the
/// one you meant is for the loop to say which one it was.
#[tokio::test]
async fn esc_names_the_bead_it_is_cancelling() {
    let fakes = Fakes::new("esc-names", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");
    assert_eq!(s.status(), SessionStatus::Running);
    drain(&mut rx);

    s.abort().unwrap();
    // Nothing has been settled yet, so this window is mostly the session's own
    // words — but the fake unwinds quickly, so the park may land inside it
    // too, and the assertions read the whole of it either way.
    let mut got =
        crate::testing::collect_within(&mut rx, Duration::from_millis(800), describe).await;
    let ack = got
        .iter()
        .position(|l| l.starts_with("system: cancelling") && l.contains("looprs-26r"));
    assert!(ack.is_some(), "the cancel did not name the bead: {got:?}");
    if !got.iter().any(|l| l == "step:await") {
        got.extend(drain_until_parked(&mut rx).await);
    }
    // Ordering, not absence: "cancelled" must not arrive before
    // "cancelling". A completion word that beats the acknowledgement leaves
    // the same silence in front of it that no acknowledgement at all would.
    if let Some(done) = got.iter().position(|l| l.contains("cancelled")) {
        assert!(
            ack.unwrap() < done,
            "the pass reported itself cancelled before the cancel was acknowledged: {got:?}"
        );
    }
    // The point of naming it: this is the pass that got stopped, and stopping
    // it does not buy another one.
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "naming the bead cancelled that pass and only that pass: {got:?}"
    );
    assert_eq!(s.status(), SessionStatus::Idle);
}

/// Esc on a beads loop with nothing running is invisible: no note, no error,
/// no worker, and no park — the loop was already waiting for a human, and a
/// no-op that announces itself is indistinguishable from a cancel that did
/// something.
#[tokio::test]
async fn esc_on_an_idle_loop_says_nothing_and_starts_nothing() {
    let fakes = Fakes::new("esc-idle-loop", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    assert_eq!(fakes.pi_spawns(), 0, "an empty board starts no worker");
    drain(&mut rx);

    s.abort().unwrap();
    assert!(s.quiesce().await, "the Esc was handled");
    let late =
        crate::testing::collect_within(&mut rx, cancel::GRACE + Duration::from_secs(1), |ev| {
            describe(ev)
        })
        .await;
    assert!(late.is_empty(), "an idle Esc made noise: {late:?}");
    assert_eq!(fakes.pi_spawns(), 0, "and started nothing");
    assert_eq!(s.status(), SessionStatus::Idle);
}

/// **The escalation: a worker that answers the abort and keeps going.**
///
/// This is the mode where the stall matters most, because a beads worker is a
/// `pi` child being paid by the token and the loop self-advances. Killed, the
/// loop parked, and — the part the board cannot show the user from here — the
/// bead named as still claimed, because a cancelled worker leaves its ticket
/// `in_progress` and that is the state the claim/close guard has to be able to
/// see (looprs-w7q).
#[tokio::test]
async fn a_worker_that_ignores_the_abort_is_killed_and_leaves_the_bead_named() {
    let fakes = Fakes::new("abort-stubborn", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
    fakes.stubborn_pi(true);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");
    let worker = fakes.pi_pids()[0];
    assert_eq!(s.status(), SessionStatus::Running);

    s.abort().unwrap();
    assert!(s.quiesce().await, "the Esc was handled");
    assert_eq!(
        s.status(),
        SessionStatus::Aborting,
        "waiting on an unwind that is not coming"
    );
    assert!(
        process_alive(worker),
        "the abort alone has not stopped it (that is the point of this fake)"
    );

    let late =
        crate::testing::collect_within(&mut rx, cancel::GRACE + Duration::from_secs(6), |ev| {
            describe(ev)
        })
        .await;
    assert!(
        !process_alive(worker),
        "the stalled worker was left running — and still billing"
    );
    assert!(
        late.iter()
            .any(|l| l.starts_with("error:") && l.contains("looprs-26r") && l.contains("killed")),
        "the stall did not say what it did, to which bead: {late:?}"
    );
    assert!(
        late.iter().any(|l| l.contains("stays claimed")),
        "the bead the worker leaves behind was not named as claimed: {late:?}"
    );
    assert!(
        late.iter().any(|l| l == "step:await"),
        "the loop stayed 'working' instead of handing the box back: {late:?}"
    );
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "killing a stalled worker is not a restart: a cancel that auto-retries is a respawn storm"
    );
    assert_eq!(
        s.status(),
        SessionStatus::Idle,
        "parked, waiting on a human"
    );
    assert_eq!(s.in_flight(), None, "nothing in flight");
}

/// The symmetric edge of the settle path. A worker that dies mid-run never
/// sends `agent_settled`, so without handling the stream end the loop sits
/// claiming it is working forever — with the input box hidden behind that
/// claim. It must come back to the human instead.
#[tokio::test]
async fn a_worker_that_dies_mid_pass_parks_the_loop_instead_of_hanging_it() {
    let fakes = Fakes::new("worker-dies", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(
        s.quiesce().await,
        "the pass started before anything killed it"
    );
    let victim = fakes.pi_pids()[0];
    assert_eq!(s.status(), SessionStatus::Running);

    crate::testing::kill_pid(victim);

    let msgs = drain_until_parked(&mut rx).await;
    assert!(
        has_error(&msgs),
        "the death is reported, not swallowed: {msgs:?}"
    );
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "parking is not a restart: a crash that auto-retries is a respawn storm"
    );
    assert!(!process_alive(victim));
    assert_eq!(
        s.status(),
        SessionStatus::Idle,
        "the loop is back in the human's hands, so the box comes back too"
    );
}
