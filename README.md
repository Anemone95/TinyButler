# TinyButler

[中文文档](README-zh.md)

TinyButler is a very simple personal assistant. Unlike OpenClaw or Hermes, it does not use a heavy framework. It focuses only on creating scheduled tasks and running them on schedule.
It simply calls local code-agent CLIs that are already installed and logged in on the machine, so it does not need provider API integration, and the whole framework stays lightweight and fast.

## Why TinyButler Exists

I have tried OpenClaw and Hermes. Their scheduled-task features fascinated me, and I often used them for tasks such as monitoring home cameras, cheap flight tickets, and discounted products.
However, their frameworks are heavy. That makes the programs unstable on one hand and consumes a large number of my tokens on the other.

After careful thought, I realized that what I actually need is a personal assistant that can automatically set up tasks and run them on schedule. Memory and similar capabilities can naturally be handled by large companies such as Claude and Codex; I do not need to worry about them myself.
For some one-off heavyweight tasks, I think today's Codex/Claude remote modes are more suitable.

Codex/Claude automation features may satisfy my needs, but: (1) they still do not support Linux environments, so I cannot run them on my small single-board hosts such as Raspberry Pi; (2) they cannot call other models. For example, I currently pay for Codex and Gemini models, and I want different tasks to use different models, but they do not support that at the moment, and I do not think they will support it in the future; (3) they do not support simple Bash commands. For some simple tasks, traditional programs are more reliable, such as reading indoor temperature and humidity from sensors, but those systems do not support them.

## Design Philosophy

1. Use the local machine's code-agent CLIs directly, avoiding the trouble of logging in again or requiring APIs.
2. Provide skills and encourage users to connect to code agents through Telegram chat to configure scheduled tasks or manage TinyButler configuration.
3. Provide only a small CLI and Telegram bridge interface, so users can manage scheduled tasks through code agents or directly through files.

## Current Status

- Suitable for Linux servers, Raspberry Pi-style machines, and other small always-on hosts.
- Scheduled tasks can be plain Bash scripts or local code-agent CLI tasks.
- Codex app-server (through `codex`) and Gemini (through `agy`) interactive streaming are integrated; Claude streaming is still TODO.
- TinyButler does not provide a web UI and does not host models.

## Prerequisites

- A Linux environment.
- Rust/Cargo and `make`.
- User-level systemd, used by `make install` to install the daemon.
- If you want to use agent tasks, install and log in to the corresponding local CLI first, such as `codex`, Antigravity CLI (`agy`) for Gemini, or future `claude`. Gemini chat streaming requires `agy` 1.1.15 or newer.
- If you want to use Telegram, prepare a bot token and the chat id allowed to operate TinyButler.

## Installation

Build from source:

```bash
make
```

Install the release binary and enable the user-level systemd daemon:

```bash
make install
```

`make install` follows Cargo conventions and runs `cargo install --path . --force`, which usually installs the release binary to `~/.cargo/bin/tinybutler`. It runs `tinybutler init`, creating missing home files without overwriting user configuration or tasks, and refreshes the TinyButler operation skill under `~/.tinybutler/.agents/skills/`. If `~/.tinybutler` does not contain `.git`, it initializes that directory as a git repository, writes `~/.config/systemd/user/tinybutler.service` with `~/.local/bin` and the Cargo bin directory on `PATH`, enables and restarts the user service, and lets the service start at boot.

## Quick Start

```bash
make
make install
tinybutler check
tinybutler tasks
```

After configuring Telegram, use `/new` in Telegram to start a new session. You can then deploy tasks by chatting with the Telegram bot. TinyButler will decide from the task description whether to write a script program for the task or run it through an agent, and you will receive the task result at the scheduled time.

## Configuration Directory

TinyButler's configuration directory is `~/.tinybutler/`. Its layout is:

```text
~/.tinybutler/
  config.yaml
  tasks/
    smoke-task/
      data/
      task.yaml
      agent.md
      state.json
    regular-check/
      data/
      task.yaml
      run.sh
      logs/
      state.json
```

`config.yaml` is TinyButler's main configuration file. Configure the Telegram bridge bot account and the personal account allowed to handle messages here:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Also configure code-agent information. Below is a simplified example:

```yaml
code_agents:
  gemini:
    command: agy
    models:
      - gemini-3.7-flash-low
    new_args:
      - "--new-project"
      - "--model"
      - "{model}"
      - "--dangerously-skip-permissions"
      - "--output-format"
      - json
      - "--print={prompt}"
    resume_args:
      - "--model"
      - "{model}"
      - "--dangerously-skip-permissions"
      - "--conversation"
      - "{sessionId}"
      - "--output-format"
      - json
      - "--print={prompt}"
    stream_args:
      - "--new-project"
      - "--model"
      - "{model}"
      - "--dangerously-skip-permissions"
      - "--input-format"
      - stream-json
      - "--output-format"
      - stream-json
      - "{stdin}"

  codex:
    command: /usr/bin/codex
    models:
      - gpt-5.3-codex-spark
      - gpt-5.5
    new_args:
      - exec
      - "--json"
      - "-m"
      - "{model}"
      - "{prompt}"
    stream_args:
      - app-server
      - "-c"
      - model="{model}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "--listen"
      - "stdio://"
      - "{stdin}"
```

Models listed under `models` are referenced in tasks and chat sessions as `group/model`, for example `codex/gpt-5.5` or `gemini/gemini-3.7-flash-low`. Non-empty `stream_args` enables interactive chat and contains the complete fresh-process argument vector. TinyButler expands placeholders, removes `{stdin}`, and otherwise preserves the configured arguments. The Agy and Codex modules recognize the configured `command`.

For detailed configuration instructions, ask your LLM after it reads `docs/configuration.md` and `docs/chatbridge.md`.

## Task Examples

Plain script task `task.yaml`:

```yaml
name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 300
```

Corresponding `run.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

printf '**regular-check ok:** `%s`\n' "$(date --iso-8601=seconds)"
```

Agent task `task.yaml`:

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

Write the task instructions in the corresponding `agent.md`, for example:

```md
Inspect this task directory and summarize whether the task setup is healthy.
```

## Local CLI

```bash
tinybutler init
tinybutler daemon
tinybutler check
tinybutler restart
tinybutler tasks
tinybutler task list
tinybutler task status <task>
tinybutler chat new
tinybutler chat session
tinybutler telegram '<message>'
tinybutler telegram --attachment <path>
```

`tinybutler check` validates configuration and task files. After editing `config.yaml`, use `tinybutler restart` so the daemon reloads configuration. Editing task directories or `task.yaml` usually does not require a restart because the daemon periodically re-scans tasks.

## Telegram Commands

* `/tasks`: list scheduled tasks, view status, and run a task once
* `/new`: start a new session
* `/session`: use a previous session or clear all saved sessions
* `/abort`: abort the active interactive agent turn
* `/restart`: restart the service

## Development

See `AGENTS.md` for development documentation.

## License

BSD-3-Clause. See `LICENSE`.

## TODO

- Support Claude streaming
- Support WeChat, Discord, and other chat integrations
