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
    assert!(task.agents.is_empty());
    assert_eq!(task.agents_label(), "shell");
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
fn rejects_removed_runner_and_agent_fields() {
    for field in ["runner: gpt-5.5", "agent: gpt-5.5"] {
        let yaml = format!(
            "name: smoke-task\nenabled: false\nschedule: \"0 9 * * *\"\n{field}\ntype: agent\ntimeout: 3600\n"
        );
        assert!(
            parse_task(&yaml).is_err(),
            "task.yaml should reject removed field: {field}"
        );
    }
}

#[test]
fn rejects_command_task_with_agents() {
    let task = parse_task(
        r#"name: regular-check
enabled: true
schedule: "*/10 * * * *"
agents:
  - codex/gpt-5.5
type: command
timeout: 1800
"#,
    )
    .expect("agents are parsed before command validation rejects them");

    assert!(task.validate().is_err());
}

#[test]
fn rejects_runner_specific_codex_block() {
    let yaml_with_codex_block = r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
agents:
  - codex
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
fn parses_gpt55_grouped_task_without_task_local_runner_config() {
    let task = parse_task(
        r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
agents:
  - codex/gpt-5.5
type: agent
timeout: 3600
"#,
    )
    .expect("code-agent task should not need task-local runner config");

    assert!(task.validate().is_ok());
    assert_eq!(task.agent_runner_keys(), vec!["codex/gpt-5.5"]);
    assert_eq!(task.task_type, TaskType::Agent);
}

#[test]
fn parses_agent_task_with_single_agent_list() {
    let task = parse_task(
        r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
agents:
  - codex/gpt-5.5
type: agent
timeout: 3600
"#,
    )
    .expect("single-item agents list should parse");

    assert!(task.validate().is_ok());
    assert_eq!(task.agent_runner_keys(), vec!["codex/gpt-5.5"]);
    assert_eq!(task.agents_label(), "codex/gpt-5.5");
}

#[test]
fn parses_agent_task_with_agent_fallback_list() {
    let task = parse_task(
        r#"name: fallback-task
enabled: false
schedule: "0 9 * * *"
agents:
  - gemini/gemini-3.1-flash-lite
  - codex/gpt-5.3-codex-spark
type: agent
timeout: 3600
"#,
    )
    .expect("agents list should parse");

    assert!(task.validate().is_ok());
    assert_eq!(
        task.agent_runner_keys(),
        vec!["gemini/gemini-3.1-flash-lite", "codex/gpt-5.3-codex-spark"]
    );
    assert_eq!(
        task.agents_label(),
        "gemini/gemini-3.1-flash-lite -> codex/gpt-5.3-codex-spark"
    );
}

#[test]
fn rejects_empty_agent_list() {
    let task = parse_task(
        r#"name: fallback-task
enabled: false
schedule: "0 9 * * *"
agents: []
type: agent
timeout: 3600
"#,
    )
    .expect("empty agents list is parsed before validation rejects it");

    assert!(task.validate().is_err());
}

#[test]
fn rejects_shell_in_agent_list() {
    let task = parse_task(
        r#"name: spark-task
enabled: false
schedule: "0 10 * * *"
agents:
  - shell
type: agent
timeout: 1800
"#,
    )
    .expect("agents list is parsed before validation rejects shell");

    assert!(task.validate().is_err());
}

#[test]
fn rejects_agent_task_without_agents() {
    let task = parse_task(
        r#"name: smoke-task
enabled: false
schedule: "0 9 * * *"
type: agent
timeout: 3600
"#,
    )
    .expect("missing agents is checked during validation");

    assert!(task.validate().is_err());
}
