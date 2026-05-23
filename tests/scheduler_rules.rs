//! Scheduler rule tests kept outside `src/` for implementation/test separation.

use std::path::PathBuf;

use tinybutler::config::Config;
use tinybutler::runner::RunOutcome;
use tinybutler::scheduler::{
    Scheduler, describe_schedule, format_schedule_for_display, normalize_cron,
    set_enabled_in_task_yaml,
};
use tinybutler::task::{SessionMode, Task, TaskType};

/// Build a minimal task for notification-rule assertions.
fn task(task_type: TaskType) -> Task {
    Task {
        name: "demo".to_string(),
        enabled: true,
        schedule: "*/10 * * * *".to_string(),
        agents: if task_type == TaskType::Agent {
            vec!["gpt-5.5".to_string()]
        } else {
            Vec::new()
        },
        task_type,
        session: SessionMode::Independent,
        timeout: 60,
        dir: PathBuf::from("/tmp/demo"),
    }
}

/// Build a minimal run outcome for notification-rule assertions.
fn outcome(status: &str, stdout: &str) -> RunOutcome {
    RunOutcome {
        status: status.to_string(),
        exit_code: Some(if status == "success" { 0 } else { 1 }),
        duration_seconds: 1,
        log_path: PathBuf::from("/tmp/demo/logs/test.log"),
        log_relative_path: "logs/test.log".to_string(),
        summary: stdout.to_string(),
        session_id: None,
        agent_runner: None,
        stdout: stdout.to_string(),
    }
}

#[test]
fn normalizes_five_field_cron() {
    assert_eq!(normalize_cron("*/10 * * * *"), "0 */10 * * * *");
    assert_eq!(normalize_cron("0 */10 * * * *"), "0 */10 * * * *");
}

#[test]
fn describes_schedules_in_english() {
    assert_eq!(describe_schedule("*/10 * * * *"), "At every 10 minutes.");
    assert_eq!(
        format_schedule_for_display("0 9 * * *"),
        "At 09:00. (0 9 * * *)"
    );
}

#[test]
fn updates_or_inserts_enabled_field_in_task_yaml() {
    let existing = "name: demo\nenabled: true\nschedule: \"*/10 * * * *\"\n";
    assert_eq!(
        set_enabled_in_task_yaml(existing, false),
        "name: demo\nenabled: false\nschedule: \"*/10 * * * *\"\n"
    );

    let missing = "name: demo\nschedule: \"*/10 * * * *\"\n";
    assert_eq!(
        set_enabled_in_task_yaml(missing, true),
        "name: demo\nenabled: true\nschedule: \"*/10 * * * *\"\n"
    );
}

#[test]
fn notification_rules_match_task_type_and_result() {
    let scheduler = Scheduler::new(Config {
        home: PathBuf::from("/tmp/tinybutler"),
        telegram: Default::default(),
        code_agents: Default::default(),
    });

    assert!(!scheduler.should_notify(&task(TaskType::Agent), &outcome("success", "done")));
    assert!(scheduler.should_notify(&task(TaskType::Agent), &outcome("failed", "")));
    assert!(!scheduler.should_notify(&task(TaskType::Command), &outcome("success", "")));
    assert!(scheduler.should_notify(&task(TaskType::Command), &outcome("success", "ok")));
    assert!(scheduler.should_notify(&task(TaskType::Command), &outcome("failed", "")));
}

#[tokio::test]
async fn task_action_labels_do_not_include_exit() {
    let temp = tempfile::tempdir().expect("temp dir");
    let task_dir = temp.path().join("tasks/demo");
    std::fs::create_dir_all(&task_dir).expect("task dir");
    std::fs::write(
        task_dir.join("task.yaml"),
        "name: demo\nenabled: true\nschedule: \"*/10 * * * *\"\ntype: command\n",
    )
    .expect("task yaml");

    let scheduler = Scheduler::new(Config {
        home: temp.path().to_path_buf(),
        telegram: Default::default(),
        code_agents: Default::default(),
    });

    let actions = scheduler
        .task_selector_action_labels("demo")
        .await
        .expect("action labels");
    assert_eq!(actions, ["status", "run", "disable"]);
}
