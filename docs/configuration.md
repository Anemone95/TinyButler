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

Each group entry defines `command` and `models`, and may define `new_args`, `resume_args`, and `stream_args`.

`command` shows the agent CLI.

`models` lists the unqualified model names exposed by that backend group.
The chat bridge session display and tasks configuration use `group_name/model_name`, for example `codex/gpt-5.5`.

`new_args` is used for creating a session for a `session: reuse` task's first run or each run of a `session: independent` task.

`resume_args` is used for running a `session: reuse` task in a previous session id.

`stream_args` is used for the chat interface, where the chat bridge can output tokens as a stream.
It is optional for a model group, but only models in a group that has `stream_args` can be listed in the new session command.

`new_args`, `resume_args`, and `stream_args` must contain a placeholder (`{model}`) so the actual task or chat session can choose the model later.
`new_args` and `resume_args` must also contain either `{prompt}` so the actual task can place the prompt, or a `{stdin}` argument to show that the prompt should be put into standard input.
`resume_args` must contain `{sessionId}` for `session: reuse` tasks.


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
