//! //! The first pass: what a beads session does the moment it is entered, before
//! any of the interesting machinery has had a turn.
//!
//! Nothing here is about the second pass. These are the tests that pin "constructing
//! the session spawns nothing", "a board with tickets on it self-starts one
//! prompted worker", "an empty board parks instead", and the two ways the very
//! first spawn can fail — a `pi` that dies on the way up and a prompt the
//! dispatcher refuses — each of which has to be reported rather than left as a
//! silence with a spinner on it.

use crate::session::BeadStep;

use super::*;

/// A constructed loop must not have touched any process. The old code spawned a
/// pi child inside new() and never prompted it.
#[tokio::test]
async fn constructing_a_loop_spawns_nothing() {
    let fakes = Fakes::new("ctor", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let (l, mut rx, _ctl) = loop_with(&fakes);

    assert!(l.is_awaiting_input(), "a new loop starts parked");
    assert_eq!(fakes.pi_spawns(), 0, "new() must not spawn a pi child");
    assert_eq!(fakes.bd_calls(), 0, "new() must not even query the board");
    assert!(drain(&mut rx).is_empty());
}

/// The bug: with a non-empty board, launching looprs sat forever with a live,
/// idle pi child. Driving the loop must start real work with no human input.
#[tokio::test]
async fn a_non_empty_board_self_starts_a_prompted_worker() {
    let fakes = Fakes::new("self-start", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(
        timeout(NO_HANG, l.next()).await.is_ok(),
        "next() hung instead of starting a worker"
    );

    assert_eq!(fakes.pi_spawns(), 1, "exactly one worker spawned");
    let prompts = fakes.pi_prompts();
    assert_eq!(prompts.len(), 1, "the worker was prompted, not left idle");
    assert!(
        prompts[0].contains("technical software engineer"),
        "worker prompt missing: {}",
        prompts[0]
    );
    assert!(
        prompts[0].contains("looprs-26r"),
        "worker was not told which bead to work: {}",
        prompts[0]
    );
    // Invariant for the whole run: no child exists that was never prompted.
    assert_eq!(fakes.pi_spawns(), fakes.pi_prompts().len());
    assert!(matches!(l.get_step(), BeadStep::WorkTickets));
    assert!(!l.is_awaiting_input());
    assert!(drain(&mut rx).contains(&"step:work".to_string()));
}

#[tokio::test]
async fn an_empty_board_parks_without_spawning() {
    let fakes = Fakes::new("empty", PiFake::Started, BdFake::Ok, EMPTY_BOARD);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(timeout(NO_HANG, l.next()).await.is_ok());

    assert_eq!(fakes.pi_spawns(), 0, "empty board must not spawn anything");
    assert!(l.is_awaiting_input());
    let msgs = drain(&mut rx);
    assert!(msgs.contains(&"step:await".to_string()), "{msgs:?}");
    assert!(
        msgs.iter().any(|m| m.contains("board empty")),
        "parking should say why: {msgs:?}"
    );
}

/// Switching passes must reap the previous session: no orphan pi children, and no
/// unprompted child in the gap between sessions.
#[tokio::test]
async fn driving_the_loop_reaps_the_previous_worker() {
    let fakes = Fakes::new("reap", PiFake::Started, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut l, _rx, _ctl) = loop_with(&fakes);

    timeout(NO_HANG, l.next()).await.unwrap();
    let first = fakes.pi_pids()[0];
    assert!(process_alive(first), "worker should be running");

    // A second pass onto a different ticket: the old child goes away, the new one works.
    fakes.set_board(SECOND_BOARD);
    timeout(NO_HANG, l.next()).await.unwrap();
    assert!(
        !process_alive(first),
        "pid {first} survived the pass boundary: orphaned child"
    );
    let second = fakes.pi_pids()[1];
    assert_ne!(first, second);
    assert!(process_alive(second), "second worker should be running");

    // Draining the board parks the loop and reaps the live worker.
    fakes.set_board(EMPTY_BOARD);
    timeout(NO_HANG, l.next()).await.unwrap();
    assert!(!process_alive(second), "parked loop left a child running");
    assert_eq!(fakes.pi_spawns(), 2);
    assert_eq!(fakes.pi_prompts().len(), 2, "every child got a prompt");
    assert!(l.is_awaiting_input());
    assert!(l.pi_rx.is_none(), "parked loop must not hold a session");
}

/// pi dying during startup must surface as a transcript error and park the loop,
/// never as a hang or a panic in main.
#[tokio::test]
async fn a_pi_that_dies_during_startup_is_reported() {
    let fakes = Fakes::new(
        "dead-pi",
        PiFake::DiesImmediately,
        BdFake::Ok,
        ONE_BEADED_BOARD,
    );
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(
        timeout(NO_HANG, l.next()).await.is_ok(),
        "next() hung on a pi that died instead of reporting"
    );

    let msgs = drain(&mut rx);
    assert!(has_error(&msgs), "death should be reported: {msgs:?}");
    assert!(
        !msgs.contains(&"step:work".to_string()),
        "a dead worker must not claim to be working: {msgs:?}"
    );
    assert!(l.is_awaiting_input());
    assert!(l.pi_rx.is_none());
}

#[tokio::test]
async fn a_failing_bd_is_reported_and_parks() {
    let fakes = Fakes::new("bad-bd", PiFake::Started, BdFake::Fails, EMPTY_BOARD);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(timeout(NO_HANG, l.next()).await.is_ok());

    let msgs = drain(&mut rx);
    assert!(
        has_error(&msgs),
        "`bd ready` failure should surface: {msgs:?}"
    );
    assert_eq!(fakes.pi_spawns(), 0, "no worker without a board read");
    assert!(l.is_awaiting_input());
}

#[tokio::test]
async fn a_refused_prompt_is_reported_and_keeps_no_session() {
    let fakes = Fakes::new("refused", PiFake::Rejects, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(timeout(NO_HANG, l.next()).await.is_ok());

    assert!(has_error(&drain(&mut rx)));
    assert!(l.is_awaiting_input());
    assert!(l.pi_rx.is_none());
}

/// `disposition: "handled"` means pi took the prompt but started no run, so no
/// `agent_settled` will ever arrive to advance the loop. Holding that session
/// open would be the same idle-child trap this ticket is about.
#[tokio::test]
async fn a_handled_prompt_does_not_leave_an_idle_worker() {
    let fakes = Fakes::new("handled", PiFake::Handled, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(timeout(NO_HANG, l.next()).await.is_ok());

    let msgs = drain(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.starts_with("error: ") && m.contains("handled")),
        "{msgs:?}"
    );
    assert!(l.is_awaiting_input());
    assert!(
        l.pi_rx.is_none(),
        "handled session must be dropped, not kept"
    );
}
