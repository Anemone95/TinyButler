use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(skip)]
    pub home: PathBuf,

    #[serde(default)]
    pub telegram: TelegramConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: Option<String>,
    #[serde(default)]
    pub chat_id: Option<String>,
    #[serde(default = "default_parse_mode")]
    pub parse_mode: Option<String>,
}

impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            bot_token: None,
            chat_id: None,
            parse_mode: default_parse_mode(),
        }
    }
}

fn default_parse_mode() -> Option<String> {
    Some("Markdown".to_string())
}

impl Config {
    pub fn load(home_override: Option<PathBuf>) -> Result<Self> {
        let home = home_override
            .or_else(|| dirs::home_dir().map(|p| p.join(".tickclaw")))
            .context("could not determine TickClaw home")?;

        let config_path = home.join("config.yaml");
        if !config_path.exists() {
            return Ok(Self {
                home,
                telegram: TelegramConfig::default(),
            });
        }

        let text = std::fs::read_to_string(&config_path)
            .with_context(|| format!("failed to read {}", config_path.display()))?;
        let mut config: Config = serde_yaml::from_str(&text)
            .with_context(|| format!("failed to parse {}", config_path.display()))?;
        config.home = home;
        Ok(config)
    }

    pub fn tasks_dir(&self) -> PathBuf {
        self.home.join("tasks")
    }

    pub fn config_path(&self) -> PathBuf {
        self.home.join("config.yaml")
    }

    pub async fn init_home(&self) -> Result<()> {
        fs::create_dir_all(self.tasks_dir()).await?;

        write_if_missing(
            &self.config_path(),
            r#"telegram:
  bot_token: null
  chat_id: null
  parse_mode: Markdown
"#,
        )
        .await?;

        let task_dir = self.tasks_dir().join("example-agent");
        fs::create_dir_all(task_dir.join("logs")).await?;
        write_if_missing(
            &task_dir.join("task.yaml"),
            r#"name: example-agent
enabled: false
schedule: "0 9 * * *"
runner: codex
type: agent
session: independent
workspace: /home/wenyuan
timeout: 3600
notify: telegram
concurrency: skip

codex:
  model: gpt-5.5
  sandbox: workspace-write
"#,
        )
        .await?;
        write_if_missing(
            &task_dir.join("agent.md"),
            "Summarize the current workspace status.\n",
        )
        .await?;

        let shell_dir = self.tasks_dir().join("example-shell");
        fs::create_dir_all(shell_dir.join("logs")).await?;
        write_if_missing(
            &shell_dir.join("task.yaml"),
            r#"name: example-shell
enabled: false
schedule: "*/30 * * * *"
runner: shell
type: command
timeout: 300
notify: telegram
concurrency: skip
"#,
        )
        .await?;
        write_if_missing(
            &shell_dir.join("run.sh"),
            "#!/usr/bin/env bash\nset -euo pipefail\n\ndate\npwd\n",
        )
        .await?;

        println!("Initialized TickClaw home at {}", self.home.display());
        Ok(())
    }
}

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
