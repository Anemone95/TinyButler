# TickClaw

TickClaw is a small file-managed scheduler for shell tasks and coding-agent tasks.

## Overview

TickClaw runs scheduled work from task directories under `~/.tickclaw/tasks/`. It does not use Linux `cron`; the daemon parses local-time cron expressions with the Rust `croner` crate. Tasks write logs and state beside their own `task.yaml`, keeping the system easy to inspect over SSH or through a coding agent.

## Install

Build from source:

```bash
make
```

Initialize a TickClaw home:

```bash
target/debug/tickclaw init
```

This creates `~/.tickclaw/config.yaml` and example tasks under `~/.tickclaw/tasks/`.

Install the release binary and enable the user-level systemd daemon:

```bash
make install
```

`make install` follows Cargo conventions and runs `cargo install --path . --force`, which usually installs to `~/.cargo/bin/tickclaw`. It also writes `~/.config/systemd/user/tickclaw.service`, runs `systemctl --user enable --now tickclaw.service`, and tries to enable lingering so the service can start at boot. Pass extra Cargo install options through `CARGO_INSTALL_ARGS`:

```bash
make install CARGO_INSTALL_ARGS='--root ~/.local --force'
```

## Runtime Layout

```text
~/.tickclaw/
  config.yaml
  telegram_state.json
  chat_state.json
  chat.lock
  tasks/
    smoke-task/
      data/
      task.yaml
      agent.md
      logs/
      state.json
    regular-check/
      data/
      task.yaml
      run.sh
      logs/
      state.json
```

`task.yaml`, `agent.md`, and `run.sh` are task-owned. `state.json`, `logs/`, `.tickclaw.lock`, `~/.tickclaw/telegram_state.json`, `~/.tickclaw/chat_state.json`, and `~/.tickclaw/chat.lock` are daemon-owned. `data/` belongs to the task execution.

## Task Files

Agent task:

```yaml
name: smoke-task
enabled: false
schedule: "0 9 * * *"
runner: gpt-5.3-codex-spark
type: agent
session: independent
timeout: 3600
```

For agent tasks, `runner` is a key in local `~/.tickclaw/config.yaml` under `code_agents`. The default templates include `gemini-3.1-flash-lite`, `gpt-5.3-codex-spark`, and `gpt-5.5`.

Shell task:

```yaml
name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 1800
```

Every task runs inside its own task directory, `~/.tickclaw/tasks/<task-name>/`. Task configs must not define `workspace`, `notify`, or `concurrency`.

## Commands

```bash
tickclaw init
tickclaw daemon
tickclaw check
tickclaw telegram '<message>'
tickclaw telegram --photo <path> --caption '<message>'
tickclaw telegram --document <path> --caption '<message>'
tickclaw task list
tickclaw task run <task>
tickclaw task status <task>
tickclaw task enable <task>
tickclaw task disable <task>
tickclaw chat new
tickclaw chat session
```

`tickclaw check` validates local config and task definitions. `tickclaw task status <task>` shows task details, current state, and the first 20 lines of the latest log.

`tickclaw chat new` starts a local REPL for an interactive Codex chat session. `tickclaw chat session` resumes a previous session. Use `/exit` to detach and Ctrl+C to abort an active turn.

## Telegram

Telegram secrets live only in `~/.tickclaw/config.yaml`:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

TickClaw uses Telegram Bot HTTP API calls. `tickclaw telegram '<message>'`, media captions, and compact daemon-generated summaries use MarkdownV2. TickClaw sanitizes CLI-authored Telegram messages before sending; still escape dynamic content deliberately when composing MarkdownV2. Send arbitrary logs and large text as documents. Agent tasks may call `tickclaw telegram ...` themselves when they want to notify. The daemon sends fallback notifications for failures, and shell tasks notify when stdout is non-empty or when the task fails. The daemon stores Telegram long-polling offset state in `~/.tickclaw/telegram_state.json`.

When the daemon is running, Telegram also supports `/new`, `/session`, and `/abort` for the interactive code-agent chat bridge. `/new` opens a model menu, `/session` opens a resumable-session menu, and bare Telegram text is redirected to the active chat session after selection.

## Development

Internal design and implementation rules live in `AGENTS.md`. Keep README user-facing and update it from AGENTS when behavior changes.
