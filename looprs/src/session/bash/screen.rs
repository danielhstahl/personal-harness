//! The full-screen handover: resize, and who holds the screen while a child has it.
//!
//! A full-screen child (an editor, `top`, a pager) is the one case where the app
//! stops owning the terminal and forwards it. [`BashTask::resize`] re-sizes the
//! pty to the frame's band rather than the window;
//! [`BashTask::on_screen_change`] takes the bytes that changed and decides
//! whether they are app chrome or the child's own screen;
//! [`BashTask::foreground`] names what is holding the terminal right now; and
//! [`BashTask::release_screen_at_command_end`] hands it back when the command
//! exits — which is the seam ADR-0001 Q2 is about, and the reason a pager
//! quitting in Bash mode restores the frame instead of leaving its corpses on it.

use crate::session::bash::reap::clip;
use crate::session::bash::task::BashTask;

use portable_pty::PtySize;

use crate::session::SessionEvent;

impl BashTask {
    pub(super) fn resize(&mut self, rows: u16, cols: u16) {
        if rows == 0 || cols == 0 {
            return;
        }
        self.size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        if let Some(shell) = self.shell.as_mut() {
            shell.resize(self.size);
        }
    }

    /// The screen changed hands. The session says so; the UI decides what that
    /// means for its own drawing.
    pub(super) fn on_screen_change(&mut self, change: crate::screen::ScreenChange) {
        match change {
            crate::screen::ScreenChange::Takeover { alt } => {
                let what = self.foreground();
                self.note(format!(
                    "`{}` took the screen ({}); looprs stops drawing until it gives it back",
                    clip(&what, 60),
                    if alt {
                        "alt screen"
                    } else {
                        "cursor-addressed output"
                    }
                ));
                self.emit(SessionEvent::ScreenHeld { active: true });
            }
            crate::screen::ScreenChange::Release => {
                self.emit(SessionEvent::ScreenHeld { active: false });
            }
        }
    }

    /// The command currently in front of the shell, for a notice that has to name
    /// something rather than say "a child".
    pub(super) fn foreground(&self) -> String {
        self.outstanding
            .front()
            .cloned()
            .unwrap_or_else(|| "the shell".to_string())
    }

    /// Take the screen back when a command ends without the child having said it
    /// did — `vim` killed with `SIGKILL`, `less` closed by a signal, a program
    /// that never emits a leave sequence.
    ///
    /// The watcher can only read a release out of bytes; the command boundary is
    /// the other thing that certainly means the screen is free. Without this the UI
    /// would keep teeing into a screen nobody owns and never redraw itself, which
    /// looks exactly like the frozen pane this whole path replaced.
    pub(super) fn release_screen_at_command_end(&mut self) {
        if !self.screen.is_held() {
            return;
        }
        // Bytes the watcher was still deciding on belong to the screen that just
        // ended, and an alt-screen leave the program died before paying is paid on
        // its way out — otherwise the terminal stays on the dead program's screen
        // and looprs redraws into a buffer nobody is looking at.
        let owed = self.screen.force_release();
        if !owed.is_empty() {
            self.emit_bytes(&owed);
        }
        self.emit(SessionEvent::ScreenHeld { active: false });
    }
}
