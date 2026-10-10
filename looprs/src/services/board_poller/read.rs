//! The read: the tick loop, the `bd` calls it makes, and what it does with each answer.
//!
//! [`poll_task`] is the whole task body: the interval timer
//! ([`MissedTickBehavior::Delay`], so an overrun pushes the next tick out
//! instead of firing a burst), the probe and the board read
//! ([`bd::journal_probe_with`](crate::services::bd::journal_probe_with) and
//! [`bd::board_read_with`](crate::services::bd::board_read_with)), the
//! watermark it carries forward, and the publish that puts a
//! [`BoardSnapshot`] on the `watch` channel that
//! [`BoardHandle`](super::BoardHandle) reads.
//!
//! What it never does is as load-bearing as what it does: a failed read never
//! replaces a populated board with an empty one, the task exits when its
//! receiver drops rather than publishing to an empty room, and the shutdown
//! path answers on the exit rather than eventually. Which tick is *owed* is
//! decided by [`decide`](super::schedule::decide); this file is where each
//! answer gets paid for, one tick at a time.

use crate::services::board_poller::config::{CATCHUP_LIMIT, JournalConfig, PROBE_LIMIT};
use crate::services::board_poller::schedule::{Decision, ReadReason, decide, report_detector};

use std::time::{Duration, Instant};

use tokio::sync::watch::Sender;
use tokio::time::MissedTickBehavior;

use crate::services::bd::{self, BdError};
use crate::state::board::BoardSnapshot;

/// The task: probe, read if it is owed, publish. Until nobody is listening.
pub(super) async fn poll_task(
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
