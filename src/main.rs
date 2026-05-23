//! TinyButler command-line entry point.
//!
//! The CLI is the primary public command surface. Telegram slash commands are
//! expected to map back to these local commands rather than introduce separate
//! behavior.

use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use crossterm::cursor;
use crossterm::event::{self, Event, KeyCode};
use crossterm::execute;
use crossterm::terminal::{self, ClearType};
use tokio::process::Command as SystemCommand;
use tokio::time::{Instant, sleep};
use tracing_subscriber::EnvFilter;

use tinybutler::chat::{
    ChatAgent, ChatEvent, ChatInstructionContext, ChatLock, ChatRuntimeState, ChatSession,
    ChatStateValue, CodexChatAgent, chat_working_directory, codex_streaming_model_names,
    load_recovered_chat_state, mark_turn_aborting, mark_turn_finished, mark_turn_started,
};
use tinybutler::config::{Config, DaemonPidRecord, read_daemon_pid_record};
use tinybutler::scheduler::Scheduler;
use tinybutler::telegram;

/// Top-level TinyButler CLI options.
#[derive(Debug, Parser)]
#[command(name = "tinybutler")]
#[command(about = "Local file-managed scheduler for agent tasks")]
struct Cli {
    /// Override the TinyButler home directory for tests or isolated installs.
    #[arg(long)]
    home: Option<PathBuf>,

    /// Command to execute.
    #[command(subcommand)]
    command: Command,
}

/// Public TinyButler command surface.
#[derive(Debug, Subcommand)]
enum Command {
    /// Create a local TinyButler home with safe example tasks.
    Init,
    /// Start the scheduler loop.
    Daemon {
        /// Seconds between daemon scheduling ticks.
        #[arg(long, default_value_t = 30)]
        interval_seconds: u64,
    },
    /// Validate config and task definitions.
    Check,
    /// Validate config and task definitions, then signal the daemon to restart.
    Restart,
    /// Send outbound Telegram text or attachment messages.
    Telegram {
        /// Text message to send.
        message: Option<String>,
        /// Local file path to send as a Telegram attachment.
        #[arg(long)]
        attachment: Option<PathBuf>,
        /// Optional attachment caption.
        #[arg(long)]
        caption: Option<String>,
        /// Task name to include in the Telegram message context.
        #[arg(long)]
        task: Option<String>,
    },
    /// Open the task selector.
    Tasks,
    /// Script-friendly task inspection commands.
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

/// Public read-only `tinybutler task ...` command surface.
#[derive(Debug, Subcommand)]
enum TaskCommand {
    /// List all tasks and their latest state summary.
    List,
    /// Show task details, state, and latest log preview.
    Status { task: String },
}

/// Public `tinybutler chat ...` command surface.
#[derive(Debug, Subcommand)]
enum ChatCommand {
    /// Start a new interactive chat session.
    New {
        /// Streaming-capable model name to use instead of prompting.
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
                if let Err(err) = telegram::send_text(
                    &config,
                    &telegram::daemon_startup_notification_text(&config),
                )
                .await
                {
                    tracing::warn!("failed to send Telegram daemon startup notification: {err:#}");
                }
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
        Command::Restart => restart_service(&config).await,
        Command::Telegram {
            message,
            attachment,
            caption,
            task,
        } => send_telegram(&config, message, attachment, caption, task).await,
        Command::Tasks => {
            let scheduler = Scheduler::new(config);
            run_task_selector(&scheduler).await
        }
        Command::Task { command } => {
            let scheduler = Scheduler::new(config);
            match command {
                TaskCommand::List => scheduler.task_list().await,
                TaskCommand::Status { task } => scheduler.task_status(&task).await,
            }
        }
        Command::Chat { command } => match command {
            ChatCommand::New { runner } => chat_new(config, runner).await,
            ChatCommand::Session { session_id } => chat_session(config, session_id).await,
        },
    }
}

async fn restart_service(config: &Config) -> Result<()> {
    let scheduler = Scheduler::new(config.clone());
    scheduler.check().await?;

    let before = read_valid_daemon_pid_record(config)?;
    let output = SystemCommand::new("kill")
        .args(["-USR2", &before.pid.to_string()])
        .output()
        .await
        .with_context(|| format!("failed to signal TinyButler daemon pid {}", before.pid))?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "failed to signal TinyButler daemon pid {} with status {}\nstdout:\n{}\nstderr:\n{}",
            before.pid,
            output.status,
            stdout.trim(),
            stderr.trim()
        );
    }

    wait_for_daemon_restart(config, &before).await?;
    println!("Restarted TinyButler daemon via pid {}", before.pid);
    Ok(())
}

fn read_valid_daemon_pid_record(config: &Config) -> Result<DaemonPidRecord> {
    let record = read_daemon_pid_record(&config.daemon_pid_path())?;
    record.validate_for_home(&config.home)?;
    Ok(record)
}

async fn wait_for_daemon_restart(config: &Config, before: &DaemonPidRecord) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_error = None;

    loop {
        if Instant::now() >= deadline {
            let detail = last_error
                .map(|err| format!(" last error: {err:#}"))
                .unwrap_or_default();
            bail!(
                "timed out waiting for TinyButler daemon pid {} to restart.{}",
                before.pid,
                detail
            );
        }

        sleep(Duration::from_millis(100)).await;
        match read_valid_daemon_pid_record(config) {
            Ok(after) if after.is_restart_of(before) => return Ok(()),
            Ok(_) => {}
            Err(err) => last_error = Some(err),
        }
    }
}

async fn run_task_selector(scheduler: &Scheduler) -> Result<()> {
    let stdin_is_terminal = io::stdin().is_terminal();
    let stdout_is_terminal = io::stdout().is_terminal();

    if stdin_is_terminal && stdout_is_terminal {
        run_interactive_task_selector(scheduler).await
    } else if !stdin_is_terminal {
        run_scripted_task_selector(scheduler).await
    } else {
        scheduler.task_list().await
    }
}

async fn run_scripted_task_selector(scheduler: &Scheduler) -> Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let choices = input
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();

    if choices.is_empty() {
        return scheduler.task_list().await;
    }

    let Some(task_name) = resolve_task_selection(scheduler, choices[0]).await? else {
        println!("exited");
        return Ok(());
    };

    println!("{}", scheduler.task_detail_text(&task_name).await?);
    if let Some(action) = choices.get(1) {
        run_task_selector_action(scheduler, &task_name, action).await?;
    }
    Ok(())
}

async fn resolve_task_selection(scheduler: &Scheduler, choice: &str) -> Result<Option<String>> {
    if choice.eq_ignore_ascii_case("exit") || choice.eq_ignore_ascii_case("q") {
        return Ok(None);
    }
    let tasks = scheduler.task_selector_items().await?;
    if let Ok(index) = choice.parse::<usize>() {
        if index == 0 {
            bail!("task selection is out of range");
        }
        return tasks
            .get(index - 1)
            .cloned()
            .map(Some)
            .context("task selection is out of range");
    }
    if tasks.iter().any(|task| task == choice) {
        return Ok(Some(choice.to_string()));
    }
    bail!("task not found: {choice}")
}

async fn run_task_selector_action(
    scheduler: &Scheduler,
    task_name: &str,
    action: &str,
) -> Result<()> {
    match action.trim().to_ascii_lowercase().as_str() {
        "status" => scheduler.task_status(task_name).await,
        "run" => {
            let outcome = scheduler.run_task_by_name(task_name).await?;
            println!(
                "Task `{}` finished with `{}` in `{}s`\n\n{}",
                task_name,
                outcome.status,
                outcome.duration_seconds,
                outcome.summary.trim()
            );
            Ok(())
        }
        "enable" => scheduler.set_task_enabled_by_name(task_name, true).await,
        "disable" => scheduler.set_task_enabled_by_name(task_name, false).await,
        other => bail!("unknown task action: {other}"),
    }
}

async fn run_interactive_task_selector(scheduler: &Scheduler) -> Result<()> {
    let mut stdout = io::stdout();
    let _guard = RawTerminalGuard::enter(&mut stdout)?;
    let mut selected_task = 0usize;

    let mut tasks = scheduler.task_selector_items().await?;
    if tasks.is_empty() {
        show_terminal_message(&mut stdout, "No tasks found.")?;
        return Ok(());
    }
    tasks.push("Exit".to_string());
    let task_index = select_terminal_option(
        &mut stdout,
        "TinyButler Tasks",
        "Use Up/Down, Enter, or q.",
        &tasks,
        &mut selected_task,
    )?;
    if task_index + 1 == tasks.len() {
        return Ok(());
    }

    let task_name = tasks[task_index].clone();
    let mut selected_action = 0usize;
    loop {
        let detail = scheduler.task_detail_text(&task_name).await?;
        let mut actions = scheduler.task_selector_action_labels(&task_name).await?;
        actions.push("Back".to_string());
        let action_index = select_terminal_option(
            &mut stdout,
            &format!("Task: {task_name}"),
            &detail,
            &actions,
            &mut selected_action,
        )?;
        let action = &actions[action_index];
        if action == "Back" {
            return Ok(());
        }
        if action == "status" {
            show_terminal_message(&mut stdout, &scheduler.task_status_text(&task_name).await?)?;
        } else if action == "run" {
            let outcome = scheduler.run_task_by_name(&task_name).await?;
            show_terminal_message(
                &mut stdout,
                &format!(
                    "Task `{}` finished with `{}` in `{}s`\n\n{}",
                    task_name,
                    outcome.status,
                    outcome.duration_seconds,
                    outcome.summary.trim()
                ),
            )?;
        } else if action == "enable" {
            scheduler.set_task_enabled_by_name(&task_name, true).await?;
            show_terminal_message(&mut stdout, &format!("`{task_name}` enabled"))?;
        } else if action == "disable" {
            scheduler
                .set_task_enabled_by_name(&task_name, false)
                .await?;
            show_terminal_message(&mut stdout, &format!("`{task_name}` disabled"))?;
        }
    }
}

struct RawTerminalGuard;

impl RawTerminalGuard {
    fn enter(stdout: &mut io::Stdout) -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(stdout, cursor::Hide)?;
        Ok(Self)
    }
}

impl Drop for RawTerminalGuard {
    fn drop(&mut self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), cursor::Show);
    }
}

fn select_terminal_option(
    stdout: &mut io::Stdout,
    title: &str,
    body: &str,
    options: &[String],
    selected: &mut usize,
) -> Result<usize> {
    if options.is_empty() {
        bail!("no selector options available");
    }
    *selected = (*selected).min(options.len().saturating_sub(1));

    loop {
        render_terminal_selector(stdout, title, body, options, *selected)?;
        match event::read()? {
            Event::Key(key) => match key.code {
                KeyCode::Up => *selected = selected.saturating_sub(1),
                KeyCode::Down => *selected = (*selected + 1).min(options.len() - 1),
                KeyCode::Enter => return Ok(*selected),
                KeyCode::Esc | KeyCode::Char('q') => return Ok(options.len() - 1),
                KeyCode::Char(ch) if ch.is_ascii_digit() => {
                    let index = ch.to_digit(10).unwrap_or_default() as usize;
                    if (1..=options.len()).contains(&index) {
                        *selected = index - 1;
                        return Ok(*selected);
                    }
                }
                _ => {}
            },
            Event::Resize(_, _) => {}
            _ => {}
        }
    }
}

fn render_terminal_selector(
    stdout: &mut io::Stdout,
    title: &str,
    body: &str,
    options: &[String],
    selected: usize,
) -> Result<()> {
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    write_terminal_line(stdout, title)?;
    write_terminal_line(stdout, "")?;
    for line in body.lines() {
        write_terminal_line(stdout, line)?;
    }
    if !body.trim().is_empty() {
        write_terminal_line(stdout, "")?;
    }
    for (index, option) in options.iter().enumerate() {
        let marker = if index == selected { ">" } else { " " };
        write_terminal_line(stdout, &format!("{marker} {}. {option}", index + 1))?;
    }
    stdout.flush()?;
    Ok(())
}

fn show_terminal_message(stdout: &mut io::Stdout, message: &str) -> Result<()> {
    execute!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )?;
    for line in message.lines() {
        write_terminal_line(stdout, line)?;
    }
    write_terminal_line(stdout, "")?;
    write_terminal_line(stdout, "Press any key to continue.")?;
    stdout.flush()?;
    loop {
        if let Event::Key(_) = event::read()? {
            return Ok(());
        }
    }
}

fn write_terminal_line(stdout: &mut io::Stdout, line: &str) -> Result<()> {
    write!(stdout, "{line}\r\n")?;
    Ok(())
}

async fn chat_new(config: Config, runner: Option<String>) -> Result<()> {
    let runner = choose_chat_runner(&config, runner)?;
    let agent_config = config
        .code_agent_for_model(&runner)
        .with_context(|| format!("missing code_agents model {runner}"))?;

    set_chat_selection_state(&config, ChatStateValue::SelectingNew).await?;
    let mut agent = CodexChatAgent::connect_with_context(
        runner.clone(),
        agent_config,
        Some(chat_working_directory(&config)),
        ChatInstructionContext::LocalCli,
    )
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
        .code_agent_for_model(&session.runner)
        .with_context(|| format!("missing code_agents model {}", session.runner))?;

    set_chat_selection_state(&config, ChatStateValue::SelectingSession).await?;
    let mut agent = CodexChatAgent::connect_with_context(
        session.runner.clone(),
        agent_config,
        Some(chat_working_directory(&config)),
        ChatInstructionContext::LocalCli,
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
    let runners = codex_streaming_model_names(config);
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
    attachment: Option<PathBuf>,
    caption: Option<String>,
    task: Option<String>,
) -> Result<()> {
    let selected = message.is_some() as u8 + attachment.is_some() as u8;
    if selected != 1 {
        bail!("provide exactly one of <message> or --attachment");
    }

    if let Some(message) = message {
        let message = telegram::with_task_context(&message, task.as_deref());
        return telegram::send_text(config, &message).await;
    }
    if let Some(attachment) = attachment {
        let caption = telegram::caption_with_task_context(caption.as_deref(), task.as_deref());
        return telegram::send_attachment(config, &attachment, caption.as_deref()).await;
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
