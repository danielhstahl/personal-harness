use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::process::{Command, Stdio};
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BeadStatus {
    Open,
    InProgress,
    Blocked,
    Deferred,
    Ready,
    Done,
    Closed,
}

#[derive(Deserialize)]
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
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct Bead {
    id: String,
    title: String,
    status: BeadStatus,
    issue_type: BeadIssueType,
}

impl Bead {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BdReady {
    data: Vec<Bead>,
    schema_version: usize,
}

/// `bd ready --json`, against an explicit binary (the TUI passes `bd`, tests pass a fake).
pub fn ready_beads_with(bin: &str) -> Result<Vec<Bead>> {
    let child = Command::new(bin)
        .args(["ready", "--json"])
        .env("BD_JSON_ENVELOPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    if !child.status.success() {
        return Err(anyhow!(
            "{bin} ready failed with exit status {}",
            child
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string())
        ));
    }
    let result: BdReady = serde_json::from_slice(&child.stdout)?;

    Ok(result.data)
}

#[cfg(test)]
mod tests {
    use super::*;

    /*#[test]
    fn test_real_serialization() {
        let result = get_ready_beads().unwrap();
    }*/
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
        let result: BdReady = serde_json::from_str(&raw_data).unwrap();
        assert_eq!(result.data[0].title, "hello world".to_string());
    }
}
