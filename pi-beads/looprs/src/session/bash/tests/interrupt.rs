//! //! `Esc`, against a real shell: what gets interrupted, what gets dropped, and
//! //! what the user is told either way.
//! //!
//! //! The queue half and the pty half are different questions — an `Esc` aimed at a
//! //! command that has not run yet takes the command out, one aimed at a running
//! //! command puts `0x03` on the master and says so before the command reports
//! //! itself done. The last test is the one that cannot be faked: a command that
//! //! traps the interrupt and keeps going is reported, not silently wedged.

use crate::session::cancel;

use super::*;

/// **Acceptance: Esc interrupts a long command and the shell survives.**
///
/// Every wait in here is an event: the session's own seam for "the keystroke
/// has been acted on", the command's own exit line for "the interrupt
/// landed". The only clocks are [`NO_HANG`] and [`INTERRUPTED_WITHIN`], and
/// both are failure bounds.
#[tokio::test]
async fn esc_interrupts_a_running_command_without_killing_the_shell() {
    let (mut s, mut rx) = bash(7);
    warm_shell(&mut s, &mut rx).await;
    in_flight(&mut s, "echo star''ted; sleep 30").await;

    let at_esc = std::time::Instant::now();
    s.abort().unwrap();
    // The keystroke is acted on inside the session's own mailbox, and the
    // seam *is* that handling: waiting for it is waiting for the event, not
    // for a clock. `Aborting` is what the state was when it completed.
    assert!(s.quiesce().await, "the Esc was handled");
    assert_eq!(s.status(), SessionStatus::Aborting, "the cancel is live");

    // The interrupt's own exit line is the proof that the sleep was cut short
    // rather than still running: block on the line instead of on a second of
    // wall clock, so a loaded runner costs this test nothing and a lost
    // interrupt cannot hide inside a tolerance.
    let ran = run_logged(&mut rx).await;
    assert!(
        ran.code == Some(130) || ran.code == Some(143) || ran.code == Some(1),
        "sleep should have been interrupted, got {:?} (out {:?})",
        ran.code,
        ran.out
    );
    assert!(
        at_esc.elapsed() < INTERRUPTED_WITHIN,
        "an interrupt exit arrived {:?} after the keystroke, which is `sleep 30` winding \
             down eventually rather than being interrupted by us (failure bound, not a wait)",
        at_esc.elapsed()
    );
    assert_eq!(s.status(), SessionStatus::Idle, "shell is still alive");

    // …and still usable afterwards.
    s.send_text("echo still-here".into()).unwrap();
    let (out2, code2) = run_command(&mut rx).await;
    assert_eq!(code2, Some(0));
    assert!(out2.contains("still-here"), "{out2:?}");
}

/// Esc at an idle prompt is a no-op: it must not interrupt anything, and must
/// not produce a spurious exit line or error.
#[tokio::test]
async fn esc_at_an_idle_prompt_is_silent() {
    let (mut s, mut rx) = bash(8);
    s.send_text("echo first".to_string()).unwrap();
    run_command(&mut rx).await;
    assert_eq!(s.status(), SessionStatus::Idle);
    drain(&mut rx);

    s.abort().unwrap();
    assert!(s.quiesce().await);
    let msgs = drain(&mut rx);
    assert!(
        msgs.iter().all(|m| !m.starts_with("error")),
        "an idle Esc is not an error: {msgs:?}"
    );
    assert!(
        msgs.iter()
            .all(|m| !m.contains("exit ") && !m.contains("interrupted")),
        "an idle Esc reported a command that never ran: {msgs:?}"
    );
}

/// The invisible half of the contract: an idle Esc must not even announce
/// itself. "cancelling…" with nothing cancelled is a claim about work in
/// flight that is not true, and the status row would carry that claim around
/// for the rest of the session.
///
/// Shell *output* is not counted as noise — the prompt keeps arriving on its
/// own and has nothing to do with the keystroke. What must not appear is a
/// word from the session about a command that was never running, including a
/// stall report, which is why the window runs past `cancel::GRACE`.
#[tokio::test]
async fn esc_at_an_idle_prompt_says_nothing_at_all() {
    let (mut s, mut rx) = bash(21);
    s.send_text("echo first".to_string()).unwrap();
    run_command(&mut rx).await;
    assert_eq!(s.status(), SessionStatus::Idle);
    drain(&mut rx);

    s.abort().unwrap();
    assert!(s.quiesce().await, "the Esc was handled");
    let late =
        crate::testing::collect_within(&mut rx, cancel::GRACE + Duration::from_secs(1), |ev| {
            describe(ev)
        })
        .await;
    let noise: Vec<&String> = late.iter().filter(|l| !l.starts_with("out ")).collect();
    assert!(noise.is_empty(), "an idle Esc made noise: {late:?}");
    assert_eq!(s.status(), SessionStatus::Idle, "and changed nothing");
}

/// **The cold-start window: `Esc` while the command has not reached the shell.**
///
/// `submit` never writes to a shell that has not printed its prompt, so a
/// command typed during start-up sits in the session's own `queue` — and
/// `status()` reports that as `Running`, because something *is* pending. The
/// interrupt used to bail out on "nothing outstanding" in exactly that window,
/// which lost the keystroke **and** left the command queued to start as soon
/// as the prompt arrived. A cancel that cancels nothing, silently, while the
/// thing it was aimed at runs a moment later.
///
/// `/bin/cat` is the fixture that holds that state open instead of racing for
/// it: it never prints the readiness marker, so the command never leaves the
/// queue and the window cannot close underneath the test. That is the same
/// window the interrupt tests used to fall into by accident whenever CI was
/// slow enough to widen it — here it is the subject rather than the accident.
#[tokio::test]
async fn an_esc_aimed_at_a_queued_command_takes_it_out_of_the_queue() {
    let cfg = SessionConfig {
        // Not a shell, and on purpose: nothing `cat` prints is the readiness
        // marker, so `ready` stays false and every submit stays queued.
        shell_bin: "/bin/cat".into(),
        ..Default::default()
    };
    let (mut s, mut rx) =
        BashSession::build(SessionId::new(TerminalType::Bash, 26), &cfg).expect("build");
    s.resize(24, 80).ok();

    s.send_text("sleep 30".into()).unwrap();
    // Through the seam rather than by polling: `status` is a mirror the session
    // task writes — at the seam and on the byte lane — and with `cat` as the
    // child no bytes ever arrive to push it. The seam is the one thing that
    // both waits for the `Submit` to have been handled and publishes what the
    // state was when it was.
    assert!(s.quiesce().await, "the submit was handled");
    assert_eq!(
        s.status(),
        SessionStatus::Running,
        "a queued command reads as work pending: the status is *right* here, and it is the \
             keystroke that has to catch up with it"
    );
    drain(&mut rx);

    s.abort().unwrap();
    assert!(s.quiesce().await, "the Esc was handled");

    let msgs = drain(&mut rx);
    assert!(
        msgs.iter().any(|m| m.contains("cancelled")
            && m.contains("sleep 30")
            && m.contains("before it started")),
        "the queued cancel said nothing: {msgs:?}"
    );
    assert_eq!(
        s.status(),
        SessionStatus::Idle,
        "the queue is empty and nothing was ever in flight, so nothing is left busy"
    );
    assert!(
        msgs.iter().all(|m| !m.contains("cancelling")),
        "a queued cancel must not claim a byte was sent at something: {msgs:?}"
    );

    // And it stays cancelled: no late line reporting the command as having run,
    // which is exactly what the swallowed version produced.
    let late = crate::testing::collect_within(&mut rx, Duration::from_millis(500), describe).await;
    assert!(
        late.iter()
            .all(|l| !l.contains("exit ") && !l.contains("cancelling")),
        "the dropped command came back to life: {late:?}"
    );
}

/// **Acceptance: the word comes before the child does.** A `sleep 30` stops
/// fast but not instantly, and the user must not spend the gap wondering
/// whether Esc reached anything. So the contract is an *ordering*: the
/// acknowledgement precedes the exit line.
///
/// **Read the ordering to its end; do not sample it.** This test used to
/// collect whatever arrived in the next 800 ms and assert the acknowledgement
/// was in the sample — a sleep wearing an assertion's clothes. On a loaded
/// runner the sample came up empty and the failure read "the keystroke was not
/// acknowledged" about a session that had acknowledged it late, which is a
/// verdict about the machine (looprs-00u.17). Blocking on the *far* end of
/// the ordering — the interrupted command's exit line — makes both halves
/// exact: the ack is there or the test says so, and nothing about how long
/// anything took changes the answer.
///
/// Why the order is structural rather than probable: one task owns the pty and
/// emits both ends. `send_sigint` writes the `0x03` and then puts the word
/// out, and the reply can only be read on a later turn of that task's loop —
/// so a session that cancels at all says so first. That is what makes this a
/// test of the ordering rather than a race against it.
#[tokio::test]
async fn esc_says_cancelling_before_the_command_reports_itself_done() {
    let (mut s, mut rx) = bash(22);
    warm_shell(&mut s, &mut rx).await;
    in_flight(&mut s, "echo star''ted; sleep 30").await;
    drain(&mut rx);

    s.abort().unwrap();
    // The far end of the ordering. Everything between the keystroke and this
    // line is the evidence, in the order the session put it on the wire.
    let got = until_event(
        &mut rx,
        |l| exit_code_of(l).is_some(),
        "the interrupted command's exit line",
    )
    .await;
    let ack = got
        .iter()
        .position(|l| l.starts_with("system: cancelling") && l.contains("sleep 30"))
        .unwrap_or_else(|| panic!("the keystroke was not acknowledged, by name: {got:?}"));
    let done = got
        .iter()
        .rposition(|l| exit_code_of(l).is_some())
        .expect("until_event stopped on the exit line");
    // "Cancelled" arriving before "cancelling" is the silence this row exists
    // to remove, arriving late instead of never.
    assert!(
        ack < done,
        "the command reported itself done before the cancel was acknowledged: {got:?}"
    );
    // And the exit that followed is the same story as the acknowledgement, not
    // an unrelated line that happened to be shaped like an exit: a cancel
    // acknowledged and then reported as a plain `exit 130` would mean the
    // session had already forgotten it was cancelling.
    assert!(
        got[ack..=done]
            .iter()
            .any(|l| l.contains("interrupted (exit ")),
        "the exit that followed the acknowledgement did not say it was an interrupt: {got:?}"
    );
}

/// **The escalation: a command that will not be interrupted.**
///
/// `trap '' INT` hands the ignore disposition to the child, so the tty's
/// SIGINT arrives, is thrown away, and no exit marker ever comes back. From
/// here that is indistinguishable from a hang, which is exactly why the stall
/// has to be a sentence rather than a spinner — and why the session must not
/// "solve" it by killing the shell the command was running inside of.
///
/// **The child says when it is ready to be uninterruptible.** A trap is not
/// installed when `send_text` returns: a `0x03` that overtakes the builtin
/// kills the sleep like any other, no stall ever happens, and every assertion
/// below is then aimed at a run that never had the property under test. So the
/// builtin is followed by an `echo` of the test's own marker and the test
/// waits for that byte — readiness declared by the child through the pty, not
/// assumed from a delay (looprs-00u.17: this is the ticket's "make the
/// observable deterministic when the observable really is time").
///
/// **"Not before the grace" is a lower bound between two events, not a 500 ms
/// sample.** The stall deadline is armed *after* the keystroke is handled, so
/// a correct implementation cannot report early however loaded the runner is:
/// measure the keystroke-to-report gap and compare it with
/// [`cancel::GRACE`], and the only way to fail the check is to actually cry
/// wolf. A sample window cannot make that claim at all — it only ever shows
/// "not yet", which is what an early report and a merely slow one have in
/// common.
#[tokio::test]
async fn a_command_that_traps_the_interrupt_is_reported_not_silently_wedged() {
    let (mut s, mut rx) = bash(23);
    warm_shell(&mut s, &mut rx).await;

    s.send_text("trap '' INT; echo looprs-trap-set".into())
        .unwrap();
    let armed = run_logged(&mut rx).await;
    assert_eq!(armed.code, Some(0), "the trap builtin failed: {armed:?}");
    assert!(
        armed.out.contains("looprs-trap-set"),
        "the trap never reported itself installed, so the interrupt below would be aimed \
             at a shell that may not be trapping anything: {armed:?}"
    );

    in_flight(&mut s, "sleep 30").await;
    drain(&mut rx);

    let at_esc = std::time::Instant::now();
    s.abort().unwrap();
    assert!(s.quiesce().await, "the Esc was handled");
    assert_eq!(s.status(), SessionStatus::Aborting);

    // Block on the escalation itself; where it landed in time is read off the
    // same two events afterwards.
    let late = until_event(
        &mut rx,
        |l| l.starts_with("error:") && l.contains("still running"),
        "the stalled-interrupt report",
    )
    .await;
    assert!(
        at_esc.elapsed() >= cancel::GRACE,
        "the escalation cried wolf: it landed {:?} after the keystroke, inside the {:?} a \
             responsive child is still allowed to take: {late:?}",
        at_esc.elapsed(),
        cancel::GRACE
    );
    assert!(
        s.status().is_alive(),
        "the escalation must not kill the shell: cwd, env and jobs are the reason this mode exists"
    );

    // And the mode is not wedged: a second Esc is a *retry*, not a keystroke
    // swallowed by the first one's pending state.
    drain(&mut rx);
    s.abort().unwrap();
    assert!(s.quiesce().await, "the second Esc was handled");
    let again = until_event(
        &mut rx,
        |l| l.contains("cancelling") && l.contains("again"),
        "the retry acknowledgement",
    )
    .await;
    assert!(
        again.last().is_some_and(|l| l.contains("sleep 30")),
        "the retry did not name the command it is cancelling again: {again:?}"
    );
}
