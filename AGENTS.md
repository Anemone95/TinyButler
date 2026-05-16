# TickClaw Agent Notes

This file is the shared development guide for Codex, Claude, Gemini, and other coding agents working on TickClaw.

User-facing usage belongs in `README.md`. Internal design rules, implementation constraints, and future development notes belong here.

## Project Goal

TickClaw is a file-managed scheduler for agent tasks. It should stay small, inspectable, and easy to manage over code agent cli such as codex, claude, or telegram bridged code agent cli.

Core principles:

- Tasks are files under `~/.tickclaw/tasks/`.
- TickClaw does not use Linux `cron`; it uses its own daemon loop with local-time cron expressions parsed by the Rust `cron` crate.
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
tickclaw telegram test --message '<message>' # send a test Telegram message using local config

tickclaw task list                           # list all tasks and their latest status
tickclaw task run <task>                     # run one task immediately through the task-management command surface
tickclaw task status <task>                  # show task details, latest state, and a log preview
```

## Repository Layout

```text
TickClaw/
  README.md      # user-facing usage and project overview
  AGENTS.md      # internal development guide for code agents
  Cargo.toml     # Rust package metadata and dependencies
  skills/        # project configurations skills and workflows for code agents
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
  config.yaml      # local TickClaw config and Telegram secrets
  tickclaw.log     # daemon-level log file
  tasks/           # task directories managed by files
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

Task execution or sub-agent-owned files:

- `data/`

Agents should generally edit task-owned files only. Do not manually rewrite runtime-owned files or task data unless explicitly debugging state corruption or task output issues.

## Task.yaml format

Agent task:

```yaml
name: smoke-task # task name; should match the task directory when possible
enabled: false # agent tasks should be opt-in or low-frequency unless cost is intentional
schedule: "0 9 * * *" # local-time cron expression; five fields are normalized with leading seconds
runner: codex # execution backend; codex for agent tasks
type: agent # task kind; agent tasks read agent.md as prompt
session: independent # independent starts fresh; reuse resumes the previous supported session
timeout: 3600 # maximum run time in seconds

codex: # Codex-specific runner options
  model: gpt-5.5 # Codex model used for this task
  sandbox: workspace-write # Codex sandbox mode; default should stay workspace-write
```

Shell task:

```yaml
name: regular-check # task name; should match the task directory when possible
enabled: true # whether the daemon should run this task on schedule
schedule: "*/10 * * * *" # local-time cron expression; cheap shell check every 10 minutes here
runner: shell # execution backend; shell runs run.sh with bash
type: command # task kind; command tasks execute run.sh
timeout: 1800 # maximum run time in seconds
```

`session` for agent tasks:

- `independent`: every run starts a new agent session.
- `reuse`: TickClaw resumes the previous successful session for this task when the runner supports session resume.

For Codex, `session: reuse` stores the latest Codex session id in `state.json` and uses `codex exec resume <session-id>` on the next run. If no previous session id exists, the first run starts a new session.

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
9. send Telegram notification for every task execution

The task directory is always the working directory. 
Telegram notification is always attempted after execution. 
If a task is already locked because the previous run has not finished, TickClaw should report that execution attempt as `Run Failure` instead of queuing, skipping by policy, or running in parallel.

The scheduler uses local-time cron expressions through the Rust `cron` crate. Five-field cron expressions are normalized by prefixing seconds with `0`.

## State File

`state.json` is daemon-owned.

```json
{
  "last_run_at": "2026-05-16T09:00:00+02:00",
  "last_status": "success",
  "last_exit_code": 0,
  "last_log": "logs/2026-05-16T09-00-00.log",
  "session_id": "00000000-0000-0000-0000-000000000000",
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
| `tickclaw task list` | `/task-list` | List all tasks and their latest status |
| `tickclaw task run <name>` | `/task-run <name>` | Run one task immediately |
| `tickclaw task status <name>` | `/task-status <name>` | Show task details, recent state, and the first 20 lines of the latest log |

Do not add Telegram-only behavior. If a Telegram feature cannot be explained as a local CLI command first, add the CLI command before adding the Telegram command.

## Telegram Task Management Design

`tickclaw task list` and `/task-list` should show:

- task name
- enabled or disabled
- task type and runner
- schedule
- last status
- last run time
- next run time
- run count and failure count
- whether the task is currently running

`tickclaw task status <name>` and `/task-status <name>` should show:

- task details from `task.yaml`
- latest state from `state.json`
- first 20 lines of the latest log, when a latest log exists

First implementation can be text-only. Inline buttons can be added later:

- `Refresh` -> `/task-list`
- `Status <task>` -> `/task-status <task>`
- `Run <task>` -> `/task-run <task>`

## config.yaml Config

1. Config telegram, TickClaw reads Telegram secrets from local `~/.tickclaw/config.yaml`.

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Do not put real Telegram tokens or chat ids in the repository, examples, or README.

Telegram parse mode is always Markdown and is not user-configurable.

Telegram sender should avoid leaking bot tokens in errors. If using reqwest errors, strip URLs before returning or logging errors.

2. Config code agents:

```yaml
code_agents:
  codex:
    command: /usr/bin/codex
    args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - workspace-write
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
    resume_args:
      - exec
      - resume
      - "{sessionId}"
      - "-c"
      - sandbox_mode="workspace-write"
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"

  google-gemini-cli:
    command: /usr/bin/gemini
    args:
      - "--skip-trust"
      - "--approval-mode"
      - yolo
      - "--output-format"
      - json
      - "--prompt"
      - "{prompt}"
    resume_args:
      - "--skip-trust"
      - "--approval-mode"
      - yolo
      - "--resume"
      - "{sessionId}"
      - "--output-format"
      - json
      - "--prompt"
      - "{prompt}"
```

`{prompt}` is replaced with the task prompt when a runner needs prompt-in-args. `{sessionId}` is replaced with the previous successful session id for `session: reuse`.

## Security Defaults

- Require an explicit task directory.
- Run every task from its own task directory.
- Use `--sandbox workspace-write` for Codex tasks by default.
- Do not default to `danger-full-access`.
- Store Telegram secrets only in local `~/.tickclaw/config.yaml`.
- Redact bot tokens in logs.
- Write logs under each task's `logs/` directory.
- Do not expose a public HTTP server in the MVP.
- Use lock files to prevent duplicate execution.
- Validate `task.yaml` before running anything.
- Enforce execution timeouts.

## Code Agent Runner Notes

Current implementation should prefer `workspace-write`; treat `danger-full-access` and Gemini `--approval-mode yolo` as explicit advanced configurations.

## Development Checks

Write through unit tests in advance before impelemnting functionality. 

Update `skills/` and `README.md` according to the latest AGENTS.md.
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

- [ ] Add skills according agents.md.
- [ ] Implement `tickclaw check`.
- [ ] Implement `tickclaw deamon`.
- [ ] Implement `tickclaw task list`.
- [ ] Implement `tickclaw task run <name>`.
- [ ] Implement `tickclaw task status <name>`.
- [ ] Add Telegram polling/ingress.
- [ ] Map `/task-list`, `/task-run`, and `/task-status` to CLI handlers.
- [ ] Add Claude and Gemini runners.
- [ ] Add log rotation.
