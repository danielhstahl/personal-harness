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
//! ## What is where (looprs-00u.18)
//!
//! This was one 2,327-line file. What is left here is the state the poller
//! publishes — the owner `main` holds and the handle every reader takes —
//! with the three responsibilities it carried next door:
//!
//! * [`config`](config) — the knobs, resolved once from strings by pure
//!   functions (`LOOPRS_KANBAN`, the poll interval, the reconcile sweep);
//! * [`schedule`](schedule) — the watermark logic: does this tick owe a read,
//!   and of which kind ([`ReadReason`](schedule::ReadReason),
//!   [`decide`](schedule::decide));
//! * [`read`](read) — the tick loop and the `bd` calls it makes;
//! * [`tests`](tests) — this suite, split along the subjects already in it.
//!
//! Behaviour-preserving: whole items moved, and `board_poller::BoardPoller`,
//! `board_poller::BoardHandle` and `board_poller::BoardConfig` still resolve
//! from the old paths.

use crate::services::board_poller::read::poll_task;

use std::time::Instant;

use tokio::sync::watch;

use crate::state::board::BoardSnapshot;

pub(crate) mod config;
mod read;
mod schedule;

#[cfg(test)]
mod tests;

pub use config::BoardConfig;
// `JournalConfig` is reached through the crate-visible module
// (`board_poller::config::JournalConfig`) rather than re-exported here: the
// only thing that names it outside `config` is a test's own fixture, and a
// re-export whose only reader is `cfg(test)` is a warning in the binary build
// and a promise the module does not actually make.

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
