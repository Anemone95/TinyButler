//! TickClaw command-line entry point.
//!
//! The CLI is the primary public command surface. Telegram slash commands are
//! expected to map back to these local commands rather than introduce separate
//! behavior.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

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
