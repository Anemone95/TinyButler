//! End-to-end CLI tests for the public TinyButler command surface.
//!
//! These tests exercise the installed binary through Cargo's test-provided
//! executable path so command parsing, init templates, check, and task commands
//! are verified together.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use chrono::{Datelike, Duration, Local, Months};
use serde_json::json;

/// Return the compiled TinyButler binary path provided by Cargo integration tests.
fn tinybutler_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tinybutler"))
}

/// Execute TinyButler against an isolated home directory.
fn run_tinybutler(home: &Path, args: &[&str]) -> Output {
    Command::new(tinybutler_bin())
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("tinybutler command should start")
}

/// Execute TinyButler with scripted stdin for non-interactive selectors.
fn run_tinybutler_with_input(home: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(tinybutler_bin())
        .arg("--home")
        .arg(home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tinybutler command should start");
    child
        .stdin
        .as_mut()
        .expect("child stdin")
        .write_all(input.as_bytes())
        .expect("write selector input");
    child.wait_with_output().expect("tinybutler output")
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

/// Return a timestamped task-log file name in a month before the current month.
fn log_file_name_months_ago(months_ago: u32) -> String {
    let current_month = Local::now()
        .date_naive()
        .with_day(1)
        .expect("current month start");
    let month = current_month
        .checked_sub_months(Months::new(months_ago))
        .expect("shifted month");
    format!("{}-{:02}-01T00-00-00.log", month.year(), month.month())
}

/// Return the monthly archive file name for a month before the current month.
fn archive_file_name_months_ago(months_ago: u32) -> String {
    let current_month = Local::now()
        .date_naive()
        .with_day(1)
        .expect("current month start");
    let month = current_month
        .checked_sub_months(Months::new(months_ago))
        .expect("shifted month");
    format!("{}-{:02}.tgz", month.year(), month.month())
}

#[test]
fn init_then_check_creates_valid_home() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let check = run_tinybutler(home, &["check"]);
    assert!(check.status.success(), "{}", stderr(&check));
    assert!(stdout(&check).contains("ok: checked config.yaml and 2 task definition"));

    let gitignore = home.join(".gitignore");
    assert!(gitignore.exists(), "init should install home .gitignore");
    let gitignore_text = std::fs::read_to_string(&gitignore).expect("home .gitignore");
    assert!(!gitignore_text.contains("/config.yaml"));
    assert!(gitignore_text.contains("/tasks/*/logs/"));
    assert!(gitignore_text.contains("/tasks/*/state.json"));

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

    let init = run_tinybutler(home, &["init"]);
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

    let init = run_tinybutler(home, &["init"]);
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

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let task_yaml = home.join("tasks/regular-check/task.yaml");
    let mut text = std::fs::read_to_string(&task_yaml).expect("read task yaml");
    text.push_str("workspace: /tmp\n");
    std::fs::write(&task_yaml, text).expect("write invalid task yaml");

    let check = run_tinybutler(home, &["check"]);
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

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    std::fs::remove_file(home.join("tasks/regular-check/run.sh")).expect("remove run.sh");
    let missing_script = run_tinybutler(home, &["check"]);
    assert!(
        !missing_script.status.success(),
        "check should reject command tasks without run.sh"
    );
    assert!(
        stderr(&missing_script).contains("run.sh"),
        "stderr should mention missing run.sh: {}",
        stderr(&missing_script)
    );

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));
    let task_yaml = home.join("tasks/smoke-task/task.yaml");
    let text = std::fs::read_to_string(&task_yaml)
        .expect("read task yaml")
        .replace("runner: gpt-5.3-codex-spark", "runner: missing-runner");
    std::fs::write(&task_yaml, text).expect("write task yaml");

    let missing_runner = run_tinybutler(home, &["check"]);
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

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let run = run_tinybutler_with_input(home, &["tasks"], "regular-check\nrun\n");
    assert!(run.status.success(), "{}", stderr(&run));

    let status = run_tinybutler_with_input(home, &["tasks"], "regular-check\nstatus\n");
    assert!(status.status.success(), "{}", stderr(&status));
    let out = stdout(&status);
    assert!(out.contains("last_status"));
    assert!(out.contains("**run.sh:**"));
    assert!(out.contains("regular-check ok:"));
    assert!(out.contains("**latest_log_first_20_lines:**"));
}

#[test]
fn task_run_maintains_monthly_log_archives() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let logs_dir = home.join("tasks/regular-check/logs");
    std::fs::create_dir_all(&logs_dir).expect("logs dir");
    let completed_log = log_file_name_months_ago(1);
    let expired_log = log_file_name_months_ago(6);
    let completed_archive = archive_file_name_months_ago(1);
    std::fs::write(logs_dir.join(&completed_log), "completed month\n").expect("completed log");
    std::fs::write(logs_dir.join(&expired_log), "expired month\n").expect("expired log");

    let run = run_tinybutler_with_input(home, &["tasks"], "regular-check\nrun\n");
    assert!(run.status.success(), "{}", stderr(&run));

    assert!(logs_dir.join(completed_archive).exists());
    assert!(!logs_dir.join(completed_log).exists());
    assert!(!logs_dir.join(expired_log).exists());
}

#[test]
fn agent_status_shows_prompt_content() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let status = run_tinybutler_with_input(home, &["tasks"], "smoke-task\nstatus\n");
    assert!(status.status.success(), "{}", stderr(&status));
    let out = stdout(&status);

    assert!(out.contains("**agent.md:**"));
    assert!(out.contains("**schedule:** At 09:00. (0 9 * * *)"));
    assert!(out.contains("Inspect this task directory"));
    assert!(out.contains("**state.json:**"));
}

#[test]
fn task_list_uses_chat_readable_multiline_blocks() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let list = run_tinybutler(home, &["tasks"]);
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
fn task_list_and_status_are_script_friendly_read_only_commands() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let list = run_tinybutler(home, &["task", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stdout(&list).contains("regular-check\n  enabled: true"));

    let status = run_tinybutler(home, &["task", "status", "smoke-task"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let out = stdout(&status);
    assert!(out.contains("**Task:** `smoke-task`"));
    assert!(out.contains("**agent.md:**"));
    assert!(out.contains("**state.json:**"));
}

#[test]
fn scripted_task_selector_rejects_zero_index() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let output = run_tinybutler_with_input(home, &["tasks"], "0\n");
    assert!(!output.status.success(), "zero should not select a task");
    assert!(
        stderr(&output).contains("task selection is out of range"),
        "stderr should mention out of range selection: {}",
        stderr(&output)
    );
}

#[test]
fn task_enable_and_disable_update_task_yaml() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let disable = run_tinybutler_with_input(home, &["tasks"], "regular-check\ndisable\n");
    assert!(disable.status.success(), "{}", stderr(&disable));
    assert!(stdout(&disable).contains("regular-check disabled"));

    let disabled_yaml =
        std::fs::read_to_string(home.join("tasks/regular-check/task.yaml")).expect("task yaml");
    assert!(disabled_yaml.contains("enabled: false"));

    let list = run_tinybutler(home, &["tasks"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stdout(&list).contains("regular-check\n  enabled: false"));

    let enable = run_tinybutler_with_input(home, &["tasks"], "regular-check\nenable\n");
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

    let init = run_tinybutler(home, &["init"]);
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

    let run = run_tinybutler_with_input(home, &["tasks"], "regular-check\nrun\n");
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

    let init = run_tinybutler(home, &["init"]);
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

    let list = run_tinybutler(home, &["tasks"]);
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
        let output = run_tinybutler(home, &[command]);
        assert!(
            !output.status.success(),
            "old command should not be accepted: {command}"
        );
    }

    for args in [
        ["task", "run", "regular-check"],
        ["task", "enable", "regular-check"],
        ["task", "disable", "regular-check"],
    ] {
        let output = run_tinybutler(home, &args);
        assert!(
            !output.status.success(),
            "mutating task command should not be accepted: {args:?}"
        );
    }
}

#[test]
fn telegram_poll_is_not_a_public_cli_command() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let output = run_tinybutler(home, &["telegram", "--poll"]);
    assert!(
        !output.status.success(),
        "telegram polling should start from daemon, not --poll"
    );
}

#[test]
fn chat_commands_are_public_cli_surface() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let output = run_tinybutler(home, &["chat", "--help"]);
    assert!(output.status.success(), "{}", stderr(&output));
    let out = stdout(&output);
    assert!(out.contains("new"));
    assert!(out.contains("session"));
}
