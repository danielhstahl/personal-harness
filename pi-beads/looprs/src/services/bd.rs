//! The `bd` (beads) service: an async, honest-error wrapper over the `bd` CLI.
//!
//! Three rules, and the whole ticket (looprs-037) is them:
//!
//! 1. **Never block the runtime.** Every call is `tokio::process`, awaited. The old
//!    `std::process::Command::output()` inside the beads loop could park a tokio
//!    worker for hundreds of ms on a Dolt-backed board, which shows up as the
//!    60 fps tick stuttering in `main.rs`.
//! 2. **Never conflate "bd is broken" with "the board is empty".** Both are
//!   `Result`s: an empty board is `Ok(vec![])`, a failing `bd` is
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
use tokio::process::Command;
use std::process::Stdio;

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
                bin, args, reason, raw
            } => write!(
                f,
                "`{bin} {args}` returned unreadable JSON: {reason} (got {:?})",
                truncate(raw, 200)
            ),
            BdError::Timeout { bin, args } => write!(
                f,
                "`{bin} {args}` did not answer within {:?}",
                BD_TIMEOUT
            ),
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

    /// "A worker could be pointed at this." Mirrors what `bd ready` promises, used
    /// for the re-work guard in looprs-w7q.
    pub fn is_workable(self) -> bool {
        matches!(self, Self::Open | Self::InProgress | Self::Ready)
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

/// Run `bd` with args, bounded, with stdout and stderr both captured.
///
/// The single place that touches `Stdio`, so "stderr is never discarded" is a
/// property of one function rather than a habit. stdout is returned only on exit
/// code 0; anything else is a [`BdError`] that carries stderr with it.
async fn run(bin: &str, args: &[&str]) -> Result<Vec<u8>, BdError> {
    let args_joined = args.join(" ");
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
                args: args_joined,
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

    if !out.status.success() {
        return Err(BdError::Failed {
            bin: bin.to_string(),
            args: args_joined,
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
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

/// `bd ready --json` — the beads the loop may pick up, in bd's own priority order.
///
/// `Ok(vec![])` means the board is genuinely empty. It does *not* mean "bd could
/// not tell us": that is `Err`, and the caller must not render it as empty.
pub async fn ready_with(bin: &str) -> Result<Vec<Bead>, BdError> {
    beads(bin, &["ready", "--json"]).await
}

/// `bd list --status <status> --json` — every bead in one lifecycle state.
///
/// The planner diff (looprs-k7v) needs a board snapshot that does not depend on
/// priority/dependency filtering the way `ready` does.
pub async fn list_status_with(bin: &str, status: &str) -> Result<Vec<Bead>, BdError> {
    beads(bin, &["list", "--status", status, "--json"]).await
}

/// `bd show <id> --json` — one bead by id, `Ok(None)` if bd knows nothing of it.
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

/// Is this bead closed? A read of truth for the post-settle check in looprs-w7q:
/// "the worker settled" and "the bead is closed" are different claims, and only
/// `bd` can answer the second one.
pub async fn is_closed_with(bin: &str, id: &str) -> Result<bool, BdError> {
    Ok(show_with(bin, id).await?.is_some_and(|b| b.is_closed()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{BdFake, EMPTY_BOARD, Fakes, ONE_BEADED_BOARD};


    /// The three outcomes looprs-037 refuses to conflate, as three distinct tests.
    #[tokio::test]
    async fn an_empty_board_is_ok_with_nothing_in_it() {
        let fakes = Fakes::new("bd-empty", crate::testing::PiFake::Started, BdFake::Ok, EMPTY_BOARD);
        let beads = ready_with(fakes.bd_bin()).await.expect("empty is not an error");
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

    /// Both shapes `bd` actually speaks parse: the envelope and the bare array.
    #[test]
    fn both_json_shapes_are_accepted() {
        let enveloped = r#"{"data":[{"id":"a-1","title":"T","status":"open","issue_type":"task"}],"schema_version":1}"#;
        let bare = r#"[{"id":"a-1","title":"T","status":"open","issue_type":"task"}]"#;
        for src in [enveloped, bare] {
            let got: Vec<Bead> = serde_json::from_str::<BdList>(src)
                .expect(src)
                .into_vec();
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
        let fakes = Fakes::new("bd-claim", crate::testing::PiFake::Started, BdFake::Ok, EMPTY_BOARD);
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

    /// `is_closed` reads the board rather than trusting the worker's own report.
    #[tokio::test]
    async fn is_closed_reads_the_truth_from_bd() {
        let fakes = Fakes::new(
            "bd-show",
            crate::testing::PiFake::Started,
            BdFake::ShowStatus,
            EMPTY_BOARD,
        );
        fakes.set_show(r#"{"id":"looprs-9","title":"t","status":"closed","issue_type":"task"}"#);
        assert!(is_closed_with(fakes.bd_bin(), "looprs-9").await.unwrap());
        fakes.set_show(r#"{"id":"looprs-9","title":"t","status":"open","issue_type":"task"}"#);
        assert!(
            !is_closed_with(fakes.bd_bin(), "looprs-9").await.unwrap(),
            "an un-closed bead must read as un-closed"
        );
        // A bead bd does not know about is not "closed": the harness must not
        // conclude the worker finished just because the lookup came back empty.
        assert!(
            !is_closed_with(fakes.bd_bin(), "looprs-404").await.unwrap(),
            "an unknown bead is not closed"
        );
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
}
