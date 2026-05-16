//! TickClaw command-line entry point.
//!
//! The CLI is the primary public command surface. Telegram slash commands are
//! expected to map back to these local commands rather than introduce separate
//! behavior.

use std::io::{self, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use tickclaw::chat::{
    codex_streaming_runner_keys, load_recovered_chat_state, mark_turn_aborting, mark_turn_finished,
    mark_turn_started, ChatAgent, ChatEvent, ChatLock, ChatRuntimeState, ChatSession,
    ChatStateValue, CodexChatAgent,
};
use tickclaw::config::Config;
use tickclaw::scheduler::Scheduler;
use tickclaw::telegram;

/// Top-level TickClaw CLI options.
#[derive(Debug, Parser)]
#[command(name = "tickclaw")]
#[command(about = "Local file-managed scheduler for agent tasks")]
struct Cli {
    /// Override the TickClaw home directory for tests or isolated installs.
    #[arg(long)]
    home: Option<PathBuf>,

    /// Command to execute.
    #[command(subcommand)]
    command: Command,
}

/// Public TickClaw command surface.
#[derive(Debug, Subcommand)]
enum Command {
    /// Create a local TickClaw home with safe example tasks.
    Init,
    /// Start the scheduler loop.
    Daemon {
        /// Seconds between daemon scheduling ticks.
        #[arg(long, default_value_t = 30)]
        interval_seconds: u64,
    },
    /// Validate config and task definitions.
    Check,
    /// Send outbound Telegram text, photo, or document messages.
    Telegram {
        /// Text message to send.
        message: Option<String>,
        /// Local image path to send as a Telegram photo.
        #[arg(long)]
        photo: Option<PathBuf>,
        /// Local file path to send as a Telegram document.
        #[arg(long)]
        document: Option<PathBuf>,
        /// Optional media caption.
        #[arg(long)]
        caption: Option<String>,
    },
    /// Task inspection and manual execution commands.
    Task {
        /// Task command to execute.
        #[command(subcommand)]
        command: TaskCommand,
    },
    /// Interactive code-agent chat commands.
    Chat {
        /// Chat command to execute.
        #[command(subcommand)]
        command: ChatCommand,
    },
}

/// Public `tickclaw task ...` command surface.
#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// List all tasks and their latest state summary.
    List,
    /// Run one task immediately.
    Run { task: String },
    /// Show task details, state, and latest log preview.
    Status { task: String },
    /// Enable one task by updating its task.yaml file.
    Enable { task: String },
    /// Disable one task by updating its task.yaml file.
    Disable { task: String },
}

/// Public `tickclaw chat ...` command surface.
#[derive(Debug, Subcommand)]
enum ChatCommand {
    /// Start a new interactive chat session.
    New {
        /// Streaming-capable runner key to use instead of prompting.
        #[arg(long)]
        runner: Option<String>,
    },
    /// Resume a previous interactive chat session.
    Session {
        /// Session id to resume instead of prompting.
        #[arg(long)]
        session_id: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let cli = Cli::parse();
    let config = Config::load(cli.home)?;

    match cli.command {
        Command::Init => config.init_home().await,
        Command::Daemon { interval_seconds } => {
            let scheduler = Scheduler::new(config.clone());
            if telegram_configured(&config) {
                tokio::select! {
                    result = scheduler.run_loop(Duration::from_secs(interval_seconds)) => result,
                    result = telegram::poll(config) => result,
                }
            } else {
                scheduler
                    .run_loop(Duration::from_secs(interval_seconds))
                    .await
            }
        }
        Command::Check => {
            let scheduler = Scheduler::new(config);
            scheduler.check().await
        }
        Command::Telegram {
            message,
            photo,
            document,
            caption,
        } => send_telegram(&config, message, photo, document, caption).await,
        Command::Task { command } => {
            let scheduler = Scheduler::new(config);
            match command {
                TaskCommand::List => scheduler.task_list().await,
                TaskCommand::Run { task } => scheduler.run_task_by_name(&task).await.map(|_| ()),
                TaskCommand::Status { task } => scheduler.task_status(&task).await,
                TaskCommand::Enable { task } => {
                    scheduler.set_task_enabled_by_name(&task, true).await
                }
                TaskCommand::Disable { task } => {
                    scheduler.set_task_enabled_by_name(&task, false).await
                }
            }
        }
        Command::Chat { command } => match command {
            ChatCommand::New { runner } => chat_new(config, runner).await,
            ChatCommand::Session { session_id } => chat_session(config, session_id).await,
        },
    }
}

async fn chat_new(config: Config, runner: Option<String>) -> Result<()> {
    let runner = choose_chat_runner(&config, runner)?;
    let agent_config = config
        .code_agents
        .get(&runner)
        .with_context(|| format!("missing code_agents.{runner}"))?;

    set_chat_selection_state(&config, ChatStateValue::SelectingNew).await?;
    let mut agent =
        CodexChatAgent::connect(runner.clone(), agent_config, Some(std::env::current_dir()?))
            .await?;
    let session = agent.start_session().await?;
    record_selected_session(&config, session.clone()).await?;
    println!(
        "started chat session with {} ({})",
        session.runner, session.session_id
    );
    chat_repl(config, agent).await
}

async fn chat_session(config: Config, session_id: Option<String>) -> Result<()> {
    let state = load_recovered_chat_state(&config).await?;
    let session = choose_chat_session(&state, session_id)?;
    let agent_config = config
        .code_agents
        .get(&session.runner)
        .with_context(|| format!("missing code_agents.{}", session.runner))?;

    set_chat_selection_state(&config, ChatStateValue::SelectingSession).await?;
    let mut agent = CodexChatAgent::connect(
        session.runner.clone(),
        agent_config,
        Some(std::env::current_dir()?),
    )
    .await?;
    let session = agent.resume_session(&session.session_id).await?;
    record_selected_session(&config, session.clone()).await?;
    println!(
        "resumed chat session with {} ({})",
        session.runner, session.session_id
    );
    chat_repl(config, agent).await
}

async fn set_chat_selection_state(config: &Config, state_value: ChatStateValue) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = load_recovered_chat_state(config).await?;
    if matches!(
        state.state,
        ChatStateValue::ActiveBusy | ChatStateValue::Aborting
    ) {
        bail!("chat bridge is busy");
    }
    state.state = state_value;
    state.last_error = None;
    state.save_atomic(&config.chat_state_path()).await
}

async fn record_selected_session(config: &Config, session: ChatSession) -> Result<()> {
    let Some(_lock) = ChatLock::acquire(config.chat_lock_path())? else {
        bail!("chat bridge is locked by another process");
    };
    let mut state = load_recovered_chat_state(config).await?;
    state.record_active_session(session);
    state.save_atomic(&config.chat_state_path()).await
}

fn choose_chat_runner(config: &Config, runner: Option<String>) -> Result<String> {
    let runners = codex_streaming_runner_keys(config);
    if runners.is_empty() {
        bail!("no configured Codex streaming runners under code_agents");
    }
    if let Some(runner) = runner {
        if runners.contains(&runner) {
            return Ok(runner);
        }
        bail!("runner {runner} is not a supported Codex streaming runner");
    }
    println!("Select a model:");
    for (index, runner) in runners.iter().enumerate() {
        println!("  {}. {}", index + 1, runner);
    }
    print!("model> ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let index = input
        .trim()
        .parse::<usize>()
        .context("model selection must be a number")?;
    runners
        .get(index.saturating_sub(1))
        .cloned()
        .context("model selection is out of range")
}

fn choose_chat_session(
    state: &ChatRuntimeState,
    session_id: Option<String>,
) -> Result<ChatSession> {
    let sessions = state.sessions_newest_first();
    if sessions.is_empty() {
        bail!("no resumable chat sessions");
    }
    if let Some(session_id) = session_id {
        return sessions
            .into_iter()
            .find(|session| session.session_id == session_id)
            .with_context(|| format!("unknown chat session {session_id}"));
    }
    println!("Select a session:");
    for (index, session) in sessions.iter().enumerate() {
        let title = session.title.as_deref().unwrap_or("(untitled)");
        println!(
            "  {}. {} {} {}",
            index + 1,
            session.runner,
            session.session_id,
            title
        );
    }
    print!("session> ");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let index = input
        .trim()
        .parse::<usize>()
        .context("session selection must be a number")?;
    sessions
        .get(index.saturating_sub(1))
        .cloned()
        .context("session selection is out of range")
}

async fn chat_repl(config: Config, mut agent: CodexChatAgent) -> Result<()> {
    loop {
        print!("chat> ");
        io::stdout().flush()?;
        let mut input = String::new();
        if io::stdin().read_line(&mut input)? == 0 {
            break;
        }
        let message = input.trim();
        if message.is_empty() {
            continue;
        }
        if message == "/exit" {
            println!("detached");
            break;
        }
        run_local_chat_turn(&config, &mut agent, message).await?;
    }
    Ok(())
}

async fn run_local_chat_turn(
    config: &Config,
    agent: &mut CodexChatAgent,
    message: &str,
) -> Result<()> {
    mark_turn_started(
        config,
        format!("local:{}", chrono::Local::now().timestamp_millis()),
    )
    .await?;
    if let Err(err) = agent.send_turn(message).await {
        mark_turn_finished(config, agent.active_session(), Some(format!("{err:#}"))).await?;
        return Err(err);
    }

    let mut ctrl_c: Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> =
        Box::pin(tokio::signal::ctrl_c());
    let mut abort_requested = false;

    loop {
        tokio::select! {
            result = &mut ctrl_c, if !abort_requested => {
                result.context("failed to listen for Ctrl+C")?;
                abort_requested = true;
                mark_turn_aborting(config).await?;
                let outcome = match agent.abort_turn().await {
                    Ok(outcome) => outcome,
                    Err(err) => {
                        mark_turn_finished(config, agent.active_session(), Some(format!("{err:#}"))).await?;
                        return Err(err);
                    }
                };
                if outcome.accepted {
                    println!("\n[aborting]");
                }
            }
            event = agent.next_event() => {
                match event? {
                    Some(ChatEvent::AssistantDelta(delta) | ChatEvent::ToolDelta(delta)) => {
                        print!("{delta}");
                        io::stdout().flush()?;
                    }
                    Some(ChatEvent::Info(_)) => {}
                    Some(ChatEvent::Warning(warning)) => eprintln!("\n[warning] {warning}"),
                    Some(ChatEvent::ApprovalRequired(request)) => {
                        let error = format!("chat turn requires unsupported approval: {request}");
                        mark_turn_finished(config, agent.active_session(), Some(error.clone())).await?;
                        bail!("{error}");
                    }
                    Some(ChatEvent::TurnCompleted) => {
                        println!();
                        mark_turn_finished(config, agent.active_session(), None).await?;
                        return Ok(());
                    }
                    Some(ChatEvent::TurnFailed(error)) => {
                        mark_turn_finished(config, agent.active_session(), Some(error.clone())).await?;
                        bail!("chat turn failed: {error}");
                    }
                    None => {
                        mark_turn_finished(config, agent.active_session(), Some("chat agent closed".to_string())).await?;
                        bail!("chat agent closed");
                    }
                }
            }
        }
    }
}

async fn send_telegram(
    config: &Config,
    message: Option<String>,
    photo: Option<PathBuf>,
    document: Option<PathBuf>,
    caption: Option<String>,
) -> Result<()> {
    let selected = message.is_some() as u8 + photo.is_some() as u8 + document.is_some() as u8;
    if selected != 1 {
        bail!("provide exactly one of <message>, --photo, or --document");
    }

    if let Some(message) = message {
        return telegram::send_text(config, &message).await;
    }
    if let Some(photo) = photo {
        return telegram::send_photo(config, &photo, caption.as_deref()).await;
    }
    if let Some(document) = document {
        return telegram::send_document(config, &document, caption.as_deref()).await;
    }

    unreachable!("selected count guarantees one Telegram mode")
}

fn telegram_configured(config: &Config) -> bool {
    config
        .telegram
        .bot_token
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        && config
            .telegram
            .chat_id
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
}
