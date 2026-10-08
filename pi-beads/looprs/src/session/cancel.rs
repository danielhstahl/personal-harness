//! Cancellation, spelled the same way in all three terminal states (looprs-5g7).
//!
//! Every mode gets its own mechanism — a beads pass parks, Pi does `clear_queue` +
//! `abort`, Bash gets `0x03` — but the *user* gets one contract, and a contract
//! with three different vocabularies is three contracts. So the words and the one
//! number live here:
//!
//! | state when Esc lands | what the session does | what the user sees |
//! |---|---|---|
//! | nothing in flight | nothing | nothing: a no-op is not an error |
//! | submitted but not yet at the child | dequeue it; there is nothing to signal | `cancelled <thing> before it started` |
//! | work in flight, cancel accepted | tell the child now, do not wait | `cancelling <thing>…` straight away |
//! | the child unwinds | the run is over | `cancelled` |
//! | the child does not unwind within [`GRACE`] | escalate, per mode (ADR-0003) | a loud line naming what is stuck, and the way out |
//!
//! The middle row is why [`arm`] exists. A session must never *wait* on its own
//! cancel — that is ADR-0002's "the Router never blocks on a child" one level
//! down, and a session that awaited its `abort` would put the keystroke behind the
//! very run the user is trying to stop. So the deadline is not a wait but another
//! command, posted back into the session's own mailbox, handled in order with
//! everything else. Which also means every session has to answer the question the
//! timeout raises — "is this deadline still mine?" — the same way it answers a
//! late event from a dead generation: by checking a serial/attempt counter rather
//! than assuming.

use std::time::Duration;

use tokio::sync::mpsc;

/// How long a cancelled run gets to unwind before the session stops trusting it.
///
/// Three seconds, not one. The acceptance bar is that a *responsive* child stops
/// in under a second, so a cancel that has not landed by three is not "slow", it
/// is "not going to" — and a shorter grace would escalate on a tool that was
/// still cleaning up after itself, which is the false alarm this number exists to
/// avoid. It is one constant in one place on purpose: all three ladders fire off
/// it, so retuning the ladder is not a hunt through three files.
pub const GRACE: Duration = Duration::from_secs(3);

/// The first word of the ladder: the keystroke landed and there was something to stop.
///
/// Emitted *before* the round trip to the child, not after. "cancelling…" that
/// only appears when the child answers is the silence this row exists to remove.
pub fn started(what: &str) -> String {
    format!("cancelling {what}…")
}

/// The word for a cancel that arrived **before the child ever had the command**.
///
/// A Bash command typed at a shell that has not printed its first prompt is still
/// held in the session's own queue: there is no process to signal, nothing
/// unwinding, and no grace to run out — the cancel *is* the dequeue. It still
/// gets a word, for the same reason the row above it in the module table gets
/// none: silence here is not the same as the no-op case, because the command would
/// otherwise **start**. A cancel that quietly does nothing while the thing it was
/// aimed at runs a moment later is the worst answer a cancel key can give, and
/// "nothing outstanding" is exactly where it used to hide.
///
/// `plural` says which, so the sentence agrees without a second helper.
pub fn dropped_before_start(what: &str, plural: bool) -> String {
    if plural {
        format!("cancelled {what} before they started")
    } else {
        format!("cancelled {what} before it started")
    }
}

/// The second word: it stopped.
pub const DONE: &str = "cancelled";

/// The third word: it did not stop, with the wait named instead of hidden.
pub fn stalled(what: &str) -> String {
    format!("`{what}` is still running {GRACE:?} after the cancel")
}

/// Post `cmd` into `tx` after [`GRACE`].
///
/// Generic over the command type because each session owns its own command enum;
/// what is shared is the shape — a timer that arrives as a message, so no session
/// blocks on a child and no escalation needs a thread. A closed mailbox is
/// silence by definition: the session is already gone, and there is nobody left to
/// warn.
pub fn arm<C>(tx: mpsc::UnboundedSender<C>, cmd: C)
where
    C: Send + 'static,
{
    tokio::spawn(async move {
        tokio::time::sleep(GRACE).await;
        let _ = tx.send(cmd);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wording is user-visible, and "no message" is as much a part of the
    /// contract as the messages are: an idle Esc must stay quiet, which is only
    /// assertable if the busy case has a stable string to compare against.
    /// The queued case has to be *sayable* too: it is the difference between "I
    /// took your command away" and a keystroke that vanished.
    #[test]
    fn the_queued_cancel_names_the_command_and_says_it_never_ran() {
        assert_eq!(
            dropped_before_start("`sleep 30`", false),
            "cancelled `sleep 30` before it started"
        );
        assert_eq!(
            dropped_before_start("3 queued commands", true),
            "cancelled 3 queued commands before they started"
        );
        assert_ne!(
            dropped_before_start("`sleep 30`", false),
            started("`sleep 30`"),
            "the queued case must not read as if a byte was sent at something"
        );
    }

    #[test]
    fn the_three_words_are_stable() {
        assert_eq!(started("the Pi run"), "cancelling the Pi run…");
        assert_eq!(DONE, "cancelled");
        assert!(
            stalled("sleep 30").contains("sleep 30")
                && stalled("sleep 30").contains("still running"),
            "{}",
            stalled("sleep 30")
        );
    }

    /// The grace has to be long enough not to cry wolf at a tool that is still
    /// unwinding, and short enough that a wedged one is not silently stuck.
    #[test]
    fn the_grace_is_a_warning_window_not_a_hope() {
        assert!(
            GRACE >= Duration::from_secs(2),
            "a grace under the acceptance bar would escalate on healthy unwinds"
        );
        assert!(
            GRACE <= Duration::from_secs(5),
            "past five seconds a stuck child reads as a hang, not as a stalled cancel"
        );
    }

    /// `arm` delivers, and delivers *late* — the whole escalation design rests on
    /// the timeout being a message that arrives after the child had its chance.
    #[tokio::test]
    async fn arm_delivers_the_command_after_the_grace() {
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        let started_at = std::time::Instant::now();
        arm(tx, 7);
        assert!(
            rx.try_recv().is_err(),
            "arming must not deliver immediately"
        );
        let got = tokio::time::timeout(GRACE * 3, rx.recv())
            .await
            .expect("the armed command never arrived")
            .expect("the mailbox closed");
        assert_eq!(got, 7);
        assert!(started_at.elapsed() >= GRACE, "it arrived early");
    }

    /// Arming into a dead mailbox is not a panic and not an error: the session is
    /// gone, so there is nothing to escalate about.
    #[tokio::test]
    async fn arming_into_a_closed_mailbox_is_silence() {
        let (tx, rx) = mpsc::unbounded_channel::<u32>();
        drop(rx);
        arm(tx, 1);
        tokio::time::sleep(GRACE + Duration::from_millis(50)).await;
    }
}
