//! Antigravity CLI implementation of TinyButler's interactive chat adapter.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use chrono::Local;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use crate::config::ResolvedCodeAgent;

use super::{
    AbortOutcome, ChatAgent, ChatEvent, ChatInstructionContext, ChatSession,
    chat_bridge_instructions, expand_chat_stream_args, summarize_user_message,
};

/// True when this module owns the configured executable.
pub(super) fn recognizes_command(command: &str) -> bool {
    Path::new(command)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("agy")
}

/// Antigravity CLI adapter backed by multi-turn NDJSON over standard I/O.
pub(super) struct AgyChatAgent {
    runner: String,
    command: String,
    model: String,
    stream_args: Vec<String>,
    working_directory: Option<PathBuf>,
    instruction_context: ChatInstructionContext,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<Lines<BufReader<ChildStdout>>>,
    active_session: Option<ChatSession>,
    known_sessions: Vec<ChatSession>,
    turn_active: bool,
    abort_requested: bool,
}

impl AgyChatAgent {
    /// Create an Agy adapter from an explicitly selected runner entry.
    pub(super) fn connect_with_context(
        runner: impl Into<String>,
        agent: ResolvedCodeAgent<'_>,
        working_directory: Option<PathBuf>,
        instruction_context: ChatInstructionContext,
    ) -> Result<Self> {
        expand_chat_stream_args(&agent.config.stream_args, &agent.model)?;
        Ok(Self {
            runner: runner.into(),
            command: agent.config.command.clone(),
            model: agent.model,
            stream_args: agent.config.stream_args.clone(),
            working_directory,
            instruction_context,
            child: None,
            stdin: None,
            stdout: None,
            active_session: None,
            known_sessions: Vec::new(),
            turn_active: false,
            abort_requested: false,
        })
    }

    async fn start_process(&mut self, expected_session_id: Option<&str>) -> Result<ChatSession> {
        self.stop_process().await?;

        let mut command = Command::new(&self.command);
        command
            .args(process_args(
                &self.stream_args,
                &self.model,
                expected_session_id,
            )?)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(directory) = &self.working_directory {
            command.current_dir(directory);
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("failed to start agy stream command {}", self.command))?;
        let stdin = child
            .stdin
            .take()
            .context("agy stream has no standard input")?;
        let stdout = child
            .stdout
            .take()
            .context("agy stream has no standard output")?;
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout).lines());

        loop {
            let Some(output) = self.next_protocol_output().await? else {
                self.clear_process();
                bail!("agy stream closed before emitting an init event");
            };
            match output {
                AgyOutputEvent::Init { conversation_id } => {
                    if let Some(expected) = expected_session_id
                        && conversation_id != expected
                    {
                        self.stop_process().await?;
                        bail!("agy resumed conversation {conversation_id}, expected {expected}");
                    }
                    let mut session = self
                        .known_sessions
                        .iter()
                        .find(|session| session.session_id == conversation_id)
                        .cloned()
                        .unwrap_or_else(|| ChatSession {
                            runner: self.runner.clone(),
                            session_id: conversation_id.clone(),
                            title: None,
                            last_activity_at: Local::now(),
                        });
                    session.last_activity_at = Local::now();
                    return Ok(self.record_session(session));
                }
                AgyOutputEvent::Result { status, error, .. } => {
                    self.stop_process().await?;
                    bail!(
                        "agy stream failed before initialization with status {status}: {}",
                        error.unwrap_or_else(|| "no error details".to_string())
                    );
                }
                AgyOutputEvent::Chat(_) | AgyOutputEvent::Ignored => {}
            }
        }
    }

    fn record_session(&mut self, session: ChatSession) -> ChatSession {
        if let Some(existing) = self
            .known_sessions
            .iter_mut()
            .find(|known| known.session_id == session.session_id)
        {
            *existing = session.clone();
        } else {
            self.known_sessions.push(session.clone());
        }
        self.active_session = Some(session.clone());
        session
    }

    async fn ensure_process(&mut self) -> Result<()> {
        let running = match self.child.as_mut() {
            Some(child) => child
                .try_wait()
                .context("failed to inspect agy stream process")?
                .is_none(),
            None => false,
        };
        if running {
            return Ok(());
        }
        self.clear_process();
        let session_id = self
            .active_session
            .as_ref()
            .map(|session| session.session_id.clone())
            .context("no active agy chat session")?;
        self.start_process(Some(&session_id)).await?;
        Ok(())
    }

    async fn write_user_message(&mut self, content: &str) -> Result<()> {
        if self.turn_active {
            bail!("agy chat turn is already active");
        }
        self.ensure_process().await?;
        let mut line = serde_json::to_vec(&serde_json::json!({
            "event": "user",
            "message": { "content": content },
        }))?;
        line.push(b'\n');
        let stdin = self.stdin.as_mut().context("agy stream is not running")?;
        stdin
            .write_all(&line)
            .await
            .context("failed to write agy user message")?;
        stdin
            .flush()
            .await
            .context("failed to flush agy user message")?;
        self.turn_active = true;
        self.abort_requested = false;
        Ok(())
    }

    async fn initialize_transport_context(&mut self) -> Result<()> {
        let instructions = format!(
            "{}\n\nAcknowledge these transport instructions briefly. TinyButler will not show the acknowledgment to the user.",
            chat_bridge_instructions(self.instruction_context)
        );
        self.write_user_message(&instructions).await?;
        loop {
            match ChatAgent::next_event(self).await? {
                Some(ChatEvent::TurnCompleted) => return Ok(()),
                Some(ChatEvent::TurnFailed(error)) => {
                    bail!("failed to initialize agy transport context: {error}")
                }
                Some(
                    ChatEvent::AssistantDelta(_)
                    | ChatEvent::ToolDelta(_)
                    | ChatEvent::Info(_)
                    | ChatEvent::Warning(_),
                ) => {}
                Some(ChatEvent::ApprovalRequired(request)) => {
                    bail!("agy transport initialization requires approval: {request}")
                }
                None => bail!("agy stream closed during transport initialization"),
            }
        }
    }

    async fn next_protocol_output(&mut self) -> Result<Option<AgyOutputEvent>> {
        let lines = self.stdout.as_mut().context("agy stream is not running")?;
        let Some(line) = lines
            .next_line()
            .await
            .context("failed to read agy stream output")?
        else {
            return Ok(None);
        };
        parse_output_line(&line)
            .with_context(|| format!("failed to parse agy stream output: {line}"))
            .map(Some)
    }

    async fn stop_process(&mut self) -> Result<()> {
        self.stdin.take();
        self.stdout.take();
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        if child
            .try_wait()
            .context("failed to inspect agy stream process")?
            .is_none()
        {
            child
                .kill()
                .await
                .context("failed to stop agy stream process")?;
        }
        Ok(())
    }

    fn clear_process(&mut self) {
        self.stdin = None;
        self.stdout = None;
        self.child = None;
        self.turn_active = false;
    }
}

#[async_trait]
impl ChatAgent for AgyChatAgent {
    fn active_session(&self) -> Option<ChatSession> {
        self.active_session.clone()
    }

    async fn start_session(&mut self) -> Result<ChatSession> {
        self.start_process(None).await?;
        self.initialize_transport_context().await?;
        self.active_session
            .clone()
            .context("agy transport initialization lost its session")
    }

    async fn list_sessions(&self) -> Result<Vec<ChatSession>> {
        Ok(self.known_sessions.clone())
    }

    async fn resume_session(&mut self, session_id: &str) -> Result<ChatSession> {
        self.start_process(Some(session_id)).await?;
        self.initialize_transport_context().await?;
        self.active_session
            .clone()
            .context("agy transport initialization lost its session")
    }

    async fn resume_session_for_turn(&mut self, session_id: &str) -> Result<ChatSession> {
        self.start_process(Some(session_id)).await
    }

    async fn send_turn(&mut self, message: &str) -> Result<()> {
        self.write_user_message(message).await?;
        if let Some(session) = self.active_session.as_mut() {
            session.last_activity_at = Local::now();
            if session.title.is_none() {
                session.title = Some(summarize_user_message(message));
            }
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ChatEvent>> {
        loop {
            let Some(output) = self.next_protocol_output().await? else {
                self.clear_process();
                if self.abort_requested {
                    self.abort_requested = false;
                    return Ok(Some(ChatEvent::TurnCompleted));
                }
                return Ok(None);
            };
            match output {
                AgyOutputEvent::Init { conversation_id } => {
                    if self
                        .active_session
                        .as_ref()
                        .is_some_and(|session| session.session_id != conversation_id)
                    {
                        bail!("agy changed conversation id to {conversation_id}");
                    }
                }
                AgyOutputEvent::Chat(event) => return Ok(Some(event)),
                AgyOutputEvent::Result {
                    conversation_id,
                    status,
                    error,
                } => {
                    self.turn_active = false;
                    if let Some(mut session) = self.active_session.clone() {
                        if session.session_id != conversation_id {
                            bail!(
                                "agy result conversation {conversation_id} does not match {}",
                                session.session_id
                            );
                        }
                        session.last_activity_at = Local::now();
                        self.record_session(session);
                    }
                    let expected_abort = self.abort_requested;
                    self.abort_requested = false;
                    if status == "SUCCESS" || expected_abort {
                        return Ok(Some(ChatEvent::TurnCompleted));
                    }
                    return Ok(Some(ChatEvent::TurnFailed(error.unwrap_or_else(|| {
                        format!("agy turn ended with status {status}")
                    }))));
                }
                AgyOutputEvent::Ignored => {}
            }
        }
    }

    async fn abort_turn(&mut self) -> Result<AbortOutcome> {
        let pid = self
            .child
            .as_ref()
            .and_then(Child::id)
            .context("no active agy process to abort")?;
        let status = Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .status()
            .await
            .context("failed to signal agy process")?;
        if !status.success() {
            bail!("failed to interrupt agy process {pid}: {status}");
        }
        self.abort_requested = true;
        Ok(AbortOutcome {
            accepted: true,
            resumable: self.active_session.is_some(),
        })
    }
}

/// One parsed event from Antigravity CLI's NDJSON output stream.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgyOutputEvent {
    Init {
        conversation_id: String,
    },
    Chat(ChatEvent),
    Result {
        conversation_id: String,
        status: String,
        error: Option<String>,
    },
    Ignored,
}

fn parse_output_line(line: &str) -> Result<AgyOutputEvent> {
    if line.trim().is_empty() {
        return Ok(AgyOutputEvent::Ignored);
    }
    let value: serde_json::Value = serde_json::from_str(line)?;
    let event = value
        .get("event")
        .and_then(serde_json::Value::as_str)
        .context("agy output event has no event field")?;
    match event {
        "init" => Ok(AgyOutputEvent::Init {
            conversation_id: string_field(&value, "/conversation_id")?.to_string(),
        }),
        "step_update" => parse_step_update(&value),
        "result" => {
            let result = value
                .get("result")
                .context("agy result event has no payload")?;
            Ok(AgyOutputEvent::Result {
                conversation_id: string_field(result, "/conversation_id")?.to_string(),
                status: string_field(result, "/status")?.to_string(),
                error: result
                    .get("error")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            })
        }
        _ => Ok(AgyOutputEvent::Ignored),
    }
}

fn parse_step_update(value: &serde_json::Value) -> Result<AgyOutputEvent> {
    let step = value
        .get("step_update")
        .context("agy step_update event has no payload")?;
    let step_type = string_field(step, "/step_type")?;
    let state = step
        .get("state")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if step_type == "agent_response" {
        return Ok(step
            .get("text_delta")
            .and_then(serde_json::Value::as_str)
            .filter(|delta| !delta.is_empty())
            .map(|delta| AgyOutputEvent::Chat(ChatEvent::AssistantDelta(delta.to_string())))
            .unwrap_or(AgyOutputEvent::Ignored));
    }
    if step_type == "tool" && state == "DONE" {
        if let Some(output) = step
            .pointer("/tool_info/output")
            .and_then(serde_json::Value::as_str)
            .filter(|output| !output.is_empty())
        {
            return Ok(AgyOutputEvent::Chat(ChatEvent::ToolDelta(
                output.to_string(),
            )));
        }
        if let Some(error) = step
            .pointer("/tool_info/error/message")
            .and_then(serde_json::Value::as_str)
            .filter(|error| !error.is_empty())
        {
            return Ok(AgyOutputEvent::Chat(ChatEvent::Warning(error.to_string())));
        }
    }
    Ok(AgyOutputEvent::Ignored)
}

fn string_field<'a>(value: &'a serde_json::Value, pointer: &str) -> Result<&'a str> {
    value
        .pointer(pointer)
        .and_then(serde_json::Value::as_str)
        .with_context(|| format!("agy output is missing string field {pointer}"))
}

fn process_args(args: &[String], model: &str, session_id: Option<&str>) -> Result<Vec<String>> {
    let mut expanded = expand_chat_stream_args(args, model)?;
    if let Some(session_id) = session_id {
        expanded.retain(|arg| arg != "--new-project");
        expanded.extend(["--conversation".to_string(), session_id.to_string()]);
    }
    Ok(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CodeAgentConfig, ResolvedCodeAgent};

    #[test]
    fn recognizes_agy_command_names() {
        assert!(recognizes_command("agy"));
        assert!(recognizes_command("/usr/local/bin/agy"));
        assert!(!recognizes_command("codex"));
        assert!(!recognizes_command("agy-wrapper"));
    }

    #[test]
    fn agy_process_args_preserve_configured_protocol_details() {
        let args = [
            "--new-project",
            "--model",
            "{model}",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            "{stdin}",
        ]
        .map(str::to_string)
        .to_vec();

        assert_eq!(
            process_args(&args, "gemini-test", None).expect("fresh args"),
            vec![
                "--new-project",
                "--model",
                "gemini-test",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json"
            ]
        );
        assert_eq!(
            process_args(&args, "gemini-test", Some("conversation-1")).expect("resume args"),
            vec![
                "--model",
                "gemini-test",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "--conversation",
                "conversation-1"
            ]
        );
    }

    #[test]
    fn parses_agy_assistant_tool_and_result_events() {
        assert_eq!(
            parse_output_line(r#"{"event":"init","conversation_id":"conversation-1","init":{}}"#)
                .expect("init event"),
            AgyOutputEvent::Init {
                conversation_id: "conversation-1".to_string()
            }
        );
        assert_eq!(
            parse_output_line(
                r#"{"event":"step_update","step_update":{"step_type":"agent_response","state":"ACTIVE","text_delta":"hello"}}"#
            )
            .expect("assistant event"),
            AgyOutputEvent::Chat(ChatEvent::AssistantDelta("hello".to_string()))
        );
        assert_eq!(
            parse_output_line(
                r#"{"event":"step_update","step_update":{"step_type":"tool","state":"DONE","tool_name":"run_command","tool_info":{"output":"ok\n"}}}"#
            )
            .expect("tool event"),
            AgyOutputEvent::Chat(ChatEvent::ToolDelta("ok\n".to_string()))
        );
        assert_eq!(
            parse_output_line(
                r#"{"event":"result","result":{"conversation_id":"conversation-1","status":"SUCCESS","response":"hello"}}"#
            )
            .expect("result event"),
            AgyOutputEvent::Result {
                conversation_id: "conversation-1".to_string(),
                status: "SUCCESS".to_string(),
                error: None,
            }
        );
        assert!(parse_output_line("not json").is_err());
    }

    #[tokio::test]
    async fn agy_adapter_starts_streams_and_resumes_fake_conversations() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let script = temp.path().join("fake-agy");
        std::fs::write(
            &script,
            r#"#!/bin/sh
session_id=conversation-fresh
model=
while [ "$#" -gt 0 ]; do
    case "$1" in
        --model)
            shift
            model="$1"
            ;;
        --conversation)
            shift
            session_id="$1"
            ;;
    esac
    shift
done
[ "$model" = "gemini-test" ] || exit 9
printf '{"event":"init","conversation_id":"%s","init":{}}\n' "$session_id"
while IFS= read -r message; do
    case "$message" in
        *force-adapter-error*)
            printf '{"event":"result","result":{"conversation_id":"%s","status":"ERROR","response":"","error":"fake failure"}}\n' "$session_id"
            continue
            ;;
    esac
    printf '{"event":"step_update","step_update":{"step_type":"agent_response","state":"ACTIVE","text_delta":"hello"}}\n'
    printf '{"event":"step_update","step_update":{"step_type":"tool","state":"DONE","tool_info":{"output":"tool output"}}}\n'
    printf '{"event":"result","result":{"conversation_id":"%s","status":"SUCCESS","response":"hello"}}\n' "$session_id"
done
"#,
        )
        .expect("write fake agy");
        let mut permissions = std::fs::metadata(&script)
            .expect("fake agy metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make fake agy executable");

        let config = CodeAgentConfig {
            command: script.display().to_string(),
            stream_args: [
                "--new-project",
                "--model",
                "{model}",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "{stdin}",
            ]
            .map(str::to_string)
            .to_vec(),
            models: vec!["gemini-test".to_string()],
            ..CodeAgentConfig::default()
        };
        let resolved = ResolvedCodeAgent {
            model: "gemini-test".to_string(),
            config: &config,
        };
        let mut agent = AgyChatAgent::connect_with_context(
            "gemini/gemini-test",
            resolved,
            Some(temp.path().to_path_buf()),
            ChatInstructionContext::LocalCli,
        )
        .expect("connect fake agy");

        let session = agent.start_session().await.expect("start conversation");
        assert_eq!(session.session_id, "conversation-fresh");
        agent.send_turn("first question").await.expect("send turn");
        assert_eq!(
            agent.next_event().await.expect("assistant event"),
            Some(ChatEvent::AssistantDelta("hello".to_string()))
        );
        assert_eq!(
            agent.next_event().await.expect("tool event"),
            Some(ChatEvent::ToolDelta("tool output".to_string()))
        );
        assert_eq!(
            agent.next_event().await.expect("result event"),
            Some(ChatEvent::TurnCompleted)
        );
        assert_eq!(
            agent.active_session().and_then(|session| session.title),
            Some("first question".to_string())
        );

        agent
            .send_turn("force-adapter-error")
            .await
            .expect("send failing turn");
        assert_eq!(
            agent.next_event().await.expect("failed result event"),
            Some(ChatEvent::TurnFailed("fake failure".to_string()))
        );

        let resumed = agent
            .resume_session("conversation-resumed")
            .await
            .expect("resume conversation");
        assert_eq!(resumed.session_id, "conversation-resumed");
    }

    #[tokio::test]
    async fn agy_adapter_interrupts_a_turn_and_keeps_the_session_resumable() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir");
        let script = temp.path().join("slow-fake-agy");
        std::fs::write(
            &script,
            r#"#!/bin/sh
session_id=conversation-abort
trap 'printf '\''{"event":"result","result":{"conversation_id":"%s","status":"ERROR","response":"","error":"timeout waiting for response"}}\n'\'' "$session_id"; exit 0' INT
printf '{"event":"init","conversation_id":"%s","init":{}}\n' "$session_id"
turn=0
while IFS= read -r message; do
    turn=$((turn + 1))
    if [ "$turn" -eq 1 ]; then
        printf '{"event":"result","result":{"conversation_id":"%s","status":"SUCCESS","response":"initialized"}}\n' "$session_id"
    else
        while :; do :; done
    fi
done
"#,
        )
        .expect("write slow fake agy");
        let mut permissions = std::fs::metadata(&script)
            .expect("slow fake agy metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("make slow fake agy executable");

        let config = CodeAgentConfig {
            command: script.display().to_string(),
            stream_args: [
                "--new-project",
                "--model",
                "{model}",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
                "{stdin}",
            ]
            .map(str::to_string)
            .to_vec(),
            models: vec!["gemini-test".to_string()],
            ..CodeAgentConfig::default()
        };
        let mut agent = AgyChatAgent::connect_with_context(
            "gemini/gemini-test",
            ResolvedCodeAgent {
                model: "gemini-test".to_string(),
                config: &config,
            },
            Some(temp.path().to_path_buf()),
            ChatInstructionContext::LocalCli,
        )
        .expect("connect slow fake agy");

        agent.start_session().await.expect("start abort session");
        agent
            .send_turn("wait forever")
            .await
            .expect("send slow turn");
        let outcome = agent.abort_turn().await.expect("interrupt turn");
        assert_eq!(
            outcome,
            AbortOutcome {
                accepted: true,
                resumable: true
            }
        );
        let event = tokio::time::timeout(tokio::time::Duration::from_secs(5), agent.next_event())
            .await
            .expect("interrupt result timeout")
            .expect("interrupt result");
        assert_eq!(event, Some(ChatEvent::TurnCompleted));
        assert_eq!(
            agent.active_session().map(|session| session.session_id),
            Some("conversation-abort".to_string())
        );
    }
}
