//! Task definition loading and validation for `tasks/*/task.yaml`.
//!
//! This module owns the user-editable task schema. It intentionally rejects
//! deprecated fields such as `workspace`, `notify`, `concurrency`, and
//! runner-specific argument blocks so the daemon has one fixed execution model
//! to review.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::fs;

use crate::cron_expr::parse_cron;

/// Runtime-ready task configuration loaded from one task directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    /// Stable task name used by CLI commands and status output.
    pub name: String,
    /// Whether scheduled daemon ticks should run this task.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Local-time cron expression. Five-field expressions get seconds added.
    pub schedule: String,
    /// Ordered fallback agent backend keys for `type: agent` tasks.
    ///
    /// TinyButler tries these in order and reports task failure only after the
    /// final configured agent fails.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Task file shape, either `agent.md` or `run.sh`.
    #[serde(rename = "type")]
    pub task_type: TaskType,
    /// Agent session policy; ignored by shell command tasks.
    #[serde(default)]
    pub session: SessionMode,
    /// Per-run timeout in seconds.
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// Directory containing this task's `task.yaml`; set after deserialization.
    #[serde(skip)]
    pub dir: PathBuf,
}

/// Task file type named in `task.yaml`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskType {
    Command,
    Agent,
}

/// Session reuse behavior for agent task runners that support resume.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    #[default]
    Independent,
    Reuse,
}

fn default_enabled() -> bool {
    true
}

fn default_timeout() -> u64 {
    3600
}

impl Task {
    /// Load and validate a task from a concrete `task.yaml` path.
    pub async fn load(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let mut task: Task = serde_yaml::from_str(&text)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        task.dir = path
            .parent()
            .context("task.yaml has no parent directory")?
            .to_path_buf();
        task.validate()?;
        Ok(task)
    }

    /// Validate cross-field task constraints that serde cannot express.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("task name cannot be empty");
        }
        parse_cron(&self.schedule)
            .with_context(|| format!("invalid schedule for task {}", self.name))?;
        match self.task_type {
            TaskType::Agent => {
                let runners = self.agent_runner_keys();
                if runners.is_empty() {
                    bail!("agent task {} requires agents", self.name);
                }
                for runner in runners {
                    if runner.trim().is_empty() || runner == "shell" {
                        bail!("agent task {} requires code-agent runners", self.name);
                    }
                }
            }
            TaskType::Command => {
                if !self.agents.is_empty() {
                    bail!("command task {} must not define agents", self.name);
                }
            }
        }
        Ok(())
    }

    /// Return the effective agent runner label for display and execution.
    pub fn agents_label(&self) -> String {
        match self.task_type {
            TaskType::Command => "shell".to_string(),
            TaskType::Agent => {
                let runners = self.agent_runner_keys();
                if runners.is_empty() {
                    "-".to_string()
                } else {
                    runners.join(" -> ")
                }
            }
        }
    }

    /// Return the ordered code-agent model names for an agent task.
    pub fn agent_runner_keys(&self) -> Vec<&str> {
        self.agents.iter().map(String::as_str).collect()
    }

    /// Return the daemon-owned state file for this task.
    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    /// Return the task-owned YAML definition file.
    pub fn task_yaml_path(&self) -> PathBuf {
        self.dir.join("task.yaml")
    }

    /// Return the daemon-owned logs directory for this task.
    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    /// Return the agent prompt path for `type: agent`.
    pub fn agent_path(&self) -> PathBuf {
        self.dir.join("agent.md")
    }

    /// Return the shell script path for `type: command`.
    pub fn run_script_path(&self) -> PathBuf {
        self.dir.join("run.sh")
    }
}
