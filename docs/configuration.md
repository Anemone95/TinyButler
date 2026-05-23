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

`tasks/*` is owned by [tasks.md](tasks.md). `telegram_state.json` is owned by [telegram-ingress.md](telegram-ingress.md). `chat_state.json` and `chat.lock` are owned by [chatbridge.md](chatbridge.md). `.agents/skills/*` is refreshed from `templates/.agents/skills/` on `tinybutler init` so code agents launched from the TinyButler home can discover the TinyButler operation skill. Source-tree `make syncskill` copies project skills from `templates/.agents/skills/` into the TinyButler home and restarts the TinyButler user service. Other template files are copied only when missing. `.gitignore` is copied from `templates/.gitignore` and is a configuration-owned ignore template for files that should not be managed by git.

## Config File

`~/.tinybutler/config.yaml` is local machine configuration. It may contain secrets and must not be copied into the repository.

`templates/config.yaml` is the repository example. It must stay usable without real secrets.

The running daemon reads `config.yaml` at startup. Use `tinybutler restart` after editing local config so the daemon reloads runner and Telegram settings; it validates config and tasks first, then signals the running daemon to re-exec itself in place without calling `systemctl restart`. Updating task directories or `task.yaml` does not require a restart because the scheduler re-scans tasks on each tick.

## Telegram Config

TinyButler reads Telegram secrets from local `~/.tinybutler/config.yaml`.

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Telegram ingress behavior, webhook caveats, and `telegram_state.json` are owned by [telegram-ingress.md](telegram-ingress.md).

## Code Agent Config

Code-agent CLI backends are configured under `code_agents.<group>` in local `~/.tinybutler/config.yaml`. Group names are backend labels such as `codex` or `gemini`.

`templates/config.yaml` is the source for example runner entries.

Each group entry may define `command`, `new_args`, `resume_args`, `stream_args`, and `models`.

`models` lists the model names that tasks and chat selection can use. The default template groups `gemini-3.1-flash-lite` under `gemini`, and `gpt-5.3-codex-spark` plus `gpt-5.5` under `codex`.

## Scheduled Agent Arguments

Scheduled agent tasks use `new_args` for a fresh run.

When `session: reuse` has a previous successful session id, scheduled agent tasks use `resume_args`.

`{model}` is replaced with the selected model name.

`{prompt}` is replaced with the task prompt plus TinyButler runtime context when a runner needs prompt-in-args. Otherwise, `new_args` or `resume_args` must end with the literal `stdio` placeholder, which TinyButler removes before spawning the command and uses to send the prompt through standard input.

`{sessionId}` is replaced with the previous successful session id for `session: reuse`.

If no previous session id exists, the first `session: reuse` run starts a new session with `new_args`.

## Streaming Agent Arguments

`stream_args` is used by interactive chat bridge runners.

`stream_args` should be complete for that runner, including model, sandbox or approval policy, and streaming output mode when the CLI requires them; it must not inherit those settings from `new_args`.

A runner supports interactive streaming when it has `stream_args` or a dedicated Rust stream adapter.

## Runner Flag Boundary

TinyButler does not manage Codex, Gemini, Claude, sandbox, or safety flags in `task.yaml`.

Users own runner flags through local `code_agents` config.

The default templates use Codex `danger-full-access` and Gemini `--approval-mode yolo`; users may change those choices in local config.

## Session State

For agent task runners, `session: reuse` stores the latest successful session id and the model name that produced it in the task `state.json`. Reuse only applies when the stored session belongs to the model currently being attempted.

Interactive chat sessions store their runtime metadata in chat bridge state, not in task `state.json`.

## Secrets

Do not put real Telegram tokens, chat ids, account credentials, or private runner credentials in repository files.

Examples in docs, templates, README, tests, and skills must use fake values.
