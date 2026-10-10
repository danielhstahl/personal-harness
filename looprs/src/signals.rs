//! The ways out that arrive from outside the terminal (looprs-pdl.3).
//!
//! Ctrl-Q is a keystroke: it comes up the key stream, the App decides what it
//! means, and the run loop takes its own exit path. `SIGTERM` and `SIGHUP` are
//! not keystrokes. They arrive from a supervisor, a `kill`, a window closing, an
//! ssh session dying — from a thing that does not care that the app has switched
//! raw mode on, taken the alternate screen and hidden the cursor, and is not going
//! to read another byte from stdin. If the app does not answer them, the default
//! disposition kills it where it stands and the user inherits a terminal that
//! echoes nothing, has no cursor and, if the alternate screen was held, no
//! scrollback either.
//!
//! So they are answered, and answered with the same call the quit key makes
//! ([`crate::teardown::Teardown::restore`]): one way out, seen from two places.
//! A second teardown written for signals is a second teardown that can disagree
//! with the first, which is the exact failure `looprs-ecr` was filed for.
//!
//! # Why tokio and not a signal handler of our own
//!
//! A `sigwait`-style handler on a dedicated thread would answer the signal even if
//! the runtime were wedged, but it would need `libc`, and it would have to reach
//! the same `Teardown` from a thread that owns none of the app's async state. The
//! trade this makes is: the answer is delivered through the run loop's own
//! `select!`, which means it lands while the runtime is alive — which is the whole
//! time this app is running, because the loop never blocks on anything that is not
//! a bounded await. A process genuinely wedged inside a synchronous write is not
//! saved by a signal handler either; it is not saved by anything but `SIGKILL`,
//! and that is the case the *terminal* survives because the mode ledger's bytes
//! went out with the last flush before it.
//!
//! # What is deliberately not here
//!
//! `SIGQUIT`, `SIGSTOP`, `SIGKILL`. The last two cannot be caught at all. The
//! first is a core-dump request, and a program that turns it into a tidy shutdown
//! is a program that cannot be asked for a backtrace.

use std::io;

/// Which outside signal asked the app to leave.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// `SIGTERM` — `kill <pid>`, a supervisor, a system shutdown.
    Sigterm,
    /// `SIGHUP` — the terminal itself went away: window closed, ssh dropped.
    ///
    /// The one that used to be the worst of the two, because it arrives without
    /// anybody typing anything and the app has no chance to be mid-keystroke about
    /// it.
    Sighup,
    /// `SIGINT` from *outside* the terminal.
    ///
    /// Not the Ctrl-C the app reads as input: raw mode clears `ISIG`, so a
    /// Ctrl-C typed at this terminal is a byte on stdin and never becomes a
    /// signal. This is the one that arrives with `kill -INT`, which the shell
    /// would otherwise take as "die without cleaning up".
    Sigint,
}

/// The outside ask to leave, on a platform with no POSIX signals.
#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Termination {
    /// Ctrl-C on a Windows console — the closest thing this platform has.
    CtrlC,
}

impl std::fmt::Display for Termination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(unix)]
            Termination::Sigterm => f.write_str("SIGTERM"),
            #[cfg(unix)]
            Termination::Sighup => f.write_str("SIGHUP"),
            #[cfg(unix)]
            Termination::Sigint => f.write_str("SIGINT"),
            #[cfg(not(unix))]
            Termination::CtrlC => f.write_str("ctrl-c"),
        }
    }
}

/// The signals this app answers, with their handlers installed.
///
/// Built inside the runtime (the async context is where tokio wants it) and driven
/// by the run loop's `select!`. Holding it is what keeps the handlers installed:
/// drop it and the default dispositions come back, which is a terminal-left-raw
/// story again.
pub struct ExitSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
    #[cfg(not(unix))]
    _none: std::marker::PhantomData<()>,
}

impl ExitSignals {
    /// Take over the signals that mean "leave, and give the terminal back".
    ///
    /// Called before the loop starts rather than lazily, because a `SIGHUP` that
    /// lands before the handler is installed is a process killed with the ledger
    /// still holding everything it switched on.
    pub fn install() -> io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            Ok(Self {
                term: signal(SignalKind::terminate())?,
                hup: signal(SignalKind::hangup())?,
                int: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        {
            Ok(Self {
                _none: std::marker::PhantomData,
            })
        }
    }

    /// Resolves when one of them arrives. Polled from the run loop's `select!`;
    /// nothing else should be waiting on these.
    pub async fn recv(&mut self) -> Termination {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => Termination::Sigterm,
                _ = self.hup.recv() => Termination::Sighup,
                _ = self.int.recv() => Termination::Sigint,
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            Termination::CtrlC
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The name that reaches the log is the name a user greps for in a crash
    /// report, so it is spelled the way `kill -l` spells it.
    #[test]
    fn a_termination_names_itself_the_way_kill_does() {
        #[cfg(unix)]
        {
            assert_eq!(Termination::Sigterm.to_string(), "SIGTERM");
            assert_eq!(Termination::Sighup.to_string(), "SIGHUP");
            assert_eq!(Termination::Sigint.to_string(), "SIGINT");
        }
        #[cfg(not(unix))]
        {
            assert_eq!(Termination::CtrlC.to_string(), "ctrl-c");
        }
    }

    /// Installing is what replaces the default disposition. If it ever fails, the
    /// run loop must not start thinking it is answering signals it is not: the
    /// `Result` here is the app's last chance to say so before the point where a
    /// missed `SIGHUP` means a terminal left in the alternate screen.
    #[test]
    fn the_signals_install_inside_a_runtime() {
        // Constructed on a real runtime, which is what tokio's signal registry
        // requires, and dropped only after the runtime is done with it.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let sig = rt.block_on(async { ExitSignals::install() });
        assert!(sig.is_ok(), "install failed: {:?}", sig.err());
        drop(sig);
    }

    /// Delivering is not the hard part; being *awaited* is. This sends the signal
    /// to itself and requires the `recv` to resolve with the right variant, which
    /// is the only proof here that the handler is wired to the thing the run loop
    /// is sitting on.
    #[cfg(unix)]
    #[test]
    fn a_signal_sent_to_this_process_is_received() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let mut sig = ExitSignals::install().unwrap();
            // A timer, so a missed signal hangs the test rather than passing it.
            let send = tokio::spawn(async {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                // `kill(2)` through the shell: this crate carries no libc
                // dependency, and the test only needs the signal delivered.
                let _ = std::process::Command::new("kill")
                    .arg("-TERM")
                    .arg(std::process::id().to_string())
                    .status();
            });
            let got = tokio::time::timeout(std::time::Duration::from_secs(5), sig.recv()).await;
            assert_eq!(
                got,
                Ok(Termination::Sigterm),
                "SIGTERM never reached the loop"
            );
            let _ = send.await;
        });
    }
}
