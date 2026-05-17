//! Task schema tests kept outside `src/` for implementation/test separation.

use tinybutler::task::{Task, TaskType};

/// Parse a task YAML snippet through the public task schema.
fn parse_task(text: &str) -> Result<Task, serde_yaml::Error> {
    serde_yaml::from_str::<Task>(text)
}

#[test]
fn rejects_removed_workspace_notify_and_concurrency_fields() {
    for field in ["workspace: /tmp", "notify: telegram", "concurrency: skip"] {
        let yaml = format!(
            "name: demo\nenabled: true\nschedule: \"0 9 * * *\"\ntype: command\ntimeout: 60\n{field}\n"
        );
        assert!(
            parse_task(&yaml).is_err(),
            "task.yaml field should be rejected: {field}"
        );
    }
}

#[test]
fn parses_minimal_shell_task() {
    let task = parse_task(
        r#"name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 1800
"#,
    )
    .expect("minimal shell task should parse");

    assert_eq!(task.name, "regular-check");
    assert_eq!(task.runner, None);
    assert_eq!(task.runner_label(), "shell");
    assert_eq!(task.task_type, TaskType::Command);
}

#[test]
fn rejects_invalid_cron_schedule_during_validation() {
    let task = parse_task(
        r#"name: broken
enabled: true
schedule: "not a cron"
type: command
timeout: 60
"#,
    )
    .expect("schema parses before schedule validation rejects it");

    assert!(task.validate().is_err());
}

#[test]
fn rejects_command_task_with_runner() {
    let task = parse_task(
        r#"name: regular-check
enabled: true
schedule: "*/10 * * * *"
runner: shell
type: command
timeout: 1800
"#,
    )
    .expect("runner is parsed before command validation rejects it");

    assert!(task.validate().is_err());
}

#[test]
fn rejects_runner_specific_codex_block() {
    let yaml_with_codex_block = r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
runner: codex
type: agent
timeout: 3600

codex:
  model: gpt-5.5
"#;

    assert!(
        parse_task(yaml_with_codex_block).is_err(),
        "task.yaml should not accept runner-specific codex config"
    );
}

#[test]
fn parses_gpt55_task_without_task_local_runner_config() {
    let task = parse_task(
        r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
runner: gpt-5.5
type: agent
timeout: 3600
"#,
    )
    .expect("code-agent task should not need task-local runner config");

    assert!(task.validate().is_ok());
    assert_eq!(task.runner.as_deref(), Some("gpt-5.5"));
    assert_eq!(task.task_type, TaskType::Agent);
}

#[test]
fn parses_agent_task_with_configured_runner_key() {
    let task = parse_task(
        r#"name: spark-task
enabled: false
schedule: "0 10 * * *"
runner: gpt-5.3-codex-spark
type: agent
timeout: 1800
"#,
    )
    .expect("agent task should accept any non-shell runner key");

    assert!(task.validate().is_ok());
    assert_eq!(task.runner.as_deref(), Some("gpt-5.3-codex-spark"));
}

#[test]
fn rejects_agent_task_without_runner() {
    let task = parse_task(
        r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
type: agent
timeout: 3600
"#,
    )
    .expect("missing runner is checked during validation");

    assert!(task.validate().is_err());
}
