# TinyButler

TinyButler is a small file-managed scheduler for shell tasks and coding-agent tasks.

## Overview

TinyButler runs scheduled work from task directories under `~/.tinybutler/tasks/`. It does not use Linux `cron`; the daemon parses local-time cron expressions with the Rust `croner` crate. Tasks write logs and state beside their own `task.yaml`, keeping the system easy to inspect over SSH or through a coding agent.

## Install

Build from source:

```bash
make
```

Install the release binary and enable the user-level systemd daemon:

```bash
make install
```

`make install` follows Cargo conventions and runs `cargo install --path . --force`, which usually installs the release binary to `~/.cargo/bin/tinybutler`. It also initializes `~/.tinybutler` with `config.yaml`, `.gitignore`, and example tasks when that directory does not already exist. It installs the TinyButler operation skill under `${CODEX_HOME:-$HOME/.codex}/skills/`, writes `~/.config/systemd/user/tinybutler.service`, runs `systemctl --user enable --now tinybutler.service`, and tries to enable lingering so the service can start at boot. Pass extra Cargo install options through `CARGO_INSTALL_ARGS`:

```bash
make install CARGO_INSTALL_ARGS='--root ~/.local --force'
```

## Runtime Layout

```text
~/.tinybutler/
  config.yaml
  tinybutler.log
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

`task.yaml`, `agent.md`, and `run.sh` are task-owned. `state.json`, `logs/`, `.tinybutler.lock`, `~/.tinybutler/telegram_state.json`, `~/.tinybutler/chat_state.json`, and `~/.tinybutler/chat.lock` are daemon-owned. `data/` belongs to the task execution.

Task logs are retained by calendar month. TinyButler keeps the current month plus the previous five months, compresses completed retained months into `logs/YYYY-MM.tgz`, and deletes older raw logs and archives.

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

For agent tasks, `runner` is a key in local `~/.tinybutler/config.yaml` under `code_agents`. The default templates include `gemini-3.1-flash-lite`, `gpt-5.3-codex-spark`, and `gpt-5.5`.

Shell task:

```yaml
name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 1800
```

Every task runs inside its own task directory, `~/.tinybutler/tasks/<task-name>/`. Task configs must not define `workspace`, `notify`, or `concurrency`.

## Commands

```bash
tinybutler init
tinybutler daemon
tinybutler check
tinybutler telegram '<message>'
tinybutler telegram --attachment <path> --caption '<message>'
tinybutler tasks
tinybutler task list
tinybutler task status <task>
tinybutler chat new
tinybutler chat session
```

`tinybutler check` validates local config and task definitions. `tinybutler tasks` opens the task selector, where you can inspect task details, run a task once, view state and latest-log previews, and enable or disable the selected task. `tinybutler task list` and `tinybutler task status <task>` are read-only, script-friendly inspection commands for agents and shell workflows.

`tinybutler chat new` starts a local REPL for an interactive Codex chat session. `tinybutler chat session` resumes a previous session. Use `/exit` to detach and Ctrl+C to abort an active turn. Local REPL sessions tell the agent to print local artifact paths; Telegram sessions tell the agent it is behind the Telegram bridge and can use Telegram attachment delivery.

## Telegram

Telegram secrets live only in `~/.tinybutler/config.yaml`:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

TinyButler uses Telegram Bot HTTP API calls. `tinybutler telegram '<message>'`, attachment captions, compact daemon-generated summaries, shell task summaries, and chat-bridge code-agent replies are treated as ordinary Markdown and converted to Telegram MarkdownV2 before delivery. `tinybutler telegram --attachment <path>` chooses a Telegram attachment display from the file type and falls back to document-style delivery for ordinary files. Send arbitrary logs and large text as documents. Agent tasks may call `tinybutler telegram ...` themselves when they want to notify. The daemon sends fallback notifications for failures, and shell tasks notify when stdout is non-empty or when the task fails. The daemon stores Telegram long-polling offset state in `~/.tinybutler/telegram_state.json`.

When `tinybutler daemon` starts with Telegram configured, it refreshes the bot slash-command menu for the configured chat so clients show the current command names and compact descriptions.

When the daemon is running, Telegram supports `/tasks` for the task selector and `/new`, `/session`, and `/abort` for the interactive code-agent chat bridge. `/tasks` opens a task button menu, `/new` opens a model menu, `/session` opens a resumable-session menu, and bare Telegram text is redirected to the active chat session after selection.

For chat-bridge attachment replies, a code agent can create a local artifact and include `ATTACH:<path>` on its own line in the final answer. TinyButler removes the marker from visible text and uploads supported files to Telegram using the most suitable attachment display. As a compatibility fallback, TinyButler also detects existing local image paths in final replies and uploads them once. The bridge strips hidden reasoning tags from final replies so the chat focuses on the answer and delivered attachments.

## Development

Internal design and implementation rules live in `AGENTS.md`. Keep README user-facing and update it from AGENTS when behavior changes.
