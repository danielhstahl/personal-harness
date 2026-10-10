//! The shell tail: the bytes a Bash session writes that are not a document.
//!
//! A command's output arrives as chunks with no beginning the user chose and no
//! end the transcript picked; this is where those chunks stop being a byte
//! stream and become transcript entries. Three verbs, in the order the shell
//! offers them: [`SessionView::push_bash`] appends what arrived,
//! [`SessionView::flush_shell_pending`] hands the run's worth of it to the
//! store when a frame asks, and [`SessionView::seal_shell_output`] closes the
//! entry — the one thing that turns a tail into a finished record, and the
//! reason a truncated escape sequence cannot follow the user into the next
//! command's copy.
//!
//! The tail is a per-view field and these are its only three writers. That is
//! what lets the Bash mode show a command still streaming while
//! [`flush`](super::flush) stays a single door with one cursor behind it.
use crate::session::view::SessionView;

impl SessionView {
    /// Shell output into the transcript (ADR-0005, superseding the "strip the
    /// presentation" reading of ADR-0001 rule 5).
    ///
    /// The bytes are *resolved* — SGR becomes a style, `\r`/`\b`/`\t`/`EL` are
    /// applied inside the line, clusters keep their cell count — and the finished
    /// lines go to [`Transcript::push_shell_lines`] with their styles attached.
    /// What is still guaranteed by ADR-0001 rule 1 is unchanged: no markdown, and
    /// no re-wrap by us at any point between the child and the scrollback.
    ///
    /// `term_width` is the width the child is writing for, in cells, and it must
    /// be the **same value that was forwarded to the pty**. That is what makes a
    /// `\r` here land on the row the child believed it was on rather than on the
    /// head of the whole logical line (ADR-0005 Q2).
    pub fn push_bash(&mut self, chunk: &str, term_width: u16) {
        self.shell.set_wrap_width(term_width as usize);
        let lines = self.shell.feed(chunk);
        if lines.is_empty() {
            return;
        }
        self.transcript.push_shell_lines(&lines);
        self.after_write();
    }

    /// Move the line the resolver still has open into the transcript, ended.
    ///
    /// The open line lives in the resolver rather than in the entry so that it can
    /// still be overwritten by the next `\r` — which is the whole reason the
    /// store can stay an append-only list of finished lines. That only works if
    /// "the stream ended" is answered by *somebody*, and every path that ends one
    /// (a different kind of entry opening, the seal, teardown) calls this first.
    /// Nothing typed into a shell is allowed to evaporate because a newline
    /// never arrived before the prompt changed hands.
    pub(super) fn flush_shell_pending(&mut self) {
        if let Some(line) = self.shell.take_pending() {
            self.transcript
                .push_shell_lines(std::slice::from_ref(&line));
        }
    }

    /// Close off the shell output gathered so far, so the command about to run
    /// starts a fresh transcript entry (looprs-pdl.13).
    ///
    /// This is the *making* of the command boundary that
    /// [`Transcript::last_command_output`] reads. Without it a Bash session's
    /// whole life is one entry, because a stream of one kind is one entry by
    /// design, and "copy the last command's output" would silently mean
    /// "everything since the shell started" — a copy that widens itself is worse
    /// than one that refuses.
    ///
    /// The pending resolver line is flushed first and deliberately: it is almost
    /// always the prompt the shell came back to, which belongs to the block that
    /// just finished. Leaving it in the resolver and sealing underneath would let
    /// it land at the *front* of the next command's entry, which is the same
    /// bytes in the wrong place — the one mistake this file's whole
    /// text-and-styles-travel-together rule exists to prevent.
    ///
    /// Called at submit rather than at command completion because submit is the
    /// moment the boundary is known. The shell itself does not report where one
    /// command ends and the next begins, and inferring it from a prompt pattern
    /// would break on every shell that is not bash.
    pub fn seal_shell_output(&mut self) {
        self.flush_shell_pending();
        self.transcript.seal_command();
        // The seal finalises the block it just closed, so it is a write as far as
        // the journal and the cap are concerned: sealed and not journalled is the
        // same loss as never sealed, and the seal is the moment that block is
        // known to be finished.
        self.after_write();
    }
}
