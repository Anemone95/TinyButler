//! Shared interactive chat state, adapter abstractions, and adapter factory.
//!
//! The chat bridge is intentionally independent from Telegram. Local REPL
//! commands and Telegram ingress should both drive the same `ChatAgent`
//! interface so state, abort, and resume behavior remain consistent.

mod agy;
mod codex;

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local};
use serde::{Deserialize, Serialize};
use tokio::fs as tokio_fs;

use crate::config::{CodeAgentConfig, Config, ResolvedCodeAgent};

/// A resumable interactive code-agent session known to TinyButler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatSession {
    /// Configured model name selected from `code_agents.<group>.models`.
    pub runner: String,
    /// Stable session/thread id returned by the adapter.
    pub session_id: String,
    /// Optional human-readable title or latest user prompt summary.
    pub title: Option<String>,
    /// Last time TinyButler observed activity in this session.
    pub last_activity_at: DateTime<Local>,
}

/// Persisted high-level chat bridge state.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChatStateValue {
    #[default]
    Inactive,
    SelectingNew,
    SelectingSession,
    ActiveIdle,
    ActiveBusy,
    Aborting,
}

/// Runtime-owned chat bridge state stored under `~/.tinybutler/chat_state.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ChatRuntimeState {
    /// Current bridge state-machine value.
    pub state: ChatStateValue,
    /// Runner key for the active session, when any.
    pub active_runner: Option<String>,
    /// Stable adapter session/thread id for the active session, when any.
    pub active_session_id: Option<String>,
    /// Resumable sessions known to TinyButler.
    pub sessions: Vec<ChatSession>,
    /// Current Telegram or local turn id, when a turn is busy.
    pub current_request_id: Option<String>,
    /// PID of the TinyButler process that started the busy turn.
    pub current_process_id: Option<u32>,
    /// Busy-state timestamp, used for stale recovery.
    pub busy_since: Option<DateTime<Local>>,
    /// Latest chat bridge error, if any.
    pub last_error: Option<String>,
    /// Latest state update timestamp.
    pub updated_at: Option<DateTime<Local>>,
}

impl Default for ChatRuntimeState {
    fn default() -> Self {
        Self {
            state: ChatStateValue::Inactive,
            active_runner: None,
            active_session_id: None,
            sessions: Vec::new(),
            current_request_id: None,
            current_process_id: None,
            busy_since: None,
            last_error: None,
            updated_at: None,
        }
    }
}

impl ChatRuntimeState {
    /// Load chat state, returning inactive state when the file is absent.
    pub async fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = tokio_fs::read_to_string(path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        serde_json::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    /// Atomically persist chat state through a temp-file rename.
    pub async fn save_atomic(&mut self, path: &Path) -> Result<()> {
        self.updated_at = Some(Local::now());
        if let Some(parent) = path.parent() {
            tokio_fs::create_dir_all(parent).await?;
        }
        let temp_path = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(self)?;
        tokio_fs::write(&temp_path, format!("{text}\n"))
            .await
            .with_context(|| format!("failed to write {}", temp_path.display()))?;
        tokio_fs::rename(&temp_path, path)
            .await
            .with_context(|| format!("failed to rename {}", temp_path.display()))?;
        Ok(())
    }

    /// Insert or update a resumable session and mark it active.
    pub fn record_active_session(&mut self, session: ChatSession) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|known| known.session_id == session.session_id)
        {
            *existing = session.clone();
        } else {
            self.sessions.push(session.clone());
        }
        self.active_runner = Some(session.runner.clone());
        self.active_session_id = Some(session.session_id.clone());
        self.state = ChatStateValue::ActiveIdle;
        self.current_request_id = None;
        self.current_process_id = None;
        self.busy_since = None;
        self.last_error = None;
    }

    /// Return sessions newest-first for menu display.
    pub fn sessions_newest_first(&self) -> Vec<ChatSession> {
        let mut sessions = self.sessions.clone();
        sessions.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at));
        sessions
    }

    /// Remove every saved session and detach the active chat bridge session.
    pub fn clear_sessions(&mut self) -> usize {
        let cleared = self.sessions.len();
        self.sessions.clear();
        self.state = ChatStateValue::Inactive;
        self.active_runner = None;
        self.active_session_id = None;
        self.current_request_id = None;
        self.current_process_id = None;
        self.busy_since = None;
        self.last_error = None;
        cleared
    }
}

/// Owned home-level chat lock that removes the lock file on drop.
pub struct ChatLock {
    path: PathBuf,
}

impl ChatLock {
    /// Acquire the chat lock, returning `None` if another process owns it.
    pub fn acquire(path: impl Into<PathBuf>) -> Result<Option<Self>> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }

        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(Some(Self { path })),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(err) => Err(err).with_context(|| format!("failed to create {}", path.display())),
        }
    }
}

impl Drop for ChatLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Return model references supported by a current chat adapter.
pub fn chat_streaming_model_names(config: &Config) -> Vec<String> {
    let mut models = Vec::new();
    for (group, agent) in &config.code_agents {
        if chat_command_supported(agent) {
            models.extend(agent.model_references(group));
        }
    }
    models
}

/// True when `model` can be selected by the interactive chat bridge.
pub fn is_chat_streaming_model(config: &Config, model: &str) -> bool {
    config.code_agents.iter().any(|(group, agent)| {
        chat_command_supported(agent) && agent.has_model_reference(group, model)
    })
}

fn chat_command_supported(agent: &CodeAgentConfig) -> bool {
    !agent.stream_args.is_empty()
        && (codex::recognizes_command(&agent.command) || agy::recognizes_command(&agent.command))
}

/// Return the filesystem working directory used by interactive chat agents.
pub fn chat_working_directory(config: &Config) -> PathBuf {
    config.home.clone()
}

/// Load state, recover stale busy markers, and save if recovery changed it.
pub async fn load_recovered_chat_state(config: &Config) -> Result<ChatRuntimeState> {
    let path = config.chat_state_path();
    let mut state = ChatRuntimeState::load(&path).await?;
    if matches!(
        state.state,
        ChatStateValue::ActiveBusy | ChatStateValue::Aborting
    ) && busy_state_is_stale(&state)
    {
        state.state = if state.active_session_id.is_some() {
            ChatStateValue::ActiveIdle
        } else {
            ChatStateValue::Inactive
        };
        state.current_request_id = None;
        state.current_process_id = None;
        state.busy_since = None;
        state.last_error = Some("recovered stale busy chat state".to_string());
        state.save_atomic(&path).await?;
    }
    Ok(state)
}

/// Clear any in-flight chat turn left behind by a daemon restart.
pub async fn clear_busy_chat_state_on_daemon_start(config: &Config) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        return Ok(());
    };
    let path = config.chat_state_path();
    let mut state = ChatRuntimeState::load(&path).await?;
    if !daemon_start_should_clear_busy_state(&state) {
        return Ok(());
    }
    state.state = if state.active_session_id.is_some() {
        ChatStateValue::ActiveIdle
    } else {
        ChatStateValue::Inactive
    };
    state.current_request_id = None;
    state.current_process_id = None;
    state.busy_since = None;
    state.last_error = Some("cleared busy chat state after daemon restart".to_string());
    state.save_atomic(&path).await
}

fn daemon_start_should_clear_busy_state(state: &ChatRuntimeState) -> bool {
    if !matches!(
        state.state,
        ChatStateValue::ActiveBusy | ChatStateValue::Aborting
    ) {
        return false;
    }
    if !state
        .current_request_id
        .as_deref()
        .is_some_and(|request_id| request_id.starts_with("telegram:"))
    {
        return false;
    }
    match state.current_process_id {
        Some(pid) if pid == std::process::id() => true,
        Some(pid) if process_is_alive(pid) => false,
        _ => true,
    }
}

fn busy_state_is_stale(state: &ChatRuntimeState) -> bool {
    if let Some(pid) = state.current_process_id {
        return !process_is_alive(pid);
    }
    let Some(busy_since) = state.busy_since else {
        return true;
    };
    Local::now().signed_duration_since(busy_since) > Duration::minutes(30)
}

fn process_is_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

/// Persist that a chat turn has started, rejecting concurrent turns.
pub async fn mark_turn_started(config: &Config, request_id: String) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = load_recovered_chat_state(config).await?;
    if matches!(
        state.state,
        ChatStateValue::ActiveBusy | ChatStateValue::Aborting
    ) {
        bail!("chat bridge is busy");
    }
    if state.active_runner.is_none() || state.active_session_id.is_none() {
        bail!("no active chat session");
    }
    state.state = ChatStateValue::ActiveBusy;
    state.current_request_id = Some(request_id);
    state.current_process_id = Some(std::process::id());
    state.busy_since = Some(Local::now());
    state.last_error = None;
    state.save_atomic(&config.chat_state_path()).await
}

/// Persist that the active turn is being aborted.
pub async fn mark_turn_aborting(config: &Config) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = ChatRuntimeState::load(&config.chat_state_path()).await?;
    state.state = ChatStateValue::Aborting;
    state.save_atomic(&config.chat_state_path()).await
}

/// Persist the final state after a turn completes, fails, or aborts.
pub async fn mark_turn_finished(
    config: &Config,
    session: Option<ChatSession>,
    error: Option<String>,
) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = ChatRuntimeState::load(&config.chat_state_path()).await?;
    state.current_request_id = None;
    state.current_process_id = None;
    state.busy_since = None;
    state.last_error = error;
    if let Some(session) = session {
        state.record_active_session(session);
    } else if state.active_session_id.is_some() {
        state.state = ChatStateValue::ActiveIdle;
    } else {
        state.state = ChatStateValue::Inactive;
        state.active_runner = None;
        state.active_session_id = None;
    }
    state.save_atomic(&config.chat_state_path()).await
}

/// Mark the chat bridge inactive after a non-resumable adapter failure.
pub async fn mark_chat_inactive(config: &Config, error: String) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = ChatRuntimeState::load(&config.chat_state_path()).await?;
    state.state = ChatStateValue::Inactive;
    state.active_runner = None;
    state.active_session_id = None;
    state.current_request_id = None;
    state.current_process_id = None;
    state.busy_since = None;
    state.last_error = Some(error);
    state.save_atomic(&config.chat_state_path()).await
}

/// Streaming events emitted while a chat turn is running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEvent {
    /// Assistant text delta suitable for user-facing display.
    AssistantDelta(String),
    /// Tool or command output delta that may be displayed separately.
    ToolDelta(String),
    /// Informational lifecycle event.
    Info(String),
    /// Adapter-side warning or recoverable error.
    Warning(String),
    /// The active turn completed normally.
    TurnCompleted,
    /// The active turn failed.
    TurnFailed(String),
    /// A server request that the chat bridge does not handle yet.
    ApprovalRequired(String),
}

/// Result of interrupting an active chat turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AbortOutcome {
    /// Whether the adapter accepted the interrupt request.
    pub accepted: bool,
    /// Whether the session can still be resumed after abort.
    pub resumable: bool,
}

/// User-facing transport context used to choose adapter instructions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatInstructionContext {
    /// Local terminal REPL launched by `tinybutler chat`.
    LocalCli,
    /// Telegram chat bridge ingress.
    Telegram,
}

/// Adapter interface shared by local REPL and Telegram chat bridge code.
#[async_trait]
pub trait ChatAgent: Send {
    /// Return the currently active session metadata, if any.
    fn active_session(&self) -> Option<ChatSession>;

    /// Start a fresh interactive session and return its stable id.
    async fn start_session(&mut self) -> Result<ChatSession>;

    /// Return sessions this adapter knows how to resume.
    async fn list_sessions(&self) -> Result<Vec<ChatSession>>;

    /// Mark an existing session as active for the next turn.
    async fn resume_session(&mut self, session_id: &str) -> Result<ChatSession>;

    /// Resume for an ordinary turn without re-injecting transport instructions.
    async fn resume_session_for_turn(&mut self, session_id: &str) -> Result<ChatSession> {
        self.resume_session(session_id).await
    }

    /// Start one user turn in the active session.
    async fn send_turn(&mut self, message: &str) -> Result<()>;

    /// Read the next streaming event for the active turn.
    async fn next_event(&mut self) -> Result<Option<ChatEvent>>;

    /// Abort the active turn without deleting the session.
    async fn abort_turn(&mut self) -> Result<AbortOutcome>;
}

/// Heap-owned adapter selected from one resolved code-agent configuration.
pub type BoxedChatAgent = Box<dyn ChatAgent>;

/// Expand the only placeholders understood by the shared chat layer.
fn expand_chat_stream_args(args: &[String], model: &str) -> Result<Vec<String>> {
    let stdin_markers = args.iter().filter(|arg| arg.as_str() == "{stdin}").count();
    if stdin_markers != 1 {
        bail!("chat stream_args must contain exactly one {{stdin}} marker");
    }
    Ok(args
        .iter()
        .filter(|arg| arg.as_str() != "{stdin}")
        .map(|arg| arg.replace("{model}", model))
        .collect())
}

/// Connect the concrete chat adapter that recognizes the configured command.
pub async fn connect_chat_agent(
    runner: impl Into<String>,
    agent: ResolvedCodeAgent<'_>,
    working_directory: Option<PathBuf>,
    instruction_context: ChatInstructionContext,
) -> Result<BoxedChatAgent> {
    let runner = runner.into();
    if agent.config.stream_args.is_empty() {
        bail!("runner does not configure stream_args");
    }
    if codex::recognizes_command(&agent.config.command) {
        return Ok(Box::new(
            codex::CodexChatAgent::connect_with_context(
                runner,
                agent,
                working_directory,
                instruction_context,
            )
            .await?,
        ));
    }
    if agy::recognizes_command(&agent.config.command) {
        return Ok(Box::new(agy::AgyChatAgent::connect_with_context(
            runner,
            agent,
            working_directory,
            instruction_context,
        )?));
    }
    bail!("unsupported chat command {}", agent.config.command)
}

fn chat_bridge_instructions(context: ChatInstructionContext) -> String {
    let mut instructions = vec![
        "Never expose hidden chain-of-thought, scratchpad text, or reasoning tags such as <think>, <thinking>, <thought>, or <final>. Send only the concise user-visible result.",
        "For a current Linux desktop screenshot, use `/home/wenyuan/linux_dotfiles/skills/screenshot/scripts/take_screenshot.py --mode temp` when it exists.",
    ];
    match context {
        ChatInstructionContext::LocalCli => {
            instructions.insert(
                0,
                "You are connected to the user through TinyButler's local terminal chat REPL.",
            );
            instructions.push(
                "When you create an image, screenshot, report, or other artifact, print the local file path in the final answer. Do not assume Telegram delivery unless the user explicitly asks you to send through Telegram.",
            );
        }
        ChatInstructionContext::Telegram => {
            instructions.insert(
                0,
                "You are connected to the user through TinyButler's Telegram chat bridge.",
            );
            instructions.push(
                "When the user asks for an image, screenshot, or generated artifact, create a local file and make it deliverable. Prefer calling `tinybutler telegram --attachment <path> --caption '<short caption>'` when you intentionally want to send it yourself.",
            );
            instructions.push(
                "If you cannot or do not call the TinyButler Telegram CLI directly, include `ATTACH:<path>` on its own line in the final answer. TinyButler will upload that file to Telegram and remove the marker from the visible text.",
            );
        }
    }
    instructions.join("\n")
}

fn summarize_user_message(message: &str) -> String {
    const LIMIT: usize = 80;
    let trimmed = message.trim();
    if trimmed.chars().count() <= LIMIT {
        return trimmed.to_string();
    }
    let mut summary = trimmed.chars().take(LIMIT).collect::<String>();
    summary.push_str("...");
    summary
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CodeAgentConfig;

    fn codex_config(stream_args: &[&str]) -> CodeAgentConfig {
        CodeAgentConfig {
            command: "/usr/bin/codex".to_string(),
            stream_args: stream_args.iter().map(|arg| arg.to_string()).collect(),
            models: vec!["gpt-test".to_string()],
            ..CodeAgentConfig::default()
        }
    }

    #[test]
    fn filters_chat_streaming_runners_to_supported_commands() {
        let mut config = Config {
            home: PathBuf::from("/tmp/tinybutler-chat-test"),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        config
            .code_agents
            .insert("codex".to_string(), codex_config(&["{stdin}"]));
        config.code_agents.insert(
            "gemini".to_string(),
            CodeAgentConfig {
                command: "agy".to_string(),
                stream_args: ["--opaque", "value", "{stdin}"]
                    .map(str::to_string)
                    .to_vec(),
                models: vec!["gemini-test".to_string()],
                ..CodeAgentConfig::default()
            },
        );
        config.code_agents.insert(
            "unsupported".to_string(),
            CodeAgentConfig {
                command: "/usr/bin/other".to_string(),
                stream_args: ["--opaque", "{stdin}"].map(str::to_string).to_vec(),
                models: vec!["other-test".to_string()],
                ..CodeAgentConfig::default()
            },
        );

        assert_eq!(
            chat_streaming_model_names(&config),
            vec!["codex/gpt-test", "gemini/gemini-test"]
        );
        assert!(is_chat_streaming_model(&config, "codex/gpt-test"));
        assert!(is_chat_streaming_model(&config, "gemini/gemini-test"));
        assert!(!is_chat_streaming_model(&config, "unsupported/other-test"));
    }

    #[test]
    fn chat_working_directory_is_tinybutler_home() {
        let config = Config {
            home: PathBuf::from("/tmp/tinybutler-chat-home"),
            telegram: Default::default(),
            code_agents: Default::default(),
        };

        assert_eq!(
            chat_working_directory(&config),
            PathBuf::from("/tmp/tinybutler-chat-home")
        );
    }

    #[test]
    fn telegram_bridge_instructions_describe_attachment_delivery() {
        let instructions = chat_bridge_instructions(ChatInstructionContext::Telegram);

        assert!(instructions.contains("TinyButler"));
        assert!(instructions.contains("Telegram chat bridge"));
        assert!(instructions.contains("ATTACH:<path>"));
        assert!(instructions.contains("tinybutler telegram --attachment"));
        assert!(instructions.contains("take_screenshot.py"));
    }

    #[test]
    fn local_bridge_instructions_do_not_claim_telegram_context() {
        let instructions = chat_bridge_instructions(ChatInstructionContext::LocalCli);

        assert!(instructions.contains("local terminal chat REPL"));
        assert!(instructions.contains("print the local file path"));
        assert!(!instructions.contains("Telegram chat bridge"));
        assert!(!instructions.contains("ATTACH:<path>"));
        assert!(!instructions.contains("tinybutler telegram --attachment"));
    }

    #[tokio::test]
    async fn recovers_stale_busy_state() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            busy_since: Some(Local::now() - Duration::minutes(31)),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        let recovered = load_recovered_chat_state(&config)
            .await
            .expect("recover state");
        assert_eq!(recovered.state, ChatStateValue::ActiveIdle);
        assert!(recovered.last_error.is_some());
    }

    #[tokio::test]
    async fn daemon_start_clears_telegram_busy_state_for_current_process() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            current_request_id: Some("telegram:1".to_string()),
            current_process_id: Some(std::process::id()),
            busy_since: Some(Local::now()),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        clear_busy_chat_state_on_daemon_start(&config)
            .await
            .expect("clear busy state");
        let recovered = ChatRuntimeState::load(&config.chat_state_path())
            .await
            .expect("load state");
        assert_eq!(recovered.state, ChatStateValue::ActiveIdle);
        assert_eq!(recovered.current_request_id, None);
        assert_eq!(recovered.current_process_id, None);
        assert_eq!(recovered.busy_since, None);
        assert!(recovered.last_error.is_some());
    }

    #[tokio::test]
    async fn daemon_start_clears_telegram_aborting_state_for_current_process() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::Aborting,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            current_request_id: Some("telegram:1".to_string()),
            current_process_id: Some(std::process::id()),
            busy_since: Some(Local::now()),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        clear_busy_chat_state_on_daemon_start(&config)
            .await
            .expect("clear busy state");
        let recovered = ChatRuntimeState::load(&config.chat_state_path())
            .await
            .expect("load state");
        assert_eq!(recovered.state, ChatStateValue::ActiveIdle);
        assert_eq!(recovered.current_request_id, None);
        assert_eq!(recovered.current_process_id, None);
        assert_eq!(recovered.busy_since, None);
        assert!(recovered.last_error.is_some());
    }

    #[tokio::test]
    async fn daemon_start_keeps_local_busy_state_for_current_process() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            current_request_id: Some("local:1".to_string()),
            current_process_id: Some(std::process::id()),
            busy_since: Some(Local::now()),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        clear_busy_chat_state_on_daemon_start(&config)
            .await
            .expect("clear busy state");
        let loaded = ChatRuntimeState::load(&config.chat_state_path())
            .await
            .expect("load state");
        assert_eq!(loaded.state, ChatStateValue::ActiveBusy);
        assert_eq!(loaded.current_request_id.as_deref(), Some("local:1"));
    }

    #[tokio::test]
    async fn daemon_start_does_not_fail_when_chat_lock_exists() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            current_request_id: Some("telegram:1".to_string()),
            current_process_id: Some(std::process::id()),
            busy_since: Some(Local::now()),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");
        let _lock = ChatLock::acquire(config.chat_lock_path())
            .expect("acquire lock")
            .expect("lock available");

        clear_busy_chat_state_on_daemon_start(&config)
            .await
            .expect("startup cleanup should not fail on lock contention");
        let loaded = ChatRuntimeState::load(&config.chat_state_path())
            .await
            .expect("load state");
        assert_eq!(loaded.state, ChatStateValue::ActiveBusy);
    }

    #[tokio::test]
    async fn keeps_fresh_busy_state_busy() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            busy_since: Some(Local::now()),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        let loaded = load_recovered_chat_state(&config)
            .await
            .expect("load state");
        assert_eq!(loaded.state, ChatStateValue::ActiveBusy);
    }

    #[tokio::test]
    async fn keeps_busy_state_for_live_owner_process_even_when_old() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            current_process_id: Some(std::process::id()),
            busy_since: Some(Local::now() - Duration::hours(2)),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        let loaded = load_recovered_chat_state(&config)
            .await
            .expect("load state");
        assert_eq!(loaded.state, ChatStateValue::ActiveBusy);
    }

    #[tokio::test]
    async fn recovers_busy_state_for_dead_owner_process() {
        let temp = tempfile::tempdir().expect("temp dir");
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let mut state = ChatRuntimeState {
            state: ChatStateValue::ActiveBusy,
            active_runner: Some("codex".to_string()),
            active_session_id: Some("thread-1".to_string()),
            current_process_id: Some(u32::MAX),
            busy_since: Some(Local::now()),
            ..ChatRuntimeState::default()
        };
        state
            .save_atomic(&config.chat_state_path())
            .await
            .expect("save state");

        let recovered = load_recovered_chat_state(&config)
            .await
            .expect("recover state");
        assert_eq!(recovered.state, ChatStateValue::ActiveIdle);
        assert!(recovered.last_error.is_some());
    }

    #[test]
    fn clearing_sessions_resets_active_chat_state() {
        let mut state = ChatRuntimeState {
            state: ChatStateValue::SelectingSession,
            active_runner: Some("codex-max/gpt-5.6-sol".to_string()),
            active_session_id: Some("thread-1".to_string()),
            sessions: vec![ChatSession {
                runner: "codex-max/gpt-5.6-sol".to_string(),
                session_id: "thread-1".to_string(),
                title: Some("test".to_string()),
                last_activity_at: Local::now(),
            }],
            current_request_id: Some("callback:clear".to_string()),
            current_process_id: Some(std::process::id()),
            busy_since: Some(Local::now()),
            last_error: Some("old error".to_string()),
            updated_at: Some(Local::now()),
        };

        let cleared = state.clear_sessions();

        assert_eq!(cleared, 1);
        assert_eq!(state.state, ChatStateValue::Inactive);
        assert_eq!(state.active_runner, None);
        assert_eq!(state.active_session_id, None);
        assert!(state.sessions.is_empty());
        assert_eq!(state.current_request_id, None);
        assert_eq!(state.current_process_id, None);
        assert_eq!(state.busy_since, None);
        assert_eq!(state.last_error, None);
    }

    #[test]
    fn summarizes_long_user_messages() {
        let summary = summarize_user_message(&"a".repeat(100));
        assert_eq!(summary.chars().count(), 83);
        assert!(summary.ends_with("..."));
    }
}
