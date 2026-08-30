//! Codex app-server implementation of TinyButler's interactive chat adapter.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::Local;
use codex_codes::{
    AsyncClient, ClientInfo, InitializeParams, Notification, ServerMessage, ThreadStartParams,
    TurnStartParams, UserInput, protocol::methods, protocol_generated::types::ThreadResumeParams,
};
use tokio::process::Command;

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
        == Some("codex")
}

/// Codex adapter backed by `codex-codes` app-server JSON-RPC.
pub(super) struct CodexChatAgent {
    runner: String,
    model: String,
    client: AsyncClient,
    instruction_context: ChatInstructionContext,
    active_session: Option<ChatSession>,
    known_sessions: Vec<ChatSession>,
    current_turn_id: Option<String>,
    abort_requested: bool,
}

impl CodexChatAgent {
    /// Start a Codex app-server for an explicitly selected runner entry.
    pub(super) async fn connect_with_context(
        runner: impl Into<String>,
        agent: ResolvedCodeAgent<'_>,
        working_directory: Option<PathBuf>,
        instruction_context: ChatInstructionContext,
    ) -> Result<Self> {
        let model = agent.model.clone();
        let client = start_client(agent, working_directory).await?;
        Ok(Self {
            runner: runner.into(),
            model,
            client,
            instruction_context,
            active_session: None,
            known_sessions: Vec::new(),
            current_turn_id: None,
            abort_requested: false,
        })
    }

    async fn interrupt_current_turn(&mut self) -> Result<()> {
        let session = self
            .active_session
            .as_ref()
            .context("no active Codex chat session to abort")?;
        let turn_id = self
            .current_turn_id
            .clone()
            .context("no active Codex turn id to abort")?;
        self.client
            .request::<_, serde_json::Value>(
                methods::TURN_INTERRUPT,
                &serde_json::json!({
                    "threadId": session.session_id.clone(),
                    "turnId": turn_id,
                }),
            )
            .await
            .context("failed to interrupt Codex turn")?;
        Ok(())
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

    async fn resume_session_inner(
        &mut self,
        session_id: &str,
        developer_instructions: Option<String>,
    ) -> Result<ChatSession> {
        let mut session = self
            .known_sessions
            .iter()
            .find(|session| session.session_id == session_id)
            .cloned()
            .unwrap_or_else(|| ChatSession {
                runner: self.runner.clone(),
                session_id: session_id.to_string(),
                title: None,
                last_activity_at: Local::now(),
            });
        let response: codex_codes::protocol_generated::types::ThreadResumeResponse = self
            .client
            .request(
                methods::THREAD_RESUME,
                &ThreadResumeParams {
                    approval_policy: None,
                    approvals_reviewer: None,
                    base_instructions: None,
                    config: None,
                    cwd: None,
                    developer_instructions,
                    model: Some(self.model.clone()),
                    model_provider: None,
                    personality: None,
                    sandbox: None,
                    service_tier: None,
                    thread_id: session_id.to_string(),
                },
            )
            .await
            .with_context(|| format!("failed to resume Codex chat session {session_id}"))?;
        session.session_id = response.thread.id.clone();
        session.last_activity_at = Local::now();
        Ok(self.record_session(session))
    }
}

#[async_trait]
impl ChatAgent for CodexChatAgent {
    fn active_session(&self) -> Option<ChatSession> {
        self.active_session.clone()
    }

    async fn start_session(&mut self) -> Result<ChatSession> {
        let thread = self
            .client
            .thread_start(&ThreadStartParams {
                developer_instructions: Some(chat_bridge_instructions(self.instruction_context)),
                model: Some(self.model.clone()),
                ..Default::default()
            })
            .await
            .context("failed to start Codex thread")?;
        let session = ChatSession {
            runner: self.runner.clone(),
            session_id: thread.thread.id,
            title: None,
            last_activity_at: Local::now(),
        };
        Ok(self.record_session(session))
    }

    async fn list_sessions(&self) -> Result<Vec<ChatSession>> {
        Ok(self.known_sessions.clone())
    }

    async fn resume_session(&mut self, session_id: &str) -> Result<ChatSession> {
        self.resume_session_inner(
            session_id,
            Some(chat_bridge_instructions(self.instruction_context)),
        )
        .await
    }

    async fn resume_session_for_turn(&mut self, session_id: &str) -> Result<ChatSession> {
        self.resume_session_inner(session_id, None).await
    }

    async fn send_turn(&mut self, message: &str) -> Result<()> {
        let session = self
            .active_session
            .as_ref()
            .context("no active Codex chat session")?;
        self.current_turn_id = None;
        self.abort_requested = false;
        if let Err(err) = self
            .client
            .turn_start(&TurnStartParams {
                thread_id: session.session_id.clone(),
                input: vec![UserInput::Text {
                    text: message.to_string(),
                    text_elements: None,
                }],
                model: Some(self.model.clone()),
                effort: None,
                sandbox_policy: None,
                ..Default::default()
            })
            .await
        {
            if let Some(message) = deserialization_chat_error(&err) {
                return Err(anyhow!(message));
            }
            return Err(err).context("failed to start Codex turn");
        }

        if let Some(active) = self.active_session.as_mut() {
            active.last_activity_at = Local::now();
            if active.title.is_none() {
                active.title = Some(summarize_user_message(message));
            }
        }
        Ok(())
    }

    async fn next_event(&mut self) -> Result<Option<ChatEvent>> {
        let message = match self.client.next_message().await {
            Ok(Some(message)) => message,
            Ok(None) => return Ok(None),
            Err(err) => {
                if let Some(message) = deserialization_chat_error(&err) {
                    return Ok(Some(ChatEvent::TurnFailed(message)));
                }
                return Err(err.into());
            }
        };
        let event = match message {
            ServerMessage::Request { request, .. } => {
                ChatEvent::ApprovalRequired(request.method().to_string())
            }
            other => {
                let event = message_to_chat_event(other, &mut self.current_turn_id);
                if matches!(event, ChatEvent::Info(_))
                    && self.abort_requested
                    && self.current_turn_id.is_some()
                {
                    self.interrupt_current_turn().await?;
                }
                event
            }
        };
        Ok(Some(event))
    }

    async fn abort_turn(&mut self) -> Result<AbortOutcome> {
        self.abort_requested = true;
        if self.current_turn_id.is_some() {
            self.interrupt_current_turn().await?;
        }
        Ok(AbortOutcome {
            accepted: true,
            resumable: true,
        })
    }
}

async fn start_client(
    agent: ResolvedCodeAgent<'_>,
    working_directory: Option<PathBuf>,
) -> Result<AsyncClient> {
    codex_codes::version::check_codex_version_async().await?;
    let mut command = command_from_config(agent, working_directory)?;
    let child = command
        .spawn()
        .context("failed to start configured Codex stream command")?;
    let mut client = AsyncClient::new(child).context("failed to connect Codex stream pipes")?;
    client
        .initialize(&InitializeParams {
            capabilities: None,
            client_info: ClientInfo {
                name: "tinybutler".to_string(),
                title: Some("TinyButler".to_string()),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        })
        .await
        .context("failed to initialize Codex app-server")?;
    Ok(client)
}

/// Build the configured Codex command without interpreting its arguments.
fn command_from_config(
    agent: ResolvedCodeAgent<'_>,
    working_directory: Option<PathBuf>,
) -> Result<Command> {
    let mut command = Command::new(&agent.config.command);
    command
        .args(expand_chat_stream_args(
            &agent.config.stream_args,
            &agent.model,
        )?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(directory) = working_directory {
        command.current_dir(directory);
    }
    Ok(command)
}

fn message_to_chat_event(
    message: ServerMessage,
    current_turn_id: &mut Option<String>,
) -> ChatEvent {
    match message {
        ServerMessage::Notification(notification) => match notification {
            Notification::AgentMessageDelta(delta) => ChatEvent::AssistantDelta(delta.delta),
            Notification::CmdOutputDelta(delta) => ChatEvent::ToolDelta(delta.delta),
            Notification::ReasoningDelta(delta) => ChatEvent::Info(delta.delta),
            Notification::ReasoningTextDelta(delta) => ChatEvent::Info(delta.delta),
            Notification::TurnStarted(started) => {
                *current_turn_id = Some(started.turn.id);
                ChatEvent::Info("turn started".to_string())
            }
            Notification::TurnCompleted(_) => {
                *current_turn_id = None;
                ChatEvent::TurnCompleted
            }
            Notification::Error(error) => {
                let message = if error.error.message.trim().is_empty() {
                    error
                        .error
                        .additional_details
                        .unwrap_or_else(|| "Codex app-server error".to_string())
                } else {
                    error.error.message
                };
                if error.will_retry {
                    ChatEvent::Warning(message)
                } else {
                    *current_turn_id = None;
                    ChatEvent::TurnFailed(message)
                }
            }
            other => ChatEvent::Info(other.method().to_string()),
        },
        ServerMessage::Request { request, .. } => {
            ChatEvent::ApprovalRequired(request.method().to_string())
        }
    }
}

fn deserialization_chat_error(err: &codex_codes::Error) -> Option<String> {
    let codex_codes::Error::Deserialization(parse_error) = err else {
        return None;
    };
    if !matches!(
        parse_error.method.as_deref(),
        Some(methods::ITEM_STARTED | methods::ITEM_COMPLETED)
    ) || !parse_error.error_message.contains("unknown variant")
    {
        return None;
    }
    let item_type = parse_error
        .raw_json
        .as_ref()?
        .pointer("/item/type")
        .and_then(serde_json::Value::as_str)?;
    Some(format!(
        "Codex item type `{}` (`{}`) is not supported by this TinyButler build. Update TinyButler's Codex protocol model before retrying.",
        item_type,
        item_type_name(item_type)
    ))
}

fn item_type_name(item_type: &str) -> String {
    let mut chars = item_type.chars();
    let Some(first) = chars.next() else {
        return "UnknownItem".to_string();
    };
    let rest: String = chars.collect();
    format!("{}{rest}Item", first.to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{CodeAgentConfig, ResolvedCodeAgent};

    #[test]
    fn recognizes_codex_command_names() {
        assert!(recognizes_command("codex"));
        assert!(recognizes_command("/usr/local/bin/codex"));
        assert!(!recognizes_command("agy"));
        assert!(!recognizes_command("codex-wrapper"));
    }

    fn codex_config(stream_args: &[&str]) -> CodeAgentConfig {
        CodeAgentConfig {
            command: "/usr/bin/codex".to_string(),
            stream_args: stream_args.iter().map(|arg| arg.to_string()).collect(),
            models: vec!["gpt-test".to_string()],
            ..CodeAgentConfig::default()
        }
    }

    fn resolved_codex_config(config: &CodeAgentConfig) -> ResolvedCodeAgent<'_> {
        ResolvedCodeAgent {
            model: "gpt-test".to_string(),
            config,
        }
    }

    #[test]
    fn decodes_codex_file_change_started_in_progress_status() {
        let params = serde_json::json!({
            "item": {
                "changes": [{
                    "diff": "@@ -1 +1 @@\n-a\n+b\n",
                    "kind": { "move_path": null, "type": "update" },
                    "path": "/tmp/example.txt"
                }],
                "id": "call_1",
                "status": "inProgress",
                "type": "fileChange"
            },
            "startedAtMs": 1779051453356_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let notification =
            codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params))
                .expect("fileChange inProgress should decode");

        assert!(matches!(notification, Notification::ItemStarted(_)));
    }

    #[test]
    fn decodes_codex_file_change_declined_status() {
        let params = serde_json::json!({
            "item": {
                "changes": [],
                "id": "call_1",
                "status": "declined",
                "type": "fileChange"
            },
            "completedAtMs": 1779051453356_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let notification =
            codex_codes::Notification::from_envelope(methods::ITEM_COMPLETED, Some(params))
                .expect("fileChange declined should decode");

        assert!(matches!(notification, Notification::ItemCompleted(_)));
    }

    #[test]
    fn decodes_codex_image_view_started_item() {
        let params = serde_json::json!({
            "item": {
                "id": "call_1",
                "path": "/tmp/room_snapshot.jpg",
                "type": "imageView"
            },
            "startedAtMs": 1779053681522_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let notification =
            codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params))
                .expect("imageView should decode");

        assert!(matches!(notification, Notification::ItemStarted(_)));
    }

    #[test]
    fn decodes_current_codex_app_server_thread_item_types() {
        let samples = [
            serde_json::json!({ "fragments": [], "id": "hook_1", "type": "hookPrompt" }),
            serde_json::json!({ "id": "plan_1", "text": "check state", "type": "plan" }),
            serde_json::json!({
                "arguments": {}, "id": "dyn_1", "status": "completed",
                "tool": "test", "type": "dynamicToolCall"
            }),
            serde_json::json!({
                "agentsStates": {}, "id": "collab_1", "receiverThreadIds": [],
                "senderThreadId": "thread_1", "status": "completed",
                "tool": "spawnAgent", "type": "collabAgentToolCall"
            }),
            serde_json::json!({
                "id": "img_1", "result": "", "status": "completed",
                "type": "imageGeneration"
            }),
            serde_json::json!({ "id": "review_1", "review": "", "type": "enteredReviewMode" }),
            serde_json::json!({ "id": "review_2", "review": "", "type": "exitedReviewMode" }),
            serde_json::json!({ "id": "compact_1", "type": "contextCompaction" }),
        ];

        for item in samples {
            let params = serde_json::json!({
                "item": item,
                "startedAtMs": 1779053681522_i64,
                "threadId": "thread_1",
                "turnId": "turn_1"
            });
            let notification =
                codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params))
                    .expect("current Codex app-server item should decode");

            assert!(matches!(notification, Notification::ItemStarted(_)));
        }
    }

    #[test]
    fn decodes_codex_thread_resume_with_max_reasoning_effort() {
        let response = serde_json::json!({
            "approvalPolicy": "never",
            "approvalsReviewer": null,
            "cwd": "/tmp",
            "model": "gpt-5.6-sol",
            "modelProvider": "openai",
            "reasoningEffort": "max",
            "sandbox": {},
            "thread": {
                "cliVersion": "0.147.0",
                "createdAt": 1779053681_i64,
                "cwd": "/tmp",
                "ephemeral": false,
                "id": "thread_1",
                "modelProvider": "openai",
                "preview": "",
                "sessionId": "thread_1",
                "source": {},
                "status": {},
                "turns": [],
                "updatedAt": 1779053681_i64
            }
        });
        let response: codex_codes::protocol_generated::types::ThreadResumeResponse =
            serde_json::from_value(response).expect("max reasoning effort should decode");

        assert_eq!(
            serde_json::to_value(response.reasoning_effort).expect("serialize reasoning effort"),
            serde_json::json!("max")
        );
    }

    #[test]
    fn reports_non_retryable_codex_error_message_and_clears_turn() {
        let notification = Notification::from_envelope(
            methods::ERROR,
            Some(serde_json::json!({
                "error": { "message": "request failed" },
                "threadId": "thread_1",
                "turnId": "turn_1",
                "willRetry": false
            })),
        )
        .expect("error notification should decode");
        let mut current_turn_id = Some("turn_1".to_string());

        let event = message_to_chat_event(
            ServerMessage::Notification(notification),
            &mut current_turn_id,
        );

        assert_eq!(event, ChatEvent::TurnFailed("request failed".to_string()));
        assert_eq!(current_turn_id, None);
    }

    #[test]
    fn keeps_turn_active_when_codex_error_will_retry() {
        let notification = Notification::from_envelope(
            methods::ERROR,
            Some(serde_json::json!({
                "error": { "message": "temporary failure" },
                "threadId": "thread_1",
                "turnId": "turn_1",
                "willRetry": true
            })),
        )
        .expect("retryable error notification should decode");
        let mut current_turn_id = Some("turn_1".to_string());

        let event = message_to_chat_event(
            ServerMessage::Notification(notification),
            &mut current_turn_id,
        );

        assert_eq!(event, ChatEvent::Warning("temporary failure".to_string()));
        assert_eq!(current_turn_id.as_deref(), Some("turn_1"));
    }

    #[test]
    fn reports_unknown_codex_item_type_with_friendly_message() {
        let params = serde_json::json!({
            "item": {
                "id": "call_1",
                "path": "/tmp/example.wav",
                "type": "audioView"
            },
            "startedAtMs": 1779053681522_i64,
            "threadId": "thread_1",
            "turnId": "turn_1"
        });
        let serde_error =
            codex_codes::Notification::from_envelope(methods::ITEM_STARTED, Some(params.clone()))
                .expect_err("audioView is intentionally unsupported in this test");
        let parse_error = codex_codes::ParseError::from_envelope(
            methods::ITEM_STARTED,
            Some(params),
            serde_error,
        );
        let error = codex_codes::Error::Deserialization(parse_error);
        let message = deserialization_chat_error(&error).expect("friendly error");

        assert!(message.contains("audioView"));
        assert!(message.contains("AudioViewItem"));
        assert!(!message.contains("\"startedAtMs\""));
    }

    #[test]
    fn rejects_codex_stream_args_without_stdin_marker() {
        let config = codex_config(&[]);
        let err = command_from_config(resolved_codex_config(&config), None).unwrap_err();
        assert!(err.to_string().contains("exactly one {stdin}"));
    }

    #[test]
    fn preserves_complete_codex_stream_args() {
        let config = codex_config(&[
            "app-server",
            "-c",
            "model=\"{model}\"",
            "-c",
            "sandbox_mode=\"danger-full-access\"",
            "--listen",
            "stdio://",
            "{stdin}",
        ]);
        let command =
            command_from_config(resolved_codex_config(&config), Some(PathBuf::from("/tmp")))
                .expect("command");
        let args = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect::<Vec<_>>();

        assert_eq!(
            args,
            vec![
                "app-server",
                "-c",
                "model=\"gpt-test\"",
                "-c",
                "sandbox_mode=\"danger-full-access\"",
                "--listen",
                "stdio://",
            ]
        );
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(std::path::Path::new("/tmp"))
        );
    }

    #[test]
    fn passes_opaque_codex_options_without_protocol_detection() {
        let config = codex_config(&["--strict-config", "{stdin}"]);
        command_from_config(resolved_codex_config(&config), None)
            .expect("opaque options should pass to the Codex adapter");
    }
}
