//! The copy half of [`App`]: what a copy is, where it goes, and what the user is
//! told about it.
//!
//! Three doors, two pending receipts and one state machine:
//!
//! * [`App::copy_text`] — the one door every copy goes through: count it, queue
//!   it, never wait on it (ADR-0004 R12/R13/R19);
//! * [`App::copy_target`] and [`App::dump_transcript`] — the keyboard's doors,
//!   where a refusal is a spoken outcome rather than an error handed back to a
//!   keystroke;
//! * [`App::poll_copy`] and [`App::poll_dump`] — the receipts, collected from the
//!   tick so a wedged sink turns into a visible late failure instead of a
//!   confidence nobody checks;
//! * [`CopyChord`] — the `Ctrl-S` prefix, which exists because the chord budget
//!   was already spent before this one was asked for.
//!
//! The types here stay inside `App`'s module tree: nothing outside `app` needs to
//! know that a pending copy is a receipt plus the timestamp that turns "still in
//! flight" into "timed out, nothing confirmed".

use super::App;
use crate::session::TerminalType;
use crate::state::selection::Selection;
use crossterm::event::{KeyCode, KeyModifiers};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The prefix state of the copy chord (looprs-pdl.13).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum CopyChord {
    /// No prefix outstanding. Where everyone starts and where everything ends.
    #[default]
    Off,
    /// `Ctrl-S` was pressed; the next key is the target.
    Armed,
}

/// What a copy chord asked for, before anyone has looked for it.
///
/// The three targets are *resolved*, not guessed: a target that is not there is a
/// refusal with a reason naming what is missing, never a shorter copy, and never
/// an adjacent thing that happened to be available. A user who asks for "the last
/// answer" in a session that has none and gets the last thinking block instead has
/// been lied to in the one place a lie is expensive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyTarget {
    /// The last assistant answer.
    Answer,
    /// The last command's output — the shell's for Bash, a tool card's result for
    /// the agentic modes, because that is what "the last thing this mode ran"
    /// means there.
    CommandOutput,
    /// Whatever selection is live, mouse-made or otherwise.
    Selection,
}

impl CopyTarget {
    /// The chord's own spelling, for refusal text that names what was asked for.
    fn label(self) -> &'static str {
        match self {
            CopyTarget::Answer => "the last answer",
            CopyTarget::CommandOutput => "the last command's output",
            CopyTarget::Selection => "the selection",
        }
    }
}

/// The second key of the copy chord, as classified.
///
/// A total function from a keystroke to a meaning, with `NotAChord` as the
/// explicit remainder: the alternative is an `Option` whose `None` gets handled
/// differently in three places, and a chord table with three answers is no table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum CopyKey {
    Answer,
    Output,
    Selection,
    TranscriptFile,
    Help,
    Cancel,
    NotAChord,
}

/// Classify a keystroke against the chord family.
///
/// Modifiers are deliberately ignored on the second key: users who keep `Ctrl`
/// down through a chord (`Ctrl-S Ctrl-A`) are common, and honouring the
/// modifier-taking reading costs nothing because none of these letters means
/// anything else while the prefix is outstanding — which is the whole argument
/// for a prefix in the first place, and the reason the shadowing audit is one key
/// (`Ctrl-S`) rather than six.
pub(super) fn copy_chord_key(k: crossterm::event::KeyEvent) -> CopyKey {
    match k.code {
        KeyCode::Char('a') | KeyCode::Char('A') => CopyKey::Answer,
        KeyCode::Char('o') | KeyCode::Char('O') => CopyKey::Output,
        KeyCode::Char('s') | KeyCode::Char('S') => CopyKey::Selection,
        KeyCode::Char('t') | KeyCode::Char('T') => CopyKey::TranscriptFile,
        KeyCode::Char('?') => CopyKey::Help,
        KeyCode::Esc => CopyKey::Cancel,
        _ => CopyKey::NotAChord,
    }
}

/// A keystroke spelled the way a toast has to spell it.
///
/// `{:?}` on a `KeyCode` says `Char('x')`, which is fine in a log and wrong in
/// a message meant for a person: they read that as a quote about a character and
/// not as the key they pressed.
pub(super) fn key_word(k: crossterm::event::KeyEvent) -> String {
    let base = match k.code {
        KeyCode::Char(c) => {
            if k.modifiers.contains(KeyModifiers::SHIFT) {
                format!("Shift-{c}")
            } else {
                format!("`{c}`")
            }
        }
        KeyCode::Esc => "Esc".to_string(),
        KeyCode::Enter => "Enter".to_string(),
        KeyCode::Tab => "Tab".to_string(),
        KeyCode::BackTab => "Shift-Tab".to_string(),
        KeyCode::Backspace => "Backspace".to_string(),
        KeyCode::Delete => "Delete".to_string(),
        KeyCode::Home => "Home".to_string(),
        KeyCode::End => "End".to_string(),
        KeyCode::PageUp => "PageUp".to_string(),
        KeyCode::PageDown => "PageDown".to_string(),
        KeyCode::F(n) => format!("F{n}"),
        other => format!("{other:?}"),
    };
    if k.modifiers.contains(KeyModifiers::CONTROL) && !base.starts_with("Ctrl-") {
        format!("Ctrl-{base}")
    } else {
        base
    }
}

/// How long the copy prefix stays outstanding (looprs-pdl.13).
///
/// Equal to [`TOAST_TTL`](crate::state::toast::TOAST_TTL) on purpose: the hint
/// that names the window lives exactly as long as the window does, so "the hint
/// is still up" and "the chord is still armed" are one fact on screen rather
/// than two claims that can disagree — and a user who waits too long sees the
/// hint go away at the moment it stopped being true.
pub const COPY_CHORD_WINDOW: Duration = crate::state::toast::TOAST_TTL;

/// A dump handed to the sink, with the number the late-failure toast needs if the
/// sink is slow: how much was asked for, and how long we have been waiting.
#[derive(Debug)]
pub(super) struct PendingDump {
    receipt: crate::services::transcript_file::DumpReceipt,
    chars: crate::services::clipboard::Chars,
    sent_at: Instant,
}

/// The mode's name in file-name dress: lowercase, one word, no spaces.
///
/// `TerminalType::label` is "Bash"/"Pi"/"Beeds" — for a status row. A file name
/// wants the other case, and the mapping is written out rather than
/// `to_lowercase()`d at the call site so a future rename of the label cannot
/// silently change where a user's transcripts live.
fn mode_label(mode: TerminalType) -> &'static str {
    match mode {
        TerminalType::Bash => "bash",
        TerminalType::Pi => "pi",
        TerminalType::Beeds => "beads",
    }
}

/// The toast's text and its tone, for a copy outcome.
///
/// The *words* stay on
/// [`CopyOutcome::toast`](crate::services::clipboard::CopyOutcome::toast),
/// next to the variants they describe, so the ladder cannot drift from the
/// state machine. All this decides is how loudly to say it.
fn describe_outcome(
    outcome: &crate::services::clipboard::CopyOutcome,
) -> (String, crate::state::toast::Tone) {
    let tone = if outcome.is_failure() {
        crate::state::toast::Tone::Bad
    } else {
        crate::state::toast::Tone::Good
    };
    (outcome.toast(), tone)
}

/// A copy handed to the sink, with the two numbers the toast needs if the sink is
/// slow: how much was asked for, and how long we have been waiting.
#[derive(Debug)]
pub(super) struct PendingCopy {
    receipt: crate::services::clipboard::Receipt,
    // Written at request time, read by the request itself: the toast is sent from
    // the local `chars` before the request is parked here, so the field is a
    // receipt the app does not consult. Kept (looprs-pdl.10, in progress) rather
    // than deleted, because the deferred-toast variant of this path reads it.
    #[allow(dead_code)]
    chars: crate::services::clipboard::Chars,
    sent_at: Instant,
}

impl App {
    // ───────────────────── drag selection (looprs-pdl.9) ─────────────────────

    /// The live drag selection, as the frame sees it.
    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    /// What the live selection would copy, as characters (ADR-0004 R14, R15).
    ///
    /// Empty when nothing is selected, and empty for a selection of blanks —
    /// which is R13's "a selection whose value is empty after trimming copies
    /// nothing and says nothing" with half its work already done: the value is
    /// resolved from the store's characters, not from the rows it was drawn
    /// over. The transport, the count and the toast are looprs-pdl.10's; the
    /// *shape* is here first so that "what was selected" has one definition in
    /// the tree and the copy ticket cannot invent a second one under pressure.
    ///
    /// Live on both copy paths today — the drag release and the keyboard chord
    /// (looprs-pdl.13) go through here via [`App::copy_selection`].
    pub fn selection_paste(&self) -> String {
        self.selection.paste(self.scrollback().rows())
    }

    // ──────────────────── select-to-copy (looprs-pdl.10) ────────────────────

    /// Replace the clipboard sink.
    ///
    /// Called once from `main` with the configured sink. The default is
    /// [`Noop`](crate::services::clipboard::Noop), which is what keeps a unit
    /// test from being a clipboard test by accident.
    pub fn set_clipboard(&mut self, clipboard: Arc<dyn crate::services::clipboard::Clipboard>) {
        self.clipboard = clipboard;
    }

    /// Turn the automatic (drag-release) copy on or off — `LOOPRS_COPY_ON_SELECT`.
    pub fn set_copy_on_select(&mut self, on: bool) {
        self.copy_on_select = on;
    }

    /// Replace the whole-transcript dump sink.
    ///
    /// Called once from `main`. The default is
    /// [`Noop`](crate::services::transcript_file::Noop), which keeps a unit test
    /// from being a filesystem test by accident, exactly as
    /// [`Self::set_clipboard`] keeps one from being a clipboard test.
    pub fn set_transcript_sink(
        &mut self,
        sink: Arc<dyn crate::services::transcript_file::TranscriptSink>,
    ) {
        self.transcript_sink = sink;
    }

    /// Replace the journal, and hand it to every view that already exists.
    ///
    /// Called once from `main`, before any session runs. Views created after this
    /// get it from [`Self::view_mut`]; views that already existed get it here,
    /// which is what keeps "every transcript goes to the same journal for the
    /// whole run" true regardless of the order the App and the views were built
    /// in (the tests build views first more often than not).
    pub fn set_journal(&mut self, journal: Arc<dyn crate::services::journal::Journal>) {
        self.journal = journal.clone();
        for v in self.views.values_mut() {
            v.set_journal(journal.clone());
        }
    }

    /// The journal this run is using — for the shutdown drain, and for anything
    /// that has to say where the transcript went.
    pub fn journal(&self) -> Arc<dyn crate::services::journal::Journal> {
        self.journal.clone()
    }

    /// The toast currently on screen.
    pub fn toast(&self) -> Option<&crate::state::toast::Toast> {
        self.toast.as_ref()
    }

    /// The one door every copy goes through: **count it, queue it, never wait on
    /// it**.
    ///
    /// Three rules are enforced here rather than at the call sites, because a
    /// rule enforced at two call sites is a rule that the second call site
    /// forgets:
    ///
    /// * **R13** — a value that is empty after trimming copies nothing and says
    ///   nothing, and leaves whatever was on the clipboard before alone. A blank
    ///   drag across the gutter is not content;
    /// * **R12** — nothing is copied while a full-screen child holds the screen.
    ///   The pixels under the pointer are the child's, so a selection made from
    ///   our last frame would describe text that is not on screen;
    /// * **R19** — the count is taken here, from this exact string, once. Every
    ///   toast that follows carries *that* number, so `Copied 44 characters`
    ///   cannot describe a different 44 than the one that went out.
    ///
    /// Returns whether a copy was queued. A `false` return means nothing
    /// happened at all: no write, no toast, no clipboard change.
    pub fn copy_text(&mut self, text: String) -> bool {
        if self.passthrough() {
            tracing::debug!("copy declined: a full-screen child holds the screen");
            return false;
        }
        if text.trim().is_empty() {
            tracing::debug!("copy skipped: the selection is blank (R13)");
            return false;
        }
        let chars = crate::services::clipboard::Chars::of(&text);
        let receipt = self.clipboard.copy(text);
        self.pending_copy = Some(PendingCopy {
            receipt,
            chars,
            sent_at: self.clock,
        });
        self.dirty = true;
        // Poll once immediately. A sink that answers on the spot — `Noop`,
        // `RecordingClipboard` — therefore shows its toast in the same breath
        // it was asked, which is what makes the latency budget assertable
        // without a test that sleeps. A queued sink answers on the next tick.
        let now = self.clock;
        self.poll_copy(now);
        true
    }

    /// The copy the selection makes, whatever made the selection.
    ///
    /// Used by the drag-release path and by the keyboard copy (looprs-pdl.13),
    /// which is the point: both go through the same sink, the same count and
    /// the same toast, so there is no second copy path with a second opinion
    /// about what "Copied" means.
    pub fn copy_selection(&mut self) -> bool {
        let text = self.selection_paste();
        self.copy_text(text)
    }

    // ─────────────── the keyboard copy (looprs-pdl.13) ───────────────

    /// Resolve a chord's target against the mode on screen, without copying.
    ///
    /// `Err(reason)` is the refusal the toast says; it is produced here rather
    /// than at the call sites so that every target refuses in the same words and a
    /// new target cannot forget to refuse at all.
    fn resolve(&self, target: CopyTarget) -> Result<String, String> {
        let Some(view) = self.active_view() else {
            return Err(format!("no {} transcript yet", self.active.label()));
        };
        let t = &view.transcript;
        let found = match target {
            // The agentic modes' answer. Bash has no answers of its own, and the
            // refusal says so and points at the chord that does work here,
            // because "nothing to copy" with no alternative is a dead end and a
            // user who just learned a chord cannot get past a dead end.
            CopyTarget::Answer => t.last_answer().map(str::to_string).ok_or_else(|| {
                if self.active == TerminalType::Bash {
                    "Bash mode has no answers \u{2014} Ctrl-S o copies the last command's output"
                        .to_string()
                } else {
                    "this session has not answered anything yet".to_string()
                }
            })?,
            // Same question, mode-relative: the shell's last block in Bash, the
            // last tool card's result in Pi and Beads. Named "the last command's
            // output" in both because that is what the user asked, and both
            // answers satisfy it.
            CopyTarget::CommandOutput => {
                if self.active == TerminalType::Bash {
                    match t.last_command_output() {
                        Some(s) => s,
                        // Two different nothings, and the user is holding one of
                        // them: a shell that has never been asked, and a command
                        // that ran and said nothing.
                        None if !t.has_command_boundary() => {
                            return Err("no command has run in this shell yet".to_string());
                        }
                        None => return Err("the last command produced no output".to_string()),
                    }
                } else {
                    t.last_tool_output()
                        .map(str::to_string)
                        .ok_or_else(|| "no tool has finished in this session yet".to_string())?
                }
            }
            // The selection is resolved by the selection itself; "no selection"
            // is the refusal, and the keyboard-selection feature (which did not
            // land in this ticket) will feed this same target when it does.
            CopyTarget::Selection => {
                if !self.selection.is_live() {
                    return Err(if self.passthrough() {
                        "a full-screen program holds the screen".to_string()
                    } else {
                        "nothing is selected \u{2014} Ctrl-S a copies the last answer, \u{b7} Ctrl-S o the last output"
                            .to_string()
                    });
                }
                self.selection_paste()
            }
        };
        if found.trim().is_empty() {
            // R13 again, on the keyboard side: an empty target is not a copy, and
            // the difference between this and the refusal above is that the user
            // asked for a thing that exists and happens to be blank.
            return Err(format!("{} is blank", target.label()));
        }
        Ok(found)
    }

    /// The copy a chord asks for: resolve the target, hand it to the clipboard
    /// sink, report through the same toast the mouse uses.
    ///
    /// A refusal is *said* here rather than returned as an error to the keystroke
    /// handler for the same reason the copy is not a `Result`: the user asked for
    /// something with a chord and the only acceptable outcomes are "it copied" and
    /// "it did not, and here is why". Note this is where the keyboard path is
    /// allowed to be loud: R13's "a blank selection copies nothing and says
    /// nothing" governs the mouse, where the selection was never a request. A
    /// chord *is* a request, so silence would be the bug.
    pub fn copy_target(&mut self, target: CopyTarget) -> bool {
        match self.resolve(target) {
            Ok(text) => self.copy_text(text),
            Err(reason) => {
                self.show_toast(
                    &format!("Nothing copied: {reason}"),
                    crate::state::toast::Tone::Bad,
                );
                false
            }
        }
    }

    /// `Ctrl-S t`: the whole transcript, to a file.
    ///
    /// The same shape as a clipboard copy with a different sink — resolve, hand
    /// over, poll the receipt, toast the result — so the escape hatch is not a
    /// second implementation of "do a thing with text and report it".
    pub fn dump_transcript(&mut self) -> bool {
        if self.passthrough() {
            // R12, and the reason the chord is not even read in this state: the
            // transcript is not what is on screen, so writing "the transcript"
            // would be writing a description of a frame the user cannot see.
            self.show_toast(
                "Nothing written: a full-screen program holds the screen",
                crate::state::toast::Tone::Bad,
            );
            return false;
        }
        let Some(view) = self.active_view() else {
            self.show_toast(
                &format!("Nothing written: no {} transcript yet", self.active.label()),
                crate::state::toast::Tone::Bad,
            );
            return false;
        };
        let text = view.transcript.plain_text();
        if text.trim().is_empty() {
            self.show_toast(
                "Nothing written: the transcript is empty",
                crate::state::toast::Tone::Bad,
            );
            return false;
        }
        let mode = self.active;
        let chars = crate::services::clipboard::Chars::of(&text);
        let receipt = self.transcript_sink.dump(mode_label(mode), text);
        self.pending_dump = Some(PendingDump {
            receipt,
            chars,
            sent_at: self.clock,
        });
        self.dirty = true;
        // Same immediate-poll trick as the clipboard: a sink that answers on the
        // spot paints its toast in the same breath it was asked, which is what
        // makes the latency assertable without a test that sleeps.
        let now = self.clock;
        self.poll_dump(now);
        true
    }

    /// `Ctrl-S`: arm or disarm the copy prefix.
    ///
    /// A second `Ctrl-S` cancels rather than re-arming, because a user who
    /// discovers the prefix is live and wants out of it should not have to find a
    /// *third* key to do it.
    ///
    /// Declined while a full-screen child holds the screen — and declined by
    /// swallowing the byte, which is the point of claiming `Ctrl-S` at all (see
    /// the XOFF note in [`Self::on_key`]). No toast here: the child owns the
    /// screen, so a toast cannot be seen, and the child does not get to be frozen
    /// either.
    pub(super) fn copy_prefix(&mut self) {
        if self.copy_chord == CopyChord::Armed {
            self.copy_chord = CopyChord::Off;
            tracing::debug!("copy chord disarmed");
            return;
        }
        if self.passthrough() {
            tracing::debug!(
                "Ctrl-S swallowed and not forwarded: looprs owns Ctrl-Q, so XOFF sent \u{2014} \u{2018}?\u{2019} lists the family"
            );
            return;
        }
        self.copy_chord = CopyChord::Armed;
        self.copy_chord_at = self.clock;
        // The hint is the discoverability path: every chord in the table is
        // reachable from inside the app by someone who has never opened a docs
        // file, and it is on screen for exactly as long as the prefix lives
        // ([`COPY_CHORD_WINDOW`] is [`TOAST_TTL`]), so the hint going away *is*
        // the window closing rather than a claim about it.
        self.show_toast(
            &crate::session::view::copy_chord_hint(),
            crate::state::toast::Tone::Good,
        );
    }

    /// Collect the result of the dump in flight, if there is one.
    ///
    /// The late-failure rule is the clipboard's, copied deliberately: a write
    /// into a volume that stopped answering must not leave the user believing
    /// their transcript is on disk. At [`DUMP_TIMEOUT`] the receipt is abandoned
    /// so a much later answer cannot repaint success over the failure the user
    /// was already given.
    pub fn poll_dump(&mut self, now: Instant) {
        let Some(pending) = self.pending_dump.as_ref() else {
            return;
        };
        if let Some(outcome) = pending.receipt.poll() {
            let tone = if outcome.is_failure() {
                crate::state::toast::Tone::Bad
            } else {
                crate::state::toast::Tone::Good
            };
            let text = outcome.toast();
            self.pending_dump = None;
            self.show_toast(&text, tone);
            return;
        }
        if now.saturating_duration_since(pending.sent_at)
            >= crate::services::transcript_file::DUMP_TIMEOUT
        {
            let text = crate::services::transcript_file::DumpOutcome::Failed {
                reason: format!(
                    "the disk did not answer in {}s \u{2014} the {} characters asked for are unconfirmed",
                    crate::services::transcript_file::DUMP_TIMEOUT.as_secs(),
                    crate::services::clipboard::thousands(pending.chars.get())
                ),
            }
            .toast();
            pending.receipt.abandon();
            self.pending_dump = None;
            self.show_toast(&text, crate::state::toast::Tone::Bad);
        }
    }

    /// Collect the result of the copy in flight, if there is one.
    ///
    /// The late failure is the case this exists for. A native helper parked on a
    /// wedged compositor and a `pbcopy` writing into an SSH connection that has
    /// stopped pumping both look exactly like a slow copy from here, and the one
    /// thing that must not happen is the user finding out at paste time, in a
    /// different application, that nothing was ever copied. So at
    /// [`COPY_TIMEOUT`] the receipt is abandoned — a much later answer cannot
    /// arrive and paint `Copied` over the failure the user was already told
    /// about — and the failure is shown.
    pub fn poll_copy(&mut self, now: Instant) {
        let Some(pending) = self.pending_copy.as_ref() else {
            return;
        };
        if let Some(outcome) = pending.receipt.poll() {
            let (text, tone) = describe_outcome(&outcome);
            self.pending_copy = None;
            self.show_toast(&text, tone);
            return;
        }
        if now.saturating_duration_since(pending.sent_at)
            >= crate::services::clipboard::COPY_TIMEOUT
        {
            let outcome = crate::services::clipboard::CopyOutcome::Failed {
                reason: format!(
                    "clipboard did not answer in {}s \u{2014} nothing confirmed",
                    crate::services::clipboard::COPY_TIMEOUT.as_secs()
                ),
            };
            let (text, tone) = describe_outcome(&outcome);
            pending.receipt.abandon();
            self.pending_copy = None;
            self.show_toast(&text, tone);
        }
    }

    /// Put a toast up. Replaces whatever was there (R21: replaced, not queued).
    pub(super) fn show_toast(&mut self, text: &str, tone: crate::state::toast::Tone) {
        tracing::debug!(toast = text, "toast");
        self.toast = Some(crate::state::toast::Toast::new(text, tone, self.clock));
        self.dirty = true;
    }

    /// The next key or click puts the toast away (R21).
    ///
    /// Deliberately not called for a wheel report or a drag motion: those are
    /// not "the user did something else", they are the user still looking at
    /// what this toast is about.
    pub fn dismiss_toast(&mut self) {
        if self.toast.take().is_some() {
            self.dirty = true;
        }
    }
}
