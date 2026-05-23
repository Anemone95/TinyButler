# TinyButler

TinyButler is a simple personal assistant that mainly for creating and running schedule tasks.

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

`make install` follows Cargo conventions and runs `cargo install --path . --force`, which usually installs the release binary to `~/.cargo/bin/tinybutler`. It runs `tinybutler init`, which creates missing home files without overwriting user config or tasks and refreshes the TinyButler operation skill under `~/.tinybutler/.agents/skills/`. It initializes `~/.tinybutler` as a git repository when `.git` is missing, writes `~/.config/systemd/user/tinybutler.service`, enables and restarts the user service, and tries to enable lingering so the service can start at boot. Pass extra Cargo install options through `CARGO_INSTALL_ARGS`:

```bash
make install CARGO_INSTALL_ARGS='--root ~/.local --force'
```

After editing skill templates, run `make syncskill` to copy `templates/.agents/skills/` into `~/.tinybutler/.agents/skills/` and restart the TinyButler user service.

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
agents:
  - codex/gpt-5.3-codex-spark
type: agent
session: independent
timeout: 3600
```

For agent tasks, `agents` is the required ordered list of `group/model` references from local `~/.tinybutler/config.yaml` under `code_agents.<group>.models`. Use a one-element list for a single model. TinyButler only reports the task as failed after the final model fails. The default templates group models under `gemini` and `codex`, including `gemini/gemini-3.1-flash-lite`, `codex/gpt-5.3-codex-spark`, and `codex/gpt-5.5`.

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
tinybutler restart
tinybutler telegram '<message>'
tinybutler telegram --attachment <path> --caption '<message>'
tinybutler telegram --task <task> '<message>'
tinybutler tasks
tinybutler task list
tinybutler task status <task>
tinybutler chat new
tinybutler chat session
```

`tinybutler check` validates local config and task definitions. `tinybutler restart` runs the same validation first, then signals the running daemon to re-exec itself in place without calling `systemctl restart`; use it after editing `config.yaml` so the daemon reloads runner and Telegram settings. Task definition updates do not need a restart because the scheduler re-scans tasks on each tick. `tinybutler tasks` opens the task selector, where you can inspect task details, run a task once, view state and latest-log previews, and enable or disable the selected task. `tinybutler task list` and `tinybutler task status <task>` are read-only, script-friendly inspection commands for agents and shell workflows. The latest-log preview is compact: for long logs it shows the first seven lines, an ellipsis, and the last seven lines, with each displayed line truncated to 80 characters.

`tinybutler chat new` starts a local REPL for an interactive Codex chat session. `tinybutler chat session` resumes a previous session. Use `/exit` to detach and Ctrl+C to abort an active turn. Local REPL sessions tell the agent to print local artifact paths; Telegram sessions tell the agent it is behind the Telegram bridge and can use Telegram attachment delivery.

## Telegram

Telegram secrets live only in `~/.tinybutler/config.yaml`:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

TinyButler uses Telegram Bot HTTP API calls. `tinybutler telegram '<message>'`, attachment captions, compact daemon-generated summaries, shell task summaries, and chat-bridge code-agent replies are treated as ordinary Markdown and converted to Telegram MarkdownV2 before delivery. Shell task stdout should be concise Markdown text because non-empty stdout may be sent as the task summary. `tinybutler telegram --attachment <path>` chooses a Telegram attachment display from the file type and falls back to document-style delivery for ordinary files. Send arbitrary logs and large text as documents. Agent tasks may call `tinybutler telegram --task "$TINYBUTLER_TASK_NAME" ...` themselves when they want to notify; `--task` prefixes the message or attachment caption with the task name so later Telegram replies carry task context. The daemon sends fallback notifications for failures, and shell tasks notify when stdout is non-empty or when the task fails. The daemon stores Telegram long-polling offset state in `~/.tinybutler/telegram_state.json`.

When `tinybutler daemon` starts with Telegram configured, it sends a restart notification and refreshes the bot slash-command menu for the configured chat so clients show the current command names and compact descriptions.

When the daemon is running, Telegram supports `/tasks` for the task selector, `/restart` for check-then-restart, and `/new`, `/session`, and `/abort` for the interactive code-agent chat bridge. `/tasks` opens a task button menu, `/restart` validates local config and task files before restarting the daemon, `/new` opens a model menu, `/session` opens a resumable-session menu, and bare Telegram text is redirected to the active chat session after selection.

For chat-bridge attachment replies, a code agent can create a local artifact and include `ATTACH:<path>` on its own line in the final answer. TinyButler removes the marker from visible text and uploads supported files to Telegram using the most suitable attachment display. As a compatibility fallback, TinyButler also detects existing local image paths in final replies and uploads them once. The bridge strips hidden reasoning tags from final replies so the chat focuses on the answer and delivered attachments.

## Development

Internal design and implementation rules live in `AGENTS.md`. Keep README user-facing and update it from AGENTS when behavior changes.
