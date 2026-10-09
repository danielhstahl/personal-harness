//! //! The out-of-band announcement (`services::notification`).
//!
//! Exactly one `notify` call exists in the binary, in the `PassOutcome::Closed`
//! arm of the task's settle handler. Five of the six rows below are the
//! interesting ones: every one of them ends a pass, and a notifier hung off
//! `agent_settled` — the obvious place, and the one this was first pointed at —
//! would have announced all of them.

use super::*;

// =========================================================================
// The out-of-band announcement (`services::notification`).
//
// Exactly one `notify` call exists in the binary, in the `PassOutcome::Closed`
// arm of `BeadsTask::worker_settled`. The five tests below are the table that
// claim is worth, and five of the six rows are the interesting ones: every one
// of them ends a pass, and a notifier hung off `agent_settled` — the obvious
// place, and the one this was first pointed at — would have announced all of
// them. `RecordingNotifier` (see `crate::testing`) is what makes "nothing was
// announced" an assertion rather than a hope.
// =========================================================================

/// **A closed ticket is announced, once, and with the title the harness claimed.**
///
/// The title comes from the claim rather than from the post-settle `bd show`, so
/// the notification names the ticket the loop paid for even though the two reads
/// are different calls — and a test that asserted the `show` title would not
/// notice the difference. `show_status` deliberately reports a *different*
/// title, so this assertion can tell them apart.
#[tokio::test]
async fn a_closed_ticket_is_announced_once_with_the_title_it_was_claimed_under() {
    let fakes = Fakes::new(
        "notify-closed",
        PiFake::Chat,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    // Working is not finished: a claimed, prompted, running ticket says nothing
    // out of band yet.
    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");
    assert!(
        fakes.notifier.is_empty(),
        "a ticket merely claimed is not a ticket done: {:?}",
        fakes.notifier.completions()
    );
    drain(&mut rx);

    // The worker does the job: the board says closed, and the worker settles.
    show_closed(&fakes, "looprs-26r");
    fakes.set_board(SECOND_BOARD);
    fakes.settle();
    fakes.wait_for_pi_spawns(2).await;
    // The FIFO seam: the announcement was made inside the settle handler, so an
    // ack taken after it can only arrive with the recording already written.
    assert!(s.quiesce().await, "the announcement is behind this ack");

    assert_eq!(
        fakes.notifier.completed_ids(),
        vec!["looprs-26r".to_string()],
        "one ticket closed, one announcement, and it names that ticket: {:?}",
        fakes.notifier.completions()
    );
    let done = fakes.notifier.completions();
    assert_eq!(
        done[0].title, "Beads loop never self-starts",
        "the claimed title, not the `bd show` one: {done:?}"
    );
}

/// The pass the loop stops itself over — talked, settled, closed nothing — is
/// the one a notifier would most confidently get wrong, because the transcript
/// says "stopped" and the settle looks like any other.
#[tokio::test]
async fn a_ticket_that_was_never_closed_announces_nothing() {
    let fakes = Fakes::new(
        "notify-never-closed",
        PiFake::Chat,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    show_status(&fakes, "looprs-26r", "open");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");
    fakes.settle();
    let msgs = drain_until_parked(&mut rx).await;
    assert!(
        has_error(&msgs),
        "the un-closed ticket was reported to the screen: {msgs:?}"
    );
    assert!(
        fakes.notifier.completions().is_empty(),
        "an un-closed ticket must not leave this terminal: {:?}",
        fakes.notifier.completions()
    );
}

/// ADR-0002 Q3, at the notification layer: the settle that follows `Esc` is
/// the abort unwinding, not a pass that finished. Byte-for-byte identical on
/// the wire, so the only thing that keeps this honest is that the announcement
/// is made from the verdict rather than from the event.
#[tokio::test]
async fn a_pass_the_user_cancelled_announces_nothing() {
    let fakes = Fakes::new("notify-abort", PiFake::Chat, BdFake::Ok, ONE_BEADED_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "a pass is running");
    s.abort().unwrap();
    let msgs = drain_until_parked(&mut rx).await;
    assert!(
        msgs.iter().any(|m| m.contains("cancel")),
        "the cancel was said out loud, in the transcript: {msgs:?}"
    );
    assert!(
        fakes.notifier.completions().is_empty(),
        "cancelling is not completing: {:?}",
        fakes.notifier.completions()
    );
}

/// The planner's settle is the settle most likely to be mistaken for a
/// completion — it verified something, it announced a plan, the loop moved —
/// and it is the one that has closed the fewest tickets.
#[tokio::test]
async fn a_planner_settling_announces_nothing_even_though_its_plan_worked() {
    let fakes = Fakes::new("notify-planner", PiFake::Chat, BdFake::Ok, EMPTY_BOARD);
    let (mut s, mut rx) = beads(&fakes, 1);
    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    drain(&mut rx);

    start_planner(&mut s, "two tickets, please").await;
    fakes.set_board(PLAN_TWO_TICKETS);
    fakes.settle();
    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    assert!(
        msgs.iter().any(|m| m.contains("planner created 2")),
        "the plan worked, and was listed: {msgs:?}"
    );
    assert_eq!(fakes.pi_spawns(), 2, "planner, then a worker started");
    assert!(
        fakes.notifier.completions().is_empty(),
        "a plan is not a finished ticket: {:?}",
        fakes.notifier.completions()
    );
}

/// `LeftForHuman` is the deliberate non-trigger the module doc names. A
/// blocked ticket is unfinished work in somebody else's hands, and an
/// all-clear about it would be the worst-shaped notification this harness can
/// send: green, and wrong.
#[tokio::test]
async fn a_ticket_its_worker_handed_to_a_human_announces_nothing() {
    let fakes = Fakes::new(
        "notify-left-behind",
        PiFake::Chat,
        BdFake::ShowStatus,
        TWO_OPEN,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");
    assert!(fakes.claimed("looprs-26r"));
    drain(&mut rx);

    show_status(&fakes, "looprs-26r", "blocked");
    fakes.set_board(BLOCKED_THEN_READY);
    fakes.settle();
    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    assert!(
        msgs.iter()
            .any(|m| m.contains("blocked") && m.contains("looprs-26r")),
        "the hand-off is on the record: {msgs:?}"
    );
    assert_eq!(fakes.pi_spawns(), 2, "the loop moved on to the next ticket");
    assert!(
        fakes.notifier.completions().is_empty(),
        "nothing was finished here: {:?}",
        fakes.notifier.completions()
    );
}

/// The claim is published when it is taken and when it is released, so the
/// status row (looprs-guh) can name the active ticket without asking `bd` —
/// and cannot keep naming one after the loop let go of it.
#[tokio::test]
async fn the_active_ticket_is_published_when_taken_and_when_released() {
    let fakes = Fakes::new(
        "w7q-active-bead",
        PiFake::Started,
        BdFake::ShowStatus,
        ONE_BEADED_BOARD,
    );
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    assert!(
        drain(&mut rx).is_empty(),
        "a loop that has not claimed says nothing about a ticket"
    );

    timeout(NO_HANG, l.next()).await.unwrap();
    assert!(
        drain(&mut rx).contains(&"active:looprs-26r".to_string()),
        "the claim is published as it is taken"
    );

    l.close().await;
    assert!(
        drain(&mut rx).contains(&"active:-".to_string()),
        "and released with the pass"
    );
    assert!(l.claim().is_none());
}
