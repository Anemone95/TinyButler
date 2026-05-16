//! TickClaw home-directory configuration and initialization.
//!
//! Configuration is read from `~/.tickclaw/config.yaml`. Runtime secrets stay
//! outside the repository, while templates here provide safe local defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

/// Parsed TickClaw configuration plus the resolved home directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Resolved TickClaw home, usually `~/.tickclaw`.
    #[serde(skip)]
    pub home: PathBuf,

    /// Telegram outbound notification settings.
    #[serde(default)]
    pub telegram: TelegramConfig,

    /// Local agent CLI command templates keyed by runner name.
    #[serde(default)]
    pub code_agents: BTreeMap<String, CodeAgentConfig>,
}

/// Telegram Bot API credentials read from local config only.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    /// Bot token used in Telegram Bot API URLs.
    #[serde(default)]
    pub bot_token: Option<String>,
    /// Chat id that receives TickClaw messages.
    #[serde(default)]
    pub chat_id: Option<String>,
}

/// Configurable command template for future agent runner backends.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeAgentConfig {
    /// Absolute or PATH-resolved executable name.
    pub command: String,
    /// Arguments used for a fresh agent session.
    #[serde(default)]
    pub args: Vec<String>,
    /// Arguments used when `session: reuse` has a stored session id.
    #[serde(default)]
    pub resume_args: Vec<String>,
}

impl Config {
    /// Load config from `home_override` or from the user's default TickClaw home.
    pub fn load(home_override: Option<PathBuf>) -> Result<Self> {
        let home = home_override
            .or_else(|| dirs::home_dir().map(|p| p.join(".tickclaw")))
            .context("could not determine TickClaw home")?;

        let config_path = home.join("config.yaml");
        if !config_path.exists() {
            return Ok(Self {
                home,
                telegram: TelegramConfig::default(),
                code_agents: BTreeMap::new(),
            });
        }

        let text = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        config.home = home;
        Ok(config)
    }

    /// Directory containing task subdirectories.
    pub fn tasks_dir(&self) -> PathBuf {
        self.home.join("tasks")
    }

    /// Local config file path.
    pub fn config_path(&self) -> PathBuf {
        self.home.join("config.yaml")
    }

    /// Daemon-owned Telegram polling offset state path.
    pub fn telegram_state_path(&self) -> PathBuf {
        self.home.join("telegram_state.json")
    }

    /// Create a safe initial TickClaw home without overwriting existing files.
    pub async fn init_home(&self) -> Result<()> {
        fs::create_dir_all(self.tasks_dir()).await?;

        write_if_missing(
            &self.config_path(),
            r#"telegram:
  bot_token: null
  chat_id: null

code_agents:
  gemini-3.1-flash-lite:
    command: /usr/bin/gemini
    args:
      - "--model"
      - gemini-3.1-flash-lite
      - "--skip-trust"
      - "--approval-mode"
      - yolo
      - "--output-format"
      - json
      - "--prompt"
      - "{prompt}"
    resume_args:
      - "--model"
      - gemini-3.1-flash-lite
      - "--skip-trust"
      - "--approval-mode"
      - yolo
      - "--resume"
      - "{sessionId}"
      - "--output-format"
      - json
      - "--prompt"
      - "{prompt}"

  gpt-5.3-codex-spark:
    command: /usr/bin/codex
    args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - danger-full-access
      - "-m"
      - gpt-5.3-codex-spark
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
    resume_args:
      - exec
      - resume
      - "{sessionId}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "-m"
      - gpt-5.3-codex-spark
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
  gpt-5.5:
    command: /usr/bin/codex
    args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - danger-full-access
      - "-m"
      - gpt-5.5
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
    resume_args:
      - exec
      - resume
      - "{sessionId}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "-m"
      - gpt-5.5
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
"#,
        )
        .await?;

        let task_dir = self.tasks_dir().join("smoke-task");
        fs::create_dir_all(task_dir.join("logs")).await?;
        fs::create_dir_all(task_dir.join("data")).await?;
        write_if_missing(
            &task_dir.join("task.yaml"),
            r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
runner: gpt-5.3-codex-spark
type: agent
session: independent
timeout: 3600
"#,
        )
        .await?;
        write_if_missing(
            &task_dir.join("agent.md"),
            "Inspect this task directory and summarize whether the task setup is healthy.\n",
        )
        .await?;

        let shell_dir = self.tasks_dir().join("regular-check");
        fs::create_dir_all(shell_dir.join("logs")).await?;
        fs::create_dir_all(shell_dir.join("data")).await?;
        write_if_missing(
            &shell_dir.join("task.yaml"),
            r#"name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 300
"#,
        )
        .await?;
        write_if_missing(
            &shell_dir.join("run.sh"),
            "#!/usr/bin/env bash\nset -euo pipefail\n\nprintf 'regular-check ok: %s\\n' \"$(date --iso-8601=seconds)\"\n",
        )
        .await?;

        println!("Initialized TickClaw home at {}", self.home.display());
        Ok(())
    }
}

/// Write a template file only when the user has not already created one.
async fn write_if_missing(path: &Path, contents: &str) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }
    fs::write(path, contents).await?;
    Ok(())
}
