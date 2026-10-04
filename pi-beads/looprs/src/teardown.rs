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
//! It already knows what it needs. The live region's top row is published by
//! [`crate::viewport::LiveView`] as it moves ([`LiveAnchor`]), and erasing
//! downward from a known row is a write, not a round trip. That is the same
//! [`ClearType::FromCursorDown`](ratatui::backend::ClearType) the live region uses
//! between shape changes all session; the difference is that this one has no
//! cursor query in front of it.
//!
//! # The contract
//!
//! [`Teardown::restore`] is idempotent, total and non-propagating: it can be
//! called by the run loop, then again by `main`, then again by a panic hook that
//! fired inside one of the first two, and the terminal is taken back exactly once
//! — clear the live pane, raw mode off, one closing newline, in that order. The
//! "exactly once" is the acceptance criterion: a second newline is a doubled
//! prompt line, and a second clear is one more erase than the user agreed to.
//!
//! The guard is an [`AtomicBool`] swap rather than a [`std::sync::Once`] on
//! purpose: `Once::call_once` re-entered from a panic *inside its own closure*
//! deadlocks, and "the teardown itself panicked" is precisely the case the panic
//! hook exists to survive. A swap can only lose to a thread that is already doing
//! the job, which is the answer we want anyway.
//!
//! Unknown anchor (`None`) means *erase nothing*. Before the first frame, or after
//! something moved the viewport without telling us (a window resize, a
//! full-screen hand-back), the row we would clear from is a guess, and a wrong
//! guess erases scrollback rows above the pane — the one loss in this program the
//! user cannot get back. In that case we still take raw mode off and still close
//! the line.
//!
//! # What lives where at exit
//!
//! This type owns steps (4) and (5) of the exit order; steps (1)-(3) and (6) are
//! the run loop's, because they need the App, the channels and the session tasks,
//! none of which belong here. The order is written out at the top of
//! [`crate::run`].

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use crossterm::cursor::MoveTo;
use crossterm::terminal::{Clear, ClearType};

/// Where the top edge of the live region is, shared between the live view and the
/// exit path.
///
/// `None` is "unknown", not "row zero", and the exit path treats it that way: see
/// the module docs on why a guessed anchor is the one unrecoverable mistake.
///
/// Cloning hands out another handle to the *same* row, which is the point: the
/// live view keeps publishing into it and the teardown keeps reading it, and there
/// is no second copy to drift out of agreement with the first.
#[derive(Clone, Default)]
pub struct LiveAnchor(Arc<Mutex<Option<u16>>>);

impl LiveAnchor {
    /// An anchor that nothing has written yet (unknown).
    pub fn new() -> Self {
        Self::default()
    }

    /// An anchor that starts on a known row. Test seam, mostly: the real one is
    /// created unknown and filled in by the first frame.
    #[cfg(test)]
    pub fn at(row: u16) -> Self {
        let a = Self::new();
        a.set(Some(row));
        a
    }

    pub fn get(&self) -> Option<u16> {
        *self.lock()
    }

    /// Publish the live region's top row (`None` = no idea, see [`Self::get`]).
    pub fn set(&self, row: Option<u16>) {
        *self.lock() = row;
    }

    /// A poisoned anchor still holds the last true row it was given. At exit that
    /// is worth far more than the panic that poisoned it, and refusing to act is
    /// not one of the options here — this lock is taken from inside a panic hook.
    fn lock(&self) -> MutexGuard<'_, Option<u16>> {
        self.0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The bytes that take the live region back, for a region starting at `anchor`.
///
/// Park on the pane's own top row at column 0, then erase from there down. The
/// rows above it — the scrollback, everything the user has read — are not part of
/// this function's reach, which is the same invariant `LiveView::fit` holds when
/// it reshapes the pane mid-session.
///
/// Empty for an unknown anchor, and empty is the safe answer.
pub fn restore_bytes(anchor: Option<u16>) -> Vec<u8> {
    let mut out = Vec::new();
    if let Some(row) = anchor {
        // `FromCursorDown` is `CSI J` — the erase ratatui issues for
        // `ClearType::AfterCursor`, so this is the pane disappearing exactly the
        // way it has been disappearing between shape changes all session.
        let _ = write!(
            out,
            "{}{}",
            MoveTo(0, row),
            Clear(ClearType::FromCursorDown)
        );
    }
    out
}

/// The exit path, in one idempotent object.
///
/// Cheap to hand out (an `Arc` in the app, a clone per hook) and safe to call
/// from anywhere, which is what lets the run loop, `main` and the panic hook all
/// claim to be "the one that restores the terminal" without ever restoring it
/// twice.
pub struct Teardown {
    anchor: LiveAnchor,
    done: AtomicBool,
    /// Everything the teardown writes goes here: the real stdout in the app, a
    /// `Vec<u8>` in a test that wants to read the escape sequences back.
    sink: Mutex<Box<dyn Write + Send>>,
}

impl Teardown {
    /// A teardown wired to the real stdout.
    pub fn new(anchor: LiveAnchor) -> Self {
        Self::with_sink(anchor, io::stdout())
    }

    /// As [`Teardown::new`], writing somewhere other than the real stdout.
    /// Test seam: the byte sequence *is* the behaviour, and a test that cannot
    /// read the bytes back cannot pin it.
    fn with_sink(anchor: LiveAnchor, sink: impl Write + Send + 'static) -> Self {
        Self {
            anchor,
            done: AtomicBool::new(false),
            sink: Mutex::new(Box::new(sink)),
        }
    }

    /// Has the terminal already been taken back?
    #[allow(dead_code)] // test seam: "restored exactly once" is only observable as "no more bytes written"; this says it directly
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }

    /// Take the terminal back: erase the live pane, turn raw mode off, close the
    /// line. Exactly once, no matter how many times this is called or from where.
    ///
    /// Never fails the caller. Every step is best-effort and every failure is
    /// logged rather than returned: a teardown that aborts halfway because the
    /// first write failed is how a user is left stuck in raw mode, which is a
    /// worse morning than a stray byte.
    pub fn restore(&self) {
        if self.done.swap(true, Ordering::SeqCst) {
            tracing::debug!("terminal already restored; this teardown is a no-op");
            return;
        }

        let anchor = self.anchor.get();
        let bytes = restore_bytes(anchor);
        let mut sink = self.sink.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = sink.write_all(&bytes).and_then(|()| sink.flush()) {
            // Nothing drawn, but raw mode still has to come off, so fall through.
            tracing::warn!("live region clear never reached the terminal: {e}");
        }
        drop(sink);

        // Raw mode off *after* the erase, so the erase is the last thing the
        // terminal was asked to draw and nothing can repaint the pane behind us.
        match crossterm::terminal::is_raw_mode_enabled() {
            Ok(true) => {
                if let Err(e) = crossterm::terminal::disable_raw_mode() {
                    tracing::warn!("raw mode is still on: {e}");
                }
            }
            Ok(false) => {}
            Err(e) => tracing::warn!("could not read the raw mode state: {e}"),
        }

        // One closing newline, cooked. The erased pane left the cursor at the left
        // margin of the row it occupied; this ends that line so the shell prompt
        // starts on a clean one. Exactly one: two is the doubled prompt.
        let mut sink = self.sink.lock().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = sink.write_all(b"\n").and_then(|()| sink.flush()) {
            tracing::warn!("the closing newline never reached the terminal: {e}");
        }
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

    /// The erase is anchored at the pane's own top row, and only erases downward.
    #[test]
    fn a_known_anchor_clears_from_the_panes_top_row_down() {
        let bytes = restore_bytes(Some(11));
        let s = String::from_utf8(bytes).expect("ascii escape sequence");
        assert_eq!(
            s, "\x1b[12;1H\x1b[J",
            "row 11 -> 1-based row 12, then erase down"
        );
    }

    /// Unknown anchor: erase nothing. Guessing a row is how a teardown paints over
    /// scrollback the user still wanted.
    #[test]
    fn an_unknown_anchor_erases_nothing() {
        assert!(restore_bytes(None).is_empty());
    }

    fn teardown(anchor: Option<u16>) -> (Teardown, Arc<Mutex<Vec<u8>>>) {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let t = Teardown::with_sink(
            anchor.map(LiveAnchor::at).unwrap_or_default(),
            Sink(buf.clone()),
        );
        (t, buf)
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

    /// The whole normal-path sequence: clear, then one closing newline.
    #[test]
    fn restore_clears_the_pane_and_closes_the_line_once() {
        let (t, buf) = teardown(Some(7));
        t.restore();
        let out = bytes(&buf);
        assert_eq!(out, "\x1b[8;1H\x1b[J\n");
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
        let (t, buf) = teardown(Some(4));
        t.restore();
        let first = bytes(&buf);
        t.restore();
        t.restore();
        assert_eq!(bytes(&buf), first, "the second call must not write");
    }

    /// Unknown anchor still gets the raw-mode and newline halves; only the erase is
    /// withheld, because there is nothing safe to erase.
    #[test]
    fn an_unknown_anchor_still_leaves_the_terminal_usable() {
        let (t, buf) = teardown(None);
        t.restore();
        assert_eq!(bytes(&buf), "\n", "no clear, but the line still closes");
        assert!(
            !crossterm::terminal::is_raw_mode_enabled().unwrap(),
            "and raw mode is not left on"
        );
    }

    /// The anchor is live, not a snapshot taken at construction: the row that is
    /// published by the time `restore` runs is the row that gets cleared from.
    /// That is what keeps the published row from being a second copy of the truth.
    #[test]
    fn the_anchor_is_read_at_restore_time_not_at_build_time() {
        let anchor = LiveAnchor::new();
        let buf = Arc::new(Mutex::new(Vec::new()));
        let t = Teardown::with_sink(anchor.clone(), Sink(buf.clone()));

        // Nothing published yet: unknown anchor, so nothing is erased.
        t.restore();
        assert_eq!(bytes(&buf), "\n", "no anchor means no clear");

        // A teardown built *after* the row became known does clear from it.
        anchor.set(Some(9));
        let buf2 = Arc::new(Mutex::new(Vec::new()));
        Teardown::with_sink(anchor.clone(), Sink(buf2.clone())).restore();
        assert_eq!(bytes(&buf2), "\x1b[10;1H\x1b[J\n");
    }

    /// A panic takes the terminal back the same way the normal exit does, because
    /// it is the same call on the same object — the anti-drift requirement.
    #[test]
    fn the_panic_hook_and_the_normal_exit_agree() {
        let direct = {
            let (t, buf) = teardown(Some(6));
            t.restore();
            bytes(&buf)
        };

        let buf = Arc::new(Mutex::new(Vec::new()));
        let prev = std::panic::take_hook();
        // Silence the real panic output: this test is about the terminal, and the
        // default hook would shout a backtrace at whoever is running the suite.
        std::panic::set_hook(Box::new(|_| {}));
        {
            let t = Arc::new(Teardown::with_sink(LiveAnchor::at(6), Sink(buf.clone())));
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

    /// Raw mode is read, not assumed: the teardown never touches a terminal that
    /// was never put into raw mode, which is why this module is safe to exercise
    /// from a test binary.
    #[test]
    fn a_restore_does_not_toggle_raw_mode_that_was_never_on() {
        assert!(
            !crossterm::terminal::is_raw_mode_enabled().unwrap(),
            "no test in this binary puts the real terminal into raw mode"
        );
        let (t, _buf) = teardown(Some(3));
        t.restore();
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
    }

    #[test]
    fn a_poisoned_anchor_is_still_read() {
        let anchor = LiveAnchor::at(5);
        let shared = anchor.clone();
        // A real panic while holding the lock — the only way this mutex can be
        // poisoned, and one of the ways the teardown gets called.
        let poisoner = std::thread::spawn(move || {
            let _held = shared.lock();
            panic!("poison it");
        });
        assert!(poisoner.join().is_err(), "the poisoner really panicked");

        // Poisoned, and still answered: the row in there is the last true one,
        // which is worth more at exit than the panic that poisoned it.
        assert_eq!(anchor.get(), Some(5));
        assert_eq!(restore_bytes(anchor.get()), restore_bytes(Some(5)));
    }
}
