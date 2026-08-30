//! Process execution for shell and Codex task runners.
//!
//! Runners execute inside the task directory, capture stdout/stderr into a
//! per-run log, and return enough structured output for scheduler state and
//! Telegram notification decisions.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Local;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{Config, PromptMode, ResolvedCodeAgent, prompt_mode_for_args};
use crate::state::TaskState;
use crate::task::{SessionMode, Task, TaskType};

const AGENT_TASK_CONTEXT: &str =
    "now run under tinybutler, message reply should use tinybutler skills";

/// Structured result of one scheduler execution attempt.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// Stable status string written to `state.json`.
    pub status: String,
    /// Process exit code, or `None` for spawn/timeout/lock failures.
    pub exit_code: Option<i32>,
    /// Wall-clock duration rounded down to seconds.
    pub duration_seconds: u64,
    /// Absolute path to the generated log file.
    pub log_path: PathBuf,
    /// Path relative to the task directory for `state.json`.
    pub log_relative_path: String,
    /// Short text used by notifications and status previews.
    pub summary: String,
    /// Agent session id extracted from structured runner output, when present.
    pub session_id: Option<String>,
    /// Code-agent model name that produced this output.
    pub agent_runner: Option<String>,
    /// Captured stdout from the runner process.
    pub stdout: String,
}

/// Execute one task and persist a log under the task's `logs/` directory.
pub async fn run_task(config: &Config, task: &Task, state: &TaskState) -> Result<RunOutcome> {
    fs::create_dir_all(task.logs_dir()).await?;
    let log_relative_path = format!("logs/{}.log", Local::now().format("%Y-%m-%dT%H-%M-%S"));
    let log_path = task.dir.join(&log_relative_path);

    let started = Instant::now();
    let output = match task.task_type {
        TaskType::Command => run_shell(task).await,
        TaskType::Agent => {
            let runners = task.agent_runner_keys();
            run_code_agents(config, &runners, task, state).await
        }
    };
    let duration_seconds = started.elapsed().as_secs();

    let (status, exit_code, stdout, stderr, session_id, agent_runner) = match output {
        Ok(output) => {
            let status = if output.exit_code == Some(0) {
                "success".to_string()
            } else {
                "failed".to_string()
            };
            (
                status,
                output.exit_code,
                output.stdout,
                output.stderr,
                output.session_id,
                output.agent_runner,
            )
        }
        Err(err) => (
            "failed".to_string(),
            None,
            String::new(),
            format!("{err:#}"),
            None,
            None,
        ),
    };

    let summary = summarize(&stdout, &stderr);
    let log = format!(
        "task: {}\nstatus: {}\nexit_code: {:?}\nduration_seconds: {}\nstarted_at: {}\n\n--- stdout ---\n{}\n\n--- stderr ---\n{}\n",
        task.name,
        status,
        exit_code,
        duration_seconds,
        Local::now().to_rfc3339(),
        stdout,
        stderr,
    );
    fs::write(&log_path, log).await?;

    Ok(RunOutcome {
        status,
        exit_code,
        duration_seconds,
        log_path,
        log_relative_path,
        summary,
        session_id,
        agent_runner,
        stdout,
    })
}

/// Create a failed run outcome without starting a runner process.
pub async fn run_failure(task: &Task, status: &str, stderr: &str) -> Result<RunOutcome> {
    fs::create_dir_all(task.logs_dir()).await?;
    let log_relative_path = format!("logs/{}.log", Local::now().format("%Y-%m-%dT%H-%M-%S"));
    let log_path = task.dir.join(&log_relative_path);
    let summary = summarize("", stderr);
    let log = format!(
        "task: {}\nstatus: {}\nexit_code: {:?}\nduration_seconds: 0\nstarted_at: {}\n\n--- stdout ---\n\n\n--- stderr ---\n{}\n",
        task.name,
        status,
        None::<i32>,
        Local::now().to_rfc3339(),
        stderr,
    );
    fs::write(&log_path, log).await?;

    Ok(RunOutcome {
        status: status.to_string(),
        exit_code: None,
        duration_seconds: 0,
        log_path,
        log_relative_path,
        summary,
        session_id: None,
        agent_runner: None,
        stdout: String::new(),
    })
}

/// Raw child-process output before TinyButler maps it to task state.
#[derive(Debug)]
struct ProcessOutput {
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    session_id: Option<String>,
    agent_runner: Option<String>,
}

async fn run_shell(task: &Task) -> Result<ProcessOutput> {
    let script = task.run_script_path();
    if !script.exists() {
        bail!("missing {}", script.display());
    }

    let mut command = Command::new("bash");
    command.arg(script);
    command.current_dir(&task.dir);
    apply_task_environment(&mut command, task);
    command.kill_on_drop(true);
    run_command(command, None, task.timeout).await
}

/// Run configured code-agent commands with `agent.md` as the task prompt.
async fn run_code_agents(
    config: &Config,
    runner_keys: &[&str],
    task: &Task,
    state: &TaskState,
) -> Result<ProcessOutput> {
    if runner_keys.is_empty() {
        bail!("agent task requires at least one runner");
    }
    let prompt_path = task.agent_path();
    let prompt = fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("failed to read {}", prompt_path.display()))?;
    let prompt = agent_task_prompt(&prompt);

    let mut failed_stdout = String::new();
    let mut failed_stderr = String::new();
    let mut last_exit_code = None;

    for runner_key in runner_keys {
        let output = run_code_agent(config, runner_key, task, state, &prompt).await;
        match output {
            Ok(output) if output.exit_code == Some(0) => {
                if failed_stdout.is_empty() && failed_stderr.is_empty() {
                    return Ok(output);
                }
                return Ok(output_with_previous_attempts(
                    runner_key,
                    output,
                    failed_stdout,
                    failed_stderr,
                ));
            }
            Ok(output) => {
                last_exit_code = output.exit_code;
                append_attempt_output(&mut failed_stdout, &mut failed_stderr, runner_key, &output);
            }
            Err(err) => {
                append_attempt_error(&mut failed_stderr, runner_key, &err);
            }
        }
    }

    Ok(ProcessOutput {
        exit_code: last_exit_code,
        stdout: failed_stdout,
        stderr: failed_stderr,
        session_id: None,
        agent_runner: None,
    })
}

/// Run one configured code-agent command.
async fn run_code_agent(
    config: &Config,
    model_name: &str,
    task: &Task,
    state: &TaskState,
    prompt: &str,
) -> Result<ProcessOutput> {
    let agent = config
        .code_agent_for_model(model_name)
        .with_context(|| format!("missing code_agents model {model_name} in config.yaml"))?;
    if task.session == SessionMode::Reuse
        && let Some(session_id) = reusable_session_id(state, model_name)
        && !agent.config.resume_args.is_empty()
    {
        let mut output = run_agent_command(
            &agent,
            &agent.config.resume_args,
            task,
            prompt,
            Some(session_id),
        )
        .await?;
        output.agent_runner = Some(model_name.to_string());
        return Ok(output);
    }

    let mut output = run_agent_command(&agent, &agent.config.new_args, task, prompt, None).await?;
    output.agent_runner = Some(model_name.to_string());
    Ok(output)
}

/// Build a user-configured agent command and deliver the prompt through the
/// template's explicit `{prompt}` or `{stdin}` marker.
async fn run_agent_command(
    agent: &ResolvedCodeAgent<'_>,
    args: &[String],
    task: &Task,
    prompt: &str,
    session_id: Option<&str>,
) -> Result<ProcessOutput> {
    let mut command = Command::new(&agent.config.command);
    command.current_dir(&task.dir);
    apply_task_environment(&mut command, task);
    command.kill_on_drop(true);

    let prompt_mode = prompt_mode_for_args(args)?;
    for arg in args {
        if prompt_mode == PromptMode::Stdin && arg == "{stdin}" {
            continue;
        }
        command.arg(expand_agent_arg(
            arg,
            &agent.model,
            task,
            prompt,
            session_id,
        ));
    }

    let stdin_text = (prompt_mode == PromptMode::Stdin).then(|| prompt.to_string());
    run_command(command, stdin_text, task.timeout).await
}

fn reusable_session_id<'a>(state: &'a TaskState, runner_key: &str) -> Option<&'a str> {
    let session_id = state.session_id.as_deref().filter(|s| !s.is_empty())?;
    match state.session_runner.as_deref() {
        Some(session_runner) if session_runner != runner_key => None,
        _ => Some(session_id),
    }
}

fn output_with_previous_attempts(
    runner_key: &str,
    output: ProcessOutput,
    mut stdout: String,
    mut stderr: String,
) -> ProcessOutput {
    append_attempt_output(&mut stdout, &mut stderr, runner_key, &output);
    ProcessOutput {
        stdout,
        stderr,
        ..output
    }
}

fn append_attempt_output(
    stdout: &mut String,
    stderr: &mut String,
    runner_key: &str,
    output: &ProcessOutput,
) {
    if !output.stdout.is_empty() {
        push_attempt_section(stdout, runner_key, "stdout", &output.stdout);
    }
    let stderr_header = format!("stderr exit_code={:?}", output.exit_code);
    push_attempt_section(stderr, runner_key, &stderr_header, &output.stderr);
}

fn append_attempt_error(stderr: &mut String, runner_key: &str, err: &anyhow::Error) {
    push_attempt_section(stderr, runner_key, "error", &format!("{err:#}"));
}

fn push_attempt_section(target: &mut String, runner_key: &str, label: &str, text: &str) {
    if !target.is_empty() && !target.ends_with('\n') {
        target.push('\n');
    }
    target.push_str(&format!("--- agent {runner_key} {label} ---\n"));
    target.push_str(text);
    if !target.ends_with('\n') {
        target.push('\n');
    }
}

fn apply_task_environment(command: &mut Command, task: &Task) {
    command.env("TINYBUTLER_TASK_NAME", &task.name);
}

/// Expand placeholders supported by local code-agent command templates.
fn expand_agent_arg(
    arg: &str,
    model: &str,
    task: &Task,
    prompt: &str,
    session_id: Option<&str>,
) -> String {
    arg.replace("{prompt}", prompt)
        .replace("{model}", model)
        .replace("{sessionId}", session_id.unwrap_or(""))
        .replace("{taskDir}", &task.dir.display().to_string())
}

fn agent_task_prompt(prompt: &str) -> String {
    format!("{AGENT_TASK_CONTEXT}\n\n{prompt}")
}

async fn run_command(
    mut command: Command,
    stdin_text: Option<String>,
    timeout_seconds: u64,
) -> Result<ProcessOutput> {
    command.stdin(std::process::Stdio::piped());
    command.stdout(std::process::Stdio::piped());
    command.stderr(std::process::Stdio::piped());

    let mut child = command.spawn().context("failed to spawn process")?;
    if let Some(text) = stdin_text
        && let Some(mut stdin) = child.stdin.take()
    {
        stdin.write_all(text.as_bytes()).await?;
    }

    let output = match tokio::time::timeout(
        Duration::from_secs(timeout_seconds),
        child.wait_with_output(),
    )
    .await
    {
        Ok(output) => output?,
        Err(_) => bail!("process timed out after {timeout_seconds}s"),
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let session_id = extract_session_id(&stdout);

    Ok(ProcessOutput {
        exit_code: output.status.code(),
        stdout,
        stderr,
        session_id,
        agent_runner: None,
    })
}

/// Prefer stdout summaries, falling back to stderr for failures.
fn summarize(stdout: &str, stderr: &str) -> String {
    let source = if !stdout.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    let lines = source
        .lines()
        .rev()
        .filter(|line| !line.trim().is_empty())
        .take(12)
        .collect::<Vec<_>>();
    lines.into_iter().rev().collect::<Vec<_>>().join("\n")
}

/// Extract common code-agent session identifiers from JSON output.
fn extract_session_id(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(session_id) =
            find_string_key(&value, &["session_id", "sessionId", "conversation_id"])
        {
            return Some(session_id);
        }
    }
    None
}

/// Recursively find a string field in arbitrary JSON runner output.
fn find_string_key(value: &Value, keys: &[&str]) -> Option<String> {
    match value {
        Value::Object(map) => {
            for key in keys {
                if let Some(Value::String(value)) = map.get(*key) {
                    return Some(value.clone());
                }
            }
            map.values().find_map(|value| find_string_key(value, keys))
        }
        Value::Array(values) => values.iter().find_map(|value| find_string_key(value, keys)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use crate::config::CodeAgentConfig;

    #[test]
    fn agent_task_prompt_includes_tinybutler_context() {
        let prompt = agent_task_prompt("Inspect task state.");

        assert!(prompt.starts_with(AGENT_TASK_CONTEXT));
        assert!(prompt.contains("\n\nInspect task state."));
    }

    #[tokio::test]
    async fn shell_tasks_receive_current_task_name_environment() {
        let temp = tempfile::tempdir().expect("temp dir");
        let task = Task {
            name: "env-task".to_string(),
            enabled: true,
            schedule: "0 * * * *".to_string(),
            agents: Vec::new(),
            task_type: TaskType::Command,
            session: SessionMode::Independent,
            timeout: 30,
            dir: temp.path().to_path_buf(),
        };
        fs::write(
            task.run_script_path(),
            "#!/usr/bin/env bash\nprintf '%s\\n' \"$TINYBUTLER_TASK_NAME\"\n",
        )
        .await
        .expect("write run.sh");

        let output = run_shell(&task).await.expect("run shell task");

        assert_eq!(output.stdout.trim(), "env-task");
    }

    #[tokio::test]
    async fn agent_tasks_fall_back_until_one_runner_succeeds() {
        let temp = tempfile::tempdir().expect("temp dir");
        let task = Task {
            name: "fallback-task".to_string(),
            enabled: true,
            schedule: "0 * * * *".to_string(),
            agents: vec![
                "fail/fail-agent".to_string(),
                "success/success-agent".to_string(),
            ],
            task_type: TaskType::Agent,
            session: SessionMode::Independent,
            timeout: 30,
            dir: temp.path().to_path_buf(),
        };
        fs::write(task.agent_path(), "Report status.")
            .await
            .expect("write agent.md");

        let mut code_agents = BTreeMap::new();
        code_agents.insert(
            "fail".to_string(),
            CodeAgentConfig {
                command: "/bin/sh".to_string(),
                new_args: vec![
                    "-c".to_string(),
                    "printf 'first failed\\n'; printf 'bad agent\\n' >&2; exit 7".to_string(),
                    "{prompt}".to_string(),
                ],
                models: vec!["fail-agent".to_string()],
                ..Default::default()
            },
        );
        code_agents.insert(
            "success".to_string(),
            CodeAgentConfig {
                command: "/bin/sh".to_string(),
                new_args: vec![
                    "-c".to_string(),
                    "printf '{\"session_id\":\"session-ok\"}\\n'".to_string(),
                    "{prompt}".to_string(),
                ],
                models: vec!["success-agent".to_string()],
                ..Default::default()
            },
        );
        let config = Config {
            home: temp.path().to_path_buf(),
            telegram: Default::default(),
            code_agents,
        };

        let outcome = run_task(&config, &task, &TaskState::default())
            .await
            .expect("run task");

        assert_eq!(outcome.status, "success");
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.session_id.as_deref(), Some("session-ok"));
        assert_eq!(
            outcome.agent_runner.as_deref(),
            Some("success/success-agent")
        );
        assert!(
            outcome
                .stdout
                .contains("--- agent fail/fail-agent stdout ---")
        );
        assert!(
            outcome
                .stdout
                .contains("--- agent success/success-agent stdout ---")
        );
        let log = fs::read_to_string(outcome.log_path)
            .await
            .expect("read log");
        assert!(log.contains("--- agent fail/fail-agent stderr exit_code=Some(7) ---"));
    }
}
