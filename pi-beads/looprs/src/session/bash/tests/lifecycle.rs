//! //! The shell's own life: bought on the first command, sized to the real window,
//! //! and — when it ends or fails to start — reported rather than left as a mode
//! //! that answers nothing.
//! //!
//! //! One persistent shell, per ADR-0001: it survives a command that exits it, and
//! //! the next command is told it is standing on a new one.

use super::*;

/// **Acceptance: `exit` -> a notice, and the next command works.**
#[tokio::test]
async fn exiting_the_shell_is_a_notice_and_the_next_command_starts_a_new_one() {
    let (mut s, mut rx) = bash(9);
    s.send_text("echo before".to_string()).unwrap();
    run_command(&mut rx).await;

    s.send_text("exit".into()).unwrap();
    let mut notices: Vec<String> = Vec::new();
    loop {
        let line = next_event(&mut rx).await;
        if line.starts_with("out ") {
            continue;
        }
        notices.push(line.clone());
        if line.contains("shell exited") {
            break;
        }
    }
    assert!(
        notices.iter().any(|l| l.contains("shell exited")),
        "the shell's exit was silent: {notices:?}"
    );
    assert_eq!(s.status(), SessionStatus::Dead);

    // The mode is not wedged: the next command brings a shell back, and says
    // so. The restart notice arrives *during* the command's own stream, so it
    // has to be read from there — draining afterwards misses it entirely, and
    // an assertion that cannot see it is an assertion that cannot fail.
    s.send_text("echo after".into()).unwrap();
    let ran = run_logged(&mut rx).await;
    assert_eq!(ran.code, Some(0));
    assert!(ran.out.contains("after"), "{:?}", ran.out);
    assert!(
        ran.notes.iter().any(|m| m.contains("restarted")),
        "the respawn was invisible: {:?}",
        ran.notes
    );
}

/// **ADR-0001 rule 6, asked of the shell itself**: `stty size` answers the
/// window we set rather than one frozen at process start, and a resize that
/// arrives *before* the shell exists is remembered and used for the pty when it
/// finally comes up. Nothing downstream can fix a wrong wrap afterwards: the
/// child already wrote its lines for the width it believed in.
#[tokio::test]
async fn the_shell_is_sized_to_the_real_window() {
    let (mut s, mut rx) = bash(15);
    // Before the first command: there is no shell to resize yet, so the size
    // is remembered for the spawn.
    s.resize(40, 132).unwrap();
    s.send_text("stty size".into()).unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(0), "stty said: {out:?}");
    assert!(
        out.contains("40 132"),
        "the pty was opened at the wrong size: {out:?}"
    );

    // With the shell running, a live resize reaches it too.
    s.resize(12, 90).unwrap();
    s.send_text("stty size".into()).unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(0), "stty said: {out:?}");
    assert!(
        out.contains("12 90"),
        "the running shell kept the old size: {out:?}"
    );
}

/// A command that dies with the shell is **named**. "Something was running" is
/// not actionable; "shell exited while running `sleep 30`" tells the user
/// which exit code they are never going to see.
///
/// Driven by the shell's own death (a background job SIGKILLs it mid-`sleep`),
/// not by app shutdown: that is the case the user hits, and the one the StreamEnd
/// path exists for.
///
/// The command is put in flight through the seam ([`in_flight`]) rather than by
/// polling `status()` until it reads `Running`, and the notice is waited for as
/// an event: the two things this test needs to be sure of are *which* message
/// arrived and *what it named*, neither of which a sample window can answer
/// (looprs-00u.17).
#[tokio::test]
async fn a_command_that_never_reported_is_named_when_the_shell_dies() {
    let (mut s, mut rx) = bash(16);
    warm_shell(&mut s, &mut rx).await;
    in_flight(
        &mut s,
        "(sleep 1; kill -KILL $$) >/dev/null 2>&1 & sleep 30",
    )
    .await;

    // `down` before the notice is the failure this used to guard against by
    // hand, so it stays part of what we wait for: stop on either, then say
    // which one it was.
    let seen = until_event(
        &mut rx,
        |l| l.contains("shell exited") || l.starts_with("down "),
        "the shell's death notice",
    )
    .await;
    let notice = seen.last().expect("until_event stopped on something");
    assert!(
        notice.contains("shell exited"),
        "the session reported itself gone without a notice: {seen:?}"
    );
    assert!(
        notice.contains("while running `"),
        "the unreported command was never named: {notice}"
    );
}

/// A shell that cannot be spawned is a reported error, not a hang: the mode
/// must say what went wrong rather than sit with the input box open.
#[tokio::test]
async fn a_shell_that_will_not_start_is_reported() {
    let cfg = SessionConfig {
        shell_bin: "/definitely/not/a/shell".into(),
        ..Default::default()
    };
    let (mut s, mut rx) = BashSession::build(SessionId::new(TerminalType::Bash, 13), &cfg).unwrap();
    s.send_text("echo hi".into()).unwrap();
    let line = next_event(&mut rx).await;
    assert!(line.starts_with("error"), "{line}");
    assert!(line.contains("shell"), "{line}");
    assert_eq!(s.status(), SessionStatus::NotStarted);
}
