# Tasks

This document is the authoritative design note for TinyButler task files, task command output, scheduling, task runtime state, notifications, and log retention.

## Task Directory

Every task lives in its own directory under `~/.tinybutler/tasks/<task-name>/`.

```text
~/.tinybutler/tasks/
  <task-name>/
    task.yaml          # task definition and schedule
    agent.md           # prompt for agent tasks
    run.sh             # script for command tasks
    data/              # task execution data
    logs/              # per-run logs
    state.json         # latest runtime state
    .tinybutler.lock   # execution lock
```

Task-owned files are edited by users or agents:

- `task.yaml`
- `agent.md`
- `run.sh`

TinyButler-owned runtime files are written by the daemon:

- `state.json`
- `logs/`
- `.tinybutler.lock`

Task execution owns `data/`. Agents should edit task-owned files by default and should rewrite runtime-owned files or task data only when explicitly debugging state corruption or task output.

## Task.yaml

Common task fields:

- `name`: task name; it should match the task directory when possible.
- `enabled`: whether the daemon should run this task on schedule.
- `schedule`: quoted local-time cron expression. Five-field expressions are normalized with leading seconds.
- `type`: `command` or `agent`.
- `timeout`: maximum run time in seconds.

Cron expressions in YAML examples and templates must always be quoted because unquoted `*` can be parsed as YAML alias syntax.

Task configs must not define `workspace`, `notify`, `concurrency`, or runner CLI flags. Codex, Gemini, Claude, model, sandbox, and safety flags belong only in local `code_agents` config.

## Command Tasks

Command tasks use `type: command` and execute `run.sh` through a shell.

```yaml
name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 1800
```

## Agent Tasks

Agent tasks use `type: agent`, read `agent.md` as the task-owned prompt, prepend TinyButler runtime context, and require an ordered `agents` list. Use a one-element list for a single model.

```yaml
name: smoke-task
enabled: false
schedule: "0 9 * * *"
agents:
  - codex/gpt-5.3-codex-spark
type: agent
session: independent
timeout: 3600
```

Agent task fields:

- `agents`: a required ordered list of model names listed under `code_agents.<group>.models` in `~/.tinybutler/config.yaml`. TinyButler resolves each group and model to its configured backend, tries them in order, and records task failure only after the final agent fails.
- `session`: `independent` starts a fresh session; `reuse` resumes the previous successful session when the runner supports it.

Agent runner configuration, session placeholders, and sandbox/model flags are owned by [configuration.md](configuration.md).

## Task Commands

Task behavior starts from local CLI commands. TinyButler provides a task selector for interactive task management, plus read-only task inspection commands for scripts and code agents.

| Local CLI | Telegram | Meaning |
| --- | --- | --- |
| `tinybutler tasks` | `/tasks` | Open the task selector |
| `tinybutler task list` | none | Print task summaries |
| `tinybutler task status <task>` | none | Print formatted runtime state and the latest-log preview |

`tinybutler tasks` lists task names and lets the user choose one with the keyboard. The local selector should support up/down navigation and keyboard cancellation.

`/tasks` shows the same task list as Telegram buttons. Selecting a task opens the task detail view.

After a task is selected, TinyButler first shows task details formatted as Markdown from the `task.yaml`, including name, enabled state, type, schedule (human-readable schedule description from `croner`), timeout, session mode, and description (100 words from either `agent.md` or `run.sh`).

The selected-task view then offers these actions:

- `status`: show only formatted runtime state fields (from `state.json`) and a latest-log preview when available. The preview is intended for quick scanning: logs longer than 14 lines show the first seven lines, an ellipsis line, and the last seven lines, and each displayed line is truncated to 80 characters with an ellipsis.
- `run`: run the selected task once.
- `enable`: show only when the task is disabled; set `enabled: true`.
- `disable`: show only when the task is enabled; set `enabled: false`.

In CLI environments, the `tinybutler task list` result is like `/task`, but without a selector.
`tinybutler task status <task>` has the same output as `/task` -> `status`.
Task-owned files can also be inspected directly from `~/.tinybutler/tasks/<task-name>/` when lower-level file access is useful.

## Scheduler Loop

The daemon uses a periodic scan model. On startup, it scans `tasks/*/task.yaml` while reconciling startup schedules, then enters the scheduler loop.

Each scheduler tick re-scans `tasks/*/task.yaml` before due checks. The default tick interval is 30 seconds and can be changed with `tinybutler daemon --interval-seconds <seconds>`.

Adding, removing, or editing task directories while the daemon is running is picked up on the next tick. Task creation, deletion, and modification do not require restarting the daemon.


CLI commands such as `tinybutler check`, `tinybutler tasks`, `tinybutler task list`, and `tinybutler task status <task>` read the relevant files directly when invoked.

On each scheduler tick, the daemon validates the task schema, decides whether a task is due, locks the task directory, runs `agent.md` or `run.sh`, writes stdout and stderr to `logs/`, updates `state.json`, and sends a Telegram notification when notification rules require it.

For agent tasks with `agents`, the daemon tries each configured runner in list order. A non-zero exit status, spawn failure, timeout, or missing runtime config for one runner moves execution to the next runner. All attempts are written into the same run log. A successful fallback attempt makes the whole task run successfully, so the daemon does not send its failure Telegram notification unless every configured agent fails.

Task runner processes receive `TINYBUTLER_TASK_NAME` in their environment. Shell scripts and agent tasks that send their own Telegram messages should pass this through with `tinybutler telegram --task "$TINYBUTLER_TASK_NAME" ...` so the outgoing message carries task context for later Telegram replies and debugging.

## Execution Rules

TinyButler validates `task.yaml` before running a task.

TinyButler runs every task from its own task directory.

TinyButler uses `.tinybutler.lock` to prevent duplicate execution. If a task is already locked because a previous run has not finished, the new execution attempt is recorded as `Run Failure`.

TinyButler enforces execution timeouts for both scheduled and manual task runs.

## Cron Semantics

The scheduler uses local-time cron expressions through the Rust `croner` crate. `croner` is the single source of truth for parsing, next-run calculation, and human-readable English schedule descriptions in CLI and Telegram output.

Five-field cron expressions are normalized by prefixing seconds with `0`.

Avoid numeric day-of-week fields in templates and demos unless their parser semantics are explicitly tested. Interval schedules such as `*/15 * * * *` are clearer for mock tasks.

## Schedule Reconciliation

TinyButler does not backfill missed scheduled runs.

On daemon startup, reconcile each enabled task before due checks. If `next_run_at` is missing, invalid, or already in the past, recompute it as the first future occurrence after startup time.

Store the normalized cron expression used to compute `next_run_at` in `state.json` as `schedule_expr`. If the current normalized `task.yaml` schedule differs from `state.schedule_expr`, recompute `next_run_at` as the first future occurrence after now and update `schedule_expr`.

The selected-task `run` action updates `last_run_at`, result fields, and counters. It does not consume or shift the scheduled `next_run_at` unless the task was already due when the manual run started.

Scheduled task success, failure, timeout, or lock conflict advances `next_run_at` to the next scheduled occurrence after the attempt.

DST behavior follows `croner` and the local timezone. `next_run_at` is stored as an RFC3339 timestamp with offset.

## Notifications

Task notification rules define when the daemon should notify. Markdown conversion, attachment delivery, chunking, and Telegram send methods are owned by [markdown-message.md](markdown-message.md).

- Agent task success: the daemon does not auto-notify. The agent may call `tinybutler telegram --task "$TINYBUTLER_TASK_NAME" ...` itself when a notification is useful.
- Agent task failure, timeout, or lock conflict: the daemon sends a fallback Telegram notification. For `agents` fallback lists, this happens only after the final agent fails.
- Shell task success with non-empty stdout: the daemon sends a Telegram notification with a stdout summary.
- Shell task success with empty stdout: the daemon sends no Telegram notification.
- Shell task failure: the daemon sends a Telegram notification even when stdout is empty, using failure, stderr, and log summary.

## Log Retention

TinyButler stores task logs under each task's `logs/` directory.

Task log retention is calendar-month based. Keep the current month plus the previous five months.

Compress completed retained months into `YYYY-MM.tgz` archives under the same task `logs/` directory.

Delete raw logs and archives older than the retained six-month window.

## State File

`state.json` is TinyButler-owned task runtime state.

```json
{
  "last_run_at": "2026-05-16T09:00:00+02:00",
  "last_status": "success",
  "last_exit_code": 0,
  "last_log": "logs/2026-05-16T09-00-00.log",
  "session_id": "00000000-0000-0000-0000-000000000000",
  "schedule_expr": "0 0 9 * * *",
  "next_run_at": "2026-05-17T09:00:00+02:00",
  "running": false,
  "run_count": 12,
  "failure_count": 0
}
```
