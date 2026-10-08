//! The board as *data*: one read of `bd`, mapped into three columns, carrying
//! its own freshness (looprs-5o4.4, under ADR-0007).
//!
//! # Why this module exists next to the widget rather than inside it
//!
//! [`crate::components::kanban`] is a renderer. ADR-0007 makes two demands that
//! are not rendering: the status → column mapping must be a **total function**
//! (rule 1: "do not drop a status"), and the accounting invariant **I1** must
//! hold at the level of the read, not the frame:
//!
//! > Σ header(c) + deferred_count = beads in the read.
//!
//! A mapping asserted inside a widget gets tested through painted pixels, which
//! is the wrong place to prove a property of a function. So the mapping, the
//! bucketing and the invariant live here as plain data and plain functions, and
//! the widget is handed the answer. That is also what keeps ADR-0007 rule 9
//! ("do not re-derive `bd`'s ready/blocker semantics in the widget") true by
//! construction: the widget never sees a status it has to interpret.
//!
//! # Who fills one of these in
//!
//! The poller (looprs-5o4.2) calls [`BoardSnapshot::from_beads`] with the
//! answer to `bd --readonly list --all --limit 0 --json`, stamps the age from
//! its own clock, and publishes it latest-wins. On a failed read it calls
//! [`BoardSnapshot::with_error`], which keeps the last good beads exactly as they
//! were — the rule that an error never destroys the snapshot (§4) is a property
//! of this type rather than a hope about the caller.
//!
//! Neither this module nor the widget talks to `bd`. Nothing here reads a clock:
//! [`BoardSnapshot::age`] arrives as an argument, computed upstream, so every
//! branch of the board is paintable in a test with no subprocess, no terminal and
//! no `sleep` — the same rule [`crate::components::status`] states for its row.

use std::time::{Duration, Instant};

use crate::services::bd::{BdError, Bead, BeadStatus};

/// The three columns of the board, in the order they are painted (ADR-0007 §1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    ToDo,
    InProgress,
    Complete,
}

impl Column {
    /// Every column, in draw order. The widget walks this; nothing indexes by a
    /// literal `0`/`1`/`2`, which is the mistake the original four-line sketch
    /// this ticket replaced made twice.
    pub const ALL: [Column; 3] = [Self::ToDo, Self::InProgress, Self::Complete];

    /// The column's name as it appears in the header row.
    pub fn name(self) -> &'static str {
        match self {
            Self::ToDo => "To-do",
            Self::InProgress => "In progress",
            Self::Complete => "Complete",
        }
    }
}

/// The marker a row carries when the stored status says something the column
/// alone cannot say (ADR-0007 §1).
///
/// Two markers, both laid out **before** the title is truncated rather than
/// appended after it: a marker that the width cut could eat would leave a
/// blocked bead looking exactly like pickable work, which is dropping the bead
/// visually while still counting it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Marker {
    /// `⊘` — the bead is `blocked`: not work the loop may take, but the most
    /// actionable thing on the board for the person reading it.
    Blocked,
    /// `?` — this build cannot classify the status (`pinned`, `hooked`, a custom
    /// status, or a status field that arrived missing).
    ///
    /// `?` is also the site of a known limitation, left visible on purpose:
    /// `#[serde(other)]` discards the raw status word, so `?` cannot say
    /// *which* status it could not name — `review`, `pinned`, whatever it was.
    /// Naming it needs `Unknown(String)`, which is ADR-0007's follow-up F1.
    Unknown,
}

impl Marker {
    pub fn glyph(self) -> &'static str {
        match self {
            Self::Blocked => "⊘",
            Self::Unknown => "?",
        }
    }

    /// The marker a **stored** status earns.
    ///
    /// Derived from `BeadStatus` alone and from nothing else: no blocker graph,
    /// no claimability, no `bd ready`. ADR-0007 §2 refuses to keep `bd`'s
    /// claimability rules in this crate, so `blocked` here is the word `bd`
    /// stored, not a conclusion this code reached.
    pub fn of(status: BeadStatus) -> Option<Self> {
        match status {
            BeadStatus::Blocked => Some(Self::Blocked),
            BeadStatus::Unknown => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// The column a status is drawn in, or `None` for the one status that is not a
/// row at all.
///
/// Total over `BeadStatus`, and `None` is **not** "dropped": the bead that
/// returns `None` is counted in [`BoardSnapshot::deferred`] and reaches the eye
/// in the footer, which is its one visible place by design. ADR-0007's rule 2 —
/// *do not merge `deferred` into To-do* — is the reason `Deferred` is the arm
/// that answers `None`: a human took it out of the running, and a row puts it
/// back in visually.
pub fn column_of(status: BeadStatus) -> Option<Column> {
    match status {
        // The plain case, and the two that mean "available work" in `bd`'s own
        // vocabulary without being stored states of their own.
        BeadStatus::Open | BeadStatus::Ready => Some(Column::ToDo),
        // Both land in To-do *marked*, so they sit where the eye is without
        // reading as work the loop may take.
        BeadStatus::Blocked | BeadStatus::Unknown => Some(Column::ToDo),
        BeadStatus::InProgress => Some(Column::InProgress),
        // `done` is not a status `bd` writes today; mapped anyway so a future or
        // custom `done` reads as Complete rather than as `?`.
        BeadStatus::Done | BeadStatus::Closed => Some(Column::Complete),
        BeadStatus::Deferred => None,
    }
}

/// One row of the board.
///
/// Three fields, which is all ADR-0007 grants a row: `id`, `title`, marker. No
/// priority, no labels, no pin, no blockers, no due date — each of those is a
/// column-of-the-mind that costs a read field and a layout slot nobody asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardBead {
    pub id: String,
    pub title: String,
    pub marker: Option<Marker>,
}

impl BoardBead {
    /// Take the row shape from a bead the read returned.
    pub fn from_bead(bead: &Bead) -> Self {
        Self {
            id: bead.id().to_string(),
            title: bead.title().to_string(),
            marker: Marker::of(bead.status()),
        }
    }
}

/// One column's worth of rows, name included.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ColumnBeads {
    pub column: Column,
    /// The column's beads in the order the read returned them (`bd list`'s own
    /// order, which measured out priority-sorted — ADR-0007 §2). Never sorted or
    /// re-ordered here, and **never truncated**: the header count has to be the
    /// true total, so truncation belongs to the frame.
    pub beads: Vec<BoardBead>,
}

/// How the last read of the board ended.
///
/// One variant per state ADR-0007 §4's failure table names. The point of the
/// enum rather than a `Result<Vec<Bead>, String>` is that **the footer picks its
/// sentence from the variant**, not by re-reading an error message: "empty
/// board", "not loaded", `command not found` and "unreadable JSON" are four
/// different states, and the looprs-037 rule says they must not be able to
/// collapse into one another — a thing you decide by `string matching` is a thing
/// that starts matching the wrong string the first time a message changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoardRead {
    /// Nothing has been read yet. Renders the header with `—` counts and the
    /// footer `reading the board…`, never a blank band.
    Never,
    /// The last read answered, and answered a board.
    Ok,
    /// The binary could not be run at all.
    Unavailable { reason: String },
    /// `bd` ran and exited non-zero. `message` is the first line of its stderr —
    /// the error's own words, which is what makes this tellable apart from an
    /// empty board at a glance.
    Failed {
        code: Option<i32>,
        message: Option<String>,
    },
    /// `bd` exited 0 with a payload this build cannot parse.
    Malformed,
    /// `bd` did not answer inside [`crate::services::bd::BD_TIMEOUT`].
    Timeout,
}

impl BoardRead {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok)
    }

    /// Anything that is not a good read and not the absence of one.
    pub fn is_error(&self) -> bool {
        matches!(
            self,
            Self::Unavailable { .. } | Self::Failed { .. } | Self::Malformed | Self::Timeout
        )
    }
}

impl From<&BdError> for BoardRead {
    /// Fold a service error into the board's own vocabulary.
    ///
    /// The bin and args ride in [`BdError`] and are deliberately **not** copied
    /// across: the band says *what happened*, and the full command line with the
    /// whole stderr is the log's job. Keeping the row short is what lets it be
    /// read at a glance, which is the whole ADR-0007 §4 requirement.
    fn from(err: &BdError) -> Self {
        match err {
            BdError::Unavailable { reason, .. } => Self::Unavailable {
                reason: reason.clone(),
            },
            BdError::Failed { code, stderr, .. } => Self::Failed {
                code: *code,
                message: first_line(stderr),
            },
            BdError::Malformed { .. } => Self::Malformed,
            BdError::Timeout { .. } => Self::Timeout,
        }
    }
}

/// The first non-empty line of `s`, trimmed — the "first line of stderr" the
/// footer's `bd failed (exit N): …` is specified as. `None` when there is
/// nothing to say, so the footer says `bd failed (exit 3)` rather than
/// `bd failed (exit 3):` with nothing after the colon.
fn first_line(s: &str) -> Option<String> {
    s.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(str::to_string)
}

/// One frame's worth of board.
///
/// The whole input to [`crate::components::kanban`]: a value, not a service.
/// Clone it, stash it in a `watch`, hand a reference to the widget — nothing in
/// here can block, read a clock, or ask `bd` anything while painting.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardSnapshot {
    /// How the last read ended.
    pub read: BoardRead,
    /// The last **good** read's rows, bucketed by column, always all three
    /// columns present and in [`Column::ALL`] order.
    ///
    /// Never truncated (ADR-0007 §4: a header count must be the true total, and
    /// the total comes from this vector, so the truncation belongs downstream, in
    /// the `+N more` marker). An error read leaves these exactly as they were.
    pub columns: Vec<ColumnBeads>,
    /// The `deferred` beads from the same read: not a row, a count (§1). The
    /// footer is the only place they reach the eye, which is what keeps them
    /// from vanishing without putting them back in play.
    pub deferred: usize,
    /// How old the last good read is, as of the moment the frame was built.
    ///
    /// Computed by whoever holds the clock — the poller at publish time, or the
    /// frame at paint time — and passed in. `None` means there has never been a
    /// good read, which is a different claim from `Some(0s)`: the first says "we
    /// do not know yet", the second says "we just looked".
    ///
    /// Which is exactly the trouble with it: a `Duration` is a *distance*, so it
    /// is true the moment it is written and wrong forever after. For anything
    /// that outlives the instant it was made, read [`BoardSnapshot::fetched_at`]
    /// and subtract.
    pub age: Option<Duration>,
    /// The instant the last **good** read finished, when whoever built this had a
    /// clock to give (ADR-0007 F3).
    ///
    /// This is the reading; [`Self::age`] is what somebody decided to call its
    /// distance. The poller publishes once and the frame paints sixty times a
    /// second off that one value, so "how old is this?" has to be answerable at
    /// paint time from the snapshot alone — otherwise the footer says `bd ok · 0s
    /// ago` about a five-second-old read, which is the freshness marker telling
    /// the opposite of the truth. [`BoardSnapshot::age_of_last_good`] does the
    /// subtraction; [`BoardSnapshot::restamp_age`] writes the answer back into
    /// [`Self::age`] for a reader that only speaks `age`.
    ///
    /// `None` when nothing was ever stamped — the never-loaded band, or a
    /// hand-built value with no clock. Such a snapshot keeps the `age` it was
    /// handed and ages no further: there is nothing to date it from, and
    /// inventing a start instant would be guessing.
    pub fetched_at: Option<Instant>,
}

impl BoardSnapshot {
    /// The band before the first read lands: nothing counted, and the footer's
    /// `reading the board…` says so.
    ///
    /// This is looprs-037's conflation killed at the type level: "bd is broken"
    /// is [`BoardRead`] variants, "bd answered an empty board" is
    /// [`BoardSnapshot::from_beads`] with nothing in it, and "we have not asked
    /// yet" is this. Three values, three renderings, no way to confuse them by
    /// leaving one out.
    pub fn loading() -> Self {
        Self {
            read: BoardRead::Never,
            columns: empty_columns(),
            deferred: 0,
            age: None,
            fetched_at: None,
        }
    }

    /// The snapshot a good read produces.
    ///
    /// One consistent read in, one mapping out (ADR-0007 §3: one read per tick,
    /// never three that can disagree with each other). Order inside a column is
    /// the read's own order.
    pub fn from_beads(beads: &[Bead], age: Option<Duration>) -> Self {
        let mut snap = Self {
            read: BoardRead::Ok,
            columns: empty_columns(),
            deferred: 0,
            age,
            fetched_at: None,
        };
        for bead in beads {
            let status = bead.status();
            // `Bead::status()` already folded `BeadStatusFallback::Missing` into
            // `Unknown`, so a bead that arrives with no status field at all lands
            // in To-do marked `?` — the honest reading, rendered.
            match column_of(status) {
                Some(column) => snap
                    .columns
                    .iter_mut()
                    .find(|c| c.column == column)
                    .expect("empty_columns builds every column")
                    .beads
                    .push(BoardBead::from_bead(bead)),
                None => snap.deferred += 1,
            }
        }
        snap
    }

    /// The snapshot an error produces: **the same good rows, kept**, with the
    /// read replaced by the error's variant.
    ///
    /// ADR-0007 §4, "an error never destroys the last good snapshot". The
    /// widget draws what is in `columns` dimmed and marks the band stale; what
    /// it must never do is fall back to an empty rendering, because then `bd:
    /// command not found` and `board empty` look the same from the band alone —
    /// the exact conflation looprs-037 was filed against.
    pub fn with_error(&self, err: &BdError, age: Option<Duration>) -> Self {
        Self {
            read: BoardRead::from(err),
            columns: self.columns.clone(),
            deferred: self.deferred,
            age,
            // The stamp rides along with the rows it dates. An error does not
            // move when the board was last seen — it *starts* the count of how
            // long it has been since it was, which is the number the stale
            // marker is made of.
            fetched_at: self.fetched_at,
        }
    }

    /// Stamp the instant this snapshot's read completed.
    #[must_use]
    pub fn stamped_at(mut self, at: Instant) -> Self {
        self.fetched_at = Some(at);
        self
    }

    /// How old the last **good** read is as of `now`, or `None` if there has
    /// never been one — the never-loaded band and the five-second-old board are
    /// still, and must stay, different answers.
    ///
    /// Takes `now` rather than reading a clock so the whole board stays paintable
    /// without a subprocess, a terminal or a `sleep`, exactly as the module doc
    /// promises. The one place a real clock belongs is the poller that produced
    /// the stamp.
    pub fn age_of_last_good(&self, now: Instant) -> Option<Duration> {
        // `saturating`: a monotonic clock cannot go backwards, but a snapshot
        // carried across a clock source it does not know about should read as
        // `0s` rather than panic in a paint path.
        self.fetched_at.map(|at| now.saturating_duration_since(at))
    }

    /// Rewrite [`Self::age`] from [`Self::fetched_at`] as of `now`, leaving
    /// every other field alone.
    ///
    /// For the reader that holds a snapshot of its own and wants its `age` to be
    /// true at this painting — one field write, no clone of the bead vectors,
    /// which is the whole reason [`Self::age`] is not simply recomputed inside
    /// the widget.
    pub fn restamp_age(&mut self, now: Instant) {
        self.age = self.age_of_last_good(now);
    }

    /// Would this snapshot paint **different pixels** from `other`?
    ///
    /// The *drawn* fields and nothing else: the read's state, the rows of every
    /// column, and the deferred count. `age` and `fetched_at` are deliberately
    /// left out, and that omission is the entire reason this function exists
    /// instead of `==`.
    ///
    /// A poll of a board that has not moved returns the same beads with a new
    /// timestamp. Under `PartialEq` that is a change, the frame repaints on
    /// every tick of the poller, forever, in a window nobody touched — which is
    /// the property the band is sold on having the opposite of. Under this
    /// function it is not a change and no frame is drawn at all.
    ///
    /// What the footer's `bd ok · Ns ago` then shows is not a frozen age, because
    /// the age is not what gets compared: it is re-derived from `fetched_at` at
    /// paint time (`App::on_tick` calling [`Self::restamp_age`]), so whatever
    /// frame is on screen states the age as of itself. Freshness is a value the
    /// frame re-reads; it is not news, and news is what costs a repaint.
    ///
    /// Written as the list it is rather than as `self == other` minus two fields,
    /// so that adding a field to [`BoardSnapshot`] forces the question this
    /// function exists to answer: *is it drawn?* If yes it belongs in this list;
    /// if it is a clock, it does not.
    pub fn same_paint_as(&self, other: &Self) -> bool {
        self.read == other.read && self.columns == other.columns && self.deferred == other.deferred
    }

    /// The rows of one column, from the last good read.
    pub fn beads_in(&self, column: Column) -> &[BoardBead] {
        self.columns
            .iter()
            .find(|c| c.column == column)
            .map(|c| c.beads.as_slice())
            .unwrap_or(&[])
    }

    /// Every row the last good read carried, across the three columns.
    /// `deferred` is not in this number: it is not a row.
    #[allow(dead_code)] // diagnostic/test seam: the band paints each column's count off
    // `header_count`, and never asks for the whole board's row count — but I1
    // ("every bead in the read is counted exactly once") is only checkable
    // against a total, and the poller's log line wants one too.
    pub fn total(&self) -> usize {
        self.columns.iter().map(|c| c.beads.len()).sum()
    }

    /// The header's count for a column, as text.
    ///
    /// **The true total for the column, never the number of rows this frame
    /// happens to draw** (ADR-0007 rule 7) — which is why the count is computed
    /// here from the snapshot rather than by counting what got painted. When the
    /// read did not answer, nothing new is counted and the header says `—`.
    pub fn header_count(&self, column: Column) -> String {
        if self.read.is_ok() {
            self.beads_in(column).len().to_string()
        } else {
            UNKNOWN_COUNT.to_string()
        }
    }
}

/// What a column header says for its count when the read cannot say.
const UNKNOWN_COUNT: &str = "—";

/// All three columns, each empty, in draw order. The builder's starting point so
/// that "all three columns are always present" is a property of one function.
fn empty_columns() -> Vec<ColumnBeads> {
    Column::ALL
        .iter()
        .map(|&column| ColumnBeads {
            column,
            beads: Vec::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::bd::{BeadIssueType, BeadStatusFallback};

    fn bead(id: &str, status: BeadStatus) -> Bead {
        Bead {
            id: id.to_string(),
            title: format!("{id} title"),
            status: BeadStatusFallback::Known(status),
            issue_type: BeadIssueType::Task,
        }
    }

    /// Every value the enum has, in one array. Written out rather than
    /// iterated-with-a-default so that adding a `BeadStatus` variant breaks this
    /// test by failing to compile the array's length/type — which is the point:
    /// ADR-0007 rule 1 says no status may be dropped, and the exhaustiveness is
    /// the test.
    const EVERY_STATUS: [BeadStatus; 8] = [
        BeadStatus::Open,
        BeadStatus::InProgress,
        BeadStatus::Blocked,
        BeadStatus::Deferred,
        BeadStatus::Ready,
        BeadStatus::Done,
        BeadStatus::Closed,
        BeadStatus::Unknown,
    ];

    /// ADR-0007 §1's mapping, transcribed from the table rather than derived
    /// from the function, so a change to one that the other does not follow is a
    /// failing test rather than a quiet drift.
    #[test]
    fn the_status_to_column_mapping_is_the_adrs_table() {
        /// (status, column, marker) — the ADR's table, in `BeadStatus` order.
        type Row = (&'static str, Option<Column>, Option<Marker>);
        let table: [Row; 8] = [
            ("open", Some(Column::ToDo), None),
            ("in_progress", Some(Column::InProgress), None),
            ("blocked", Some(Column::ToDo), Some(Marker::Blocked)),
            ("deferred", None, None),
            ("ready", Some(Column::ToDo), None),
            ("done", Some(Column::Complete), None),
            ("closed", Some(Column::Complete), None),
            ("unknown", Some(Column::ToDo), Some(Marker::Unknown)),
        ];
        for ((want_status, want_col, want_marker), status) in table.iter().zip(EVERY_STATUS.iter())
        {
            assert_eq!(
                status.as_str(),
                *want_status,
                "table out of step with the enum"
            );
            assert_eq!(
                column_of(*status),
                *want_col,
                "{want_status} is not in the column ADR-0007 §1 puts it in"
            );
            assert_eq!(
                Marker::of(*status),
                *want_marker,
                "{want_status} does not carry the marker the ADR gives it"
            );
        }
    }

    /// The marker half of the same table.
    #[test]
    fn blocked_and_unknown_are_the_marked_rows_and_nothing_else_is() {
        assert_eq!(Marker::of(BeadStatus::Blocked), Some(Marker::Blocked));
        assert_eq!(Marker::of(BeadStatus::Unknown), Some(Marker::Unknown));
        for s in EVERY_STATUS {
            if s != BeadStatus::Blocked && s != BeadStatus::Unknown {
                assert_eq!(Marker::of(s), None, "{s} must not carry a marker");
            }
        }
        assert_eq!(Marker::Blocked.glyph(), "⊘");
        assert_eq!(Marker::Unknown.glyph(), "?");
    }

    /// **I1**, at the level of the read: every bead in the read is counted once,
    /// in exactly one visible place — a column or the deferred count. A dropped
    /// `Unknown`, or a `deferred` merged into To-do, fails here rather than in
    /// front of a user.
    #[test]
    fn every_bead_in_the_read_is_counted_exactly_once() {
        let beads: Vec<Bead> = EVERY_STATUS
            .iter()
            .enumerate()
            .map(|(i, &s)| bead(&format!("looprs-{i}"), s))
            .collect();
        let snap = BoardSnapshot::from_beads(&beads, None);

        let in_columns: usize = snap.columns.iter().map(|c| c.beads.len()).sum();
        assert_eq!(
            in_columns + snap.deferred,
            beads.len(),
            "Σ column rows + deferred must equal the read"
        );
        // The ADR's *stronger* form: not one bead appears in two columns.
        let mut ids: Vec<&str> = snap
            .columns
            .iter()
            .flat_map(|c| c.beads.iter())
            .map(|b| b.id.as_str())
            .collect();
        ids.sort();
        let n = ids.len();
        ids.dedup();
        assert_eq!(ids.len(), n, "a bead appears in two columns: {ids:?}");
    }

    #[test]
    fn deferred_is_a_count_and_never_a_row() {
        let snap = BoardSnapshot::from_beads(
            &[
                bead("looprs-a", BeadStatus::Deferred),
                bead("looprs-b", BeadStatus::Deferred),
                bead("looprs-c", BeadStatus::Open),
            ],
            None,
        );
        assert_eq!(snap.deferred, 2);
        assert_eq!(snap.beads_in(Column::ToDo).len(), 1);
        for col in Column::ALL {
            assert!(
                !snap
                    .beads_in(col)
                    .iter()
                    .any(|b| b.id == "looprs-a" || b.id == "looprs-b"),
                "{:?} has a deferred row in it",
                col.name()
            );
        }
    }

    /// The three states looprs-037 refuses to conflate are three different
    /// values, not three spellings of one value.
    #[test]
    fn loading_an_empty_board_and_a_broken_bd_are_three_different_values() {
        let loading = BoardSnapshot::loading();
        let empty = BoardSnapshot::from_beads(&[], Some(Duration::from_secs(1)));
        let broken = empty.with_error(
            &BdError::Unavailable {
                bin: "bd".into(),
                reason: "No such file or directory".into(),
            },
            Some(Duration::from_secs(1)),
        );
        assert_eq!(loading.read, BoardRead::Never);
        assert!(empty.read.is_ok() && !empty.read.is_error());
        assert!(broken.read.is_error() && !broken.read.is_ok());
        assert_ne!(loading, empty, "not-loaded and empty must not be one value");
    }

    /// §4: the error keeps the data. `with_error` does not clear, does not
    /// truncate and does not re-read — it only replaces the read.
    #[test]
    fn an_error_keeps_every_row_of_the_last_good_read() {
        let good = BoardSnapshot::from_beads(
            &[
                bead("looprs-1", BeadStatus::Open),
                bead("looprs-2", BeadStatus::InProgress),
                bead("looprs-3", BeadStatus::Closed),
                bead("looprs-4", BeadStatus::Deferred),
            ],
            Some(Duration::from_secs(2)),
        );
        for (variant, err) in [
            (
                "unavailable",
                BdError::Unavailable {
                    bin: "bd".into(),
                    reason: "not installed".into(),
                },
            ),
            (
                "failed",
                BdError::Failed {
                    bin: "bd".into(),
                    args: "list".into(),
                    code: Some(3),
                    stderr: "repository lock held\nsecond line nobody reads\n".into(),
                },
            ),
            (
                "malformed",
                BdError::Malformed {
                    bin: "bd".into(),
                    args: "list".into(),
                    reason: "expected array".into(),
                    raw: "not json".into(),
                },
            ),
            (
                "timeout",
                BdError::Timeout {
                    bin: "bd".into(),
                    args: "list".into(),
                },
            ),
        ] {
            let after = good.with_error(&err, Some(Duration::from_secs(30)));
            assert_eq!(
                after.columns, good.columns,
                "{variant} must not touch the last good rows"
            );
            assert_eq!(
                after.deferred, good.deferred,
                "{variant} must not touch the deferred count"
            );
            assert!(
                after.read.is_error(),
                "{variant} did not mark the read as an error"
            );
            // And the header stops counting, because the read did not answer.
            for col in Column::ALL {
                assert_eq!(after.header_count(col), UNKNOWN_COUNT, "{variant}");
            }
        }
    }

    /// The error's own words survive the fold, and only the *first* line: the
    /// footer is one row and a stack trace does not fit on it.
    #[test]
    fn the_error_folds_to_its_first_line_only() {
        let read = BoardRead::from(&BdError::Failed {
            bin: "bd".into(),
            args: "list".into(),
            code: Some(9),
            stderr: "  Dolt lock held by pid 4211  \nfull trace here\n".to_string(),
        });
        assert_eq!(
            read,
            BoardRead::Failed {
                code: Some(9),
                message: Some("Dolt lock held by pid 4211".into())
            }
        );
        // Silent failure: no message, but still the exit code.
        let quiet = BoardRead::from(&BdError::Failed {
            bin: "bd".into(),
            args: "list".into(),
            code: Some(1),
            stderr: "   \n".to_string(),
        });
        assert_eq!(
            quiet,
            BoardRead::Failed {
                code: Some(1),
                message: None
            }
        );
    }

    /// Before the first read, **nothing** is counted, and the header says so with
    /// `—` rather than with a `0` that would read as an empty board.
    #[test]
    fn before_the_first_read_nothing_is_counted_and_the_header_says_so() {
        let loading = BoardSnapshot::loading();
        for col in Column::ALL {
            assert_eq!(loading.header_count(col), UNKNOWN_COUNT, "{}", col.name());
        }
        assert_eq!(loading.total(), 0);
        assert_eq!(loading.deferred, 0);
        assert_eq!(loading.age, None, "not-loaded carries no age");
    }

    /// The good-read counterpart: the header count is the column's true total,
    /// independent of what a frame later decides to draw.
    #[test]
    fn the_header_count_is_the_columns_true_total() {
        let snap = BoardSnapshot::from_beads(
            &[
                bead("looprs-a", BeadStatus::Open),
                bead("looprs-b", BeadStatus::Blocked),
                bead("looprs-c", BeadStatus::InProgress),
                bead("looprs-d", BeadStatus::Closed),
            ],
            Some(Duration::from_secs(4)),
        );
        assert_eq!(snap.header_count(Column::ToDo), "2");
        assert_eq!(snap.header_count(Column::InProgress), "1");
        assert_eq!(snap.header_count(Column::Complete), "1");
    }

    #[test]
    fn all_three_columns_are_present_even_when_two_have_nothing_in_them() {
        let snap = BoardSnapshot::from_beads(&[bead("looprs-z", BeadStatus::Open)], None);
        assert_eq!(snap.columns.len(), Column::ALL.len());
        let order: Vec<Column> = snap.columns.iter().map(|c| c.column).collect();
        assert_eq!(order, Column::ALL.to_vec(), "columns come in draw order");
        assert!(snap.beads_in(Column::InProgress).is_empty());
        assert!(snap.beads_in(Column::Complete).is_empty());
    }

    /// The order inside a column is the read's order — ADR-0007 §2 keeps
    /// `bd list`'s priority order rather than inventing one here.
    #[test]
    fn the_reads_own_order_is_preserved_inside_a_column() {
        let snap = BoardSnapshot::from_beads(
            &[
                bead("looprs-first", BeadStatus::Open),
                bead("looprs-second", BeadStatus::Ready),
                bead("looprs-third", BeadStatus::Unknown),
            ],
            None,
        );
        let ids: Vec<&str> = snap
            .beads_in(Column::ToDo)
            .iter()
            .map(|b| b.id.as_str())
            .collect();
        assert_eq!(ids, vec!["looprs-first", "looprs-second", "looprs-third"]);
    }

    /// A bead that arrives with no `status` field at all is `Unknown`, and shows
    /// up in To-do with `?` — the rule `bd.rs` already commits to, rendered.
    #[test]
    fn a_bead_with_no_status_field_is_a_marked_unknown_not_an_open_row() {
        let mut b = bead("looprs-nostatus", BeadStatus::Open);
        b.status = BeadStatusFallback::Missing;
        let snap = BoardSnapshot::from_beads(&[b], None);
        let row = &snap.beads_in(Column::ToDo)[0];
        assert_eq!(row.marker, Some(Marker::Unknown));
    }

    #[test]
    fn the_three_names_are_the_boards_three_words() {
        let names: Vec<&str> = Column::ALL.iter().map(|c| c.name()).collect();
        assert_eq!(names, vec!["To-do", "In progress", "Complete"]);
    }
}
