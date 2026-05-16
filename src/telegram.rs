//! Telegram Bot API sender and minimal long-polling ingress.
//!
//! Outbound sending is used by task notifications and agent calls. The MVP
//! ingress path uses direct Bot API long polling for the task-management slash
//! commands and can later be replaced by a richer teloxide workflow.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use chrono::Local;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::fs;
use tokio::time::{sleep, Duration};
use tracing::{info, warn};

use crate::config::Config;
use crate::scheduler::Scheduler;

/// Telegram parse mode used by TickClaw-generated and CLI-authored messages.
pub const TELEGRAM_PARSE_MODE: &str = "MarkdownV2";

/// Parsed Telegram command supported by TickClaw ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngressCommand {
    Help,
    TaskList,
    TaskRun(String),
    TaskStatus(String),
    TaskEnable(String),
    TaskDisable(String),
    Unknown(String),
}

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
        "*TickClaw task:* {}\n*status:* {}\n*duration:* {}\n*log:* {}",
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
                            let mut update_handled = update.message.is_none();
                            if let Some(message) = update.message {
                                if message.chat.id.to_string() != allowed_chat_id {
                                    update_handled = true;
                                } else if let Some(text) = message.text.as_deref() {
                                    if let Err(err) = handle_command(&config, &message.chat.id.to_string(), text).await {
                                        warn!("failed to handle Telegram command: {err:#}");
                                        let _ = send_markdown_text_to_chat(
                                            &config,
                                            &message.chat.id.to_string(),
                                            &format!("TickClaw command failed:\n{}", code_block(&truncate(&format!("{err:#}"), 1200))),
                                        ).await;
                                    }
                                    update_handled = true;
                                } else {
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

/// Parse Telegram text into a TickClaw ingress command.
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

async fn handle_command(config: &Config, chat_id: &str, text: &str) -> Result<()> {
    let command = parse_ingress_command(text);
    let scheduler = Scheduler::new(config.clone());
    let response = match command {
        IngressCommand::Help => help_text(),
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
                    "Unknown TickClaw command: {}\n\n{}",
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
        "TickClaw commands:".to_string(),
        format!("{}: list tasks", inline_code("/task_list")),
        format!("{}: show task status", inline_code("/task_status <task>")),
        format!("{}: run one task now", inline_code("/task_run <task>")),
        format!("{}: enable one task", inline_code("/task_enable <task>")),
        format!("{}: disable one task", inline_code("/task_disable <task>")),
        format!("{}: show this help", inline_code("/help")),
    ]
    .join("\n")
}

async fn poll_once(
    client: &Client,
    config: &Config,
    offset: Option<i64>,
) -> Result<Vec<TelegramUpdate>> {
    let url = telegram_method_url(config, "getUpdates")?;
    let mut payload = json!({
        "timeout": 25,
        "allowed_updates": ["message"],
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

    ensure_success(response, "sendMessage").await
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
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

async fn ensure_success(response: reqwest::Response, method: &str) -> Result<()> {
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Telegram {method} failed: {status} {body}"));
    }
    Ok(())
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
