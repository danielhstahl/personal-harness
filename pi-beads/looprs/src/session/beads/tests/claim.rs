//! //! The harness claims, and cannot re-work an un-closed bead (looprs-w7q).
//!
//! `bd ready` hands back any ticket still open, so a self-advancing loop that
//! takes whatever it is handed takes the same one forever — billing every pass.
//! Each test pins one of the four things that have to be true instead: the
//! harness claims before the spawn, the board is asked whether the work
//! landed, a ticket that needs a human is not worked, and nothing restarts
//! without a human.

use crate::services::bd::BeadStatus;

use crate::session::beads::machine::Pick;

use super::*;

// --------- the harness claims, and cannot re-work an un-closed bead (looprs-w7q) ---------
//
// The bug this section exists for: `bd ready` hands back any ticket that is
// still open, and a self-advancing loop that takes whatever it is handed will
// take the same one forever. Every test here pins one of the four things that
// have to be true instead — the harness claims, the board is asked whether the
// work landed, a ticket that needs a human is not worked, and nothing restarts
// without a human.

/// **The harness claims, before it pays.** `bd update <id> --claim` comes from
/// the loop rather than from the agent, so "which ticket is this run about" has
/// an answer that does not depend on the agent saying so.
#[tokio::test]
async fn the_harness_claims_the_ticket_it_is_paying_for() {
    let fakes = Fakes::new(
        "w7q-claims",
        PiFake::Started,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut l, _rx, _ctl) = loop_with(&fakes);

    timeout(NO_HANG, l.next()).await.unwrap();

    assert!(
        fakes.claimed("looprs-26r"),
        "the harness must run `bd update <id> --claim` itself: {:?}",
        fakes.bd_log()
    );
    assert_eq!(
        fakes.bd_call_count("update looprs-26r --claim"),
        1,
        "one claim per pass, not one per question about the pass: {:?}",
        fakes.bd_log()
    );
    assert_eq!(l.claim().map(|c| c.id.as_str()), Some("looprs-26r"));
    assert!(l.worked.contains("looprs-26r"), "claimed == on the hook");
}

/// **The ordering proof, run as a consequence.** If the claim came after the
/// spawn, a refused claim would still have bought a `pi` session — aimed at a
/// bead the harness does not hold, answering to whoever prompted it. Claiming
/// first makes a refusal cost one CLI call.
#[tokio::test]
async fn a_claim_bd_refuses_buys_no_worker() {
    let fakes = Fakes::new(
        "w7q-claim-refused",
        PiFake::Started,
        BdFake::ShowStatus,
        TWO_OPEN,
    );
    fakes.refuse_claim(true);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    timeout(NO_HANG, l.next()).await.unwrap();

    assert_eq!(
        fakes.pi_spawns(),
        0,
        "a ticket we could not claim must never have a worker pointed at it"
    );
    let err = last_error(&drain(&mut rx)).expect("a refused claim is reported");
    assert!(err.contains("looprs-26r"), "{err}");
    assert!(err.contains("claim"), "{err}");
    assert!(l.claim().is_none(), "nothing is held on a failed claim");
    assert!(l.is_awaiting_input(), "and the box comes back");
}

/// **The acceptance case: a fake `bd` that never closes the ticket proves the
/// loop stops instead of spinning.** This is the runaway the ticket was filed
/// for — the worker settles, `bd ready` returns the same open bead, another
/// worker is bought, and nothing anywhere counts. One pass is spent, the loop
/// says which ticket and why it stopped, and further nudges buy nothing.
#[tokio::test]
async fn a_ticket_that_is_never_closed_stops_the_loop_instead_of_spinning() {
    let fakes = Fakes::new(
        "w7q-never-closes",
        PiFake::Chat,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    // The worker talks, settles, and closes nothing — forever.
    show_status(&fakes, "looprs-26r", "open");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");
    assert_eq!(fakes.pi_spawns(), 1, "one worker, claimed and prompted");
    drain(&mut rx);

    fakes.settle();
    let msgs = drain_until_parked(&mut rx).await;
    let err = last_error(&msgs).expect("an un-closed ticket is an error, not a shrug");
    assert!(err.contains("looprs-26r"), "{err}");
    assert!(err.contains("not closed"), "{err}");
    assert!(
        err.contains("bd close looprs-26r"),
        "a stop that does not say what would unblock it just moves the confusion: {err}"
    );
    assert!(!msgs.contains(&"step:work".to_string()), "{msgs:?}");
    assert_eq!(s.status(), SessionStatus::Idle, "waiting on a human");

    // And it *stays* stopped. The board never changed, so every later nudge
    // re-reads the same un-closed ticket and refuses again — three nudges,
    // zero extra workers. Before this ticket the same sequence is unbounded.
    for _ in 0..3 {
        s.set_active(true).unwrap();
        assert!(s.quiesce().await);
    }
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "the guard held: no second pass on the ticket that was never closed"
    );
    assert_eq!(fakes.pi_prompts().len(), 1);
    assert_eq!(s.in_flight(), None);
}

/// The guard releases on a human and only on a human: a new instruction is
/// planned, verified, and the board is workable again — while merely looking at
/// the mode again (the loop above) deliberately is not.
#[tokio::test]
async fn a_new_instruction_is_the_acknowledgement_that_releases_the_guard() {
    let fakes = Fakes::new(
        "w7q-acknowledged",
        PiFake::Chat,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    show_status(&fakes, "looprs-26r", "open");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    fakes.settle();
    drain_until_parked(&mut rx).await;
    assert_eq!(fakes.pi_spawns(), 1, "stopped by the guard");

    // The human acts: a fresh instruction, planned into real tickets.
    start_planner(&mut s, "here is what I actually want").await;
    fakes.set_board(PLAN_TWO_TICKETS);
    show_closed(&fakes, "looprs-101");
    fakes.settle();

    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    assert_eq!(
        fakes.pi_spawns(),
        3,
        "the refused worker, the planner, and one worker after the acknowledgement: {msgs:?}"
    );
    assert!(fakes.claimed("looprs-101"), "{:?}", fakes.bd_log());
}

/// A ticket `bd` reports as blocked is not work, even in a `ready` list: a
/// human (or a worker that gave up) took it out of the loop on purpose, and a
/// metered pass on it finishes nothing. It is skipped **and named**, because a
/// silent skip is indistinguishable from a lost ticket.
#[tokio::test]
async fn a_blocked_ticket_is_skipped_named_and_never_claimed() {
    let fakes = Fakes::new(
        "w7q-blocked-skip",
        PiFake::Started,
        BdFake::ShowStatus,
        BLOCKED_THEN_READY,
    );
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    timeout(NO_HANG, l.next()).await.unwrap();

    assert!(
        !fakes.claimed("looprs-26r"),
        "a blocked ticket is never claimed: {:?}",
        fakes.bd_log()
    );
    assert!(
        fakes.claimed("looprs-99"),
        "the workable ticket behind it is the one worked: {:?}",
        fakes.bd_log()
    );
    assert!(
        !l.worked.contains("looprs-26r"),
        "skipped is not the same as worked"
    );
    let msgs = drain(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.contains("need a human") && m.contains("looprs-26r")),
        "the skip is said out loud, with the ticket named: {msgs:?}"
    );
}

/// A worker that leaves its ticket `blocked` has handed it to a human, which is
/// the worker doing the right thing loudly — not the loop failing. So the loop
/// reports the hand-off and keeps going with the rest of the board rather than
/// parking on a ticket that was never its own to finish.
#[tokio::test]
async fn a_ticket_its_worker_left_blocked_does_not_stop_the_loop() {
    let fakes = Fakes::new(
        "w7q-left-blocked",
        PiFake::Chat,
        BdFake::ShowStatus,
        TWO_OPEN,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    assert_eq!(fakes.pi_spawns(), 1);
    assert!(fakes.claimed("looprs-26r"));
    drain(&mut rx);

    // The worker gives up properly: marks the ticket blocked (which takes it
    // out of `bd ready`), then settles.
    show_status(&fakes, "looprs-26r", "blocked");
    fakes.set_board(BLOCKED_THEN_READY);
    fakes.settle();

    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    assert!(
        !has_error(&msgs),
        "a worker handing a ticket to a human is not an error: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("blocked") && m.contains("looprs-26r")),
        "the hand-off is on the record: {msgs:?}"
    );
    assert_eq!(fakes.pi_spawns(), 2, "the loop moved on to the next ticket");
    assert!(fakes.claimed("looprs-99"), "{:?}", fakes.bd_log());
}

/// A board that cannot answer is neither "closed" nor "left open". The loop
/// says which of the two it could not determine, because the user's next
/// command differs — `bd close` the ticket, or fix `bd`.
#[tokio::test]
async fn an_unreadable_board_after_a_pass_is_unverifiable_not_a_verdict() {
    let fakes = Fakes::new(
        "w7q-unreadable",
        PiFake::Chat,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started while bd still worked");
    fakes.fail_bd(true); // the ticket may well have closed; nobody can read that
    fakes.settle();

    let msgs = drain_until_parked(&mut rx).await;
    let err = last_error(&msgs).expect("an unreadable board is an error");
    assert!(err.contains("looprs-26r"), "{err}");
    assert!(err.contains("cannot verify"), "{err}");
    assert!(
        !err.contains("not closed"),
        "a read failure must not be reported as a verdict about the ticket: {err}"
    );
    assert!(
        fakes.notifier.completions().is_empty(),
        "a board nobody can read is not something to announce: {:?}",
        fakes.notifier.completions()
    );
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "no further pass on a ticket nobody can vouch for"
    );
    assert_eq!(s.status(), SessionStatus::Idle, "the box comes back");
}

/// The pick rules on their own, with no subprocess in the way: work the first
/// workable ticket, skip what a human has taken out of the loop, refuse what
/// this loop already burned a pass on — and keep working what this build cannot
/// classify, so a `bd` upgrade cannot quietly empty the board.
#[tokio::test]
async fn the_pick_rules_work_skip_then_refuse_in_that_order() {
    let fakes = Fakes::new(
        "w7q-pick-rules",
        PiFake::Started,
        BdFake::ShowStatus,
        EMPTY_BOARD,
    );
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(
        matches!(l.pick_bead(&[bead("a", "first"), bead("b", "second")]), Pick::Work(b) if b.id() == "a"),
        "bd's own priority order is kept"
    );

    let mixed = vec![
        bead_status("skip-1", "waiting on a human", BeadStatus::Blocked),
        bead_status("skip-2", "postponed", BeadStatus::Deferred),
        bead("work-me", "actually workable"),
    ];
    assert!(
        matches!(l.pick_bead(&mixed), Pick::Work(b) if b.id() == "work-me"),
        "blocked and deferred are walked past"
    );
    let msgs = drain(&mut rx);
    let said = msgs
        .iter()
        .filter(|m| m.contains("need a human"))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        said.len(),
        1,
        "one note per pass, not one per skipped ticket: {msgs:?}"
    );
    assert!(
        said[0].contains("skip-1") && said[0].contains("skip-2"),
        "{said:?}"
    );

    assert!(
        matches!(
            l.pick_bead(&[bead_status("u", "from the future", BeadStatus::Unknown)]),
            Pick::Work(b) if b.id() == "u"
        ),
        "an unfamiliar status is worked, not skipped"
    );

    l.worked.insert("again".to_string());
    assert!(
        matches!(l.pick_bead(&[bead("again", "been worked")]), Pick::AlreadyWorked(b) if b.id() == "again"),
        "a worked-and-un-closed ticket is refused, not re-run"
    );
}

/// **No work-discovery in the worker's instructions.** The agent is *told*
/// which ticket it owns. Handing it `bd ready` and hoping it picks the bead the
/// loop is reporting on is a race with every other hand on the board, so the
/// prompt names the ticket, says it is already claimed, and does not offer the
/// commands that would let the worker wander somewhere else.
#[tokio::test]
async fn the_worker_is_told_its_ticket_not_invited_to_go_shopping() {
    let fakes = Fakes::new(
        "w7q-prompt",
        PiFake::Started,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut l, _rx, _ctl) = loop_with(&fakes);

    timeout(NO_HANG, l.next()).await.unwrap();
    let prompt = fakes.pi_prompts().pop().expect("the worker was prompted");

    assert!(
        prompt.contains("looprs-26r"),
        "the concrete id is in the prompt: {prompt}"
    );
    assert!(
        prompt.contains("Beads loop never self-starts"),
        "and so is the title, so the worker knows what it is doing: {prompt}"
    );
    assert!(
        prompt.contains("already"),
        "it says the claim is already done: {prompt}"
    );
    assert!(
        !prompt.contains("bd ready"),
        "`bd ready` must not be offered to a worker the harness has assigned: {prompt}"
    );
    assert!(
        !prompt.contains("bd update <id> --claim"),
        "nor a claim instruction — the harness claims, the worker works: {prompt}"
    );
}
