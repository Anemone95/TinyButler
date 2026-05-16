# TickClaw

TickClaw is a lightweight version of OpenClaw focused on receiving and running scheduled tasks.

## Overview

TickClaw lets you manage scheduled tasks on a remote server through local coding agents such as Codex, Claude, or Gemini. Tasks can be simple scripts that write results to the console log, or agent jobs executed by an AI coding agent.

## Usage

1. Set up a TickClaw directory on your server and connect it to a Telegram bot.

2. Add the server as a remote SSH target in your local Codex environment.

3. Ask Codex to create, update, list, run, or delete scheduled tasks by editing files under `~/.tickclaw/tasks/`.

4. Each task can be either:
   - a program that writes output to the console log, or
   - an agent task executed by Codex, Claude, Gemini, or another coding agent.

5. TickClaw runs as a daemon on the server. It scans task directories, executes due tasks, writes logs and state files, and reports completion through Telegram.

## Planned Features

- Support claude code, gemini and other code agents.
- Interact with TickClaw directly through Telegram, similar to OpenClaw. Invoke coding agents from Telegram using commands such as `/codex`, `/claude`, and `/gemini`.


## Why This Exists

Codex/Claude Destop Automations are useful, but linux does have it. Besides, you can not run task though different agents.

## Design

TickClaw should be managed through files, not through MCP or a complex API. This keeps the system small and inspectable: Codex, Claude, Gemini, a human in Vim, or a future Telegram bot can all manage the same tasks because the interface is just files.

## Directory Layout

Runtime data lives in one TickClaw home directory.

```text
~/.tickclaw/
  config.yaml
  tickclaw.log
  tasks/
    daily-report/
      task.yaml
      agent.md
      logs/
        2026-05-16T09-00-00.log
      state.json
    check-build/
      task.yaml
      run.sh
      logs/
      state.json
```

Each task is a directory under `tasks/`. The only required file is `task.yaml`. A task can also include:

- `agent.md` for an agent task
- `run.sh` for a shell task
- `logs/` for run logs
- `state.json` for the latest runtime state

## Task Types

TickClaw supports two task types in the first version.

### Agent Task

An agent task runs a coding agent with `agent.md` as input.

```yaml
# tasks/daily-report/task.yaml
name: daily-report
enabled: true
schedule: "0 9 * * *"
runner: codex
type: agent
session: independent
workspace: /home/wenyuan/work/project
timeout: 3600
notify: telegram
concurrency: skip
created_at: "2026-05-16T10:00:00+02:00"
updated_at: "2026-05-16T10:00:00+02:00"

codex:
  model: gpt-5.5
  sandbox: workspace-write
```

```text
# tasks/daily-report/agent.md
Check the latest experiment results in this repository and summarize what changed.
```

The first Codex command can be:

```bash
codex exec --json --sandbox workspace-write --cd /home/wenyuan/work/project -
```

TickClaw sends `agent.md` through stdin.

`session` controls whether an agent task starts from a fresh context or continues the previous context for the same scheduled task:

- `independent`: every run starts a new agent session
- `reuse`: TickClaw resumes the previous successful session for this task when the runner supports session resume

For Codex, `session: reuse` means TickClaw stores the latest Codex session id in `state.json` and uses `codex exec resume <session-id>` on the next run. If no previous session id exists, the first run starts a new session.

### Shell Task

A shell task runs `run.sh`.

```yaml
# tasks/check-build/task.yaml
name: check-build
enabled: true
schedule: "*/30 * * * *"
runner: shell
type: command
timeout: 1800
notify: telegram
concurrency: skip
created_at: "2026-05-16T10:00:00+02:00"
updated_at: "2026-05-16T10:00:00+02:00"
```

```bash
# tasks/check-build/run.sh
#!/usr/bin/env bash
set -euo pipefail

python scripts/report.py
```

## Scheduler Behavior

The TickClaw daemon does only a few things:

1. scan `tasks/*/task.yaml`
2. validate task schema
3. decide whether a task is due
4. lock the task directory before execution
5. run `agent.md` or `run.sh` according to `type` and `runner`
6. write stdout and stderr to `logs/`
7. update `state.json`
8. send a Telegram notification when configured

The scheduler does not use Linux `cron`. Cron expressions are parsed inside TickClaw.

The first implementation should use `croniter` plus a simple daemon loop. That is enough for this project and avoids keeping scheduler state in a separate framework.

## State File

`state.json` is the daemon-owned status file for a task.

```json
{
  "last_run_at": "2026-05-16T09:00:00+02:00",
  "last_status": "success",
  "last_exit_code": 0,
  "last_log": "logs/2026-05-16T09-00-00.log",
  "session_id": "00000000-0000-0000-0000-000000000000",
  "next_run_at": "2026-05-17T09:00:00+02:00",
  "running": false,
  "run_count": 12,
  "failure_count": 0
}
```

Codex can read this file to understand what happened. Codex should generally edit `task.yaml`, `agent.md`, and `run.sh`; TickClaw owns `state.json`, `logs/`, and lock files.

## Telegram Notifications

TickClaw sends a Telegram message after each configured run.

The first message format should include:

- task name
- status
- duration
- exit code
- short final output or summary
- local log path

Configuration lives in `~/.tickclaw/config.yaml`:

```yaml
telegram:
  bot_token_env: TICKCLAW_TELEGRAM_BOT_TOKEN
  chat_id_env: TICKCLAW_TELEGRAM_CHAT_ID
```

## Future Telegram Interface

Later, TickClaw can run a Telegram ingress service.

Example commands:

```text
/codex check the current repo and summarize risky changes
/claude refactor this function
/gemini summarize this document
```

The intended behavior is:

- `/codex` routes to Codex
- `/claude` routes to Claude
- `/gemini` routes to Gemini
- each model can reuse its own persistent context
- scheduled jobs and ad-hoc Telegram jobs share the same task and log layout

Context reuse should be explicit and inspectable. A future design can decide whether this means:

- one session per named task
- one session per model
- one session per Telegram chat
- one session per project workspace
- `codex exec resume <session-id>` for Codex tasks

## Project Layout

```text
TickClaw/
  README.md
  pyproject.toml
  src/tickclaw/
    __init__.py
    cli.py
    config.py
    daemon.py
    fs.py
    schema.py
    scheduler.py
    runner.py
    telegram.py
  templates/
    config.yaml
    tasks/
      daily-report/
        task.yaml
        agent.md
    .gitignore
```

## CLI Sketch

The CLI is for local operation and debugging. It is not the primary management API.

```bash
tickclaw init
tickclaw daemon
tickclaw scan
tickclaw run daily-report
tickclaw state daily-report
tickclaw logs daily-report
tickclaw telegram test
```

Codex can manage tasks without these commands by editing files directly:

```text
SSH to my server, create a TickClaw agent task under ~/.tickclaw/tasks/ that runs every day at 9 AM, and put the agent instructions in agent.md.
```

## Concurrency

The MVP implements `concurrency: skip`.

If a task is already running when the next scheduled time arrives, TickClaw records a skipped run and leaves the active run alone.

Later options:

- `queue`: run after the previous run finishes
- `parallel`: allow overlapping runs

## Security Defaults

Unattended agent execution needs conservative defaults.

Initial defaults:

- require an explicit task directory
- require an explicit workspace for agent tasks
- use `--sandbox workspace-write` for Codex tasks
- do not default to `danger-full-access`
- store Telegram secrets in environment variables
- redact bot tokens in logs
- write logs under each task's `logs/` directory
- do not expose a public HTTP server in the MVP
- use lock files to prevent duplicate execution
- validate `task.yaml` before running anything
- enforce execution timeouts

## Initial Implementation Plan

1. Create Python package and CLI.
2. Add `~/.tickclaw/config.yaml` loading.
3. Add task schema parsing and validation.
4. Add file-based scheduler with `croniter`.
5. Add shell runner.
6. Add Codex agent runner.
7. Add task locking, log writing, and `state.json` updates.
8. Add Telegram notification.
9. Add examples.
10. Run a smoke test with a harmless shell task before running real `codex exec`.


## SubAgents
To execute task though codex:
```json
"codex-cli": {
 "command": "/usr/bin/codex",
 "args": [
   "exec",
   "--json",
   "--color",
   "never",
   "--sandbox",
   "danger-full-access",
   "-c",
   "service_tier=\"fast\"",
   "--skip-git-repo-check"
 ],
  "resumeArgs": [
    "exec",
    "resume",
    "{sessionId}",
    "-c",
    "sandbox_mode=\"danger-full-access\"",
    "-c",
    "service_tier=\"fast\"",
    "--skip-git-repo-check"
  ]
}
```
