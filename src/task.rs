use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use tokio::fs;

#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    pub name: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub schedule: String,
    pub runner: RunnerKind,
    #[serde(rename = "type")]
    pub task_type: TaskType,
    #[serde(default)]
    pub session: SessionMode,
    pub workspace: Option<PathBuf>,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    pub notify: Option<String>,
    #[serde(default)]
    pub concurrency: ConcurrencyPolicy,
    #[serde(default)]
    pub codex: CodexConfig,

    #[serde(skip)]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunnerKind {
    Shell,
    Codex,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TaskType {
    Command,
    Agent,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum SessionMode {
    #[default]
    Independent,
    Reuse,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ConcurrencyPolicy {
    #[default]
    Skip,
    Queue,
    Parallel,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodexConfig {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_sandbox")]
    pub sandbox: String,
    #[serde(default)]
    pub service_tier: Option<String>,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            model: None,
            sandbox: default_sandbox(),
            service_tier: None,
        }
    }
}

fn default_enabled() -> bool {
    true
}

fn default_timeout() -> u64 {
    3600
}

fn default_sandbox() -> String {
    "workspace-write".to_string()
}

impl Task {
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

    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            bail!("task name cannot be empty");
        }
        if self.task_type == TaskType::Agent && self.workspace.is_none() {
            bail!("agent task {} requires workspace", self.name);
        }
        if self.task_type == TaskType::Agent && self.runner == RunnerKind::Shell {
            bail!("agent task {} cannot use shell runner", self.name);
        }
        if self.task_type == TaskType::Command && self.runner != RunnerKind::Shell {
            bail!("command task {} must use shell runner", self.name);
        }
        Ok(())
    }

    pub fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub fn agent_path(&self) -> PathBuf {
        self.dir.join("agent.md")
    }

    pub fn run_script_path(&self) -> PathBuf {
        self.dir.join("run.sh")
    }
}
