//! Telegram ingress command parsing tests.

use tickclaw::telegram::{
    code_block, escape_markdown_v2, parse_ingress_command, sanitize_markdown_v2_message,
    IngressCommand, TelegramPollState, TELEGRAM_PARSE_MODE,
};

#[test]
fn parses_task_list_slash_command() {
    assert_eq!(
        parse_ingress_command("/task_list"),
        IngressCommand::TaskList
    );
    assert_eq!(
        parse_ingress_command("/task_list@OpenClawBot"),
        IngressCommand::TaskList
    );
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
}

#[test]
fn parses_task_status_and_run_arguments() {
    assert_eq!(
        parse_ingress_command("/task_status mock-gpt55-review"),
        IngressCommand::TaskStatus("mock-gpt55-review".to_string())
    );
    assert_eq!(
        parse_ingress_command("/task_run mock-spark-code-smoke"),
        IngressCommand::TaskRun("mock-spark-code-smoke".to_string())
    );
    assert_eq!(
        parse_ingress_command("/task_enable mock-gpt55-review"),
        IngressCommand::TaskEnable("mock-gpt55-review".to_string())
    );
    assert_eq!(
        parse_ingress_command("/task_disable mock-gpt55-review"),
        IngressCommand::TaskDisable("mock-gpt55-review".to_string())
    );
}

#[test]
fn reports_missing_task_arguments() {
    assert!(matches!(
        parse_ingress_command("/task_status"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/task_run"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/task_enable"),
        IngressCommand::Unknown(_)
    ));
    assert!(matches!(
        parse_ingress_command("/task_disable"),
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
    assert_eq!(
        code_block("path`with\\chars"),
        "```\npath\\`with\\\\chars\n```"
    );
}

#[test]
fn sanitizes_cli_authored_markdown_v2_without_breaking_simple_formatting() {
    assert_eq!(
        sanitize_markdown_v2_message("Build finished. task-name!"),
        r"Build finished\. task\-name\!"
    );
    assert_eq!(
        sanitize_markdown_v2_message("Task *done* at `task-name`."),
        r"Task *done* at `task-name`\."
    );
    assert_eq!(
        sanitize_markdown_v2_message(r"Already escaped\."),
        r"Already escaped\."
    );
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
