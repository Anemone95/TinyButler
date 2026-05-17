//! Process execution for shell and Codex task runners.
//!
//! Runners execute inside the task directory, capture stdout/stderr into a
//! per-run log, and return enough structured output for scheduler state and
//! Telegram notification decisions.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::{CodeAgentConfig, Config};
use crate::state::TaskState;
use crate::task::{SessionMode, Task, TaskType};

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
            let runner = task
                .runner
                .as_deref()
                .context("agent task requires runner")?;
            run_code_agent(config, runner, task, state).await
        }
    };
    let duration_seconds = started.elapsed().as_secs();

    let (status, exit_code, stdout, stderr, session_id) = match output {
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
            )
        }
        Err(err) => (
            "failed".to_string(),
            None,
            String::new(),
            format!("{err:#}"),
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
}

async fn run_shell(task: &Task) -> Result<ProcessOutput> {
    let script = task.run_script_path();
    if !script.exists() {
        bail!("missing {}", script.display());
    }

    let mut command = Command::new("bash");
    command.arg(script);
    command.current_dir(&task.dir);
    command.kill_on_drop(true);
    run_command(command, None, task.timeout).await
}

/// Run a configured code-agent command with `agent.md` as the task prompt.
async fn run_code_agent(
    config: &Config,
    runner_key: &str,
    task: &Task,
    state: &TaskState,
) -> Result<ProcessOutput> {
    let agent = config
        .code_agents
        .get(runner_key)
        .with_context(|| format!("missing code_agents.{runner_key} in config.yaml"))?;
    let prompt_path = task.agent_path();
    let prompt = fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("failed to read {}", prompt_path.display()))?;

    if task.session == SessionMode::Reuse {
        if let Some(session_id) = state.session_id.as_deref().filter(|s| !s.is_empty()) {
            if !agent.resume_args.is_empty() {
                return run_agent_command(
                    agent,
                    &agent.resume_args,
                    task,
                    &prompt,
                    Some(session_id),
                )
                .await;
            }
        }
    }

    run_agent_command(agent, &agent.args, task, &prompt, None).await
}

/// Build a user-configured agent command and feed the prompt on stdin unless
/// the argument template explicitly embeds `{prompt}`.
async fn run_agent_command(
    agent: &CodeAgentConfig,
    args: &[String],
    task: &Task,
    prompt: &str,
    session_id: Option<&str>,
) -> Result<ProcessOutput> {
    let mut command = Command::new(&agent.command);
    command.current_dir(&task.dir);
    command.kill_on_drop(true);

    let mut prompt_in_args = false;
    for arg in args {
        if arg.contains("{prompt}") {
            prompt_in_args = true;
        }
        command.arg(expand_agent_arg(arg, task, prompt, session_id));
    }

    let stdin_text = if prompt_in_args {
        None
    } else {
        Some(prompt.to_string())
    };
    run_command(command, stdin_text, task.timeout).await
}

/// Expand placeholders supported by local code-agent command templates.
fn expand_agent_arg(arg: &str, task: &Task, prompt: &str, session_id: Option<&str>) -> String {
    arg.replace("{prompt}", prompt)
        .replace("{sessionId}", session_id.unwrap_or(""))
        .replace("{taskDir}", &task.dir.display().to_string())
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
    if let Some(text) = stdin_text {
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
        }
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

/// Extract Codex session identifiers from newline-delimited JSON output.
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
