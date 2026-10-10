//! The toast: one short-lived line that confirms a thing the user just did
//! (looprs-pdl.10).
//!
//! # Why this is a state type and not a string in the frame
//!
//! The copy confirmation has three things that all have to agree and none of them
//! belong to the renderer: *what to say* (which comes from the sink's
//! [`CopyOutcome`](crate::services::clipboard::CopyOutcome), possibly late),
//! *how long to say it* (a constant, so a test can name it and a user can rely
//! on it), and *what makes it go away early* (the next key or click, per
//! ADR-0004 R21). A `String` field on `App` would answer none of those and would
//! be set from three places at once.
//!
//! # Why an overlay and not a row
//!
//! Because a row is a reshape, and a reshape moves the text the user just
//! selected out from under the pointer. ADR-0004 R21 prices the hole a reshape
//! leaves at 8.8–9.3 ms worst-case; a copy fires one of those per release, and
//! the whole point of the toast is that it arrives at the moment the user is
//! still looking at the selection. The widget that draws this therefore covers
//! cells in a fixed slot and changes no geometry anywhere — see
//! [`crate::components::toast`].
//!
//! # What it is not
//!
//! Not the status row. That row is a priority ladder with `ERROR` on top ("the
//! error is never dropped, only shortened"), and a copy toast must neither
//! displace an error nor be dropped because the ladder is full. Not a
//! `Notifier` message either: R18 is emphatic that the person who just dragged
//! a selection is, by construction, *at this terminal*, so its confirmation
//! belongs here and the durable artifact is the clipboard contents rather than
//! the message about them.

use std::time::{Duration, Instant};

/// How long a toast stays up before it dismisses itself (ADR-0004 R21: 2 s).
///
/// A constant and not a parameter: "the toast will still be there when I look up"
/// and "the toast is not in my way" are the same property, and the user should
/// not have to know that they were tuned separately.
pub const TOAST_TTL: Duration = Duration::from_secs(2);

/// How a toast is painted. The *words* carry the meaning; this only says whether
/// to shout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Something worked, or went somewhere it cannot be confirmed to have reached.
    Good,
    /// Something did not work.
    Bad,
}

/// One toast, with the moment it was shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Toast {
    text: String,
    tone: Tone,
    shown_at: Instant,
}

impl Toast {
    pub fn new(text: impl Into<String>, tone: Tone, shown_at: Instant) -> Self {
        Self {
            text: text.into(),
            tone,
            shown_at,
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn tone(&self) -> Tone {
        self.tone
    }

    /// Is its time up?
    ///
    /// Uses `saturating_duration_since` because this clock is the App's, which
    /// `on_tick` advances from the message time — and a test (or a resumed
    /// machine) can hand that value anything, including something earlier than
    /// the toast's own `Instant`. Saturating means a clock that goes backwards
    /// keeps the toast up rather than dismissing everything at once.
    pub fn expired(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.shown_at) >= TOAST_TTL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_toast_is_live_until_the_ttl_and_not_one_tick_before() {
        let shown = Instant::now();
        let t = Toast::new("Copied 3 characters", Tone::Good, shown);
        assert!(
            !t.expired(shown + Duration::from_millis(1999)),
            "still within the 2 s window"
        );
        assert!(t.expired(shown + TOAST_TTL), "the TTL is the boundary");
    }

    /// A clock that goes backwards must not dismiss everything: the expiry is a
    /// *duration since*, and a negative one saturates to zero, which is "just
    /// shown".
    #[test]
    fn a_clock_that_goes_backwards_keeps_the_toast() {
        let early = Instant::now();
        let late = early + Duration::from_secs(1);
        // Toast shown at the *later* instant, asked about at the earlier one:
        // how the App's clock behaves when a message's timestamp is behind the
        // moment the toast was painted.
        let t = Toast::new("x", Tone::Bad, late);
        assert!(!t.expired(early));
    }

    #[test]
    fn the_text_and_the_tone_are_what_was_put_in() {
        let now = Instant::now();
        let t = Toast::new("Copy failed: no clipboard", Tone::Bad, now);
        assert_eq!(t.text(), "Copy failed: no clipboard");
        assert_eq!(t.tone(), Tone::Bad);
    }
}
