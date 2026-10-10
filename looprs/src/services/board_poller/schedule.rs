//! The schedule and the watermark: does this tick owe a read, and of which kind?
//!
//! All of the deciding is here and none of the doing: [`decide`] takes what the
//! poller last saw — the watermark it holds, when it last read the whole
//! board, whether it is behind the head of the journal, whether the last read
//! failed — and returns a [`Decision`] naming a [`ReadReason`] or no read at
//! all. The tick loop in [`read`](super::read) calls it and then obeys.
//!
//! Keeping it pure is what makes the change detector's shape testable: "a
//! quiet journal means no board read", "a stale watermark means a full read",
//! "behind the head means the catch-up limit, not the probe limit", and "a
//! failed detector is not the same answer as a quiet one" are each a row in a
//! table rather than a path a test happens to walk.

use std::time::Duration;

use crate::services::bd;

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
pub(super) struct Decision {
    /// `Some` = read the whole board this tick, and why.
    pub(super) read: Option<ReadReason>,
    /// The watermark a **successful** board read this tick adopts. `None` when
    /// there is nothing new to adopt, or when the read failed (see the task).
    pub(super) adopt: Option<i64>,
    /// Whether the watermark is now proved to sit at the head of the journal,
    /// which is what lets the next probe ask for one record instead of a batch.
    pub(super) at_head: bool,
    /// The detector's complaint, if it has one. The words are carried so the
    /// loud-once log line can say something specific.
    pub(super) broken: Option<String>,
    /// `bd` said the events journal is disabled for this workspace.
    pub(super) disabled: bool,
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
pub(super) fn decide(
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
pub(super) fn report_detector(
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
