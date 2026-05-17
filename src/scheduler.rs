//! Scheduler orchestration, task inspection, and public task command handlers.
//!
//! The scheduler owns the daemon loop, task lock handling, state updates, and
//! the CLI-facing `check` and `task ...` command behavior.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, Utc};
use tokio::fs;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::cron_expr::next_run_after as cron_next_run_after;
pub use crate::cron_expr::{describe_schedule, format_schedule_for_display, normalize_cron};
use crate::lock::TaskLock;
use crate::runner::{self, RunOutcome};
use crate::state::TaskState;
use crate::task::{Task, TaskType};
use crate::telegram;

/// Long-running scheduler bound to one TinyButler home directory.
pub struct Scheduler {
    config: Config,
}

impl Scheduler {
    /// Construct a scheduler from already-loaded configuration.
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Run the daemon loop until Ctrl-C.
    pub async fn run_loop(&self, interval: Duration) -> Result<()> {
        fs::create_dir_all(self.config.tasks_dir()).await?;
        info!(
            "TinyButler daemon started with home {}",
            self.config.home.display()
        );
        self.reconcile_startup_schedules().await?;

        loop {
            if let Err(err) = self.tick().await {
                error!("scheduler tick failed: {err:#}");
            }

            tokio::select! {
                _ = sleep(interval) => {}
                _ = tokio::signal::ctrl_c() => {
                    info!("TinyButler daemon stopped");
                    return Ok(());
                }
            }
        }
    }

    /// Validate local config and every task definition under `tasks/`.
    pub async fn check(&self) -> Result<()> {
        let mut failures = Vec::new();
        if let Err(err) = Config::load(Some(self.config.home.clone())) {
            failures.push(format!("config.yaml: {err:#}"));
        }

        let tasks_dir = self.config.tasks_dir();
        fs::create_dir_all(&tasks_dir).await?;
        let mut entries = fs::read_dir(&tasks_dir).await?;
        let mut checked = 0usize;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path().join("task.yaml");
            if !path.exists() {
                continue;
            }
            checked += 1;
            match Task::load(&path).await {
                Ok(task) => {
                    if let Err(err) = self.validate_task_runtime_contract(&task).await {
                        failures.push(format!("{}: {err:#}", path.display()));
                    }
                }
                Err(err) => {
                    failures.push(format!("{}: {err:#}", path.display()));
                }
            }
        }

        if failures.is_empty() {
            println!(
                "ok: checked config.yaml and {checked} task definition(s) under {}",
                tasks_dir.display()
            );
            Ok(())
        } else {
            for failure in &failures {
                eprintln!("error: {failure}");
            }
            bail!("check failed with {} error(s)", failures.len())
        }
    }

    /// Validate task-owned files and config references needed before execution.
    async fn validate_task_runtime_contract(&self, task: &Task) -> Result<()> {
        match task.task_type {
            TaskType::Command => {
                let script = task.run_script_path();
                if fs::metadata(&script).await.is_err() {
                    bail!("command task {} requires {}", task.name, script.display());
                }
            }
            TaskType::Agent => {
                let prompt = task.agent_path();
                if fs::metadata(&prompt).await.is_err() {
                    bail!("agent task {} requires {}", task.name, prompt.display());
                }
                let runner = task
                    .runner
                    .as_deref()
                    .context("agent task requires runner")?;
                if !self.config.code_agents.contains_key(runner) {
                    bail!(
                        "agent task {} references missing code_agents.{} in config.yaml",
                        task.name,
                        runner
                    );
                }
            }
        }
        Ok(())
    }

    /// Print task summaries with stable line breaks for CLI and Telegram use.
    pub async fn task_list(&self) -> Result<()> {
        println!("{}", self.task_list_text().await?);
        Ok(())
    }

    /// Build task-list text that can be printed locally or sent to Telegram.
    pub async fn task_list_text(&self) -> Result<String> {
        let tasks = self.load_tasks_strict().await?;
        if tasks.is_empty() {
            return Ok("No tasks found.".to_string());
        }

        let mut output = String::new();
        for (index, task) in tasks.into_iter().enumerate() {
            let mut state = TaskState::load(&task.state_path()).await?;
            self.ensure_next_run(&task, &mut state).await?;
            if index > 0 {
                output.push_str("\n\n");
            }
            output.push_str(&format!(
                "{name}\n  enabled: {enabled}\n  type: {task_type:?}\n  runner: {runner}\n  schedule: {schedule}\n  last_status: {last_status}\n  last_run_at: {last_run_at}\n  next_run_at: {next_run_at}\n  run_count: {run_count}\n  failure_count: {failure_count}\n  running: {running}",
                name = task.name,
                enabled = task.enabled,
                task_type = task.task_type,
                runner = task.runner_label(),
                schedule = format_schedule_for_display(&task.schedule),
                last_status = state.last_status.as_deref().unwrap_or("-"),
                last_run_at = state.last_run_at.as_deref().unwrap_or("-"),
                next_run_at = state.next_run_at.as_deref().unwrap_or("-"),
                run_count = state.run_count,
                failure_count = state.failure_count,
                running = state.running,
            ));
        }
        Ok(output)
    }

    /// Run one task immediately by task name or task directory name.
    pub async fn run_task_by_name(&self, name: &str) -> Result<RunOutcome> {
        let task = self.find_task(name).await?;
        self.run_task(&task, true).await
    }

    /// Enable or disable one task by rewriting only its `enabled:` YAML line.
    pub async fn set_task_enabled_by_name(&self, name: &str, enabled: bool) -> Result<()> {
        let task = self.find_task(name).await?;
        let path = task.task_yaml_path();
        let text = fs::read_to_string(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        let updated = set_enabled_in_task_yaml(&text, enabled);
        fs::write(&path, updated)
            .await
            .with_context(|| format!("failed to write {}", path.display()))?;

        Task::load(&path).await.with_context(|| {
            format!(
                "updated task.yaml did not validate after changing enabled for {}",
                task.name
            )
        })?;
        println!(
            "{} {}",
            task.name,
            if enabled { "enabled" } else { "disabled" }
        );
        Ok(())
    }

    /// Print task details, current state, and the first 20 lines of the latest log.
    pub async fn task_status(&self, name: &str) -> Result<()> {
        println!("{}", self.task_status_text(name).await?);
        Ok(())
    }

    /// Build task-status text that can be printed locally or sent to Telegram.
    pub async fn task_status_text(&self, name: &str) -> Result<String> {
        let task = self.find_task(name).await?;
        let state = TaskState::load(&task.state_path()).await?;

        let mut output = format!(
            "task:\n  name: {}\n  enabled: {}\n  type: {:?}\n  runner: {}\n  schedule: {}\n  timeout: {}\n  session: {:?}",
            task.name,
            task.enabled,
            task.task_type,
            task.runner_label(),
            format_schedule_for_display(&task.schedule),
            task.timeout,
            task.session,
        );
        output.push_str(&self.task_execution_file_preview(&task).await);
        output.push_str(&format!(
            "\n\nstate:\n{}",
            serde_json::to_string_pretty(&state)?
        ));

        if let Some(log) = state.last_log.as_deref() {
            let log_path = task.dir.join(log);
            output.push_str(&format!(
                "\n\nlatest_log_first_20_lines: {}",
                log_path.display()
            ));
            match fs::read_to_string(&log_path).await {
                Ok(text) => {
                    for line in text.lines().take(20) {
                        output.push('\n');
                        output.push_str(line);
                    }
                }
                Err(err) => output.push_str(&format!("\nfailed to read latest log: {err:#}")),
            }
        }

        Ok(output)
    }

    /// Return the task-owned prompt or shell script content for status output.
    async fn task_execution_file_preview(&self, task: &Task) -> String {
        let (label, path) = match task.task_type {
            TaskType::Agent => ("agent_prompt", task.agent_path()),
            TaskType::Command => ("shell_command", task.run_script_path()),
        };

        match fs::read_to_string(&path).await {
            Ok(text) => format!("\n\n{label}: {}\n{}", path.display(), text.trim_end()),
            Err(err) => format!("\n\n{label}: {}\nfailed to read: {err:#}", path.display()),
        }
    }

    async fn tick(&self) -> Result<()> {
        for task in self.load_tasks_lenient().await? {
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

    async fn run_task(&self, task: &Task, manual: bool) -> Result<RunOutcome> {
        if !manual && !task.enabled {
            return Err(anyhow!("task is disabled: {}", task.name));
        }

        let state_path = task.state_path();
        let mut state = TaskState::load(&state_path).await?;
        self.ensure_next_run(task, &mut state).await?;
        let should_advance_schedule = !manual || self.is_due(&state);

        let Some(_lock) = TaskLock::acquire(&task.dir)? else {
            warn!(
                "task {} is already locked; reporting Run Failure",
                task.name
            );
            let outcome = runner::run_failure(
                task,
                "Run Failure",
                "task is already locked because a previous run has not finished",
            )
            .await?;
            return self
                .finish_task_attempt(task, outcome, true, should_advance_schedule)
                .await;
        };

        state.running = true;
        state.save(&state_path).await?;

        info!("running task {}", task.name);
        let outcome = runner::run_task(&self.config, task, &state).await?;
        self.finish_task_attempt(task, outcome, false, should_advance_schedule)
            .await
    }

    async fn finish_task_attempt(
        &self,
        task: &Task,
        outcome: RunOutcome,
        running: bool,
        advance_schedule: bool,
    ) -> Result<RunOutcome> {
        let state_path = task.state_path();
        let mut state = TaskState::load(&state_path).await?;
        let now = Local::now();

        state.running = running;
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
        state.schedule_expr = Some(normalize_cron(&task.schedule));
        if advance_schedule {
            state.next_run_at = Some(self.next_run_after(task, now)?.to_rfc3339());
        }
        state.save(&state_path).await?;

        if self.should_notify(task, &outcome) {
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

    /// Decide whether the daemon should send a Telegram notification.
    pub fn should_notify(&self, task: &Task, outcome: &RunOutcome) -> bool {
        match task.task_type {
            TaskType::Agent => outcome.status != "success",
            TaskType::Command => outcome.status != "success" || !outcome.stdout.trim().is_empty(),
        }
    }

    async fn ensure_next_run(&self, task: &Task, state: &mut TaskState) -> Result<()> {
        self.reconcile_next_run(task, state, Local::now(), false)
            .await
    }

    async fn reconcile_startup_schedules(&self) -> Result<()> {
        let now = Local::now();
        for task in self.load_tasks_lenient().await? {
            if !task.enabled {
                continue;
            }
            let mut state = TaskState::load(&task.state_path()).await?;
            self.reconcile_next_run(&task, &mut state, now, true)
                .await?;
        }
        Ok(())
    }

    async fn reconcile_next_run(
        &self,
        task: &Task,
        state: &mut TaskState,
        after: DateTime<Local>,
        recompute_past: bool,
    ) -> Result<()> {
        let normalized = normalize_cron(&task.schedule);
        let schedule_changed = state.schedule_expr.as_deref() != Some(normalized.as_str());
        let parsed_next_run = state
            .next_run_at
            .as_deref()
            .map(DateTime::parse_from_rfc3339)
            .transpose();
        let invalid_next_run = parsed_next_run.is_err();
        let next_run_is_past = parsed_next_run
            .ok()
            .flatten()
            .map(|dt| dt.with_timezone(&Utc) <= Utc::now())
            .unwrap_or(false);

        if schedule_changed
            || state.next_run_at.is_none()
            || invalid_next_run
            || (recompute_past && next_run_is_past)
        {
            state.schedule_expr = Some(normalized);
            state.next_run_at = Some(self.next_run_after(task, after)?.to_rfc3339());
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

    fn next_run_after(&self, task: &Task, after: DateTime<Local>) -> Result<DateTime<Local>> {
        cron_next_run_after(&task.schedule, after)
            .with_context(|| format!("invalid schedule for {}: {}", task.name, task.schedule))
    }

    async fn find_task(&self, name: &str) -> Result<Task> {
        self.load_tasks_strict()
            .await?
            .into_iter()
            .find(|task| {
                task.name == name || task.dir.file_name().and_then(|s| s.to_str()) == Some(name)
            })
            .ok_or_else(|| anyhow!("task not found: {name}"))
    }

    async fn load_tasks_strict(&self) -> Result<Vec<Task>> {
        self.load_tasks(false).await
    }

    async fn load_tasks_lenient(&self) -> Result<Vec<Task>> {
        self.load_tasks(true).await
    }

    async fn load_tasks(&self, skip_invalid: bool) -> Result<Vec<Task>> {
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
                Err(err) if skip_invalid => warn!("skipping {}: {err:#}", path.display()),
                Err(err) => return Err(err).with_context(|| format!("invalid {}", path.display())),
            }
        }

        tasks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(tasks)
    }
}

/// Update or insert the top-level `enabled:` field in a task YAML document.
pub fn set_enabled_in_task_yaml(text: &str, enabled: bool) -> String {
    let enabled_line = format!("enabled: {enabled}");
    let mut lines = text.lines().collect::<Vec<_>>();
    if let Some(index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("enabled:"))
    {
        lines[index] = &enabled_line;
        let mut out = lines.join("\n");
        if text.ends_with('\n') {
            out.push('\n');
        }
        return out;
    }

    if let Some(index) = lines
        .iter()
        .position(|line| line.trim_start().starts_with("name:"))
    {
        lines.insert(index + 1, &enabled_line);
    } else {
        lines.insert(0, &enabled_line);
    }
    let mut out = lines.join("\n");
    out.push('\n');
    out
}
