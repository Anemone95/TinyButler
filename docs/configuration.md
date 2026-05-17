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
  .agents/skills/       # repo-scoped code-agent operation skills
  tasks/                # task directories
  .gitignore            # ignore template for logs and TinyButler-owned runtime files
```

`tasks/*` is owned by [tasks.md](tasks.md). `telegram_state.json` is owned by [telegram-ingress.md](telegram-ingress.md). `chat_state.json` and `chat.lock` are owned by [chatbridge.md](chatbridge.md). `.agents/skills/*` is refreshed from `templates/.agents/skills/` on `tinybutler init` so code agents launched from the TinyButler home can discover the TinyButler operation skill. Other template files are copied only when missing. `.gitignore` is copied from `templates/.gitignore` and is a configuration-owned ignore template for files that should not be managed by git.

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

Each runner entry may define `command`, `args`, `resume_args`, and `stream_args`.

Template runner keys are `gemini-3.1-flash-lite`, `gpt-5.3-codex-spark`, and `gpt-5.5`.

## Scheduled Agent Arguments

Scheduled agent tasks use `args` for a fresh run.

When `session: reuse` has a previous successful session id, scheduled agent tasks use `resume_args`.

`{prompt}` is replaced with the task prompt plus TinyButler runtime context when a runner needs prompt-in-args.

`{sessionId}` is replaced with the previous successful session id for `session: reuse`.

If no previous session id exists, the first `session: reuse` run starts a new session with `args`.

## Streaming Agent Arguments

`stream_args` is used by interactive chat bridge runners.

`stream_args` should be complete for that runner, including model, sandbox or approval policy, and streaming output mode when the CLI requires them; it must not inherit those settings from `args`.

A runner supports interactive streaming when it has `stream_args` or a dedicated Rust stream adapter.

## Runner Flag Boundary

TinyButler does not manage Codex, Gemini, Claude, model, sandbox, or safety flags in `task.yaml`.

Users own runner flags through local `code_agents` config.

The default templates use Codex `danger-full-access` and Gemini `--approval-mode yolo`; users may change those choices in local config.

## Session State

For agent task runners, `session: reuse` stores the latest successful session id in the task `state.json`.

Interactive chat sessions store their runtime metadata in chat bridge state, not in task `state.json`.

## Secrets

Do not put real Telegram tokens, chat ids, account credentials, or private runner credentials in repository files.

Examples in docs, templates, README, tests, and skills must use fake values.
