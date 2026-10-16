//! An epic is worked after its children, never before them (looprs-lpw).
//!
//! The bug in one sentence: a planner that turns "build a whole subsystem" into an
//! epic writes the **container** first, usually at the sharpest priority, and `bd
//! ready` — which knows about blockers, not about containment — hands that epic
//! back as the top of the ready list. The loop took it, spent a metered pass reading
//! a paragraph of intent, and left the children underneath untouched. Every pass
//! after that was the same argument again.
//!
//! What is pinned instead: a bead named as the parent of other ready work goes to
//! the back of the queue, out loud; a hierarchy resolves deepest-first; a parent
//! comes back the moment nothing is left under it; and a parent whose children are
//! all *human* tickets is not held down by them, because there is nothing beneath it
//! this loop may do.
//!
//! The one thing this rule deliberately cannot see is a parent whose children are not
//! in the ready list at all — closed, `in_progress` elsewhere, or never written. Such
//! a parent is workable, which is the same conservative reading `pick_bead` gives
//! an unknown status: the loop works what `bd` offered rather than inventing a
//! blocker it cannot check.

use super::*;
use crate::services::bd::BeadStatus;
use crate::session::beads::machine::Pick;

/// **The bug, and its fix.** The board is a planner's epic plus two children, in the
/// order real `bd ready` returns them — epic on top. The pass goes to the first
/// child, not to the epic, and the transcript says why before anything is billed.
#[tokio::test]
async fn an_epic_is_worked_after_its_children() {
    let fakes = Fakes::new(
        "lpw-epic-last",
        PiFake::Started,
        BdFake::ShowStatus,
        EPIC_AND_CHILDREN,
    );
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the pass started");

    assert!(
        fakes.claimed("looprs-epic.1"),
        "the first child is the work: {:?}",
        fakes.bd_log()
    );
    assert!(
        !fakes.claimed("looprs-epic"),
        "the container is not, and is not claimed either: {:?}",
        fakes.bd_log()
    );
    assert_eq!(fakes.pi_spawns(), 1, "one worker, for the child");
    let prompt = fakes.pi_prompts().pop().expect("and it was prompted");
    assert!(
        prompt.contains("Your assigned ticket: looprs-epic.1"),
        "the worker is told the child's ticket: {prompt}"
    );

    let msgs = drain(&mut rx);
    assert!(
        msgs.contains(&"active:looprs-epic.1".to_string()),
        "and the loop holds the child, not the epic: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .any(|m| m.contains("looprs-epic") && m.contains("child")),
        "the pass-over-the-epic is said out loud, with the epic named: {msgs:?}"
    );
    assert!(
        !has_error(&msgs),
        "working a child instead of its epic is the rule working, not a failure: {msgs:?}"
    );
}

/// **The epic is deferred, not benched.** Once its children are closed there is
/// nothing left under it, and the container is the only thing on the board that still
/// needs a hand — the pass that decides whether the epic is actually finished and
/// closes it. A rule that made parents unworkable would leave every epic open forever
/// and the loop would park on an empty board.
#[tokio::test]
async fn a_parent_with_nothing_left_under_it_is_worked() {
    let fakes = Fakes::new(
        "lpw-epic-returns",
        PiFake::Chat,
        BdFake::ShowStatus,
        EPIC_AND_CHILDREN,
    );
    show_closed(&fakes, "looprs-epic.1");
    let (mut s, mut rx) = beads(&fakes, 1);

    s.set_active(true).unwrap();
    assert!(s.quiesce().await, "the child's pass started");
    drain(&mut rx);

    // The child ships, and the board that is left is the epic on its own.
    fakes.set_board(EPIC_ALONE);
    fakes.settle();
    let msgs = drain_until(&mut rx, |m| m == "step:work").await;
    assert!(!has_error(&msgs), "a closed child is not a stop: {msgs:?}");
    assert!(
        fakes.claimed("looprs-epic"),
        "with nothing under it the epic is work again: {:?}",
        fakes.bd_log()
    );
    assert_eq!(
        fakes.notifier.completed_ids(),
        vec!["looprs-epic.1".to_string()],
        "and the child that closed is announced like any other ticket"
    );
}

/// **The rule itself, with no subprocess in the way.** Three rules decide a pick and
/// this is the newest one: a bead that some other workable bead in the same list
/// names as its parent goes to the back, and the deferral is said out loud — a loop
/// that works `epic.1` when `epic` was sitting on top of the list looks, from the
/// outside, exactly like a loop that picked the wrong ticket.
#[tokio::test]
async fn a_parent_is_deferred_while_any_of_its_children_is_workable() {
    let fakes = Fakes::new(
        "lpw-pure-defer",
        PiFake::Started,
        BdFake::ShowStatus,
        EMPTY_BOARD,
    );
    let (l, mut rx, _ctl) = loop_with(&fakes);

    let board = vec![
        bead("looprs-epic", "Epic: build the thing"),
        bead_child("looprs-epic.1", "the first piece", "looprs-epic"),
        bead_child("looprs-epic.2", "the second piece", "looprs-epic"),
    ];
    let pick = l.pick_bead(&board);
    assert!(
        matches!(pick, Pick::Work(ref b) if b.id() == "looprs-epic.1"),
        "the first child is worked, not the epic on top: {pick:?}"
    );
    let said = drain(&mut rx);
    let note = said
        .iter()
        .find(|m| m.contains("child"))
        .expect("the deferral is said out loud");
    assert!(note.contains("looprs-epic"), "{note}");
    assert!(
        note.contains("looprs-epic.1"),
        "and names what it took: {note}"
    );
    assert!(
        note.contains("2 child"),
        "with how many children are holding the epic down: {note}"
    );
}

/// **A hierarchy resolves deepest-first, for free.** An epic, a child under it and a
/// subtask under *that*: every parent defers to whatever hangs below it, so the leaf
/// is worked, then the middle, then the epic — three passes, each one taking the only
/// thing left that is actually work. No sorting function needed, because the deferral
/// is re-derived from the list on every pass.
#[tokio::test]
async fn a_deep_hierarchy_is_worked_from_the_bottom_up() {
    let fakes = Fakes::new("lpw-deep", PiFake::Started, BdFake::ShowStatus, EMPTY_BOARD);
    let (l, _rx, _ctl) = loop_with(&fakes);

    let epic = bead("looprs-epic", "Epic: build the thing");
    let mid = bead_child("looprs-epic.1", "a piece", "looprs-epic");
    let leaf = bead_child("looprs-epic.1.1", "the actual code", "looprs-epic.1");

    // All three listed: the leaf is the work.
    let all = vec![epic.clone(), mid.clone(), leaf.clone()];
    assert!(
        matches!(l.pick_bead(&all), Pick::Work(b) if b.id() == "looprs-epic.1.1"),
        "the deepest bead is the only one that is not somebody's container"
    );

    // The leaf closes: the middle is now a leaf, and it goes next — still not the epic.
    let mid_left = vec![epic.clone(), mid.clone()];
    assert!(
        matches!(l.pick_bead(&mid_left), Pick::Work(b) if b.id() == "looprs-epic.1"),
        "a parent that lost its own children becomes the work"
    );

    // And when the middle closes too, the epic is the board.
    let epic_only = vec![epic.clone()];
    assert!(
        matches!(l.pick_bead(&epic_only), Pick::Work(b) if b.id() == "looprs-epic"),
        "last, and only last, the container"
    );
}

/// **A human's ticket does not hold its parent down.** A child `blocked` or
/// `deferred` is work this loop may not do, so it must not act as an unfinished
/// child: an epic whose only listed children are a human's is exactly as workable as
/// an epic with no children, and parking the loop over it would be a stall invented
/// out of nothing.
#[tokio::test]
async fn a_child_a_human_took_out_of_the_loop_does_not_hold_its_parent_down() {
    let fakes = Fakes::new(
        "lpw-blocked-child",
        PiFake::Started,
        BdFake::ShowStatus,
        EMPTY_BOARD,
    );
    let (l, _rx, _ctl) = loop_with(&fakes);

    for status in [BeadStatus::Blocked, BeadStatus::Deferred] {
        let board = vec![
            bead("looprs-epic", "Epic: build the thing"),
            bead_child_status(
                "looprs-epic.1",
                "waiting on a person",
                "looprs-epic",
                status,
            ),
        ];
        let pick = l.pick_bead(&board);
        assert!(
            matches!(pick, Pick::Work(ref b) if b.id() == "looprs-epic"),
            "a {status} child is not work, so it cannot be what holds its parent back: {pick:?}"
        );
    }
}

/// **The re-work guard still answers underneath a parent.** The deferral rule makes a
/// parent step aside; it does not make the loop forget that it already paid for one of
/// the children. A board whose only remaining work is a ticket this loop burned a pass
/// on is still a refusal, and still a stop: `bd ready` will offer it forever, and
/// every pass is billed (looprs-w7q).
#[tokio::test]
async fn the_re_work_guard_still_answers_under_a_parent() {
    let fakes = Fakes::new(
        "lpw-guard-under-parent",
        PiFake::Started,
        BdFake::ShowStatus,
        EMPTY_BOARD,
    );
    let (mut l, _rx, _ctl) = loop_with(&fakes);
    l.worked.insert("looprs-epic.1".to_string());

    let board = vec![
        bead("looprs-epic", "Epic: build the thing"),
        bead_child(
            "looprs-epic.1",
            "the piece it already paid for",
            "looprs-epic",
        ),
    ];
    assert!(
        matches!(l.pick_bead(&board), Pick::AlreadyWorked(b) if b.id() == "looprs-epic.1"),
        "deferring the epic must not walk past the guard and re-buy a worked ticket"
    );
}

/// **A refusal under a parent explains itself.** The deferral is said when the rule
/// changes the answer, and a refusal changes it as much as a pick does: the ready
/// list had the epic on top, the loop went and parked on `looprs-epic.1` instead, and
/// an operator reading only the refusal has no way to work out that the epic was ever
/// considered. So the deferral is spoken for a refused pick too.
#[tokio::test]
async fn a_refusal_under_a_parent_says_why_the_parent_was_not_taken() {
    let fakes = Fakes::new(
        "lpw-guard-says-why",
        PiFake::Started,
        BdFake::ShowStatus,
        EMPTY_BOARD,
    );
    let (mut l, mut rx, _ctl) = loop_with(&fakes);
    l.worked.insert("looprs-epic.1".to_string());

    let board = vec![
        bead("looprs-epic", "Epic: build the thing"),
        bead_child(
            "looprs-epic.1",
            "the piece it already paid for",
            "looprs-epic",
        ),
    ];
    assert!(
        matches!(l.pick_bead(&board), Pick::AlreadyWorked(_)),
        "the guard still refuses"
    );
    let said = drain(&mut rx);
    assert!(
        said.iter()
            .any(|m| m.contains("looprs-epic") && m.contains("child")),
        "and the pass over the epic is said, or the refusal looks like a mispick: {said:?}"
    );
}

/// **What the rule cannot see, it does not guess.** A parent whose children are not in
/// this list — closed, held `in_progress` by another worker, or never written — is
/// workable, and so is a bead in a status this build cannot name. Inferring hidden
/// children from the shape of an id (`looprs-x.4`) would be a guess about the board,
/// and a guess that silently empties the board is looprs-037's conflation again.
#[tokio::test]
async fn nothing_is_deferred_for_children_the_read_cannot_show() {
    let fakes = Fakes::new(
        "lpw-no-invention",
        PiFake::Started,
        BdFake::ShowStatus,
        EMPTY_BOARD,
    );
    let (l, _rx, _ctl) = loop_with(&fakes);

    // An id that *looks* like a child, and an epic whose children are simply not
    // here. Neither defers anybody.
    let board = vec![
        bead("looprs-epic", "Epic: nothing under it"),
        bead("looprs-other.9", "an id that merely looks like a child"),
    ];
    assert!(
        matches!(l.pick_bead(&board), Pick::Work(b) if b.id() == "looprs-epic"),
        "no listed child, no deferral: bd's order stands"
    );

    // A child in an unfamiliar status is workable (looprs-037), so it does hold its
    // parent down like any other work.
    let future = vec![
        bead("looprs-epic", "Epic: build the thing"),
        bead_child_status(
            "looprs-epic.1",
            "a status from a newer bd",
            "looprs-epic",
            BeadStatus::Unknown,
        ),
    ];
    assert!(
        matches!(l.pick_bead(&future), Pick::Work(b) if b.id() == "looprs-epic.1"),
        "unknown statuses are work, not skips"
    );
}
