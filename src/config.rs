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
    /// Arguments used for long-lived interactive chat bridge sessions.
    #[serde(default)]
    pub stream_args: Vec<String>,
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

    /// Runtime-owned interactive chat bridge state path.
    pub fn chat_state_path(&self) -> PathBuf {
        self.home.join("chat_state.json")
    }

    /// Home-level lock used to serialize interactive chat state mutations.
    pub fn chat_lock_path(&self) -> PathBuf {
        self.home.join("chat.lock")
    }

    /// Create a safe initial TickClaw home without overwriting existing files.
    pub async fn init_home(&self) -> Result<()> {
        copy_templates_into_home(&self.home).await?;

        println!("Initialized TickClaw home at {}", self.home.display());
        Ok(())
    }
}

/// Copy repository templates into a TickClaw home without overwriting user files.
async fn copy_templates_into_home(home: &Path) -> Result<()> {
    let template_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    let mut pending_dirs = vec![template_root.clone()];

    while let Some(source_dir) = pending_dirs.pop() {
        let relative_dir = source_dir
            .strip_prefix(&template_root)
            .with_context(|| format!("failed to relativize {}", source_dir.display()))?;
        let target_dir = home.join(relative_dir);
        fs::create_dir_all(&target_dir)
            .await
            .with_context(|| format!("failed to create {}", target_dir.display()))?;

        let mut entries = fs::read_dir(&source_dir).await.with_context(|| {
            format!("failed to read template directory {}", source_dir.display())
        })?;
        while let Some(entry) = entries.next_entry().await? {
            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                pending_dirs.push(entry.path());
            } else if file_type.is_file() {
                copy_template_file_if_missing(&entry.path(), &target_dir.join(entry.file_name()))
                    .await?;
            }
        }
    }

    Ok(())
}

/// Copy one template file only when the user has not already created it.
async fn copy_template_file_if_missing(source: &Path, target: &Path) -> Result<()> {
    if target.exists() {
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let permissions = fs::metadata(source)
        .await
        .with_context(|| format!("failed to stat template {}", source.display()))?
        .permissions();
    fs::copy(source, target).await.with_context(|| {
        format!(
            "failed to copy template {} to {}",
            source.display(),
            target.display()
        )
    })?;
    fs::set_permissions(target, permissions)
        .await
        .with_context(|| format!("failed to set permissions on {}", target.display()))?;
    Ok(())
}
