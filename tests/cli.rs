//! End-to-end CLI tests for the public TickClaw command surface.
//!
//! These tests exercise the installed binary through Cargo's test-provided
//! executable path so command parsing, init templates, check, and task commands
//! are verified together.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{Duration, Local};
use serde_json::json;

/// Return the compiled TickClaw binary path provided by Cargo integration tests.
fn tickclaw_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tickclaw"))
}

/// Execute TickClaw against an isolated home directory.
fn run_tickclaw(home: &Path, args: &[&str]) -> Output {
    Command::new(tickclaw_bin())
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("tickclaw command should start")
}

/// Convert command stdout into UTF-8 for readable assertions.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Return every file under a directory as paths relative to that directory.
fn relative_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending_dirs = vec![root.to_path_buf()];

    while let Some(dir) = pending_dirs.pop() {
        for entry in std::fs::read_dir(&dir).expect("read directory") {
            let entry = entry.expect("directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("file type");
            if file_type.is_dir() {
                pending_dirs.push(path);
            } else if file_type.is_file() {
                files.push(
                    path.strip_prefix(root)
                        .expect("relative path")
                        .to_path_buf(),
                );
            }
        }
    }

    files.sort();
    files
}

/// Convert command stderr into UTF-8 for readable failure messages.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

#[test]
fn init_then_check_creates_valid_home() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let check = run_tickclaw(home, &["check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    assert!(stdout(&check).contains("ok: checked config.yaml and 2 task definition"));

    assert!(home.join("tasks/smoke-task/task.yaml").exists());
    assert!(home.join("tasks/regular-check/run.sh").exists());

    let template_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("templates");
    for relative_path in relative_files(&template_root) {
        let template_text =
            std::fs::read_to_string(template_root.join(&relative_path)).expect("template file");
        let initialized_text =
            std::fs::read_to_string(home.join(&relative_path)).expect("initialized file");
        assert_eq!(
            initialized_text,
            template_text,
            "init should copy template {} exactly",
            relative_path.display()
        );
    }
}

#[test]
fn init_codex_runners_read_prompt_from_stdin_and_emit_json() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let config_text = std::fs::read_to_string(home.join("config.yaml")).expect("config");
    let config: serde_yaml::Value = serde_yaml::from_str(&config_text).expect("parse config");
    for runner in ["gpt-5.3-codex-spark", "gpt-5.5"] {
        let args = config["code_agents"][runner]["args"]
            .as_sequence()
            .expect("args");
        let resume_args = config["code_agents"][runner]["resume_args"]
            .as_sequence()
            .expect("resume args");

        assert!(
            args.iter().any(|value| value.as_str() == Some("--json")),
            "{runner} fresh args should emit JSONL"
        );
        assert_eq!(
            args.last().and_then(|value| value.as_str()),
            Some("-"),
            "{runner} fresh args should read prompt from stdin"
        );
        assert!(
            resume_args
                .iter()
                .any(|value| value.as_str() == Some("--json")),
            "{runner} resume args should emit JSONL"
        );
        assert_eq!(
            resume_args.last().and_then(|value| value.as_str()),
            Some("-"),
            "{runner} resume args should read prompt from stdin"
        );
    }
}

#[test]
fn init_stream_runners_include_complete_streaming_flags() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let config_text = std::fs::read_to_string(home.join("config.yaml")).expect("config");
    let config: serde_yaml::Value = serde_yaml::from_str(&config_text).expect("parse config");

    let gemini_stream_args = config["code_agents"]["gemini-3.1-flash-lite"]["stream_args"]
        .as_sequence()
        .expect("gemini stream args");
    assert!(
        gemini_stream_args
            .windows(2)
            .any(|pair| pair[0].as_str() == Some("--output-format")
                && pair[1].as_str() == Some("stream-json")),
        "gemini streaming should use stream-json output"
    );
    assert!(
        gemini_stream_args
            .windows(2)
            .any(|pair| pair[0].as_str() == Some("--approval-mode")
                && pair[1].as_str() == Some("yolo")),
        "gemini streaming should include approval mode"
    );

    for runner in ["gpt-5.3-codex-spark", "gpt-5.5"] {
        let codex_stream_args = config["code_agents"][runner]["stream_args"]
            .as_sequence()
            .expect("codex stream args");
        let codex_stream_arg_text = codex_stream_args
            .iter()
            .filter_map(|value| value.as_str())
            .collect::<Vec<_>>();
        assert!(
            codex_stream_arg_text.contains(&"app-server"),
            "{runner} streaming should use app-server"
        );
        assert!(
            codex_stream_arg_text.contains(&format!("model=\"{runner}\"").as_str()),
            "{runner} streaming should set model"
        );
        assert!(
            codex_stream_arg_text.contains(&"sandbox_mode=\"danger-full-access\""),
            "{runner} streaming should set sandbox mode"
        );
    }
}

#[test]
fn check_rejects_removed_task_fields() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let task_yaml = home.join("tasks/regular-check/task.yaml");
    let mut text = std::fs::read_to_string(&task_yaml).expect("read task yaml");
    text.push_str("workspace: /tmp\n");
    std::fs::write(&task_yaml, text).expect("write invalid task yaml");

    let check = run_tickclaw(home, &["check"]);
    assert!(
        !check.status.success(),
        "check should fail on obsolete field"
    );
    assert!(
        stderr(&check).contains("workspace"),
        "stderr should mention rejected field: {}",
        stderr(&check)
    );
}

#[test]
fn check_rejects_missing_task_execution_files_and_runners() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    std::fs::remove_file(home.join("tasks/regular-check/run.sh")).expect("remove run.sh");
    let missing_script = run_tickclaw(home, &["check"]);
    assert!(
        !missing_script.status.success(),
        "check should reject command tasks without run.sh"
    );
    assert!(
        stderr(&missing_script).contains("run.sh"),
        "stderr should mention missing run.sh: {}",
        stderr(&missing_script)
    );

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));
    let task_yaml = home.join("tasks/smoke-task/task.yaml");
    let text = std::fs::read_to_string(&task_yaml)
        .expect("read task yaml")
        .replace("runner: gpt-5.3-codex-spark", "runner: missing-runner");
    std::fs::write(&task_yaml, text).expect("write task yaml");

    let missing_runner = run_tickclaw(home, &["check"]);
    assert!(
        !missing_runner.status.success(),
        "check should reject agent tasks with unknown runners"
    );
    assert!(
        stderr(&missing_runner).contains("code_agents.missing-runner"),
        "stderr should mention missing runner: {}",
        stderr(&missing_runner)
    );
}

#[test]
fn task_run_and_status_record_latest_log() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let run = run_tickclaw(home, &["task", "run", "regular-check"]);
    assert!(run.status.success(), "{}", stderr(&run));

    let status = run_tickclaw(home, &["task", "status", "regular-check"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let out = stdout(&status);
    assert!(out.contains("last_status"));
    assert!(out.contains("shell_command:"));
    assert!(out.contains("regular-check ok:"));
    assert!(out.contains("latest_log_first_20_lines"));
}

#[test]
fn agent_status_shows_prompt_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let status = run_tickclaw(home, &["task", "status", "smoke-task"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let out = stdout(&status);

    assert!(out.contains("agent_prompt:"));
    assert!(out.contains("schedule: At 09:00. (0 9 * * *)"));
    assert!(out.contains("Inspect this task directory"));
    assert!(out.contains("state:"));
}

#[test]
fn task_list_uses_chat_readable_multiline_blocks() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let list = run_tickclaw(home, &["task", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    let out = stdout(&list);

    assert!(out.contains("regular-check\n  enabled: true"));
    assert!(out.contains("\n  runner: shell\n"));
    assert!(out.contains("schedule: At every 10 minutes. (*/10 * * * *)"));
    assert!(out.contains("\n  next_run_at: "));
    assert!(
        !out.contains("enabled=true type="),
        "task list should not use the old long single-line format"
    );
}

#[test]
fn task_enable_and_disable_update_task_yaml() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let disable = run_tickclaw(home, &["task", "disable", "regular-check"]);
    assert!(disable.status.success(), "{}", stderr(&disable));
    assert!(stdout(&disable).contains("regular-check disabled"));

    let disabled_yaml =
        std::fs::read_to_string(home.join("tasks/regular-check/task.yaml")).expect("task yaml");
    assert!(disabled_yaml.contains("enabled: false"));

    let list = run_tickclaw(home, &["task", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stdout(&list).contains("regular-check\n  enabled: false"));

    let enable = run_tickclaw(home, &["task", "enable", "regular-check"]);
    assert!(enable.status.success(), "{}", stderr(&enable));
    assert!(stdout(&enable).contains("regular-check enabled"));

    let enabled_yaml =
        std::fs::read_to_string(home.join("tasks/regular-check/task.yaml")).expect("task yaml");
    assert!(enabled_yaml.contains("enabled: true"));
}

#[test]
fn manual_task_run_preserves_future_scheduled_next_run() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let state_path = home.join("tasks/regular-check/state.json");
    let future_next_run = (Local::now() + Duration::hours(12)).to_rfc3339();
    std::fs::write(
        &state_path,
        serde_json::to_string_pretty(&json!({
            "schedule_expr": "0 */10 * * * *",
            "next_run_at": future_next_run,
            "running": false,
            "run_count": 0,
            "failure_count": 0
        }))
        .expect("state json"),
    )
    .expect("write state");

    let run = run_tickclaw(home, &["task", "run", "regular-check"]);
    assert!(run.status.success(), "{}", stderr(&run));

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(state["next_run_at"], future_next_run);
    assert_eq!(state["schedule_expr"], "0 */10 * * * *");
    assert_eq!(state["last_status"], "success");
}

#[test]
fn task_list_reconciles_state_when_schedule_changes() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tickclaw(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let state_path = home.join("tasks/regular-check/state.json");
    let stale_next_run = (Local::now() + Duration::hours(12)).to_rfc3339();
    std::fs::write(
        &state_path,
        serde_json::to_string_pretty(&json!({
            "schedule_expr": "0 0 9 * * *",
            "next_run_at": stale_next_run,
            "running": false,
            "run_count": 0,
            "failure_count": 0
        }))
        .expect("state json"),
    )
    .expect("write state");

    let list = run_tickclaw(home, &["task", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));

    let state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(state_path).expect("read state"))
            .expect("parse state");
    assert_eq!(state["schedule_expr"], "0 */10 * * * *");
    assert_ne!(state["next_run_at"], stale_next_run);
}

#[test]
fn old_top_level_commands_are_not_public_cli() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    for command in ["scan", "run", "state", "logs"] {
        let output = run_tickclaw(home, &[command]);
        assert!(
            !output.status.success(),
            "old command should not be accepted: {command}"
        );
    }
}

#[test]
fn telegram_poll_is_not_a_public_cli_command() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let output = run_tickclaw(home, &["telegram", "--poll"]);
    assert!(
        !output.status.success(),
        "telegram polling should start from daemon, not --poll"
    );
}

#[test]
fn chat_commands_are_public_cli_surface() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let output = run_tickclaw(home, &["chat", "--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("new"));
    assert!(out.contains("session"));
}
