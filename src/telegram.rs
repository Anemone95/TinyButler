use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::json;

use crate::config::Config;

pub async fn send_text(config: &Config, text: &str) -> Result<()> {
    let token = config
        .telegram
        .bot_token
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("missing telegram.bot_token in config.yaml")?;
    let chat_id = config
        .telegram
        .chat_id
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .context("missing telegram.chat_id in config.yaml")?;

    let url = format!("https://api.telegram.org/bot{token}/sendMessage");
    let mut payload = json!({
        "chat_id": chat_id,
        "text": text,
        "disable_web_page_preview": true,
    });
    if let Some(parse_mode) = config
        .telegram
        .parse_mode
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        payload["parse_mode"] = json!(parse_mode);
    }

    let response = Client::new()
        .post(url)
        .json(&payload)
        .send()
        .await
        .map_err(|err| anyhow!("Telegram sendMessage request failed: {}", err.without_url()))?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!("Telegram sendMessage failed: {status} {body}"));
    }

    Ok(())
}

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
        "*TickClaw task:* `{}`\n*status:* `{}`\n*duration:* `{}s`\n*log:* `{}`",
        inline_code(task_name),
        inline_code(status),
        duration_seconds,
        inline_code(log_path)
    );
    if let Some(code) = exit_code {
        text.push_str(&format!("\n*exit code:* `{code}`"));
    }
    if !summary.trim().is_empty() {
        text.push_str("\n\n*summary:*\n");
        text.push_str(&code_block(&truncate(summary.trim(), 1200)));
    }
    send_text(config, &text).await
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
    text.replace('\\', "\\\\").replace('`', "\\`")
}

fn code_block(text: &str) -> String {
    let escaped = text.replace('\\', "\\\\").replace("```", "`\\`\\`");
    format!("```\n{escaped}\n```")
}
