use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use chrono::Local;
use serde_json::Value;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::state::TaskState;
use crate::task::{RunnerKind, SessionMode, Task, TaskType};

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub status: String,
    pub exit_code: Option<i32>,
    pub duration_seconds: u64,
    pub log_path: PathBuf,
    pub log_relative_path: String,
    pub summary: String,
    pub session_id: Option<String>,
}

pub async fn run_task(task: &Task, state: &TaskState) -> Result<RunOutcome> {
    fs::create_dir_all(task.logs_dir()).await?;
    let log_relative_path = format!("logs/{}.log", Local::now().format("%Y-%m-%dT%H-%M-%S"));
    let log_path = task.dir.join(&log_relative_path);

    let started = Instant::now();
    let output = match (&task.task_type, &task.runner) {
        (TaskType::Command, RunnerKind::Shell) => run_shell(task).await,
        (TaskType::Agent, RunnerKind::Codex) => run_codex(task, state).await,
        _ => bail!("unsupported task type/runner combination for {}", task.name),
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
    })
}

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

    let cwd = task.workspace.as_ref().unwrap_or(&task.dir);
    let mut command = Command::new("bash");
    command.arg(script);
    command.current_dir(cwd);
    command.kill_on_drop(true);
    run_command(command, None, task.timeout).await
}

async fn run_codex(task: &Task, state: &TaskState) -> Result<ProcessOutput> {
    let prompt_path = task.agent_path();
    let prompt = fs::read_to_string(&prompt_path)
        .await
        .with_context(|| format!("failed to read {}", prompt_path.display()))?;
    let workspace = task
        .workspace
        .as_ref()
        .context("agent task requires workspace")?;

    let mut command = Command::new("codex");
    command.current_dir(workspace);
    command.kill_on_drop(true);

    if task.session == SessionMode::Reuse {
        if let Some(session_id) = state.session_id.as_deref().filter(|s| !s.is_empty()) {
            command.args(["exec", "resume", "--json", "--color", "never"]);
            command.args(["-c", &format!("sandbox_mode=\"{}\"", task.codex.sandbox)]);
            if let Some(model) = &task.codex.model {
                command.args(["-m", model]);
            }
            if let Some(service_tier) = &task.codex.service_tier {
                command.args(["-c", &format!("service_tier=\"{}\"", service_tier)]);
            }
            command.arg("--skip-git-repo-check");
            command.arg(session_id);
            command.arg("-");
            return run_command(command, Some(prompt), task.timeout).await;
        }
    }

    command.args([
        "exec",
        "--json",
        "--color",
        "never",
        "--sandbox",
        &task.codex.sandbox,
        "--cd",
    ]);
    command.arg(workspace);
    command.arg("--skip-git-repo-check");
    if let Some(model) = &task.codex.model {
        command.args(["-m", model]);
    }
    if let Some(service_tier) = &task.codex.service_tier {
        command.args(["-c", &format!("service_tier=\"{}\"", service_tier)]);
    }
    command.arg("-");

    run_command(command, Some(prompt), task.timeout).await
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
