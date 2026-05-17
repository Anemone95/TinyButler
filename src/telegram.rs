//! Telegram Bot API sender and minimal long-polling ingress.
//!
//! Outbound sending is used by task notifications and agent calls. The MVP
//! ingress path uses direct Bot API long polling for the task-management slash
//! commands and can later be replaced by a richer teloxide workflow.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use telegram_markdown_v2::{convert_with_strategy, UnsupportedTagsStrategy};
use tokio::fs;
use tokio::sync::oneshot;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::chat::{
    chat_working_directory, codex_streaming_runner_keys, load_recovered_chat_state,
    mark_chat_inactive, mark_turn_finished, mark_turn_started, ChatAgent, ChatEvent,
    ChatInstructionContext, ChatLock, ChatSession, ChatStateValue, CodexChatAgent,
};
use crate::config::Config;
use crate::scheduler::Scheduler;

/// Telegram parse mode used by TinyButler-generated and CLI-authored messages.
pub const TELEGRAM_PARSE_MODE: &str = "MarkdownV2";

/// Build the daemon startup notification sent when Telegram is configured.
pub fn daemon_startup_notification_text(config: &Config) -> String {
    format!(
        "**TinyButler daemon restarted**\n**home:** {}",
        inline_code(&config.home.display().to_string())
    )
}

/// Parsed Telegram command supported by TinyButler ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressCommand {
    Help,
    New,
    Session,
    Abort,
    Tasks,
    Unknown(String),
}

type AbortSender = oneshot::Sender<()>;
type SharedAbort = Arc<Mutex<Option<AbortSender>>>;

/// Send an ordinary Markdown text message to the configured chat.
pub async fn send_text(config: &Config, text: &str) -> Result<()> {
    let chat_id = configured_chat_id(config)?;
    send_markdown_text_to_chat(config, &chat_id, text).await
}

/// Send a local file using the Telegram attachment method that best fits its type.
pub async fn send_attachment(config: &Config, path: &Path, caption: Option<&str>) -> Result<()> {
    let chat_id = configured_chat_id(config)?;
    let attachment = classify_attachment(path);
    send_media_to_chat(
        config,
        attachment.method,
        attachment.field_name,
        &chat_id,
        path,
        caption,
    )
    .await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TelegramAttachment {
    method: &'static str,
    field_name: &'static str,
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
        "**TinyButler task:** {}\n**status:** {}\n**duration:** {}\n**log:** {}",
        inline_code(task_name),
        inline_code(status),
        inline_code(&format!("{duration_seconds}s")),
        inline_code(log_path)
    );
    if let Some(code) = exit_code {
        text.push_str(&format!(
            "\n**exit code:** {}",
            inline_code(&code.to_string())
        ));
    }
    if !summary.trim().is_empty() {
        text.push_str("\n\n**summary:**\n");
        text.push_str(&truncate(summary.trim(), 1200));
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
    if let Err(err) = sync_bot_menu(&client, &config, &allowed_chat_id).await {
        warn!("failed to sync Telegram bot menu: {err:#}");
    }
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
                                            &format!("TinyButler command failed:\n{}", markdown_code_block(&truncate(&format!("{err:#}"), 1200))),
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

    let raw_command = trimmed.split_whitespace().next().unwrap_or_default();
    if !raw_command.starts_with('/') {
        return IngressCommand::Unknown(trimmed.to_string());
    }
    let command = raw_command
        .trim_start_matches('/')
        .split('@')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    match command.as_str() {
        "help" | "start" => IngressCommand::Help,
        "new" => IngressCommand::New,
        "session" => IngressCommand::Session,
        "abort" => IngressCommand::Abort,
        "tasks" => IngressCommand::Tasks,
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
        IngressCommand::Tasks => {
            return handle_tasks_command(config, chat_id).await;
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
        "**TinyButler commands:**".to_string(),
        format!(
            "{}: {}",
            inline_code("/new"),
            "start a code-agent chat session"
        ),
        format!(
            "{}: {}",
            inline_code("/session"),
            "resume a code-agent chat session"
        ),
        format!(
            "{}: {}",
            inline_code("/abort"),
            "abort the active code-agent turn"
        ),
        format!("{}: {}", inline_code("/tasks"), "open the task selector"),
        format!("{}: {}", inline_code("/help"), "show this help"),
    ]
    .join("\n")
}

async fn handle_tasks_command(config: &Config, chat_id: &str) -> Result<()> {
    let scheduler = Scheduler::new(config.clone());
    let tasks = scheduler.task_selector_items().await?;
    if tasks.is_empty() {
        return send_markdown_text_to_chat(config, chat_id, "No tasks found.").await;
    }
    send_task_index_menu(config, chat_id, "Select a task:", &tasks).await
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

async fn handle_task_callback(config: &Config, chat_id: &str, task_data: &str) -> Result<()> {
    let scheduler = Scheduler::new(config.clone());
    let selection = parse_task_callback_data(task_data)?;
    let index = selection.index;
    let task_name = task_name_for_callback_selection(&scheduler, selection).await?;
    send_task_detail_menu(config, chat_id, &scheduler, index, &task_name).await
}

async fn handle_task_action_callback(
    config: &Config,
    chat_id: &str,
    action_data: &str,
) -> Result<()> {
    let Some((action, task_data)) = action_data.split_once(':') else {
        return send_markdown_text_to_chat(config, chat_id, "Stale TinyButler selection").await;
    };
    let scheduler = Scheduler::new(config.clone());
    let selection = parse_task_callback_data(task_data)?;
    let index = selection.index;
    let task_name = task_name_for_callback_selection(&scheduler, selection).await?;

    match action {
        "status" => {
            let text = scheduler.task_status_text(&task_name).await?;
            send_task_action_menu(config, chat_id, &scheduler, index, &task_name, &text).await
        }
        "run" => {
            send_markdown_text_to_chat(
                config,
                chat_id,
                &format!("Running {}", inline_code(&task_name)),
            )
            .await?;
            let outcome = scheduler.run_task_by_name(&task_name).await?;
            let text = format!(
                "Task {} finished with {} in {}\n\n{}",
                inline_code(&task_name),
                inline_code(&outcome.status),
                inline_code(&format!("{}s", outcome.duration_seconds)),
                truncate(outcome.summary.trim(), 2500)
            );
            send_task_action_menu(config, chat_id, &scheduler, index, &task_name, &text).await
        }
        "enable" => {
            scheduler.set_task_enabled_by_name(&task_name, true).await?;
            send_task_detail_menu(config, chat_id, &scheduler, index, &task_name).await
        }
        "disable" => {
            scheduler
                .set_task_enabled_by_name(&task_name, false)
                .await?;
            send_task_detail_menu(config, chat_id, &scheduler, index, &task_name).await
        }
        "exit" => send_markdown_text_to_chat(config, chat_id, "Exited task selector.").await,
        _ => send_markdown_text_to_chat(config, chat_id, "Stale TinyButler selection").await,
    }
}

async fn task_name_for_callback_selection(
    scheduler: &Scheduler,
    selection: TaskCallbackSelection,
) -> Result<String> {
    let tasks = scheduler.task_selector_items().await?;
    let task_name = tasks
        .get(selection.index)
        .cloned()
        .context("stale task selection")?;
    if task_selector_fingerprint(&task_name) != selection.fingerprint {
        bail!("stale task selection");
    }
    Ok(task_name)
}

async fn send_task_index_menu(
    config: &Config,
    chat_id: &str,
    text: &str,
    tasks: &[String],
) -> Result<()> {
    send_inline_menu(
        config,
        chat_id,
        text,
        tasks
            .iter()
            .enumerate()
            .map(|(index, task)| {
                (
                    task.clone(),
                    format!("tb_task:{index}:{}", task_selector_fingerprint(task)),
                )
            })
            .collect(),
    )
    .await
}

async fn send_task_detail_menu(
    config: &Config,
    chat_id: &str,
    scheduler: &Scheduler,
    task_index: usize,
    task_name: &str,
) -> Result<()> {
    let text = scheduler.task_detail_text(task_name).await?;
    send_task_action_menu(config, chat_id, scheduler, task_index, task_name, &text).await
}

async fn send_task_action_menu(
    config: &Config,
    chat_id: &str,
    scheduler: &Scheduler,
    task_index: usize,
    task_name: &str,
    text: &str,
) -> Result<()> {
    let buttons = task_action_buttons(scheduler, task_index, task_name).await?;
    if inline_menu_text_fits(text) {
        return send_inline_menu(config, chat_id, text, buttons).await;
    }

    send_markdown_text_to_chat(config, chat_id, text).await?;
    send_inline_menu(
        config,
        chat_id,
        &format!("Choose an action for {}:", inline_code(task_name)),
        buttons,
    )
    .await
}

async fn task_action_buttons(
    scheduler: &Scheduler,
    task_index: usize,
    task_name: &str,
) -> Result<Vec<(String, String)>> {
    Ok(scheduler
        .task_selector_action_labels(task_name)
        .await?
        .into_iter()
        .map(|action| {
            (
                action.clone(),
                format!(
                    "tb_task_action:{action}:{task_index}:{}",
                    task_selector_fingerprint(task_name)
                ),
            )
        })
        .collect())
}

fn inline_menu_text_fits(text: &str) -> bool {
    render_markdown_v2(text).chars().count() <= 3500
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskCallbackSelection {
    index: usize,
    fingerprint: String,
}

fn parse_task_callback_data(data: &str) -> Result<TaskCallbackSelection> {
    let Some((index_text, fingerprint)) = data.split_once(':') else {
        bail!("invalid task selection callback");
    };
    let index = index_text
        .parse::<usize>()
        .with_context(|| format!("invalid task selection index: {index_text}"))?;
    if !valid_task_fingerprint(fingerprint) {
        bail!("invalid task selection fingerprint");
    }
    Ok(TaskCallbackSelection {
        index,
        fingerprint: fingerprint.to_string(),
    })
}

fn task_selector_fingerprint(task_name: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in task_name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn valid_task_fingerprint(value: &str) -> bool {
    value.len() == 16 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
                    markdown_code_block(&truncate(&format!("{err:#}"), 1200))
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
    if let Some(task_data) = data.strip_prefix("tb_task:") {
        return handle_task_callback(config, &chat_id, task_data).await;
    }
    if let Some(action_data) = data.strip_prefix("tb_task_action:") {
        return handle_task_action_callback(config, &chat_id, action_data).await;
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
        let mut agent = CodexChatAgent::connect_with_context(
            runner.to_string(),
            agent_config,
            Some(chat_working_directory(config)),
            ChatInstructionContext::Telegram,
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
        let mut agent = CodexChatAgent::connect_with_context(
            session.runner.clone(),
            agent_config,
            Some(chat_working_directory(config)),
            ChatInstructionContext::Telegram,
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
    let working_directory = chat_working_directory(&config);
    let mut agent = CodexChatAgent::connect_with_context(
        runner.clone(),
        agent_config,
        Some(working_directory.clone()),
        ChatInstructionContext::Telegram,
    )
    .await?;
    if let Err(err) = agent.resume_session_for_turn(&session_id).await {
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

    let mut assistant_output = String::new();
    let mut tool_output = String::new();
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
                    Some(ChatEvent::AssistantDelta(delta)) => {
                        assistant_output.push_str(&delta);
                        if last_edit.elapsed() >= Duration::from_millis(1200) {
                            let preview = telegram_output_preview(&assistant_visible_text(&assistant_output));
                            if !preview.trim().is_empty() {
                                edit_or_send_markdown_v2(&config, &chat_id, placeholder, &preview).await?;
                            }
                            last_edit = Instant::now();
                        }
                    }
                    Some(ChatEvent::ToolDelta(delta)) => {
                        tool_output.push_str(&delta);
                    }
                    Some(ChatEvent::Info(_)) => {}
                    Some(ChatEvent::Warning(warning)) => {
                        tool_output.push_str("\n[warning] ");
                        tool_output.push_str(&warning);
                    }
                    Some(ChatEvent::ApprovalRequired(request)) => {
                        let error = format!("chat turn requires unsupported approval: {request}");
                        typing_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        mark_turn_finished(&config, agent.active_session(), Some(error.clone())).await?;
                        bail!("{error}");
                    }
                    Some(ChatEvent::TurnCompleted) => {
                        typing_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        let mut final_text = assistant_visible_text(&assistant_output);
                        let mut attachment_paths =
                            extract_outbound_attachment_paths(&assistant_output, &working_directory);
                        final_text = remove_attachment_markers(&final_text).trim().to_string();
                        extend_unique_paths(
                            &mut attachment_paths,
                            extract_plain_image_paths(&final_text, &working_directory),
                        );
                        if final_text.trim().is_empty() {
                            if aborted {
                                final_text = "Aborted".to_string();
                            } else if attachment_paths.is_empty() {
                                final_text = "(no output)".to_string();
                            } else {
                                final_text = "Sent attachment.".to_string();
                            }
                        }
                        send_telegram_output(&config, &chat_id, placeholder, &final_text).await?;
                        send_outbound_attachment_paths(&config, &chat_id, &attachment_paths, &final_text).await?;
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

async fn sync_bot_menu(client: &Client, config: &Config, chat_id: &str) -> Result<()> {
    set_bot_commands(client, config, None).await?;
    set_bot_commands(
        client,
        config,
        Some(json!({ "type": "chat", "chat_id": chat_id })),
    )
    .await
}

async fn set_bot_commands(
    client: &Client,
    config: &Config,
    scope: Option<serde_json::Value>,
) -> Result<()> {
    let url = telegram_method_url(config, "setMyCommands")?;
    let mut payload = json!({
        "commands": bot_menu_commands(),
    });
    if let Some(scope) = scope {
        payload["scope"] = scope;
    }
    let response = client
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| {
            anyhow!(
                "Telegram setMyCommands request failed: {}",
                err.without_url()
            )
        })?;
    ensure_success(response, "setMyCommands").await
}

fn bot_menu_commands() -> Vec<TelegramBotCommand> {
    vec![
        TelegramBotCommand::new("new", "New chat"),
        TelegramBotCommand::new("session", "Resume chat"),
        TelegramBotCommand::new("abort", "Abort turn"),
        TelegramBotCommand::new("tasks", "Task selector"),
        TelegramBotCommand::new("help", "Help"),
    ]
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct TelegramBotCommand {
    command: String,
    description: String,
}

impl TelegramBotCommand {
    fn new(command: &str, description: &str) -> Self {
        Self {
            command: command.to_string(),
            description: description.to_string(),
        }
    }
}

async fn send_media_to_chat(
    config: &Config,
    method: &str,
    field_name: &'static str,
    chat_id: &str,
    path: &Path,
    caption: Option<&str>,
) -> Result<()> {
    let client = Client::new();
    let url = telegram_method_url(config, method)?;
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
            .text("caption", render_markdown_v2(caption))
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

fn classify_attachment(path: &Path) -> TelegramAttachment {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    match extension.as_deref() {
        Some("png" | "jpg" | "jpeg" | "webp") => TelegramAttachment {
            method: "sendPhoto",
            field_name: "photo",
        },
        Some("gif") => TelegramAttachment {
            method: "sendAnimation",
            field_name: "animation",
        },
        Some("mp4" | "m4v") => TelegramAttachment {
            method: "sendVideo",
            field_name: "video",
        },
        Some("mp3" | "m4a") => TelegramAttachment {
            method: "sendAudio",
            field_name: "audio",
        },
        _ => TelegramAttachment {
            method: "sendDocument",
            field_name: "document",
        },
    }
}

async fn send_markdown_text_to_chat(config: &Config, chat_id: &str, text: &str) -> Result<()> {
    let mut chunks = chunk_markdown_v2(text, 3500).into_iter();
    if let Some(first) = chunks.next() {
        send_markdown_v2_to_chat_with_id(config, chat_id, &first).await?;
    }
    for chunk in chunks {
        send_markdown_v2_to_chat(config, chat_id, &chunk).await?;
    }
    Ok(())
}

async fn send_markdown_text_to_chat_with_id(
    config: &Config,
    chat_id: &str,
    text: &str,
) -> Result<i64> {
    let mut chunks = chunk_markdown_v2(text, 3500).into_iter();
    let first = chunks.next().unwrap_or_default();
    let message_id = send_markdown_v2_to_chat_with_id(config, chat_id, &first).await?;
    for chunk in chunks {
        send_markdown_v2_to_chat(config, chat_id, &chunk).await?;
    }
    Ok(message_id)
}

async fn send_markdown_v2_to_chat(config: &Config, chat_id: &str, text: &str) -> Result<()> {
    send_markdown_v2_to_chat_with_id(config, chat_id, text)
        .await
        .map(|_| ())
}

async fn send_markdown_v2_to_chat_with_id(
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

async fn edit_markdown_v2_to_chat(
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

async fn edit_or_send_markdown_v2(
    config: &Config,
    chat_id: &str,
    message_id: i64,
    text: &str,
) -> Result<()> {
    if let Err(err) = edit_markdown_v2_to_chat(config, chat_id, message_id, text).await {
        if telegram_message_not_modified(&err) {
            return Ok(());
        }
        send_markdown_v2_to_chat(config, chat_id, text).await?;
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
        "text": render_markdown_v2(text),
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

async fn send_outbound_attachment_paths(
    config: &Config,
    chat_id: &str,
    paths: &[PathBuf],
    _visible_text: &str,
) -> Result<()> {
    for path in paths {
        if !is_supported_attachment_path(path) {
            continue;
        }
        let attachment = classify_attachment(path);
        send_media_to_chat(
            config,
            attachment.method,
            attachment.field_name,
            chat_id,
            path,
            None,
        )
        .await?;
    }
    Ok(())
}

fn assistant_visible_text(text: &str) -> String {
    transform_outside_fenced_code(text, sanitize_visible_segment)
}

fn sanitize_visible_segment(segment: &str) -> String {
    let final_content =
        extract_tag_content(segment, "final").unwrap_or_else(|| segment.to_string());
    strip_final_tags(&strip_reasoning_blocks(&final_content))
        .trim()
        .to_string()
}

fn extract_outbound_attachment_paths(text: &str, working_directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence_boundary(line) {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        let trimmed = line.trim_start();
        let Some(raw) = strip_ascii_prefix(trimmed, "ATTACH:") else {
            continue;
        };
        let Some(path) = resolve_attachment_path(raw, working_directory) else {
            continue;
        };
        if !paths.iter().any(|known| known == &path) {
            paths.push(path);
        }
    }
    paths
}

fn extract_plain_image_paths(text: &str, working_directory: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for raw in text.split_whitespace() {
        let Some(path) = resolve_attachment_path(raw, working_directory) else {
            continue;
        };
        if !is_supported_plain_image_path(&path) {
            continue;
        }
        if !paths.iter().any(|known| known == &path) {
            paths.push(path);
        }
    }
    paths
}

fn extend_unique_paths(paths: &mut Vec<PathBuf>, extra_paths: Vec<PathBuf>) {
    for path in extra_paths {
        if !paths.iter().any(|known| known == &path) {
            paths.push(path);
        }
    }
}

fn remove_attachment_markers(text: &str) -> String {
    let mut out = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        if is_fence_boundary(line) {
            in_fence = !in_fence;
            out.push(line);
            continue;
        }
        if !in_fence && strip_ascii_prefix(line.trim_start(), "ATTACH:").is_some() {
            continue;
        }
        out.push(line);
    }
    out.join("\n")
}

fn resolve_attachment_path(raw: &str, working_directory: &Path) -> Option<PathBuf> {
    let candidate = normalize_attachment_candidate(raw)?;
    if candidate.starts_with("http://")
        || candidate.starts_with("https://")
        || candidate.starts_with("file://")
    {
        return None;
    }
    let expanded = if let Some(rest) = candidate.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|home| home.join(rest))?
    } else {
        PathBuf::from(candidate)
    };
    let path = if expanded.is_absolute() {
        expanded
    } else {
        working_directory.join(expanded)
    };
    if !is_supported_attachment_path(&path) || !path.is_file() {
        return None;
    }
    std::fs::canonicalize(&path).ok().or(Some(path))
}

fn normalize_attachment_candidate(raw: &str) -> Option<String> {
    let mut value = raw.trim();
    if value.is_empty() {
        return None;
    }
    if let Some(quoted) = value.strip_prefix('"') {
        value = quoted.split('"').next().unwrap_or(quoted);
    } else if let Some(quoted) = value.strip_prefix('\'') {
        value = quoted.split('\'').next().unwrap_or(quoted);
    } else if let Some(quoted) = value.strip_prefix('`') {
        value = quoted.split('`').next().unwrap_or(quoted);
    } else if let Some(first) = value.split_whitespace().next() {
        value = first;
    }
    let cleaned = value.trim_matches(|ch: char| {
        matches!(
            ch,
            '"' | '\'' | '`' | ',' | ';' | ')' | ']' | '}' | '<' | '>'
        )
    });
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

fn is_supported_attachment_path(path: &Path) -> bool {
    path.is_file()
}

fn is_supported_plain_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp" | "gif"
            )
        })
        .unwrap_or(false)
        && path.is_file()
}

fn transform_outside_fenced_code(text: &str, transform: fn(&str) -> String) -> String {
    let mut output = String::new();
    let mut segment = String::new();
    let mut code = String::new();
    let mut in_fence = false;

    for line in text.split_inclusive('\n') {
        if is_fence_boundary(line) {
            if in_fence {
                code.push_str(line);
                output.push_str(&code);
                code.clear();
                in_fence = false;
            } else {
                let transformed = transform(&segment);
                output.push_str(&transformed);
                if !transformed.is_empty() && !transformed.ends_with('\n') {
                    output.push('\n');
                }
                segment.clear();
                code.push_str(line);
                in_fence = true;
            }
            continue;
        }

        if in_fence {
            code.push_str(line);
        } else {
            segment.push_str(line);
        }
    }

    if in_fence {
        output.push_str(&code);
    }
    output.push_str(&transform(&segment));
    output
}

fn is_fence_boundary(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("```") || trimmed.starts_with("~~~")
}

fn strip_ascii_prefix<'a>(value: &'a str, prefix: &str) -> Option<&'a str> {
    value
        .get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &value[prefix.len()..])
}

#[derive(Debug, Clone)]
struct SimpleTag {
    start: usize,
    end: usize,
    name: String,
}

fn extract_tag_content(text: &str, name: &str) -> Option<String> {
    let open = find_tag(text, 0, &[name], false)?;
    let close = find_tag(text, open.end, &[name], true)?;
    Some(text[open.end..close.start].to_string())
}

fn strip_final_tags(text: &str) -> String {
    strip_tag_markers(text, &["final"])
}

fn strip_tag_markers(text: &str, names: &[&str]) -> String {
    let mut cleaned = text.to_string();
    while let Some(tag) =
        find_tag(&cleaned, 0, names, false).or_else(|| find_tag(&cleaned, 0, names, true))
    {
        cleaned.replace_range(tag.start..tag.end, "");
    }
    cleaned
}

fn strip_reasoning_blocks(text: &str) -> String {
    let mut cleaned = text.to_string();
    let names = ["think", "thinking", "thought", "antthinking"];
    while let Some(open) = find_tag(&cleaned, 0, &names, false) {
        if let Some(close) = find_tag(&cleaned, open.end, &[open.name.as_str()], true) {
            cleaned.replace_range(open.start..close.end, "");
        } else {
            cleaned.truncate(open.start);
            break;
        }
    }
    strip_tag_markers(&cleaned, &names)
}

fn find_tag(text: &str, from: usize, names: &[&str], closing: bool) -> Option<SimpleTag> {
    let lower = text.to_ascii_lowercase();
    let mut search = from.min(lower.len());
    while let Some(relative) = lower[search..].find('<') {
        let start = search + relative;
        let mut index = start + 1;
        skip_ascii_whitespace(&lower, &mut index);

        let is_closing = lower[index..].starts_with('/');
        if is_closing {
            index += 1;
            skip_ascii_whitespace(&lower, &mut index);
        }
        if is_closing != closing {
            search = start + 1;
            continue;
        }

        if lower[index..].starts_with("antml:") {
            index += "antml:".len();
        }

        for name in names {
            if lower[index..].starts_with(name) && tag_name_boundary(&lower, index + name.len()) {
                let end = lower[index..]
                    .find('>')
                    .map(|relative_end| index + relative_end + 1)?;
                return Some(SimpleTag {
                    start,
                    end,
                    name: (*name).to_string(),
                });
            }
        }
        search = start + 1;
    }
    None
}

fn skip_ascii_whitespace(value: &str, index: &mut usize) {
    while value
        .as_bytes()
        .get(*index)
        .is_some_and(u8::is_ascii_whitespace)
    {
        *index += 1;
    }
}

fn tag_name_boundary(value: &str, index: usize) -> bool {
    value
        .as_bytes()
        .get(index)
        .map(|byte| byte.is_ascii_whitespace() || matches!(byte, b'>' | b'/'))
        .unwrap_or(true)
}

async fn send_telegram_output(
    config: &Config,
    chat_id: &str,
    first_message_id: i64,
    text: &str,
) -> Result<()> {
    let chunks = chunk_markdown_v2(text, 3500);
    if let Some(first) = chunks.first() {
        edit_or_send_markdown_v2(config, chat_id, first_message_id, first).await?;
    }
    for chunk in chunks.into_iter().skip(1) {
        send_markdown_v2_to_chat(config, chat_id, &chunk).await?;
    }
    Ok(())
}

fn telegram_output_preview(text: &str) -> String {
    chunk_markdown_v2(text, 3500)
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

fn chunk_markdown_v2(text: &str, limit: usize) -> Vec<String> {
    if text.is_empty() {
        return vec![String::new()];
    }

    let limit = limit.max(1);
    let mut chunks = Vec::new();
    let mut current = String::new();

    for block in markdown_blocks(text) {
        let candidate = format!("{current}{block}");
        if render_markdown_v2(&candidate).chars().count() <= limit {
            current = candidate;
            continue;
        }

        if !current.is_empty() {
            chunks.push(render_markdown_v2(&current));
            current.clear();
        }

        if render_markdown_v2(&block).chars().count() <= limit {
            current = block;
        } else {
            chunks.extend(chunk_escaped_markdown_v2(&block, limit));
        }
    }

    if !current.is_empty() {
        chunks.push(render_markdown_v2(&current));
    }
    if chunks.is_empty() {
        vec![render_markdown_v2(text)]
    } else {
        chunks
    }
}

fn render_markdown_v2(text: &str) -> String {
    convert_with_strategy(text, UnsupportedTagsStrategy::Escape)
        .map(|rendered| rendered.trim_end_matches('\n').to_string())
        .unwrap_or_else(|_| escape_markdown_v2(text))
}

fn markdown_blocks(text: &str) -> Vec<String> {
    let mut blocks = Vec::new();
    let mut current = String::new();
    let mut in_fence = false;

    for line in text.split_inclusive('\n') {
        current.push_str(line);
        if is_fence_boundary(line) {
            in_fence = !in_fence;
        }
        if !in_fence && line.trim().is_empty() {
            blocks.push(std::mem::take(&mut current));
        }
    }

    if !current.is_empty() {
        blocks.push(current);
    }
    blocks
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
    let fence = "`".repeat(longest_backtick_run(text) + 1);
    if text.starts_with('`') || text.ends_with('`') {
        format!("{fence} {text} {fence}")
    } else {
        format!("{fence}{text}{fence}")
    }
}

/// Low-level plain-text fallback for Telegram MarkdownV2.
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

/// Wrap text in a CommonMark fenced code block.
pub fn code_block(text: &str) -> String {
    markdown_code_block(text)
}

fn markdown_code_block(text: &str) -> String {
    let fence = "`".repeat((longest_backtick_run(text) + 1).max(3));
    format!("{fence}\n{text}\n{fence}")
}

fn longest_backtick_run(text: &str) -> usize {
    let mut longest = 0usize;
    let mut current = 0usize;
    for ch in text.chars() {
        if ch == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_text_is_common_markdown_and_renders_to_markdown_v2() {
        let text = help_text();
        let rendered = render_markdown_v2(&text);

        assert!(text.contains("code-agent"));
        assert!(rendered.contains("code\\-agent"));
        assert!(rendered.contains("*TinyButler commands:*"));
    }

    #[test]
    fn daemon_startup_notification_is_common_markdown() {
        let config = Config {
            home: PathBuf::from("/tmp/tinybutler-home"),
            telegram: Default::default(),
            code_agents: Default::default(),
        };
        let text = daemon_startup_notification_text(&config);
        let rendered = render_markdown_v2(&text);

        assert!(text.contains("daemon restarted"));
        assert!(text.contains("/tmp/tinybutler-home"));
        assert!(rendered.contains("*TinyButler daemon restarted*"));
    }

    #[test]
    fn bot_menu_commands_match_current_public_telegram_surface() {
        let commands = bot_menu_commands();
        let command_names = commands
            .iter()
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            command_names,
            vec!["new", "session", "abort", "tasks", "help"]
        );
        for command in commands {
            assert!(command
                .command
                .chars()
                .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_'));
            assert!(!command.description.contains("TickClaw"));
            assert!(!command.description.contains("tickclaw"));
            assert!(!command.description.contains("TinyButler"));
            assert!(!command.description.contains("tinybutler"));
            assert!(!command.description.contains("TinyBulter"));
            assert!(!command.description.contains("tinybulter"));
            assert!(command.description.len() <= 16);
        }
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

    #[test]
    fn renders_common_markdown_before_telegram_delivery() {
        let rendered = render_markdown_v2(
            "## Summary\n\n**Done** with `path-name`.\n\n- item_one\n\n[OpenAI](https://openai.com/a_b)",
        );

        assert!(rendered.contains("*Summary*"));
        assert!(rendered.contains("*Done*"));
        assert!(rendered.contains("`path-name`"));
        assert!(rendered.contains("\\_one"));
        assert!(rendered.contains("[OpenAI](https://openai.com/a_b)"));
        assert!(!rendered.contains("\\*\\*Done\\*\\*"));
    }

    #[test]
    fn chunks_common_markdown_on_block_boundaries() {
        let chunks = chunk_markdown_v2("**first**\n\n**second**", 10);

        assert_eq!(chunks, vec!["*first*", "*second*"]);
    }

    #[test]
    fn task_callback_data_uses_stable_short_fingerprint() {
        let fingerprint = task_selector_fingerprint("regular-check");
        let callback = format!("12:{fingerprint}");

        assert_eq!(
            parse_task_callback_data(&callback).expect("callback data"),
            TaskCallbackSelection {
                index: 12,
                fingerprint,
            }
        );
        assert!(callback.len() < 64);
    }

    #[test]
    fn rejects_malformed_task_callback_data() {
        assert!(parse_task_callback_data("missing-fingerprint").is_err());
        assert!(parse_task_callback_data("0:not-hex").is_err());
        assert!(parse_task_callback_data("not-number:0123456789abcdef").is_err());
    }

    #[test]
    fn inline_menu_text_limit_accounts_for_markdown_v2_expansion() {
        assert!(inline_menu_text_fits("short **menu**"));
        assert!(!inline_menu_text_fits(&"x_y".repeat(1000)));
    }

    #[test]
    fn assistant_visible_text_prefers_final_and_strips_reasoning() {
        let text = "draft <think>private reasoning</think><final>Visible answer</final>";

        assert_eq!(assistant_visible_text(text), "Visible answer");
    }

    #[test]
    fn assistant_visible_text_preserves_reasoning_tags_inside_code_fences() {
        let text = "Visible\n```xml\n<think>literal</think>\n```\n<think>hidden</think>";

        assert_eq!(
            assistant_visible_text(text),
            "Visible\n```xml\n<think>literal</think>\n```\n"
        );
    }

    #[test]
    fn extracts_and_removes_attachment_markers() {
        let temp = tempfile::tempdir().expect("temp dir");
        let image = temp.path().join("screen.png");
        std::fs::write(&image, b"not really a png").expect("write image");
        let text = format!("Here\nATTACH:{}\nDone", image.display());

        assert_eq!(
            extract_outbound_attachment_paths(&text, temp.path()),
            vec![image]
        );
        assert_eq!(remove_attachment_markers(&text), "Here\nDone");
    }

    #[test]
    fn extracts_home_and_relative_attachment_paths() {
        let temp = tempfile::tempdir().expect("temp dir");
        let image = temp.path().join("relative.jpg");
        std::fs::write(&image, b"jpg").expect("write image");

        assert_eq!(
            extract_outbound_attachment_paths("ATTACH:./relative.jpg", temp.path()),
            vec![image]
        );
    }

    #[test]
    fn extracts_plain_local_image_paths_as_fallback() {
        let temp = tempfile::tempdir().expect("temp dir");
        let image = temp.path().join("plain.png");
        std::fs::write(&image, b"png").expect("write image");
        let text = format!("saved at `{}`", image.display());

        assert_eq!(extract_plain_image_paths(&text, temp.path()), vec![image]);
    }

    #[test]
    fn classifies_attachment_methods_by_extension() {
        assert_eq!(
            classify_attachment(Path::new("screen.png")),
            TelegramAttachment {
                method: "sendPhoto",
                field_name: "photo",
            }
        );
        assert_eq!(
            classify_attachment(Path::new("clip.gif")),
            TelegramAttachment {
                method: "sendAnimation",
                field_name: "animation",
            }
        );
        assert_eq!(
            classify_attachment(Path::new("clip.mp4")),
            TelegramAttachment {
                method: "sendVideo",
                field_name: "video",
            }
        );
        assert_eq!(
            classify_attachment(Path::new("sound.mp3")),
            TelegramAttachment {
                method: "sendAudio",
                field_name: "audio",
            }
        );
        assert_eq!(
            classify_attachment(Path::new("report.txt")),
            TelegramAttachment {
                method: "sendDocument",
                field_name: "document",
            }
        );
    }
}
