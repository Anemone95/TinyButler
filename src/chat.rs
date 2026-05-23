//! Interactive chat bridge abstractions and code-agent adapters.
//!
//! The chat bridge is intentionally independent from Telegram. Local REPL
//! commands and Telegram ingress should both drive the same `ChatAgent`
//! interface so state, abort, and resume behavior remain consistent.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use chrono::{DateTime, Duration, Local};
use codex_codes::{
    AppServerBuilder, AsyncClient, Notification, ServerMessage, ServerRequest, ThreadStartParams,
    TurnStartParams, UserInput, protocol::methods, protocol_generated::types::ThreadResumeParams,
};
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

/// Return model references supported by the current Codex chat adapter.
pub fn codex_streaming_model_names(config: &Config) -> Vec<String> {
    let mut models = Vec::new();
    for (group, agent) in &config.code_agents {
        if codex_chat_supported(agent) {
            models.extend(agent.model_references(group));
        }
    }
    models
}

/// True when `model` is a chat-selectable Codex model reference.
pub fn is_codex_streaming_model(config: &Config, model: &str) -> bool {
    config.code_agents.iter().any(|(group, agent)| {
        codex_chat_supported(agent) && agent.has_model_reference(group, model)
    })
}

/// True when a runner can be handled by `CodexChatAgent`.
pub fn codex_chat_supported(agent: &CodeAgentConfig) -> bool {
    !agent.stream_args.is_empty() && agent.stream_args.iter().any(|arg| arg == "app-server")
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
    instruction_context: ChatInstructionContext,
    active_session: Option<ChatSession>,
    known_sessions: Vec<ChatSession>,
    current_turn_id: Option<String>,
    abort_requested: bool,
}

impl CodexChatAgent {
    /// Start a Codex app-server from local runner config.
    pub async fn connect(
        runner: impl Into<String>,
        agent: ResolvedCodeAgent<'_>,
        working_directory: Option<PathBuf>,
    ) -> Result<Self> {
        Self::connect_with_context(
            runner,
            agent,
            working_directory,
            ChatInstructionContext::LocalCli,
        )
        .await
    }

    /// Start a Codex app-server with explicit user-facing transport context.
    pub async fn connect_with_context(
        runner: impl Into<String>,
        agent: ResolvedCodeAgent<'_>,
        working_directory: Option<PathBuf>,
        instruction_context: ChatInstructionContext,
    ) -> Result<Self> {
        let builder = codex_builder_from_config(agent, working_directory)?;
        let client = AsyncClient::start_with(builder)
            .await
            .context("failed to start Codex app-server")?;
        Ok(Self {
            runner: runner.into(),
            client,
            instruction_context,
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

    /// Resume a session for an ordinary turn without re-injecting transport instructions.
    pub async fn resume_session_for_turn(&mut self, session_id: &str) -> Result<ChatSession> {
        self.resume_session_inner(session_id, None).await
    }

    async fn resume_session_inner(
        &mut self,
        session_id: &str,
        developer_instructions: Option<String>,
    ) -> Result<ChatSession> {
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
                    developer_instructions,
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
}

#[async_trait]
impl ChatAgent for CodexChatAgent {
    async fn start_session(&mut self) -> Result<ChatSession> {
        let thread = self
            .client
            .thread_start(&ThreadStartParams {
                instructions: Some(chat_bridge_instructions(self.instruction_context)),
                tools: None,
            })
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
        self.resume_session_inner(
            session_id,
            Some(chat_bridge_instructions(self.instruction_context)),
        )
        .await
    }

    async fn send_turn(&mut self, message: &str) -> Result<()> {
        let session = self
            .active_session
            .as_ref()
            .context("no active Codex chat session")?;
        self.current_turn_id = None;
        self.abort_requested = false;
        if let Err(err) = self
            .client
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
        {
            if let Some(message) = codex_deserialization_chat_error(&err) {
                return Err(anyhow!(message));
            }
            return Err(err).context("failed to start Codex turn");
        }

        if let Some(active) = self.active_session.as_mut() {
            active.last_activity_at = Local::now();
            if active.title.is_none() {
                active.title = Some(summarize_user_message(message));
            }
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ChatEvent>> {
        let message = match self.client.next_message().await {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(None),
            Err(err) => {
                if let Some(message) = codex_deserialization_chat_error(&err) {
                    return Ok(Some(ChatEvent::TurnFailed(message)));
                }
                return Err(err.into());
            }
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

fn codex_deserialization_chat_error(err: &codex_codes::Error) -> Option<String> {
    let codex_codes::Error::Deserialization(parse_error) = err else {
        return None;
    };
    if !matches!(
        parse_error.method.as_deref(),
        Some(methods::ITEM_STARTED | methods::ITEM_COMPLETED)
    ) || !parse_error.error_message.contains("unknown variant")
    {
        return None;
    }
    let item_type = parse_error
        .raw_json
        .as_ref()?
        .pointer("/item/type")
        .and_then(serde_json::Value::as_str)?;
    Some(format!(
        "Codex item type `{}` (`{}`) is not supported by this TinyButler build. Update TinyButler's Codex protocol model before retrying.",
        item_type,
        codex_item_type_name(item_type)
    ))
}

fn codex_item_type_name(item_type: &str) -> String {
    let mut chars = item_type.chars();
    let Some(first) = chars.next() else {
        return "UnknownItem".to_string();
    };
    let rest: String = chars.collect();
    format!("{}{rest}Item", first.to_uppercase())
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

/// Build a Codex app-server builder from a TinyButler `stream_args` runner entry.
pub fn codex_builder_from_config(
    agent: ResolvedCodeAgent<'_>,
    working_directory: Option<PathBuf>,
) -> Result<AppServerBuilder> {
    if agent.config.stream_args.is_empty() {
        bail!("Codex chat runner is missing stream_args");
    }

    let mut builder = AppServerBuilder::new().command(&agent.config.command);
    if let Some(directory) = working_directory {
        builder = builder.working_directory(directory);
    }

    let mut extra_args = Vec::new();
    let expanded_args = agent
        .config
        .stream_args
        .iter()
        .map(|arg| arg.replace("{model}", &agent.model))
        .collect::<Vec<_>>();
    let mut iter = expanded_args.iter().peekable();
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
            models: vec!["gpt-test".to_string()],
            ..CodeAgentConfig::default()
        }
    }

    fn resolved_codex_config(config: &CodeAgentConfig) -> ResolvedCodeAgent<'_> {
        ResolvedCodeAgent {
            model: "gpt-test".to_string(),
            config,
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
        config.code_agents.insert(
            "gemini".to_string(),
            CodeAgentConfig {
                command: "/usr/bin/gemini".to_string(),
                stream_args: vec!["--output-format".to_string()],
                models: vec!["gemini-test".to_string()],
                ..CodeAgentConfig::default()
            },
        );

        assert_eq!(codex_streaming_model_names(&config), vec!["codex/gpt-test"]);
        assert_eq!(
            config.streaming_model_names(),
            vec!["codex/gpt-test", "gemini/gemini-test"]
        );
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

    #[test]
    fn decodes_codex_file_change_started_in_progress_status() {
        let params = serde_json::json!({
            "item": {
                "changes": [{
                    "diff": "@@ -1 +1 @@\n-a\n+b\n",
                    "kind": { "move_path": null, "type": "update" },
                    "path": "/tmp/example.txt"
                }],
                "id": "call_1",
                "status": "inProgress",
                "type": "fileChange"
            },
            "startedAtMs": 1779051453356_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let notification =
            codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params))
                .expect("fileChange inProgress should decode");

        assert!(matches!(notification, Notification::ItemStarted(_)));
    }

    #[test]
    fn decodes_codex_file_change_declined_status() {
        let params = serde_json::json!({
            "item": {
                "changes": [],
                "id": "call_1",
                "status": "declined",
                "type": "fileChange"
            },
            "completedAtMs": 1779051453356_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let notification =
            codex_codes::Notification::from_envelope(methods::ITEM_COMPLETED, Some(params))
                .expect("fileChange declined should decode");

        assert!(matches!(notification, Notification::ItemCompleted(_)));
    }

    #[test]
    fn decodes_codex_image_view_started_item() {
        let params = serde_json::json!({
            "item": {
                "id": "call_1",
                "path": "/tmp/room_snapshot.jpg",
                "type": "imageView"
            },
            "startedAtMs": 1779053681522_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let notification =
            codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params))
                .expect("imageView should decode");

        assert!(matches!(notification, Notification::ItemStarted(_)));
    }

    #[test]
    fn decodes_current_codex_app_server_thread_item_types() {
        let samples = [
            serde_json::json!({ "id": "hook_1", "type": "hookPrompt" }),
            serde_json::json!({ "id": "plan_1", "text": "check state", "type": "plan" }),
            serde_json::json!({ "id": "dyn_1", "type": "dynamicToolCall" }),
            serde_json::json!({ "id": "collab_1", "type": "collabAgentToolCall" }),
            serde_json::json!({ "id": "img_1", "status": "completed", "type": "imageGeneration" }),
            serde_json::json!({ "id": "review_1", "type": "enteredReviewMode" }),
            serde_json::json!({ "id": "review_2", "type": "exitedReviewMode" }),
            serde_json::json!({ "id": "compact_1", "type": "contextCompaction" }),
        ];

        for item in samples {
            let params = serde_json::json!({
                "item": item,
                "startedAtMs": 1779053681522_i64,
                "threadId": "thread_1",
                "turnId": "turn_1"
            });
            let notification =
                codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params))
                    .expect("current Codex app-server item should decode");

            assert!(matches!(notification, Notification::ItemStarted(_)));
        }
    }

    #[test]
    fn reports_unknown_codex_item_type_with_friendly_message() {
        let params = serde_json::json!({
            "item": {
                "id": "call_1",
                "path": "/tmp/example.wav",
                "type": "audioView"
            },
            "startedAtMs": 1779053681522_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let serde_error =
            codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params.clone()))
                .expect_err("audioView is intentionally unsupported in this test");
        let parse_error = codex_codes::ParseError::from_envelope(
            methods::ITEM_STARTED,
            Some(params),
            serde_error,
        );
        let error = codex_codes::Error::Deserialization(parse_error);
        let message = codex_deserialization_chat_error(&error).expect("friendly error");

        assert!(message.contains("audioView"));
        assert!(message.contains("AudioViewItem"));
        assert!(!message.contains("\"startedAtMs\""));
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
        let config = codex_config(&[]);
        let err = codex_builder_from_config(resolved_codex_config(&config), None).unwrap_err();
        assert!(err.to_string().contains("missing stream_args"));
    }

    #[test]
    fn parses_codex_stream_args_config_overrides() {
        let config = codex_config(&[
            "app-server",
            "-c",
            "model=\"{model}\"",
            "-c",
            "sandbox_mode=\"danger-full-access\"",
            "--listen",
            "stdio://",
        ]);
        codex_builder_from_config(resolved_codex_config(&config), Some(PathBuf::from("/tmp")))
            .expect("builder");
    }

    #[test]
    fn rejects_non_stdio_codex_stream_args() {
        let config = codex_config(&["app-server", "--listen", "ws://127.0.0.1:9999"]);
        let err = codex_builder_from_config(resolved_codex_config(&config), None).unwrap_err();
        assert!(err.to_string().contains("stdio://"));
    }

    #[test]
    fn summarizes_long_user_messages() {
        let summary = summarize_user_message(&"a".repeat(100));
        assert_eq!(summary.chars().count(), 83);
        assert!(summary.ends_with("..."));
    }
}
