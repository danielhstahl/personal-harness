use crate::components::compaction::CompactionState;
use crate::components::tool::ToolStateCategory;
use crate::utils::shelltext::{StyleRun, StyledLine};

#[derive(Clone, Debug, PartialEq)]
pub enum MessageKind {
    User,
    Thinking,
    Answer,
    Tool {
        id: String,
        name: String,
        input: String,
        state: ToolStateCategory,
    },
    /// A context compaction (`compaction_start` / `compaction_end`): pi pausing
    /// the run to summarise old messages so the conversation fits the context
    /// window.
    ///
    /// The same shape as a tool card, on purpose. Both are one row saying "the
    /// session is busy with *this*, and here is how it went", and both were
    /// invisible before: a compaction is a multi-second LLM call that prints
    /// nothing, so without this entry it is indistinguishable from a hang.
    ///
    /// No `id`, unlike a tool: pi's compaction events carry none, and only one
    /// compaction can be in flight per session, so "the open compaction card" is
    /// an unambiguous thing to look up
    /// ([`Transcript::finish_compaction`]).
    Compaction {
        /// `"manual"`, `"threshold"` or `"overflow"` — why the run stopped to
        /// do this. Empty when the card was opened by a `compaction_end` that
        /// never saw its `compaction_start`; the renderer drops the segment then
        /// rather than printing a bare separator.
        reason: String,
        state: CompactionState,
    },
    /// Loop/harness status lines ("working looprs-1", "board empty, awaiting input").
    System,
    /// Raw shell output from the Bash terminal state.
    ///
    /// ADR-0001 rule 1: **no markdown, no theme reinterpretation, no re-wrap.**
    /// The child wrapped its own lines for a width it knows; running this through
    /// the markdown renderer would corrupt tables, progress bars and box-drawing.
    Bash,
    Error,
}
impl MessageKind {
    /// Appended incrementally and closed implicitly (by a different kind arriving,
    /// or by `finish_last`).
    pub fn is_streamed(&self) -> bool {
        matches!(
            self,
            MessageKind::Thinking | MessageKind::Answer | MessageKind::Bash
        )
    }

    /// Streamed **and** rendered verbatim: never a markdown or wrap pass over it.
    pub fn is_raw(&self) -> bool {
        matches!(self, MessageKind::Bash)
    }
}

pub struct Entry {
    pub kind: MessageKind,
    pub text: String,
    pub done: bool,
    /// The *presentation* of `text`, as byte ranges into it (ADR-0005).
    ///
    /// Only shell output ever fills this: it is the one kind whose incoming bytes
    /// carry styling that is worth keeping and that cannot be expressed in the
    /// `text` itself. Empty means "nothing was said about style", which is not
    /// the same fact as "default style" but renders the same, and it is the
    /// common case for every other kind in the tree.
    ///
    /// Ranges are only meaningful against this entry's own `text` — they are
    /// re-based at push time ([`Transcript::push_shell_lines`]) — so an
    /// `Entry`'s text and styles must always be appended together.
    pub styles: Vec<StyleRun>,
}

#[derive(Default)]
pub struct Transcript {
    pub entries: Vec<Entry>,
    /// The entry index the last submitted Bash command's output starts at, set by
    /// [`Transcript::seal_command`]. This is the command boundary the copy reads:
    /// a Bash session is one long stream of one kind, so without an index there is
    /// nothing that says where this command stops and the next one starts.
    command_start: Option<usize>,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            entries: vec![],
            command_start: None,
        }
    }
    /// Streaming append. Same kind as the open entry extends it; a different kind
    /// closes it and starts a new one (thinking -> answer happens automatically).
    pub fn push_delta(&mut self, kind: MessageKind, delta: &str) {
        match self.entries.last_mut() {
            Some(e) if e.kind == kind && !e.done => e.text.push_str(delta),
            _ => {
                self.finish_last();
                self.entries.push(Entry {
                    kind,
                    text: delta.to_string(),
                    done: false,
                    styles: Vec::new(),
                });
            }
        }
    }

    /// Resolved shell output, appended line by line with its styling intact
    /// (ADR-0005).
    ///
    /// This is the only way shell output enters a transcript — the plain
    /// [`Self::push_delta`] path would take the bytes but drop the one thing the
    /// resolver went to some trouble to work out. Each line is written with its
    /// own terminating `\n`, so an entry's text is a sequence of complete lines
    /// and its `styles` are byte ranges into that text: the styles travel with
    /// the text they belong to, and the two can never be re-paired by guesswork.
    ///
    /// Extends the open Bash entry if there is one (a shell stream is one entry,
    /// not one entry per line), and closes whatever else was open, exactly like
    /// [`Self::push_delta`] would.
    pub fn push_shell_lines(&mut self, lines: &[StyledLine]) {
        if lines.is_empty() {
            return;
        }
        match self.entries.last_mut() {
            Some(e) if e.kind == MessageKind::Bash && !e.done => {}
            _ => {
                self.finish_last();
                self.entries.push(Entry {
                    kind: MessageKind::Bash,
                    text: String::new(),
                    done: false,
                    styles: Vec::new(),
                });
            }
        }
        let e = self
            .entries
            .last_mut()
            .expect("the entry above was just created");
        for line in lines {
            let base = e.text.len();
            e.text.push_str(&line.text);
            e.text.push('\n');
            for run in &line.runs {
                e.styles.push(StyleRun {
                    start: base + run.start,
                    end: base + run.end,
                    style: run.style,
                });
            }
        }
    }

    /// Complete, one-shot entries (user message, tool result, error).
    pub fn push_done(&mut self, kind: MessageKind, text: String) {
        self.finish_last();
        self.entries.push(Entry {
            kind,
            text,
            done: true,
            styles: Vec::new(),
        });
    }

    //start of adding tools
    pub fn start_tool(&mut self, id: String, name: String, input: String) {
        self.finish_last();
        self.entries.push(Entry {
            kind: MessageKind::Tool {
                id,
                name,
                input,
                state: ToolStateCategory::InProgress,
            },
            text: String::new(), // result summary arrives later
            done: false,
            styles: Vec::new(),
        });
    }

    pub fn finish_tool(&mut self, id: String, summary: String, is_error: bool) {
        let found = self
            .entries
            .iter_mut()
            .rev()
            .find(|e| matches!(&e.kind, MessageKind::Tool { id: i, .. } if *i == id));
        if let Some(e) = found {
            if let MessageKind::Tool { state, .. } = &mut e.kind {
                *state = if is_error {
                    ToolStateCategory::Error
                } else {
                    ToolStateCategory::Success
                };
            }
            e.text = summary;
            e.done = true;
        }
    }

    /// The cards that are still running, in transcript order.
    ///
    /// Both openable kinds — tools and compaction — because the live region's card
    /// band shows "everything the session is in the middle of", and a compaction
    /// that missed its row is exactly the thing the band exists to make visible.
    pub fn open_cards(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| {
            !e.done
                && matches!(
                    e.kind,
                    MessageKind::Tool { .. } | MessageKind::Compaction { .. }
                )
        })
    }

    /// pi is pausing the run to compact the context. Opens the card; the matching
    /// [`Self::finish_compaction`] closes it.
    pub fn start_compaction(&mut self, reason: String) {
        self.finish_last();
        self.entries.push(Entry {
            kind: MessageKind::Compaction {
                reason,
                state: CompactionState::Running,
            },
            // What the card adds after the reason — `150k → 32k` on a success,
            // pi's error string on a failure — arrives later, in `text`: the same
            // slot a tool's result summary uses.
            text: String::new(),
            done: false,
            styles: Vec::new(),
        });
    }

    /// Close the open compaction card with `state`; `detail` becomes the row's
    /// trailing text (`150k → 32k`, or pi's error string).
    ///
    /// Returns `false` when there was no card open to close — a `compaction_end`
    /// whose `compaction_start` never arrived. The caller decides what to do with
    /// that (record it anyway rather than swallow it); what this function will not
    /// do is invent a card behind the caller's back, because the one thing that
    /// *was* open may be the card of a compaction nobody has reported the end of.
    pub fn finish_compaction(&mut self, state: CompactionState, detail: String) -> bool {
        if let Some(e) = self.entries.iter_mut().rev().filter(|e| !e.done).find(|e| {
            matches!(
                &e.kind,
                MessageKind::Compaction {
                    state: CompactionState::Running,
                    ..
                }
            )
        }) {
            if let MessageKind::Compaction { state: s, .. } = &mut e.kind {
                *s = state;
            }
            e.text = detail;
            e.done = true;
            return true;
        }
        false
    }

    /// Force-close every card left open, as `Aborted`.
    ///
    /// Called when the stream behind them ends ([`SessionView::seal`](crate::session::view::SessionView::seal)).
    /// A `!done` entry is not a harmless loose end: the flusher stops dead at the
    /// first one and nothing after it ever reaches the scrollback, so a session
    /// that died mid-tool takes the rest of its own transcript down with it.
    ///
    /// `Aborted` rather than leaving the state at "in progress": a frozen spinner
    /// in a transcript whose process is gone is a lie that outlives its subject.
    pub fn abandon_open_cards(&mut self) {
        for e in self.entries.iter_mut().filter(|e| !e.done) {
            match &mut e.kind {
                MessageKind::Tool { state, .. } => *state = ToolStateCategory::Aborted,
                MessageKind::Compaction { state, .. } => *state = CompactionState::Aborted,
                // Streamed kinds are `finish_last`'s business, and anything else
                // is already final by construction.
                _ => {}
            }
            e.done = true;
        }
    }

    /// Total buffered text. The per-view cap (ADR-0002 "Consequences": buffered
    /// output while hidden is unbounded) measures this so a chatty child that
    /// nobody is looking at cannot grow the process forever.
    pub fn byte_len(&self) -> usize {
        self.entries.iter().map(|e| e.text.len()).sum()
    }

    pub fn finish_last(&mut self) {
        // only streamed entries close implicitly; a running tool must not be closed
        // by whatever comes next (parallel tools, for example)
        // tools are often run in parallel, so this is required
        if let Some(e) = self.entries.last_mut().filter(|e| e.kind.is_streamed()) {
            e.done = true;
        }
    }

    // ─────────────── the keyboard copy targets (looprs-pdl.13) ───────────────
    //
    // These four resolvers are the whole of "what does this chord copy?". They are
    // here, next to the entries they read, for the same reason `CopyOutcome::toast`
    // lives next to its variants: the moment a second module has its own opinion
    // about what "the last answer" is, there are two answers and no way to tell
    // which one the clipboard holds.
    //
    // Every one of them reads the *entry*, not the rendered store, and that is
    // deliberate. A drag selection has to be a cell rectangle over wrapped rows
    // and therefore has to go through the store; a chord has no geometry to be
    // wrong about, so the answer is the text itself, complete, whether or not it
    // has ever been flushed to a screen and whether or not the view has been
    // trimmed since. Copying "the last answer" out of the visible window would
    // make the chord's result depend on where the user was scrolled to, which is
    // a fact about the frame and not about the answer.

    /// The text of the most recent assistant answer, whole.
    ///
    /// `None` when the session has not answered anything — which in Bash mode is
    /// the normal case, and the caller is expected to say so rather than to copy
    /// nothing in silence.
    pub fn last_answer(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| e.kind == MessageKind::Answer)
            .map(|e| e.text.as_str())
    }

    /// The text of the most recent shell block: everything the shell said for the
    /// last command, and nothing before it.
    ///
    /// The boundary is not inferred here — it is *made* at submit time by
    /// [`Self::seal_command`] (called through the view), which closes the open
    /// Bash entry so the next command's output starts a new one. Without that
    /// seal a whole Bash session is a single entry, because a stream of one kind
    /// is one entry by design ([`Transcript::push_shell_lines`]), and "the last
    /// command's output" would silently mean "every command since the mode was
    /// entered". Choosing to widen the copy rather than fail would be the worse
    /// lie, so the boundary is real and this returns the last entry only.
    ///
    /// What the entry holds is what the terminal held: the shell's own echo of the
    /// line, the output, and the prompt it came back to. Trimming the prompt off
    /// the end would mean guessing where output ends and shell furniture starts,
    /// and ADR-0001 rule 1 is that the shell's bytes are not ours to reinterpret.
    ///
    /// Blank entries *inside* the block are kept — a command's output may
    /// legitimately contain a blank line. A block whose whole content is blank is
    /// `None`, and the difference between that and "no command has run"
    /// ([`Self::has_command_boundary`]) is the difference between "it said
    /// nothing" and "nothing was asked".
    ///
    /// Reading the *block* rather than "the last Bash entry" is not a preference.
    /// A real pty leaves a trailing blank artifact: the next prompt arrives as a
    /// bare `\r`, which is a Bash line of its own, and "the last Bash entry" made
    /// `Ctrl-S o` answer "the last command's output is blank" about a screen
    /// full of the command's output. Measured through tmux in
    /// `spikes/tmux_keyboard_e2e.py`; this shape is what closes it.
    pub fn last_command_output(&self) -> Option<String> {
        let start = self.command_start?;
        if start >= self.entries.len() {
            return None;
        }
        let block: Vec<&str> = self.entries[start..]
            .iter()
            .filter(|e| e.kind == MessageKind::Bash && !e.text.trim().is_empty())
            .map(|e| e.text.as_str())
            .collect();
        if block.is_empty() {
            return None;
        }
        // The trailing newline each pushed line carries is stripped. A copy that
        // ends in a newline is a copy that runs an extra command the moment it is
        // pasted into a shell, and the user asked for the output, not for a
        // keystroke. Blank lines *inside* the block survive; only the tail goes.
        Some(block.join("\n").trim_end_matches('\n').to_string())
    }

    /// Whether a command boundary exists at all — which is how the refusal tells
    /// "no command has run yet" apart from "the last command said nothing".
    pub fn has_command_boundary(&self) -> bool {
        self.command_start.is_some()
    }

    /// The result summary of the most recent *finished* tool card.
    ///
    /// This is the agentic modes' answer to the same question `Ctrl-S o` asks in
    /// Bash — "what did the last thing I made this session do produce?". An
    /// in-progress card is skipped: its `text` is empty until
    /// [`Transcript::finish_tool`] fills it, and copying an empty string under a
    /// `Copied` verb is exactly the confident wrong answer R20 is about.
    pub fn last_tool_output(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find(|e| matches!(e.kind, MessageKind::Tool { .. }) && e.done && !e.text.is_empty())
            .map(|e| e.text.as_str())
    }

    /// Close off the open entry so the next thing written starts a new one.
    ///
    /// The command boundary for the Bash copy (see
    /// [`Transcript::last_command_output`]). Only streamed entries close, exactly
    /// as in [`Transcript::finish_last`]: a running tool card is not closed by
    /// whatever the user typed over the top of it, because parallel tools are
    /// ordinary and a seal that ate an open card would lose the run's state.
    ///
    /// Idempotent, and never creates an entry: sealing an empty transcript leaves
    /// an empty transcript, so a submit that produced nothing adds nothing.
    pub fn seal_command(&mut self) {
        self.finish_last();
        // The boundary is the *next* entry: everything from here on is what this
        // command produced. Recorded after the finish so a seal never points into
        // an entry that belongs to the command before it.
        self.command_start = Some(self.entries.len());
    }

    /// The whole transcript as plain text, entries joined by a blank line.
    ///
    /// The escape hatch for "anything bigger than a clipboard" (looprs-pdl.13):
    /// the *content*, with no frame chrome, no line numbers and no band markers,
    /// because the file's use is `grep` and a paste, and both are ruined by
    /// decoration. Entries are separated by a blank line so a shell block does
    /// not run into the answer that follows it; the text inside each entry is
    /// verbatim, styles dropped (ADR-0004 R14 says a copy never carries a style;
    /// a file copy is a copy).
    pub fn plain_text(&self) -> String {
        let mut out = String::with_capacity(self.byte_len() + self.entries.len() * 2);
        for e in &self.entries {
            // One blank between entries, whatever the entries happen to end with.
            // Trailing blank runs *inside* an entry's tail are collapsed: a file
            // with three blank lines where one entry stopped and the next started
            // reads like something broke.
            let body = e.text.trim_end_matches('\n');
            if !out.is_empty() && !body.is_empty() {
                out.push('\n');
            }
            if !body.is_empty() {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(body);
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn shell_line(text: &str) -> crate::utils::shelltext::StyledLine {
        crate::utils::shelltext::StyledLine {
            text: text.to_string(),
            runs: vec![],
            cells: text.chars().count(),
        }
    }

    fn shell(t: &mut Transcript, lines: &[&str]) {
        t.push_shell_lines(
            &lines
                .iter()
                .map(|l| shell_line(l))
                .collect::<Vec<crate::utils::shelltext::StyledLine>>(),
        );
    }

    /// The block is what arrived since the last seal, and nothing before it.
    #[test]
    fn the_command_block_is_what_arrived_since_the_last_seal() {
        let mut t = Transcript::new();
        shell(&mut t, &["before the command"]);
        t.seal_command();
        shell(&mut t, &["$ make", "built"]);

        assert_eq!(
            t.last_command_output().as_deref(),
            Some("$ make\nbuilt"),
            "both lines of the block, neither of what came before"
        );
    }

    /// The reason the read is a block and not "the last Bash entry": a real pty
    /// ends a command with a blank line of its own — the carriage return of the
    /// next prompt — and with the `exit 0` note breaking the stream behind it, the
    /// last entry in the transcript was the blank. `Ctrl-S o` answered "the last
    /// command's output is blank" about a screen full of the command's output.
    /// Seen through tmux in `spikes/tmux_keyboard_e2e.py`.
    #[test]
    fn a_trailing_blank_shell_line_does_not_erase_the_command_that_ran() {
        let mut t = Transcript::new();
        t.seal_command();
        shell(&mut t, &["$ echo MARKER", "MARKER"]);
        t.push_done(MessageKind::System, "exit 0".into());
        shell(&mut t, &["\r"]);

        let got = t.last_command_output().expect("the command is still there");
        assert!(got.contains("MARKER"), "{got:?}");
        assert!(
            !got.contains("exit 0"),
            "our note is not the command's output"
        );
    }

    /// The three nothings are distinguishable, because the user can only act on
    /// the one that is actually true.
    #[test]
    fn no_boundary_and_no_output_are_different_facts() {
        let mut t = Transcript::new();
        assert!(
            t.last_command_output().is_none() && !t.has_command_boundary(),
            "nothing has been submitted"
        );

        t.seal_command();
        shell(&mut t, &["\r", "   "]);
        assert!(
            t.last_command_output().is_none() && t.has_command_boundary(),
            "a command ran and said nothing: a boundary exists, output does not"
        );
    }

    use crate::components::compaction::CompactionState;

    fn compaction_in(state: CompactionState) -> impl Fn(&&Entry) -> bool {
        move |e| matches!(&e.kind, MessageKind::Compaction { state: s, .. } if *s == state)
    }

    fn tool_named(id: &str) -> impl Fn(&&Entry) -> bool + '_ {
        move |e| matches!(&e.kind, MessageKind::Tool { id: i, .. } if i == id)
    }

    /// The correlation the card depends on: pi gives compaction no
    /// `toolCallId`, so "the running card" has to be what the end event finds —
    /// including when streamed text has been pushed on top of it in the meantime.
    #[test]
    fn the_end_event_finds_the_running_card_whatever_sits_above_it() {
        let mut t = Transcript::new();
        t.start_compaction("threshold".into());
        t.push_delta(MessageKind::Answer, "text that arrived mid-compaction\n");

        assert!(
            t.finish_compaction(CompactionState::Done, "150.0k -> 32.0k".into()),
            "there was a card to close"
        );

        let card = t
            .entries
            .iter()
            .find(compaction_in(CompactionState::Done))
            .expect("the card is finished, not gone");
        assert!(card.done, "and it released the flush cursor");
        assert_eq!(card.text, "150.0k -> 32.0k");
        assert_eq!(t.open_cards().count(), 0, "nothing is left running");
    }

    /// `false` is not noise: it is how the caller learns the end arrived with no
    /// start, and records the event anyway rather than swallowing it.
    #[test]
    fn finishing_with_nothing_running_says_so_instead_of_inventing_a_card() {
        let mut t = Transcript::new();
        assert!(!t.finish_compaction(CompactionState::Done, String::new()));
        assert!(
            t.entries.is_empty(),
            "a failed finish must not leave a half-explained entry behind"
        );
    }

    /// A second compaction closes its own card and never rewrites the first: two
    /// finished rows in one transcript must describe two different events.
    #[test]
    fn a_finished_card_is_left_alone_by_the_next_compaction() {
        let mut t = Transcript::new();
        t.start_compaction("threshold".into());
        t.finish_compaction(CompactionState::Done, "first".into());
        t.start_compaction("manual".into());
        t.finish_compaction(CompactionState::Aborted, String::new());

        let cards: Vec<&Entry> = t
            .entries
            .iter()
            .filter(|e| matches!(e.kind, MessageKind::Compaction { .. }))
            .collect();
        assert_eq!(cards.len(), 2);
        assert!(compaction_in(CompactionState::Done)(&cards[0]));
        assert!(compaction_in(CompactionState::Aborted)(&cards[1]));
        assert_eq!(
            cards[0].text, "first",
            "the first card's own words survive the second compaction"
        );
    }

    /// The seal path, at the transcript's own level: every open card of either
    /// kind becomes a closed, `Aborted` one, and entries that were already final
    /// are not touched.
    #[test]
    fn abandoning_closes_every_open_card_and_nothing_else() {
        let mut t = Transcript::new();
        t.push_done(MessageKind::User, "a prompt".into());
        t.start_tool("t1".into(), "bash".into(), "make test".into());
        t.start_tool("t2".into(), "read".into(), "src/app.rs".into());
        t.start_compaction("overflow".into());
        t.finish_tool("t1".into(), "ok".into(), false);
        assert_eq!(t.open_cards().count(), 2, "t2 and the compaction");

        t.abandon_open_cards();

        assert_eq!(t.open_cards().count(), 0);
        for e in &t.entries {
            assert!(e.done, "nothing is left open to stall the flusher");
        }
        let abandoned = t.entries.iter().find(tool_named("t2")).unwrap();
        assert!(
            matches!(&abandoned.kind, MessageKind::Tool { state, .. } if *state == ToolStateCategory::Aborted),
            "the unfinished tool says it was abandoned, not that it succeeded"
        );
        let reported = t.entries.iter().find(tool_named("t1")).unwrap();
        assert!(
            matches!(&reported.kind, MessageKind::Tool { state, .. } if *state == ToolStateCategory::Success),
            "and the one that reported back keeps its answer"
        );
    }
}
