//! `Esc`: the interrupt path, from the keystroke to the reap that backs it up.
//!
//! Three methods, one per thing that can go wrong with cancelling a command.
//! [`BashTask::interrupt`] is the user's `Esc` — take the queued command out of
//! the queue if it has not run, or put `0x03` on the pty if it has.
//! [`BashTask::send_sigint`] is the one that actually writes the byte and arms
//! the escalation; [`BashTask::interrupt_stalled`] fires when the command
//! ignored it and says so, because a shell that will not take `0x03` is a
//! different failure from one that took it slowly, and the user is told which.
//!
//! The escalation numbers live in [`cancel`](crate::session::cancel), not here;
//! what lives here is the ordering — the notice goes out before the byte, so the
//! transcript says "cancelling" while the command is still cancelleable
//! (looprs-5g7).

use crate::session::bash::BashCmd;
use crate::session::bash::reap::clip;
use crate::session::bash::task::BashTask;

use crate::session::cancel;

impl BashTask {
    /// `Esc` / Ctrl-C: interrupt the command in front of the shell.
    ///
    /// Four cases, and only the last two send a byte:
    ///
    /// * **nothing outstanding, nothing queued** — an idle prompt. `0x03` would
    ///   print a `^C` for no reason, so an idle Esc is a silent no-op, not an
    ///   error (looprs-5g7).
    /// * **queued, not started** — the command is still in this session's own
    ///   queue, because [`Self::submit`] never writes to a shell that has not
    ///   printed its prompt. Nothing has reached the child, so there is nothing to
    ///   signal and the cancel *is* the dequeue. It is said out loud
    ///   ([`cancel::dropped_before_start`]) because the alternative was the worst
    ///   answer a cancel key can give: the keystroke lost **and** the command left
    ///   queued to start a moment later. `status()` calls that window `Running`,
    ///   so from the user's side there is a command to cancel — the branch that
    ///   refuses to see it is the bug, not the status.
    /// * **already cancelling and the attempt is still live** — the byte is already
    ///   in the line discipline. Stacking a second one on it buys nothing and
    ///   makes "which interrupt is the pending one?" unanswerable, so the repeat
    ///   keystroke is dropped rather than queued behind it.
    /// * **cancelling, but the last attempt was reported stalled** — that report
    ///   is the user being told the command ignored us; hitting Esc again means
    ///   "try anyway", so the ladder restarts with a fresh attempt and a fresh
    ///   deadline.
    pub(super) fn interrupt(&mut self) {
        if self.outstanding.is_empty() {
            let queued: Vec<String> = std::mem::take(&mut self.queue).into();
            if queued.is_empty() {
                return;
            }
            // The whole queue rather than the newest entry: "stop" means the things
            // I have not seen start shall not start. Picking among them would be
            // guessing at the meaning of a key whose whole job is "not that".
            let what = if queued.len() == 1 {
                format!("`{}`", clip(&queued[0], 60))
            } else {
                format!("{} queued commands", queued.len())
            };
            self.note(cancel::dropped_before_start(&what, queued.len() > 1));
            return;
        }
        if self.aborting && !self.stall_reported {
            return;
        }
        let what = clip(&self.foreground(), 60);
        let note = if self.aborting {
            format!("cancelling `{what}` again…")
        } else {
            cancel::started(&format!("`{what}`"))
        };
        self.send_sigint(note);
    }

    /// One `0x03`, one word about it, one deadline armed.
    ///
    /// The word goes out *before* anything can come back, because the thing this
    /// row of the contract buys is the end of the silence: the shell may take
    /// seconds to unwind a `find`, and the user should not have to wonder whether
    /// their keystroke did anything at all.
    fn send_sigint(&mut self, note: String) {
        let Some(shell) = self.shell.as_mut() else {
            return;
        };
        if let Err(e) = shell.write_line("\x03") {
            self.err(format!("interrupt did not reach the shell: {e:#}"));
            return;
        }
        self.aborting = true;
        self.stall_reported = false;
        self.interrupt_attempt += 1;
        let attempt = self.interrupt_attempt;
        self.note(note);
        cancel::arm(self.cmd.clone(), BashCmd::InterruptStalled { attempt });
    }

    /// The `0x03` sent `attempt` ago produced no exit marker within
    /// [`cancel::GRACE`].
    ///
    /// A command can be uncancellable by us without anything being wrong with the
    /// shell: `trap '' INT`, a program that reset its own handler, a process stuck
    /// in an uninterruptible syscall. What the session must **not** do is kill the
    /// shell to make the prompt come back — cwd, exports, aliases and background
    /// jobs are the entire reason Bash mode has a pty (ADR-0001), and they are
    /// exactly what a `kill` here would throw away in order to fix somebody
    /// else's command.
    ///
    /// So the escalation is one sentence, and the choice stays with the user:
    /// `Esc` sends another interrupt, `Ctrl-Q` quits and takes the shell along.
    pub(super) fn interrupt_stalled(&mut self, attempt: u32) {
        // Not ours if this attempt is no longer the live one: the marker may have
        // landed first, or the user may have Esc'd again and a newer attempt now
        // owns the deadline.
        if !self.aborting || self.stall_reported || self.interrupt_attempt != attempt {
            return;
        }
        self.stall_reported = true;
        let what = clip(&self.foreground(), 60);
        self.err(format!(
            "{} — it may be trapping the interrupt. Esc sends another one; Ctrl-Q quits if you meant it.",
            cancel::stalled(&what)
        ));
    }
}
