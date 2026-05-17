//! Telegram Bot API sender and minimal long-polling ingress.
//!
//! Outbound sending is used by task notifications and agent calls. The MVP
//! ingress path uses direct Bot API long polling for the task-management slash
//! commands and can later be replaced by a richer teloxide workflow.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::fs;
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::chat::{
    codex_streaming_runner_keys, load_recovered_chat_state, mark_chat_inactive, mark_turn_finished,
    mark_turn_started, ChatAgent, ChatEvent, ChatLock, ChatSession, ChatStateValue, CodexChatAgent,
};
use crate::config::Config;
use crate::scheduler::Scheduler;

/// Telegram parse mode used by TinyButler-generated and CLI-authored messages.
pub const TELEGRAM_PARSE_MODE: &str = "MarkdownV2";

/// Parsed Telegram command supported by TinyButler ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressCommand {
    Help,
    New,
    Session,
    Abort,
    TaskList,
    TaskRun(String),
    TaskStatus(String),
    TaskEnable(String),
    TaskDisable(String),
    Unknown(String),
}

type AbortSender = oneshot::Sender<()>;
type SharedAbort = Arc<Mutex<Option<AbortSender>>>;

/// Send a MarkdownV2 text message to the configured chat.
pub async fn send_text(config: &Config, text: &str) -> Result<()> {
    let chat_id = configured_chat_id(config)?;
    send_markdown_text_to_chat(config, &chat_id, &sanitize_markdown_v2_message(text)).await
}

/// Send a local image file as a Telegram photo.
pub async fn send_photo(config: &Config, path: &Path, caption: Option<&str>) -> Result<()> {
    send_media(config, "sendPhoto", "photo", path, caption).await
}

/// Send a local file as a Telegram document.
pub async fn send_document(config: &Config, path: &Path, caption: Option<&str>) -> Result<()> {
    send_media(config, "sendDocument", "document", path, caption).await
}

/// Send the scheduler's standard task-run notification message.
pub async fn notify_run(
    config: &Config,
    task_name: &str,
    status: &str,
    exit_code: Option<i32>,
    duration_seconds: u64,
    log_path: &str,
    summary: &str,
) -> Result<()> {
    let mut text = format!(
        "*TinyButler task:* {}\n*status:* {}\n*duration:* {}\n*log:* {}",
        inline_code(task_name),
        inline_code(status),
        inline_code(&format!("{duration_seconds}s")),
        inline_code(log_path)
    );
    if let Some(code) = exit_code {
        text.push_str(&format!(
            "\n*exit code:* {}",
            inline_code(&code.to_string())
        ));
    }
    if !summary.trim().is_empty() {
        text.push_str("\n\n*summary:*\n");
        text.push_str(&code_block(&truncate(summary.trim(), 1200)));
    }
    let chat_id = configured_chat_id(config)?;
    send_markdown_text_to_chat(config, &chat_id, &text).await
}

/// Long-poll Telegram updates and route configured chat commands to CLI logic.
pub async fn poll(config: Config) -> Result<()> {
    let allowed_chat_id = configured_chat_id(&config)?;
    let client = Client::new();
    let state_path = config.telegram_state_path();
    let mut poll_state = TelegramPollState::load(&state_path).await?;
    let abort_sender: SharedAbort = Arc::new(Mutex::new(None));

    info!("starting Telegram polling for configured chat");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("stopping Telegram polling");
                return Ok(());
            }
            result = poll_once(&client, &config, poll_state.next_offset()) => {
                match result {
                    Ok(updates) => {
                        for update in updates {
                            let update_id = update.update_id;
                            let mut update_handled = update.message.is_none() && update.callback_query.is_none();
                            if let Some(message) = update.message {
                                if message.chat.id.to_string() != allowed_chat_id {
                                    update_handled = true;
                                } else if message.text.is_some() {
                                    if let Err(err) = handle_message(&config, abort_sender.clone(), &message).await {
                                        warn!("failed to handle Telegram command: {err:#}");
                                        let _ = send_markdown_text_to_chat(
                                            &config,
                                            &message.chat.id.to_string(),
                                            &format!("TinyButler command failed:\n{}", code_block(&truncate(&format!("{err:#}"), 1200))),
                                        ).await;
                                    }
                                    update_handled = true;
                                } else {
                                    update_handled = true;
                                }
                            }
                            if let Some(callback) = update.callback_query {
                                if callback.message.as_ref().map(|message| message.chat.id.to_string()) != Some(allowed_chat_id.clone()) {
                                    update_handled = true;
                                } else {
                                    if let Err(err) = handle_callback(&config, &callback).await {
                                        warn!("failed to handle Telegram callback: {err:#}");
                                        let _ = answer_callback_query(&config, &callback.id, Some("TinyButler callback failed")).await;
                                    }
                                    update_handled = true;
                                }
                            }
                            if update_handled {
                                poll_state.record_update(update_id);
                                poll_state.save(&state_path).await?;
                            }
                        }
                        poll_state.record_poll();
                        poll_state.save(&state_path).await?;
                    }
                    Err(err) => {
                        warn!("Telegram polling failed: {err:#}");
                        sleep(Duration::from_secs(3)).await;
                    }
                }
            }
        }
    }
}

/// Parse Telegram text into a TinyButler ingress command.
pub fn parse_ingress_command(text: &str) -> IngressCommand {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return IngressCommand::Unknown(String::new());
    }

    let mut parts = trimmed.split_whitespace();
    let raw_command = parts.next().unwrap_or_default();
    if !raw_command.starts_with('/') {
        return IngressCommand::Unknown(trimmed.to_string());
    }
    let command = raw_command
        .trim_start_matches('/')
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let rest = parts.collect::<Vec<_>>().join(" ");

    match command.as_str() {
        "help" | "start" => IngressCommand::Help,
        "new" => IngressCommand::New,
        "session" => IngressCommand::Session,
        "abort" => IngressCommand::Abort,
        "task_list" => IngressCommand::TaskList,
        "task_run" => {
            if rest.trim().is_empty() {
                IngressCommand::Unknown("missing task name for /task_run".to_string())
            } else {
                IngressCommand::TaskRun(rest)
            }
        }
        "task_status" => {
            if rest.trim().is_empty() {
                IngressCommand::Unknown("missing task name for /task_status".to_string())
            } else {
                IngressCommand::TaskStatus(rest)
            }
        }
        "task_enable" => {
            if rest.trim().is_empty() {
                IngressCommand::Unknown("missing task name for /task_enable".to_string())
            } else {
                IngressCommand::TaskEnable(rest)
            }
        }
        "task_disable" => {
            if rest.trim().is_empty() {
                IngressCommand::Unknown("missing task name for /task_disable".to_string())
            } else {
                IngressCommand::TaskDisable(rest)
            }
        }
        _ => IngressCommand::Unknown(trimmed.to_string()),
    }
}

async fn handle_message(
    config: &Config,
    abort_sender: SharedAbort,
    message: &TelegramMessage,
) -> Result<()> {
    let chat_id = message.chat.id.to_string();
    let Some(text) = message.text.as_deref() else {
        return Ok(());
    };
    let command = parse_ingress_command(text);
    if matches!(command, IngressCommand::Unknown(_)) && !text.trim().starts_with('/') {
        return handle_bare_chat_text(config, abort_sender, &chat_id, message.message_id, text)
            .await;
    }
    handle_command(config, abort_sender, &chat_id, text).await
}

async fn handle_command(
    config: &Config,
    abort_sender: SharedAbort,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    let command = parse_ingress_command(text);
    let scheduler = Scheduler::new(config.clone());
    let response = match command {
        IngressCommand::Help => help_text(),
        IngressCommand::New => {
            return handle_new_command(config, chat_id).await;
        }
        IngressCommand::Session => {
            return handle_session_command(config, chat_id).await;
        }
        IngressCommand::Abort => {
            return handle_abort_command(config, abort_sender, chat_id).await;
        }
        IngressCommand::TaskList => code_block(&truncate(&scheduler.task_list_text().await?, 3500)),
        IngressCommand::TaskStatus(task) => code_block(&truncate(
            &scheduler.task_status_text(task.trim()).await?,
            3500,
        )),
        IngressCommand::TaskEnable(task) => {
            let task_name = task.trim();
            scheduler.set_task_enabled_by_name(task_name, true).await?;
            format!("Task {} enabled", inline_code(task_name))
        }
        IngressCommand::TaskDisable(task) => {
            let task_name = task.trim();
            scheduler.set_task_enabled_by_name(task_name, false).await?;
            format!("Task {} disabled", inline_code(task_name))
        }
        IngressCommand::TaskRun(task) => {
            let task_name = task.trim();
            send_markdown_text_to_chat(
                config,
                chat_id,
                &format!("Running {}", inline_code(task_name)),
            )
            .await?;
            let outcome = scheduler.run_task_by_name(task_name).await?;
            format!(
                "Task {} finished with {} in {}\n\n{}",
                inline_code(task_name),
                inline_code(&outcome.status),
                inline_code(&format!("{}s", outcome.duration_seconds)),
                code_block(&truncate(&outcome.summary, 2500))
            )
        }
        IngressCommand::Unknown(text) => {
            if text.is_empty() {
                help_text()
            } else {
                format!(
                    "Unknown TinyButler command: {}\n\n{}",
                    inline_code(&text),
                    help_text()
                )
            }
        }
    };
    send_markdown_text_to_chat(config, chat_id, &response).await
}

fn help_text() -> String {
    [
        escape_markdown_v2("TinyButler commands:"),
        format!(
            "{}: {}",
            inline_code("/new"),
            escape_markdown_v2("start a code-agent chat session")
        ),
        format!(
            "{}: {}",
            inline_code("/session"),
            escape_markdown_v2("resume a code-agent chat session")
        ),
        format!(
            "{}: {}",
            inline_code("/abort"),
            escape_markdown_v2("abort the active code-agent turn")
        ),
        format!(
            "{}: {}",
            inline_code("/task_list"),
            escape_markdown_v2("list tasks")
        ),
        format!(
            "{}: {}",
            inline_code("/task_status <task>"),
            escape_markdown_v2("show task status")
        ),
        format!(
            "{}: {}",
            inline_code("/task_run <task>"),
            escape_markdown_v2("run one task now")
        ),
        format!(
            "{}: {}",
            inline_code("/task_enable <task>"),
            escape_markdown_v2("enable one task")
        ),
        format!(
            "{}: {}",
            inline_code("/task_disable <task>"),
            escape_markdown_v2("disable one task")
        ),
        format!(
            "{}: {}",
            inline_code("/help"),
            escape_markdown_v2("show this help")
        ),
    ]
    .join("\n")
}

async fn handle_new_command(config: &Config, chat_id: &str) -> Result<()> {
    let runners = codex_streaming_runner_keys(config);
    if runners.is_empty() {
        return send_markdown_text_to_chat(
            config,
            chat_id,
            "No Codex streaming runners configured",
        )
        .await;
    }
    set_telegram_selection_state(config, ChatStateValue::SelectingNew).await?;
    send_inline_menu(
        config,
        chat_id,
        "Select a model:",
        runners
            .iter()
            .map(|runner| (runner.clone(), format!("tc_new:{runner}")))
            .collect(),
    )
    .await
}

async fn handle_session_command(config: &Config, chat_id: &str) -> Result<()> {
    let state = load_recovered_chat_state(config).await?;
    let sessions = state.sessions_newest_first();
    if sessions.is_empty() {
        return send_markdown_text_to_chat(config, chat_id, "No resumable chat sessions").await;
    }
    set_telegram_selection_state(config, ChatStateValue::SelectingSession).await?;
    send_inline_menu(
        config,
        chat_id,
        "Select a session:",
        sessions
            .iter()
            .filter(|session| session.session_id.len() <= 51)
            .map(|session| {
                let title = session.title.as_deref().unwrap_or("(untitled)");
                (
                    format!(
                        "{} {}",
                        session.runner,
                        truncate(title, 28).replace('\n', " ")
                    ),
                    format!("tc_session:{}", session.session_id),
                )
            })
            .collect(),
    )
    .await
}

async fn handle_abort_command(
    config: &Config,
    abort_sender: SharedAbort,
    chat_id: &str,
) -> Result<()> {
    let sender = abort_sender.lock().expect("abort mutex poisoned").take();
    if let Some(sender) = sender {
        let _ = sender.send(());
        send_markdown_text_to_chat(config, chat_id, "Aborting active chat turn").await
    } else {
        send_markdown_text_to_chat(config, chat_id, "No active chat turn to abort").await
    }
}

async fn handle_bare_chat_text(
    config: &Config,
    abort_sender: SharedAbort,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> Result<()> {
    let state = load_recovered_chat_state(config).await?;
    match state.state {
        ChatStateValue::ActiveIdle => {}
        ChatStateValue::ActiveBusy | ChatStateValue::Aborting => {
            return send_markdown_text_to_chat(config, chat_id, "Chat bridge is busy").await;
        }
        ChatStateValue::SelectingNew | ChatStateValue::SelectingSession => {
            return send_markdown_text_to_chat(config, chat_id, "Choose from the menu first").await;
        }
        ChatStateValue::Inactive => return Ok(()),
    }
    let runner = state
        .active_runner
        .clone()
        .context("active chat session has no runner")?;
    let session_id = state
        .active_session_id
        .clone()
        .context("active chat session has no session id")?;

    set_check_reaction(config, chat_id, message_id).await.ok();
    mark_turn_started(config, format!("telegram:{message_id}")).await?;
    let (sender, receiver) = oneshot::channel();
    *abort_sender.lock().expect("abort mutex poisoned") = Some(sender);
    let config = config.clone();
    let chat_id = chat_id.to_string();
    let text = text.to_string();
    let abort_sender_for_task = abort_sender.clone();
    tokio::spawn(async move {
        if let Err(err) = run_telegram_chat_turn(
            config.clone(),
            chat_id.clone(),
            runner,
            session_id,
            text,
            receiver,
        )
        .await
        {
            warn!("Telegram chat turn failed: {err:#}");
            if nonresumable_chat_error(&err) {
                let _ = mark_chat_inactive(&config, format!("{err:#}")).await;
            } else {
                let _ = mark_turn_finished(&config, None, Some(format!("{err:#}"))).await;
            }
            let _ = send_markdown_text_to_chat(
                &config,
                &chat_id,
                &format!(
                    "Chat turn failed:\n{}",
                    code_block(&truncate(&format!("{err:#}"), 1200))
                ),
            )
            .await;
        }
        *abort_sender_for_task.lock().expect("abort mutex poisoned") = None;
    });
    Ok(())
}

async fn handle_callback(config: &Config, callback: &TelegramCallbackQuery) -> Result<()> {
    answer_callback_query(config, &callback.id, None).await?;
    let message = callback
        .message
        .as_ref()
        .context("callback query has no message")?;
    let chat_id = message.chat.id.to_string();
    let data = callback.data.as_deref().unwrap_or_default();
    if let Some(runner) = data.strip_prefix("tc_new:") {
        return handle_new_callback(config, &chat_id, runner).await;
    }
    if let Some(session_id) = data.strip_prefix("tc_session:") {
        return handle_session_callback(config, &chat_id, session_id).await;
    }
    send_markdown_text_to_chat(config, &chat_id, "Stale TinyButler selection").await
}

async fn handle_new_callback(config: &Config, chat_id: &str, runner: &str) -> Result<()> {
    let agent_config = config
        .code_agents
        .get(runner)
        .with_context(|| format!("missing code_agents.{runner}"))?;
    if !codex_streaming_runner_keys(config).contains(&runner.to_string()) {
        return send_markdown_text_to_chat(config, chat_id, "Runner no longer supports chat").await;
    }
    if !claim_telegram_selection(
        config,
        ChatStateValue::SelectingNew,
        format!("callback:new:{runner}"),
    )
    .await?
    {
        return send_markdown_text_to_chat(config, chat_id, "Stale model selection").await;
    }
    let session_result = async {
        let mut agent = CodexChatAgent::connect(
            runner.to_string(),
            agent_config,
            Some(std::env::current_dir()?),
        )
        .await?;
        agent.start_session().await
    }
    .await;
    let session = match session_result {
        Ok(session) => session,
        Err(err) => {
            let _ = mark_turn_finished(config, None, Some(format!("{err:#}"))).await;
            return Err(err);
        }
    };
    record_telegram_session(config, session.clone()).await?;
    send_markdown_text_to_chat(
        config,
        chat_id,
        &format!(
            "Started {} session {}",
            inline_code(&session.runner),
            inline_code(&session.session_id)
        ),
    )
    .await
}

async fn handle_session_callback(config: &Config, chat_id: &str, session_id: &str) -> Result<()> {
    let state = load_recovered_chat_state(config).await?;
    if state.state != ChatStateValue::SelectingSession {
        return send_markdown_text_to_chat(config, chat_id, "Stale session selection").await;
    }
    let session = state
        .sessions
        .iter()
        .find(|session| session.session_id == session_id)
        .cloned()
        .with_context(|| format!("unknown chat session {session_id}"))?;
    let agent_config = config
        .code_agents
        .get(&session.runner)
        .with_context(|| format!("missing code_agents.{}", session.runner))?;
    if !claim_telegram_selection(
        config,
        ChatStateValue::SelectingSession,
        format!("callback:session:{session_id}"),
    )
    .await?
    {
        return send_markdown_text_to_chat(config, chat_id, "Stale session selection").await;
    }
    let session_result = async {
        let mut agent = CodexChatAgent::connect(
            session.runner.clone(),
            agent_config,
            Some(std::env::current_dir()?),
        )
        .await?;
        agent.resume_session(session_id).await
    }
    .await;
    let session = match session_result {
        Ok(session) => session,
        Err(err) => {
            let _ = mark_turn_finished(config, None, Some(format!("{err:#}"))).await;
            return Err(err);
        }
    };
    record_telegram_session(config, session.clone()).await?;
    send_markdown_text_to_chat(
        config,
        chat_id,
        &format!(
            "Resumed {} session {}",
            inline_code(&session.runner),
            inline_code(&session.session_id)
        ),
    )
    .await
}

async fn run_telegram_chat_turn(
    config: Config,
    chat_id: String,
    runner: String,
    session_id: String,
    text: String,
    mut abort_receiver: oneshot::Receiver<()>,
) -> Result<()> {
    let agent_config = config
        .code_agents
        .get(&runner)
        .with_context(|| format!("missing code_agents.{runner}"))?;
    let mut agent =
        CodexChatAgent::connect(runner.clone(), agent_config, Some(std::env::current_dir()?))
            .await?;
    if let Err(err) = agent.resume_session(&session_id).await {
        if codex_resume_missing_rollout(&err) {
            agent.start_session().await?;
        } else {
            return Err(err);
        }
    }
    agent.send_turn(&text).await?;

    let placeholder = send_markdown_text_to_chat_with_id(&config, &chat_id, "Working").await?;
    let typing_done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let typing_done_task = typing_done.clone();
    let typing_config = config.clone();
    let typing_chat = chat_id.clone();
    tokio::spawn(async move {
        while !typing_done_task.load(std::sync::atomic::Ordering::Relaxed) {
            let _ = send_chat_action(&typing_config, &typing_chat, "typing").await;
            sleep(Duration::from_secs(4)).await;
        }
    });

    let mut output = String::new();
    let mut last_edit = Instant::now();
    let mut aborted = false;
    loop {
        tokio::select! {
            _ = &mut abort_receiver, if !aborted => {
                aborted = true;
                crate::chat::mark_turn_aborting(&config).await?;
                if let Err(err) = agent.abort_turn().await {
                    mark_turn_finished(&config, agent.active_session(), Some(format!("{err:#}"))).await?;
                    return Err(err);
                }
            }
            event = agent.next_event() => {
                match event? {
                    Some(ChatEvent::AssistantDelta(delta) | ChatEvent::ToolDelta(delta)) => {
                        output.push_str(&delta);
                        if last_edit.elapsed() >= Duration::from_millis(1200) {
                            edit_or_send_markdown(&config, &chat_id, placeholder, &telegram_output_preview(&output)).await?;
                            last_edit = Instant::now();
                        }
                    }
                    Some(ChatEvent::Info(_)) => {}
                    Some(ChatEvent::Warning(warning)) => {
                        output.push_str("\n[warning] ");
                        output.push_str(&warning);
                    }
                    Some(ChatEvent::ApprovalRequired(request)) => {
                        let error = format!("chat turn requires unsupported approval: {request}");
                        typing_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        mark_turn_finished(&config, agent.active_session(), Some(error.clone())).await?;
                        bail!("{error}");
                    }
                    Some(ChatEvent::TurnCompleted) => {
                        typing_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        let final_text = if output.trim().is_empty() {
                            if aborted { "Aborted".to_string() } else { "(no output)".to_string() }
                        } else {
                            output.clone()
                        };
                        send_telegram_output(&config, &chat_id, placeholder, &final_text).await?;
                        mark_turn_finished(&config, agent.active_session(), None).await?;
                        return Ok(());
                    }
                    Some(ChatEvent::TurnFailed(error)) => {
                        typing_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        mark_turn_finished(&config, agent.active_session(), Some(error.clone())).await?;
                        bail!("chat turn failed: {error}");
                    }
                    None => {
                        typing_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        mark_turn_finished(&config, agent.active_session(), Some("chat agent closed".to_string())).await?;
                        bail!("chat agent closed");
                    }
                }
            }
        }
    }
}

async fn set_telegram_selection_state(config: &Config, state_value: ChatStateValue) -> Result<()> {
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
    state.state = state_value;
    state.last_error = None;
    state.save_atomic(&config.chat_state_path()).await
}

async fn claim_telegram_selection(
    config: &Config,
    expected: ChatStateValue,
    request_id: String,
) -> Result<bool> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = load_recovered_chat_state(config).await?;
    if state.state != expected {
        return Ok(false);
    }
    state.state = ChatStateValue::ActiveBusy;
    state.current_request_id = Some(request_id);
    state.current_process_id = Some(std::process::id());
    state.busy_since = Some(Local::now());
    state.last_error = None;
    state.save_atomic(&config.chat_state_path()).await?;
    Ok(true)
}

async fn record_telegram_session(config: &Config, session: ChatSession) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = load_recovered_chat_state(config).await?;
    state.record_active_session(session);
    state.save_atomic(&config.chat_state_path()).await
}

async fn poll_once(
    client: &Client,
    config: &Config,
    offset: Option<i64>,
) -> Result<Vec<TelegramUpdate>> {
    let url = telegram_method_url(config, "getUpdates")?;
    let mut payload = json!({
        "timeout": 25,
        "allowed_updates": ["message", "callback_query"],
    });
    if let Some(offset) = offset {
        payload["offset"] = json!(offset);
    }

    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| anyhow!("Telegram getUpdates request failed: {}", err.without_url()))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("Telegram getUpdates failed: {status} {body}"));
    }
    let parsed: TelegramApiResponse<Vec<TelegramUpdate>> = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse Telegram getUpdates response: {body}"))?;
    if !parsed.ok {
        return Err(anyhow!("Telegram getUpdates returned ok=false"));
    }
    Ok(parsed.result)
}

async fn send_media(
    config: &Config,
    method: &str,
    field_name: &'static str,
    path: &Path,
    caption: Option<&str>,
) -> Result<()> {
    let (client, url, chat_id) = telegram_request(config, method)?;
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("failed to read {}", path.display()))?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("upload")
        .to_string();

    let mut form = Form::new()
        .text("chat_id", chat_id.to_string())
        .part(field_name, Part::bytes(bytes).file_name(filename));
    if let Some(caption) = caption.filter(|value| !value.trim().is_empty()) {
        form = form
            .text("caption", sanitize_markdown_v2_message(caption))
            .text("parse_mode", TELEGRAM_PARSE_MODE);
    }

    let response = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .map_err(|err| anyhow!("Telegram {method} request failed: {}", err.without_url()))?;

    ensure_success(response, method).await
}

fn telegram_request(config: &Config, method: &str) -> Result<(Client, String, String)> {
    let chat_id = configured_chat_id(config)?;

    Ok((Client::new(), telegram_method_url(config, method)?, chat_id))
}

async fn send_markdown_text_to_chat(config: &Config, chat_id: &str, text: &str) -> Result<()> {
    send_markdown_text_to_chat_with_id(config, chat_id, text)
        .await
        .map(|_| ())
}

async fn send_markdown_text_to_chat_with_id(
    config: &Config,
    chat_id: &str,
    text: &str,
) -> Result<i64> {
    let client = Client::new();
    let url = telegram_method_url(config, "sendMessage")?;
    let payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
        "parse_mode": TELEGRAM_PARSE_MODE,
    });

    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| anyhow!("Telegram sendMessage request failed: {}", err.without_url()))?;

    parse_message_id_response(response, "sendMessage").await
}

async fn edit_markdown_text_to_chat(
    config: &Config,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> Result<()> {
    let client = Client::new();
    let url = telegram_method_url(config, "editMessageText")?;
    let payload = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "text": text,
        "disable_web_page_preview": true,
        "parse_mode": TELEGRAM_PARSE_MODE,
    });
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            anyhow!(
                "Telegram editMessageText request failed: {}",
                err.without_url()
            )
        })?;

    ensure_success(response, "editMessageText").await
}

async fn edit_or_send_markdown(
    config: &Config,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> Result<()> {
    if let Err(err) = edit_markdown_text_to_chat(config, chat_id, message_id, text).await {
        if telegram_message_not_modified(&err) {
            return Ok(());
        }
        send_markdown_text_to_chat(config, chat_id, text).await?;
    }
    Ok(())
}

async fn send_inline_menu(
    config: &Config,
    chat_id: &str,
    text: &str,
    buttons: Vec<(String, String)>,
) -> Result<()> {
    let client = Client::new();
    let url = telegram_method_url(config, "sendMessage")?;
    let rows = buttons
        .into_iter()
        .map(|(label, data)| vec![json!({ "text": label, "callback_data": data })])
        .collect::<Vec<_>>();
    let payload = json!({
        "chat_id": chat_id,
        "text": escape_markdown_v2(text),
        "parse_mode": TELEGRAM_PARSE_MODE,
        "reply_markup": { "inline_keyboard": rows },
    });
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| anyhow!("Telegram sendMessage request failed: {}", err.without_url()))?;
    ensure_success(response, "sendMessage").await
}

async fn answer_callback_query(
    config: &Config,
    callback_id: &str,
    text: Option<&str>,
) -> Result<()> {
    let client = Client::new();
    let url = telegram_method_url(config, "answerCallbackQuery")?;
    let mut payload = json!({ "callback_query_id": callback_id });
    if let Some(text) = text {
        payload["text"] = json!(text);
    }
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            anyhow!(
                "Telegram answerCallbackQuery request failed: {}",
                err.without_url()
            )
        })?;
    ensure_success(response, "answerCallbackQuery").await
}

async fn send_chat_action(config: &Config, chat_id: &str, action: &str) -> Result<()> {
    let client = Client::new();
    let url = telegram_method_url(config, "sendChatAction")?;
    let payload = json!({
        "chat_id": chat_id,
        "action": action,
    });
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            anyhow!(
                "Telegram sendChatAction request failed: {}",
                err.without_url()
            )
        })?;
    ensure_success(response, "sendChatAction").await
}

async fn set_check_reaction(config: &Config, chat_id: &str, message_id: i64) -> Result<()> {
    let client = Client::new();
    let url = telegram_method_url(config, "setMessageReaction")?;
    let payload = json!({
        "chat_id": chat_id,
        "message_id": message_id,
        "reaction": [{ "type": "emoji", "emoji": "✅" }],
    });
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            anyhow!(
                "Telegram setMessageReaction request failed: {}",
                err.without_url()
            )
        })?;
    ensure_success(response, "setMessageReaction").await
}

fn telegram_method_url(config: &Config, method: &str) -> Result<String> {
    let token = config
        .telegram
        .bot_token
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("missing telegram.bot_token in config.yaml")?;
    Ok(format!("https://api.telegram.org/bot{token}/{method}"))
}

fn configured_chat_id(config: &Config) -> Result<String> {
    config
        .telegram
        .chat_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .context("missing telegram.chat_id in config.yaml")
}

/// Persisted long-polling offset state for Telegram ingress.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct TelegramPollState {
    /// Highest update id written before command execution.
    #[serde(default)]
    pub last_update_id: Option<i64>,
    /// RFC3339 timestamp of the latest successful poll or update record.
    #[serde(default)]
    pub last_poll_at: Option<String>,
}

impl TelegramPollState {
    /// Load Telegram polling state, returning default state when absent.
    pub async fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path).await?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Save Telegram polling state as pretty JSON with a trailing newline.
    pub async fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_string_pretty(self)?;
        fs::write(path, format!("{text}\n")).await?;
        Ok(())
    }

    /// Return the Telegram `getUpdates` offset following the persisted update.
    pub fn next_offset(&self) -> Option<i64> {
        self.last_update_id.map(|id| id + 1)
    }

    /// Record an update before running its command to avoid replay on restart.
    pub fn record_update(&mut self, update_id: i64) {
        self.last_update_id = Some(
            self.last_update_id
                .map_or(update_id, |id| id.max(update_id)),
        );
        self.record_poll();
    }

    /// Refresh the latest successful polling timestamp.
    pub fn record_poll(&mut self) {
        self.last_poll_at = Some(Local::now().to_rfc3339());
    }
}

#[derive(Debug, Deserialize)]
struct TelegramApiResponse<T> {
    ok: bool,
    result: T,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
    callback_query: Option<TelegramCallbackQuery>,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    message_id: i64,
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackQuery {
    id: String,
    message: Option<TelegramCallbackMessage>,
    data: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramCallbackMessage {
    chat: TelegramChat,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

async fn parse_message_id_response(response: reqwest::Response, method: &str) -> Result<i64> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Telegram {method} failed: {status} {body}"));
    }
    let body = response.text().await.unwrap_or_default();
    let parsed: TelegramApiResponse<TelegramSentMessage> = serde_json::from_str(&body)
        .with_context(|| format!("failed to parse Telegram {method} response: {body}"))?;
    if !parsed.ok {
        return Err(anyhow!("Telegram {method} returned ok=false"));
    }
    Ok(parsed.result.message_id)
}

#[derive(Debug, Deserialize)]
struct TelegramSentMessage {
    message_id: i64,
}

async fn ensure_success(response: reqwest::Response, method: &str) -> Result<()> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Telegram {method} failed: {status} {body}"));
    }
    Ok(())
}

async fn send_telegram_output(
    config: &Config,
    chat_id: &str,
    first_message_id: i64,
    text: &str,
) -> Result<()> {
    let chunks = chunk_escaped_markdown_v2(text, 3500);
    if let Some(first) = chunks.first() {
        edit_or_send_markdown(config, chat_id, first_message_id, first).await?;
    }
    for chunk in chunks.into_iter().skip(1) {
        send_markdown_text_to_chat(config, chat_id, &chunk).await?;
    }
    Ok(())
}

fn telegram_output_preview(text: &str) -> String {
    chunk_escaped_markdown_v2(text, 3500)
        .into_iter()
        .next()
        .unwrap_or_default()
}

fn codex_resume_missing_rollout(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("no rollout found for thread id")
}

fn nonresumable_chat_error(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("failed to resume Codex chat session")
        && !text.contains("no rollout found for thread id")
}

fn telegram_message_not_modified(err: &anyhow::Error) -> bool {
    format!("{err:#}").contains("message is not modified")
}

fn chunk_escaped_markdown_v2(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        let escaped = escape_markdown_v2(&ch.to_string());
        if !current.is_empty() && current.chars().count() + escaped.chars().count() > limit {
            chunks.push(current);
            current = String::new();
        }
        current.push_str(&escaped);
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out = text.chars().take(limit).collect::<String>();
    out.push_str("\n...");
    out
}

fn inline_code(text: &str) -> String {
    format!("`{}`", escape_markdown_v2_code(text))
}

/// Escape general user-controlled text for Telegram MarkdownV2.
pub fn escape_markdown_v2(text: &str) -> String {
    const RESERVED: &[char] = &[
        '_', '*', '[', ']', '(', ')', '~', '`', '>', '#', '+', '-', '=', '|', '{', '}', '.', '!',
        '\\',
    ];
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if RESERVED.contains(&ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Make CLI-authored MarkdownV2 safe while preserving simple bold and code spans.
pub fn sanitize_markdown_v2_message(text: &str) -> String {
    let chars = text.chars().collect::<Vec<_>>();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    let mut bold_open = false;

    while index < chars.len() {
        let ch = chars[index];
        if ch == '\\' {
            if let Some(next) = chars.get(index + 1) {
                out.push('\\');
                out.push(*next);
                index += 2;
            } else {
                out.push_str("\\\\");
                index += 1;
            }
            continue;
        }

        if ch == '`' {
            if let Some(end) = chars[index + 1..]
                .iter()
                .position(|candidate| *candidate == '`')
            {
                let end_index = index + 1 + end;
                out.push('`');
                out.push_str(&escape_markdown_v2_code(
                    &chars[index + 1..end_index].iter().collect::<String>(),
                ));
                out.push('`');
                index = end_index + 1;
            } else {
                out.push_str("\\`");
                index += 1;
            }
            continue;
        }

        if ch == '*' {
            if bold_open {
                bold_open = false;
                out.push('*');
            } else if chars[index + 1..].contains(&'*') {
                bold_open = true;
                out.push('*');
            } else {
                out.push_str("\\*");
            }
            index += 1;
            continue;
        }

        out.push_str(&escape_markdown_v2(&ch.to_string()));
        index += 1;
    }

    out
}

fn escape_markdown_v2_code(text: &str) -> String {
    text.replace('\\', "\\\\").replace('`', "\\`")
}

/// Wrap short user-controlled text in a Telegram MarkdownV2 code block.
pub fn code_block(text: &str) -> String {
    format!("```\n{}\n```", escape_markdown_v2_code(text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_escapes_markdown_v2_plain_descriptions() {
        let text = help_text();

        assert!(text.contains("code\\-agent"));
        assert!(!text.contains("code-agent"));
    }

    #[test]
    fn detects_codex_missing_rollout_resume_errors() {
        let err = anyhow::anyhow!("JSON-RPC error (-32600): no rollout found for thread id abc");

        assert!(codex_resume_missing_rollout(&err));
    }

    #[test]
    fn treats_telegram_message_not_modified_as_successful_edit() {
        let err = anyhow::anyhow!(
            "Telegram editMessageText failed: 400 Bad Request message is not modified"
        );

        assert!(telegram_message_not_modified(&err));
    }

    #[test]
    fn detects_nonresumable_chat_errors() {
        let missing_rollout = anyhow::anyhow!(
            "failed to resume Codex chat session x: no rollout found for thread id x"
        );
        let other_resume = anyhow::anyhow!("failed to resume Codex chat session x: JSON-RPC error");

        assert!(!nonresumable_chat_error(&missing_rollout));
        assert!(nonresumable_chat_error(&other_resume));
    }

    #[test]
    fn chunks_output_after_markdown_v2_escaping() {
        let chunks = chunk_escaped_markdown_v2(&"_".repeat(10), 5);

        assert_eq!(chunks, vec![r"\_\_".to_string(); 5]);
    }
}
