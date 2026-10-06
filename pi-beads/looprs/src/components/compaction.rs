//! The live card for context compaction (`compaction_start` / `compaction_end`).
//!
//! A compaction is a separate LLM call that pauses the run, summarises the old
//! half of the conversation and prints *nothing* while it does it. Ten to sixty
//! seconds of a transcript that has stopped moving, which is precisely the shape
//! of "is it hung?" — the same shape an unreported tool call has, and the reason
//! this gets the same one-row treatment as one.
//!
//! The card is drawn twice from the same [`Entry`], exactly as the tool card is:
//! live, with a spinner, while the compaction is in flight, and once as a finished
//! row in the scrollback when it ends. Both draws read the entry rather than a
//! snapshot of their own, so the two can never disagree about what happened.

use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::components::status::fmt_tokens;
use crate::state::transcript::{Entry, MessageKind};
use crate::utils::render::FRAMES;

/// Where this session's compaction has got to.
///
/// Per card rather than per session: the state is what the glyph shows, and a
/// session can have compiled several finished cards alongside at most one running
/// one ([`crate::state::transcript::Transcript::finish_compaction`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompactionState {
    /// `compaction_start` seen, its end not yet.
    Running,
    /// Summarised and back.
    Done,
    /// The user pressed Esc while it was running (`aborted: true`).
    ///
    /// Grey, not red: the user asked for this and nothing broke. Painting a cancel
    /// in the same red as a failed API call teaches the user that their own keystroke
    /// is an error.
    Aborted,
    /// The summarisation call itself failed (`errorMessage`).
    Failed,
}

impl CompactionState {
    fn color(self) -> Color {
        match self {
            Self::Running => Color::Blue,
            Self::Done => Color::Green,
            Self::Aborted => Color::DarkGray,
            Self::Failed => Color::Red,
        }
    }

    fn icon(self, spinner: usize) -> &'static str {
        match self {
            Self::Running => FRAMES[spinner % FRAMES.len()],
            Self::Done => "✓",
            Self::Aborted => "⊘",
            Self::Failed => "✗",
        }
    }

    /// The words the card leads with, before the reason and the detail.
    fn verb(self) -> &'static str {
        match self {
            Self::Running => "compacting context",
            Self::Done => "context compacted",
            Self::Aborted => "compaction aborted",
            Self::Failed => "compaction failed",
        }
    }
}

/// What pi reported the run costing, before and after: `150k → 32k`.
///
/// Through the status row's own formatter rather than a second one, so the two
/// places that show token counts never disagree about what 32 000 looks like.
pub fn token_delta(before: u64, after: u64) -> String {
    format!("{} → {}", fmt_tokens(before), fmt_tokens(after))
}

pub fn compaction_line(e: &Entry, spinner: usize) -> Line<'static> {
    let MessageKind::Compaction { reason, state } = &e.kind else {
        return Line::default();
    };
    // What goes after the reason — `150k → 32k` on a success, pi's error string
    // on a failure — is the entry's `text`, the same slot a tool's result summary
    // uses, so both card kinds carry their detail the same way.
    let detail = &e.text;
    // `reason` ("manual" / "threshold" / "overflow") is dropped when it is empty,
    // which happens for the one card that never saw its `compaction_start` — a
    // lone `·` at the end of the row would read as a field that lost its value.
    let mut text = format!("{} {}", state.icon(spinner), state.verb());
    if !reason.is_empty() {
        text.push_str(&format!(" · {reason}"));
    }
    if !detail.is_empty() {
        text.push_str(&format!(" · {detail}"));
    }
    Line::from(text).style(Style::new().fg(state.color()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(reason: &str, state: CompactionState, detail: &str) -> Entry {
        Entry {
            kind: MessageKind::Compaction {
                reason: reason.into(),
                state,
            },
            text: detail.into(),
            done: state != CompactionState::Running,
        }
    }

    #[test]
    fn a_running_compaction_says_so_and_wears_the_spinner() {
        let l = compaction_line(&card("threshold", CompactionState::Running, ""), 3);
        let s = l.to_string();
        assert!(s.contains("compacting context"), "{s}");
        assert!(s.contains("threshold"), "why it started: {s}");
        assert!(
            s.starts_with(FRAMES[3 % FRAMES.len()]),
            "the spinner frame is the one it was handed: {s}"
        );
    }

    #[test]
    fn a_finished_one_reports_what_it_freed() {
        let d = token_delta(150_000, 32_000);
        let s = compaction_line(&card("threshold", CompactionState::Done, &d), 0).to_string();
        assert!(s.starts_with('✓'), "{s}");
        assert!(s.contains("150.0k → 32.0k"), "{s}");
    }

    /// The two ways a compaction can not-happen have to be tellable apart, and one
    /// of them must not look like a crash: `aborted` and `errorMessage` are the
    /// two things the wire says a user must not have to guess about.
    #[test]
    fn aborted_is_grey_and_failed_is_red() {
        let aborted = compaction_line(&card("manual", CompactionState::Aborted, ""), 0);
        let failed = compaction_line(&card("overflow", CompactionState::Failed, "boom"), 0);
        assert!(aborted.to_string().contains("aborted"), "{aborted}");
        assert!(failed.to_string().contains("boom"), "{failed}");
        assert_ne!(
            aborted.style.fg, failed.style.fg,
            "a cancel must not be painted in failure's colour"
        );
        assert_eq!(
            aborted.style.fg,
            Some(Color::DarkGray),
            "a cancel is grey: {:?}",
            aborted.style
        );
        assert_eq!(failed.style.fg, Some(Color::Red));
    }

    /// A card that never saw its start event still reads as a complete sentence —
    /// no dangling separator where the missing reason would have been.
    #[test]
    fn an_unknown_reason_leaves_no_trailing_separator() {
        let s = compaction_line(&card("", CompactionState::Done, ""), 0).to_string();
        assert_eq!(s.trim_end(), "✓ context compacted", "{s}");
        assert!(!s.contains("· "), "{s}");
    }
}
