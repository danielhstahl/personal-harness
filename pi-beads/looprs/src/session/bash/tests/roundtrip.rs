//! //! The ordinary round trip: a command goes in, its bytes come out, and the exit
//! //! code is the shell's rather than a guess.
//! //!
//! //! These are the tests for the marker contract ADR-0001 Q3 is built on — a real
//! //! `bash` on a real pty, the `DC1 looprs:exit:<code> DC2` marker stripped at
//! //! the boundary, and the byte order the child itself chose. Everything here is
//! //! one command or two; the interesting failures are the ones where the shell is
//! //! not yet up, the stream splits a codepoint in half, or the output has no
//! //! trailing newline to hide behind.

use crate::session::bash::reap::split_complete_utf8;

use super::*;

/// Cold start is lazy: nothing exists until the first command.
#[tokio::test]
async fn no_shell_exists_before_the_first_command() {
    let (s, mut rx) = bash(1);
    assert_eq!(s.status(), SessionStatus::NotStarted);
    assert!(rx.try_recv().is_err(), "and it says nothing either");
}

/// **Acceptance: `echo hi` -> `hi` appears, exit 0.**
#[tokio::test]
async fn echo_streams_its_output_and_reports_exit_0() {
    let (mut s, mut rx) = bash(2);
    s.send_text("echo hi".into()).unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert!(out.contains("hi"), "output was {out:?}");
    assert_eq!(code, Some(0));
    assert_no_marker_bytes(&out);
    assert_eq!(s.status(), SessionStatus::Idle, "settled back to idle");
}

/// **Acceptance: cwd persists.** The whole reason for a persistent shell: a
/// one-shot `bash -c` cannot remember a `cd`.
#[tokio::test]
async fn cwd_persists_across_commands() {
    let (mut s, mut rx) = bash(3);
    let target = "/tmp";
    s.send_text(format!("cd {target}")).unwrap();
    run_command(&mut rx).await;
    s.send_text("pwd".into()).unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(0));
    let lines = clean_lines(&out);
    assert!(
        lines.iter().any(|l| l == "/tmp" || l == "/private/tmp"),
        "cwd did not survive the previous command: {out:?}"
    );
}

/// **Acceptance: `false` -> visibly exit 1.**
#[tokio::test]
async fn a_failing_command_is_loudly_not_zero() {
    let (mut s, mut rx) = bash(4);
    s.send_text("false".into()).unwrap();
    let (_out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(1));
    let msgs = drain(&mut rx);
    assert_no_marker_bytes(&msgs.join("|"));
}

/// **Readiness, and the counter, together.** Three commands typed into a cold
/// shell with no waiting between them: none of them may be written before the
/// shell prints its first prompt, and each exit marker must close the command
/// it belongs to rather than the head of the queue.
///
/// This is the regression that read as "`false` exited 0": bash emits a
/// `PROMPT_COMMAND` marker for its *first* prompt before it has read any of
/// our input, so with a plain `running` bool that marker answered the command
/// typed at cold start.
#[tokio::test]
async fn commands_typed_before_the_shell_is_up_run_in_order_and_each_owns_its_exit() {
    let (mut s, mut rx) = bash(14);
    s.send_text("echo first-cmd".into()).unwrap();
    s.send_text("false".into()).unwrap();
    s.send_text("echo third-cmd".into()).unwrap();

    let a = run_logged(&mut rx).await;
    assert_eq!(a.code, Some(0), "first: {:?}", a);
    assert!(a.out.contains("first-cmd"), "first: {:?}", a.out);

    let b = run_logged(&mut rx).await;
    assert_eq!(b.code, Some(1), "second: {:?}", b.notes);

    let c = run_logged(&mut rx).await;
    assert_eq!(c.code, Some(0), "third: {:?}", c.notes);
    assert!(c.out.contains("third-cmd"), "third: {:?}", c.out);

    assert_eq!(s.status(), SessionStatus::Idle, "everything settled");
}

/// **Acceptance: stderr shows up, interleaved with stdout.** A pty merges the
/// two, so ordering is the program's own; a piped backend loses it.
#[tokio::test]
async fn stderr_and_stdout_arrive_in_the_programs_own_order() {
    let (mut s, mut rx) = bash(5);
    // One command, both streams, ordered: stdout, stderr, stdout.
    s.send_text("echo one; echo two 1>&2; echo three".into())
        .unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(0));
    let one = out.find("one").expect("stdout missing");
    let two = out.find("two").expect("stderr missing");
    let three = out.find("three").expect("later stdout missing");
    assert!(one < two && two < three, "streams were reordered: {out:?}");
}

/// `ls /nope` is the acceptance case that *stderr reaches the transcript*.
#[tokio::test]
async fn a_missing_path_shows_its_stderr() {
    let (mut s, mut rx) = bash(6);
    s.send_text("ls /definitely-not-a-real-path-xyz".into())
        .unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_ne!(code, Some(0), "ls of a missing path must not be a success");
    assert!(
        out.contains("No such file") || out.contains("cannot access"),
        "stderr never arrived: {out:?}"
    );
}

/// Env and shell state survive too, not just cwd — the persistent-shell claim.
#[tokio::test]
async fn exported_variables_persist_across_commands() {
    let (mut s, mut rx) = bash(10);
    s.send_text("export LOOPRS_PROBE=present".into()).unwrap();
    run_command(&mut rx).await;
    s.send_text("echo $LOOPRS_PROBE".into()).unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(0));
    assert!(out.contains("present"), "env did not persist: {out:?}");
}

/// A command with a non-UTF-8-safe boundary must not garble: the reader holds
/// the partial sequence until the rest arrives.
#[test]
fn a_utf8_sequence_split_across_reads_is_not_lost() {
    let bytes = "héllo".as_bytes();
    // Split in the middle of the two-byte `é`: `h` + the first half of it, and
    // nothing else may be handed back — 0xc3 on its own is not text.
    let (a, b) = split_complete_utf8(bytes[..2].to_vec());
    assert_eq!(a, "h");
    assert_eq!(b, vec![0xc3]);
    let (c, rest) = split_complete_utf8(b.into_iter().chain(bytes[2..].to_vec()).collect());
    assert_eq!(a + &c, "héllo");
    assert!(rest.is_empty());
}

/// The marker can straddle two reads, and a byte of it in the wrong place must
/// not corrupt the output around it.
#[tokio::test]
async fn a_command_whose_output_ends_without_a_trailing_newline_still_terminates() {
    let (mut s, mut rx) = bash(11);
    s.send_text("printf 'no-newline-end'".to_string()).unwrap();
    let (out, code) = run_command(&mut rx).await;
    assert_eq!(code, Some(0));
    assert!(out.contains("no-newline-end"), "{out:?}");
    assert_no_marker_bytes(&out);
}
