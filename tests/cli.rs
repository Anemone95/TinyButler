//! End-to-end CLI tests for the public TinyButler command surface.
//!
//! These tests exercise the installed binary through Cargo's test-provided
//! executable path so command parsing, init templates, check, and task commands
//! are verified together.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration as StdDuration, Instant};

use chrono::{Datelike, Duration, Local, Months};
use serde_json::json;
use tinybutler::config::DaemonPidRecord;

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

/// Wait for the daemon to write parseable pid metadata.
fn wait_for_pid_record(path: &Path) -> DaemonPidRecord {
    let deadline = Instant::now() + StdDuration::from_secs(5);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(record) = serde_yaml::from_str::<DaemonPidRecord>(&text) {
                return record;
            }
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for daemon pid record at {}",
            path.display()
        );
        std::thread::sleep(StdDuration::from_millis(50));
    }
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
    assert!(
        home.join(".agents/skills/tinybutler-operations/SKILL.md")
            .exists()
    );

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
fn init_refreshes_repo_scoped_operation_skill() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let installed_skill = home.join(".agents/skills/tinybutler-operations/SKILL.md");
    std::fs::write(&installed_skill, "stale skill\n").expect("write stale skill");

    let refresh = run_tinybutler(home, &["init"]);
    assert!(refresh.status.success(), "{}", stderr(&refresh));

    let template_skill = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("templates/.agents/skills/tinybutler-operations/SKILL.md");
    assert_eq!(
        std::fs::read_to_string(&installed_skill).expect("installed skill"),
        std::fs::read_to_string(&template_skill).expect("template skill")
    );
}

#[test]
fn init_codex_group_reads_prompt_from_stdin_and_emit_json() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let config_text = std::fs::read_to_string(home.join("config.yaml")).expect("config");
    let config: serde_yaml::Value = serde_yaml::from_str(&config_text).expect("parse config");
    let codex = &config["code_agents"]["codex"];
    let models = codex["models"].as_sequence().expect("codex models");
    assert!(
        models
            .iter()
            .any(|value| value.as_str() == Some("gpt-5.3-codex-spark"))
    );
    assert!(models.iter().any(|value| value.as_str() == Some("gpt-5.5")));

    for field in ["new_args", "resume_args"] {
        let args = codex[field].as_sequence().expect(field);

        assert!(
            args.iter().any(|value| value.as_str() == Some("--json")),
            "codex {field} should emit JSONL"
        );
        assert!(
            args.iter().any(|value| value.as_str() == Some("{model}")),
            "codex {field} should use the selected model placeholder"
        );
        assert_eq!(
            args.last().and_then(|value| value.as_str()),
            Some("stdin"),
            "codex {field} should mark prompt delivery through stdin"
        );
    }
}

#[test]
fn check_rejects_agent_config_without_prompt_or_stdin() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let config_text = std::fs::read_to_string(home.join("config.yaml"))
        .expect("read config")
        .replace("      - stdin", "      - --no-prompt-placeholder");
    std::fs::write(home.join("config.yaml"), config_text).expect("write config");

    let check = run_tinybutler(home, &["check"]);
    assert!(
        !check.status.success(),
        "check should reject agent args without prompt or stdin"
    );
    let error = stderr(&check);
    assert!(
        error.contains("must contain {prompt} or end with stdin"),
        "stderr should explain prompt delivery requirement: {error}"
    );
}

#[test]
fn check_rejects_referenced_agent_with_empty_new_args() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    std::fs::write(
        home.join("config.yaml"),
        r#"telegram:
  bot_token: null
  chat_id: null

code_agents:
  codex:
    command: /bin/true
    models:
      - gpt-5.3-codex-spark
    new_args: []
"#,
    )
    .expect("write config");

    let check = run_tinybutler(home, &["check"]);
    assert!(
        !check.status.success(),
        "check should reject task-referenced agent models without runnable new_args"
    );
    let error = stderr(&check);
    assert!(
        error.contains("non-runnable code_agents model gpt-5.3-codex-spark"),
        "stderr should mention the non-runnable model: {error}"
    );
    assert!(
        error.contains("must contain {prompt} or end with stdin"),
        "stderr should explain prompt delivery requirement: {error}"
    );
}

#[test]
fn init_stream_runners_include_complete_streaming_flags() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let config_text = std::fs::read_to_string(home.join("config.yaml")).expect("config");
    let config: serde_yaml::Value = serde_yaml::from_str(&config_text).expect("parse config");

    let gemini_stream_args = config["code_agents"]["gemini"]["stream_args"]
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

    let codex_stream_args = config["code_agents"]["codex"]["stream_args"]
        .as_sequence()
        .expect("codex stream args");
    let codex_stream_arg_text = codex_stream_args
        .iter()
        .filter_map(|value| value.as_str())
        .collect::<Vec<_>>();
    assert!(
        codex_stream_arg_text.contains(&"app-server"),
        "codex streaming should use app-server"
    );
    assert!(
        codex_stream_arg_text.contains(&"model=\"{model}\""),
        "codex streaming should set model through the selected model placeholder"
    );
    assert!(
        codex_stream_arg_text.contains(&"sandbox_mode=\"danger-full-access\""),
        "codex streaming should set sandbox mode"
    );
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
        .replace("  - gpt-5.3-codex-spark", "  - missing-runner");
    std::fs::write(&task_yaml, text).expect("write task yaml");

    let missing_runner = run_tinybutler(home, &["check"]);
    assert!(
        !missing_runner.status.success(),
        "check should reject agent tasks with unknown runners"
    );
    assert!(
        stderr(&missing_runner).contains("code_agents model missing-runner"),
        "stderr should mention missing runner: {}",
        stderr(&missing_runner)
    );
}

#[test]
fn restart_checks_config_and_tasks_before_signaling_daemon() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let task_yaml = home.join("tasks/regular-check/task.yaml");
    let mut text = std::fs::read_to_string(&task_yaml).expect("read task yaml");
    text.push_str("workspace: /tmp\n");
    std::fs::write(&task_yaml, text).expect("write invalid task yaml");

    let output = run_tinybutler(home, &["restart"]);

    assert!(
        !output.status.success(),
        "restart should fail invalid check"
    );
    assert!(
        stderr(&output).contains("workspace"),
        "stderr should explain check failure: {}",
        stderr(&output)
    );
    assert!(
        !stderr(&output).contains("failed to read"),
        "restart should not read or signal daemon pid after failed check: {}",
        stderr(&output)
    );
}

#[test]
fn restart_execs_running_daemon_without_systemctl_restart() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let mut daemon = Command::new(tinybutler_bin())
        .arg("--home")
        .arg(home)
        .args(["daemon", "--interval-seconds", "60"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start daemon");

    let pid_path = home.join("tinybutler.pid");
    let first = wait_for_pid_record(&pid_path);

    let output = run_tinybutler(home, &["restart"]);
    if !output.status.success() {
        let _ = daemon.kill();
        let _ = daemon.wait();
    }
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("Restarted TinyButler daemon"),
        "stdout should report completed restart: {}",
        stdout(&output)
    );

    let second = wait_for_pid_record(&pid_path);
    assert_eq!(
        second.pid, first.pid,
        "restart should exec the same daemon pid"
    );
    assert_ne!(
        second.launched_at_unix_nanos, first.launched_at_unix_nanos,
        "daemon should rewrite pid metadata after exec"
    );
    assert!(
        daemon.try_wait().expect("daemon status").is_none(),
        "daemon should still be running after in-place restart"
    );

    daemon.kill().expect("stop daemon");
    let _ = daemon.wait();
}

#[test]
fn agent_task_tries_agents_in_order_until_one_succeeds() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));
    std::fs::remove_dir_all(home.join("tasks/smoke-task")).expect("remove template agent task");

    std::fs::write(
        home.join("config.yaml"),
        r#"telegram:
  bot_token: null
  chat_id: null

code_agents:
  fail-agent:
    command: /bin/sh
    models:
      - fail-agent
    new_args:
      - "-c"
      - |
        printf 'first stdout\n'
        printf 'first stderr\n' >&2
        exit 7
      - "{prompt}"
  success-agent:
    command: /bin/sh
    models:
      - success-agent
    new_args:
      - "-c"
      - |
        printf '{"session_id":"session-ok"}\n'
      - "{prompt}"
"#,
    )
    .expect("write config");

    let task_dir = home.join("tasks/fallback-task");
    std::fs::create_dir_all(&task_dir).expect("task dir");
    std::fs::write(
        task_dir.join("task.yaml"),
        r#"name: fallback-task
enabled: false
schedule: "0 9 * * *"
agents:
  - fail-agent
  - success-agent
type: agent
session: independent
timeout: 30
"#,
    )
    .expect("write task yaml");
    std::fs::write(task_dir.join("agent.md"), "Run the fallback test.\n").expect("agent md");

    let check = run_tinybutler(home, &["check"]);
    assert!(check.status.success(), "{}", stderr(&check));

    let list = run_tinybutler(home, &["task", "list"]);
    assert!(list.status.success(), "{}", stderr(&list));
    assert!(stdout(&list).contains("agents: fail-agent -> success-agent"));

    let run = run_tinybutler_with_input(home, &["tasks"], "fallback-task\nrun\n");
    assert!(run.status.success(), "{}", stderr(&run));

    let state: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(task_dir.join("state.json")).expect("read state"),
    )
    .expect("parse state");
    assert_eq!(state["last_status"], "success");
    assert_eq!(state["session_id"], "session-ok");
    assert_eq!(state["session_runner"], "success-agent");
    assert_eq!(state["failure_count"], 0);

    let latest_log = state["last_log"].as_str().expect("last log");
    let log = std::fs::read_to_string(task_dir.join(latest_log)).expect("read latest log");
    assert!(log.contains("--- agent fail-agent stdout ---"));
    assert!(log.contains("--- agent fail-agent stderr exit_code=Some(7) ---"));
    assert!(log.contains("--- agent success-agent stdout ---"));
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
    assert!(out.contains("**last_status:** `success`"));
    assert!(!out.contains("**run.sh:**"));
    assert!(!out.contains("**state.json:**"));
    assert!(out.contains("regular-check ok:"));
    assert!(out.contains("**latest_log_preview:**"));
}

#[test]
fn task_status_compacts_and_truncates_latest_log_preview() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let task_dir = home.join("tasks/smoke-task");
    let logs_dir = task_dir.join("logs");
    std::fs::create_dir_all(&logs_dir).expect("logs dir");

    let long_line = "x".repeat(100);
    let mut log_lines = vec![long_line.clone()];
    log_lines.extend((2..=20).map(|index| format!("line-{index:02}")));
    std::fs::write(logs_dir.join("long.log"), log_lines.join("\n")).expect("write log");

    std::fs::write(
        task_dir.join("state.json"),
        serde_json::to_string_pretty(&json!({
            "last_status": "success",
            "last_exit_code": 0,
            "last_log": "logs/long.log",
            "running": false,
            "run_count": 1,
            "failure_count": 0
        }))
        .expect("state json"),
    )
    .expect("write state");

    let status = run_tinybutler(home, &["task", "status", "smoke-task"]);
    assert!(status.status.success(), "{}", stderr(&status));
    let out = stdout(&status);

    let truncated_long_line = format!("{}...", "x".repeat(77));
    assert!(out.contains("**latest_log_preview:**"));
    assert!(out.contains(&truncated_long_line));
    assert!(!out.contains(&long_line));
    assert!(out.contains("line-07"));
    assert!(out.contains("\n...\n"));
    assert!(!out.contains("line-08"));
    assert!(!out.contains("line-13"));
    assert!(out.contains("line-14"));
    assert!(out.contains("line-20"));
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
fn selected_task_detail_formats_task_fields_without_file_contents() {
    let temp = tempfile::tempdir().expect("temp dir");
    let home = temp.path();

    let init = run_tinybutler(home, &["init"]);
    assert!(init.status.success(), "{}", stderr(&init));

    let detail = run_tinybutler_with_input(home, &["tasks"], "smoke-task\n");
    assert!(detail.status.success(), "{}", stderr(&detail));
    let out = stdout(&detail);

    assert!(out.contains("**Task:** `smoke-task`"));
    assert!(out.contains("**type:** `Agent`"));
    assert!(out.contains("**schedule:** At 09:00. (0 9 * * *)"));
    assert!(!out.contains("**agent.md:**"));
    assert!(!out.contains("**task.yaml:**"));
    assert!(!out.contains("Inspect this task directory"));
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
    assert!(out.contains("\n  agents: shell\n"));
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
    assert!(out.contains("**Task status:** `smoke-task`"));
    assert!(out.contains("**last_status:**"));
    assert!(!out.contains("**agent.md:**"));
    assert!(!out.contains("**state.json:**"));
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
