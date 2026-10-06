//! The full-screen seam (ADR-0001 Q2 rule 2, "the screen-buffer path").
//!
//! Bash mode gives the child a real pty, so `vim`, `less` and `htop` start without
//! complaint. Starting is not the problem; **showing** them is. A full-screen
//! program's frame is cursor addressing, clears and colour — a whole screen of
//! bytes with no line breaks in it — and looprs' transcript flushes a line at a
//! time. So the child paints, nothing terminates a line, and the pane sits frozen
//! on the previous frame (measured in `spikes/vim_fullscreen.py`: ~1 KB chunks
//! with zero linefeeds, no `--INSERT--`, no `~` filler, file never written).
//!
//! The ADR's answer is not to emulate a terminal — that is a much bigger project —
//! it is to **get out of the way**: while a child holds the screen, looprs copies
//! the child's bytes verbatim to the real terminal and stops drawing. The child's
//! own cursor addressing lands on the real screen, at the real size, because the
//! pty is sized to the window (rule 6).
//!
//! Three pieces, all here because they are one decision seen from three sides:
//!
//! * [`ScreenWatch`] — read the child's bytes and tell who owns the screen.
//!   Detects the alt-screen codes (`ESC[?1049h`/`?1047h`/`?47h`, and their `l`
//!   counterparts) plus the no-linefeed-cursor-addressed case that programs
//!   without an alt screen leave behind.
//! * [`tee`] — the passthrough itself: verbatim bytes, flushed, no interpretation.
//! * [`key_bytes`] — the other half of the deal. A program that owns the screen
//!   owns the keyboard too, and looprs' keyboard is *parsed* (`crossterm` gives
//!   us `KeyEvent`, not bytes), so a held screen needs the keystroke re-serialised
//!   to the bytes the terminal sent. This is what makes `Esc` be `Esc` for vim
//!   instead of `0x03`, without taking `Esc` away from the line command that needs
//!   it to mean interrupt.
//!
//! What is deliberately **not** here: a VT100 emulator. `ScreenWatch` recognises a
//! dozen sequences at the byte level to decide *who draws*; it never tracks cells,
//! scroll regions or wide characters, because nothing downstream needs that once
//! the child is drawing its own screen.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::sync::{Arc, Mutex};

/// A change in who owns the real terminal screen, read out of the child's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScreenChange {
    /// The child took the screen over. From here the UI must stop drawing and
    /// `tee` the child's bytes instead.
    ///
    /// `alt` says whether the takeover came from a real alt-screen switch
    /// (`ESC[?1049h`) rather than from the cursor-addressed heuristic. It is a
    /// promise, not a behaviour: `alt = true` means the child is obliged to
    /// restore the main screen when it leaves, which is why the release from an
    /// alt screen can be trusted to leave looprs' scrollback intact.
    Takeover { alt: bool },
    /// The child gave the screen back.
    Release,
}

/// One item in the observed stream, in stream order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Piece {
    /// Child output. Forward it exactly as you would have without a watcher.
    Out(Vec<u8>),
    /// Who owns the screen changed. Report it **now**, before the next `Out`, and
    /// after everything already emitted.
    Change(ScreenChange),
}

/// Reads a shell's byte stream and reports who owns the screen.
///
/// The ordering rule the caller must not get wrong, and which is the reason this
/// returns pieces rather than a bool:
///
/// * a `Takeover` is reported **before** the bytes that caused it — the terminal
///   has to be told to switch screens by the very bytes that follow, so the UI
///   must already be teeing when they arrive;
/// * a `Release` is reported **after** them — if the `ESC[?1049l` were held back
///   until the UI stopped teeing, the terminal would never leave the alt screen
///   and looprs would be drawing into a screen that is not showing.
///
/// Stateful because a sequence can straddle a read boundary, and the bytes of a
/// sequence that *might* turn out to be a takeover cannot be emitted until it is
/// decided: emitting `ESC[?104` in chunk one and discovering `9h` in chunk two
/// would put the alt-screen switch in the transcript instead of on the terminal,
/// which is the same silent-garbage failure this module exists to remove. So an
/// undecided tail is held in `pending` and comes out with the next call.
#[derive(Default)]
pub struct ScreenWatch {
    held: bool,
    /// Started the alt screen (and therefore owes a restore).
    alt: bool,
    /// Which alternate-screen code the switch used (`1049`, `1047`, `47`), so the
    /// leave that gets paid back is the one that matches the entry. `None` for a
    /// takeover the watcher only *inferred* (cursor addressing with no linefeeds),
    /// which switched nothing and therefore owes nothing.
    alt_code: Option<u16>,
    state: Scan,
    /// The bytes still being decided: everything read since the last piece was
    /// handed back. Held rather than emitted because an escape sequence at the end
    /// of a read may turn out to be a screen switch, and the run it belongs to is
    /// emitted *before* that announcement.
    run: Vec<u8>,
    /// Index into `run` of the `ESC` that started the sequence in flight.
    seq_start: usize,
}

#[derive(Default, PartialEq, Eq)]
enum Scan {
    #[default]
    Text,
    /// Saw `ESC`; the next byte says whether this is a CSI sequence.
    Esc,
    /// Inside `ESC [ … <final>`.
    Csi,
}

/// Longest plausible CSI sequence before we call it noise and stop watching it.
const SEQ_MAX: usize = 32;

impl ScreenWatch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_held(&self) -> bool {
        self.held
    }

    /// The alternate-screen code this watcher believes is currently switched on,
    /// if the switch it saw was an explicit one.
    ///
    /// This is the half of [`Self::alt`] that a leave sequence needs: `?1047h` is
    /// not put back by `?1049l`, and guessing is the difference between a screen
    /// that returns and a screen that changes again on the way out.
    pub fn alt_code(&self) -> Option<u16> {
        self.alt_code
    }

    /// Feed one read of the child's output; get the pieces back in order.
    pub fn observe(&mut self, bytes: &[u8]) -> Vec<Piece> {
        // The heuristic half (cursor addressing with no linefeeds) needs the shape
        // of the whole chunk, so ask before slicing it up.
        let has_lf = bytes.contains(&b'\n');
        let mut out = Vec::new();
        let mut run = std::mem::take(&mut self.run);

        for &b in bytes {
            match self.state {
                Scan::Text => {
                    run.push(b);
                    if b == ESC {
                        self.state = Scan::Esc;
                        self.seq_start = run.len() - 1;
                    }
                }
                Scan::Esc => {
                    run.push(b);
                    self.state = if b == b'[' { Scan::Csi } else { Scan::Text };
                }
                Scan::Csi => {
                    run.push(b);
                    let final_byte = (0x40..=0x7e).contains(&b);
                    if final_byte {
                        let change = classify(&run[self.seq_start..], has_lf, self.held);
                        // Which alternate-screen code this sequence switched on,
                        // read out of the same bytes now, before the branch below
                        // starts draining `run` out from under the slice.
                        let entered = match change {
                            Some(ScreenChange::Takeover { alt: true }) => {
                                alt_code_of(&run[self.seq_start..])
                            }
                            _ => None,
                        };
                        if let Some(change) = change {
                            match change {
                                ScreenChange::Takeover { alt } => {
                                    // Everything before the switch belongs to the
                                    // old owner; emit it first, then announce, and
                                    // keep the switch itself in the run so it is
                                    // the first thing teed under the new owner.
                                    let head: Vec<u8> = run.drain(..self.seq_start).collect();
                                    if !head.is_empty() {
                                        out.push(Piece::Out(head));
                                    }
                                    out.push(Piece::Change(ScreenChange::Takeover { alt }));
                                    self.held = true;
                                    self.alt = alt;
                                    self.alt_code = entered;
                                    // The run now starts at the sequence, so the
                                    // index it is addressed by moves to the front.
                                    self.seq_start = 0;
                                }
                                ScreenChange::Release => {
                                    // The leave sequence itself must reach the
                                    // terminal first — that byte is what puts the
                                    // main screen back.
                                    out.push(Piece::Out(std::mem::take(&mut run)));
                                    out.push(Piece::Change(ScreenChange::Release));
                                    self.held = false;
                                    self.alt = false;
                                    self.alt_code = None;
                                }
                            }
                        }
                        self.state = Scan::Text;
                    } else if run.len() - self.seq_start > SEQ_MAX {
                        // Not a sequence anyone can name; stop treating it as one.
                        self.state = Scan::Text;
                    }
                }
            }
        }

        // Only an unfinished escape sequence may survive a call. Everything else is
        // decided, and is output now — holding it back is what freezes a pane, so
        // this is the line that matters for latency.
        let hold_from = if self.state == Scan::Text {
            run.len()
        } else {
            self.seq_start
        };
        let tail = run.split_off(hold_from);
        if !run.is_empty() {
            out.push(Piece::Out(run));
        }
        // The tail starts at the `ESC` the sequence was measured from.
        self.seq_start = 0;
        self.run = tail;
        out
    }

    /// Whatever is still undecided, at the end of the stream (or of a command).
    ///
    /// An unfinished escape sequence is not a reason to lose bytes: hand them over
    /// as output and let the terminal make of them what it will.
    pub fn drain(&mut self) -> Vec<u8> {
        self.state = Scan::Text;
        std::mem::take(&mut self.run)
    }

    /// End the hold without the child having said so, and return the bytes needed
    /// to put the screen back.
    ///
    /// This is the command-boundary release: `vim` killed with `SIGKILL`, `less`
    /// closed by a signal, a program that exits without a leave sequence. The
    /// watcher cannot read a release that never came, so the caller says it — and
    /// the watcher has to be *reset* as part of saying it, or it keeps believing a
    /// dead child holds the screen and never reports the next one taking it.
    ///
    /// The returned bytes are the undecided tail, plus an alt-screen leave if the
    /// program went into one and died before paying it back. A real terminal leaves
    /// you stranded on the alternate screen in that case; paying the debt is what
    /// gets the main screen — and the scrollback with it — back.
    pub fn force_release(&mut self) -> Vec<u8> {
        let mut bytes = self.drain();
        if let Some(code) = self.alt_code {
            bytes.extend_from_slice(alt_leave(code));
        }
        self.held = false;
        self.alt = false;
        self.alt_code = None;
        bytes
    }
}

const ESC: u8 = 0x1b;

/// A complete CSI sequence, taken apart: whether it is a private (`?`) mode
/// sequence, the numbers it carries, and its final byte.
///
/// One parser for every reader of these bytes. `ScreenWatch`'s classifier and the
/// alternate-screen debt tracker below ask different questions of the same
/// sequences, and two hand-rolled parsers is how one of them starts answering a
/// question the other got wrong.
fn parse_csi(seq: &[u8]) -> Option<(bool, Vec<u16>, u8)> {
    // seq = ESC '[' <params> <final>
    if seq.len() < 3 || seq[0] != ESC || seq[1] != b'[' {
        return None;
    }
    let final_byte = seq[seq.len() - 1];
    let params = &seq[2..seq.len() - 1];
    let private = params.first() == Some(&b'?');
    let numeric = if private { &params[1..] } else { params };
    let nums: Vec<u16> = String::from_utf8_lossy(numeric)
        .split(';')
        .filter_map(|p| p.trim().parse::<u16>().ok())
        .collect();
    Some((private, nums, final_byte))
}

/// Classify a complete CSI sequence.
///
/// `has_lf` is the shape of the chunk this came from: the cursor-addressing
/// heuristic only fires for a chunk with **no linefeeds in it**, because that is
/// the measurable difference between a program that writes lines (which the
/// transcript serves fine) and a program that paints a screen (which it cannot).
fn classify(seq: &[u8], has_lf: bool, held: bool) -> Option<ScreenChange> {
    let (private, nums, final_byte) = parse_csi(seq)?;

    // The explicit signal: someone switched to the alternate screen. 1049 is the
    // xterm/vim/less/htop code; 1047 and 47 are its older spellings.
    if private && alt_screen_code(&nums).is_some() {
        return match final_byte {
            b'h' if !held => Some(ScreenChange::Takeover { alt: true }),
            b'l' if held => Some(ScreenChange::Release),
            _ => None,
        };
    }

    if held || private || has_lf {
        // Already passed through; a private mode set (`?25l` hide cursor, `?2004h`
        // bracketed paste) is not a screen; and a chunk that breaks lines is
        // output the line-oriented transcript can render.
        return None;
    }

    // The inferred signal: cursor addressing in a chunk with no linefeeds.
    let paint = match final_byte {
        // CUU — moving *up* in output only happens to redraw what is already there.
        b'A' => true,
        // CUP / HVP — absolute cursor position.
        b'H' | b'f' => true,
        // ED — clear display (2 = whole screen, 3 = whole screen + scrollback).
        b'J' => nums.first().copied().unwrap_or(0) >= 2,
        _ => false,
    };
    paint.then_some(ScreenChange::Takeover { alt: false })
}

/// The alternate-screen code among these parameters, if there is one.
fn alt_screen_code(nums: &[u16]) -> Option<u16> {
    nums.iter().copied().find(|n| matches!(n, 1049 | 1047 | 47))
}

/// Which alternate-screen code this sequence switched **on**.
///
/// `None` for anything that is not a private-mode set of one of the alt-screen
/// codes, including the matching reset: a `l` is somebody else paying a debt, not
/// one being taken on.
fn alt_code_of(seq: &[u8]) -> Option<u16> {
    let (private, nums, final_byte) = parse_csi(seq)?;
    if !private || final_byte != b'h' {
        return None;
    }
    alt_screen_code(&nums)
}

/// The bytes that put a given alternate-screen code back.
///
/// The leave has to be spelled the way the entry was: `?1047h` is not undone by
/// `?1049l`, and a mismatched leave is a screen that is still not the user's when
/// the app stops writing to it.
pub fn alt_leave(code: u16) -> &'static [u8] {
    match code {
        1047 => b"\x1b[?1047l",
        47 => b"\x1b[?47l",
        // 1049 is the spelling everything real uses, and the one that saves the
        // cursor along with the screen.
        _ => b"\x1b[?1049l",
    }
}

/// The alternate screen a full-screen child switched on and may never have given
/// back, seen from the bytes the passthrough wrote to the **real** terminal.
///
/// [`crate::teardown::Ledger`] tracks the modes *this app* switched on, and it is
/// the right owner of those. The alternate screen a `vim` entered is different in
/// one important way: the app did not switch it on, it *passed the byte through*.
/// That distinction is worth nothing to the user standing in front of the terminal
/// — either way the program on the glass is not the shell — and it is worth
/// everything to the exit path, because the only thing left that can write to the
/// terminal when the app goes down is the app.
///
/// So the debt is recorded where it can be seen honestly: from the tee'd bytes
/// themselves. `?1049h` going out makes us answerable for `?1049l`; seeing the
/// child's own `?1049l` go out through the same pipe discharges it. No question
/// is asked of the terminal, in this or any other direction: the bytes we wrote
/// are the whole record, exactly as [`crate::teardown`] requires of a mode.
///
/// Note what this is *not*: it is not the same fact as "a child currently holds
/// the screen" (`SessionEvent::ScreenHeld`). That is about who gets the next
/// frame. This is about who owes the terminal a leave sequence, and it outlives
/// the session that ran up the debt on purpose — a child killed with `SIGKILL`
/// never pays it, and the pump that would have carried its last words is already
/// gone by the time the app hands the terminal back.
#[derive(Clone, Default)]
pub struct ScreenDebt(Arc<Mutex<DebtInner>>);

#[derive(Default)]
struct DebtInner {
    /// A watcher pointed at the tee'd stream rather than at the child's own read,
    /// so the debt follows what actually reached the terminal. If the app was not
    /// teeing, the bytes never got there and no debt was incurred — which is the
    /// answer a `ScreenWatch` on the child's side could not give.
    watch: ScreenWatch,
    /// The alt-screen code still owed, and the only thing `restore` reads.
    owed: Option<u16>,
}

impl ScreenDebt {
    pub fn new() -> Self {
        Self::default()
    }

    /// The passthrough wrote `bytes` to the real terminal: update the debt.
    ///
    /// Only an explicit alt-screen switch counts, and only in the direction that
    /// takes it on. A cursor-addressing takeover (the watcher's inferred half)
    /// switched nothing the terminal needs a leave for, and treating it as one
    /// would put a `?1049l` on the wire for a screen nobody switched — the exact
    /// mistake the mode ledger exists to prevent.
    pub fn note_tee(&self, bytes: &[u8]) {
        let mut inner = self.lock();
        for piece in inner.watch.observe(bytes) {
            match piece {
                Piece::Change(ScreenChange::Takeover { alt: true }) => {
                    inner.owed = inner.watch.alt_code();
                }
                Piece::Change(ScreenChange::Release) => inner.owed = None,
                _ => {}
            }
        }
    }

    /// The alternate-screen code the app still owes the terminal, if any.
    pub fn outstanding(&self) -> Option<u16> {
        self.lock().owed
    }

    /// Discharge the debt without writing anything.
    ///
    /// For the caller that *paid* it with bytes of its own, which is the ordinary
    /// case: `vim` quits properly, its leave goes out through the tee, and the
    /// child's command-end [`ScreenWatch::force_release`] has already settled up.
    pub fn pay(&self) {
        self.lock().owed = None;
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, DebtInner> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Copy the child's bytes to the real terminal, verbatim, and flush.
///
/// Verbatim means no re-wrap, no re-style, no line buffering (ADR-0001 rule 1), and
/// it is the whole point: the child laid this out for the terminal it is attached
/// to, which is the terminal we are handing over. Flushing per write is not
/// politeness either — a buffered passthrough is a frame that never arrives, which
/// is the bug this path replaces.
///
/// A failed write is reported and swallowed rather than propagated: the child does
/// not need to die because the outer terminal had a bad day, and the pane being
/// dark is visible enough on its own.
pub fn tee(bytes: &[u8]) {
    let mut out = std::io::stdout();
    if let Err(e) = tee_to(&mut out, bytes) {
        tracing::warn!("full-screen passthrough write failed: {e}");
    }
}

/// The same copy, against any writer. Split out so a test can see the bytes that
/// would have gone to the terminal without a test run scribbling escape sequences
/// over the test harness's own output.
pub fn tee_to<W: std::io::Write>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    w.write_all(bytes)?;
    w.flush()
}

/// Re-serialize a parsed keystroke into the bytes the terminal sent for it.
///
/// A held screen must behave like the terminal the program believes it is driving,
/// and the program's input is bytes. `crossterm` parses those bytes into a
/// `KeyEvent` for the input box; a full-screen program needs them back. This is
/// the xterm/`xterm-256color` spelling, which is what `TERM` is set to.
///
/// `None` means "no byte string for this" (a release/repeat event we were not
/// asked about, a media key, a function-key spelling we do not claim). Dropping
/// such a key is correct — inventing a byte for it is not.
pub fn key_bytes(k: KeyEvent) -> Option<Vec<u8>> {
    if k.kind != KeyEventKind::Press {
        return None;
    }
    let mods = k.modifiers;
    let alt = mods.contains(KeyModifiers::ALT);
    // `Shift` is not encoded in these sequences except where it changes the byte
    // (an uppercase character, or Shift-Tab). `Control` never combines with `Alt`
    // in a way xterm can express, so control wins on its own and Ctrl-Alt is the
    // same Alt-prefixed key the terminal itself would have sent.
    let body: Vec<u8> = match k.code {
        KeyCode::Char(c) => {
            if mods.contains(KeyModifiers::CONTROL) {
                control_char(c)?
            } else {
                // `crossterm` has already applied Shift: the char it gives us *is*
                // the character the user got, so its UTF-8 bytes are the bytes the
                // terminal sent.
                let mut s = String::new();
                s.push(c);
                s.into_bytes()
            }
        }
        KeyCode::Enter => vec![0x0d],
        KeyCode::Tab => vec![0x09],
        KeyCode::BackTab => vec![0x1b, b'[', b'Z'],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Delete => vec![0x1b, b'[', b'3', b'~'],
        KeyCode::Insert => vec![0x1b, b'[', b'2', b'~'],
        KeyCode::Home => vec![0x1b, b'[', b'H'],
        KeyCode::End => vec![0x1b, b'[', b'F'],
        KeyCode::PageUp => vec![0x1b, b'[', b'5', b'~'],
        KeyCode::PageDown => vec![0x1b, b'[', b'6', b'~'],
        KeyCode::Left => vec![0x1b, b'[', b'D'],
        KeyCode::Right => vec![0x1b, b'[', b'C'],
        KeyCode::Up => vec![0x1b, b'[', b'A'],
        KeyCode::Down => vec![0x1b, b'[', b'B'],
        KeyCode::Esc => vec![0x1b],
        KeyCode::F(n) => return Some(function_key(n, alt)),
        _ => return None,
    };
    if alt {
        let mut v = vec![0x1b];
        v.extend(body);
        Some(v)
    } else {
        Some(body)
    }
}

/// Ctrl-<c> as the byte the line discipline reads.
fn control_char(c: char) -> Option<Vec<u8>> {
    let up = c.to_ascii_uppercase();
    let b = match up {
        'A'..='Z' => (up as u8) - 0x40,
        '@' | ' ' | '2' => 0x00,
        '[' | '3' => 0x1b,
        '\\' | '4' => 0x1c,
        ']' | '5' => 0x1d,
        '^' | '6' => 0x1e,
        '_' | '7' => 0x1f,
        '8' | '?' => 0x7f,
        _ => return None,
    };
    Some(vec![b])
}

/// F-keys, xterm spelling: `ESC O P`..`ESC O S` for F1..F4, then the
/// `ESC [ <n> ~` codes for F5..F12. The numbers are not a formula — F5 is 15 and
/// F6 is 17 — so they are spelled out, and anything above F12 is not claimed
/// rather than guessed at.
fn function_key(n: u8, alt: bool) -> Vec<u8> {
    let base: Vec<u8> = match n {
        1 => b"\x1bOP".to_vec(),
        2 => b"\x1bOQ".to_vec(),
        3 => b"\x1bOR".to_vec(),
        4 => b"\x1bOS".to_vec(),
        5 => b"\x1b[15~".to_vec(),
        6 => b"\x1b[17~".to_vec(),
        7 => b"\x1b[20~".to_vec(),
        8 => b"\x1b[21~".to_vec(),
        9 => b"\x1b[23~".to_vec(),
        10 => b"\x1b[24~".to_vec(),
        11 => b"\x1b[25~".to_vec(),
        12 => b"\x1b[26~".to_vec(),
        _ => vec![],
    };
    if alt && !base.is_empty() {
        let mut v = vec![0x1b];
        v.extend(base);
        v
    } else {
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;

    fn s(x: &str) -> Vec<u8> {
        x.as_bytes().to_vec()
    }

    /// The watcher remembers *which* alternate screen it saw, so the leave that
    /// gets paid back matches the entry instead of guessing at it.
    #[test]
    fn the_watcher_remembers_which_alt_screen_was_switched() {
        for code in [1049u16, 1047, 47] {
            let mut w = ScreenWatch::new();
            w.observe(format!("\x1b[?{code}h").as_bytes());
            assert_eq!(
                w.alt_code(),
                Some(code),
                "{code} was switched on and must be remembered"
            );
            w.observe(format!("\x1b[?{code}l").as_bytes());
            assert_eq!(w.alt_code(), None, "{code} was given back");
        }
    }

    /// A takeover inferred from cursor addressing switched nothing, so nothing is
    /// remembered and `force_release` invents no leave bytes.
    #[test]
    fn an_inferred_takeover_is_not_an_alt_screen() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[10;1H\x1b[2Jpainted without switching");
        assert!(w.is_held(), "the heuristic still holds the screen");
        assert_eq!(w.alt_code(), None, "but nothing was switched on");
        let forced = w.force_release();
        assert!(
            !String::from_utf8_lossy(&forced).contains("1049")
                && !String::from_utf8_lossy(&forced).contains("1047"),
            "no leave for a screen nobody switched: {forced:?}"
        );
    }

    /// `force_release` pays with the code that was entered, not with a literal.
    #[test]
    fn a_forced_release_pays_the_code_it_remembered() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[?1047h");
        let forced = w.force_release();
        assert_eq!(
            String::from_utf8_lossy(&forced),
            "\x1b[?1047l",
            "1047 goes back as 1047"
        );
        assert_eq!(w.alt_code(), None, "and the debt is settled");
    }

    /// The debt tracks what the passthrough actually wrote: `h` takes it on, the
    /// child's own `l` discharges it.
    #[test]
    fn the_debt_follows_the_bytes_the_passthrough_wrote() {
        let d = ScreenDebt::new();
        assert_eq!(d.outstanding(), None);
        d.note_tee(b"\x1b[?1049hframe");
        assert_eq!(d.outstanding(), Some(1049));
        // Still on the child's screen a chunk later; the debt did not fade.
        d.note_tee(b"more paint");
        assert_eq!(d.outstanding(), Some(1049));
        d.note_tee(b"\x1b[?1049l");
        assert_eq!(d.outstanding(), None, "the child paid its own way out");
    }

    /// A sequence split across two tees is still one switch — the same straddle
    /// the watcher exists for, now feeding the debt instead of the drawing path.
    #[test]
    fn a_switch_split_across_two_tees_is_still_one_switch() {
        let d = ScreenDebt::new();
        d.note_tee(b"paint\x1b[?104");
        assert_eq!(d.outstanding(), None, "not decided yet");
        d.note_tee(b"9hmore");
        assert_eq!(d.outstanding(), Some(1049), "decided, and owed");
    }

    /// Paying without writing is the seam for "somebody else's bytes already left
    /// the screen" — the command-boundary release in the Bash session.
    #[test]
    fn paying_the_debt_by_hand_leaves_nothing_owed() {
        let d = ScreenDebt::new();
        d.note_tee(b"\x1b[?1049h");
        d.pay();
        assert_eq!(d.outstanding(), None);
    }

    /// Each alt-screen code leaves with its own spelling.
    #[test]
    fn every_alt_screen_code_names_its_own_leave() {
        assert_eq!(alt_leave(1049), b"\x1b[?1049l");
        assert_eq!(alt_leave(1047), b"\x1b[?1047l");
        assert_eq!(alt_leave(47), b"\x1b[?47l");
        // An unknown code still gets the one spelling that is always safe to send
        // rather than nothing at all.
        assert_eq!(alt_leave(9999), b"\x1b[?1049l");
    }

    /// Alt screen in, alt screen out, with the ordering rule held: the takeover is
    /// reported before the `ESC[?1049h` (so the teeing starts with the byte that
    /// switches screens) and the release after the `ESC[?1049l` (so that byte is
    /// not held back and the main screen actually comes back).
    #[test]
    fn alt_screen_takeover_and_release_are_ordered_around_the_switch_bytes() {
        let mut w = ScreenWatch::new();
        let pieces = w.observe(b"\x1b[?1049hpainted screen\x1b[?1049l");
        assert_eq!(
            pieces,
            vec![
                Piece::Change(ScreenChange::Takeover { alt: true }),
                Piece::Out(s("\x1b[?1049hpainted screen\x1b[?1049l")),
                Piece::Change(ScreenChange::Release),
            ]
        );
        assert!(!w.is_held());
    }

    /// The bytes of a held session must still be forwarded, and forwarded in order —
    /// dropping or reordering them is how a screen ends up half painted.
    #[test]
    fn while_held_everything_is_output_and_nothing_is_re_reported() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[?1049h");
        let pieces = w.observe(b"line one\r\nline two\x1b[24;1Hstatus");
        assert_eq!(
            pieces
                .iter()
                .filter(|p| matches!(p, Piece::Change(_)))
                .count(),
            0,
            "a held screen does not keep announcing itself"
        );
        let joined: Vec<u8> = pieces
            .iter()
            .flat_map(|p| match p {
                Piece::Out(b) => b.clone(),
                Piece::Change(_) => vec![],
            })
            .collect();
        assert_eq!(joined, s("line one\r\nline two\x1b[24;1Hstatus"));
    }

    /// A takeover sequence split across two reads is the reason the undecided tail
    /// cannot be emitted early: half an alt-screen switch in the transcript is
    /// worse than no switch at all.
    #[test]
    fn a_takeover_split_across_reads_is_still_a_takeover() {
        let mut w = ScreenWatch::new();
        let a = w.observe(b"text\x1b[?104");
        assert_eq!(
            a,
            vec![Piece::Out(s("text"))],
            "the half sequence is held, not emitted"
        );
        assert!(!w.is_held());
        let b = w.observe(b"9hpaint");
        assert_eq!(
            b,
            vec![
                Piece::Change(ScreenChange::Takeover { alt: true }),
                Piece::Out(s("\x1b[?1049hpaint")),
            ]
        );
        assert!(w.is_held());
    }

    /// Same for the release: the leave sequence cannot be split into "half on the
    /// terminal, half in the transcript".
    #[test]
    fn a_release_split_across_reads_still_leaves_the_screen() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[?1049h");
        // "frame" is decided output and goes now; the half sequence does not.
        assert_eq!(
            w.observe(b"frame\x1b[?104"),
            vec![Piece::Out(s("frame"))],
            "the undecided tail is held back"
        );
        let after = w.observe(b"9l after");
        assert_eq!(
            after,
            vec![
                Piece::Out(s("\x1b[?1049l")),
                Piece::Change(ScreenChange::Release),
                Piece::Out(s(" after")),
            ],
            "the leave bytes reach the terminal before the release is reported"
        );
        assert!(!w.is_held());
    }

    /// The legacy spellings, because `TERM` and terminfo vary and a missed code is
    /// an invisible failure.
    #[test]
    fn the_older_alt_screen_codes_are_alt_too() {
        for code in ["\x1b[?1047h", "\x1b[?47h"] {
            let mut w = ScreenWatch::new();
            assert_eq!(
                w.observe(code.as_bytes()),
                vec![
                    Piece::Change(ScreenChange::Takeover { alt: true }),
                    Piece::Out(code.as_bytes().to_vec()),
                ],
                "{code} is an alt screen, announced before the bytes that make it one"
            );
        }
        for code in ["\x1b[?1047l", "\x1b[?47l"] {
            let mut w = ScreenWatch::new();
            w.observe(b"\x1b[?1047h");
            assert!(
                w.observe(code.as_bytes())
                    .contains(&Piece::Change(ScreenChange::Release)),
                "{code} leaves the alt screen"
            );
        }
    }

    /// The heuristic: vim's measured shape — a chunk of cursor addressing with
    /// **no linefeeds** — takes the screen even without an alt-screen code.
    #[test]
    fn cursor_addressed_output_with_no_linefeeds_takes_the_screen() {
        for chunk in [
            "\x1b[24;1Hstatus line",
            "\x1b[H\x1b[2J",
            "second half\x1b[3A",
            "row\x1b[10;40f",
        ] {
            let mut w = ScreenWatch::new();
            let pieces = w.observe(chunk.as_bytes());
            assert!(
                pieces.contains(&Piece::Change(ScreenChange::Takeover { alt: false })),
                "{chunk:?} paints a screen"
            );
        }
    }

    /// What the heuristic must **not** fire on: ordinary output. Colour is the
    /// common case (`ls --color`, `git log`), and mode sets like "hide cursor" or
    /// "bracketed paste" are not screens.
    #[test]
    fn ordinary_output_never_takes_the_screen() {
        for chunk in [
            "\x1b[32mgreen\x1b[0m no newline either",
            "\x1b[?25lhiding the cursor is not a screen",
            "\x1b[?2004h",
            "\x1b[Ktrailing clear, still a line",
            "a line with \x1b[1mbold\x1b[0m and a newline\n",
        ] {
            let mut w = ScreenWatch::new();
            let pieces = w.observe(chunk.as_bytes());
            assert!(
                !pieces.iter().any(|p| matches!(p, Piece::Change(_))),
                "{chunk:?} is not a screen: {pieces:?}"
            );
        }
    }

    /// A chunk that breaks lines is output the transcript can show, even if it
    /// contains cursor addressing — which is why the shape of the whole chunk is
    /// part of the rule and not just the sequence.
    #[test]
    fn a_chunk_with_linefeeds_is_not_a_paint_even_when_it_addresses() {
        let mut w = ScreenWatch::new();
        let pieces = w.observe(b"\x1b[24;1Hstatus\nthen a normal line\n");
        assert!(
            pieces.iter().all(|p| matches!(p, Piece::Out(_))),
            "line-oriented output stays output: {pieces:?}"
        );
        assert!(!w.is_held());
    }

    /// Bytes held for a decision are never lost: whatever is undecided at the end
    /// of a stream comes back as output.
    #[test]
    fn undecided_bytes_are_drained_not_dropped() {
        let mut w = ScreenWatch::new();
        // The decided prefix ("tail") went out with the call; the half sequence is
        // what is left to drain.
        assert_eq!(w.observe(b"tail\x1b[?104"), vec![Piece::Out(s("tail"))]);
        assert_eq!(w.drain(), s("\x1b[?104"));
        assert!(w.drain().is_empty(), "and only once");
    }

    /// After a release the watcher is usable again: the next program is a fresh
    /// takeover rather than an ignored one.
    #[test]
    fn the_watch_rearms_after_a_release() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[?1049h\x1b[?1049l");
        assert!(
            w.observe(b"\x1b[?1049h")
                .contains(&Piece::Change(ScreenChange::Takeover { alt: true })),
            "a second program takes the screen too"
        );
    }

    /// A program that dies inside the alt screen never sends its leave sequence, and
    /// a terminal left on the alternate screen shows a dead program until the
    /// terminal itself is restarted. The command boundary pays the debt: the
    /// release the program owed is emitted on its way out.
    #[test]
    fn a_forced_release_pays_an_unpaid_alt_screen_leave() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[?1049h painting");
        assert!(w.is_held());
        assert_eq!(w.force_release(), s("\x1b[?1049l"));
        assert!(!w.is_held());
    }

    /// A paint hold has nothing to give back — no switch was ever made, so the main
    /// screen is the only screen there is.
    #[test]
    fn a_forced_release_from_a_paint_hold_owes_nothing() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[24;1Hredrawn");
        assert!(w.is_held());
        assert!(w.force_release().is_empty());
        assert!(!w.is_held());
    }

    /// And the watcher is armed again afterwards: the **next** program must still
    /// be able to take the screen. Leaving `held` set is the one mistake here that
    /// turns one dead program into a permanently dead screen path.
    #[test]
    fn a_forced_release_rearms_the_watcher() {
        let mut w = ScreenWatch::new();
        w.observe(b"\x1b[?1049h");
        w.force_release();
        assert!(
            w.observe(b"\x1b[?1049h")
                .contains(&Piece::Change(ScreenChange::Takeover { alt: true })),
            "a second program still gets its screen"
        );
    }

    // ---------------- the passthrough write ----------------
    /// The passthrough is a copy, not a rendition: what the child wrote is what
    /// goes out, byte for byte, and it is flushed rather than buffered — a frame
    /// sitting in a userspace buffer is a frame the user cannot see.
    #[test]
    fn the_passthrough_copies_verbatim_and_flushes() {
        let mut buf: Vec<u8> = Vec::new();
        let payload = b"\x1b[?1049h\x1b[H\x1b[2Jscreen\x1b[?1049l";
        crate::screen::tee_to(&mut buf, payload).unwrap();
        assert_eq!(buf, payload, "not one byte changed on the way through");
    }

    // ---------------- keystrokes ----------------

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    /// The whole reason this exists: inside vim, `Esc` is the key that leaves
    /// insert mode, and `0x03` is not it.
    #[test]
    fn esc_is_esc_and_not_a_signal() {
        assert_eq!(
            key_bytes(key(KeyCode::Esc, KeyModifiers::NONE)),
            Some(vec![0x1b])
        );
    }

    #[test]
    fn ordinary_keys_become_the_bytes_the_terminal_would_have_sent() {
        assert_eq!(
            key_bytes(key(KeyCode::Char('i'), KeyModifiers::NONE)),
            Some(b"i".to_vec())
        );
        assert_eq!(
            key_bytes(key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(vec![0x0d]),
            "Enter is CR, which is what a terminal sends and what vim reads as <CR>"
        );
        assert_eq!(
            key_bytes(key(KeyCode::Backspace, KeyModifiers::NONE)),
            Some(vec![0x7f])
        );
        assert_eq!(
            key_bytes(key(KeyCode::Tab, KeyModifiers::NONE)),
            Some(vec![0x09])
        );
        for (code, want) in [
            (KeyCode::Up, b"\x1b[A".to_vec()),
            (KeyCode::Down, b"\x1b[B".to_vec()),
            (KeyCode::Right, b"\x1b[C".to_vec()),
            (KeyCode::Left, b"\x1b[D".to_vec()),
            (KeyCode::Home, b"\x1b[H".to_vec()),
            (KeyCode::End, b"\x1b[F".to_vec()),
            (KeyCode::PageUp, b"\x1b[5~".to_vec()),
            (KeyCode::PageDown, b"\x1b[6~".to_vec()),
            (KeyCode::Delete, b"\x1b[3~".to_vec()),
            (KeyCode::Insert, b"\x1b[2~".to_vec()),
        ] {
            assert_eq!(
                key_bytes(key(code, KeyModifiers::NONE)),
                Some(want.clone()),
                "{code:?} -> {want:?}"
            );
        }
    }

    /// Ctrl-C is still `0x03` on the wire, however the app chooses to route it.
    #[test]
    fn control_chords_become_control_bytes() {
        assert_eq!(
            key_bytes(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(vec![0x03])
        );
        assert_eq!(
            key_bytes(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            Some(vec![0x04]),
            "Ctrl-D stays EOF at the prompt"
        );
        assert_eq!(
            key_bytes(key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
            Some(vec![0x1a]),
            "and Ctrl-Z stays SIGTSTP — job control lives on the line discipline"
        );
        assert_eq!(
            key_bytes(key(KeyCode::Char('w'), KeyModifiers::CONTROL)),
            Some(vec![0x17]),
            "Ctrl-W is what vim reads for a delete-word / window command"
        );
    }

    /// Non-ASCII keystrokes are forwarded as their UTF-8 bytes, which is what a
    /// UTF-8 terminal sends.
    #[test]
    fn unicode_characters_survive_the_round_trip() {
        assert_eq!(
            key_bytes(key(KeyCode::Char('é'), KeyModifiers::NONE)),
            Some("é".as_bytes().to_vec())
        );
    }

    /// Alt is the escape prefix, exactly as xterm spells it.
    #[test]
    fn alt_prefixes_the_key() {
        assert_eq!(
            key_bytes(key(KeyCode::Char('f'), KeyModifiers::ALT)),
            Some(vec![0x1b, b'f'])
        );
    }

    #[test]
    fn function_keys_use_the_xterm_spelling() {
        assert_eq!(
            key_bytes(key(KeyCode::F(1), KeyModifiers::NONE)),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            key_bytes(key(KeyCode::F(5), KeyModifiers::NONE)),
            Some(b"\x1b[15~".to_vec()),
            "F5 is 15, not 12+5 — the codes are not a formula"
        );
        assert_eq!(
            key_bytes(key(KeyCode::F(12), KeyModifiers::NONE)),
            Some(b"\x1b[26~".to_vec())
        );
        assert_eq!(
            key_bytes(key(KeyCode::F(13), KeyModifiers::NONE)),
            Some(vec![]),
            "beyond F12 nothing is claimed rather than guessed"
        );
    }

    /// Repeat events are a caller decision, not a byte string; and keys with no
    /// honest spelling are refused rather than guessed at.
    #[test]
    fn keys_that_are_not_presses_are_not_bytes() {
        let mut k = key(KeyCode::Char('x'), KeyModifiers::NONE);
        k.kind = KeyEventKind::Release;
        assert_eq!(key_bytes(k), None);
    }
}
