//! The keyboard contract's vocabulary: who owns a key, and what pressing it means.
//!
//! [`KeySym`] is a keystroke reduced to what the table cares about; [`Effect`]
//! is the answer a row gives; [`Owner`] says which component gets the key in a
//! given [`ChordState`] — the transcript, the input box, the session, the app,
//! or nobody; [`ChordRow`] ties (mode, state, key) to an effect and a hint,
//! and [`copy_chord_hint`] renders the hint the status row shows while the copy
//! chord is armed.
//!
//! The rows themselves are in [`chord_table`](super::chord_table). Keeping the
//! vocabulary apart from 580 lines of data is what lets the rule tests — no
//! double owner, every mode covered, the `Esc` branch written down per mode —
//! sit next to the type they constrain instead of a page away from it.
use crate::session::view::chord_table::CHORD_TABLE;

use super::TerminalType;

// ─────────────────── the chord table (looprs-pdl.13) ───────────────────
//
/// Every key the app claims, in every mode, in every state of the screen, with
/// exactly one owner each. This is the table the ticket asks for, and it is data
/// rather than prose for one reason: the rules written against it are tests, and a
/// rule checked against a comment is a rule checked by nobody.
///
/// **The rows are the order of decision**, and [`App::on_key`] executes them in
/// that same order: the control chords first (`Ctrl-C`, `Ctrl-Q`, `Ctrl-S`, all
/// three claimed before anything else because all three have to survive every
/// other state), then the armed chord's second key, then the passthrough handover,
/// then `Esc`-clears-the-selection, then the scroll keys, then the input box.
/// A row appearing above another row is the same statement as "handles the key
/// first", which is what makes the no-shadowing audit mean something.
///
/// ```text
/// MODE        KEY              STATE          OWNER     EFFECT
/// Pi, Beads   Ctrl-C         plain          App       quit (no Cancel worth the name yet)
/// Bash        Ctrl-C         plain          Shell     0x03 → SIGINT the foreground group
/// all         Ctrl-Q         plain          App       quit
/// all         Ctrl-S         plain          App       arm the copy chord (help toast is the window)
/// all         a              armed          App       copy the last answer
/// all         o              armed          App       copy the last command's output
/// all         s              armed          App       copy the live selection
/// all         t              armed          App       write the whole transcript to a file
/// all         ?              armed          App       show the chord help
/// all         Esc            armed          App       cancel the chord, and nothing else
/// Bash        Ctrl-C         child holds    Shell     0x03 → SIGINT
/// Bash        Ctrl-Q         child holds    App       quit
/// Bash        Ctrl-S         child holds    App       swallowed: XOFF is never forwarded
/// Bash        any other      child holds    Child     forwarded as the bytes the terminal sent
/// all         Esc            selection live App       clear the selection, nothing sent
/// all         Esc            plain          session   the mode's cancel (ADR-0003)
/// all         Tab            plain          App       switch mode (clears selection, chord, throttle)
/// all         Shift-Tab      plain          Box       newline
/// all         Shift-Enter    plain          Box       newline
/// all         Enter          plain          Box       submit
/// all         PageUp         plain          App       one page up (unpins)
/// all         PageDown       plain          App       one page down (re-pins at the tail)
/// all         Home           plain          App       top of the transcript
/// all         End            plain          App       bottom, and re-pin
/// all         anything else  plain          Box       typing
/// ```
///
/// The three rules the audit below runs against it are the ticket's:
///
/// 1. **`Ctrl-C` is not copy.** In Bash mode it is SIGINT and stays SIGINT. No
///    row in this table may pair a `Ctrl-C` with a copy or a dump, in any mode.
/// 2. **Nothing added may shadow an existing binding in the mode it is added
///    to**, and the audit covers the modes' *differences*: the same key must
///    have exactly one owner per (mode, state), which is why `Ctrl-C` gets two
///    rows and why "the child holds the screen" is a state at all rather than a
///    footnote.
/// 3. **`Esc`'s branch is written down per state**, with the "was a selection
///    live?" question explicit — and, since looprs-pdl.13 added one, with the
///    pending-chord state ahead of it: `Esc` undoes the most recent thing the
///    user gave us, never something older and louder.
///
/// The state of the screen a chord is being decided in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChordState {
    /// We hold the screen, nothing is outstanding, no selection is live.
    Plain,
    /// A selection is live (made by the mouse, or by any other means).
    SelectionLive,
    /// `Ctrl-S` is outstanding and the next key is the target.
    ChordArmed,
    /// A full-screen child holds the real terminal (ADR-0001 Q2).
    ChildHolds,
}

/// Who ends up with the keystroke.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// looprs's own chord/scroll layer.
    App,
    /// The shell, by way of bytes down the pty.
    Shell,
    /// The mode's session (the cancel that ADR-0003 owns).
    Session,
    /// The input box.
    Box,
    /// A full-screen child program, forwarded verbatim.
    Child,
}

impl Effect {
    /// The noun `copy_chord_hint` pairs with this effect's sub-key: what arming
    /// the prefix and pressing this target gets you. Only the copy family has a
    /// hint; everything else in the table is not a thing the chord offers.
    fn hint(self) -> Option<&'static str> {
        match self {
            Effect::CopyAnswer => Some("answer"),
            Effect::CopyLastOutput => Some("last output"),
            Effect::CopySelection => Some("selection"),
            Effect::DumpTranscriptFile => Some("transcript to file"),
            Effect::CancelChord => Some("cancel"),
            _ => None,
        }
    }
}

/// What the app does with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effect {
    Quit,
    /// `0x03` to the pty master: SIGINT to the foreground process group.
    SigInt,
    /// The mode's own cancel.
    CancelRun,
    ClearSelection,
    ArmChord,
    CancelChord,
    CopyAnswer,
    CopyLastOutput,
    CopySelection,
    DumpTranscriptFile,
    ChordHelp,
    ScrollUp,
    ScrollDown,
    Top,
    Tail,
    SwitchMode,
    Newline,
    Submit,
    Typing,
    /// Forwarded raw to a full-screen child.
    Forwarded,
    /// Deliberately thrown away. The only one in the table is XOFF: we own
    /// `Ctrl-Q`, so a `Ctrl-S` we forwarded could stop a child the user then had
    /// no chord left to restart.
    Swallowed,
}

/// A key, spelled the way the table spells it, and constructible into the real
/// keystroke the driving tests send.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeySym {
    CtrlC,
    CtrlQ,
    CtrlS,
    /// The chord's second keys.
    TargetAnswer,
    TargetOutput,
    TargetSelection,
    TargetTranscript,
    Help,
    Esc,
    Tab,
    ShiftTab,
    Enter,
    ShiftEnter,
    PageUp,
    PageDown,
    Home,
    End,
    /// Any key not named above.
    AnyOther,
}

impl KeySym {
    /// The real keystroke this row talks about.
    ///
    /// #[allow(dead_code)] is below: the shipped binary never needs to turn a row
    /// back into a KeyEvent — the driving tests do, which is how the table is
    /// checked against the real handler instead of against a description of it.
    #[allow(dead_code)] // test seam: driven tests replay the table's chords
    /// For [`KeySym::AnyOther`] this is a plain `x`: the row's *meaning* is
    /// "anything unnamed", and `x` is the representative the driving tests use —
    /// chosen because nothing in the table names it, so a test that sends it is
    /// really sending the fallback and not a chord that happens to match.
    pub fn event(self) -> crossterm::event::KeyEvent {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
        match self {
            KeySym::CtrlC => KeyEvent::new(KeyCode::Char('c'), M::CONTROL),
            KeySym::CtrlQ => KeyEvent::new(KeyCode::Char('q'), M::CONTROL),
            KeySym::CtrlS => KeyEvent::new(KeyCode::Char('s'), M::CONTROL),
            KeySym::TargetAnswer => KeyEvent::new(KeyCode::Char('a'), M::NONE),
            KeySym::TargetOutput => KeyEvent::new(KeyCode::Char('o'), M::NONE),
            KeySym::TargetSelection => KeyEvent::new(KeyCode::Char('s'), M::NONE),
            KeySym::TargetTranscript => KeyEvent::new(KeyCode::Char('t'), M::NONE),
            KeySym::Help => KeyEvent::new(KeyCode::Char('?'), M::NONE),
            KeySym::Esc => KeyEvent::new(KeyCode::Esc, M::NONE),
            KeySym::Tab => KeyEvent::new(KeyCode::Tab, M::NONE),
            KeySym::ShiftTab => KeyEvent::new(KeyCode::BackTab, M::SHIFT),
            KeySym::Enter => KeyEvent::new(KeyCode::Enter, M::NONE),
            KeySym::ShiftEnter => KeyEvent::new(KeyCode::Enter, M::SHIFT),
            KeySym::PageUp => KeyEvent::new(KeyCode::PageUp, M::NONE),
            KeySym::PageDown => KeyEvent::new(KeyCode::PageDown, M::NONE),
            KeySym::Home => KeyEvent::new(KeyCode::Home, M::NONE),
            KeySym::End => KeyEvent::new(KeyCode::End, M::NONE),
            KeySym::AnyOther => KeyEvent::new(KeyCode::Char('x'), M::NONE),
        }
    }

    /// Is this a *copy* chord? Rule 1 is stated in terms of this: nothing whose
    /// effect moves text out of the app may be reachable on `Ctrl-C`.
    #[allow(dead_code)] // audit-only: rule 1 of the table audit is stated over this
    pub fn is_copy(self) -> bool {
        matches!(
            self,
            KeySym::TargetAnswer
                | KeySym::TargetOutput
                | KeySym::TargetSelection
                | KeySym::TargetTranscript
        )
    }
}

/// One row of [`CHORD_TABLE`].
///
/// The shipped binary reads `state`, `keys` and `does` — that is what
/// [`copy_chord_hint`] is built from. `mode`, `key`, `owner` and `note` exist so
/// the audit in `tests` can state the rules over the whole row; the table is the
/// artifact that gets checked, not a structure the key path walks.
#[allow(dead_code)] // four of the seven columns are the audit's, three are production's
pub struct ChordRow {
    /// The mode this row applies to. Every row names one mode explicitly: a row
    /// that said "all modes" would hide the fact that `Ctrl-C` does not mean the
    /// same thing in all of them.
    pub mode: TerminalType,
    /// The key.
    pub key: KeySym,
    /// How it is spelled in the table above, and in a failure message.
    pub keys: &'static str,
    /// The state this row applies in.
    pub state: ChordState,
    /// Who ends up with it.
    pub owner: Owner,
    /// What happens.
    pub does: Effect,
    /// Why, in one line — the part a reader of the code needs and a reader of
    /// the enum cannot carry.
    pub note: &'static str,
}

/// The hint shown while the copy chord is armed, and what `Ctrl-S ?` prints.
///
/// Read out of the table rather than written again: the string the user reads and
/// the table the audit checks are then the same data. A target added to the table
/// shows up in the hint by itself; a target that is not in the table cannot be
/// advertised.
pub fn copy_chord_hint() -> String {
    let mut parts: Vec<String> = Vec::new();
    for row in CHORD_TABLE
        .iter()
        .filter(|r| r.state == ChordState::ChordArmed)
    {
        let Some(noun) = row.does.hint() else {
            continue;
        };
        // The table spells each chord out in full (`"Ctrl-S a"`); the hint wants
        // the sub-key by itself next to the noun it gets you.
        let sub = row.keys.rsplit(' ').next().unwrap_or(row.keys);
        let item = format!("{sub} {noun}");
        if !parts.contains(&item) {
            parts.push(item);
        }
    }
    format!("Copy: {}", parts.join(" \u{b7} "))
}
