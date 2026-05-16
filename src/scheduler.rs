use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Local, Utc};
use cron::Schedule;
use tokio::fs;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::lock::TaskLock;
use crate::runner;
use crate::state::TaskState;
use crate::task::{ConcurrencyPolicy, Task};
use crate::telegram;

pub struct Scheduler {
    config: Config,
}

impl Scheduler {
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    pub async fn run_loop(&self, interval: Duration) -> Result<()> {
        fs::create_dir_all(self.config.tasks_dir()).await?;
        info!(
            "TickClaw daemon started with home {}",
            self.config.home.display()
        );

        loop {
            if let Err(err) = self.tick().await {
                error!("scheduler tick failed: {err:#}");
            }

            tokio::select! {
                _ = sleep(interval) => {}
                _ = tokio::signal::ctrl_c() => {
                    info!("TickClaw daemon stopped");
                    return Ok(());
                }
            }
        }
    }

    pub async fn scan(&self) -> Result<()> {
        for task in self.load_tasks().await? {
            let mut state = TaskState::load(&task.state_path()).await?;
            self.ensure_next_run(&task, &mut state).await?;
            let due = self.is_due(&state);
            println!(
                "{} enabled={} due={} next_run_at={}",
                task.name,
                task.enabled,
                due,
                state.next_run_at.as_deref().unwrap_or("-")
            );
        }
        Ok(())
    }

    pub async fn run_task_by_name(&self, name: &str) -> Result<runner::RunOutcome> {
        let task = self
            .load_tasks()
            .await?
            .into_iter()
            .find(|task| {
                task.name == name || task.dir.file_name().and_then(|s| s.to_str()) == Some(name)
            })
            .ok_or_else(|| anyhow!("task not found: {name}"))?;
        self.run_task(&task, true).await
    }

    pub async fn print_state(&self, name: &str) -> Result<()> {
        let task = self
            .load_tasks()
            .await?
            .into_iter()
            .find(|task| {
                task.name == name || task.dir.file_name().and_then(|s| s.to_str()) == Some(name)
            })
            .ok_or_else(|| anyhow!("task not found: {name}"))?;
        let state = TaskState::load(&task.state_path()).await?;
        println!("{}", serde_json::to_string_pretty(&state)?);
        Ok(())
    }

    pub async fn print_logs(&self, name: &str, lines: usize) -> Result<()> {
        let task = self
            .load_tasks()
            .await?
            .into_iter()
            .find(|task| {
                task.name == name || task.dir.file_name().and_then(|s| s.to_str()) == Some(name)
            })
            .ok_or_else(|| anyhow!("task not found: {name}"))?;
        let state = TaskState::load(&task.state_path()).await?;
        let log = state
            .last_log
            .ok_or_else(|| anyhow!("task has no last_log: {name}"))?;
        let text = fs::read_to_string(task.dir.join(log)).await?;
        let selected = text
            .lines()
            .rev()
            .take(lines)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        println!("{selected}");
        Ok(())
    }

    async fn tick(&self) -> Result<()> {
        for task in self.load_tasks().await? {
            if !task.enabled {
                continue;
            }
            let mut state = TaskState::load(&task.state_path()).await?;
            self.ensure_next_run(&task, &mut state).await?;
            if self.is_due(&state) {
                if let Err(err) = self.run_task(&task, false).await {
                    error!("task {} failed: {err:#}", task.name);
                }
            }
        }
        Ok(())
    }

    async fn run_task(&self, task: &Task, manual: bool) -> Result<runner::RunOutcome> {
        if !manual && !task.enabled {
            return Err(anyhow!("task is disabled: {}", task.name));
        }

        let Some(_lock) = TaskLock::acquire(&task.dir)? else {
            if task.concurrency == ConcurrencyPolicy::Skip {
                warn!("task {} is already running; skipping", task.name);
                return Err(anyhow!("task is already running: {}", task.name));
            }
            return Err(anyhow!(
                "concurrency policy {:?} is not implemented yet",
                task.concurrency
            ));
        };

        let state_path = task.state_path();
        let mut state = TaskState::load(&state_path).await?;
        state.running = true;
        state.save(&state_path).await?;

        info!("running task {}", task.name);
        let outcome = runner::run_task(task, &state).await?;
        let now = Local::now();

        state.running = false;
        state.last_run_at = Some(now.to_rfc3339());
        state.last_status = Some(outcome.status.clone());
        state.last_exit_code = outcome.exit_code;
        state.last_log = Some(outcome.log_relative_path.clone());
        if outcome.session_id.is_some() {
            state.session_id = outcome.session_id.clone();
        }
        state.run_count += 1;
        if outcome.status != "success" {
            state.failure_count += 1;
        }
        state.next_run_at = self.next_run_after(task, now)?.map(|dt| dt.to_rfc3339());
        state.save(&state_path).await?;

        if task.notify.as_deref() == Some("telegram") {
            if let Err(err) = telegram::notify_run(
                &self.config,
                &task.name,
                &outcome.status,
                outcome.exit_code,
                outcome.duration_seconds,
                &outcome.log_path.display().to_string(),
                &outcome.summary,
            )
            .await
            {
                warn!(
                    "failed to send Telegram notification for {}: {err:#}",
                    task.name
                );
            }
        }

        Ok(outcome)
    }

    async fn ensure_next_run(&self, task: &Task, state: &mut TaskState) -> Result<()> {
        if state.next_run_at.is_none() {
            state.next_run_at = self
                .next_run_after(task, Local::now())?
                .map(|dt| dt.to_rfc3339());
            state.save(&task.state_path()).await?;
        }
        Ok(())
    }

    fn is_due(&self, state: &TaskState) -> bool {
        let Some(next_run_at) = &state.next_run_at else {
            return false;
        };
        let Ok(next_run_at) = DateTime::parse_from_rfc3339(next_run_at) else {
            return false;
        };
        next_run_at.with_timezone(&Utc) <= Utc::now()
    }

    fn next_run_after(
        &self,
        task: &Task,
        after: DateTime<Local>,
    ) -> Result<Option<DateTime<Local>>> {
        let expr = normalize_cron(&task.schedule);
        let schedule = Schedule::from_str(&expr)
            .with_context(|| format!("invalid schedule for {}: {}", task.name, task.schedule))?;
        Ok(schedule.after(&after).next())
    }

    async fn load_tasks(&self) -> Result<Vec<Task>> {
        let tasks_dir = self.config.tasks_dir();
        fs::create_dir_all(&tasks_dir).await?;
        let mut entries = fs::read_dir(&tasks_dir).await?;
        let mut tasks = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path().join("task.yaml");
            if !path.exists() {
                continue;
            }
            match Task::load(&path).await {
                Ok(task) => tasks.push(task),
                Err(err) => warn!("skipping {}: {err:#}", path.display()),
            }
        }

        tasks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tasks)
    }
}

fn normalize_cron(expr: &str) -> String {
    let fields = expr.split_whitespace().count();
    if fields == 5 {
        format!("0 {expr}")
    } else {
        expr.to_string()
    }
}
