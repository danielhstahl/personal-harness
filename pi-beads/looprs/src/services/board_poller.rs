//! The board poller: one task, one cheap probe per tick, the whole board only
//! when it is owed, one **newest** snapshot (looprs-5o4.2, under ADR-0007 —
//! `docs/adr/0007-kanban-board.md`; the operator-facing page for what this
//! feeds is `docs/kanban.md`).
//!
//! The user's ask was "it could have its own thread that periodically checks for
//! bead status". In this codebase that is a `tokio` task, and the precedent is
//! [`Ntfy::spawn`](crate::services::notification::Ntfy::spawn) over in
//! `services::notification`: a long-lived worker built once in `main`, created
//! before the Router so it outlives every session — because a beads session is
//! respawned per generation and parked on Tab, and a board that died and came
//! back along with it would go blank exactly when the user asks it the question
//! it exists to answer.
//!
//! # What it publishes, and why not onto the bus
//!
//! A [`BoardSnapshot`] into a [`tokio::sync::watch`]. Latest-wins, by the
//! primitive whose entire job is "keep the newest value".
//!
//! The bus is the wrong channel here and the ticket says why: [`crate::bus`]
//! deliberately never drops or merges a non-`BashOutput` message, so one `Msg`
//! per poll would accumulate snapshots the UI then replays *in order* — a user
//! watching a board that polls faster than the frame drains would see last
//! minute's board arrive one row at a time. Nothing about a poll is an event
//! worth keeping: only the newest value means anything, and a queue of old ones
//! is the same lie delivered slowly.
//!
//! # What it owns
//!
//! * **The schedule.** A [`tokio::time::Interval`] with
//!   [`MissedTickBehavior::Delay`], so a read that overruns the interval pushes
//!   the next tick out instead of firing a burst of catch-up reads.
//! * **The read.** [`bd::board_read_with`](crate::services::bd::board_read_with)
//!   — `bd --readonly list --all --limit 0 --json`, one per tick that is owed
//!   one, already `tokio::process` under
//!   [`BD_TIMEOUT`](crate::services::bd::BD_TIMEOUT). Nothing in this path is a
//!   blocking `std::process::Command`, which is the whole reason the UI does not
//!   stall behind a Dolt-backed board (looprs-037).
//! * **The change detector.**
//!   [`bd::journal_probe_with`](crate::services::bd::journal_probe_with) —
//!   `bd --readonly events tail --since <watermark>`, a fraction of the cost of
//!   the board read and flat in the size of the board, plus the sweep that keeps
//!   it honest. The whole section below is about it.
//! * **The mapping, by delegation.** [`BoardSnapshot::from_beads`] owns the
//!   status→column function; this file never learns that a column exists.
//!
//! **Two reads cannot be in flight, and that is structural rather than
//! scheduled.** There is one task and it `.await`s its read, so the second read
//! cannot start until the first has returned. That is the property the ticket
//! asks for — "two concurrent `bd` reads on a Dolt-backed board also means two
//! snapshots racing to be the *latest*" — and it is why there is no lock, no
//! generation counter and no in-flight flag here to get wrong: the task *is*
//! the mutex. `Delay` is what keeps the *schedule* honest underneath it, so a
//! board that takes longer than the tick pays in staleness rather than in load.
//!
//! # The three rules this file exists to keep
//!
//! The contract, stated once, each with the failure it is there to prevent. A
//! later editor who breaks one of these breaks the band in a way no test of the
//! widget will explain.
//!
//! 1. **Latest-wins, never one bus message per poll.** The channel is a
//!    [`tokio::sync::watch`], not [`crate::bus`]. The failure prevented: a
//!    replayed backlog of old boards. The bus deliberately never drops or merges
//!    a non-`BashOutput` message — a board snapshot would be exactly such a
//!    message — so one message per poll would queue every snapshot the poll
//!    outran and deliver them *in order*: a user watching a board that polls
//!    faster than the frame drains would see last minute's board arrive one row
//!    at a time. Nothing about a poll is an event worth keeping; only the newest
//!    value means anything. (§"What it publishes, and why not onto the bus"
//!    below has the long version.)
//! 2. **An error retains the last good snapshot.** A failed read publishes the
//!    error over the previous value rather than replacing it, and the band dims
//!    what it still has and says what went wrong. The failure prevented: a
//!    populated board going blank because `bd` hiccuped — "bd is broken" read
//!    as "the board is empty", which is looprs-037's conflation rebuilt one
//!    layer up. (§"What it never does" below.)
//! 3. **A poll that changed nothing paints nothing.** Under the change detector
//!    a quiet tick does not even *publish* — there is no news to carry, and a
//!    `watch` wakes its reader on every send. What that makes safe is that the
//!    footer's freshness never came from the publish anyway: the value carries
//!    [`fetched_at`](crate::state::board::BoardSnapshot::fetched_at) and the
//!    frame re-derives the age from it on the App's own tick
//!    ([`crate::App::on_tick`]), so the last painted frame keeps stating the
//!    age of the rows whether or not the last tick sent anything. The failure
//!    prevented: a board that has not moved making an untouched terminal repaint
//!    every tick forever, which is the exact opposite of the "the band costs
//!    nothing while nothing happens" claim it is sold on, and a busy loop next to
//!    a session that is trying to use the CPU.
//!
//!    Deduplicating in the **poller** by *suppressing the age* is the tempting
//!    wrong shape and is still what this rule is against: freeze the freshness
//!    marker and it starts telling the opposite of the truth about how long it
//!    has been since anybody looked. What changed with the detector is that a
//!    quiet tick publishes nothing; what has not changed is that the age is
//!    never the thing being deduplicated, and never frozen. (The other half of
//!    the rule still runs on every read that *does* happen: `adopt_board`'s
//!    [`BoardSnapshot::same_paint_as`] comparison is what stops a re-read of an
//!    unchanged board costing a frame.)
//!
//! # The change detector: ask what changed, don't re-look at everything
//!
//! Every tick used to cost a whole `bd list`: ~0.45 s of wall and ~0.18 s of CPU in
//! a second 130 MB process, whether or not anything had happened since the last
//! tick. `bd` keeps a **durable events journal** — every mutation through its write
//! paths recorded in-transaction as an ordered, replayable row — and reading it
//! answers the question the poller actually asks. Measured on this repo's board:
//!
//! | read | wall | CPU |
//! |---|---|---|
//! | `--readonly list --all --limit 0 --json` (what a tick used to be) | ~0.45 s | ~0.18 s |
//! | `--readonly events tail --since <head> --json` (a quiet tick now) | ~0.15 s | ~0.09 s |
//!
//! So a tick is: probe the journal, and read the board **only** when it reports
//! records, when the periodic sweep is due, or when the probe could not answer.
//! Changes a worker makes in this workspace still land on the next 5 s tick; what
//! changed is that the idle board no longer pays for the busy one. The band still
//! never replays history into the picture — the journal is read for *whether*, never
//! for *what*, and the rows always come from one consistent `bd list`.
//!
//! **The journal is not a mirror of the board, so the sweep is not optional.** Four
//! things can change what the band shows without a journal record here:
//!
//! * `bd dolt pull` / a merge — the rows arrived as data, not as a mutation this
//!   replica made, and `bd` says plainly that they are not journaled;
//! * `bd sql` and anything else that bypasses the write paths;
//! * a workspace with `events-journal` switched off — which answers the probe with
//!   *nothing*, on a successful exit, forever;
//! * retention: the floors prune the prefix, so a watermark can be pruned out from
//!   under the consumer.
//!
//! Hence [`JournalConfig::reconcile`]: whatever the journal says, the board is read
//! in full every 30 s by default. That bound is the whole cost of trusting the
//! journal — a change that never journalled is visible on the band up to half a
//! minute later instead of one tick later — and it is tunable, including down to
//! "every tick", which is the pre-journal behaviour.
//!
//! **The watermark is what makes the probe safe, and it moves only on a successful
//! read.** [`decide`] states the three rules that keep it honest; the one worth
//! stating twice is that the seq adopted after a full read is the one the probe saw
//! *before* that read started. Adopting a later one would let a mutation that landed
//! mid-read be marked covered by rows that do not contain it, which is a silently
//! lost change; the rule instead costs one redundant read next tick.
//!
//! **A quiet tick publishes nothing.** Nothing was learned that the picture already
//! had, and a `watch` send wakes the run loop whether or not the value moved. The
//! footer's age is not carried by the send — see rule 3 above — so nothing about
//! what the user sees depends on it.
//!
//! **Why not `bd events tail --follow`?** A streaming child would answer new records
//! with no polling at all, and it is not taken: it replaces a bounded request under
//! `BD_TIMEOUT` with a long-lived process holding a connection into a Dolt-backed
//! board that the beads loop has to write to, plus its own restart, backoff and
//! EOF-reconnect story. The poller's whole shape is one task that `.await`s one
//! bounded read at a time; a permanent child is a different shape with strictly more
//! that can go wrong, and the remaining win over a 0.15 s probe is small.
//!
//! # What it never does
//!
//! **Throw away the last good snapshot.** A failed read publishes
//! [`BoardSnapshot::with_error`] over the previous value: same rows, same
//! `fetched_at`, the error's own variant on top. The band then dims the rows it
//! still has and says what went wrong, which is ADR-0007 §4's failure table
//! rendered off one field. A poll that fails must never be able to make a
//! populated board look empty — that conflation is looprs-037's, one layer up.
//!
//! **Guess at the board.** It never fabricates a snapshot. Before the first read
//! returns, the published value is [`BoardSnapshot::loading()`]: `never_loaded`,
//! which is a third answer alongside "empty" and "broken" — and the reason
//! [`BoardRead`](crate::state::board::BoardRead) is an enum rather than a
//! `Result<_, String>`, so the widget never has to work out which of the three
//! it is looking at by matching on a message.
//!
//! # Testability, same seam as everything else in this service
//!
//! The `bd` binary arrives as a `String` on [`BoardConfig`], so every test here
//! runs against [`crate::testing::BdFake`] and nothing in the suite touches a
//! real board, a database or the network. `default()` and
//! [`SessionConfig::default`](crate::session::SessionConfig::default) keep
//! carrying no-op variants for the same reason ~550 tests are off the clipboard
//! and off ntfy by construction — and the poller's own no-op variant is a
//! `BoardConfig` with `enabled: false`, which spawns no task at all and so
//! cannot run a process even by accident.
//!
//! # Lifecycle
//!
//! Built once, in `main`, by [`BoardPoller::spawn`], which hands back the two
//! ends: the owner, and the read handle (which is `Clone`, so more readers cost
//! nothing).
//!
//! Dropping the [`BoardPoller`] **aborts** the task. That is not tidiness: the
//! alternative is waiting for the read in flight, which is up to
//! [`BD_TIMEOUT`](crate::services::bd::BD_TIMEOUT) of holding a live `bd`
//! child behind a terminal the user has already left. Aborting drops the pending
//! `wait_with_output` future inside
//! [`bd::run`](crate::services::bd), whose `kill_on_drop(true)` is already this
//! crate's answer to "who cleans up a child nobody waited on"; the abort is the
//! half that makes that answer arrive *on the exit path* rather than eventually.
//!
//! The other end of the leash is the reader side: a poller nobody is reading
//! stops itself, so a handle that goes away does not leave a task publishing to
//! an empty room.

use std::time::{Duration, Instant};

use tokio::sync::watch;
use tokio::sync::watch::Sender;
use tokio::time::MissedTickBehavior;

use crate::services::bd::{self, BdError};
use crate::state::board::BoardSnapshot;

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

/// The poller: the **owner** of the polling task.
///
/// `main` holds this. Readers want [`BoardHandle`], not this — taking the poller
/// by value would mean taking the right to end it.
#[derive(Debug)]
pub struct BoardPoller {
    /// `None` for a disabled board: the never-polling variant, which cannot run
    /// a process because there is nothing running that could.
    task: Option<tokio::task::JoinHandle<()>>,
}

impl BoardPoller {
    /// Start polling, and hand back both ends of the deal: the owner, whose drop
    /// ends the task, and a handle to read the board through.
    ///
    /// The channel starts life holding [`BoardSnapshot::loading()`] so that a
    /// reader arriving before the first read returns has something to look at
    /// that is not a blank band — "not asked yet" gets drawn, never guessed as
    /// "no beads".
    ///
    /// With the board switched off this builds a handle and **no task**: nothing
    /// to shut down, nothing to leak, and no code path left that can reach `bd`
    /// by accident.
    pub fn spawn(cfg: BoardConfig) -> (Self, BoardHandle) {
        let (tx, rx) = watch::channel(BoardSnapshot::loading());
        if !cfg.enabled {
            tracing::info!(
                "kanban board: off (LOOPRS_KANBAN=0) — no poller task, and no `bd` read will \
                 be made"
            );
            return (Self { task: None }, BoardHandle { rx, enabled: false });
        }
        if cfg.journal.enabled {
            tracing::info!(
                "kanban board: watching `{} --readonly events tail` every {:?}, reading `{} \
                 --readonly list --all --limit 0 --json` on a change or every {:?} \
                 (LOOPRS_KANBAN_POLL_MS / LOOPRS_KANBAN_RECONCILE_MS to retune, \
                 LOOPRS_KANBAN_EVENTS=0 to read the board every tick, LOOPRS_KANBAN=0 for none)",
                cfg.bin,
                cfg.interval,
                cfg.bin,
                cfg.journal.reconcile,
            );
        } else {
            tracing::info!(
                "kanban board: reading `{} --readonly list --all --limit 0 --json` every {:?} \
                 (change detector off — LOOPRS_KANBAN_EVENTS=0; the full read is the only read)",
                cfg.bin,
                cfg.interval,
            );
        }
        let task = tokio::spawn(poll_task(tx, cfg.bin, cfg.interval, cfg.journal));
        (Self { task: Some(task) }, BoardHandle { rx, enabled: true })
    }
}

impl Drop for BoardPoller {
    /// Ending the poller means ending the task **now**, not when it notices.
    ///
    /// The difference between the two is one read's worth of wall clock — up to
    /// [`BD_TIMEOUT`](crate::services::bd::BD_TIMEOUT) — during which the task
    /// is holding a live `bd` child. Aborting drops the pending
    /// `wait_with_output` future inside [`bd::run`](crate::services::bd), whose
    /// `kill_on_drop(true)` is the thing that keeps that child from going on
    /// running behind a terminal the user has already left.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// The read side of the board: a value, not a service.
///
/// The frame takes one of these and never learns that `bd` exists — ADR-0007
/// rule 10, "do not put `bd` in the draw path", is a property of this type
/// being a snapshot rather than a client.
#[derive(Clone, Debug)]
pub struct BoardHandle {
    rx: watch::Receiver<BoardSnapshot>,
    enabled: bool,
}

impl BoardHandle {
    /// The newest snapshot, **borrowed**.
    ///
    /// A `watch::Ref` is a read lock on the shared value: use it and drop it.
    /// Holding it across an `await` is how a reader ends up blocking the
    /// writer's next publish — and the poller publishes from a task the UI has
    /// no business delaying.
    pub fn borrow(&self) -> watch::Ref<'_, BoardSnapshot> {
        self.rx.borrow()
    }

    /// The newest snapshot as of `now`, with its `age` re-derived from the
    /// poller's stamp.
    ///
    /// This clones. At 60 fps that is the wrong shape — take [`Self::borrow`]
    /// and read [`BoardSnapshot::age_of_last_good`], or keep a snapshot of your
    /// own and [`BoardSnapshot::restamp_age`] it, which is one field write.
    /// This exists for whoever wants a value and does not care about the copy.
    #[allow(dead_code)] // deliberately not the frame's door: looprs-5o4.5 adopted the
    // *value* into `App` instead of cloning a board per paint, and keeps it
    // current by restamping one field per tick (`App::on_tick`). This is the
    // convenience read for a caller that wants a snapshot and not the copy.
    pub fn snapshot_at(&self, now: Instant) -> BoardSnapshot {
        let mut snap = self.rx.borrow().clone();
        snap.restamp_age(now);
        snap
    }

    /// Wait for the next snapshot. `false` once the poller is gone, which is the
    /// reader's cue that what it is holding is all it is ever going to get.
    ///
    /// A watch keeps **one** value, so a reader asleep through three polls wakes
    /// once and sees the third. The missed ones were never queued, because they
    /// were never the news.
    pub async fn changed(&mut self) -> bool {
        self.rx.changed().await.is_ok()
    }

    /// Is the board on at all? `false` for a `LOOPRS_KANBAN=0` run — the
    /// frame's cue not to draw a band, rather than to draw one that says
    /// `reading the board…` forever.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Why a tick owes the board a full read. Carried for the log and nothing else:
/// every one of these produces the same command line, and the reason is the only
/// thing an operator reading `looprs.log` can act on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadReason {
    /// Nothing has been read yet, so the band has no picture to be quiet about.
    First,
    /// The journal reported mutations since the watermark.
    Journal,
    /// The periodic sweep: the detector cannot see everything, so the board gets
    /// asked directly every so often regardless (ADR-0007 §7).
    Sweep,
    /// The probe could not be answered. Unknown, and unknown is never "nothing
    /// changed".
    ProbeFailed,
    /// Our checkpoint had been pruned out from under us; re-baselined at the head
    /// `bd` named.
    Rebaselined,
    /// `LOOPRS_KANBAN_EVENTS=0`: no detector, so the tick *is* the read.
    DetectorOff,
}

/// One tick's decision, made by [`decide`] out of the probe's answer.
#[derive(Debug, PartialEq, Eq)]
struct Decision {
    /// `Some` = read the whole board this tick, and why.
    read: Option<ReadReason>,
    /// The watermark a **successful** board read this tick adopts. `None` when
    /// there is nothing new to adopt, or when the read failed (see the task).
    adopt: Option<i64>,
    /// Whether the watermark is now proved to sit at the head of the journal,
    /// which is what lets the next probe ask for one record instead of a batch.
    at_head: bool,
    /// The detector's complaint, if it has one. The words are carried so the
    /// loud-once log line can say something specific.
    broken: Option<String>,
    /// `bd` said the events journal is disabled for this workspace.
    disabled: bool,
}

/// Decide one tick from one probe answer.
///
/// Pure, and split out of the task for the reason every other decision in this
/// crate is split out: this is the part carrying the safety argument, and a
/// policy that can only be exercised by racing a subprocess is observed, not
/// tested.
///
/// Three rules do all the work here, and each is the answer to a way the band
/// could lie:
///
/// 1. **`never_read` always reads.** A quiet journal is quiet *relative to a
///    watermark*; with no picture on screen there is nothing for that quiet to
///    be about, and the band would sit on `reading the board…` forever on a
///    board that is perfectly well populated.
/// 2. **A probe that failed never reads as "nothing changed"** — hence
///    `ProbeFailed` reading the board. The inverted failure, where a probe that
///    could not answer reported a quiet board, is the one way this design could
///    freeze the band on stale rows while the footer said `bd ok`.
/// 3. **`adopt` is what the probe saw *before* the read, never after.** The
///    read is a snapshot taken later than the probe, so the only watermark that
///    cannot outrun the rows is the probe's own. Adopting a seq observed after
///    the read would let a mutation that landed during the read be marked as
///    covered by rows that do not contain it — a lost change, silent forever.
///    The cost of this rule is one redundant read the next tick, which is the
///    correct trade by a wide margin.
fn decide(
    probe: Result<bd::JournalProbe, bd::JournalError>,
    limit: i64,
    never_read: bool,
    sweep_due: bool,
) -> Decision {
    match probe {
        Err(err @ bd::JournalError::Truncated { head, .. }) => Decision {
            read: Some(ReadReason::Rebaselined),
            // `head` came from `bd` before the board read that follows, so rule 3
            // holds: anything newer still has a seq above it.
            adopt: Some(head),
            at_head: true,
            // Reported as broken, and honestly so: until the re-baselined probe
            // comes back clean, this consumer *is* below the retained window. The
            // loud-once logging is what keeps a truncation that keeps failing
            // from becoming 720 lines an hour.
            broken: Some(err.to_string()),
            disabled: false,
        },
        Err(err) => Decision {
            read: Some(ReadReason::ProbeFailed),
            adopt: None,
            // Stay where we were, including "behind": a probe that could not
            // answer tells us nothing about where the head is, and the safe guess
            // is the one that keeps draining.
            at_head: false,
            broken: Some(err.to_string()),
            disabled: false,
        },
        Ok(p) => {
            let disabled = p.disabled;
            // A batch shorter than what was asked for means the journal has no
            // more records behind it — with the empty batch spelled out first,
            // because "quiet" and "drained to the head" are the same answer
            // arrived at two different ways and both count.
            let drained = p.is_quiet() || !p.hit_limit(limit);
            let read = match (p.head(), never_read, sweep_due) {
                (_, true, _) => Some(ReadReason::First),
                (Some(_), _, _) => Some(ReadReason::Journal),
                (None, false, true) => Some(ReadReason::Sweep),
                (None, false, false) => None,
            };
            Decision {
                read,
                // Rule 3: the watermark adopted is the probe's own highest seq,
                // never anything observed later. Nothing new in, nothing to
                // adopt, and `head()` is exactly that statement.
                adopt: p.head(),
                at_head: drained,
                broken: None,
                disabled,
            }
        }
    }
}

/// Log the detector's state at the volume it deserves: **loud once**.
///
/// Three facts worth an operator's attention, each worth exactly one line: the
/// detector broke (the band is now refreshing on the sweep alone, not on the
/// change), the journal is disabled (nothing will ever arrive here), and the
/// detector came back. Everything past the first is `debug` — a detector that
/// stays broken and warns every five seconds is 720 identical lines an hour,
/// which is not a signal but the reason people stop reading the log.
fn report_detector(
    decision: &Decision,
    reconcile: Duration,
    broken: &mut bool,
    disabled_reported: &mut bool,
) {
    if decision.disabled && !*disabled_reported {
        *disabled_reported = true;
        tracing::warn!(
            "kanban board: `bd` reports the events journal is disabled for this workspace — the \
             change detector will never report a change here, so the band refreshes on the full-board \
             sweep every {reconcile:?} instead (LOOPRS_KANBAN_EVENTS=0 says the same thing on purpose)"
        );
    }
    match &decision.broken {
        Some(why) => {
            if *broken {
                tracing::debug!("board change detector still unusable: {why}");
            } else {
                tracing::warn!(
                    "board change detector unusable: {why} — the band still gets a full board read \
                     every {reconcile:?}, just not the moment something moves"
                );
            }
            *broken = true;
        }
        None => {
            if *broken {
                tracing::info!("board change detector usable again");
            }
            *broken = false;
        }
    }
}

/// The task: probe, read if it is owed, publish. Until nobody is listening.
async fn poll_task(
    tx: Sender<BoardSnapshot>,
    bin: String,
    interval: Duration,
    journal: JournalConfig,
) {
    let mut tick = tokio::time::interval(interval);
    // `Delay`, not the default `Burst`, so the schedule re-baselines off the
    // tick that was actually received instead of banking missed ticks to hand
    // back later.
    //
    // Stated exactly, because the tokio semantics are subtler than the ticket's
    // phrasing and a comment that oversells this is worse than one that says
    // less: a read that overruns the period is *already late*, so the next
    // tick is delivered as soon as it is asked for under either setting. What
    // `Delay` rules out is the burst — the deadline moves to `now + period`
    // rather than advancing one `period` per consumed tick, which under `Burst`
    // leaves a schedule that never catches up and answers every read with an
    // immediate next one.
    //
    // The thing that actually makes "a poll that overruns the interval must not
    // overlap or pile up" true lives one level up: this is a single task, and it
    // `.await`s its read, so a second read cannot begin while the first is
    // still running. The task is the mutex; `Delay` is what stops the clock from
    // being the next problem.
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);

    // The **watermark**: the highest journal seq that a *successful full board
    // read* has reflected. Not "the highest seq we have seen", and the gap
    // between those two is where a lost change would live — see rule 3 on
    // [`decide`]. `0` is "nothing reflected yet", which combined with
    // `never_read` is also what forces the first tick.
    let mut since: i64 = 0;
    // Whether `since` is proved to sit at the head of the journal. False to
    // start, because against a journal with history in it the poller *is*
    // behind, and the honest way to find out is to drain a bounded batch per
    // tick rather than to guess.
    let mut at_head = false;
    // When the last full board read started. `None` until the first one, which
    // is the same fact as `never_read` and carries the sweep's clock too.
    let mut last_full_read: Option<Instant> = None;
    // Loud-once bookkeeping: the detector breaking is worth an operator's
    // attention exactly once per break, and worth `debug` every time after, for
    // the same reason a broken board is not logged 720 times an hour.
    let mut detector_broken = false;
    let mut disabled_reported = false;

    loop {
        tick.tick().await;

        // Nobody left to tell. A published value nobody reads is not a cache, it
        // is garbage with a channel attached to it.
        if tx.receiver_count() == 0 {
            tracing::debug!("board poller: no readers left, stopping");
            return;
        }

        // The stamp is taken **before** anything goes out, not when it comes
        // back, and that ordering does real work: it makes the distance between
        // two consecutive snapshots `max(interval, read)` — the next read cannot
        // start until its tick is due *and* the previous one has returned — so
        // the gap can never come out shorter than the interval. A stamp taken at
        // the end would fold the read's own duration into the measurement with
        // the opposite sign, and a cold first `bd` of 150 ms followed by a warm
        // 5 ms one would then look like a 155 ms gap on a 300 ms interval: a
        // burst the poller never committed.
        //
        // It also puts `age` on the side that can be trusted. A snapshot taken at
        // T is *at least* (now - T) old, which is the only honest direction for a
        // freshness marker; "not quite this fresh" is the direction that gets
        // people a stale board read as a live one.
        let taken_at = Instant::now();
        let sweep_due = last_full_read
            .is_none_or(|at| taken_at.saturating_duration_since(at) >= journal.reconcile);

        // ── 1. the cheap read: what has been done to the board lately? ──
        let limit = if at_head { PROBE_LIMIT } else { CATCHUP_LIMIT };
        let decision = if journal.enabled {
            decide(
                bd::journal_probe_with(&bin, since, limit).await,
                limit,
                last_full_read.is_none(),
                sweep_due,
            )
        } else {
            Decision {
                read: Some(ReadReason::DetectorOff),
                adopt: None,
                at_head: false,
                broken: None,
                disabled: false,
            }
        };
        at_head = decision.at_head;
        report_detector(
            &decision,
            journal.reconcile,
            &mut detector_broken,
            &mut disabled_reported,
        );

        let Some(reason) = decision.read else {
            // The quiet tick — and the whole point of the change detector. The
            // board on screen is still the board `bd` has, so nothing is
            // published: a `watch` wakes its reader on every send, and a send
            // carrying no news is a wake for nothing to do.
            //
            // What the footer says is unaffected, and this is the one place a
            // later editor could break the freshness rule by mistake. The age is
            // not carried by the publish: it is re-derived from the last read's
            // own `fetched_at` on the App's tick (`App::on_tick` →
            // `restamp_age`), so the frame keeps telling the truth about how old
            // the rows are whether or not this tick sent anything. What *is*
            // true, and worth knowing, is that the number therefore never goes
            // *down* on a quiet tick — it counts to the sweep, which resets it.
            tracing::debug!(
                "board poll: journal quiet at seq {since} (limit {limit}), no board read this tick"
            );
            continue;
        };

        // ── 2. the expensive read: the board itself ──
        match bd::board_read_with(&bin).await {
            Ok(beads) => {
                // The watermark moves **only** here, and only with what the probe
                // had seen before this read started. A read that failed must not
                // take the watermark with it: the changes this tick was reacting
                // to have not been reflected in anything, and leaving `since`
                // behind is what makes the next tick report them again.
                if let Some(upto) = decision.adopt {
                    since = since.max(upto);
                }
                last_full_read = Some(taken_at);
                tracing::debug!(
                    "board poll: full read ({:?}), watermark now {since}",
                    reason
                );
                let snap = BoardSnapshot::from_beads(&beads, Some(taken_at.elapsed()))
                    .stamped_at(taken_at);
                if tx.send(snap).is_err() {
                    return;
                }
            }
            Err(err) => {
                // The last good snapshot, kept exactly as it was, plus this
                // error's variant. Not cleared, not emptied, not replaced with
                // an `Ok(vec![])` — that would be an empty board, and this is
                // not one.
                let next = {
                    let prev = tx.borrow();
                    let age = prev.age_of_last_good(Instant::now());
                    let failing_again = prev.read.is_error();
                    (prev.with_error(&err, age), failing_again)
                };
                let (snap, was_failing) = next;
                log_read_error(&err, was_failing);
                if tx.send(snap).is_err() {
                    return;
                }
            }
        }
    }
}

/// One failed board read, logged at the volume the situation deserves.
///
/// Loud **once**. The move onto the failure table is the thing worth an
/// operator's attention; a board that stays broken and warns every five seconds
/// is 720 identical lines an hour, which is not a signal but the reason people
/// stop reading the log.
fn log_read_error(err: &BdError, already_failing: bool) {
    if already_failing {
        tracing::debug!("board poll still failing: {err}");
    } else {
        tracing::warn!("board poll failed: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::board::{BoardRead, Column};
    use crate::testing::{
        BOARD_VARIETY, BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD, PiFake, process_alive,
    };
    use std::time::Instant;

    /// The board read, exactly as ADR-0007 §3 fixes it — the string the tests
    /// count command lines against.
    const BOARD_READ: &str = "--readonly list --all --limit 0 --json";

    /// How long a test waits before concluding the poller is not going to answer.
    /// Everything awaited here is answered by a fake in milliseconds, so timing
    /// out means it is broken, not that the machine was slow.
    const WAIT: Duration = Duration::from_secs(10);

    /// The change detector with its sweep pinned to the tick: every tick is a
    /// sweep, so the read cadence is exactly what it was before the detector
    /// existed.
    ///
    /// Tests about *the tick* — its cadence, its error ladder, its shutdown,
    /// the three states it publishes — take this. They are not asking what the
    /// journal saved; leaving the production 30 s sweep in them would mean they
    /// read once and then wait for nothing to happen.
    fn sweep_every(interval: Duration) -> JournalConfig {
        JournalConfig {
            enabled: true,
            reconcile: interval,
        }
    }

    /// A detector that will not sweep within the test's lifetime: the journal is
    /// then the *only* thing that can make the poller re-read the board, which is
    /// what turns "the board moved because something said it did" into a real
    /// assertion rather than one the sweep quietly satisfies.
    fn never_sweep() -> JournalConfig {
        JournalConfig {
            enabled: true,
            reconcile: Duration::from_secs(3600),
        }
    }

    /// A poller plus its fake `bd`, kept alive together for the test's duration.
    ///
    /// Field order **is** teardown order: the poller is declared first so it is
    /// dropped first, which ends the task before the scratch dir it reads from is
    /// deleted. The other order leaves a task able to start one more read into a
    /// directory that no longer exists.
    struct Running {
        _poller: BoardPoller,
        fakes: Fakes,
        handle: BoardHandle,
    }

    /// A poller aimed at a fake `bd`.
    ///
    /// Callers pass the interval: 40–60 ms is what makes a 350 ms fake read span
    /// half a dozen ticks, which is the shape a 1 s interval and a 6 s read has
    /// for real — and it keeps the whole slow-fake group at about a second of
    /// wall clock.
    fn running(tag: &str, fake: BdFake, board: &str, interval: Duration) -> Running {
        running_journal(tag, fake, board, interval, sweep_every(interval))
    }

    /// A poller aimed at a fake `bd`, with the detector's own knobs set by the
    /// test rather than defaulted to "sweep every tick".
    fn running_journal(
        tag: &str,
        fake: BdFake,
        board: &str,
        interval: Duration,
        journal: JournalConfig,
    ) -> Running {
        let fakes = Fakes::new(tag, PiFake::Started, fake, board);
        let (poller, handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval,
            enabled: true,
            journal,
        });
        Running {
            _poller: poller,
            fakes,
            handle,
        }
    }

    /// Poll a condition rather than sleep a guessed amount: the fake answers in
    /// milliseconds, so a wait that runs past its budget is a failure rather
    /// than a race this test hopes to win.
    async fn until(what: &str, timeout: Duration, cond: impl Fn() -> bool) {
        let start = Instant::now();
        while !cond() {
            assert!(
                start.elapsed() < timeout,
                "{what} never happened in {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn wait_reads(fakes: &Fakes, n: usize) {
        until(&format!("{n} completed board read(s)"), WAIT, || {
            fakes.bd_stops() >= n
        })
        .await;
    }

    // ───────────────── reading the poller's own command log ─────────────────
    //
    // The watermark is state inside a task a test cannot reach, so every
    // assertion about it is made against the one place it is observable: the
    // `--since` number that went out on the last probe. That is also the
    // stronger form of the claim — it is the watermark as `bd` saw it, not as
    // the poller remembers it.

    /// The board-read lines in the fake's log, in order.
    fn board_reads(fakes: &Fakes) -> Vec<String> {
        fakes
            .bd_log()
            .into_iter()
            .filter(|l| l.as_str() == BOARD_READ)
            .collect()
    }

    /// The journal-probe lines in the fake's log, in order.
    fn probes(fakes: &Fakes) -> Vec<String> {
        fakes
            .bd_log()
            .into_iter()
            .filter(|l| l.starts_with("--readonly events tail"))
            .collect()
    }

    /// One `--flag <value>` out of a logged command line.
    fn flag(line: &str, want: &str) -> i64 {
        let parts: Vec<&str> = line.split_whitespace().collect();
        parts
            .iter()
            .position(|p| *p == want)
            .and_then(|i| parts.get(i + 1))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or_else(|| panic!("no {want} <number> in {line:?}"))
    }

    /// The `--since` of every probe, in order: the watermark's trajectory.
    fn probe_since(fakes: &Fakes) -> Vec<i64> {
        probes(fakes).iter().map(|l| flag(l, "--since")).collect()
    }

    /// The `--limit` of every probe, in order: which end of the catch-up /
    /// steady-state pair each tick was asking with.
    fn probe_limit(fakes: &Fakes) -> Vec<i64> {
        probes(fakes).iter().map(|l| flag(l, "--limit")).collect()
    }

    /// Wait until the poller's first board read has landed and it has had at
    /// least `ticks` probes since — i.e. it is settled and quiet, which is the
    /// state every detector test starts from.
    async fn wait_settled(r: &Running, ticks: usize) {
        until("the first read plus a settled probe stream", WAIT, || {
            probes(&r.fakes).len() >= ticks && !board_reads(&r.fakes).is_empty()
        })
        .await;
    }

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

    // ───────────────────────── the read it runs ─────────────────────────

    #[tokio::test]
    async fn the_poller_runs_two_fixed_reads_and_never_a_write() {
        let r = running(
            "poll-once",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(60),
        );
        wait_reads(&r.fakes, 4).await;

        let lines = r.fakes.bd_log();
        assert!(lines.len() >= 4, "expected several reads: {lines:?}");
        for line in &lines {
            let is_board = line == BOARD_READ;
            let is_probe = line.starts_with("--readonly events tail --since ")
                && line.contains(" --limit ")
                && line.ends_with("--json");
            assert!(
                is_board || is_probe,
                "the poller ran {line:?}, which is neither the board read nor the journal probe: \
                 {lines:?}"
            );
        }
        // Both halves actually ran, or this passed by running only one of them.
        assert!(
            lines.iter().any(|l| l == BOARD_READ),
            "no board read at all: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("--readonly events tail")),
            "no journal probe at all: {lines:?}"
        );
        // The band cannot change the board — as a property of the command line.
        let joined = lines.join(" ");
        for forbidden in [
            "update", "claim", "create", "close", "ready", "delete", "label", "prune",
        ] {
            assert!(
                !joined.contains(forbidden),
                "the poller ran {forbidden:?}, which either writes or re-derives: {lines:?}"
            );
        }
    }

    /// Zero network, zero real board: the poller only ever runs the binary it
    /// was handed. That the *fake* is the only thing that could have logged is
    /// the assertion — a poller wired to a real `bd` would leave this log empty
    /// while doing something much worse.
    #[tokio::test]
    async fn the_suite_never_touches_a_real_board_because_the_binary_is_injected() {
        let r = running(
            "poll-injected",
            BdFake::Ok,
            EMPTY_BOARD,
            Duration::from_millis(60),
        );
        wait_reads(&r.fakes, 2).await;
        assert!(
            r.fakes.bd_calls() >= 2,
            "the injected fake never ran, which would mean the poller found a real `bd` \
             somewhere instead: {:?}",
            r.fakes.bd_log()
        );
    }

    // ─────────────────── never_loaded / empty / broken ───────────────────
    //
    // looprs-037's rule one layer up, as three separate tests rather than one
    // test of three branches: the entire point is that these are three
    // different facts, so they get three independent ways to fail.

    /// Before the first read lands the board is `never_loaded` — **not** empty,
    /// and not an error. A slow fake is what makes this observable at all: with
    /// a fast `bd` the window closes before the test can look.
    #[tokio::test]
    async fn a_board_that_has_not_been_read_yet_is_never_loaded_not_empty() {
        let fakes = Fakes::new(
            "poll-never-loaded",
            PiFake::Started,
            BdFake::Slow,
            EMPTY_BOARD,
        );
        let (poller, handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval: Duration::from_millis(60),
            enabled: true,
            journal: sweep_every(Duration::from_millis(60)),
        });

        let snap = handle.borrow();
        assert_eq!(
            snap.read,
            BoardRead::Never,
            "before the first read the band says `reading the board…`"
        );
        assert_eq!(snap.total(), 0);
        assert_eq!(snap.age, None, "nothing read, so nothing has an age");
        assert_eq!(snap.fetched_at, None);
        // …and explicitly not the empty-board rendering.
        let empty = BoardSnapshot::from_beads(&[], Some(Duration::ZERO));
        assert_ne!(
            snap.read, empty.read,
            "not-loaded and empty are not one state"
        );
        assert!(
            !snap.read.is_error(),
            "waiting is not failing: {:?}",
            snap.read
        );
        for col in Column::ALL {
            assert_eq!(
                snap.header_count(col),
                "—",
                "{} is not-yet-counted, not zero-counted",
                col.name()
            );
        }
        drop(snap);
        drop(handle);
        drop(poller);
    }

    /// `Ok(vec![])` renders as three empty columns, because the board answered
    /// and its answer was nothing.
    #[tokio::test]
    async fn an_empty_board_is_an_answer_and_not_a_failure() {
        let r = running(
            "poll-empty",
            BdFake::Ok,
            EMPTY_BOARD,
            Duration::from_millis(60),
        );
        wait_snapshot(&r.handle, "an Ok snapshot", |s| s.read.is_ok()).await;

        let snap = r.handle.borrow();
        assert_eq!(snap.read, BoardRead::Ok, "the board answered");
        assert!(!snap.read.is_error());
        assert_eq!(snap.total(), 0);
        assert_eq!(snap.deferred, 0);
        for col in Column::ALL {
            assert!(snap.beads_in(col).is_empty(), "{}", col.name());
            // A counted zero, not the `—` of "cannot say".
            assert_eq!(snap.header_count(col), "0", "{}", col.name());
        }
        assert!(
            snap.fetched_at.is_some(),
            "a board that answered has a timestamp"
        );
    }

    /// The third of the three: `Err` is neither of the above, and arrives
    /// carrying which kind of broken it is.
    #[tokio::test]
    async fn a_board_that_cannot_be_read_is_not_a_board_with_nothing_on_it() {
        let r = running(
            "poll-broken-from-the-start",
            BdFake::Fails,
            EMPTY_BOARD,
            Duration::from_millis(60),
        );
        wait_snapshot(&r.handle, "a failed snapshot", |s| s.read.is_error()).await;

        let snap = r.handle.borrow();
        assert!(snap.read.is_error(), "{:?}", snap.read);
        // `BdFake::Fails` exits 3 without writing a single byte to stderr, which
        // makes this the sharpest form the looprs-037 rule can take: an error with
        // nothing to say is still an error, and never a board with nothing on it.
        assert_eq!(
            snap.read,
            BoardRead::Failed {
                code: Some(3),
                message: None
            },
            "the error arrives as its own variant, with its exit code intact"
        );
        // Never loaded, so nothing to be stale about — and neither of the other
        // two states this may be mistaken for.
        assert_eq!(snap.age, None);
        assert_ne!(snap.read, BoardRead::Never);
        assert_ne!(snap.read, BoardRead::Ok);
    }

    // ─────────────────────── the mapping it ships ───────────────────────

    /// The snapshot on the wire carries the ADR's mapping and the **true
    /// totals**, un-truncated: counts are the poller's job, truncation is the
    /// frame's.
    #[tokio::test]
    async fn the_snapshot_carries_the_whole_read_and_its_true_totals() {
        let r = running(
            "poll-mapping",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(60),
        );
        wait_snapshot(&r.handle, "a fully counted board", |s| {
            s.read.is_ok() && s.total() == 6
        })
        .await;

        let snap = r.handle.borrow();
        assert_eq!(snap.read, BoardRead::Ok);
        // BOARD_VARIETY: 3 to-do (open, blocked, unknown), 1 in progress,
        // 2 complete, 1 deferred-and-not-a-row.
        assert_eq!(snap.header_count(Column::ToDo), "3");
        assert_eq!(snap.header_count(Column::InProgress), "1");
        assert_eq!(snap.header_count(Column::Complete), "2");
        assert_eq!(snap.deferred, 1, "deferred is counted, never a row");
        // Nothing truncated: every bead in the read is accounted for exactly
        // once, which is ADR-0007's invariant I1 arriving intact through the
        // poll path.
        assert_eq!(snap.total() + snap.deferred, 7);
        assert_eq!(
            snap.beads_in(Column::Complete)
                .iter()
                .map(|b| b.id.as_str())
                .collect::<Vec<_>>(),
            vec!["looprs-done-1", "looprs-done-2"]
        );
        assert_eq!(
            snap.beads_in(Column::ToDo)
                .iter()
                .filter(|b| b.marker.is_some())
                .count(),
            2,
            "the blocked and unknown beads arrive marked, as the ADR maps them"
        );
    }

    // ───────────────────── the error ladder, over time ─────────────────────

    /// A failure keeps the last good rows and sets the error; a later success
    /// clears it. Both directions are the whole "never lose the last good
    /// snapshot" contract, and both have to be watched on one running poller
    /// rather than asserted on a hand-built value.
    #[tokio::test]
    async fn a_failure_keeps_the_last_good_board_and_a_later_success_clears_it() {
        let r = running(
            "poll-recovers",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(60),
        );
        wait_snapshot(&r.handle, "a good board to lose", |s| {
            s.read.is_ok() && s.total() == 6
        })
        .await;
        let good_rows = r.handle.borrow().columns.clone();
        let good_at = r
            .handle
            .borrow()
            .fetched_at
            .expect("a good read stamps its clock");

        // Break the board *after* it was readable: the case a user actually
        // hits, hours into a run.
        r.fakes.fail_bd(true);
        until("the poller noticed the board broke", WAIT, || {
            r.handle.borrow().read.is_error()
        })
        .await;

        let after = r.handle.borrow();
        assert!(after.read.is_error(), "{:?}", after.read);
        assert!(
            matches!(
                &after.read,
                BoardRead::Failed {
                    code: Some(3),
                    message: Some(m)
                } if m.contains("fail marker set")
            ),
            "the error carries the `bd` side of its own story: {:?}",
            after.read
        );
        assert_eq!(
            after.columns, good_rows,
            "an error must not touch the last good rows"
        );
        assert_eq!(after.deferred, 1, "…or the deferred count");
        let stamp = after
            .fetched_at
            .expect("the stamp survives the error that follows it");
        assert!(
            stamp >= good_at,
            "the stamp dates the last good read, not the failed one"
        );
        let age1 = after
            .age
            .expect("the kept rows carry an age: the footer needs it to say how stale");
        // The header stops counting — this read did not answer, so nothing new
        // may be counted from it.
        for col in Column::ALL {
            assert_eq!(after.header_count(col), "—", "{}", col.name());
        }
        drop(after);

        // A second failed tick, so the stamp is shown not to drift: it dates the
        // last good read, so it cannot move, while the age — the whole content of
        // the footer's `stale Ns` — has to grow.
        until("a second failed read", WAIT, || {
            let s = r.handle.borrow();
            s.read.is_error() && s.age.is_some_and(|a| a > age1)
        })
        .await;
        let again = r.handle.borrow();
        assert_eq!(
            again.fetched_at,
            Some(stamp),
            "failed reads must not refresh the stamp"
        );
        assert!(
            again.age.is_some_and(|a| a > age1),
            "…and the stale marker has to get staler, or the footer is lying about \
             a board nobody has looked at"
        );
        drop(again);

        // …and the recovery clears the error instead of leaving it sticky.
        r.fakes.fail_bd(false);
        until("the poller saw the board come back", WAIT, || {
            r.handle.borrow().read.is_ok()
        })
        .await;
        let back = r.handle.borrow();
        assert_eq!(back.read, BoardRead::Ok);
        assert!(!back.read.is_error(), "{:?}", back.read);
        assert_eq!(back.total(), 6, "the board is counted again");
    }

    // ─────────────────── no overlap, no pile-up, no orphans ───────────────────

    /// **Two reads in flight is impossible**, measured rather than argued.
    ///
    /// The fake records its pid on the way in and out (see
    /// `tests/fixtures/fake_bd.sh`), so "how many `bd` were alive at once" is
    /// a fact read off the log and not a thing the poller says about itself. With
    /// a 350 ms read against a 60 ms tick a poller that *could* overlap has an
    /// enormous amount of room to do it, which is exactly what this test buys.
    #[tokio::test]
    async fn a_slow_board_never_gets_two_reads_in_flight_at_once() {
        let r = running(
            "poll-no-overlap",
            BdFake::Slow,
            BOARD_VARIETY,
            Duration::from_millis(60),
        );
        wait_reads(&r.fakes, 3).await;

        assert_eq!(
            r.fakes.bd_max_concurrency(),
            1,
            "two `bd` reads overlapped: {:?}",
            r.fakes.bd_trace()
        );
        // Every read that started has finished, except the one in flight right
        // now. A started-and-never-finished pile is the other way this breaks.
        let (starts, stops) = (r.fakes.bd_starts(), r.fakes.bd_stops());
        assert!(
            starts - stops <= 1,
            "{starts} reads started and only {stops} finished: {:?}",
            r.fakes.bd_trace()
        );
    }

    /// Wait for the published **snapshot** to satisfy a condition.
    ///
    /// Not the same thing as counting the fake's finished reads: the fake marks
    /// itself done when it exits, and the read still has to be parsed and
    /// published before a reader can see it. A test about the snapshot waits on
    /// the snapshot; a test about the command line waits on the log.
    async fn wait_snapshot(
        handle: &BoardHandle,
        what: &str,
        cond: impl Fn(&BoardSnapshot) -> bool,
    ) {
        until(what, WAIT, || cond(&handle.borrow())).await
    }

    /// The distinct `fetched_at` stamps the poller has published, oldest first,
    /// waiting until `n` of them have landed.
    ///
    /// Consecutive-equality is what "distinct" means here: a watch holds one
    /// value, so a reader that arrives between two publishes sees the same stamp
    /// twice. Skipping a whole publish is possible under load, and can only ever
    /// make the *distance* between the stamps a test looks at longer.
    async fn distinct_stamps(handle: &BoardHandle, n: usize) -> Vec<Instant> {
        let mut stamps: Vec<Instant> = Vec::new();
        let began = Instant::now();
        while stamps.len() < n {
            assert!(
                began.elapsed() < WAIT,
                "only {} snapshot(s) arrived in {WAIT:?}, wanted {n}",
                stamps.len()
            );
            if let Some(at) = handle.borrow().fetched_at
                && stamps.last() != Some(&at)
            {
                stamps.push(at);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        stamps
    }

    /// Reads never come faster than the interval the knob was set to — measured
    /// from the **producer's own clock**, which is what makes it a test rather
    /// than a load measurement.
    ///
    /// Because the stamp is taken when a read *begins* (see `poll_task`), the
    /// distance between two consecutive stamps is `max(interval, read)`: the
    /// next read cannot start until its tick is due **and** the previous one has
    /// come back. So the gap cannot be shorter than the interval, and a reader
    /// that arrives late can skip whole snapshots but can never shorten the
    /// distance between the two it did see. Measuring the same property with
    /// the distance between the two it did see. Measuring the same property with
    /// wall-clock *arrival* times cannot promise that: this suite runs several
    /// hundred tests at once and has been seen to starve a single-threaded
    /// runtime for a second, which is a measurement error of the same size as
    /// the thing being measured. The producer's stamps are not subject to that,
    /// and so this test is the schedule's, not the machine's.
    ///
    /// The other half of "must not pile up" — no two reads in flight — is
    /// structural and tested by
    /// `a_slow_board_never_gets_two_reads_in_flight_at_once`.
    #[tokio::test]
    async fn reads_never_come_faster_than_the_configured_interval() {
        let interval = Duration::from_millis(300);
        let r = running("poll-cadence", BdFake::Ok, BOARD_VARIETY, interval);
        let stamps = distinct_stamps(&r.handle, 4).await;
        for pair in stamps.windows(2) {
            let gap = pair[1].duration_since(pair[0]);
            // The one-tenth of slack is clock granularity and nothing else: the
            // bound is `gap >= interval + read`, and a read is not negative.
            assert!(
                gap * 10 >= interval * 9,
                "two snapshots are {:?} apart on a {:?} interval — the poller is reading \
                 faster than it was told to",
                gap,
                interval
            );
        }
    }

    /// Dropping the poller mid-read leaves **no `bd` child**, and does not hang.
    ///
    /// `kill_on_drop(true)` is set in `services::bd::run`; this is the test that
    /// checks it covers the poll path. The drop lands while the fake is still
    /// asleep, which is the only moment the question is interesting — after the
    /// read has returned there is nothing left to kill.
    #[tokio::test]
    async fn dropping_the_poller_mid_read_leaves_no_bd_running() {
        let fakes = Fakes::new(
            "poll-dropped-mid-read",
            PiFake::Started,
            BdFake::Slow,
            BOARD_VARIETY,
        );
        let (poller, handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval: Duration::from_millis(60),
            enabled: true,
            journal: sweep_every(Duration::from_millis(60)),
        });
        // Wait for a read that is *in flight* — started, and not yet finished.
        until("the fake bd to have a read in flight", WAIT, || {
            fakes.bd_starts() > fakes.bd_stops()
        })
        .await;
        let pid = fakes
            .last_bd_pid()
            .expect("a read is in flight, so it has a pid");
        assert!(
            process_alive(pid),
            "the read about to be interrupted is not running: {pid}"
        );

        // The drop *is* the shutdown, and it must not wait for the read.
        let dropped = Instant::now();
        drop(handle);
        drop(poller);
        assert!(
            dropped.elapsed() < Duration::from_secs(1),
            "dropping the poller waited on the read instead of ending it: {:?}",
            dropped.elapsed()
        );

        // And the child went with it: `kill_on_drop` fires on the drop and the
        // runtime reaps the body.
        until(
            &format!("bd child {pid} to die with its poller"),
            WAIT,
            || !process_alive(pid),
        )
        .await;
        assert!(!process_alive(pid), "bd child {pid} outlived its poller");
    }

    /// A board that is off spawns no task and runs no process. Off has to mean
    /// *off*, not "polls quietly and hides the answer".
    #[tokio::test]
    async fn a_board_that_is_off_runs_nothing_at_all() {
        let fakes = Fakes::new("poll-off", PiFake::Started, BdFake::Ok, BOARD_VARIETY);
        let (poller, handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval: Duration::from_millis(60),
            enabled: false,
            journal: sweep_every(Duration::from_millis(60)),
        });

        assert!(!handle.is_enabled(), "the handle knows too");
        assert!(
            poller.task.is_none(),
            "a disabled board must not be left with a task to leak"
        );
        // Give every chance of having ticked.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            fakes.bd_calls(),
            0,
            "a disabled board ran `bd` anyway: {:?}",
            fakes.bd_log()
        );
        // …and the value it hands out is the never-loaded one — which nothing
        // draws, because `is_enabled()` says not to.
        assert_eq!(handle.borrow().read, BoardRead::Never);
    }

    /// A poller nobody reads stops polling: there is no point publishing news to
    /// an empty room.
    #[tokio::test]
    async fn a_board_nobody_reads_stops_polling() {
        let fakes = Fakes::new(
            "poll-no-readers",
            PiFake::Started,
            BdFake::Ok,
            BOARD_VARIETY,
        );
        let (poller, mut handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval: Duration::from_millis(40),
            enabled: true,
            journal: sweep_every(Duration::from_millis(40)),
        });
        wait_reads(&fakes, 1).await;
        // The poller itself stays alive the whole time, so the only thing that
        // can stop the task is this: nobody left to publish to.
        assert!(handle.changed().await, "the second read never arrived");
        drop(handle);
        // Let whatever tick was in flight when the reader left finish, then take
        // the baseline. The poller checks `receiver_count` at the top of every
        // tick, so this window — ten ticks' worth — is far more than the one it
        // needs to notice and return.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let before_reads = board_reads(&fakes).len();
        let before = fakes.bd_starts();
        // And now nothing: no probe, no read, no process. Six ticks' worth of
        // silence is the assertion — a poller that had not stopped would have
        // run several of each by now.
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(
            fakes.bd_starts(),
            before,
            "`bd` ran after the last reader left: {} -> {}",
            before,
            fakes.bd_starts()
        );
        assert_eq!(
            board_reads(&fakes).len(),
            before_reads,
            "a board read happened with nobody left to see it"
        );
        assert!(poller.task.is_some(), "a running board owns a task");
    }

    // ═══════════════════ the change detector (ADR-0007 §7) ═══════════════════
    //
    // Everything below is the same claim from different sides: the journal is
    // what the poller asks, the board is what it reads, and **no variation of
    // "the probe said nothing" may ever leave a changed board unpainted**. The
    // first two tests are the saving; the rest are the safety.

    /// The saving this whole change is for: a quiet board costs a probe, not a
    /// board read.
    #[tokio::test]
    async fn a_quiet_journal_costs_no_board_read_after_the_first_one() {
        let r = running_journal(
            "detector-quiet",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(40),
            never_sweep(),
        );
        wait_settled(&r, 3).await;
        // Several more ticks, and the board is still read exactly once — the
        // first one, which is not optional.
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(
            board_reads(&r.fakes).len(),
            1,
            "a quiet journal re-read the board: probes {:?}",
            probe_since(&r.fakes)
        );
        // …and that is not the skip-forever failure masquerading as saving: the
        // board *was* read, and is on the handle.
        assert_eq!(r.handle.borrow().total(), 6, "the first read landed");
        assert!(r.handle.borrow().read.is_ok());
        assert!(
            probes(&r.fakes).len() >= 5,
            "it stopped probing rather than stopping reading"
        );
    }

    /// One record in the journal buys **one** board read — not one per record,
    /// and not one per tick afterwards.
    #[tokio::test]
    async fn one_journal_record_buys_exactly_one_board_read() {
        let r = running_journal(
            "detector-moved",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(40),
            never_sweep(),
        );
        wait_settled(&r, 2).await;
        let before = board_reads(&r.fakes).len();

        r.fakes.bump_journal(1);
        until("the read the journal asked for", WAIT, || {
            board_reads(&r.fakes).len() > before
        })
        .await;

        // And it settles: the read it just did covered the change, so the next
        // several ticks are quiet ticks again. A detector that re-read on every
        // tick after a single record is no detector at all.
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(
            board_reads(&r.fakes).len(),
            before + 1,
            "one record caused more than one read: probes {:?}",
            probe_since(&r.fakes)
        );
    }

    /// **The watermark rule, tested as a lost change would appear.**
    ///
    /// The dangerous implementation is the poller that adopts the highest seq it
    /// has ever *seen* — including one seen after the board read that is meant
    /// to have covered it. That loses the change silently: the rows never
    /// contain it, the watermark is past it, and the band never asks again. With
    /// the safe rule (adopt only what the probe saw *before* the read) the same
    /// race costs one extra read, which is what this asserts.
    ///
    /// `BdFake::Slow` is what makes the window wide enough to matter: 350 ms of
    /// read against a 20 ms tick is ~17 ticks of room to drop a change into.
    #[tokio::test]
    async fn a_change_that_lands_during_a_read_is_still_picked_up() {
        let r = running_journal(
            "detector-midread",
            BdFake::Slow,
            BOARD_VARIETY,
            Duration::from_millis(20),
            never_sweep(),
        );
        wait_settled(&r, 2).await;
        let before = board_reads(&r.fakes).len();

        // First change: this is the read that will be running when the second
        // one arrives.
        r.fakes.bump_journal(1);
        until("a `bd` in flight for the first change", WAIT, || {
            r.fakes.bd_starts() > r.fakes.bd_stops()
        })
        .await;
        // Second change, dropped into the middle of that read.
        r.fakes.bump_journal(1);

        until(
            "a second read, for the change that arrived mid-read",
            WAIT,
            || board_reads(&r.fakes).len() >= before + 2,
        )
        .await;
        // And the watermark moved past both records: the detector is not stuck
        // replaying them, and it is not ahead of them either.
        until("the watermark to pass seq 2", WAIT, || {
            probe_since(&r.fakes).last().copied().unwrap_or(0) >= 2
        })
        .await;
    }

    /// The sweep, and nothing else, is what can see a change the journal did not
    /// record — `bd dolt pull`, `bd sql`, a workspace with the journal off.
    #[tokio::test]
    async fn an_unjournaled_change_lands_on_the_sweep_and_the_journal_never_claimed_it() {
        let sweep = Duration::from_millis(150);
        let r = running_journal(
            "detector-sweep",
            BdFake::Ok,
            ONE_BEADED_BOARD,
            Duration::from_millis(25),
            JournalConfig {
                enabled: true,
                reconcile: sweep,
            },
        );
        wait_settled(&r, 2).await;
        assert_eq!(r.handle.borrow().total(), 1);

        // Change the board **without** a journal record. Nothing the probe can
        // see has happened.
        r.fakes.set_board_unjournaled(BOARD_VARIETY);
        let watermark_at_change = *probe_since(&r.fakes).last().expect("a probe ran");

        until(
            "the sweep to notice what the journal could not",
            WAIT,
            || r.handle.borrow().total() == 6,
        )
        .await;
        assert_eq!(
            *probe_since(&r.fakes).last().expect("a probe ran"),
            watermark_at_change,
            "the watermark moved, so this was the journal taking credit for a change \
             it never saw"
        );
    }

    /// A probe that cannot be answered is never read as "nothing changed". The
    /// board keeps getting read while the detector is down, and goes quiet again
    /// when it comes back.
    #[tokio::test]
    async fn a_probe_that_cannot_answer_never_reads_as_a_quiet_board() {
        let r = running_journal(
            "detector-broken",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(40),
            never_sweep(),
        );
        wait_settled(&r, 2).await;
        let before = board_reads(&r.fakes).len();

        r.fakes.fail_journal(true);
        until("the board read that a broken probe owes", WAIT, || {
            board_reads(&r.fakes).len() > before
        })
        .await;
        // …every tick, not once: with the detector down the poller is back to
        // asking the board directly.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            board_reads(&r.fakes).len() >= before + 4,
            "a broken probe stopped the board being read: probes {:?}",
            probe_since(&r.fakes)
        );

        // Recovery is not sticky either.
        r.fakes.fail_journal(false);
        // Give the detector a few ticks to notice it can answer again; reads
        // stop the tick a quiet probe lands.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let settled = board_reads(&r.fakes).len();
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(
            board_reads(&r.fakes).len(),
            settled,
            "still reading after the detector recovered: {:?}",
            probe_since(&r.fakes)
        );
    }

    /// A watermark the retention floors pruned away is a **typed** answer with
    /// an address in it, and the poller goes there: read the board, resume from
    /// the head `bd` named, do not re-probe the pruned prefix forever.
    #[tokio::test]
    async fn a_pruned_watermark_rebaselines_at_the_head_bd_named() {
        let r = running_journal(
            "detector-truncated",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(40),
            never_sweep(),
        );
        wait_settled(&r, 2).await;
        assert_eq!(*probe_since(&r.fakes).last().unwrap(), 0, "starts at zero");

        r.fakes.truncate_journal(10, 42);
        until("a probe above the retention floor", WAIT, || {
            *probe_since(&r.fakes).last().unwrap() >= 42
        })
        .await;
        assert!(
            board_reads(&r.fakes).len() >= 2,
            "the re-baseline did not come with a fresh board read"
        );
        // And the truncation is not a permanent condition: it settled at the
        // head instead of looping on the refusal.
        let settled = board_reads(&r.fakes).len();
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(
            board_reads(&r.fakes).len(),
            settled,
            "looping on the truncation: {:?}",
            probe_since(&r.fakes)
        );
    }

    /// A workspace with the journal switched off answers the probe with *nothing*,
    /// successfully, forever. That is the case the sweep is there for, and the
    /// band must keep following the board.
    #[tokio::test]
    async fn a_disabled_journal_is_a_quiet_lie_the_sweep_covers() {
        let r = running_journal(
            "detector-disabled",
            BdFake::Ok,
            ONE_BEADED_BOARD,
            Duration::from_millis(25),
            JournalConfig {
                enabled: true,
                reconcile: Duration::from_millis(120),
            },
        );
        wait_settled(&r, 2).await;

        r.fakes.disable_journal(true);
        r.fakes.set_board_unjournaled(BOARD_VARIETY);
        until("a sweep past the disabled journal", WAIT, || {
            r.handle.borrow().total() == 6
        })
        .await;
        // The probe is still being asked (cheap), it just never says anything.
        assert!(!probes(&r.fakes).is_empty());
    }

    /// A journal with more in it than one batch can carry is **drained**, not
    /// guessed at — and while it is being drained the poller reads the board
    /// every tick, because it cannot know whether the records it has not read
    /// touched a bead. That is the property that makes a big journal delay the
    /// savings rather than cost more than the pre-detector poller ever did.
    #[tokio::test]
    async fn a_big_journal_drains_in_bounded_batches_and_then_starts_saving() {
        let fakes = Fakes::new(
            "detector-catchup",
            PiFake::Started,
            BdFake::Ok,
            BOARD_VARIETY,
        );
        let mut prefill = String::new();
        for seq in 1..=(CATCHUP_LIMIT * 2) {
            prefill.push_str(&format!(
                r#"{{"seq":{seq},"op":"update","issue_id":"looprs-old-{seq}"}}"#
            ));
            prefill.push('\n');
        }
        fakes.set_journal(&prefill);
        let (_poller, handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval: Duration::from_millis(30),
            enabled: true,
            journal: never_sweep(),
        });

        until("the drain to reach the head", WAIT, || {
            probes(&fakes).len() >= 4 && probe_limit(&fakes).last() == Some(&PROBE_LIMIT)
        })
        .await;

        // Every batch was bounded: no single probe was asked to swallow the lot.
        let limits = probe_limit(&fakes);
        assert_eq!(limits[0], CATCHUP_LIMIT, "starts in catch-up");
        assert!(
            limits.iter().all(|l| *l <= CATCHUP_LIMIT),
            "an unbounded probe: {limits:?}"
        );
        // The watermark walked the journal in CATCHUP_LIMIT steps.
        let since = probe_since(&fakes);
        assert!(
            since.windows(2).all(|w| w[1] >= w[0]),
            "the watermark went backwards: {since:?}"
        );
        assert!(
            since.contains(&(CATCHUP_LIMIT * 2)),
            "never drained to the head: {since:?}"
        );
        // While draining, every tick that found records read the board: two
        // full batches in, two reads out. Skipping them would be the lost-change
        // bug wearing a different name.
        assert!(
            board_reads(&fakes).len() >= 2,
            "drained the journal without reading the board it was behind: reads {}, probes {:?}",
            board_reads(&fakes).len(),
            since
        );
        // And once drained, it stops.
        let settled = board_reads(&fakes).len();
        tokio::time::sleep(Duration::from_millis(240)).await;
        assert_eq!(
            board_reads(&fakes).len(),
            settled,
            "still reading after catching up: {:?}",
            probe_since(&fakes)
        );
        assert_eq!(handle.borrow().total(), 6);
    }

    /// `LOOPRS_KANBAN_EVENTS=0`: the pre-detector poller, exactly. Every tick a
    /// board read, and the journal is never touched at all.
    #[tokio::test]
    async fn detector_off_reads_the_board_every_tick_and_never_the_journal() {
        let r = running_journal(
            "detector-off",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(40),
            JournalConfig {
                enabled: false,
                reconcile: Duration::from_secs(3600),
            },
        );
        wait_reads(&r.fakes, 4).await;
        assert!(
            board_reads(&r.fakes).len() >= 4,
            "the board stopped being read every tick: {:?}",
            r.fakes.bd_log()
        );
        assert!(
            probes(&r.fakes).is_empty(),
            "LOOPRS_KANBAN_EVENTS=0 still probed the journal: {:?}",
            probes(&r.fakes)
        );
    }

    // ─────────────────── the decision table, with no subprocess ───────────────────

    /// [`decide`] as a table. The task-level tests above prove it works; these
    /// prove the three rules that make it safe, in the cases a running poller is
    /// awkward to put in exactly the right state for.
    #[test]
    fn the_decision_table() {
        type Probe = Result<bd::JournalProbe, bd::JournalError>;
        let quiet: Probe = Ok(bd::JournalProbe {
            seqs: vec![],
            disabled: false,
        });
        let one_record: Probe = Ok(bd::JournalProbe {
            seqs: vec![7],
            disabled: false,
        });
        let filled_batch: Probe = Ok(bd::JournalProbe {
            seqs: vec![8, 9],
            disabled: false,
        });
        let disabled: Probe = Ok(bd::JournalProbe {
            seqs: vec![],
            disabled: true,
        });
        let pruned: Probe = Err(bd::JournalError::Truncated {
            since: 0,
            floor: 5,
            head: 99,
        });
        let broken: Probe = Err(bd::JournalError::Unusable(BdError::Timeout {
            bin: "bd".into(),
            args: "events tail".into(),
        }));

        // Rule 1: never read ⇒ always read, however quiet the journal is.
        let d = decide(quiet.clone(), CATCHUP_LIMIT, true, false);
        assert_eq!(d.read, Some(ReadReason::First), "{d:?}");
        assert!(d.adopt.is_none(), "nothing new to adopt: {d:?}");

        // Quiet, board already read, no sweep ⇒ skip. This is the saving.
        let d = decide(quiet.clone(), PROBE_LIMIT, false, false);
        assert_eq!(d.read, None, "{d:?}");
        assert!(d.at_head);

        // Quiet but the sweep is due ⇒ read, and the journal still owns nothing.
        let d = decide(quiet, PROBE_LIMIT, false, true);
        assert_eq!(d.read, Some(ReadReason::Sweep), "{d:?}");
        assert!(d.adopt.is_none());

        // A record ⇒ read, adopt exactly what the probe saw.
        let d = decide(one_record, CATCHUP_LIMIT, false, false);
        assert_eq!(d.read, Some(ReadReason::Journal), "{d:?}");
        assert_eq!(d.adopt, Some(7));
        assert!(d.at_head, "a short batch means the drain reached the head");

        // A batch that filled its limit ⇒ behind, and not at the head yet.
        let d = decide(filled_batch, 2, false, false);
        assert_eq!(d.read, Some(ReadReason::Journal));
        assert_eq!(d.adopt, Some(9));
        assert!(!d.at_head, "{d:?}");

        // Pruned ⇒ re-baseline at the head `bd` named, and report it.
        let d = decide(pruned, PROBE_LIMIT, false, false);
        assert_eq!(d.read, Some(ReadReason::Rebaselined), "{d:?}");
        assert_eq!(d.adopt, Some(99), "resume where the refusal pointed");
        assert!(d.at_head, "there is nothing behind the floor to drain");
        assert!(d.broken.is_some(), "and it is worth saying out loud");

        // Rule 2, the one that matters most: a probe that could not answer
        // reads the board, and adopts NOTHING. Moving the watermark here is the
        // lost change.
        let d = decide(broken, PROBE_LIMIT, false, false);
        assert_eq!(d.read, Some(ReadReason::ProbeFailed), "{d:?}");
        assert_eq!(
            d.adopt, None,
            "a failed probe must never move the watermark"
        );
        assert!(
            !d.at_head,
            "a probe that failed says nothing about the head"
        );
        assert!(d.broken.is_some());

        // A disabled journal answers quiet and says so; that is not a failure,
        // but it is not a reason to stop sweeping either.
        let d = decide(disabled, PROBE_LIMIT, false, true);
        assert_eq!(d.read, Some(ReadReason::Sweep), "{d:?}");
        assert!(d.disabled);
        assert!(d.broken.is_none());
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
}
