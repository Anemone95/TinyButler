//! Persistent daemon-owned task state.
//!
//! Each task keeps its own `state.json` beside `task.yaml`, so runtime state is
//! inspectable without a central database.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::fs;

/// Latest persisted execution state for one task.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskState {
    /// RFC3339 timestamp of the latest execution attempt.
    #[serde(default)]
    pub last_run_at: Option<String>,
    /// Latest status string, such as `success`, `failed`, or `Run Failure`.
    #[serde(default)]
    pub last_status: Option<String>,
    /// Latest process exit code when a process actually ran.
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    /// Latest log path relative to the task directory.
    #[serde(default)]
    pub last_log: Option<String>,
    /// Stored agent session id for `session: reuse`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Normalized cron expression used to compute `next_run_at`.
    #[serde(default)]
    pub schedule_expr: Option<String>,
    /// RFC3339 timestamp of the next scheduled run.
    #[serde(default)]
    pub next_run_at: Option<String>,
    /// Whether TinyButler believes this task is currently running.
    #[serde(default)]
    pub running: bool,
    /// Count of all execution attempts recorded in state.
    #[serde(default)]
    pub run_count: u64,
    /// Count of non-success execution attempts.
    #[serde(default)]
    pub failure_count: u64,
}

impl TaskState {
    /// Load state from disk, returning default state when the file is absent.
    pub async fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Persist state as pretty JSON with a trailing newline.
    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, format!("{text}\n")).await?;
        Ok(())
    }
}
