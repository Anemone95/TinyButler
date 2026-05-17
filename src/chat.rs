//! Interactive chat bridge abstractions and code-agent adapters.
//!
//! The chat bridge is intentionally independent from Telegram. Local REPL
//! commands and Telegram ingress should both drive the same `ChatAgent`
//! interface so state, abort, and resume behavior remain consistent.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local};
use codex_codes::{
    protocol::methods, protocol_generated::types::ThreadResumeParams, AppServerBuilder,
    AsyncClient, Notification, ServerMessage, ServerRequest, ThreadStartParams, TurnStartParams,
    UserInput,
};
use serde::{Deserialize, Serialize};
use tokio::fs as tokio_fs;

use crate::config::{CodeAgentConfig, Config};

/// A resumable interactive code-agent session known to TinyButler.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChatSession {
    /// Configured runner key under `code_agents`.
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

/// Return configured runner keys that can start interactive streaming sessions.
pub fn streaming_runner_keys(config: &Config) -> Vec<String> {
    config
        .code_agents
        .iter()
        .filter(|(_, agent)| !agent.stream_args.is_empty())
        .map(|(key, _)| key.clone())
        .collect()
}

/// Return runner keys supported by the current Codex chat adapter.
pub fn codex_streaming_runner_keys(config: &Config) -> Vec<String> {
    config
        .code_agents
        .iter()
        .filter(|(_, agent)| codex_chat_supported(agent))
        .map(|(key, _)| key.clone())
        .collect()
}

/// True when a runner can be handled by `CodexChatAgent`.
pub fn codex_chat_supported(agent: &CodeAgentConfig) -> bool {
    !agent.stream_args.is_empty() && agent.stream_args.iter().any(|arg| arg == "app-server")
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
        state.last_error =
            Some("recovered stale busy chat state after process restart".to_string());
        state.save_atomic(&path).await?;
    }
    Ok(state)
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

/// Adapter interface shared by local REPL and Telegram chat bridge code.
#[async_trait]
pub trait ChatAgent {
    /// Start a fresh interactive session and return its stable id.
    async fn start_session(&mut self) -> Result<ChatSession>;

    /// Return sessions this adapter knows how to resume.
    async fn list_sessions(&self) -> Result<Vec<ChatSession>>;

    /// Mark an existing session as active for the next turn.
    async fn resume_session(&mut self, session_id: &str) -> Result<ChatSession>;

    /// Start one user turn in the active session.
    async fn send_turn(&mut self, message: &str) -> Result<()>;

    /// Read the next streaming event for the active turn.
    async fn next_event(&mut self) -> Result<Option<ChatEvent>>;

    /// Abort the active turn without deleting the session.
    async fn abort_turn(&mut self) -> Result<AbortOutcome>;
}

/// Codex `ChatAgent` implementation backed by `codex-codes` app-server.
pub struct CodexChatAgent {
    runner: String,
    client: AsyncClient,
    active_session: Option<ChatSession>,
    known_sessions: Vec<ChatSession>,
    current_turn_id: Option<String>,
    abort_requested: bool,
}

impl CodexChatAgent {
    /// Start a Codex app-server from local runner config.
    pub async fn connect(
        runner: impl Into<String>,
        agent: &CodeAgentConfig,
        working_directory: Option<PathBuf>,
    ) -> Result<Self> {
        let builder = codex_builder_from_config(agent, working_directory)?;
        let client = AsyncClient::start_with(builder)
            .await
            .context("failed to start Codex app-server")?;
        Ok(Self {
            runner: runner.into(),
            client,
            active_session: None,
            known_sessions: Vec::new(),
            current_turn_id: None,
            abort_requested: false,
        })
    }

    async fn interrupt_current_turn(&mut self) -> Result<()> {
        let session = self
            .active_session
            .as_ref()
            .context("no active Codex chat session to abort")?;
        let turn_id = self
            .current_turn_id
            .clone()
            .context("no active Codex turn id to abort")?;
        self.client
            .request::<_, serde_json::Value>(
                methods::TURN_INTERRUPT,
                &serde_json::json!({
                    "threadId": session.session_id.clone(),
                    "turnId": turn_id,
                }),
            )
            .await
            .context("failed to interrupt Codex turn")?;
        Ok(())
    }

    fn record_session(&mut self, session: ChatSession) -> ChatSession {
        if let Some(existing) = self
            .known_sessions
            .iter_mut()
            .find(|known| known.session_id == session.session_id)
        {
            *existing = session.clone();
        } else {
            self.known_sessions.push(session.clone());
        }
        self.active_session = Some(session.clone());
        session
    }

    /// Return the currently active session metadata, if any.
    pub fn active_session(&self) -> Option<ChatSession> {
        self.active_session.clone()
    }
}

#[async_trait]
impl ChatAgent for CodexChatAgent {
    async fn start_session(&mut self) -> Result<ChatSession> {
        let thread = self
            .client
            .thread_start(&ThreadStartParams::default())
            .await
            .context("failed to start Codex thread")?;
        let session = ChatSession {
            runner: self.runner.clone(),
            session_id: thread.thread_id().to_string(),
            title: None,
            last_activity_at: Local::now(),
        };
        Ok(self.record_session(session))
    }

    async fn list_sessions(&self) -> Result<Vec<ChatSession>> {
        Ok(self.known_sessions.clone())
    }

    async fn resume_session(&mut self, session_id: &str) -> Result<ChatSession> {
        let mut session = self
            .known_sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
            .unwrap_or_else(|| ChatSession {
                runner: self.runner.clone(),
                session_id: session_id.to_string(),
                title: None,
                last_activity_at: Local::now(),
            });
        let response: codex_codes::protocol_generated::types::ThreadResumeResponse = self
            .client
            .request(
                methods::THREAD_RESUME,
                &ThreadResumeParams {
                    approval_policy: None,
                    approvals_reviewer: None,
                    base_instructions: None,
                    config: None,
                    cwd: None,
                    developer_instructions: None,
                    model: None,
                    model_provider: None,
                    personality: None,
                    sandbox: None,
                    service_tier: None,
                    thread_id: session_id.to_string(),
                },
            )
            .await
            .with_context(|| format!("failed to resume Codex chat session {session_id}"))?;
        session.session_id = response.thread.id.clone();
        session.last_activity_at = Local::now();
        self.active_session = Some(session.clone());
        Ok(self.record_session(session))
    }

    async fn send_turn(&mut self, message: &str) -> Result<()> {
        let session = self
            .active_session
            .as_ref()
            .context("no active Codex chat session")?;
        self.current_turn_id = None;
        self.abort_requested = false;
        self.client
            .turn_start(&TurnStartParams {
                thread_id: session.session_id.clone(),
                input: vec![UserInput::Text {
                    text: message.to_string(),
                }],
                model: None,
                reasoning_effort: None,
                sandbox_policy: None,
            })
            .await
            .context("failed to start Codex turn")?;

        if let Some(active) = self.active_session.as_mut() {
            active.last_activity_at = Local::now();
            if active.title.is_none() {
                active.title = Some(summarize_user_message(message));
            }
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ChatEvent>> {
        let Some(message) = self.client.next_message().await? else {
            return Ok(None);
        };
        let event = match message {
            ServerMessage::Request { request, .. } => {
                let description = request.method().to_string();
                match request {
                    ServerRequest::CmdExecApproval(_) | ServerRequest::FileChangeApproval(_) => {
                        ChatEvent::ApprovalRequired(description)
                    }
                    _ => ChatEvent::ApprovalRequired(description),
                }
            }
            other => {
                let event = codex_message_to_chat_event(other, &mut self.current_turn_id);
                if matches!(event, ChatEvent::Info(_))
                    && self.abort_requested
                    && self.current_turn_id.is_some()
                {
                    self.interrupt_current_turn().await?;
                }
                event
            }
        };
        Ok(Some(event))
    }

    async fn abort_turn(&mut self) -> Result<AbortOutcome> {
        self.abort_requested = true;
        if self.current_turn_id.is_some() {
            self.interrupt_current_turn().await?;
        }
        Ok(AbortOutcome {
            accepted: true,
            resumable: true,
        })
    }
}

/// Build a Codex app-server builder from a TinyButler `stream_args` runner entry.
pub fn codex_builder_from_config(
    agent: &CodeAgentConfig,
    working_directory: Option<PathBuf>,
) -> Result<AppServerBuilder> {
    if agent.stream_args.is_empty() {
        bail!("Codex chat runner is missing stream_args");
    }

    let mut builder = AppServerBuilder::new().command(&agent.command);
    if let Some(directory) = working_directory {
        builder = builder.working_directory(directory);
    }

    let mut extra_args = Vec::new();
    let mut iter = agent.stream_args.iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "app-server" => {}
            "-c" | "--config" => {
                let Some(value) = iter.next() else {
                    bail!("Codex stream_args contains {arg} without key=value");
                };
                let Some((key, value)) = value.split_once('=') else {
                    bail!("Codex stream_args config override must be key=value: {value}");
                };
                builder = builder.config_override(key, value);
            }
            "--listen" => {
                let Some(value) = iter.next() else {
                    bail!("Codex stream_args contains --listen without endpoint");
                };
                if value != "stdio://" {
                    bail!("Codex chat bridge only supports --listen stdio://, got {value}");
                }
            }
            "stdio://" => {}
            other => extra_args.push(other.to_string()),
        }
    }

    if !extra_args.is_empty() {
        builder = builder.extra_args(extra_args);
    }
    Ok(builder)
}

fn codex_message_to_chat_event(
    message: ServerMessage,
    current_turn_id: &mut Option<String>,
) -> ChatEvent {
    match message {
        ServerMessage::Notification(notification) => match notification {
            Notification::AgentMessageDelta(delta) => ChatEvent::AssistantDelta(delta.delta),
            Notification::CmdOutputDelta(delta) => ChatEvent::ToolDelta(delta.delta),
            Notification::ReasoningDelta(delta) => ChatEvent::Info(delta.delta),
            Notification::ReasoningTextDelta(delta) => ChatEvent::Info(delta.delta),
            Notification::TurnStarted(started) => {
                *current_turn_id = Some(started.turn.id);
                ChatEvent::Info("turn started".to_string())
            }
            Notification::TurnCompleted(_) => {
                *current_turn_id = None;
                ChatEvent::TurnCompleted
            }
            Notification::Error(error) => {
                *current_turn_id = None;
                ChatEvent::TurnFailed(format!("{error:?}"))
            }
            other => ChatEvent::Info(other.method().to_string()),
        },
        ServerMessage::Request { request, .. } => {
            ChatEvent::ApprovalRequired(request.method().to_string())
        }
    }
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

    fn codex_config(stream_args: &[&str]) -> CodeAgentConfig {
        CodeAgentConfig {
            command: "/usr/bin/codex".to_string(),
            stream_args: stream_args.iter().map(|arg| arg.to_string()).collect(),
            ..CodeAgentConfig::default()
        }
    }

    #[test]
    fn filters_codex_streaming_runners_to_app_server() {
        let mut config = Config {
            home: PathBuf::from("/tmp/tinybutler-chat-test"),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        config
            .code_agents
            .insert("codex".to_string(), codex_config(&["app-server"]));
        config
            .code_agents
            .insert("gemini".to_string(), codex_config(&["--output-format"]));

        assert_eq!(codex_streaming_runner_keys(&config), vec!["codex"]);
        assert_eq!(streaming_runner_keys(&config), vec!["codex", "gemini"]);
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
    fn rejects_missing_codex_stream_args() {
        let err = codex_builder_from_config(&codex_config(&[]), None).unwrap_err();
        assert!(err.to_string().contains("missing stream_args"));
    }

    #[test]
    fn parses_codex_stream_args_config_overrides() {
        let config = codex_config(&[
            "app-server",
            "-c",
            "model=\"gpt-5.5\"",
            "-c",
            "sandbox_mode=\"danger-full-access\"",
            "--listen",
            "stdio://",
        ]);
        codex_builder_from_config(&config, Some(PathBuf::from("/tmp"))).expect("builder");
    }

    #[test]
    fn rejects_non_stdio_codex_stream_args() {
        let config = codex_config(&["app-server", "--listen", "ws://127.0.0.1:9999"]);
        let err = codex_builder_from_config(&config, None).unwrap_err();
        assert!(err.to_string().contains("stdio://"));
    }

    #[test]
    fn summarizes_long_user_messages() {
        let summary = summarize_user_message(&"a".repeat(100));
        assert_eq!(summary.chars().count(), 83);
        assert!(summary.ends_with("..."));
    }
}
