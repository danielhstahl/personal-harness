use anyhow::{Result, anyhow};
use serde::Deserialize;
use std::process::Command;
use std::{
    collections::HashMap,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Child,
    sync::{mpsc, oneshot},
};
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

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct BdReady {
    data: Vec<Bead>,
    schema_version: usize,
}

pub fn get_ready_beads() -> Result<Vec<Bead>> {
    let child = Command::new("bd")
        .args(["ready", "--json"])
        .env("BD_JSON_ENVELOPE", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
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
