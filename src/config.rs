//! TinyButler home-directory configuration and initialization.
//!
//! Configuration is read from `~/.tinybutler/config.yaml`. Runtime secrets stay
//! outside the repository, while templates here provide safe local defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use tokio::fs;

/// Parsed TinyButler configuration plus the resolved home directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Resolved TinyButler home, usually `~/.tinybutler`.
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
    /// Chat id that receives TinyButler messages.
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
    /// Load config from `home_override` or from the user's default TinyButler home.
    pub fn load(home_override: Option<PathBuf>) -> Result<Self> {
        let home = home_override
            .or_else(|| dirs::home_dir().map(|p| p.join(".tinybutler")))
            .context("could not determine TinyButler home")?;
        let home = normalize_path_for_identity(&home)?;

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

    /// Runtime-owned daemon process id path used by `tinybutler restart`.
    pub fn daemon_pid_path(&self) -> PathBuf {
        self.home.join("tinybutler.pid")
    }

    /// Home-level lock used to serialize interactive chat state mutations.
    pub fn chat_lock_path(&self) -> PathBuf {
        self.home.join("chat.lock")
    }

    /// Create a safe initial TinyButler home without overwriting existing files.
    pub async fn init_home(&self) -> Result<()> {
        copy_templates_into_home(&self.home).await?;

        println!("Initialized TinyButler home at {}", self.home.display());
        Ok(())
    }
}

/// Process metadata written by the daemon and validated by `tinybutler restart`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct DaemonPidRecord {
    /// Operating-system process id for the TinyButler daemon.
    pub pid: u32,
    /// TinyButler home owned by this daemon process.
    pub home: PathBuf,
    /// Linux `/proc/<pid>/stat` start-time ticks used to reject stale pid reuse.
    pub start_time_ticks: u64,
    /// Wall-clock launch timestamp used by restart to observe a completed exec.
    pub launched_at_unix_nanos: u128,
}

impl DaemonPidRecord {
    /// Build a pid record for the current process and TinyButler home.
    pub fn current(home: &Path) -> Result<Self> {
        let pid = std::process::id();
        Ok(Self {
            pid,
            home: normalize_path_for_identity(home)?,
            start_time_ticks: linux_process_start_time_ticks(pid)?,
            launched_at_unix_nanos: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .context("system clock is before the Unix epoch")?
                .as_nanos(),
        })
    }

    /// Validate that this pid still identifies the daemon for `expected_home`.
    pub fn validate_for_home(&self, expected_home: &Path) -> Result<()> {
        let expected_home = normalize_path_for_identity(expected_home)?;
        if self.home != expected_home {
            bail!(
                "pid file belongs to TinyButler home {}, not {}",
                self.home.display(),
                expected_home.display()
            );
        }

        let current_start_time = linux_process_start_time_ticks(self.pid)
            .with_context(|| format!("failed to inspect daemon process {}", self.pid))?;
        if current_start_time != self.start_time_ticks {
            bail!(
                "pid {} has been reused since TinyButler wrote the pid file",
                self.pid
            );
        }

        let cmdline = std::fs::read(format!("/proc/{}/cmdline", self.pid))
            .with_context(|| format!("failed to inspect daemon process {}", self.pid))?;
        let args = cmdline
            .split(|byte| *byte == 0)
            .filter(|part| !part.is_empty())
            .map(|part| String::from_utf8_lossy(part).to_string())
            .collect::<Vec<_>>();
        let executable_matches = args
            .first()
            .map(|arg| arg.ends_with("tinybutler"))
            .unwrap_or(false);
        let daemon_arg_matches = args.iter().any(|arg| arg == "daemon");
        if !executable_matches || !daemon_arg_matches {
            bail!(
                "pid {} does not look like a TinyButler daemon: {}",
                self.pid,
                args.join(" ")
            );
        }

        Ok(())
    }

    /// Whether this record was rewritten by a restart after `previous`.
    pub fn is_restart_of(&self, previous: &Self) -> bool {
        self.home == previous.home
            && (self.pid != previous.pid
                || self.start_time_ticks != previous.start_time_ticks
                || self.launched_at_unix_nanos != previous.launched_at_unix_nanos)
    }
}

/// Read daemon pid metadata from disk.
pub fn read_daemon_pid_record(path: &Path) -> Result<DaemonPidRecord> {
    let text = std::fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read {}; is tinybutler daemon running?",
            path.display()
        )
    })?;
    serde_yaml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
}

/// Write daemon pid metadata to disk and return the record.
pub async fn write_daemon_pid_record(path: &Path, home: &Path) -> Result<DaemonPidRecord> {
    let record = DaemonPidRecord::current(home)?;
    let text = serde_yaml::to_string(&record).context("failed to serialize daemon pid record")?;
    fs::write(path, text)
        .await
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(record)
}

/// Return the Linux process start time field used to detect pid reuse.
pub fn linux_process_start_time_ticks(pid: u32) -> Result<u64> {
    let stat_path = format!("/proc/{pid}/stat");
    let stat = std::fs::read_to_string(&stat_path)
        .with_context(|| format!("failed to read {stat_path}"))?;
    let suffix = stat
        .rfind(") ")
        .map(|index| &stat[index + 2..])
        .context("failed to parse process stat command field")?;
    suffix
        .split_whitespace()
        .nth(19)
        .context("failed to parse process stat start time")?
        .parse::<u64>()
        .context("failed to parse process stat start time")
}

fn normalize_path_for_identity(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()
            .context("failed to read current directory")?
            .join(path))
    }
}

/// Copy repository templates into a TinyButler home without overwriting user files.
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
            let target_path = target_dir.join(entry.file_name());
            if file_type.is_dir() {
                if relative_dir == Path::new(".agents/skills") {
                    remove_existing_template_skill(&target_path).await?;
                }
                pending_dirs.push(entry.path());
            } else if file_type.is_file() {
                copy_template_file_if_missing(&entry.path(), &target_path).await?;
            }
        }
    }

    Ok(())
}

/// Remove one installed template-owned skill before copying a fresh version.
async fn remove_existing_template_skill(target: &Path) -> Result<()> {
    match fs::remove_dir_all(target).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to remove {}", target.display())),
    }
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
