# Configuration

This document is the authoritative design note for TinyButler's home directory layout and local configuration boundaries.

## TinyButler Home

TinyButler stores local configuration and runtime files under `~/.tinybutler/` by default.

```text
~/.tinybutler/
  config.yaml           # local config and secrets
  tinybutler.log        # daemon-level log file
  telegram_state.json   # Telegram ingress offset state
  chat_state.json       # chat bridge runtime state
  chat.lock             # chat bridge mutation lock
  tasks/                # task directories
```

`tasks/*/task.yaml`, `tasks/*/agent.md`, `tasks/*/run.sh`, and `tasks/*/data/` are owned by [tasks.md](tasks.md). `tasks/*/state.json`, `tasks/*/logs/`, and `tasks/*/.tinybutler.lock` are owned by [scheduler.md](scheduler.md). `telegram_state.json` is owned by [telegram-ingress.md](telegram-ingress.md). `chat_state.json` and `chat.lock` are owned by [chatbridge.md](chatbridge.md).

## Config File

`~/.tinybutler/config.yaml` is local machine configuration. It may contain secrets and must not be copied into the repository.

`templates/config.yaml` is the repository example. It must stay usable without real secrets.

## Telegram Config

TinyButler reads Telegram secrets from local `~/.tinybutler/config.yaml`.

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Telegram ingress behavior, webhook caveats, and `telegram_state.json` are owned by [telegram-ingress.md](telegram-ingress.md).

## Code Agent Config

Code-agent runners are configured under `code_agents.<runner>` in local `~/.tinybutler/config.yaml`.

`templates/config.yaml` is the source for example runner entries.

Runner command fields, placeholders, and streaming arguments are owned by [code-agents.md](code-agents.md).

## Secrets

Do not put real Telegram tokens, chat ids, account credentials, or private runner credentials in repository files.

Examples in docs, templates, README, tests, and skills must use fake values.
