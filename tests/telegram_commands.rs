//! Telegram ingress command parsing tests.

use tinybutler::telegram::{
    code_block, escape_markdown_v2, parse_ingress_command, IngressCommand, TelegramPollState,
    TELEGRAM_PARSE_MODE,
};

#[test]
fn parses_tasks_slash_command() {
    assert_eq!(parse_ingress_command("/tasks"), IngressCommand::Tasks);
    assert_eq!(
        parse_ingress_command("/tasks@OpenClawBot"),
        IngressCommand::Tasks
    );
}

#[test]
fn parses_chat_bridge_commands() {
    assert_eq!(parse_ingress_command("/new"), IngressCommand::New);
    assert_eq!(parse_ingress_command("/session"), IngressCommand::Session);
    assert_eq!(parse_ingress_command("/abort"), IngressCommand::Abort);
}

#[test]
fn rejects_bare_or_legacy_tasklist_aliases() {
    assert!(matches!(
        parse_ingress_command("tasklist"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/tasklist"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/task_list"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/task_status mock-gpt55-review"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/task_run mock-spark-code-smoke"),
        IngressCommand::Unknown(_)
    ));
}

#[test]
fn escapes_markdown_v2_user_controlled_text() {
    assert_eq!(TELEGRAM_PARSE_MODE, "MarkdownV2");
    assert_eq!(
        escape_markdown_v2(r"_*[]()~`>#+-=|{}.!\\"),
        r"\_\*\[\]\(\)\~\`\>\#\+\-\=\|\{\}\.\!\\\\"
    );
}

#[test]
fn builds_common_markdown_code_blocks() {
    assert_eq!(code_block("path`with\\chars"), "```\npath`with\\chars\n```");
}

#[tokio::test]
async fn persists_telegram_poll_offset_state() {
    let temp = tempfile::tempdir().expect("temp dir");
    let path = temp.path().join("telegram_state.json");
    let state = TelegramPollState {
        last_update_id: Some(123),
        last_poll_at: Some("2026-05-16T20:00:00+02:00".to_string()),
    };

    state.save(&path).await.expect("save state");

    let loaded = TelegramPollState::load(&path).await.expect("load state");
    assert_eq!(loaded.last_update_id, Some(123));
    assert_eq!(
        loaded.last_poll_at.as_deref(),
        Some("2026-05-16T20:00:00+02:00")
    );
    assert_eq!(loaded.next_offset(), Some(124));
}
