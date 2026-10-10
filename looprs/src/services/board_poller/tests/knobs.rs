//! //! The knobs, resolved: what the operator's environment means, and what it
//! //! cannot accidentally mean.
//! //!
//! //! Every test here is a resolution rather than a behaviour — the ADR's default
//! //! five seconds and a `bd` on the path, the only value that turns the band off,
//! //! the interval knob's lenient-and-loud fallbacks, the binary knob's
//! //! inject-and-fall-back, and the detector's own two knobs beside them. A knob
//! //! that silently means something else is worse than a knob that is missing, so
//! //! these are tables of strings in and policy out.

use crate::services::board_poller::config::{
    DEFAULT_POLL_MS, DEFAULT_RECONCILE_MS, JournalConfig, MIN_POLL_MS,
};

use super::*;

// ───────────────────────────── the knobs ─────────────────────────────

#[test]
fn the_default_is_the_adrs_five_seconds_and_a_bd_on_the_path() {
    let cfg = BoardConfig::resolve(None, None, None);
    assert_eq!(cfg.interval, Duration::from_secs(5));
    assert_eq!(cfg.bin, "bd");
    assert!(cfg.enabled, "the board is on unless somebody turns it off");
    assert_eq!(DEFAULT_POLL_MS, 5_000);
}

/// The on/off rule as a table: off **only** for an explicit no. A typo in
/// `LOOPRS_KANBAN` must not quietly take the band away.
#[test]
fn only_an_explicit_no_turns_the_board_off() {
    for off in ["0", "off", "OFF", "  no ", "False"] {
        assert!(
            !BoardConfig::resolve(Some(off), None, None).enabled,
            "{off:?} should mean off"
        );
    }
    for on in ["1", "yes", "true", "on", "kanban", "01", "2", "off-ish"] {
        assert!(
            BoardConfig::resolve(Some(on), None, None).enabled,
            "{on:?} is not an explicit no and must leave the board on"
        );
    }
}

/// Every interval the knob can produce, against what the knob asked for.
#[test]
fn the_poll_interval_knob_resolves_leniently_and_loudly() {
    assert_eq!(
        BoardConfig::resolve(None, Some("1500"), None).interval,
        Duration::from_millis(1500)
    );
    assert_eq!(
        BoardConfig::resolve(None, Some(" 5000 "), None).interval,
        Duration::from_secs(5),
        "whitespace is not part of the number"
    );
    // The two spellings of "spin": both fall back rather than turn into a
    // `bd` subprocess back-to-back.
    assert_eq!(
        BoardConfig::resolve(None, Some("0"), None).interval,
        Duration::from_secs(5)
    );
    assert_eq!(
        BoardConfig::resolve(None, Some("not-a-number"), None).interval,
        Duration::from_secs(5)
    );
    assert_eq!(
        BoardConfig::resolve(None, Some("   "), None).interval,
        Duration::from_secs(5),
        "blank is unset, not zero"
    );
    // Under the floor: clamped up, not honoured, not fatal.
    assert_eq!(
        BoardConfig::resolve(None, Some("1"), None).interval,
        Duration::from_millis(MIN_POLL_MS)
    );
    assert_eq!(
        BoardConfig::resolve(None, Some("249"), None).interval,
        Duration::from_millis(MIN_POLL_MS)
    );
    assert_eq!(
        BoardConfig::resolve(None, Some("250"), None).interval,
        Duration::from_millis(MIN_POLL_MS),
        "the floor itself is honoured"
    );
    // A slow board is a legitimate ask, not a bug to correct.
    assert_eq!(
        BoardConfig::resolve(None, Some("600000"), None).interval,
        Duration::from_millis(600_000)
    );
}

#[test]
fn the_binary_knob_takes_what_it_is_given_and_falls_back_to_bd() {
    assert_eq!(
        BoardConfig::resolve(None, None, Some("/tmp/fakes/bd")).bin,
        "/tmp/fakes/bd"
    );
    assert_eq!(BoardConfig::resolve(None, None, Some("   ")).bin, "bd");
    assert_eq!(BoardConfig::resolve(None, None, None).bin, "bd");
}

/// **The env read, exercised for real.** `from_env` is the only thing in
/// this module that touches the process environment, `main` is the only
/// thing that calls it, and those two facts together are why the other ~700
/// tests in this binary never touch it either.
///
/// Ignored because it mutates the environment the whole process shares —
/// which is exactly why edition 2024 made the write `unsafe`. Nothing else
/// in the ignored set reads these three variables, and every non-ignored
/// test goes through the pure `resolve`.
#[test]
#[ignore = "mutates the process environment"]
fn the_environment_configures_the_board_it_is_read_from_and_once() {
    // SAFETY: `#[ignore]`d, so this runs only when explicitly asked for,
    // and nothing else in that set reads these variables.
    unsafe {
        std::env::set_var("LOOPRS_KANBAN", "0");
        std::env::set_var("LOOPRS_KANBAN_POLL_MS", "1234");
        std::env::set_var("LOOPRS_BD_BIN", "/tmp/looprs-test/bd");
        std::env::set_var("LOOPRS_KANBAN_EVENTS", "0");
        std::env::set_var("LOOPRS_KANBAN_RECONCILE_MS", "9000");
    }
    let cfg = BoardConfig::from_env();
    assert!(!cfg.enabled, "LOOPRS_KANBAN=0 must turn it off: {cfg:?}");
    assert_eq!(cfg.interval, Duration::from_millis(1234), "{cfg:?}");
    assert_eq!(cfg.bin, "/tmp/looprs-test/bd", "{cfg:?}");
    assert!(!cfg.journal.enabled, "LOOPRS_KANBAN_EVENTS=0: {cfg:?}");
    assert_eq!(cfg.journal.reconcile, Duration::from_secs(9), "{cfg:?}");

    // SAFETY: as above; removed rather than blanked, so the second reading
    // is the *unset* case and not the blank-value one.
    unsafe {
        std::env::remove_var("LOOPRS_KANBAN");
        std::env::remove_var("LOOPRS_KANBAN_POLL_MS");
        std::env::remove_var("LOOPRS_BD_BIN");
        std::env::remove_var("LOOPRS_KANBAN_EVENTS");
        std::env::remove_var("LOOPRS_KANBAN_RECONCILE_MS");
    }
    let bare = BoardConfig::from_env();
    assert!(bare.enabled);
    assert_eq!(bare.interval, Duration::from_secs(5));
    assert_eq!(bare.bin, "bd");
    assert!(
        bare.journal.enabled,
        "the detector is on by default: {bare:?}"
    );
    assert_eq!(
        bare.journal.reconcile,
        Duration::from_millis(DEFAULT_RECONCILE_MS)
    );
}

/// The detector's own two knobs, as a table.
#[test]
fn the_detector_knobs_resolve_leniently() {
    assert!(JournalConfig::resolve(None, None).enabled, "on by default");
    assert_eq!(
        JournalConfig::resolve(None, None).reconcile,
        Duration::from_millis(DEFAULT_RECONCILE_MS)
    );
    for off in ["0", "off", "OFF", "  no ", "False"] {
        assert!(
            !JournalConfig::resolve(Some(off), None).enabled,
            "{off:?} should mean off"
        );
    }
    for on in ["1", "yes", "on", "kanban", "nonsense"] {
        assert!(
            JournalConfig::resolve(Some(on), None).enabled,
            "{on:?} is not an explicit no"
        );
    }
    assert_eq!(
        JournalConfig::resolve(None, Some("60000")).reconcile,
        Duration::from_secs(60)
    );
    assert_eq!(
        JournalConfig::resolve(None, Some(" nonsense ")).reconcile,
        Duration::from_millis(DEFAULT_RECONCILE_MS),
        "unparseable falls back, loudly"
    );
    assert_eq!(
        JournalConfig::resolve(None, Some("   ")).reconcile,
        Duration::from_millis(DEFAULT_RECONCILE_MS),
        "blank is unset, not zero"
    );
    // `0` is honoured rather than refused: the tick paces the poller, so it
    // cannot spin. It just makes the detector pointless, which is warned.
    assert_eq!(
        JournalConfig::resolve(None, Some("0")).reconcile,
        Duration::ZERO
    );
}
