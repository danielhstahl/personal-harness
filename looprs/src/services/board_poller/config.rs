//! The knobs: what the operator can turn, resolved once, from strings.
//!
//! Every number and switch this poller has is here, and every one of them is
//! resolved by a pure function of strings — `main` reads the environment and
//! decides nothing, and hands what it read to something that can be
//! table-tested without mutating a process. The resolution rules are the
//! lenient-and-loud ones the rest of the crate uses: a mistyped value falls
//! back, says which fallback it took, and never silently turns into a
//! different policy.
//!
//! The two configs are kept apart rather than merged into one bag of options
//! because they are read by different code: [`BoardConfig`] decides whether
//! the band exists and how often the tick fires, [`JournalConfig`] decides
//! what the cheap change detector asks between the full reads. A knob that
//! shares a resolver with three others can only be tested alongside them.

use std::time::Duration;

use crate::services::bd;

/// The default poll interval: **5 s** (ADR-0007 §4).
///
/// Not 1 s — the read is ~0.5 s of wall and ~0.3 s of CPU in a *separate
/// 130 MB process*, and the measured max read (623 ms at this board's size,
/// 690 ms at 50×) already overruns a 1 s tick, so a 1 s poller would spend its
/// life in `Delay` while contending with the loop's own `bd` traffic on the same
/// Dolt-backed board. Not 60 s — the band's whole job is to track movement, and
/// a minute of stale "To-do" after a bead was claimed is the band lying about
/// the one thing the user is watching it for.
pub const DEFAULT_POLL_MS: u64 = 5_000;

/// The floor a resolved interval is clamped up to.
///
/// The knob is a millisecond count typed by a human into a shell, and the two
/// typos in reach are `1` and `0`, either of which means "run `bd` as fast as
/// this machine allows, forever". A quarter-second floor turns a mistyped knob
/// into a fast board instead of a denial of service against a Dolt-backed repo —
/// and keeps it far enough off the read's own ~0.5 s cost that the process pool
/// is still left some air.
pub const MIN_POLL_MS: u64 = 250;

/// Everything the poller needs, resolved **once** from the environment in
/// `main`.
///
/// A struct rather than three loose arguments because all three come from the
/// same place and mean one thing together — "what the board *is* this run" — and
/// because they are the three questions the startup log line has to answer
/// anyway.
///
/// The resolution itself is [`BoardConfig::resolve`], a pure function of three
/// strings: `main` reads the environment and decides nothing, and hands what it
/// read to something that can be table-tested without mutating a process. The
/// change detector's two knobs resolve next door, in
/// [`JournalConfig::resolve`], and [`BoardConfig::from_env`] composes the two;
/// they are kept apart because they are read by different code and a knob that
/// shares a resolver with three others can only be tested alongside them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardConfig {
    /// The `bd` binary to read: `$LOOPRS_BD_BIN`, else `bd` — the same binary
    /// the beads loop uses, because two binaries in one run means two boards
    /// (ADR-0007 §5).
    pub bin: String,
    /// The tick period.
    pub interval: Duration,
    /// `LOOPRS_KANBAN=0` turns the whole board off.
    pub enabled: bool,
    /// The events-journal change detector: what to ask between the full reads,
    /// and how long it may be since one was last taken on trust.
    pub journal: JournalConfig,
}

/// The default full re-read period: **30 s**.
///
/// This is the bound on the one thing the journal cannot see — a change that
/// arrived rather than a change that happened: rows landing through
/// `bd dolt pull` / a merge are not journaled on this replica, and neither is
/// anything written with `bd sql`, nor anything at all while the journal is
/// switched off. Six ticks of the default poll, so the band is at worst half a
/// minute behind on those, while the changes a worker actually makes in this
/// workspace still land on the next 5 s tick.
pub const DEFAULT_RECONCILE_MS: u64 = 30_000;

/// The normal journal probe asks for **one** record.
///
/// The question is "has anything changed since my watermark?", and one record is
/// enough evidence to answer it. Asking for more would pay for payloads — every
/// journal record carries the whole issue as it stood after the mutation — that
/// nothing here reads.
pub const PROBE_LIMIT: i64 = 1;

/// What the probe asks for while the poller is still **behind** the head of the
/// journal.
///
/// A poller that starts against a journal with history in it cannot know its
/// watermark is 40 000 records back without reading something, so it drains a
/// bounded batch per tick instead. The cost stays honest because while it is
/// behind it re-reads the whole board every tick anyway — which is what it has
/// to do, not knowing whether the records it has not read yet touched a bead —
/// so a big journal *delays* the savings rather than costing more than the old
/// always-read behaviour. 512 records is a few hundred KB of transient payload
/// per tick, and ~10 ticks drains 5 000 records.
pub const CATCHUP_LIMIT: i64 = 512;

/// The change detector: the cheap read that decides whether the expensive one is
/// owed this tick.
///
/// Two knobs, one policy (ADR-0007 §7):
///
/// * **every tick, ask the journal** what has been mutated since the watermark —
///   ~0.15 s wall against the full board read's ~0.45 s, and flat in the size
///   of the board;
/// * **every `reconcile`, read the board regardless**, because the journal is
///   not a record of everything that can change what the band shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalConfig {
    /// `LOOPRS_KANBAN_EVENTS=0` takes the detector off: every tick is a full
    /// board read, exactly as this poller worked before the journal existed.
    pub enabled: bool,
    /// The longest the poller may go without a full board read, however quiet the
    /// journal is.
    pub reconcile: Duration,
}

impl Default for JournalConfig {
    /// On, with the ADR's 30 s sweep — the same pair
    /// [`JournalConfig::resolve`] falls back to when it is given nothing.
    fn default() -> Self {
        Self {
            enabled: true,
            reconcile: Duration::from_millis(DEFAULT_RECONCILE_MS),
        }
    }
}

impl JournalConfig {
    /// Resolve the detector's two raw values.
    ///
    /// Same lenient-and-loud rules as every other knob in this file: strings in,
    /// explained values out, no environment touched.
    ///
    /// * `events` — off only for an explicit `0` / `off` / `no` / `false`, the
    ///   rule [`BoardConfig`] uses for the board itself.
    /// * `reconcile_ms` — a millisecond count. Unparseable falls back to
    ///   [`DEFAULT_RECONCILE_MS`] with a warning. `0` is accepted rather than
    ///   refused: unlike the poll interval it cannot mean "spin", because the
    ///   tick still paces the poller — `0` simply makes every tick sweep, i.e.
    ///   the detector stops saving anything, which is what
    ///   `LOOPRS_KANBAN_EVENTS=0` says it wants and is said better there, so it
    ///   is worth a note on the way through.
    pub fn resolve(events: Option<&str>, reconcile_ms: Option<&str>) -> Self {
        let default = Duration::from_millis(DEFAULT_RECONCILE_MS);
        let reconcile = match trimmed(reconcile_ms) {
            None => default,
            Some(raw) => match raw.parse::<u64>() {
                Err(_) => {
                    tracing::warn!(
                        "LOOPRS_KANBAN_RECONCILE_MS={raw:?} is not a millisecond count; sweeping \
                         the full board every {DEFAULT_RECONCILE_MS}ms instead"
                    );
                    default
                }
                Ok(0) => {
                    tracing::warn!(
                        "LOOPRS_KANBAN_RECONCILE_MS=0 re-reads the whole board on every tick, which \
                         is what the change detector exists to avoid (to take it off outright, say \
                         LOOPRS_KANBAN_EVENTS=0)"
                    );
                    Duration::ZERO
                }
                Ok(ms) => Duration::from_millis(ms),
            },
        };
        Self {
            enabled: !is_off(events),
            reconcile,
        }
    }
}

impl BoardConfig {
    /// The board the environment asks for.
    ///
    /// **Call this in `main`, and once.** A knob re-read per frame is a knob
    /// whose value can change halfway through an answer, and a band that changes
    /// height because something else exported `LOOPRS_KANBAN` mid-run is a bug
    /// nobody can reproduce.
    pub fn from_env() -> Self {
        let mut cfg = Self::resolve(
            env_value("LOOPRS_KANBAN").as_deref(),
            env_value("LOOPRS_KANBAN_POLL_MS").as_deref(),
            Some(&bd::bd_bin_from_env()),
        );
        cfg.journal = JournalConfig::resolve(
            env_value("LOOPRS_KANBAN_EVENTS").as_deref(),
            env_value("LOOPRS_KANBAN_RECONCILE_MS").as_deref(),
        );
        cfg
    }

    /// Resolve the three raw values into a config.
    ///
    /// Pure, and taking strings rather than reading the process environment is
    /// the point: every rule below is then a row in a test table instead of a
    /// mutation of global state that ~700 other tests in this binary share.
    ///
    /// * `enabled` — off only for an explicit `0` / `off` / `no` / `false`,
    ///   case- and whitespace-insensitive: the same rule
    ///   [`copy_on_select_enabled`](crate::services::clipboard::copy_on_select_enabled)
    ///   uses. Anything else, including nonsense, leaves it on. A typo in a
    ///   variable nobody meant to set should not take a feature away.
    /// * `poll_ms` — see [`resolve_interval`].
    /// * `bin` — falls back to `bd`.
    pub fn resolve(enabled: Option<&str>, poll_ms: Option<&str>, bin: Option<&str>) -> Self {
        Self {
            bin: trimmed(bin).unwrap_or("bd").to_string(),
            interval: resolve_interval(poll_ms),
            enabled: !is_off(enabled),
            // The detector's own knobs have their own resolver and their own
            // entry point through `from_env`; a `resolve` that was handed two
            // strings it knows nothing about would have to guess them, so it
            // takes the documented default and says so.
            journal: JournalConfig::default(),
        }
    }
}

/// `LOOPRS_KANBAN_POLL_MS`, leniently.
///
/// Lenient means *loud*, not silent: every value that is not taken literally is
/// explained on its way to the value that is used. A silently ignored knob is a
/// knob the user keeps setting.
fn resolve_interval(raw: Option<&str>) -> Duration {
    let default = Duration::from_millis(DEFAULT_POLL_MS);
    let Some(raw) = trimmed(raw) else {
        return default;
    };
    let Ok(ms) = raw.parse::<u64>() else {
        tracing::warn!(
            "LOOPRS_KANBAN_POLL_MS={raw:?} is not a millisecond count; polling every \
             {DEFAULT_POLL_MS}ms instead"
        );
        return default;
    };
    if ms == 0 {
        // The one value that cannot be honoured: `0` on a tokio interval means
        // "as fast as possible", which here means a `bd` subprocess back-to-back
        // against a database that nobody else can then lock.
        tracing::warn!(
            "LOOPRS_KANBAN_POLL_MS=0 would run `bd` continuously; polling every \
             {DEFAULT_POLL_MS}ms instead (to take the board off entirely, say LOOPRS_KANBAN=0)"
        );
        return default;
    }
    if ms < MIN_POLL_MS {
        tracing::warn!(
            "LOOPRS_KANBAN_POLL_MS={ms} is under the {MIN_POLL_MS}ms floor — one read of the \
             board costs ~500ms on its own; clamped to {MIN_POLL_MS}ms"
        );
        return Duration::from_millis(MIN_POLL_MS);
    }
    Duration::from_millis(ms)
}

/// An explicit "no": `0`, `off`, `no`, `false`, in any case.
fn is_off(raw: Option<&str>) -> bool {
    matches!(
        trimmed(raw).map(|v| v.to_ascii_lowercase()).as_deref(),
        Some("0") | Some("off") | Some("no") | Some("false")
    )
}

/// Trimmed, and blank treated as unset — so `LOOPRS_KANBAN=""` is "not set"
/// rather than a value nobody meant.
fn trimmed(raw: Option<&str>) -> Option<&str> {
    raw.map(str::trim).filter(|v| !v.is_empty())
}

/// An env var as a value, with blank treated as unset.
fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}
