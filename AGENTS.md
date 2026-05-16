# TickClaw Agent Notes

This file is the shared development guide for Codex, Claude, Gemini, and other coding agents working on TickClaw.

User-facing usage belongs in `README.md`. Internal design rules, implementation constraints, and future development notes belong here.

## Project Goal

TickClaw is a file-managed scheduler for agent tasks. It should stay small, inspectable, and easy to manage over code agent cli such as codex, claude, or telegram bridged code agent cli.

Core principles:

- Tasks are files under `~/.tickclaw/tasks/`.
- TickClaw does not use Linux `cron`; it uses its own daemon loop with local-time cron expressions parsed by the Rust `croner` crate.
- TickClaw does not require MCP for task management.
- The CLI is the primary command surface.
- Telegram commands must map to most of local CLI commands.
- Runtime state is stored beside each task, not in a central database.

## Architecture

```text
TickClaw daemon/Codex/Claude/Telegram bridged(Codex/Claude/...)
            |
            v
~/.tickclaw/tasks/*/task.yaml
            |
            v
 shell runner / Codex runner -> stdout
            |
            v
 logs/ + state.json + Telegram notification
```

## CLI Commands

```bash
tickclaw init                                # create the TickClaw configuration directory (~/.tickclaw), including config file, and example tasks
tickclaw daemon                              # start the scheduler loop and run due tasks
tickclaw check                               # validate all TickClaw-controlled files under ~/.tickclaw, including config.yaml and task.yaml files
tickclaw telegram '<message>'                # send a Telegram text message using local config
tickclaw telegram --photo <path>             # send a Telegram photo; accepts optional --caption '<message>'
tickclaw telegram --document <path>          # send a Telegram document; accepts optional --caption '<message>'

tickclaw task list                           # list all tasks and their latest status
tickclaw task run <task>                     # run one task immediately through the task-management command surface
tickclaw task status <task>                  # show task details, latest state, and a log preview
tickclaw task enable <task>                  # set enabled: true in the task.yaml file
tickclaw task disable <task>                 # set enabled: false in the task.yaml file
```

## Repository Layout

```text
TickClaw/
  README.md      # user-facing usage and project overview
  AGENTS.md      # internal development guide for code agents
  Cargo.toml     # Rust package metadata and dependencies
  skills/        # installed-TickClaw operation/configuration skills usable by Codex, Claude, and Gemini
  src/           # TickClaw Rust source code
    main.rs      # CLI parsing and top-level command dispatch
    config.rs    # TickClaw home directory and config loading/init
    task.rs      # task.yaml schema, defaults, and validation
    scheduler.rs # daemon loop, cron scheduling, task scanning, and run orchestration
    runner.rs    # shell and Codex task execution, logging, timeouts, and session extraction
    telegram.rs  # Telegram message sender and run notifications
    state.rs     # per-task state.json model and persistence
    lock.rs      # per-task lock file acquisition and cleanup
  templates/     # files copied or mirrored into initialized TickClaw configurations, see configuration layout for details
    config.yaml  # example local config without secrets
    tasks/       # example task directories
```

## Configuration Layout

```text
~/.tickclaw/
  config.yaml           # local TickClaw config and Telegram secrets
  telegram_state.json   # Telegram polling offset state written by TickClaw
  tickclaw.log          # daemon-level log file
  tasks/                # task directories managed by files
    smoke-task/    # on-demand or low-frequency agent task for deeper repository inspection
      data/        # task execution or sub-agent-owned working data
      task.yaml    # task definition and schedule
      agent.md     # prompt for agent task runners
      logs/        # per-run logs written by TickClaw
      state.json   # latest task runtime state written by TickClaw
    regular-check/ # recurring shell task for cheap repository health and scheduler checks
      data/        # task execution or sub-agent-owned working data
      task.yaml    # task definition and schedule
      run.sh       # shell script executed by shell runner
      logs/        # per-run logs written by TickClaw
      state.json   # latest task runtime state written by TickClaw
```

Task-owned files:

- `task.yaml`
- `agent.md`
- `run.sh`

Runtime-owned files:

- `state.json`
- `logs/`
- `.tickclaw.lock`
- `~/.tickclaw/telegram_state.json`

Task execution or sub-agent-owned files:

- `data/`

Agents should generally edit task-owned files only. Do not manually rewrite runtime-owned files or task data unless explicitly debugging state corruption or task output issues.

## Task.yaml format

Agent task:

```yaml
name: smoke-task # task name; should match the task directory when possible
enabled: false # agent tasks should be opt-in or low-frequency unless cost is intentional
schedule: "0 9 * * *" # local-time cron expression; five fields are normalized with leading seconds
runner: gpt-5.3-codex-spark # code_agents key used to run this agent task
type: agent # task kind; agent tasks read agent.md as prompt
session: independent # independent starts fresh; reuse resumes the previous supported session
timeout: 3600 # maximum run time in seconds
```

Shell task:

```yaml
name: regular-check # task name; should match the task directory when possible
enabled: true # whether the daemon should run this task on schedule
schedule: "*/10 * * * *" # local-time cron expression; cheap shell check every 10 minutes here
type: command # task kind; command tasks execute run.sh
timeout: 1800 # maximum run time in seconds
```

Every task runs inside its own task directory, `~/.tickclaw/tasks/<task-name>/`.

Common task fields:

- `name`: task name; should match the task directory when possible.
- `enabled`: whether the daemon should run this task on schedule.
- `schedule`: quoted local-time cron expression; five fields are normalized with leading seconds. Cron expressions in YAML examples and templates must always be quoted because unquoted `*` can be parsed as YAML alias syntax.
- `type`: `command` or `agent`.
- `timeout`: maximum run time in seconds.

Command tasks use `type: command` and execute `run.sh` through shell.

Agent tasks use `type: agent`, read `agent.md` as the prompt, and require two extra fields:

- `runner`: a string key under `code_agents.<runner>` in `~/.tickclaw/config.yaml`, such as `gpt-5.3-codex-spark`, `gpt-5.5`, or `gemini-3.1-flash-lite`.
- `session`: agent session mode. `independent` starts a new agent session for every run. `reuse` resumes the previous successful session when the configured runner supports session resume.

For agent runners, `session: reuse` stores the latest session id in `state.json` and uses the configured `resume_args` on the next run. If no previous session id exists, the first run starts a new session with `args`.

## Scheduler Rules

The daemon should:

1. scan `tasks/*/task.yaml`
2. validate task schema
3. decide whether a task is due
4. lock the task directory before execution
5. run every task from its own task directory, `~/.tickclaw/tasks/<task-name>/`
6. run `agent.md` or `run.sh` according to `type` and `runner`
7. write stdout and stderr to `logs/`
8. update `state.json`
9. send Telegram notification according to the task type and result rules below

Telegram notification rules:

- Agent task success: the daemon does not auto-notify. The agent may call `tickclaw telegram ...` itself when a notification is useful.
- Agent task failure, timeout, or lock conflict: the daemon sends a fallback Telegram notification.
- Shell task success with non-empty stdout: the daemon sends a Telegram notification with a stdout summary.
- Shell task success with empty stdout: the daemon sends no Telegram notification.
- Shell task failure: the daemon sends a Telegram notification even when stdout is empty, using failure, stderr, and log summary.

If a task is already locked because the previous run has not finished, TickClaw should report that execution attempt as `Run Failure` instead of queuing, skipping by policy, or running in parallel.

The scheduler uses local-time cron expressions through the Rust `croner` crate. `croner` is the single source of truth for parsing, next-run calculation, and human-readable English schedule descriptions in CLI and Telegram output. Five-field cron expressions are normalized by prefixing seconds with `0`. Avoid numeric day-of-week fields in templates and demos unless their parser semantics are explicitly tested; interval schedules such as `*/15 * * * *` are clearer for mock tasks.

Missed-run and schedule-change rules:

- TickClaw does not backfill missed scheduled runs.
- On daemon startup, reconcile each enabled task before due checks. If `next_run_at` is missing, invalid, or already in the past, recompute it as the first future occurrence after startup time.
- Store the normalized cron expression used to compute `next_run_at` in `state.json` as `schedule_expr`. If the current normalized `task.yaml` schedule differs from `state.schedule_expr`, recompute `next_run_at` as the first future occurrence after now and update `schedule_expr`.
- Manual `tickclaw task run <task>` updates `last_run_at`, result fields, and counters. It does not consume or shift the scheduled `next_run_at` unless the task was already due when the manual run started.
- Scheduled task success, failure, timeout, or lock conflict always advances `next_run_at` to the next scheduled occurrence after the attempt.
- DST behavior follows `croner` and the local timezone. `next_run_at` is stored as an RFC3339 timestamp with offset; TickClaw does not add separate DST compensation or backfill for skipped local times.

## State File

`state.json` is daemon-owned.

```json
{
  "last_run_at": "2026-05-16T09:00:00+02:00",
  "last_status": "success",
  "last_exit_code": 0,
  "last_log": "logs/2026-05-16T09-00-00.log",
  "session_id": "00000000-0000-0000-0000-000000000000",
  "schedule_expr": "0 0 9 * * *",
  "next_run_at": "2026-05-17T09:00:00+02:00",
  "running": false,
  "run_count": 12,
  "failure_count": 0
}
```

## Command Mapping Rule

Every user-facing command must start as a local `tickclaw` CLI command. Telegram then exposes the same behavior with a slash command.

Current planned mapping:

| Local CLI | Telegram | Meaning |
| --- | --- | --- |
| `tickclaw task list` | `/task_list` | List all tasks and their latest status |
| `tickclaw task run <name>` | `/task_run <name>` | Run one task immediately |
| `tickclaw task status <name>` | `/task_status <name>` | Show task details, recent state, and the first 20 lines of the latest log |
| `tickclaw task enable <name>` | `/task_enable <name>` | Enable a task by setting `enabled: true` |
| `tickclaw task disable <name>` | `/task_disable <name>` | Disable a task by setting `enabled: false` |

Do not add Telegram-only behavior. If a Telegram feature cannot be explained as a local CLI command first, add the CLI command before adding the Telegram command.

Telegram bot menu commands must use lowercase letters, digits, and underscores only. Use underscore command names such as `/task_list`; do not use hyphenated command names such as `/task-list`.
`tickclaw daemon` starts the MVP Telegram long-polling ingress loop automatically when `telegram.bot_token` and `telegram.chat_id` are configured. There is no separate `tickclaw telegram --poll` public command. The robot handles `/help`, `/task_list`, `/task_status <task>`, `/task_run <task>`, `/task_enable <task>`, and `/task_disable <task>` for the configured `telegram.chat_id` only. It must not respond to bare text aliases such as `tasklist`; users should use slash commands.

## Telegram Task Management Design

`tickclaw task list` and `/task_list` should show:

- task name
- enabled or disabled
- task type and runner
- schedule
- human-readable English schedule description generated through `croner`
- last status
- last run time
- next run time
- run count and failure count
- whether the task is currently running

`task list` output must be readable when forwarded to Telegram. Do not print one long line per task; format each task as a compact multi-line block with stable line breaks.

`tickclaw task status <name>` and `/task_status <name>` should show:

- task details from `task.yaml`
- human-readable English schedule description generated through `croner`
- for agent tasks: the `agent.md` prompt content
- for shell tasks: the `run.sh` script content
- latest state from `state.json`
- first 20 lines of the latest log, when a latest log exists

First implementation can be text-only. Inline buttons can be added later:

- `List` -> `/task_list`
- `Status <task>` -> `/task_status <task>`
- `Run <task>` -> `/task_run <task>`
- `Enable <task>` -> `/task_enable <task>`
- `Disable <task>` -> `/task_disable <task>`

## config.yaml Config

1. Config telegram, TickClaw reads Telegram secrets from local `~/.tickclaw/config.yaml`.

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Do not put real Telegram tokens or chat ids in the repository, examples, or README.

Telegram parse mode is `MarkdownV2` for compact TickClaw-generated summaries and for `tickclaw telegram` text messages and media captions; it is not user-configurable. All task names, paths, stdout, stderr, log previews, task prompts, scripts, captions, and other user-controlled content must be MarkdownV2-escaped before sending. CLI-authored Telegram messages and captions must be sanitized before the API request so ordinary text does not fail Telegram parsing; preserve simple MarkdownV2 formatting such as bold and inline code when possible. Use one shared escaping helper for daemon-generated Telegram messages; `teloxide::utils::markdown` is the preferred mature Rust helper set when the dependency is acceptable. If converting full Markdown documents to Telegram MarkdownV2 is needed, use a dedicated converter such as `telegram_markdown_v2` instead of ad hoc string rewriting.

Large or arbitrary text blocks should be sent as documents instead of Telegram Markdown messages. Safely escaped code blocks are acceptable only for short previews.

Telegram sender should avoid leaking bot tokens in errors. If using reqwest errors, strip URLs before returning or logging errors.

For outbound-only Telegram messages, photos, and documents, TickClaw should use direct Telegram Bot HTTP API calls or a lightweight wrapper. Do not add a full bot framework for outbound-only sending.

For the MVP Telegram ingress loop, direct Telegram Bot API long polling is acceptable because it only routes a few local CLI-equivalent commands. For richer polling, webhook, inline buttons, callback queries, dialogue state, or larger bot workflows, prefer `teloxide`.

Telegram long polling must persist the highest handled `update_id` to `~/.tickclaw/telegram_state.json` so daemon restarts do not execute old Telegram commands again. On startup, TickClaw must call `getUpdates` with the persisted offset. If no persisted state exists, TickClaw may start from the first returned update and persist offsets as commands are handled; do not silently replay already persisted updates.

`~/.tickclaw/telegram_state.json` example:

```json
{
  "last_update_id": 123456789,
  "last_poll_at": "2026-05-16T20:00:00+02:00"
}
```

If a Telegram webhook is configured for the bot, `getUpdates` long polling will not work until the webhook is removed.

2. Config code agents:

```yaml
code_agents:
  gemini-3.1-flash-lite:
    command: /usr/bin/gemini
    args:
      - "--model"
      - gemini-3.1-flash-lite
      - "--skip-trust"
      - "--approval-mode"
      - yolo
      - "--output-format"
      - json
      - "--prompt"
      - "{prompt}"
    resume_args:
      - "--model"
      - gemini-3.1-flash-lite
      - "--skip-trust"
      - "--approval-mode"
      - yolo
      - "--resume"
      - "{sessionId}"
      - "--output-format"
      - json
      - "--prompt"
      - "{prompt}"

  gpt-5.3-codex-spark:
    command: /usr/bin/codex
    args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - "danger-full-access"
      - "-m"
      - gpt-5.3-codex-spark
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
    resume_args:
      - exec
      - resume
      - "{sessionId}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "-m"
      - gpt-5.3-codex-spark
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
  gpt-5.5:
    command: /usr/bin/codex
    args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - "danger-full-access"
      - "-m"
      - gpt-5.5
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
    resume_args:
      - exec
      - resume
      - "{sessionId}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "-m"
      - gpt-5.5
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "-"
```

`{prompt}` is replaced with the task prompt when a runner needs prompt-in-args. `{sessionId}` is replaced with the previous successful session id for `session: reuse`.

## Runtime Defaults

- Require an explicit task directory.
- Run every task from its own task directory.
- Task configs must not define `workspace`, `notify`, or `concurrency`.
- Task configs must not define Codex sandbox/model fields; runner flags are controlled only by `code_agents` command args in local config.
- TickClaw does not manage sandbox policy itself. The default Codex templates use `danger-full-access`, and users can change runner flags only through local `code_agents` config.
- Store Telegram secrets only in local `~/.tickclaw/config.yaml`.
- Redact bot tokens in logs.
- Write logs under each task's `logs/` directory.
- Keep task logs for six months. Compress completed monthly log sets into `.tgz` archives under the same task `logs/` directory.
- Do not expose a public HTTP server in the MVP.
- Use lock files to prevent duplicate execution.
- Validate `task.yaml` before running anything.
- Enforce execution timeouts.

## Code Agent Runner Notes

Current implementation should not manage runner sandbox/model flags in `task.yaml`. Code-agent command lines are assembled from local `code_agents.<runner>.command`, `args`, and `resume_args`; users own those arguments. Template runner keys are `gemini-3.1-flash-lite`, `gpt-5.3-codex-spark`, and `gpt-5.5`. TickClaw templates use Codex `danger-full-access` and Gemini `--approval-mode yolo`.

## Development Checks

Write through unit tests in advance before implementing functionality.

Prefer repository-level integration tests under `tests/` for user-facing behavior. Small module-level unit tests in `src/*.rs` are allowed for private parsing, validation, formatting, and normalization helpers when integration tests would be awkward.

When developing code, write clear comments for human code review. Each source file should start with a brief file-level comment describing its responsibility. Public structs, enums, and non-trivial functions should have concise comments explaining their purpose, inputs, outputs, side effects, and important failure behavior. Complex control flow should include short inline comments before the logic it explains.

All project decisions, requirements, and implementation rules agreed in agent conversations must be written back to `AGENTS.md` before implementation continues. This rule itself must remain in `AGENTS.md` so future agent sessions inherit it.

Update `skills/` and `README.md` according to the latest AGENTS.md. Project skills are for operating and configuring an installed TickClaw instance, not for developing TickClaw itself. They must be self-contained guides for code agents that may not have access to TickClaw's Rust source, and should explain how to use the installed `tickclaw` CLI, configure `~/.tickclaw/config.yaml`, create task directories, manage `task.yaml`, and send Telegram messages.

Use `make syncdoc` when only `AGENTS.md` should be committed and pushed. The target runs `git add AGENTS.md`, `git commit -m "sync"`, and `git push`.

Makefile targets:

- `make`: build the debug binary with `cargo build`.
- `make verify`: run `cargo fmt`, `cargo check`, `cargo test`, and `cargo clippy -- -D warnings`.
- `make install`: run `cargo install --path . $(CARGO_INSTALL_ARGS)`, write `~/.config/systemd/user/tickclaw.service`, run `systemctl --user enable --now tickclaw.service`, and try `loginctl enable-linger "$USER"` so the user service can start at boot. `CARGO_INSTALL_ARGS` defaults to `--force`; pass Cargo install options through it, for example `make install CARGO_INSTALL_ARGS='--root ~/.local --force'`.
- `make uninstall`: stop and disable the user service, remove the service file, and remove the installed binary.
- `make service-status`: show the user service status.

Before committing Rust code changes, run:

```bash
cargo fmt
cargo check
cargo test
cargo clippy -- -D warnings
```

For behavior changes, also run a temporary-home smoke test:

```bash
rm -rf /home/wenyuan/TickClaw/.tickclaw-test
cargo run -- --home /home/wenyuan/TickClaw/.tickclaw-test init
target/debug/tickclaw --home /home/wenyuan/TickClaw/.tickclaw-test check
target/debug/tickclaw --home /home/wenyuan/TickClaw/.tickclaw-test task list
target/debug/tickclaw --home /home/wenyuan/TickClaw/.tickclaw-test task run regular-check
target/debug/tickclaw --home /home/wenyuan/TickClaw/.tickclaw-test task status regular-check
rm -rf /home/wenyuan/TickClaw/.tickclaw-test
```

The smoke test should use the public task-management command surface. Do not use the old low-level `run`, `state`, or `logs` commands for documented behavior checks.

## Planned Work

- [x] Add skills according agents.md.
- [x] Implement `tickclaw check`.
- [x] Align task schema and scheduler rules with this document.
- [x] Implement `tickclaw daemon`.
- [x] Implement `tickclaw task list`.
- [x] Implement `tickclaw task run <name>`.
- [x] Implement `tickclaw task status <name>`.
- [x] Implement `tickclaw task enable <name>`.
- [x] Implement `tickclaw task disable <name>`.
- [x] Add Telegram polling/ingress.
- [x] Map `/task_list`, `/task_run`, `/task_status`, `/task_enable`, and `/task_disable` to CLI handlers.
- [x] Add Gemini runner.
- [ ] Add Claude runner.
- [ ] Add six-month task log retention and monthly `.tgz` log archives.
