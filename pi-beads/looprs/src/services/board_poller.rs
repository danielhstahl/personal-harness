//! The board poller: one task, one read per tick, one **newest** snapshot
//! (looprs-5o4.2, under ADR-0007 — `docs/adr/0007-kanban-board.md`; the
//! operator-facing page for what this feeds is `docs/kanban.md`).
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
//!   — `bd --readonly list --all --limit 0 --json`, one per tick, already
//!   `tokio::process` under
//!   [`BD_TIMEOUT`](crate::services::bd::BD_TIMEOUT). Nothing in this path is a
//!   blocking `std::process::Command`, which is the whole reason the UI does not
//!   stall behind a Dolt-backed board (looprs-037).
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
//! 3. **A poll that changed nothing paints nothing.** The poller still
//!    *publishes* every tick — it has to, because the value carries
//!    [`fetched_at`](crate::state::board::BoardSnapshot::fetched_at) and the
//!    footer's `bd ok · Ns ago` is a fact the frame re-reads rather than news —
//!    but it must not *cost a frame*. That half is enforced on the consumer's
//!    side, in [`crate::App::adopt_board`], which compares the incoming
//!    snapshot with [`BoardSnapshot::same_paint_as`]: the drawn fields — the
//!    read's state, the columns, the deferred count — and deliberately **not**
//!    the two clocks. The failure prevented: a board that has not moved making
//!    an untouched terminal repaint every tick forever, which is the exact
//!    opposite of the "the band costs nothing while nothing happens" claim it is
//!    sold on, and a busy loop next to a session that is trying to use the CPU.
//!
//!    Deduplicating in the **poller** is the tempting wrong shape: suppress the
//!    publish and the footer freezes at the age of the last *change*, so the
//!    freshness marker starts telling the opposite of the truth about how long
//!    it has been since anybody looked. Publish the age; let the paint decide.
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
/// read to something that can be table-tested without mutating a process.
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
}

impl BoardConfig {
    /// The board the environment asks for.
    ///
    /// **Call this in `main`, and once.** A knob re-read per frame is a knob
    /// whose value can change halfway through an answer, and a band that changes
    /// height because something else exported `LOOPRS_KANBAN` mid-run is a bug
    /// nobody can reproduce.
    pub fn from_env() -> Self {
        Self::resolve(
            env_value("LOOPRS_KANBAN").as_deref(),
            env_value("LOOPRS_KANBAN_POLL_MS").as_deref(),
            Some(&bd::bd_bin_from_env()),
        )
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
        tracing::info!(
            "kanban board: reading `{} --readonly list --all --limit 0 --json` every {:?} \
             (LOOPRS_KANBAN_POLL_MS to retune, LOOPRS_KANBAN=0 for none)",
            cfg.bin,
            cfg.interval,
        );
        let task = tokio::spawn(poll_task(tx, cfg.bin, cfg.interval));
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

/// The task: tick, read, publish. Until nobody is listening.
async fn poll_task(tx: Sender<BoardSnapshot>, bin: String, interval: Duration) {
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

    loop {
        tick.tick().await;

        // Nobody left to tell. A published value nobody reads is not a cache, it
        // is garbage with a channel attached to it.
        if tx.receiver_count() == 0 {
            tracing::debug!("board poller: no readers left, stopping");
            return;
        }

        // The stamp is taken **before** the read goes out, not when it comes
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
        match bd::board_read_with(&bin).await {
            Ok(beads) => {
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
    use crate::testing::{BOARD_VARIETY, BdFake, EMPTY_BOARD, Fakes, PiFake, process_alive};
    use std::time::Instant;

    /// How long a test waits before concluding the poller is not going to answer.
    /// Everything awaited here is answered by a fake in milliseconds, so timing
    /// out means it is broken, not that the machine was slow.
    const WAIT: Duration = Duration::from_secs(10);

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
        let fakes = Fakes::new(tag, PiFake::Started, fake, board);
        let (poller, handle) = BoardPoller::spawn(BoardConfig {
            bin: fakes.bd_bin().to_string(),
            interval,
            enabled: true,
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
        }
        let cfg = BoardConfig::from_env();
        assert!(!cfg.enabled, "LOOPRS_KANBAN=0 must turn it off: {cfg:?}");
        assert_eq!(cfg.interval, Duration::from_millis(1234), "{cfg:?}");
        assert_eq!(cfg.bin, "/tmp/looprs-test/bd", "{cfg:?}");

        // SAFETY: as above; removed rather than blanked, so the second reading
        // is the *unset* case and not the blank-value one.
        unsafe {
            std::env::remove_var("LOOPRS_KANBAN");
            std::env::remove_var("LOOPRS_KANBAN_POLL_MS");
            std::env::remove_var("LOOPRS_BD_BIN");
        }
        let bare = BoardConfig::from_env();
        assert!(bare.enabled);
        assert_eq!(bare.interval, Duration::from_secs(5));
        assert_eq!(bare.bin, "bd");
    }

    // ───────────────────────── the read it runs ─────────────────────────

    /// One read per tick, and the read is ADR-0007 §3's command line.
    ///
    /// The ADR forbids three per-status queries per frame (2.9× the cost, and
    /// three reads of one board can disagree with each other inside one frame),
    /// so the assertion is not "the command is right" but **"every command,
    /// always the same one, and never one that writes"**.
    #[tokio::test]
    async fn every_tick_is_the_one_board_read_and_never_a_write() {
        let r = running(
            "poll-once",
            BdFake::Ok,
            BOARD_VARIETY,
            Duration::from_millis(60),
        );
        wait_reads(&r.fakes, 3).await;

        let lines = r.fakes.bd_log();
        assert!(lines.len() >= 3, "expected several reads: {lines:?}");
        for line in &lines {
            assert_eq!(
                line, "--readonly list --all --limit 0 --json",
                "the board is one fixed read, not a per-column query"
            );
        }
        // The band cannot change the board — as a property of the command line.
        let joined = lines.join(" ");
        for forbidden in ["update", "claim", "create", "close", "ready"] {
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
        });
        wait_reads(&fakes, 1).await;
        // The poller itself stays alive the whole time, so the only thing that
        // can stop the task is this: nobody left to publish to.
        assert!(handle.changed().await, "the second read never arrived");
        drop(handle);
        let before = fakes.bd_starts();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            fakes.bd_starts() <= before + 1,
            "reads kept going after the last reader left: {before} -> {}",
            fakes.bd_starts()
        );
        assert!(poller.task.is_some(), "a running board owns a task");
    }
}
