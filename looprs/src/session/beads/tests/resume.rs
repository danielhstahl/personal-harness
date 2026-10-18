//! A ticket left `in_progress` by a dead session is finished before anything new
//! is picked up (looprs-hjo), and the ticket *this* loop just failed to close gets
//! another pass without waiting for a human (workspace-9th).
//!
//! The bug in one sentence: `bd ready` excludes `in_progress` work, so a ticket
//! whose session crashed is not merely unclaimed, it is **invisible**, and the next
//! session took the top of the ready list and left that one on the board forever.
//! Each test pins one thing that has to be true instead: the unfinished ticket is
//! resumed before the untouched one; a ticket another worker still holds is left
//! alone; the re-work guard answers here too — including the bounded exception it
//! makes for an unexpected agent complete, and the `Esc` it does *not* make an
//! exception for; an unreadable board is not "nothing to resume"; and a resumed pass
//! still has to close its ticket.

use super::*;
use crate::session::beads::guards::MAX_AUTO_RESUME;

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

/// **The guard answers at this boundary too.** A ticket left `in_progress` *and*
/// already paid for is not resumed by a loop that simply came back: an `Esc` leaves
/// it in exactly this state and earned no re-buy, and so does a pass whose automatic
/// re-buys have all been spent (looprs-w7q, bounded by workspace-9th). Neither a Tab
/// back into the mode nor the passing of time is an acknowledgement, so the resume
/// path refuses it exactly as the ready path refuses it.
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
/// verification — and so the same stop when the ticket is never closed. Resuming must
/// not become a second way to run the loop into a runaway, which is what the
/// automatic re-buy would otherwise be: the loop spends
/// `MAX_AUTO_RESUME` passes on it and then the guard answers.
#[tokio::test]
async fn a_resumed_ticket_still_has_to_close() {
    let fakes = Fakes::new(
        "hjo-still-open",
        PiFake::Chat,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    // The resumed worker talks, settles, and leaves the ticket exactly as
    // in-progress as it found it. Forever.
    show_status(&fakes, "looprs-mid", "in_progress");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the resumed pass started");
    assert!(fakes.claimed("looprs-mid"));
    drain(&mut rx);

    // Every automatic re-buy gets spent, and each one ends the same way the last
    // did. This is the loop refusing to become a bill: the passes are counted, and
    // the count is the guard's, not the agent's.
    let msgs = settle_until_parked(&mut s, &fakes, &mut rx).await;
    assert_eq!(
        fakes.pi_spawns(),
        1 + MAX_AUTO_RESUME as usize,
        "the original pass plus exactly its automatic re-buys, and not one more"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("automatic re-buy 1 of") && m.contains("looprs-mid")),
        "the first re-buy said its own number: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains(&format!("automatic re-buy {MAX_AUTO_RESUME} of"))),
        "the last one said its number too: {msgs:?}"
    );
    assert!(
        !msgs
            .iter()
            .any(|m| m.contains(&format!("automatic re-buy {} of", MAX_AUTO_RESUME + 1))),
        "and there was never a re-buy past the budget: {msgs:?}"
    );
    let err = last_error(&msgs).expect("a ticket that never closes is an error");
    assert!(err.contains("looprs-mid"), "{err}");
    assert!(err.contains("not closed"), "{err}");
    assert!(
        err.contains("bd close looprs-mid"),
        "and the stop names its own way out: {err}"
    );
    assert!(
        fakes.notifier.completions().is_empty(),
        "a resumed ticket that did not close is not a completion: {:?}",
        fakes.notifier.completions()
    );
}

/// **The unexpected agent complete, and what it must not cost a human.** The worker
/// stopped talking with its ticket still `in_progress` and nobody pressed `Esc`, so
/// the loop does not park: it goes on to the next part of itself and the same ticket
/// gets another pass (workspace-9th).
///
/// The half this pins rather than "it keeps going" is *which* ticket gets the pass.
/// The re-buy is for the unfinished ticket and never for fresh work: `bd ready` is
/// still offering `looprs-99`, and taking it would abandon the ticket the loop
/// already paid for — looprs-hjo's bug, arriving by the other door.
#[tokio::test]
async fn a_pass_that_did_not_close_its_ticket_buys_that_ticket_another_pass() {
    let fakes = Fakes::new(
        "9th-auto-resume",
        PiFake::Chat,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    show_status(&fakes, "looprs-mid", "in_progress");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the first pass started");
    assert_eq!(fakes.pi_spawns(), 1);
    drain(&mut rx);

    // The agent completes. Nothing closed, and no human anywhere near it.
    fakes.settle();
    fakes.wait_for_pi_spawns(2).await;
    s.quiesce().await;

    assert_eq!(
        fakes.pi_spawns(),
        2,
        "the loop carried on into the next pass instead of parking"
    );
    assert!(
        !fakes.claimed("looprs-99"),
        "the re-buy is for the unfinished ticket, never for fresh work: {:?}",
        fakes.bd_log()
    );
    assert_eq!(
        fakes.bd_call_count("update looprs-mid --claim --force"),
        2,
        "and the second pass claims it like any other pass does: {:?}",
        fakes.bd_log()
    );
    let prompt = fakes.pi_prompts().pop().expect("and it was prompted");
    assert!(prompt.contains("looprs-mid"), "{prompt}");
    assert!(
        prompt.contains("do not start over"),
        "the second pass is told work may already exist: {prompt}"
    );

    let msgs = drain(&mut rx);
    assert!(
        msgs.iter()
            .any(|m| m.contains("automatic re-buy 1 of") && m.contains("looprs-mid")),
        "the re-buy is said out loud, with its number: {msgs:?}"
    );
    assert!(
        !has_error(&msgs),
        "a repair the loop chose is not a failure: {msgs:?}"
    );
    assert_eq!(
        s.status(),
        SessionStatus::Running,
        "and the agent is still running"
    );
}

/// **`Esc` is not an unexpected complete.** A human stopped that pass, which is a
/// decision about spending, so it earns no re-buy: when the loop comes back (a `Tab`
/// out and in is enough — no instruction typed) the ticket it cancelled is refused
/// with the guard's own two ways out (workspace-9th).
///
/// This is the half that a wholesale removal of the resume guard would have taken
/// with it: the same `in_progress` state on the board, read as the same permission to
/// spend again.
#[tokio::test]
async fn a_ticket_a_human_cancelled_is_not_resumed_when_the_loop_comes_back() {
    let fakes = Fakes::new(
        "9th-esc-holds",
        PiFake::Chat,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    show_status(&fakes, "looprs-mid", "in_progress");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(
        s.quiesce().await,
        "the pass on the in-progress ticket started"
    );
    assert!(fakes.claimed("looprs-mid"));

    s.abort().unwrap();
    drain_until_parked(&mut rx).await;
    assert_eq!(fakes.pi_spawns(), 1, "the cancel parked the loop");

    // Coming back is not an instruction: the loop looks for work again and finds its
    // own cancelled ticket still sitting `in_progress`.
    s.set_active(true).unwrap();
    let msgs = drain_until_parked(&mut rx).await;

    assert_eq!(
        fakes.pi_spawns(),
        1,
        "the pass a human stopped was not bought again: {msgs:?}"
    );
    assert!(
        !fakes.claimed("looprs-99"),
        "and the refusal stops the pass rather than falling through to the next ticket: {:?}",
        fakes.bd_log()
    );
    let err = last_error(&msgs).expect("the guard says something, not nothing");
    assert!(err.contains("looprs-mid"), "{err}");
    assert!(
        err.contains("bd close looprs-mid"),
        "and names the way out: {err}"
    );
    assert_eq!(s.status(), SessionStatus::Idle, "back to the human");
}

/// **A re-buy is for one ticket, not for the next one that happens to be lying
/// around.** The exception names a ticket on purpose: a credit earned by `looprs-mid`
/// must not be spendable on some other ticket this loop also worked, because an
/// exception that transferable is the guard deleted. Driven through the board rather
/// than the pure guard so the claim order is the real one.
#[tokio::test]
async fn a_re_buy_earned_by_one_ticket_does_not_buy_another_one() {
    let fakes = Fakes::new(
        "9th-not-transferable",
        PiFake::Chat,
        BdFake::ShowStatus,
        CRASH_LEFT_ONE_IN_PROGRESS,
    );
    let (mut l, _rx, _ctl) = loop_with(&fakes);
    // `looprs-mid` is the ticket whose pass just failed, and it earned the re-buy;
    // `looprs-other` is unrelated work this loop also happens to have paid for.
    l.owe_auto_resume("looprs-mid");
    l.worked.insert("looprs-other".to_string());

    assert!(
        !l.spend_auto_resume("looprs-other"),
        "another worked ticket cannot spend this credit"
    );
    assert!(
        l.spend_auto_resume("looprs-mid"),
        "the ticket that earned it can"
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
