//! //! The planner is checked before anybody works (looprs-k7v).
//!
//! `agent_settled` means "the planner stopped talking", not "there is a plan".
//! Every test below pins one of the verdicts the pre/post board diff can return,
//! plus the two things the ticket asked for: a plan of zero is impossible to
//! miss, and a plan of N is on the transcript before the first worker is paid
//! to read it.

use super::*;

// ---------------- the planner is checked before anybody works (looprs-k7v) ----------------
//
// `agent_settled` was being read as "there is a plan". It is not. Every test
// below pins one of the four verdicts the diff can return, and the two things
// the ticket asked for: a plan of zero is impossible to miss, and a plan of N
// is on screen before the first worker is paid to read it.

/// **The acceptance case.** A planner that creates nothing must not look like a
/// successful no-op: it must be an error, it must say so, and it must not
/// queue a worker.
#[tokio::test]
async fn a_plan_that_creates_no_tickets_is_a_loud_error_not_a_silent_idle() {
    let fakes = Fakes::new("plan-nothing", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    drain(&mut rx); // the "board empty" park note from entering the mode

    start_planner(&mut s, "gibberish that plans nothing").await;
    assert_eq!(
        board_reads(&fakes),
        1,
        "the board was snapshotted before the planner ran: {:?}",
        fakes.bd_log()
    );

    fakes.settle(); // the planner finishes, having written nothing

    let msgs = drain_until_parked(&mut rx).await;
    let err = last_error(&msgs)
        .unwrap_or_else(|| panic!("a zero-ticket plan must be an error: {msgs:?}"));
    assert!(err.contains("created no tickets"), "{err}");
    assert!(
        err.contains("reply 1"),
        "the planner's own final message is quoted, not paraphrased: {err}"
    );
    assert!(
        !msgs.contains(&"step:work".to_string()),
        "nothing was queued, so no worker runs: {msgs:?}"
    );
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "exactly the planner ran — no worker was bought: {:?}",
        fakes.pi_pids()
    );
    assert_eq!(
        board_reads(&fakes),
        2,
        "one snapshot before, one diff after: {:?}",
        fakes.bd_log()
    );
    assert_eq!(s.status(), SessionStatus::Idle, "the box comes back");
    assert_eq!(s.in_flight(), None, "and nothing is left in flight");
}

/// **The other acceptance case**, including its ordering: the human sees the
/// plan — id and title per ticket — before the first worker starts burning
/// tokens on it, so a bad plan can be Esc'd while it is still cheap.
#[tokio::test]
async fn a_successful_plan_is_listed_before_the_first_worker_starts() {
    let fakes = Fakes::new("plan-listed", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    drain(&mut rx);

    start_planner(&mut s, "two tickets, please").await;
    fakes.set_board(PLAN_TWO_TICKETS); // the planner created them mid-run
    fakes.settle();

    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    let listed = msgs
        .iter()
        .position(|m| m.contains("planner created 2"))
        .unwrap_or_else(|| panic!("the plan was never listed: {msgs:?}"));
    let working = msgs.iter().position(|m| m == "step:work").unwrap();
    assert!(
        listed < working,
        "the plan is on screen before the first worker runs: {msgs:?}"
    );
    let note = &msgs[listed];
    assert!(note.contains("looprs-101: first planned ticket"), "{note}");
    assert!(note.contains("looprs-102: second planned ticket"), "{note}");
    assert!(
        msgs[..=listed].iter().all(|m| !m.starts_with("error: ")),
        "a plan that worked is not reported as a failure: {msgs:?}"
    );

    assert_eq!(fakes.pi_spawns(), 2, "planner, then exactly one worker");
    let prompts = fakes.pi_prompts();
    assert!(
        prompts[0].contains("engineering manager"),
        "the first child was the planner"
    );
    assert!(
        prompts[1].contains("Your assigned ticket: looprs-101 — first planned ticket"),
        "and the second was put to work on a named ticket from the plan: {}",
        prompts[1]
    );
}

/// The diff is a set difference on ticket ids, not a count of the board. A
/// ticket that was already there is not evidence of a plan, and reporting it as
/// one would let "nothing was created" pass for "here is the plan".
#[tokio::test]
async fn tickets_that_predate_the_planner_are_not_reported_as_planned() {
    let fakes = Fakes::new("plan-diff", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    drain(&mut rx);
    // A ticket that was already on the board before the planner was asked for
    // anything. It goes in after entering the mode and before the submit, so
    // the loop is not off working it by the time the baseline is taken.
    fakes.set_board(PRE_EXISTING);
    start_planner(&mut s, "add one more ticket").await;
    fakes.set_board(OLD_PLUS_ONE_NEW);
    fakes.settle();

    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    let note = msgs
        .iter()
        .find(|m| m.contains("planner created 1"))
        .unwrap_or_else(|| panic!("only the new ticket should count: {msgs:?}"));
    assert!(note.contains("looprs-101"), "{note}");
    assert!(
        !note.contains("looprs-old"),
        "the pre-existing ticket is not part of the plan: {note}"
    );
}

/// A board that cannot be read at verification time is *unverifiable*, which is
/// a different sentence from "empty" and must never be shortened into it — the
/// user's next action differs (fix `bd` vs. re-word the instruction).
#[tokio::test]
async fn a_board_that_cannot_be_read_after_planning_is_unverifiable_not_empty() {
    let fakes = Fakes::new("plan-unreadable", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    start_planner(&mut s, "plan something").await;
    fakes.fail_bd(true); // the snapshot got through; the diff read does not
    fakes.settle();

    let msgs = drain_until_parked(&mut rx).await;
    let err = last_error(&msgs)
        .unwrap_or_else(|| panic!("an unreadable board must be an error: {msgs:?}"));
    assert!(err.contains("cannot verify"), "{err}");
    assert!(
        !err.contains("created no tickets"),
        "a read failure must not be reported as an empty plan: {err}"
    );
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "no worker runs on a plan that could not be checked"
    );
    assert!(!msgs.contains(&"step:work".to_string()), "{msgs:?}");
}

/// The snapshot is taken before the child is bought, so a board that was never
/// readable costs one `bd` call rather than a whole planner session.
#[tokio::test]
async fn a_board_that_cannot_be_snapshotted_costs_no_planner_run() {
    let fakes = Fakes::new("plan-no-baseline", PiFake::Chat, BdFake::Fails, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);

    s.send_text("plan something".to_string()).unwrap();
    assert!(s.quiesce().await);

    let msgs = drain(&mut rx);
    assert!(has_error(&msgs), "the refusal is visible: {msgs:?}");
    assert_eq!(
        fakes.pi_spawns(),
        0,
        "a pass that could not be verified afterwards should not have been paid for"
    );
    assert_eq!(s.status(), SessionStatus::NotStarted);
}

/// The planner dying mid-run is the no-settle case: nothing is ever going to
/// advance this loop, so it has to say so within a bounded time rather than go
/// on claiming it is planning.
#[tokio::test]
async fn a_planner_that_dies_mid_run_is_surfaced_and_starts_no_workers() {
    let fakes = Fakes::new("planner-dies", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    drain(&mut rx); // the empty-board park note
    start_planner(&mut s, "plan something").await;
    let victim = fakes.pi_pids()[0];
    assert!(
        process_alive(victim),
        "the planner is running before the kill"
    );

    crate::testing::kill_pid(victim);

    let msgs = drain_until_parked(&mut rx).await;
    let err = last_error(&msgs).unwrap_or_else(|| panic!("the death must be reported: {msgs:?}"));
    assert!(err.contains("planner"), "and named as the planner: {err}");
    assert!(
        !msgs.contains(&"step:work".to_string()),
        "an unverified plan never reaches the workers: {msgs:?}"
    );
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "parking is not a restart: a crash that auto-retries is a respawn storm"
    );
    assert!(!process_alive(victim));
    assert_eq!(s.status(), SessionStatus::Idle);
}

/// `disposition: "handled"` means no run started, so no `agent_settled` will
/// ever arrive to trigger the verification. Waiting for one is the hang; the
/// child is dropped and the failure is reported instead.
#[tokio::test]
async fn a_handled_planner_prompt_waits_for_no_settle_that_never_comes() {
    let fakes = Fakes::new("planner-handled", PiFake::Handled, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);

    s.send_text("plan something".to_string()).unwrap();
    assert!(s.quiesce().await);

    let msgs = drain(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.starts_with("error: ") && m.contains("handled")),
        "{msgs:?}"
    );
    assert_eq!(
        s.in_flight(),
        None,
        "the handled child is dropped, not held"
    );

    // And nothing resurrects the pass later: there is no settle to wait for,
    // and nothing that should.
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fakes.pi_spawns(), 1, "one spawn, ever");
    assert_eq!(s.status(), SessionStatus::NotStarted);
}

/// Esc on the planner is a cancellation, not a verdict. Reporting "created no
/// tickets" here would blame the planner for work the user just stopped, and
/// would train them to ignore the message that matters.
#[tokio::test]
async fn an_aborted_planner_is_cancelled_not_reported_as_an_empty_plan() {
    let fakes = Fakes::new("planner-abort", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    start_planner(&mut s, "plan something").await;

    s.abort().unwrap();

    let msgs = drain_until_parked(&mut rx).await;
    assert!(msgs.iter().any(|m| m.contains("cancel")), "{msgs:?}");
    assert!(
        !msgs.iter().any(|m| m.contains("created no tickets")),
        "an interrupted planner is not a failed plan: {msgs:?}"
    );
    assert!(
        !msgs.iter().any(|m| m.contains("cannot verify")),
        "and not an unverifiable one either: {msgs:?}"
    );
    assert_eq!(fakes.pi_spawns(), 1);
    assert!(!msgs.contains(&"step:work".to_string()), "{msgs:?}");
}
