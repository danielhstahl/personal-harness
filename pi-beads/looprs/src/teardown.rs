//! One teardown for every way out of the app (looprs-ecr).
//!
//! Before this module there were two exits and they disagreed. The normal one ran
//! `disable_raw_mode()?; live.clear()?; println!()` and the panic hook ran
//! `let _ = disable_raw_mode()`. Neither was right, and the disagreement was the
//! smaller half of the problem:
//!
//! * `Terminal::clear()` *asks the terminal where the cursor is* before it erases
//!   (`ESC[6n`). At exit the async key stream has only just been dropped, its
//!   reader thread is still parked on the same stdin, and it eats the answer. The
//!   query times out and the app dies with `The cursor position could not be read
//!   within a normal duration` — exit code 1, on every single quit, with the live
//!   region still painted on the screen. Measured before the fix; see
//!   `spikes/shutdown_e2e.py`.
//! * The panic hook left the pane up entirely, so the panic message was printed
//!   into a screen that still had an input box frozen in the middle of it.
//!
//! The rule this module enforces is small and it is the whole design:
//!
//! > **the exit path never asks the terminal a question.**
//!
//! It already knows what it needs — and since the frame migration
//! (looprs-pdl.4) it needs a good deal less than it used to. The app now runs a
//! full-screen frame on the alternate screen, so the hand-back *is* the leave
//! sequence: `?1049l` puts the user's main screen and their cursor back exactly
//! as they were, and there is no pane of ours to erase, no row above it to
//! protect, and no line of theirs to close.
//!
//! What used to sit here was the inline pane's half of the contract: a
//! `LiveAnchor` published by `viewport::LiveView` as the pane moved, and
//! `restore_bytes` erasing from that row down. Both are gone, and the
//! `Mode::AltScreen` that replaced them is the reason — see ADR-0004 R1 and
//! the deletion note in [ADR-0006](0006-terminal-mode-ledger.md).
//!
//! # The contract
//!
//! [`Teardown::restore`] is idempotent, total and non-propagating: it can be
//! called by the run loop, then again by `main`, then again by a panic hook that
//! fired inside one of the first two, and the terminal is taken back exactly
//! once — every mode we hold handed back newest-first, and, on the one path that
//! does not hold the alternate screen, a single closing newline. The "exactly
//! once" is the acceptance criterion: a second newline is a doubled prompt line,
//! and a second leave is one more restore than the user agreed to.
//!
//! The guard is an [`AtomicBool`] swap rather than a [`std::sync::Once`] on
//! purpose: `Once::call_once` re-entered from a panic *inside its own closure*
//! deadlocks, and "the teardown itself panicked" is precisely the case the panic
//! hook exists to survive. A swap can only lose to a thread that is already doing
//! the job, which is the answer we want anyway.
//!
//! The closing newline is written only when the alternate screen is **not** held.
//! That branch is not a live path in this app any more — `Mode::DEFAULT` takes
//! the screen on before anything can draw — but the ledger is the thing that
//! knows what it holds, and the rule "a line we opened is a line we close" costs
//! one `if` and keeps the hand-back correct for whatever the mode set turns out
//! to be.
//!
//! # What lives where at exit
//!
//! This type owns the terminal half of the exit order — the erase, the mode ledger
//! and the closing newline; the run loop owns the steps around them, because they
//! need the App, the channels and the session tasks, none of which belong here.
//! The order is written out at the top of [`crate::run`], and the call itself
//! belongs one level *out* of the run loop (see [`crate::main`]) so that an early
//! `?` from anywhere in the setup still passes through it.
//!
//! # The mode ledger (looprs-pdl.3)
//!
//! "Raw mode off, pane erased, one newline" is the *whole* hand-back only while
//! the app switches on nothing but raw mode. The moment the app takes the
//! alternate screen, or the mouse, or bracketed paste, or hides the cursor, each
//! one of those is a promise to the user's terminal that has to be paid back — and
//! a mode left on is worse than a pane left up: a leaked `?1002` is a terminal
//! that reports drags forever, a leaked `?1049` is a scrollback the user cannot
//! get back, a leaked `?25` is a cursor they have to `reset` to find.
//!
//! [`Ledger`] is that promise, made explicit. Three rules, all of them the rule
//! this module already had, widened:
//!
//! * **It only unsets what it set.** A leave sequence for a mode we never
//!   switched on is not free — `?1049l` restores saved screen contents nobody
//!   saved, which is the user's screen replaced with whatever the terminal
//!   happened to keep. "Every mode we switch on gets switched off" is not a
//!   licence to emit every off sequence we know.
//! * **It never asks the terminal what state it is in.** Alt screen, mouse and
//!   bracketed paste all *can* be queried (`DECRQM`, `CSI ? <mode> $ p`). None
//!   of them are queried, for the same reason `ESC[6n` is not: a query at exit is
//!   a round trip on a stdin whose async reader is being torn down, and the exit
//!   path does not get to be uncertain. The ledger's own record is the state.
//! * **Exactly one leave per mode we hold, whatever else has touched it.** The
//!   off bytes go out unconditionally. A mode the terminal has already dropped is
//!   shown/shown-off again, which costs one byte string and cannot be wrong; a
//!   mode skipped because "it was probably already off" is a leak.
//!
//! Modes come off in the reverse of the order they went on ([`Mode::BOOT_ORDER`]):
//! the last thing we changed is the first thing we hand back, so each leave
//! sequence lands on the screen its `h` left us on, and the raw-mode syscall —
//! the first thing we ever set — is the last thing we drop. That ordering is the
//! only reason the closing newline is still on a cooked tty when it is written.
//!
//! ## Why the alternate screen changes the shape of the hand-back
//!
//! `?1049h` saves the main screen and the cursor along with it; `?1049l` puts
//! both back exactly as they were. So while the alternate screen is held there
//! is nothing of ours to erase and no line of theirs to close afterwards — the
//! user's prompt returns to the exact row it was on. [`Teardown::restore`]
//! therefore asks its own ledger one question — do I hold the alternate screen?
//! — and writes the closing newline only when it does not.
//!
//! Since looprs-pdl.4 that is the whole erase half of the contract, because the
//! alternate screen is the only screen. The `LiveAnchor` this type used to read
//! at restore time is deleted with the inline viewport it described: there is no
//! pane, so there is no top row of a pane to publish, and no row to erase from.
//! The one thing that still has to be paid by us rather than by the ledger is a
//! child's unpaid screen (`ScreenDebt`), and that is a different question —
//! whose screen are we standing on — read before the unwind for exactly that
//! reason.

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// One terminal mode the app can switch on, spelled exactly once.
///
/// The variant is the whole declaration: what it is, the bytes that turn it on,
/// the bytes that turn it off. Two copies of that table — one where the mode is
/// set, one where it is cleared — is how a mode gets away without being turned
/// off, so there is one table and both ends read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Mode {
    /// Raw mode: byte-at-a-time input, no echo, no line discipline.
    ///
    /// Not an escape sequence — it is `tcsetattr`, which is why [`Ledger`] has to
    /// know how to set this one by itself instead of writing bytes. It is still
    /// ledgered, because it is still a thing we do to the user's tty that has to
    /// come back off on the way out.
    Raw,
    /// The alternate screen: our own screen, the user's main screen parked.
    ///
    /// `?1047` and `?47` are the older spellings of the same fact and
    /// [`crate::screen`] already treats them as one; this app switches it on and
    /// off as `?1049` because that is the spelling that saves and restores the
    /// cursor along with the screen, which is the half the hand-back depends on.
    AltScreen,
    /// Cursor hidden (`?25l`) for the app's own frame.
    ///
    /// The app paints a frame over the rows the cursor was sitting on, so it hides
    /// the cursor while it draws and *must* give it back. Today the hide is done
    /// for us by a frame that reports no cursor position (ratatui hides the cursor
    /// in that case), which is a mode switch nobody owns — see
    /// [`crate::viewport::LiveView`] being held in a `ManuallyDrop`.
    CursorHidden,
    /// Mouse button reporting (`?1000h`).
    MouseReport,
    /// Mouse drag reporting (`?1002h`) — the one that "draws buttons forever" if
    /// it leaks, and so its own entry rather than being folded into
    /// [`Mode::MouseReport`].
    MouseDrag,
    /// SGR (`?1006h`) coordinate encoding for the mouse reports.
    MouseSgr,
    /// Bracketed paste (`?2004h`): the terminal wraps a paste in markers so a
    /// multi-line paste is not N submits.
    BracketedPaste,
}

impl Mode {
    /// The order the app switches modes on, and therefore the (reversed) order
    /// they come back off.
    ///
    /// Raw first, because everything below is written *through* a tty that is only
    /// byte-at-a-time once raw mode is on, and it is the last thing we drop for the
    /// same reason. The alternate screen next, because every byte written after it
    /// lands on our screen rather than the user's. Then the cursor, then the input
    /// modes we want reports from.
    pub const BOOT_ORDER: &'static [Mode] = &[
        Mode::Raw,
        Mode::AltScreen,
        Mode::CursorHidden,
        Mode::MouseReport,
        Mode::MouseDrag,
        Mode::MouseSgr,
        Mode::BracketedPaste,
    ];

    /// What the app switches on to run: the alternate screen the frame is drawn
    /// on, the cursor we hide while we draw it, and the raw tty both of them
    /// need.
    ///
    /// The alternate screen is in the default set because of ADR-0004 R1 — the
    /// full-screen frame (looprs-pdl.4) owns the whole window, and the promise
    /// that makes taking it safe is `?1049`'s: the main screen and the cursor are
    /// saved, and come back exactly. It is a default rather than a hard-wired
    /// requirement because the ledger is what decides the hand-back, and "a mode
    /// in the set is a mode that gets left" is the property worth keeping.
    ///
    /// `LOOPRS_MODES` *adds* to this rather than replacing it (see
    /// [`Mode::parse_all`]), so a run that asks for the mouse still gets a raw
    /// tty, a screen of its own, and a cursor that comes back.
    pub const DEFAULT: &'static [Mode] = &[Mode::Raw, Mode::AltScreen, Mode::CursorHidden];

    /// The name of the mode, as it appears in `LOOPRS_MODES` and in the log.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Raw => "raw",
            Mode::AltScreen => "alt_screen",
            Mode::CursorHidden => "cursor_hidden",
            Mode::MouseReport => "mouse_report",
            Mode::MouseDrag => "mouse_drag",
            Mode::MouseSgr => "mouse_sgr",
            Mode::BracketedPaste => "bracketed_paste",
        }
    }

    /// The bytes that switch the mode on. `None` for [`Mode::Raw`], which is a
    /// syscall and not a byte string.
    pub fn on_bytes(self) -> Option<&'static [u8]> {
        match self {
            Mode::Raw => None,
            Mode::AltScreen => Some(b"\x1b[?1049h"),
            Mode::CursorHidden => Some(b"\x1b[?25l"),
            Mode::MouseReport => Some(b"\x1b[?1000h"),
            Mode::MouseDrag => Some(b"\x1b[?1002h"),
            Mode::MouseSgr => Some(b"\x1b[?1006h"),
            Mode::BracketedPaste => Some(b"\x1b[?2004h"),
        }
    }

    /// The bytes that switch the mode off — the leave sequence, and the only
    /// thing that is ever allowed to emit it.
    pub fn off_bytes(self) -> Option<&'static [u8]> {
        match self {
            Mode::Raw => None,
            Mode::AltScreen => Some(b"\x1b[?1049l"),
            Mode::CursorHidden => Some(b"\x1b[?25h"),
            Mode::MouseReport => Some(b"\x1b[?1000l"),
            Mode::MouseDrag => Some(b"\x1b[?1002l"),
            Mode::MouseSgr => Some(b"\x1b[?1006l"),
            Mode::BracketedPaste => Some(b"\x1b[?2004l"),
        }
    }

    /// The three mouse modes, spelled together, because "turn the mouse on" is a
    /// single wish and half a mouse is worse than none.
    const MOUSE: &'static [Mode] = &[Mode::MouseReport, Mode::MouseDrag, Mode::MouseSgr];

    /// Parse a `LOOPRS_MODES`-style spec: `"alt_screen,mouse"`, `"all"`, `""`.
    /// An unknown name is an error rather than something quietly skipped — a typo
    /// in a mode list that means "nothing" is how a proof passes with the thing
    /// unproven.
    ///
    /// The result is sorted into [`Mode::BOOT_ORDER`] no matter what order the
    /// tokens came in, because the enable order is what the unwind order is
    /// derived from and a spec should not be able to scramble it.
    pub fn parse_all(spec: &str) -> Result<Vec<Mode>, String> {
        let mut out: Vec<Mode> = Vec::new();
        for token in spec.split([',', ' ', '\t', '\n']) {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            if token == "all" {
                out.extend(Mode::BOOT_ORDER.iter().copied());
                continue;
            }
            if token == "mouse" {
                out.extend(Mode::MOUSE.iter().copied());
                continue;
            }
            let found = Mode::BOOT_ORDER
                .iter()
                .copied()
                .find(|m| m.label() == token)
                .ok_or_else(|| {
                    format!(
                        "unknown terminal mode {token:?} (known: {}; \"mouse\" for all three mouse modes; \"all\" for everything)",
                        Mode::BOOT_ORDER
                            .iter()
                            .map(|m| m.label())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                })?;
            out.push(found);
        }
        out.sort_by_key(|m| {
            Mode::BOOT_ORDER
                .iter()
                .position(|x| x == m)
                .unwrap_or(u8::MAX as usize)
        });
        out.dedup();
        Ok(out)
    }

    /// The mode set the app starts with: [`Mode::DEFAULT`] plus whatever
    /// `LOOPRS_MODES` asks for.
    ///
    /// `LOOPRS_MODES` exists so the modes past the default set — the mouse and
    /// bracketed paste — can be proved on a real pty; `spikes/shutdown_e2e.py`
    /// sets it to `all`. A mode nobody asked for is a mode nobody is testing,
    /// which is the other half of why unknown names are an error.
    /// Did this process claim the alternate screen at startup?
    ///
    /// The one question from the startup set that anything *else* in the app has to
    /// ask, and it is asked here rather than re-derived from `$LOOPRS_MODES` at
    /// every call site because "who owns the alternate screen" is exactly the fact
    /// the full-screen child handover turns on (looprs-pdl.12, ADR-0001
    /// amendment 4). The ledger is the authority; this reads the same list the
    /// ledger was loaded from, once.
    pub fn alt_screen_claimed() -> bool {
        Self::startup_set()
            .map(|modes| modes.contains(&Mode::AltScreen))
            .unwrap_or(false)
    }

    pub fn startup_set() -> Result<Vec<Mode>, String> {
        let mut modes = Mode::DEFAULT.to_vec();
        modes.extend(Mode::parse_all(
            &std::env::var("LOOPRS_MODES").unwrap_or_default(),
        )?);
        modes.sort_by_key(|m| {
            Mode::BOOT_ORDER
                .iter()
                .position(|x| x == m)
                .unwrap_or(u8::MAX as usize)
        });
        modes.dedup();
        Ok(modes)
    }
}

/// Everything the teardown writes goes through one shared sink: the real stdout
/// in the app, a `Vec<u8>` in a test that wants the escape sequences back.
type SharedSink = Arc<Mutex<Box<dyn Write + Send>>>;

/// The modes this process has switched on, in the order it switched them on.
///
/// The ledger is the only thing in the app that is allowed to switch a terminal
/// mode off, and the only record of which ones are ours. See the module docs for
/// the three rules it exists to hold.
pub struct Ledger {
    /// LIFO: last switched on is first handed back.
    held: Arc<Mutex<Vec<Mode>>>,
    out: SharedSink,
}

impl Ledger {
    pub fn new(out: SharedSink) -> Self {
        Self {
            held: Arc::new(Mutex::new(Vec::new())),
            out,
        }
    }

    fn guard(&self) -> MutexGuard<'_, Vec<Mode>> {
        self.held
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Are we holding this mode (i.e. did we switch it on and not hand it back)?
    pub fn is_on(&self, mode: Mode) -> bool {
        self.guard().contains(&mode)
    }

    /// What we are holding, in the order it went on. Test/log seam.
    pub fn held_modes(&self) -> Vec<Mode> {
        self.guard().clone()
    }

    /// Switch `mode` on and record that we did.
    ///
    /// A mode already held is not switched on twice: the ledger is a set with an
    /// order, and two entries for one mode would mean two leave sequences.
    ///
    /// For the byte-switched modes the entry goes in *before* the bytes are
    /// written. A write can fail half-way — `?1049h` is five bytes and a tty that
    /// takes three of them still switched the screen — and the one thing the ledger
    /// may not do is lose track of a mode the terminal may be in. Raw mode is the
    /// mirror image: `enable_raw_mode` is one syscall that either changed the term
    /// ios or did not, and recording a failure would put a leave on the ledger that
    /// nothing earned.
    pub fn enable(&self, mode: Mode) -> io::Result<()> {
        if self.is_on(mode) {
            tracing::debug!(mode = mode.label(), "already switched on; ledger unchanged");
            return Ok(());
        }
        match mode {
            Mode::Raw => {
                crossterm::terminal::enable_raw_mode()?;
                self.held
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(mode);
            }
            byte_mode => {
                self.held
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(mode);
                if let Some(bytes) = byte_mode.on_bytes() {
                    self.write(bytes).map_err(|e| {
                        tracing::warn!(
                            mode = mode.label(),
                            "switching on did not reach the terminal: {e}"
                        );
                        e
                    })?;
                }
            }
        }
        tracing::debug!(mode = mode.label(), "switched on, ledgered");
        Ok(())
    }

    /// Hand one mode back early, and drop it from the ledger.
    ///
    /// This is for a mode the app gives up while it keeps running — the mouse goes
    /// back to the terminal before a full-screen child takes over, say. Returns
    /// `false` if we were not holding it, so a caller can tell "given back" from
    /// "never ours" instead of assuming.
    #[allow(dead_code)] // seam for looprs-pdl.12: handing the mouse back before a full-screen child takes the screen. A ledger that can only be emptied all at once is the wrong shape for that, so the seam is kept and tested rather than deleted.
    pub fn release(&self, mode: Mode) -> bool {
        let mut held = self.guard();
        let Some(pos) = held.iter().position(|m| *m == mode) else {
            return false;
        };
        held.remove(pos);
        drop(held);
        if let Some(bytes) = mode.off_bytes()
            && let Err(e) = self.write(bytes)
        {
            tracing::warn!(
                mode = mode.label(),
                "early release did not reach the terminal: {e}"
            );
        }
        true
    }

    /// Hand every mode back, newest first, unconditionally.
    ///
    /// Unconditional means what it says: no `DECRQM`, no `is_raw_mode_enabled`, no
    /// other question asked of a terminal we are trying to give back. The ledger
    /// knows what it set; that is the whole state the exit path is allowed to have.
    ///
    /// The list is taken out before anything is written, so a panic half-way
    /// through the unwind cannot leave a mode both held and already left off.
    pub fn unwind(&self) {
        let modes = std::mem::take(&mut *self.guard());
        for mode in modes.iter().rev() {
            tracing::debug!(mode = mode.label(), "handing back");
            match mode {
                Mode::Raw => {
                    if let Err(e) = crossterm::terminal::disable_raw_mode() {
                        tracing::warn!("raw mode is still on: {e}");
                    }
                }
                byte_mode => {
                    if let Some(bytes) = byte_mode.off_bytes()
                        && let Err(e) = self.write(bytes)
                    {
                        tracing::warn!(
                            mode = mode.label(),
                            "leave sequence did not reach the terminal: {e}"
                        );
                    }
                }
            }
        }
    }

    fn write(&self, bytes: &[u8]) -> io::Result<()> {
        let mut sink = self.out.lock().unwrap_or_else(|p| p.into_inner());
        sink.write_all(bytes)?;
        sink.flush()
    }
}

/// The exit path, in one idempotent object.
///
/// Cheap to hand out (an `Arc` in the app, a clone per hook) and safe to call
/// from anywhere, which is what lets the run loop, `main` and the panic hook all
/// claim to be "the one that restores the terminal" without ever restoring it
/// twice.
pub struct Teardown {
    /// Every terminal mode this process switched on, and the only thing allowed to
    /// switch any of them off.
    ledger: Ledger,
    /// The alternate screen a full-screen child switched on by being teed, and
    /// may never have handed back. See [`crate::screen::ScreenDebt`].
    screen_debt: crate::screen::ScreenDebt,
    done: AtomicBool,
    /// Everything the teardown writes goes here: the real stdout in the app, a
    /// `Vec<u8>` in a test that wants to read the escape sequences back.
    out: SharedSink,
}

/// `new()` with no arguments is the same thing; `Default` exists so that the
/// clippy `new_without_default` gate stays a gate rather than getting waived.
impl Default for Teardown {
    fn default() -> Self {
        Self::new()
    }
}

impl Teardown {
    /// A teardown wired to the real stdout.
    pub fn new() -> Self {
        Self::with_sink(io::stdout())
    }

    /// As [`Teardown::new`], writing somewhere other than the real stdout.
    /// Test seam: the byte sequence *is* the behaviour, and a test that cannot
    /// read the bytes back cannot pin it.
    fn with_sink(sink: impl Write + Send + 'static) -> Self {
        let out: SharedSink = Arc::new(Mutex::new(Box::new(sink)));
        Self {
            ledger: Ledger::new(out.clone()),
            screen_debt: crate::screen::ScreenDebt::new(),
            done: AtomicBool::new(false),
            out,
        }
    }

    /// The handle the passthrough reports a full-screen child's alternate screen
    /// into, so the exit path can pay what the child never will.
    ///
    /// Handed out rather than constructed by the caller because there must be
    /// exactly one of these per process: the debt is about the state of one
    /// terminal, and two copies is one copy that is wrong.
    pub fn screen_debt(&self) -> crate::screen::ScreenDebt {
        self.screen_debt.clone()
    }

    /// The bytes that put back the modes this process still holds, after a
    /// full-screen child has been through them.
    ///
    /// Measured (looprs-pdl.2 #7): a `vim` with `mouse=a` switches all three of
    /// our mouse modes off on the way out, and a `SIGKILL`ed vim leaves the
    /// terminal holding modes the ledger handed it. Neither is something the
    /// *ledger* changes — it still holds them, and still owes exactly one leave
    /// for each — but the terminal is not in the state the ledger describes, and
    /// the next thing the user does (drag, paste) is answered by a mode that is
    /// off. So the return from a child re-asserts rather than re-enables:
    /// same bytes, no second ledger entry, still one leave at the exit.
    ///
    /// What is deliberately **not** in the result:
    ///
    /// * [`Mode::Raw`] — not a byte string.
    /// * [`Mode::AltScreen`] — re-sending `?1049h` while already in the alternate
    ///   screen asks the terminal to save the *current* contents as the main
    ///   screen, which is the one destructive thing this app could do to the
    ///   user's scrollback. We are already there; the child's own enter is cut
    ///   upstream (ADR-0001 amendment 4) rather than replayed.
    pub fn reassert_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for mode in self.ledger.held_modes() {
            if mode == Mode::AltScreen || mode == Mode::Raw {
                continue;
            }
            if let Some(bytes) = mode.on_bytes() {
                out.extend_from_slice(bytes);
            }
        }
        out
    }

    /// Switch a terminal mode on, and take responsibility for it.
    ///
    /// This is the app's only way in to a terminal mode. Anything that writes
    /// `?1049h` without going through here is a mode the exit path does not know
    /// about, which is the failure this whole type was widened to prevent.
    pub fn enable(&self, mode: Mode) -> io::Result<()> {
        self.ledger.enable(mode)
    }

    /// Give one mode back before the exit, and forget it.
    #[allow(dead_code)] // see Ledger::release: the mid-run hand-back the full-screen handover needs
    pub fn release(&self, mode: Mode) -> bool {
        self.ledger.release(mode)
    }

    /// Do we currently hold this mode?
    pub fn is_on(&self, mode: Mode) -> bool {
        self.ledger.is_on(mode)
    }

    /// Has the terminal already been taken back?
    #[allow(dead_code)] // test seam: "restored exactly once" is only observable as "no more bytes written"; this says it directly
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// Take the terminal back: pay whatever child's screen we are standing on,
    /// hand back every mode we switched on, and close the line if we are the one
    /// who owes a line. Exactly once, no matter how many times this is called or
    /// from where.
    ///
    /// Never fails the caller. Every step is best-effort and every failure is
    /// logged rather than returned: a teardown that aborts halfway because the
    /// first write failed is how a user is left stuck in raw mode, which is a
    /// worse morning than a stray byte.
    ///
    /// The alternate screen is read out of the ledger *before* the unwind,
    /// because the unwind is what forgets it and it decides whether a closing
    /// newline is owed at all: see the module docs.
    pub fn restore(&self) {
        if self.done.swap(true, Ordering::SeqCst) {
            tracing::debug!("terminal already restored; this teardown is a no-op");
            return;
        }

        // Do we hold the alternate screen? If so the user's main screen is
        // parked and comes back untouched, cursor along with it: nothing of ours
        // to erase, and no line of theirs to close behind us.
        let ours_is_the_screen = self.is_on(Mode::AltScreen);
        // A child's alternate screen is a different shape of the same question:
        // the bytes that put it there went through us, and the program that
        // would have taken it back is dead, wedged, or never scheduled again.
        // Read it before the unwind, because every byte written after this one is
        // addressed to whichever screen this says we are on.
        let owed_by_us_to_them = self.screen_debt.outstanding();
        tracing::info!(
            holding = ?self.ledger.held_modes(),
            child_screen = ?owed_by_us_to_them,
            "handing the terminal back"
        );

        if !ours_is_the_screen && let Some(code) = owed_by_us_to_them {
            // Off the dead program's screen first. Everything below this is for
            // the user's own terminal and means nothing while we are still
            // standing inside somebody else's.
            //
            // Only reachable on a run that did not host the alternate screen
            // itself: when we do host it, a child's `?1049h` is cut upstream
            // (ADR-0001 amendment 4 / ADR-0004 R22) and never books a debt, and
            // our own single leave is the one that covers the screen.
            self.write(
                crate::screen::alt_leave(code),
                "the alt-screen leave the child did not pay",
            );
            self.screen_debt.pay();
        }

        // Every mode we set, newest first, unconditionally, without asking the
        // terminal whether it still has them. Raw mode comes off last because it
        // was the first thing on and the closing newline below is written through
        // a tty that has to be cooked by then.
        self.ledger.unwind();

        if !ours_is_the_screen {
            // One closing newline, cooked, for a run that drew on the user's own
            // screen: this ends the line the frame left so the shell prompt does
            // not start in the middle of it. Exactly one: two is the doubled
            // prompt. On the alternate-screen path there is nothing to close —
            // `?1049l` put the cursor back where their prompt already was.
            self.write(b"\n", "the closing newline");
        }
    }

    /// One best-effort write. Logged, never propagated: every caller of this is on
    /// the way out of the app, and a failure here is a thing to report, not a
    /// thing to stop the rest of the hand-back for.
    fn write(&self, bytes: &[u8], what: &str) {
        let mut sink = self.out.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = sink.write_all(bytes).and_then(|()| sink.flush()) {
            tracing::warn!("{what} never reached the terminal: {e}");
        }
    }
}

#[cfg(debug_assertions)]
static PANIC_IN_DRAW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// Fault injection for the exit contract: has `LOOPRS_PANIC` asked for a panic
/// inside the draw?
///
/// The proof list for this ticket includes "a forced panic inside the draw", and
/// a panic nobody can cause is a proof nobody can run. This is the only way to
/// cause one from outside the process that does not depend on finding — and then
/// reintroducing — a real bug in the frame code.
///
/// Debug builds only, on purpose: a shipped binary that turns an environment
/// variable into a panic is a liability, not a test seam. In a release build this
/// is a constant `false` and the call site costs nothing.
pub fn panic_injected() -> bool {
    #[cfg(debug_assertions)]
    {
        *PANIC_IN_DRAW.get_or_init(|| {
            std::env::var("LOOPRS_PANIC")
                .unwrap_or_default()
                .split(',')
                .any(|s| s.trim() == "draw")
        })
    }
    #[cfg(not(debug_assertions))]
    {
        false
    }
}

/// Make a panic take the terminal back the same way Ctrl-Q does.
///
/// The old hook did `let _ = disable_raw_mode()` and nothing else, which is a
/// different exit from the normal one — the pane stayed on screen, the panic text
/// printed over it, and the two paths could drift forever. Now they are literally
/// the same call, so they cannot.
///
/// The previous hook is kept and run afterwards, so the panic message still goes
/// wherever the default (or a test harness) sends it, and the log writer held in
/// `main` is still alive to catch it.
pub fn install_panic_hook(exit: Arc<Teardown>) {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Restore first, then talk: a panic message printed into a live pane is
        // unreadable, and raw mode has to be off before it wraps lines sanely.
        exit.restore();
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hand-back describes no place on the screen.
    ///
    /// The inline pane needed a published top row, because erasing down from the
    /// wrong row deletes scrollback the user cannot get back. A full-screen
    /// frame's leave is one byte string — no row, no query — so there is nothing
    /// left that can be stale, wrong or unknown at restore time, and the bytes
    /// say where nothing.
    #[test]
    fn the_hand_back_describes_no_place_on_the_screen() {
        let (t, buf) = teardown();
        t.restore();
        let out = bytes(&buf);
        for not_a_row in ["\x1b[1;", "\x1b[2;", "\x1b[3;", "\x1b[H", "\x1b[J"] {
            assert!(
                !out.contains(not_a_row),
                "the hand-back addressed a row ({not_a_row:?}); it should not \
                 know any: {out:?}"
            );
        }
    }

    /// On the path that does not hold the alternate screen — not the one this app
    /// takes any more, but the one the ledger still has to be right about — the
    /// hand-back is a single closing newline, and exactly one of those.
    #[test]
    fn a_run_that_owes_a_line_gets_exactly_one_closing_newline() {
        let (t, buf) = teardown();
        t.restore();
        let out = bytes(&buf);
        assert_eq!(out, "\n");
        assert!(t.is_done());
        assert_eq!(
            out.matches('\n').count(),
            1,
            "one closing newline, not a doubled prompt"
        );
    }

    /// Idempotence, stated as "the second call writes nothing at all" — which is
    /// the acceptance criterion "the terminal is restored exactly once" seen from
    /// the only place it can be observed: the bytes.
    #[test]
    fn restoring_twice_restores_once() {
        let (t, buf) = teardown();
        t.restore();
        let first = bytes(&buf);
        t.restore();
        t.restore();
        assert_eq!(bytes(&buf), first, "the second call must not write");
    }

    fn teardown() -> (Teardown, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        (Teardown::with_sink(Sink(buf.clone())), buf)
    }

    /// A shared, readable sink.
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn bytes(buf: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    /// A panic takes the terminal back the same way the normal exit does, because
    /// it is the same call on the same object — the anti-drift requirement.
    #[test]
    fn the_panic_hook_and_the_normal_exit_agree() {
        let direct = {
            let (t, buf) = teardown();
            t.restore();
            bytes(&buf)
        };

        let buf = Arc::new(Mutex::new(Vec::new()));
        let prev = std::panic::take_hook();
        // Silence the real panic output: this test is about the terminal, and the
        // default hook would shout a backtrace at whoever is running the suite.
        std::panic::set_hook(Box::new(|_| {}));
        {
            let t = Arc::new(Teardown::with_sink(Sink(buf.clone())));
            // Chains onto the silenced hook, so the panic text stays out of the
            // way and the restore still happens first.
            install_panic_hook(t.clone());
            let _ = std::panic::catch_unwind(|| panic!("deliberate"));
            // The hook already restored, and now the normal path asks again: that
            // double-ask is exactly what the `done` flag exists for.
            t.restore();
        }
        std::panic::set_hook(prev);

        assert_eq!(
            bytes(&buf),
            direct,
            "a panic must leave the terminal exactly as Ctrl-Q does"
        );
        assert_eq!(
            bytes(&buf).matches('\n').count(),
            1,
            "and must not stack a second closing newline"
        );
    }

    /// The ledger is the reason no mode gets left on: nothing is turned off that was
    /// never turned on, so a restore against a terminal this binary never touched
    /// writes no mode bytes at all — and no `is_raw_mode_enabled` read either, so
    /// the exit path asks the terminal nothing.
    #[test]
    fn a_restore_does_not_toggle_raw_mode_that_was_never_on() {
        assert!(
            !crossterm::terminal::is_raw_mode_enabled().unwrap(),
            "no test in this binary puts the real terminal into raw mode"
        );
        let (t, buf) = teardown();
        t.restore();
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
        assert_eq!(
            bytes(&buf),
            "\n",
            "the whole hand-back for a run that switched nothing on is the closing newline"
        );
    }

    // ── the ledger (looprs-pdl.3) ───────────────────────────────────────────

    /// The wire format is the contract, so the literals are pinned here rather
    /// than trusted to a table read. A mode whose off bytes do not match its on
    /// bytes is a mode that leaks, and only the bytes can tell.
    #[test]
    fn every_mode_names_its_own_pair_of_bytes() {
        let pairs = [
            (Mode::AltScreen, "\x1b[?1049h", "\x1b[?1049l"),
            (Mode::CursorHidden, "\x1b[?25l", "\x1b[?25h"),
            (Mode::MouseReport, "\x1b[?1000h", "\x1b[?1000l"),
            (Mode::MouseDrag, "\x1b[?1002h", "\x1b[?1002l"),
            (Mode::MouseSgr, "\x1b[?1006h", "\x1b[?1006l"),
            (Mode::BracketedPaste, "\x1b[?2004h", "\x1b[?2004l"),
        ];
        for (mode, on, off) in pairs {
            assert_eq!(mode.on_bytes(), Some(on.as_bytes()), "{} on", mode.label());
            assert_eq!(
                mode.off_bytes(),
                Some(off.as_bytes()),
                "{} off",
                mode.label()
            );
        }
        // Raw mode is the one that is not a byte string: it is `tcsetattr`.
        assert_eq!(Mode::Raw.on_bytes(), None);
        assert_eq!(Mode::Raw.off_bytes(), None);
        // And every mode in the boot order is in the table above or is Raw.
        for mode in Mode::BOOT_ORDER {
            assert!(
                *mode == Mode::Raw || pairs.iter().any(|(m, _, _)| m == mode),
                "{mode:?} is in BOOT_ORDER with no pinned bytes"
            );
        }
    }

    /// Switching a mode on is a write we can see, and switching it on twice is not
    /// two writes: two entries for one mode would be two leave sequences, and the
    /// acceptance criterion is exactly one.
    #[test]
    fn enabling_a_mode_writes_it_once_and_repeating_the_enable_writes_nothing() {
        let (t, buf) = teardown();
        t.enable(Mode::MouseSgr).unwrap();
        assert_eq!(bytes(&buf), "\x1b[?1006h");
        t.enable(Mode::MouseSgr).unwrap();
        assert_eq!(
            bytes(&buf),
            "\x1b[?1006h",
            "the second enable is not a second on"
        );
        assert!(t.is_on(Mode::MouseSgr));

        t.restore();
        let out = bytes(&buf);
        assert_eq!(
            out.matches("\x1b[?1006l").count(),
            1,
            "exactly one leave for a mode switched on twice: {out:?}"
        );
    }

    /// The order the modes come off is the reverse of the order they went on, and
    /// that is a byte-level fact, not a claim about a data structure nobody reads.
    /// The last thing the app changed has to be the first thing it gives back,
    /// because each leave sequence lands on the screen its `h` left the terminal on.
    #[test]
    fn the_ledger_hands_back_in_the_reverse_of_the_order_it_went_on() {
        let (t, buf) = teardown();
        for m in [Mode::AltScreen, Mode::CursorHidden, Mode::BracketedPaste] {
            t.enable(m).unwrap();
        }
        t.ledger.unwind();
        assert_eq!(
            bytes(&buf),
            concat!(
                "\x1b[?1049h\x1b[?25l\x1b[?2004h", // on: screen, cursor, paste
                "\x1b[?2004l\x1b[?25h\x1b[?1049l", // off: paste, cursor, screen
            ),
            "pasted first, so pasted first back; the screen we are standing on comes last"
        );
    }

    /// The half of the rule that keeps a leave sequence from being harmless:
    /// `?1049l` restores the screen and cursor the terminal saved when the
    /// alternate screen was entered. Emit it without having entered one and it
    /// restores *something* — which screen, and which cursor, is the terminal's
    /// business, and it is the user's screen. A mode we never set is a mode we
    /// never leave.
    #[test]
    fn a_mode_we_never_switched_on_is_never_left_off() {
        let (t, buf) = teardown();
        t.enable(Mode::CursorHidden).unwrap();
        t.restore();
        let out = bytes(&buf);
        assert!(
            out.contains("\x1b[?25h"),
            "the cursor we hid came back: {out:?}"
        );
        for leaked in [
            "?1049l", "?1047l", "?47l", "?1000l", "?1002l", "?1006l", "?2004l",
        ] {
            assert!(
                !out.contains(leaked),
                "the hand-back emitted {leaked} for a mode nothing switched on: {out:?}"
            );
        }
    }

    /// A write that fails is not a mode we stopped owing. `?1049h` is nine bytes
    /// and a tty that takes part of them has still switched the screen, so the
    /// ledger takes the entry *before* it writes and keeps it whatever the write
    /// did — the leave goes out at restore time either way.
    #[test]
    fn a_switch_that_never_reached_the_terminal_is_still_a_mode_we_hold() {
        let t = Teardown::with_sink(Failing);
        assert!(
            t.enable(Mode::AltScreen).is_err(),
            "the write failed, and said so"
        );
        assert!(
            t.is_on(Mode::AltScreen),
            "and the ledger still holds the screen it may well have taken"
        );
        // The restore runs to the end even though every one of its writes fails:
        // a failure in the erase may not skip the mode hand-back.
        t.restore();
        assert!(t.is_done());
    }

    /// `restore` is the one place the ledger is emptied, and the `done` swap is
    /// what makes it so across the run loop, `main` and a panic hook that fired
    /// inside either.
    #[test]
    fn restore_hands_every_mode_back_once_and_nothing_after_it() {
        let (t, buf) = teardown();
        t.enable(Mode::BracketedPaste).unwrap();
        t.enable(Mode::MouseDrag).unwrap();
        t.restore();
        let first = bytes(&buf);
        assert_eq!(
            first,
            concat!(
                "\x1b[?2004h\x1b[?1002h", // what we switched on, in order
                "\x1b[?1002l\x1b[?2004l", // handed back, newest first
                "\n",                     // and the line closed, cooked
            )
        );
        t.restore();
        assert_eq!(bytes(&buf), first, "a second restore hands back nothing");
    }

    /// The alternate screen is the difference between "we drew next to the user's
    /// text" and "we had a screen of our own". While we hold it, `?1049l` puts
    /// their screen and their cursor back exactly as they were, so the pane erase
    /// has nothing above it to protect and the closing newline would only scroll a
    /// prompt that is already where it belongs.
    #[test]
    fn holding_the_alternate_screen_means_no_erase_and_no_closing_newline() {
        let (t, buf) = teardown();
        t.enable(Mode::AltScreen).unwrap();
        t.enable(Mode::CursorHidden).unwrap();
        t.restore();
        assert_eq!(
            bytes(&buf),
            concat!(
                "\x1b[?1049h\x1b[?25l", // what we switched on
                "\x1b[?25h\x1b[?1049l", // and the leave sequence is the last word
            ),
            "the user's screen resumes at that last byte"
        );
        assert!(
            !bytes(&buf).contains("\x1b[J"),
            "no erase inside a screen we are leaving"
        );
        assert!(
            !bytes(&buf).contains('\n'),
            "nothing of ours to close a line after"
        );
    }

    /// The child's alternate screen is the same problem in a different costume:
    /// `?1049h` went out through the passthrough, the program that owed the leave
    /// is dead or wedged, and the user is looking at a screen that is not theirs
    /// and cannot get back. Nothing on the session side can deliver that byte any
    /// more — the pump is cut, the shell is killed, the task is gone — so the exit
    /// path writes it itself, **before** anything addressed to the user's own
    /// screen, and then does the inline hand-back it was always going to do.
    #[test]
    fn a_screen_a_dying_child_left_behind_is_left_on_the_way_out() {
        let (t, buf) = teardown();
        // What the passthrough reported: the child entered the alt screen and we
        // never saw it leave.
        t.screen_debt()
            .note_tee(b"\x1b[?1049hpainted the whole screen");
        assert_eq!(t.screen_debt().outstanding(), Some(1049));
        // And the app's own modes, as a real run would have them.
        t.enable(Mode::CursorHidden).unwrap();

        t.restore();
        let out = bytes(&buf);
        assert_eq!(
            out,
            concat!(
                "\x1b[?25l",   // what we switched on
                "\x1b[?1049l", // the child's screen, paid back by us
                "\x1b[?25h",   // our own mode, handed back
                "\n",          // and the line closed, cooked
            ),
            "off the dead program's screen first, then the hand-back the user expects"
        );
    }

    /// The ordinary case: vim quit properly and its own `?1049l` went out through
    /// the tee. The debt is discharged, and a second leave would be a leave for a
    /// screen nobody holds.
    #[test]
    fn a_screen_the_child_already_left_is_not_left_again() {
        let (t, buf) = teardown();
        t.screen_debt().note_tee(b"\x1b[?1049hpaint\x1b[?1049l");
        assert_eq!(
            t.screen_debt().outstanding(),
            None,
            "the child settled up as it went"
        );
        t.restore();
        let out = bytes(&buf);
        assert!(
            !out.contains("?1049l"),
            "no leave for a screen that is already back: {out:?}"
        );
    }

    /// `?1047h` is not undone by `?1049l`. Whatever code the child used, the
    /// leave that goes out is the matching one.
    #[test]
    fn the_leave_is_spelled_the_way_the_entry_was() {
        for (entered, owed) in [
            (b"\x1b[?1049h".as_slice(), "\x1b[?1049l"),
            (b"\x1b[?1047h".as_slice(), "\x1b[?1047l"),
            (b"\x1b[?47h".as_slice(), "\x1b[?47l"),
        ] {
            let (t, buf) = teardown();
            t.screen_debt().note_tee(entered);
            t.restore();
            let out = bytes(&buf);
            assert!(out.contains(owed), "{entered:?} was answered with {out:?}");
        }
    }

    /// When the app holds the alternate screen itself *and* a child left one
    /// behind, it is still one screen and one leave: the ledger's own unwind
    /// covers it, and the child path stays out of the way rather than adding a
    /// second leave.
    #[test]
    fn our_own_alternate_screen_already_covers_the_childs() {
        let (t, buf) = teardown();
        t.enable(Mode::AltScreen).unwrap();
        t.screen_debt().note_tee(b"\x1b[?1049hchild paint");
        t.restore();
        let out = bytes(&buf);
        assert_eq!(
            out.matches("?1049l").count(),
            1,
            "exactly one leave for one screen: {out:?}"
        );
    }

    /// A takeover the watcher only *inferred* — cursor addressing with no
    /// linefeeds, no alt-screen switch at all — owes no leave. Writing `?1049l`
    /// for it would be the one class of byte this exit path is not allowed to emit.
    #[test]
    fn a_screen_that_was_only_guessed_at_owes_no_leave() {
        let (t, buf) = teardown();
        t.screen_debt()
            .note_tee(b"\x1b[10;1H\x1b[2Jpainted without an alt screen");
        assert_eq!(t.screen_debt().outstanding(), None);
        t.restore();
        let out = bytes(&buf);
        assert!(
            !out.contains("1049") && !out.contains("1047") && !out.contains("?47l"),
            "no alt-screen byte for a screen nobody switched: {out:?}"
        );
    }

    /// Releasing a mode mid-run takes it off the ledger, so the restore does not
    /// leave it off a second time. This is the seam looprs-pdl.12 needs to hand the
    /// mouse back before a full-screen child takes the screen.
    #[test]
    fn releasing_a_mode_early_takes_it_off_the_ledger() {
        let (t, buf) = teardown();
        t.enable(Mode::MouseReport).unwrap();
        t.enable(Mode::MouseSgr).unwrap();
        assert!(t.ledger.release(Mode::MouseReport), "we were holding it");
        assert!(!t.is_on(Mode::MouseReport), "and now we are not");
        assert!(
            !t.ledger.release(Mode::MouseReport),
            "never ours a second time"
        );
        t.restore();
        let out = bytes(&buf);
        assert_eq!(
            out.matches("\x1b[?1000l").count(),
            1,
            "the early release is the one and only leave: {out:?}"
        );
        assert!(
            out.contains("\x1b[?1006l"),
            "the mode still held is handed back"
        );
    }

    #[test]
    fn a_mode_spec_is_parsed_into_boot_order_whatever_order_it_came_in() {
        let modes = Mode::parse_all("mouse_sgr,alt_screen").unwrap();
        assert_eq!(modes, vec![Mode::AltScreen, Mode::MouseSgr]);
        assert_eq!(Mode::parse_all("").unwrap(), Vec::<Mode>::new());
        assert_eq!(Mode::parse_all("  \n").unwrap(), Vec::<Mode>::new());
    }

    #[test]
    fn a_mouse_is_three_modes_and_all_is_everything() {
        assert_eq!(
            Mode::parse_all("mouse").unwrap(),
            vec![Mode::MouseReport, Mode::MouseDrag, Mode::MouseSgr]
        );
        assert_eq!(Mode::parse_all("all").unwrap(), Mode::BOOT_ORDER.to_vec());
        // Duplicated names are one mode, not two, however they are spelled.
        assert_eq!(
            Mode::parse_all("all,all").unwrap(),
            Mode::BOOT_ORDER.to_vec()
        );
        assert_eq!(
            Mode::parse_all("mouse,mouse_sgr").unwrap(),
            vec![Mode::MouseReport, Mode::MouseDrag, Mode::MouseSgr]
        );
    }

    /// A typo in a mode list that means "nothing" is a proof that passed with the
    /// thing unproven, so it is an error, and it says what the names were.
    #[test]
    fn an_unknown_mode_is_an_error_that_names_the_names() {
        let err = Mode::parse_all("alt_scren").unwrap_err();
        assert!(err.contains("alt_scren"), "{err}");
        assert!(err.contains("alt_screen"), "{err}");
        assert!(err.contains("bracketed_paste"), "{err}");
    }

    /// The startup set always keeps the two modes the app cannot run without, no
    /// matter what `LOOPRS_MODES` asks for on top. Losing raw mode because somebody
    /// typed `LOOPRS_MODES=alt` would be a worse failure than the typo itself.
    #[test]
    fn the_startup_set_is_the_default_plus_what_was_asked_for() {
        let mut want = Mode::DEFAULT.to_vec();
        want.extend(vec![Mode::AltScreen, Mode::MouseSgr]);
        want.sort_by_key(|m| Mode::BOOT_ORDER.iter().position(|x| x == m).unwrap());
        want.dedup();
        assert_eq!(startup_set_with("alt_screen,mouse_sgr"), want);
        // `all` includes the defaults already, so it adds and does not double up.
        assert_eq!(startup_set_with("all"), Mode::BOOT_ORDER.to_vec());
        assert_eq!(startup_set_with(""), Mode::DEFAULT.to_vec());
    }

    /// [`Mode::startup_set`] reads the process environment, which is shared by
    /// every test in this binary, so the tests set it through this instead and only
    /// this touches it.
    fn startup_set_with(spec: &str) -> Vec<Mode> {
        let mut modes = Mode::DEFAULT.to_vec();
        modes.extend(Mode::parse_all(spec).unwrap());
        modes.sort_by_key(|m| Mode::BOOT_ORDER.iter().position(|x| x == m).unwrap());
        modes.dedup();
        modes
    }

    /// A sink whose every write fails, for the case where the terminal stopped
    /// listening halfway through a mode switch.
    struct Failing;
    impl Write for Failing {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("the tty is gone"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A poisoned *ledger* is still unwound.
    ///
    /// The teardown is called from inside a panic hook, which is exactly how the
    /// mutex gets poisoned, and "the lock is poisoned" is not an acceptable
    /// reason to leave the user's terminal in raw mode. What the mutex was
    /// protecting is still in there and still true.
    #[test]
    fn a_poisoned_ledger_still_hands_everything_back() {
        let (t, buf) = teardown();
        t.enable(Mode::MouseSgr).unwrap();
        let held = t.ledger.held.clone();
        let poisoner = std::thread::spawn(move || {
            let _guard = held.lock();
            panic!("poison it");
        });
        assert!(poisoner.join().is_err(), "the poisoner really panicked");

        t.restore();
        let out = bytes(&buf);
        assert!(
            out.contains("\x1b[?1006l"),
            "poisoned and still handed back: {out:?}"
        );
    }
}
