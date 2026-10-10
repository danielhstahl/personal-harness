//! The `bd` (beads) service: an async, honest-error wrapper over the `bd` CLI.
//!
//! Three rules, and the whole ticket (looprs-037) is them:
//!
//! 1. **Never block the runtime.** Every call is `tokio::process`, awaited. The old
//!    `std::process::Command::output()` inside the beads loop could park a tokio
//!    worker for hundreds of ms on a Dolt-backed board, which shows up as the
//!    60 fps tick stuttering in `main.rs`.
//! 2. **Never conflate "bd is broken" with "the board is empty".** Both are
//!    `Result`s: an empty board is `Ok(vec![])`, a failing `bd` is
//!    [`BdError::Failed`] carrying the exit code *and* the captured stderr. The
//!    difference is the whole reason the old code looked healthy while doing
//!    nothing: `stderr(Stdio::null())` threw "bd: command not found" away and a
//!    non-zero exit with empty stdout parsed as an empty board.
//! 3. **Degrade, don't break, on a bd upgrade.** Unknown statuses and issue types
//!    deserialize into `Unknown` / the raw string rather than failing the whole
//!    payload, so a newer `bd` cannot turn into "no beads".
//!
//! Every function takes the binary explicitly (`bin: &str`). That is the test seam:
//! production passes `"bd"` (or `$LOOPRS_BD_BIN`), tests pass a fake that records
//! what it was asked to do (see [`crate::testing`]).
//!
//! Every call is bounded by [`BD_TIMEOUT`]. A wedged `bd` (Dolt lock, network
//! filesystem) must not be able to hold a session's mailbox shut forever, because
//! the beads session handles `Esc` on that same mailbox.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use std::process::Stdio;
use tokio::process::Command;

/// Bound on any single `bd` invocation.
///
/// Generous enough for a slow Dolt-backed board, short enough that a wedged one is
/// a reported error rather than a frozen UI.
pub const BD_TIMEOUT: Duration = Duration::from_secs(30);

/// How a `bd` call can fail. Distinct variants, because the *remedy* differs and a
/// single "bd failed" string cannot express that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BdError {
    /// The process could not be started at all: not on PATH, not executable, a bad
    /// `LOOPRS_BD_BIN`. The board's contents are unknown, not empty.
    Unavailable { bin: String, reason: String },
    /// `bd` ran and exited non-zero. Carries the exit code and stderr, which is
    /// where "no .beads repo here" and every Dolt error actually lives.
    Failed {
        bin: String,
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    /// `bd` exited 0 but stdout was not the JSON we were told it would be.
    Malformed {
        bin: String,
        args: String,
        reason: String,
        raw: String,
    },
    /// `bd` did not answer within [`BD_TIMEOUT`].
    Timeout { bin: String, args: String },
}

impl std::fmt::Display for BdError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BdError::Unavailable { bin, reason } => {
                write!(f, "cannot run `{bin}`: {reason} (is it installed?)")
            }
            BdError::Failed {
                bin,
                args,
                code,
                stderr,
            } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                write!(f, "`{bin} {args}` failed (exit {code})")?;
                let detail = stderr.trim();
                if !detail.is_empty() {
                    write!(f, ": {detail}")?;
                }
                Ok(())
            }
            BdError::Malformed {
                bin,
                args,
                reason,
                raw,
            } => write!(
                f,
                "`{bin} {args}` returned unreadable JSON: {reason} (got {:?})",
                truncate(raw, 200)
            ),
            BdError::Timeout { bin, args } => {
                write!(f, "`{bin} {args}` did not answer within {:?}", BD_TIMEOUT)
            }
        }
    }
}

impl std::error::Error for BdError {}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut cut = max;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}…", &s[..cut])
    }
}

/// A bead's lifecycle state.
///
/// `#[serde(other)]` is the whole "a bd upgrade must not break the loop" story: a
/// status this build has never heard of becomes [`BeadStatus::Unknown`] rather than
/// a deserialization error that would surface as "no beads". Unknown is treated as
/// *not closed* everywhere, which is the conservative reading — the harness will
/// not assume a ticket it cannot classify is finished.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadStatus {
    Open,
    InProgress,
    Blocked,
    Deferred,
    Ready,
    Done,
    Closed,
    #[serde(other)]
    Unknown,
}

impl BeadStatus {
    /// "This ticket is out of the loop's way." `done` and `closed` are the only
    /// states a worker's job ends in; everything else is still owed work.
    pub fn is_closed(self) -> bool {
        matches!(self, Self::Done | Self::Closed)
    }

    /// "A human took this out of the loop on purpose, and a worker must not put
    /// itself back in."
    ///
    /// A `blocked` bead is waiting on somebody else; a `deferred` one has been
    /// postponed. A pass spent on either buys a ticket that is neither closer nor
    /// finished, so the loop skips them — and says so, because a silently skipped
    /// ticket is indistinguishable from a lost one.
    ///
    /// `Unknown` is deliberately **not** in here: treating a status this build has
    /// never seen as "needs a human" would let a `bd` upgrade silently drain the
    /// board. Unknown beads stay workable; unknown *closures* are the conservative
    /// side of the same rule (see [`BeadStatus::is_closed`]).
    ///
    /// This is the whole "may the worker take this?" question, expressed as a
    /// refusal rather than as a whitelist of open/in_progress/ready: a whitelist
    /// would silently exclude exactly the one case a newer `bd` can produce.
    pub fn needs_human(self) -> bool {
        matches!(self, Self::Blocked | Self::Deferred)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Ready => "ready",
            Self::Done => "done",
            Self::Closed => "closed",
            Self::Unknown => "unknown",
        }
    }
}

impl std::fmt::Display for BeadStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A bead's kind. Same degrade-don't-break rule as [`BeadStatus`]: a kind this
/// build has never heard of is `Unknown`, not a failed payload.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadIssueType {
    Task,
    Bug,
    Feature,
    Chore,
    Epic,
    Decision,
    Spike,
    Story,
    Milestone,
    #[default]
    #[serde(other)]
    Unknown,
}

/// One bead. Fields are public: the status row (looprs-guh), the claim guard
/// (looprs-w7q) and the worker prompt all need them, and the old private-and-unused
/// version was dead weight the compiler kept pointing at.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Bead {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub status: BeadStatusFallback,
    #[serde(default)]
    pub issue_type: BeadIssueType,
}

/// `status` as a *value* that can be absent. A missing status is not "open" — it
/// is "unknown", which is the honest answer and reads correctly in the transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(untagged)]
pub enum BeadStatusFallback {
    #[default]
    Missing,
    Known(BeadStatus),
}

impl BeadStatusFallback {
    pub fn status(self) -> BeadStatus {
        match self {
            Self::Missing => BeadStatus::Unknown,
            Self::Known(s) => s,
        }
    }
}

impl Bead {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn status(&self) -> BeadStatus {
        self.status.status()
    }
    pub fn is_closed(&self) -> bool {
        self.status().is_closed()
    }
    pub fn needs_human(&self) -> bool {
        self.status().needs_human()
    }
}

/// `bd` answers with a bare JSON array (`bd list`, `bd show`) unless
/// `BD_JSON_ENVELOPE=1` is set, in which case it wraps it as `{"data": [...]}`
/// plus a schema version. All three shapes parse, because pinning the harness to
/// one is a self-inflicted break on the first `bd` upgrade: `show` returning a
/// bare object rather than a one-element array is a plausible change, and the cost
/// of accepting it is one line.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BdList {
    Envelope { data: Vec<Bead> },
    Bare(Vec<Bead>),
    One(Bead),
}

impl BdList {
    fn into_vec(self) -> Vec<Bead> {
        match self {
            BdList::Envelope { data } => data,
            BdList::Bare(v) => v,
            BdList::One(b) => vec![b],
        }
    }
}

/// A `bd` that ran, with all three of its answers kept whatever they were.
///
/// This exists because "stdout only, and only on success" is the wrong shape for
/// the events journal probe: `bd events tail` answers a *refusal* with a JSON
/// object on stdout and a non-zero exit, and answers a **success** with an
/// explanatory note on stderr. A wrapper that throws away one of the three can
/// see neither, and "the journal is disabled" and "bd is broken" are the last
/// two things in this file that may be confused (looprs-037).
struct RawRun {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    code: Option<i32>,
    success: bool,
}

impl RawRun {
    fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).to_string()
    }
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).to_string()
    }
}

/// Spawn, await under [`BD_TIMEOUT`], and hand back whatever came out.
///
/// Fails only on the two things that mean "there is no answer at all": the
/// process could not be started, or it never finished. A non-zero exit is an
/// *answer* and comes back in the [`RawRun`] for the caller to read.
async fn run_raw(bin: &str, args: &[&str]) -> Result<RawRun, BdError> {
    let spawned = Command::new(bin)
        .args(args)
        .env("BD_JSON_ENVELOPE", "1")
        // stdin is closed, not inherited: an interactive `bd` prompt must not be
        // able to steal keystrokes from the TUI.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();

    let child = match spawned {
        Ok(c) => c,
        Err(e) => {
            return Err(BdError::Unavailable {
                bin: bin.to_string(),
                reason: e.to_string(),
            });
        }
    };

    let waited = tokio::time::timeout(BD_TIMEOUT, child.wait_with_output()).await;
    let out = match waited {
        Err(_) => {
            return Err(BdError::Timeout {
                bin: bin.to_string(),
                args: args.join(" "),
            });
        }
        Ok(Err(e)) => {
            return Err(BdError::Unavailable {
                bin: bin.to_string(),
                reason: e.to_string(),
            });
        }
        Ok(Ok(out)) => out,
    };

    Ok(RawRun {
        stdout: out.stdout,
        stderr: out.stderr,
        code: out.status.code(),
        success: out.status.success(),
    })
}

/// Run `bd` with args, bounded, with stdout and stderr both captured.
///
/// The single place that touches `Stdio`, so "stderr is never discarded" is a
/// property of one function rather than a habit. stdout is returned only on exit
/// code 0; anything else is a [`BdError`] that carries stderr with it.
async fn run(bin: &str, args: &[&str]) -> Result<Vec<u8>, BdError> {
    let args_joined = args.join(" ");
    let out = run_raw(bin, args).await?;

    if !out.success {
        return Err(BdError::Failed {
            bin: bin.to_string(),
            args: args_joined,
            code: out.code,
            stderr: out.stderr_text(),
        });
    }
    Ok(out.stdout)
}

async fn beads(bin: &str, args: &[&str]) -> Result<Vec<Bead>, BdError> {
    let raw = run(bin, args).await?;
    match serde_json::from_slice::<BdList>(&raw) {
        Ok(list) => Ok(list.into_vec()),
        Err(e) => Err(BdError::Malformed {
            bin: bin.to_string(),
            args: args.join(" "),
            reason: e.to_string(),
            raw: String::from_utf8_lossy(&raw).to_string(),
        }),
    }
}

/// The `bd` binary this run should use: `$LOOPRS_BD_BIN`, else `bd`.
///
/// One home for the default so the beads loop and the board poller cannot drift
/// onto two different binaries — ADR-0007 §5 wants the board reading *the same*
/// `bd` the loop reads, or "the board" means two things in one run.
pub fn bd_bin_from_env() -> String {
    std::env::var("LOOPRS_BD_BIN").unwrap_or_else(|_| "bd".to_string())
}

/// `bd --readonly list --all --limit 0 --json` — the whole board in **one**
/// consistent read (ADR-0007 §3). This is the board poller's read.
///
/// Four flags, each load-bearing, so they are written out rather than assembled:
///
/// * **`--readonly`** is a *global* `bd` flag and therefore has to come before
///   the subcommand. It is free, and it makes "the board cannot change the
///   board" a property of the command line instead of of good intentions: a
///   write under it is refused (`rc=1`, "operation 'update' is not allowed in
///   read-only mode") and the bead is unchanged.
/// * **`--all`** — without it `bd list` hides closed beads, so the Complete
///   column would read empty forever.
/// * **`--limit 0`** — mandatory per looprs-037. `bd list` defaults to 50
///   rows, and a truncated page makes every count in the snapshot a lie about
///   a bigger board.
/// * **one read, not three.** Three per-status queries measured 2.9× the cost
///   of this one *and* can disagree with each other inside a frame: a bead that
///   closes between query 1 and query 2 shows up in two columns at once.
///
/// `--skip-labels` is deliberately absent — measured to save nothing and to
/// change the payload into a shape [`BdList`] reports as `Malformed`.
pub async fn board_read_with(bin: &str) -> Result<Vec<Bead>, BdError> {
    beads(
        bin,
        &["--readonly", "list", "--all", "--limit", "0", "--json"],
    )
    .await
}

// ─────────────────────── the events journal ───────────────────────

/// What `bd` puts in every journal record that this crate cares about.
///
/// Only `seq` is read. Deliberately: each record also carries the **whole issue
/// as it stood after the mutation** (`{"seq":..,"op":..,"issue":{…}}`), which at
/// a few hundred bytes to a few kilobytes a row is the payload this crate is
/// trying *not* to pay for on a five-second timer. Serde skips the rest.
#[derive(Debug, Deserialize)]
struct JournalRecordSeq {
    seq: i64,
}

/// `bd`'s refusal when a checkpoint has been pruned away, verbatim shape:
///
/// ```json
/// {"code":"events_journal_truncated","error":"…","floor":150,"head":202,
///  "schema_version":1,"since":0}
/// ```
#[derive(Debug, Deserialize)]
struct JournalTruncated {
    code: String,
    #[serde(default)]
    floor: i64,
    #[serde(default)]
    head: i64,
}

/// The code string `bd` uses for the one journal failure a consumer can recover
/// from on its own, because the answer names where to resume.
const JOURNAL_TRUNCATED: &str = "events_journal_truncated";

/// What a journal probe found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JournalProbe {
    /// The `seq` of every record newer than the `since` we asked with, ascending.
    /// **Empty is an answer, not a failure** — it means nothing was mutated since
    /// that point. This is the one place in this file where an empty response is
    /// read as "nothing" rather than as [`BdError::Malformed`], and it is the
    /// whole reason the journal is worth probing: see the note on
    /// [`journal_probe_with`].
    pub seqs: Vec<i64>,
    /// `bd` said the events journal is **disabled for this workspace** (it puts
    /// that on stderr even on a successful, empty read). Nothing new will ever
    /// arrive here, which is not an error but *is* a fact the caller has to know
    /// so it can keep refreshing the board some other way.
    pub disabled: bool,
}

impl JournalProbe {
    /// Nothing newer than the watermark.
    pub fn is_quiet(&self) -> bool {
        self.seqs.is_empty()
    }

    /// The highest seq this probe covered — the watermark to adopt once the
    /// change has been reflected in a full board read.
    pub fn head(&self) -> Option<i64> {
        self.seqs.last().copied()
    }

    /// Whether the probe filled its `--limit`, i.e. there may be more records
    /// behind it and this consumer is still behind the head of the journal.
    pub fn hit_limit(&self, limit: i64) -> bool {
        limit > 0 && self.seqs.len() as i64 >= limit
    }
}

/// How a journal probe can come back.
///
/// Its own error type rather than a [`BdError`] arm, because a journal failure is
/// **not** a board failure and must never reach the band's failure table by
/// accident: `BoardRead` has no arm for "the change detector broke", and it
/// should not. Every variant here means one thing to the poller — *go and read the
/// board* — and two of them additionally say where to resume from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// Our checkpoint is below the retained window, so the records between it and
    /// the floor are gone. Carries what `bd` named: `floor` (oldest retained seq)
    /// and `head` (highest ever assigned), which is exactly enough to
    /// re-baseline against a fresh full read.
    Truncated { since: i64, floor: i64, head: i64 },
    /// The probe could not be answered at all — `bd` is missing, wedged, or
    /// said something this build cannot read. Either way: unknown, and unknown is
    /// never "nothing changed".
    Unusable(BdError),
}

impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            JournalError::Truncated { since, floor, head } => write!(
                f,
                "events journal truncated: checkpoint {since} is below the retained window \
                 [{floor}..{head}]"
            ),
            JournalError::Unusable(err) => write!(f, "events journal unreadable: {err}"),
        }
    }
}

impl std::error::Error for JournalError {}

/// `bd --readonly events tail --since <since> --limit <limit> --json` — ask the
/// journal whether anything has been mutated since `since`, **without re-reading
/// the board**.
///
/// This is the cheap half of the board's polling (ADR-0007 §7): measured on this
/// repo's board at **~0.15 s wall / ~0.09 s CPU** against the full board read's
/// **~0.45 s / ~0.18 s**, and its cost scales with the number of records since
/// the watermark rather than with the size of the board — so an idle board costs
/// the poller a fraction of what a tick used to cost it.
///
/// Three things about this read are load-bearing and are the reason the return
/// type is a struct rather than a `Vec`:
///
/// * **Empty means "nothing changed", not "unreadable"** — the opposite of the
///   rule every other read in this file follows. That inversion is safe only
///   because a mutation *cannot* be journaled invisibly while the journal is on;
///   the cases where it *can* (a disabled journal, a `bd dolt pull` whose rows
///   arrived as data, `bd sql`) are surfaced by the `disabled` flag and by the
///   poller's periodic full re-read, never silently swallowed.
/// * **A refusal on stdout is a typed answer.** `--since` below the retention
///   floor exits non-zero with a JSON object naming `floor` and `head`; that is
///   [`JournalError::Truncated`] and it is recoverable.
/// * **`--readonly` still leads**, for the same reason the board read leads with
///   it: "the band cannot change the board" stays a property of the command line
///   across every read the band makes.
pub async fn journal_probe_with(
    bin: &str,
    since: i64,
    limit: i64,
) -> Result<JournalProbe, JournalError> {
    let since_arg = since.to_string();
    let limit_arg = limit.to_string();
    let args = [
        "--readonly",
        "events",
        "tail",
        "--since",
        &since_arg,
        "--limit",
        &limit_arg,
        "--json",
    ];
    let out = run_raw(bin, &args).await.map_err(JournalError::Unusable)?;
    let stdout = out.stdout_text();
    let stderr = out.stderr_text();
    // The disabled note arrives on stderr alongside a successful empty read; it
    // is the difference between "this board is quiet" and "this board will never
    // report anything here", and only the poller's full re-read keeps the band
    // honest in the second case.
    let disabled = stderr.contains("events journal is disabled");

    if !out.success {
        // A refusal, in the one shape `bd` documents for it. Anything else that
        // exited non-zero is just a broken `bd`, wrapped so the caller can say
        // which.
        if let Some(t) = parse_truncation(&stdout) {
            return Err(JournalError::Truncated {
                since,
                floor: t.floor,
                head: t.head,
            });
        }
        return Err(JournalError::Unusable(BdError::Failed {
            bin: bin.to_string(),
            args: args.join(" "),
            code: out.code,
            stderr,
        }));
    }

    let seqs = journal_seqs(&stdout).map_err(|e| {
        // A half-parsable journal is read as *unknown*, never as "no
        // changes": a probe that quietly reported nothing would freeze the board
        // on stale rows while the footer said `bd ok`.
        JournalError::Unusable(BdError::Malformed {
            bin: bin.to_string(),
            args: args.join(" "),
            reason: format!("journal record is not a `{{\"seq\":…}}` line: {e}"),
            raw: stdout.clone(),
        })
    })?;
    Ok(JournalProbe { seqs, disabled })
}

/// The JSONL body `bd events tail --json` answers with, as seq numbers.
///
/// Split out of [`journal_probe_with`] so the one thing that must not be
/// guessed at — "was that a record, or was that the end of the stream?" — is
/// testable against a string rather than only against a subprocess.
fn journal_seqs(raw: &str) -> Result<Vec<i64>, serde_json::Error> {
    let mut seqs = Vec::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        seqs.push(serde_json::from_str::<JournalRecordSeq>(line)?.seq);
    }
    Ok(seqs)
}

/// Parse `bd`'s truncation refusal out of a stdout blob. `None` for any other
/// payload — including a well-formed JSON object with a different `code`, which
/// is somebody else's problem and reads as `Unusable`.
fn parse_truncation(stdout: &str) -> Option<JournalTruncated> {
    let t: JournalTruncated = serde_json::from_str(stdout.trim()).ok()?;
    if t.code == JOURNAL_TRUNCATED {
        Some(t)
    } else {
        None
    }
}

/// `bd ready --json` — the beads the loop may pick up, in bd's own priority order.
///
/// `Ok(vec![])` means the board is genuinely empty. It does *not* mean "bd could
/// not tell us": that is `Err`, and the caller must not render it as empty.
pub async fn ready_with(bin: &str) -> Result<Vec<Bead>, BdError> {
    beads(bin, &["ready", "--json"]).await
}

/// `bd list --status <status> --json --limit 0` — every bead in one lifecycle
/// state, with the default page size turned off.
///
/// The planner diff (looprs-k7v) needs a board snapshot that does not depend on
/// priority/dependency filtering the way `ready` does.
///
/// `--limit 0` is load-bearing, not tidying: `bd list` defaults to 50 rows, and a
/// *differential* read over a truncated page reads as "nothing new" the moment the
/// board outgrows that page. A short read and an empty read have to stay
/// distinguishable, and the only way to keep them so is not to truncate in the
/// first place.
pub async fn list_status_with(bin: &str, status: &str) -> Result<Vec<Bead>, BdError> {
    beads(bin, &["list", "--status", status, "--limit", "0", "--json"]).await
}

/// `bd show <id> --json` — one bead by id, `Ok(None)` if bd knows nothing of it.
///
/// The read of truth for the post-settle check in looprs-w7q: "the worker
/// settled" and "the bead is closed" are different claims, and only `bd` can
/// answer the second one, so the caller asks for the bead's current status here
/// rather than trusting the worker's own last words.
///
/// `Ok(None)` — `bd` has never heard of this id — is *not* "closed". A claim on a
/// bead that is not on the board is the least verifiable state there is.
pub async fn show_with(bin: &str, id: &str) -> Result<Option<Bead>, BdError> {
    let found = beads(bin, &["show", id, "--json"]).await?;
    Ok(found.into_iter().find(|b| b.id == id))
}

/// `bd update <id> --claim` — atomically take the bead (looprs-w7q).
///
/// The harness claims *before* prompting the worker so that it knows, independent
/// of the agent, which bead it is spending tokens on. stdout is deliberately not
/// parsed: `--claim` answers with human prose, and exit code is the contract.
pub async fn claim_with(bin: &str, id: &str) -> Result<(), BdError> {
    run(bin, &["update", id, "--claim"]).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD};

    /// The three outcomes looprs-037 refuses to conflate, as three distinct tests.
    #[tokio::test]
    async fn an_empty_board_is_ok_with_nothing_in_it() {
        let fakes = Fakes::new(
            "bd-empty",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        let beads = ready_with(fakes.bd_bin())
            .await
            .expect("empty is not an error");
        assert!(beads.is_empty(), "an empty board must be Ok(vec![])");
    }

    #[tokio::test]
    async fn a_non_empty_board_parses_its_beads() {
        let fakes = Fakes::new(
            "bd-one",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            ONE_BEADED_BOARD,
        );
        let beads = ready_with(fakes.bd_bin()).await.unwrap();
        assert_eq!(beads.len(), 1);
        assert_eq!(beads[0].id(), "looprs-26r");
        assert_eq!(beads[0].title(), "Beads loop never self-starts");
        assert_eq!(beads[0].status(), BeadStatus::Open);
        assert_eq!(beads[0].issue_type, BeadIssueType::Bug);
        assert!(!beads[0].is_closed());
    }

    /// A failing `bd` is an error that *names itself and says why*, never an empty
    /// board. This is the exact conflation the ticket was filed for.
    #[tokio::test]
    async fn a_failing_bd_is_not_an_empty_board() {
        let fakes = Fakes::new(
            "bd-fails",
            crate::testing::PiFake::Started,
            BdFake::Fails,
            ONE_BEADED_BOARD,
        );
        let err = ready_with(fakes.bd_bin()).await.unwrap_err();
        assert!(
            matches!(err, BdError::Failed { code: Some(3), .. }),
            "expected the exit code to survive: {err:?}"
        );
        assert!(format!("{err}").contains("exit 3"), "{err}");
    }

    /// No binary at all: the failure names the binary and says it could not run it.
    /// The loop must show this, not "board empty, awaiting input".
    #[tokio::test]
    async fn a_missing_bd_is_named_not_gossiped_around() {
        let err = ready_with("/nonexistent/looprs/bd-not-installed")
            .await
            .unwrap_err();
        assert!(
            matches!(err, BdError::Unavailable { .. }),
            "a missing binary is Unavailable, not Failed: {err:?}"
        );
        let text = err.to_string();
        assert!(text.contains("bd-not-installed"), "{text}");
        assert!(text.contains("installed"), "{text}");
    }

    /// bd exits 0 with nonsense: distinguishable from both of the above, and it
    /// carries the offending bytes so the failure is diagnosable.
    #[tokio::test]
    async fn malformed_json_is_its_own_outcome() {
        let fakes = Fakes::new(
            "bd-malformed",
            crate::testing::PiFake::Started,
            BdFake::Malformed,
            "this is not json at all",
        );
        let err = ready_with(fakes.bd_bin()).await.unwrap_err();
        assert!(matches!(err, BdError::Malformed { .. }), "{err:?}");
        assert!(err.to_string().contains("unreadable JSON"), "{err}");
    }

    /// Empty-but-valid vs malformed must not collapse into the same error either:
    /// `bd` printing nothing and exiting 0 is a parse failure, not an empty board.
    #[tokio::test]
    async fn silence_from_bd_is_malformed_never_empty() {
        let fakes = Fakes::new(
            "bd-silent",
            crate::testing::PiFake::Started,
            BdFake::EmptyOutput,
            "",
        );
        let err = ready_with(fakes.bd_bin()).await.unwrap_err();
        assert!(matches!(err, BdError::Malformed { .. }), "{err:?}");
    }

    /// The board read is the ADR's literal command line, flag for flag — and
    /// `--readonly` in the position that makes it a *global* flag rather than a
    /// `list` option it would be rejected as.
    #[tokio::test]
    async fn the_board_read_is_the_boards_one_read_and_no_other_verb_ever() {
        let fakes = Fakes::new(
            "bd-board-read",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            ONE_BEADED_BOARD,
        );
        let beads = board_read_with(fakes.bd_bin()).await.unwrap();
        assert_eq!(beads.len(), 1);
        let lines = fakes.bd_log();
        assert_eq!(
            lines.as_slice(),
            &["--readonly list --all --limit 0 --json".to_string()],
            "ADR-0007 §3 fixes this command line exactly"
        );
    }

    /// The board read fails like every other read: a broken `bd` is an error,
    /// never an empty board — including when the binary is missing entirely.
    #[tokio::test]
    async fn a_board_that_cannot_be_read_is_never_reported_as_an_empty_board() {
        let fakes = Fakes::new(
            "bd-board-fails",
            crate::testing::PiFake::Started,
            BdFake::Fails,
            EMPTY_BOARD,
        );
        assert!(matches!(
            board_read_with(fakes.bd_bin()).await,
            Err(BdError::Failed { .. })
        ));
        assert!(matches!(
            board_read_with("/nonexistent/looprs/bd-not-installed").await,
            Err(BdError::Unavailable { .. })
        ));
        // …and a board that answers with an actual zero beads *is* an empty one.
        let empty = Fakes::new(
            "bd-board-empty",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        assert!(board_read_with(empty.bd_bin()).await.unwrap().is_empty());
    }

    /// Both shapes `bd` actually speaks parse: the envelope and the bare array.
    #[test]
    fn both_json_shapes_are_accepted() {
        let enveloped = r#"{"data":[{"id":"a-1","title":"T","status":"open","issue_type":"task"}],"schema_version":1}"#;
        let bare = r#"[{"id":"a-1","title":"T","status":"open","issue_type":"task"}]"#;
        for src in [enveloped, bare] {
            let got: Vec<Bead> = serde_json::from_str::<BdList>(src).expect(src).into_vec();
            assert_eq!(got.len(), 1, "{src}");
            assert_eq!(got[0].id(), "a-1");
        }
    }

    /// The forward-compatibility rule: an unknown status/type is `Unknown`, not a
    /// failed parse. A bd upgrade must read as "unclassified", never as "no beads".
    #[tokio::test]
    async fn a_newer_bd_degrades_instead_of_breaking() {
        let src = r#"{"data":[{"id":"x-1","title":"from the future","status":"frobnicated","issue_type":"quantum"}],"schema_version":99}"#;
        let got = serde_json::from_str::<BdList>(src)
            .expect("unknown enums must not fail the payload")
            .into_vec();
        assert_eq!(got[0].status(), BeadStatus::Unknown);
        assert_eq!(got[0].issue_type, BeadIssueType::Unknown);
        assert!(
            !got[0].is_closed(),
            "an unknown status is never assumed finished"
        );
    }

    /// A missing `status` field is "unknown", not a silently-assumed "open".
    #[test]
    fn a_missing_status_is_unknown_not_open() {
        let src = r#"[{"id":"x-2","title":"no status"}]"#;
        let got = serde_json::from_str::<BdList>(src).unwrap().into_vec();
        assert_eq!(got[0].status(), BeadStatus::Unknown);
        assert!(!got[0].is_closed());
    }

    #[test]
    fn closed_states_are_the_only_finished_ones() {
        assert!(BeadStatus::Closed.is_closed());
        assert!(BeadStatus::Done.is_closed());
        for s in [
            BeadStatus::Open,
            BeadStatus::InProgress,
            BeadStatus::Blocked,
            BeadStatus::Deferred,
            BeadStatus::Ready,
            BeadStatus::Unknown,
        ] {
            assert!(!s.is_closed(), "{s} is not finished");
        }
    }

    /// The `bd` fake records the whole command line, which is what makes the claim
    /// guard's "did the harness actually claim" assertion possible.
    #[tokio::test]
    async fn claim_runs_the_documented_command() {
        let fakes = Fakes::new(
            "bd-claim",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        claim_with(fakes.bd_bin(), "looprs-77").await.unwrap();
        assert!(
            fakes
                .bd_log()
                .iter()
                .any(|l| l.contains("update looprs-77 --claim")),
            "claim must be `bd update <id> --claim`: {:?}",
            fakes.bd_log()
        );
    }

    /// A refused claim is an error, not a silent success: if the harness thinks it
    /// claimed a bead it did not, the re-work guard is fiction.
    #[tokio::test]
    async fn a_refused_claim_is_an_error() {
        let fakes = Fakes::new(
            "bd-claim-fails",
            crate::testing::PiFake::Started,
            BdFake::Fails,
            EMPTY_BOARD,
        );
        let err = claim_with(fakes.bd_bin(), "looprs-77").await.unwrap_err();
        assert!(matches!(err, BdError::Failed { .. }), "{err:?}");
    }

    /// The listing used by the planner diff must not be paginated: a truncated page
    /// and an empty board would otherwise be the same read, and the diff would say
    /// "nothing was created" about a board with 51 open tickets.
    #[tokio::test]
    async fn listing_a_status_asks_bd_for_the_whole_page() {
        let fakes = Fakes::new(
            "bd-list-unlimited",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            ONE_BEADED_BOARD,
        );
        list_status_with(fakes.bd_bin(), "open").await.unwrap();
        let lines = fakes.bd_log();
        let line = lines
            .iter()
            .find(|l| l.starts_with("list --status open"))
            .unwrap_or_else(|| panic!("expected a `bd list --status open …`: {lines:?}"));
        assert!(
            line.contains("--limit 0"),
            "a paginated diff read is a silent lie about a big board: {line}"
        );
    }

    /// The post-settle read: the board's answer, not the worker's.
    #[tokio::test]
    async fn the_board_reads_as_the_truth_it_is_and_not_as_the_hope_it_isnt() {
        let fakes = Fakes::new(
            "bd-show",
            crate::testing::PiFake::Started,
            BdFake::ShowStatus,
            EMPTY_BOARD,
        );
        fakes.set_show(r#"{"id":"looprs-9","title":"t","status":"closed","issue_type":"task"}"#);
        assert!(
            show_with(fakes.bd_bin(), "looprs-9")
                .await
                .unwrap()
                .is_some_and(|b| b.is_closed())
        );
        fakes.set_show(r#"{"id":"looprs-9","title":"t","status":"open","issue_type":"task"}"#);
        assert!(
            !show_with(fakes.bd_bin(), "looprs-9")
                .await
                .unwrap()
                .is_some_and(|b| b.is_closed()),
            "an un-closed bead must read as un-closed"
        );
        // A bead bd does not know about is not "closed": the harness must not
        // conclude the worker finished just because the lookup came back empty.
        assert!(
            show_with(fakes.bd_bin(), "looprs-404")
                .await
                .unwrap()
                .is_none(),
            "an unknown bead is not closed"
        );
    }

    /// The two states the loop must not spend a pass on, and the ones it must not
    /// mistake for them.
    #[test]
    fn blocked_and_deferred_are_the_humans_tickets_not_the_loops() {
        assert!(BeadStatus::Blocked.needs_human());
        assert!(BeadStatus::Deferred.needs_human());
        for s in [
            BeadStatus::Open,
            BeadStatus::InProgress,
            BeadStatus::Ready,
            // An unfamiliar status is worked, not skipped: skipping it would let a
            // `bd` upgrade quietly empty the board.
            BeadStatus::Unknown,
            BeadStatus::Done,
            BeadStatus::Closed,
        ] {
            assert!(!s.needs_human(), "{s} is not a human-only ticket");
        }
    }

    /// The old serialization fixture, kept working through the field changes.
    #[test]
    fn test_serialization() {
        let raw_data = r#"
{
    "data": [
    {
        "id": "looprs-gbi",
        "title": "hello world",
        "status": "open",
        "priority": 2,
        "issue_type": "feature",
        "owner": "danstahl1138@gmail.com",
        "created_at": "2026-10-03T11:34:52Z",
        "created_by": "danielstahl",
        "updated_at": "2026-10-03T11:34:52Z",
        "dependency_count": 0,
        "dependent_count": 0,
        "comment_count": 0
    }
    ],
    "schema_version": 1
}
            "#;
        let result = serde_json::from_str::<BdList>(raw_data).unwrap().into_vec();
        assert_eq!(result[0].title, "hello world".to_string());
        assert_eq!(result[0].status(), BeadStatus::Open);
    }

    // ────────────────────── the events journal probe ──────────────────────

    /// The probe's command line, flag for flag. `--readonly` leads for the same
    /// reason it leads the board read — "the band cannot change the board" is a
    /// property of every command line the band runs, not just the big one — and
    /// the watermark and the batch size are where `bd` expects them.
    #[tokio::test]
    async fn the_journal_probe_is_a_readonly_tail_at_the_watermark() {
        let fakes = Fakes::new(
            "probe-cmd",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        journal_probe_with(fakes.bd_bin(), 41, 1).await.unwrap();
        assert_eq!(
            fakes.bd_log().as_slice(),
            &["--readonly events tail --since 41 --limit 1 --json".to_string()],
            "one fixed shape, two numbers in it"
        );
    }

    /// **An empty journal answers quiet, not broken.** This is the one place in
    /// this file where an empty stdout is *not* [`BdError::Malformed`], and it
    /// is worth writing down as its own test because the rule is the exact
    /// opposite of the one every other read here follows — and the reason it is
    /// safe is the poller's sweep, not anything about this response.
    #[tokio::test]
    async fn an_empty_journal_is_quiet_and_not_a_parse_failure() {
        let fakes = Fakes::new(
            "probe-empty",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        let probe = journal_probe_with(fakes.bd_bin(), 0, 512).await.unwrap();
        assert!(probe.is_quiet(), "{probe:?}");
        assert!(!probe.disabled, "an empty journal is not a disabled one");
        assert_eq!(probe.head(), None);
    }

    /// Records above the watermark come back as their seq numbers and nothing
    /// else: the whole issue payload each record carries is skipped, which is
    /// the economy the probe exists for.
    #[tokio::test]
    async fn the_probe_returns_records_above_the_watermark_as_seq_only() {
        let fakes = Fakes::new(
            "probe-records",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        // Records with the payload shape real `bd` writes — the full issue after
        // the mutation — to prove the parser ignores all of it.
        fakes.set_journal(
            "{\"seq\":1,\"ts\":\"2026-01-01T00:00:00Z\",\"op\":\"create\",\"issue_id\":\"l-1\",\"actor\":\"a\",\"issue\":{\"id\":\"l-1\",\"title\":\"a big long payload nobody reads\",\"status\":\"open\"}}\n{\"seq\":2,\"op\":\"close\",\"issue_id\":\"l-1\",\"issue\":{\"id\":\"l-1\",\"status\":\"closed\"}}\n{\"seq\":3,\"op\":\"dep_add\",\"issue_id\":\"l-2\",\"dep\":{\"kind\":\"blocks\",\"target\":\"l-1\",\"metadata\":null}}\n",
        );

        let all = journal_probe_with(fakes.bd_bin(), 0, 10).await.unwrap();
        assert_eq!(all.seqs, vec![1, 2, 3]);
        assert_eq!(all.head(), Some(3));
        assert!(!all.hit_limit(10));

        let since_one = journal_probe_with(fakes.bd_bin(), 1, 10).await.unwrap();
        assert_eq!(since_one.seqs, vec![2, 3], "the watermark is exclusive");

        let capped = journal_probe_with(fakes.bd_bin(), 0, 2).await.unwrap();
        assert_eq!(capped.seqs, vec![1, 2]);
        assert!(capped.hit_limit(2), "a full batch means \"still behind\"");
        assert!(!capped.hit_limit(10));
        // …and a 0 limit is "no cap", same as the CLI's.
        assert!(!capped.hit_limit(0));
    }

    /// A checkpoint the retention floors pruned away is the one journal failure
    /// that carries its own repair: `bd` names the floor it could not read below
    /// and the head it is at, which is enough to re-baseline against a fresh
    /// board read.
    #[tokio::test]
    async fn a_pruned_watermark_is_a_typed_answer_with_the_resume_address_in_it() {
        let fakes = Fakes::new(
            "probe-pruned",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        fakes.truncate_journal(5, 99);
        let err = journal_probe_with(fakes.bd_bin(), 0, 1).await.unwrap_err();
        assert_eq!(
            err,
            JournalError::Truncated {
                since: 0,
                floor: 5,
                head: 99
            },
            "{err}"
        );
        // Above the floor the same probe is fine again — the refusal is about the
        // checkpoint, not about the journal.
        assert!(
            journal_probe_with(fakes.bd_bin(), 99, 1)
                .await
                .unwrap()
                .is_quiet()
        );
    }

    /// A workspace with the journal switched off answers the probe *successfully
    /// and quietly*, forever, with its explanation on stderr. Carrying that note
    /// out is what lets the poller say out loud that the band is now riding on
    /// the sweep rather than on the change.
    #[tokio::test]
    async fn a_disabled_journal_says_so_on_stderr_and_the_probe_carries_it() {
        let fakes = Fakes::new(
            "probe-disabled",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        fakes.bump_journal(3);
        fakes.disable_journal(true);
        let probe = journal_probe_with(fakes.bd_bin(), 0, 10).await.unwrap();
        assert!(
            probe.is_quiet(),
            "a disabled journal says nothing: {probe:?}"
        );
        assert!(probe.disabled, "and says why, and we kept the why");
    }

    /// A probe whose `bd` exited non-zero **without** the truncation shape stays
    /// `Unusable` and keeps its exit code — a broken `bd` must not be able to
    /// dress up as a recoverable re-baseline.
    #[tokio::test]
    async fn a_refusal_that_is_not_truncation_stays_unusable() {
        let fakes = Fakes::new(
            "probe-refused",
            crate::testing::PiFake::Started,
            BdFake::Ok,
            EMPTY_BOARD,
        );
        fakes.fail_journal(true);
        let err = journal_probe_with(fakes.bd_bin(), 0, 1).await.unwrap_err();
        assert!(
            matches!(
                err,
                JournalError::Unusable(BdError::Failed { code: Some(5), .. })
            ),
            "{err:?}"
        );
    }

    /// A missing `bd` is `Unusable` carrying `Unavailable` — looprs-037's rule
    /// reaching the probe the same way it reaches every other read: no answer is
    /// never the same as an empty answer.
    #[tokio::test]
    async fn a_bd_that_cannot_be_run_is_unusable_not_quiet() {
        let err = journal_probe_with("/nonexistent/looprs/bd-not-installed", 0, 1)
            .await
            .unwrap_err();
        assert!(
            matches!(err, JournalError::Unusable(BdError::Unavailable { .. })),
            "{err:?}"
        );
    }

    /// The line parser itself, against strings — including the two shapes that
    /// must be errors rather than a short answer. The fake filters what it will
    /// emit, so this is the only place a garbage journal line can be tested.
    #[test]
    fn journal_lines_are_records_or_errors_never_a_short_read() {
        assert_eq!(journal_seqs("").unwrap(), Vec::<i64>::new());
        assert_eq!(journal_seqs("\n  \n").unwrap(), Vec::<i64>::new());
        assert_eq!(journal_seqs("{\"seq\":7}").unwrap(), vec![7]);
        assert_eq!(
            journal_seqs("{\"seq\": 8 }\n{\"seq\":9,\"op\":\"close\"}\n").unwrap(),
            vec![8, 9]
        );
        // Trailing blank lines are the end of the stream, not a bad record.
        assert_eq!(journal_seqs("{\"seq\":1}\n\n\n").unwrap(), vec![1]);
        // Not JSON at all, and JSON that is not a record: both errors.
        assert!(journal_seqs("not a record").is_err());
        assert!(journal_seqs("{\"seq\":\"seven\"}").is_err());
        assert!(
            journal_seqs("{}") // a record with no seq
                .is_err()
        );
        // And an error partway through does not come back as the records that
        // parsed before it: a partial answer is not an answer.
        assert!(journal_seqs("{\"seq\":1}\ngarbage\n{\"seq\":2}").is_err());
    }
}
