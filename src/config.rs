//! TinyButler home-directory configuration and initialization.
//!
//! Configuration is read from `~/.tinybutler/config.yaml`. Runtime secrets stay
//! outside the repository, while templates here provide safe local defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
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

/// Configurable command templates for one code-agent CLI backend.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeAgentConfig {
    /// Absolute or PATH-resolved executable name.
    pub command: String,
    /// Arguments used for a fresh scheduled agent session.
    #[serde(default, alias = "args")]
    pub new_args: Vec<String>,
    /// Arguments used when `session: reuse` has a stored session id.
    #[serde(default)]
    pub resume_args: Vec<String>,
    /// Arguments used for long-lived interactive chat bridge sessions.
    #[serde(default)]
    pub stream_args: Vec<String>,
    /// Model names supported by this backend configuration.
    #[serde(default)]
    pub models: Vec<String>,
}

/// Resolved code-agent backend for a requested `group/model` reference.
#[derive(Debug, Clone)]
pub struct ResolvedCodeAgent<'a> {
    /// Concrete model name passed to the backend CLI.
    pub model: String,
    /// Backend command template that supports `model`.
    pub config: &'a CodeAgentConfig,
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

    /// Resolve a `group/model` reference to its configured code-agent backend.
    pub fn code_agent_for_model(&self, model_reference: &str) -> Option<ResolvedCodeAgent<'_>> {
        let (group, model_name) = model_reference.split_once('/')?;
        let config = self
            .code_agents
            .get(group)
            .filter(|config| config.models.iter().any(|model| model == model_name))?;
        Some(ResolvedCodeAgent {
            model: model_name.to_string(),
            config,
        })
    }

    /// Return all configured model references that have interactive stream args.
    pub fn streaming_model_names(&self) -> Vec<String> {
        let mut models = Vec::new();
        for (group, config) in &self.code_agents {
            if config.stream_args.is_empty() {
                continue;
            }
            models.extend(config.model_references(group));
        }
        models
    }

    /// Validate local code-agent command templates.
    pub fn validate_code_agents(&self) -> Result<()> {
        for (group, config) in &self.code_agents {
            config.validate_templates(group)?;
        }
        Ok(())
    }

    /// Create a safe initial TinyButler home without overwriting existing files.
    pub async fn init_home(&self) -> Result<()> {
        copy_templates_into_home(&self.home).await?;

        println!("Initialized TinyButler home at {}", self.home.display());
        Ok(())
    }
}

impl CodeAgentConfig {
    /// Return chat-facing model references, qualifying grouped models.
    pub fn model_references(&self, group: &str) -> Vec<String> {
        self.models
            .iter()
            .map(|model| format!("{group}/{model}"))
            .collect()
    }

    /// True when `model` matches one chat-facing reference from this group.
    pub fn has_model_reference(&self, group: &str, model: &str) -> bool {
        model
            .split_once('/')
            .is_some_and(|(model_group, model_name)| {
                model_group == group && self.models.iter().any(|model| model == model_name)
            })
    }

    /// Validate argument templates that run scheduled prompts.
    pub fn validate_templates(&self, group: &str) -> Result<()> {
        if self.models.is_empty() {
            bail!("code_agents.{group}.models must list at least one model");
        }
        validate_optional_model_args(group, "new_args", &self.new_args)?;
        validate_optional_model_args(group, "resume_args", &self.resume_args)?;
        validate_optional_model_args(group, "stream_args", &self.stream_args)?;
        validate_optional_prompt_args(group, "new_args", &self.new_args)?;
        validate_optional_prompt_args(group, "resume_args", &self.resume_args)?;
        validate_optional_prompt_args(group, "stream_args", &self.stream_args)?;
        validate_resume_args(group, &self.resume_args)?;
        Ok(())
    }
}

/// How a scheduled agent command receives its prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptMode {
    /// The prompt is expanded into an argument containing `{prompt}`.
    Argument,
    /// The prompt is written to stdin and the `{stdin}` marker is not passed.
    Stdin,
}

/// Infer and validate prompt delivery for one argument template.
pub(crate) fn prompt_mode_for_args(args: &[String]) -> Result<PromptMode> {
    let has_prompt_arg = args.iter().any(|arg| arg.contains("{prompt}"));
    let stdin_marker_count = args.iter().filter(|arg| arg.as_str() == "{stdin}").count();
    match (has_prompt_arg, stdin_marker_count) {
        (true, 0) => Ok(PromptMode::Argument),
        (false, 1) => Ok(PromptMode::Stdin),
        (true, _) => bail!("agent args must use either {{prompt}} or {{stdin}}, not both"),
        (false, 0) => bail!("agent args must contain {{prompt}} or {{stdin}}"),
        (false, _) => bail!("agent args must contain only one {{stdin}} marker"),
    }
}

fn validate_optional_prompt_args(group: &str, field: &str, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    prompt_mode_for_args(args).with_context(|| format!("invalid code_agents.{group}.{field}"))?;
    Ok(())
}

fn validate_optional_model_args(group: &str, field: &str, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    if !args.iter().any(|arg| arg.contains("{model}")) {
        bail!("code_agents.{group}.{field} must contain {{model}}");
    }
    Ok(())
}

fn validate_resume_args(group: &str, args: &[String]) -> Result<()> {
    if args.is_empty() {
        return Ok(());
    }
    if !args.iter().any(|arg| arg.contains("{sessionId}")) {
        bail!("code_agents.{group}.resume_args must contain {{sessionId}}");
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_agent(group: &str, models: &[&str]) -> Config {
        let mut code_agents = BTreeMap::new();
        code_agents.insert(
            group.to_string(),
            CodeAgentConfig {
                command: "/bin/true".to_string(),
                models: models.iter().map(|model| model.to_string()).collect(),
                ..CodeAgentConfig::default()
            },
        );
        Config {
            home: PathBuf::from("/tmp/tinybutler-config-test"),
            telegram: TelegramConfig::default(),
            code_agents,
        }
    }

    #[test]
    fn rejects_unqualified_model_names() {
        let config = config_with_agent("codex", &["gpt-5.5"]);

        assert!(config.code_agent_for_model("gpt-5.5").is_none());
    }

    #[test]
    fn resolves_qualified_chat_model_references() {
        let config = config_with_agent("codex", &["gpt-5.5"]);
        let agent = config
            .code_agent_for_model("codex/gpt-5.5")
            .expect("qualified model should resolve");

        assert_eq!(agent.model, "gpt-5.5");
        assert_eq!(agent.config.command, "/bin/true");
    }

    #[test]
    fn rejects_unknown_groups_in_qualified_references() {
        let config = config_with_agent("codex", &["gpt-5.5"]);

        assert!(config.code_agent_for_model("openai/gpt-4.1").is_none());
    }

    #[test]
    fn grouped_agent_key_is_not_a_model_name() {
        let config = config_with_agent("codex", &["gpt-5.5"]);

        assert!(config.code_agent_for_model("codex").is_none());
    }

    #[test]
    fn allows_duplicate_model_names_in_different_groups() {
        let mut config = config_with_agent("codex", &["gpt-5.5"]);
        config.code_agents.insert(
            "backup-codex".to_string(),
            CodeAgentConfig {
                command: "/bin/true".to_string(),
                models: vec!["gpt-5.5".to_string()],
                ..CodeAgentConfig::default()
            },
        );

        config
            .validate_code_agents()
            .expect("group-qualified references are distinct");
    }

    #[test]
    fn rejects_agent_groups_without_models() {
        let config = config_with_agent("gpt-5.5", &[]);
        let err = config
            .validate_code_agents()
            .expect_err("empty models should be rejected");

        assert!(
            err.to_string()
                .contains("models must list at least one model")
        );
    }

    #[test]
    fn rejects_runnable_args_without_model_placeholder() {
        let mut config = config_with_agent("codex", &["gpt-5.5"]);
        config
            .code_agents
            .get_mut("codex")
            .expect("codex config")
            .new_args = vec!["exec".to_string(), "{prompt}".to_string()];

        let err = config
            .validate_code_agents()
            .expect_err("missing model placeholder should be rejected");

        assert!(err.to_string().contains("new_args must contain {model}"));
    }

    #[test]
    fn rejects_resume_args_without_model_placeholder() {
        let mut config = config_with_agent("codex", &["gpt-5.5"]);
        config
            .code_agents
            .get_mut("codex")
            .expect("codex config")
            .resume_args = vec![
            "exec".to_string(),
            "resume".to_string(),
            "{sessionId}".to_string(),
            "{prompt}".to_string(),
        ];

        let err = config
            .validate_code_agents()
            .expect_err("missing model placeholder should be rejected");

        assert!(err.to_string().contains("resume_args must contain {model}"));
    }

    #[test]
    fn rejects_resume_args_without_session_placeholder() {
        let mut config = config_with_agent("codex", &["gpt-5.5"]);
        config
            .code_agents
            .get_mut("codex")
            .expect("codex config")
            .resume_args = vec![
            "exec".to_string(),
            "resume".to_string(),
            "{model}".to_string(),
            "{prompt}".to_string(),
        ];

        let err = config
            .validate_code_agents()
            .expect_err("missing session placeholder should be rejected");

        assert!(
            err.to_string()
                .contains("resume_args must contain {sessionId}")
        );
    }

    #[test]
    fn rejects_stream_args_without_prompt_or_stdin_marker() {
        let mut config = config_with_agent("codex", &["gpt-5.5"]);
        config
            .code_agents
            .get_mut("codex")
            .expect("codex config")
            .stream_args = vec!["app-server".to_string(), "model={model}".to_string()];

        let err = config
            .validate_code_agents()
            .expect_err("missing prompt delivery marker should be rejected");

        assert!(format!("{err:#}").contains("must contain {prompt} or {stdin}"));
    }

    #[test]
    fn accepts_stdin_prompt_marker() {
        let args = vec!["exec".to_string(), "{stdin}".to_string()];

        assert_eq!(prompt_mode_for_args(&args).unwrap(), PromptMode::Stdin);
    }
}
