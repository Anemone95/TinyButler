# TickClaw

TickClaw is a lightweight version of OpenClaw focused on receiving and running scheduled tasks.

## Overview

TickClaw lets you manage scheduled tasks on a remote server through local coding agents such as Codex, Claude, or Gemini. Tasks can be simple scripts that write results to the console log, or prompts executed by an AI coding agent.

## Usage

1. Set up a TickClaw directory on your server and connect it to a Telegram bot.

2. Add the server as a remote SSH target in your local Codex environment.

3. Ask Codex to create, update, list, run, or delete scheduled tasks.

4. Each task can be either:
   - a program that writes output to the console log, or
   - a prompt executed by Codex, Claude, Gemini, or another coding agent.

## Planned Features

- Support claude code, gemini and other code agents.
- Interact with TickClaw directly through Telegram, similar to OpenClaw. Invoke coding agents from Telegram using commands such as `/codex`, `/claude`, and `/gemini`.


## Why This Exists

Codex/Claude Destop Automations are useful, but linux does have it. Besides, you can not run prompt 

## Proposed MVP

The MVP has four pieces.

1. **Scheduler service**

   A long-running local process wakes up periodically, checks due jobs in SQLite, and starts them.

   It supports:

   - one-shot jobs
   - interval jobs, such as every 10 minutes
   - cron-expression jobs, implemented inside TickClaw, not through system cron
   - manual `run now`
   - enable/disable
   - per-job timeout
   - per-job concurrency policy

2. **Runner**

   The runner executes an agent command and stores the result.

   Initial supported command:

   ```bash
   codex exec --json --sandbox workspace-write --cd <workspace> "<prompt>"
   ```

   The runner records:

   - start time and end time
   - exit code
   - stdout and stderr
   - final model message, when available
   - timeout or failure reason

3. **Telegram notifier**

   When a job finishes, TickClaw sends a Telegram message.

   The message should include:

   - job name
   - status
   - duration
   - short summary
   - path to local log file

   Configuration should come from environment variables or a local config file:

   ```bash
   TICKCLAW_TELEGRAM_BOT_TOKEN=...
   TICKCLAW_TELEGRAM_CHAT_ID=...
   ```

4. **MCP server**

   TickClaw exposes MCP tools so Codex can manage jobs directly.

   Proposed tools:

   - `tickclaw_create_job`
   - `tickclaw_update_job`
   - `tickclaw_list_jobs`
   - `tickclaw_get_job`
   - `tickclaw_delete_job`
   - `tickclaw_run_job_now`
   - `tickclaw_list_runs`
   - `tickclaw_get_run_log`
   - `tickclaw_send_telegram`

## Future Telegram Interface

Later, TickClaw can run a Telegram bot ingress service.

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
- scheduled jobs and ad-hoc Telegram jobs share the same run database

Context reuse should be explicit and inspectable. A future design can decide whether this means:

- `codex exec resume <session-id>`
- one session per named job
- one session per model
- one session per Telegram chat
- one session per project workspace

## Suggested Architecture

```text
Codex
  |
  | MCP tools
  v
TickClaw MCP server
  |
  v
SQLite database  <---->  Scheduler service
                         |
                         v
                    Agent runner
                         |
                         v
                  codex / claude / gemini
                         |
                         v
                   Telegram notifier
```

## Proposed Project Layout

```text
TickClaw/
  README.md
  pyproject.toml
  src/tickclaw/
    __init__.py
    cli.py
    config.py
    db.py
    models.py
    scheduler.py
    runner.py
    telegram.py
    mcp_server.py
  examples/
    config.example.toml
    codex-job.example.toml
  scripts/
    install-mcp.sh
```

Runtime data should live outside the repo:

```text
~/.tickclaw/
  config.toml
  tickclaw.db
  logs/
```

## CLI Sketch

```bash
tickclaw init
tickclaw serve
tickclaw mcp
tickclaw telegram test

tickclaw job add \
  --name repo-check \
  --schedule "*/30 * * * *" \
  --workspace /home/wenyuan/work/project \
  --agent codex \
  --prompt "Check the repository and summarize important issues."

tickclaw job list
tickclaw job run repo-check
tickclaw job disable repo-check
tickclaw runs list repo-check
tickclaw runs log <run-id>
```

## Job Model Draft

```toml
name = "repo-check"
enabled = true
agent = "codex"
schedule = "*/30 * * * *"
workspace = "/home/wenyuan/work/project"
prompt = "Check the repository and summarize important issues."
timeout_seconds = 1800
concurrency_policy = "skip"
notify_telegram = true

[codex]
model = "gpt-5.5"
sandbox = "workspace-write"
resume = true
context_key = "repo-check"
```

Concurrency policies:

- `skip`: if the previous run is still active, skip the new run
- `queue`: run after the previous run finishes
- `parallel`: allow overlapping runs

The MVP should implement `skip` first.

## Technology Choices

Proposed stack:

- Python
- `uv` for local development
- SQLite for state
- `apscheduler` or a small custom scheduler loop
- `httpx` for Telegram API calls
- `typer` for CLI
- `mcp` Python SDK for MCP server

Open question: use `apscheduler` for reliability and cron parsing, or keep a simpler custom loop plus `croniter`.

My current recommendation:

- use `apscheduler` for the first implementation
- keep TickClaw's job database as the source of truth
- rebuild scheduler state from SQLite on service startup

## Security Defaults

Unattended agent execution needs conservative defaults.

Initial defaults:

- use `--sandbox workspace-write` for Codex jobs
- require an explicit workspace path
- do not default to `danger-full-access`
- store Telegram token in `~/.tickclaw/config.toml` or env vars, not in job files
- redact bot tokens in logs
- write logs under `~/.tickclaw/logs`
- do not expose a public HTTP server in the MVP

## Open Questions

Before implementation, decide:

1. Should the first scheduler use `apscheduler`, or should TickClaw use a very small hand-written scheduler loop?
2. Should MCP tools be the primary interface first, or should the CLI be implemented first and MCP wrap the CLI?
3. Should Codex context reuse be enabled in the MVP, or should the first version always start a fresh `codex exec` session?
4. For Telegram notifications, should TickClaw send only completion summaries, or include full logs when the output is short?
5. Should the first version support only Codex, or include placeholder agent adapters for Claude and Gemini from day one?

## Initial Implementation Plan

1. Create Python package and CLI.
2. Add config loading from env vars and `~/.tickclaw/config.toml`.
3. Add SQLite schema for jobs, runs, and model contexts.
4. Add Telegram send-message tool and CLI test command.
5. Add Codex runner.
6. Add scheduler service.
7. Add MCP server tools.
8. Add examples and install notes.
9. Run a smoke test with a harmless local command before running real `codex exec`.

