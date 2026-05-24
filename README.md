# TinyButler

[中文文档](README-zh.md)

TinyButler is a very simple personal assistant. Unlike OpenClaw or Hermes, it does not use a heavy framework. It focuses only on creating scheduled tasks and running them on schedule.
It simply calls local code-agent CLIs, so it does not need extra authentication, and the whole framework stays lightweight and fast.

# Why TinyButler Exists

I have tried OpenClaw and Hermes. Their scheduled-task features fascinated me, and I often used them for tasks such as monitoring home cameras, cheap flight tickets, and discounted products.
However, their frameworks are heavy. That makes the programs buggy on one hand and consumes a large number of my tokens on the other.

After careful thought, I realized that what I actually need is a personal assistant that can automatically set up tasks and run them on schedule. Memory and similar capabilities can naturally be handled by large companies such as Claude and Codex; I do not need to worry about them myself.
For some one-off heavyweight tasks, I think today's Codex/Claude remote modes are more suitable.

Codex/Claude automation features may satisfy my needs, but: (1) they still do not support Linux environments, so I cannot run them on my small single-board hosts such as Raspberry Pi; (2) they cannot call other models. For example, I currently pay for Codex and Gemini models, and I want different tasks to use different models, but they do not support that at the moment, and I do not think they will support it in the future; (3) they do not support simple Bash commands. For some simple tasks, traditional programs are more reliable, such as reading indoor temperature and humidity from sensors, but those systems do not support them.

# Design Philosophy

1. Use the local machine's code-agent CLIs directly, avoiding the trouble of logging in again or requiring APIs.
2. Provide skills and encourage users to connect to code agents through Telegram chat to configure scheduled tasks or manage TinyButler configuration.
3. Provide only a small CLI and Telegram bridge interface, so users can manage scheduled tasks through code agents or directly through files.

# Installation

Build from source:

```bash
make
```

Install the release binary and enable the user-level systemd daemon:

```bash
make install
```

`make install` follows Cargo conventions and runs `cargo install --path . --force`, which usually installs the release binary to `~/.cargo/bin/tinybutler`. It runs `tinybutler init`, creating missing home files without overwriting user configuration or tasks, and refreshes the TinyButler operation skill under `~/.tinybutler/.agents/skills/`. If `~/.tinybutler` does not contain `.git`, it initializes that directory as a git repository, writes `~/.config/systemd/user/tinybutler.service`, enables and restarts the user service, and lets the service start at boot.

# Configuration Directory

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

For detailed configuration instructions, ask your LLM to read `docs/chatbridge.md` and explain it to you.

Also configure code-agent information:

```yaml
code_agents:
  codex:
    command: /usr/bin/codex
    ...
```

For detailed configuration instructions, ask your LLM to read `docs/configuration.md` and explain it to you.

# Telegram Commands

* `/tasks`: list scheduled tasks, view status, and run a task once
* `/new`: start a new session
* `/session`: use a previous session
* `/restart`: restart the service

# Development

See `AGENTS.md` for development documentation.

# TODO

- Support Claude/Gemini streaming
- Support WeChat, Discord, and other chat integrations
