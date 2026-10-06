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
}

impl Transcript {
    pub fn new() -> Self {
        Self { entries: vec![] }
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
}

#[cfg(test)]
mod tests {
    use super::*;
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
