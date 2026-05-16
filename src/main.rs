mod config;
mod lock;
mod runner;
mod scheduler;
mod state;
mod task;
mod telegram;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::config::Config;
use crate::scheduler::Scheduler;

#[derive(Debug, Parser)]
#[command(name = "tickclaw")]
#[command(about = "Local file-managed scheduler for agent tasks")]
struct Cli {
    #[arg(long)]
    home: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Init,
    Daemon {
        #[arg(long, default_value_t = 30)]
        interval_seconds: u64,
    },
    Scan,
    Run {
        task: String,
    },
    State {
        task: String,
    },
    Logs {
        task: String,
        #[arg(long, default_value_t = 80)]
        lines: usize,
    },
    Telegram {
        #[command(subcommand)]
        command: TelegramCommand,
    },
}

#[derive(Debug, Subcommand)]
enum TelegramCommand {
    Test {
        #[arg(long, default_value = "TickClaw Telegram test")]
        message: String,
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
            let scheduler = Scheduler::new(config);
            scheduler
                .run_loop(Duration::from_secs(interval_seconds))
                .await
        }
        Command::Scan => {
            let scheduler = Scheduler::new(config);
            scheduler.scan().await
        }
        Command::Run { task } => {
            let scheduler = Scheduler::new(config);
            scheduler.run_task_by_name(&task).await.map(|_| ())
        }
        Command::State { task } => {
            let scheduler = Scheduler::new(config);
            scheduler.print_state(&task).await
        }
        Command::Logs { task, lines } => {
            let scheduler = Scheduler::new(config);
            scheduler.print_logs(&task, lines).await
        }
        Command::Telegram { command } => match command {
            TelegramCommand::Test { message } => telegram::send_text(&config, &message).await,
        },
    }
}
