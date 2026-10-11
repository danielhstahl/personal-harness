//! A ticket left `in_progress` by a dead session is finished before anything new
//! is picked up (looprs-hjo).
//!
//! The bug in one sentence: `bd ready` excludes `in_progress` work, so a ticket
//! whose session crashed is not merely unclaimed, it is **invisible**, and the next
//! session took the top of the ready list and left that one on the board forever.
//! Each test pins one thing that has to be true instead: the unfinished ticket is
//! resumed before the untouched one; a ticket another worker still holds is left
//! alone; the re-work guard answers here too; an unreadable board is not "nothing to
//! resume"; and a resumed pass still has to close its ticket.

use super::*;

/// **The bug, and its fix.** A restart onto a board with a ticket still mid-flight
/// works *that* ticket — the one `bd ready` cannot show, and the one that is not even
/// first in the list — and says so on the transcript before spending anything.
#[tokio::test]
async fn a_ticket_left_in_progress_is_resumed_before_new_work() {
    let fakes = Fakes::new(
        "hjo-resume",
        PiFake::Started,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");

    assert!(
        fakes.claimed("looprs-mid"),
        "the abandoned ticket is the one worked: {:?}",
        fakes.bd_log()
    );
    assert!(
        !fakes.claimed("looprs-99"),
        "fresh work must not jump the queue in front of unfinished work: {:?}",
        fakes.bd_log()
    );
    assert_eq!(fakes.pi_spawns(), 1, "one worker, for the resumed ticket");
    let prompt = fakes.pi_prompts().pop().expect("and it was prompted");
    assert!(prompt.contains("looprs-mid"), "{prompt}");
    assert!(
        prompt.contains("in_progress"),
        "the worker is told the ticket was already in progress: {prompt}"
    );
    assert!(
        prompt.contains("do not start over"),
        "and told to continue rather than begin again: {prompt}"
    );

    let msgs = drain(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.contains("resuming") && m.contains("looprs-mid")),
        "the resume is said out loud, with the ticket named: {msgs:?}"
    );
    assert!(msgs.contains(&"active:looprs-mid".to_string()), "{msgs:?}");
    assert!(msgs.contains(&"step:work".to_string()), "{msgs:?}");
}

/// **A ticket `bd` will not release is another worker's, not this loop's.**
///
/// The failure this pins is the worse one: a live claim held by a human or a second
/// loop, read as "abandoned". The claim is the arbiter — `bd` refuses it — so the
/// loop leaves the ticket alone, *says* it did, and takes fresh work instead of
/// stopping.
#[tokio::test]
async fn a_ticket_another_worker_holds_is_left_alone_and_named() {
    let fakes = Fakes::new(
        "hjo-held",
        PiFake::Started,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    fakes.refuse_claim_for("looprs-mid");
    let (mut l, mut rx, _ctl) = loop_with(&fakes);

    timeout(NO_HANG, l.next()).await.unwrap();

    assert!(
        fakes.claimed("looprs-99"),
        "fresh work is still taken after the refusal: {:?}",
        fakes.bd_log()
    );
    let msgs = drain(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.starts_with("system:") && m.contains("looprs-mid")),
        "the skip is on the record with the ticket named: {msgs:?}"
    );
    assert!(
        !has_error(&msgs),
        "somebody else holding a ticket is not a failure of this loop: {msgs:?}"
    );
    assert_eq!(fakes.pi_spawns(), 1, "one worker, on the fresh ticket");
    assert_eq!(
        l.claim().map(|c| c.id.as_str()),
        Some("looprs-99"),
        "and the loop holds the ticket it actually claimed"
    );
    assert!(
        !l.worked.contains("looprs-mid"),
        "a ticket whose claim was refused is not one this loop worked, so the guard \
         must not start treating it as spent"
    );
}

/// **The guard answers at this boundary too.** An `Esc`, a worker that died, or a
/// settle that closed nothing all leave the ticket `in_progress` *and* already paid
/// for, and a Tab back into the mode is not an acknowledgement (looprs-w7q). The
/// resume path has to refuse it exactly as the ready path refuses it.
#[tokio::test]
async fn a_ticket_this_loop_already_paid_for_is_not_resumed() {
    let fakes = Fakes::new(
        "hjo-guarded",
        PiFake::Started,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    let (mut l, mut rx, _ctl) = loop_with(&fakes);
    l.worked.insert("looprs-mid".to_string());

    timeout(NO_HANG, l.next()).await.unwrap();

    let err = last_error(&drain(&mut rx)).expect("the refusal is an error, not a silence");
    assert!(err.contains("looprs-mid"), "{err}");
    assert!(err.contains("not closed"), "{err}");
    assert!(
        err.contains("bd close looprs-mid"),
        "a stop that does not say what would unblock it just moves the confusion: {err}"
    );
    assert_eq!(fakes.pi_spawns(), 0, "a refused ticket buys no worker");
    assert!(
        !fakes.claimed("looprs-mid"),
        "and does not even take the claim: {:?}",
        fakes.bd_log()
    );
    assert!(
        !fakes.claimed("looprs-99"),
        "the refusal stops the pass rather than falling through to the next ticket: {:?}",
        fakes.bd_log()
    );
    assert!(l.is_awaiting_input(), "and the box comes back");
}

/// **A board nobody can read is not a board with nothing to resume.** looprs-037's
/// rule, applied to the new read: the resume check runs first, so a `bd` that died
/// between passes has to surface as the failure it is, not as "board empty".
#[tokio::test]
async fn an_unreadable_board_is_not_read_as_nothing_to_resume() {
    let fakes = Fakes::new("hjo-unreadable", PiFake::Started, BdFake::Ok, EMPTY_BOARD);
    let (mut l, mut rx, _ctl) = loop_with(&fakes);
    fakes.fail_bd(true);

    timeout(NO_HANG, l.next()).await.unwrap();

    let err = last_error(&drain(&mut rx)).expect("an unreadable board is an error");
    assert!(
        err.contains("in progress"),
        "and it names the read that failed, not just that something failed: {err}"
    );
    assert_eq!(fakes.pi_spawns(), 0, "no worker on a board nobody can read");
    assert!(l.is_awaiting_input());
}

/// **A resumed pass is an ordinary pass.** Same claim, same guard, same
/// verification — and so the same stop when the ticket is still not closed. Resuming
/// must not become a second way to run the loop into a runaway.
#[tokio::test]
async fn a_resumed_ticket_still_has_to_close() {
    let fakes = Fakes::new(
        "hjo-still-open",
        PiFake::Chat,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    // The resumed worker talks, settles, and leaves the ticket exactly as
    // in-progress as it found it.
    show_status(&fakes, "looprs-mid", "in_progress");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the resumed pass started");
    assert!(fakes.claimed("looprs-mid"));
    drain(&mut rx);

    fakes.settle();
    let msgs = drain_until_parked(&mut rx).await;
    let err = last_error(&msgs).expect("a ticket that never closes is an error");
    assert!(err.contains("looprs-mid"), "{err}");
    assert!(err.contains("not closed"), "{err}");
    assert_eq!(
        fakes.pi_spawns(),
        1,
        "and the loop stopped rather than buying the ticket a second time: {msgs:?}"
    );
    assert!(
        fakes.notifier.completions().is_empty(),
        "a resumed ticket that did not close is not a completion: {:?}",
        fakes.notifier.completions()
    );
}

/// And when the resumed worker *does* close it, the loop goes on to the fresh work
/// with nothing left claimed — the resume is a pass, not a detention.
#[tokio::test]
async fn a_resumed_ticket_that_closes_hands_the_board_on() {
    let fakes = Fakes::new(
        "hjo-closed",
        PiFake::Chat,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    show_closed(&fakes, "looprs-mid");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await);
    drain(&mut rx);

    // The resumed worker finishes the job: the ticket closes and the board moves on
    // under the loop, which is the only way the next pass can tell.
    fakes.set_board(SECOND_BOARD);
    fakes.settle();
    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    assert!(!has_error(&msgs), "a closed ticket is not a stop: {msgs:?}");
    assert_eq!(
        fakes.notifier.completed_ids(),
        vec!["looprs-mid".to_string()],
        "and the finished ticket is announced like any other"
    );
    assert_eq!(
        fakes.pi_spawns(),
        2,
        "and the loop moved on to the next ticket"
    );
    assert!(fakes.claimed("looprs-99"), "{:?}", fakes.bd_log());
}
