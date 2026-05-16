---
name: tickclaw-operations
description: Operate and configure TickClaw. Use this when configuring TickClaw, creating scheduled command or agent tasks, sending Telegram notifications from code agents, inspecting task status, enabling or disabling tasks, or editing files under ~/.tickclaw; this guide is self-contained for agents that cannot read the Rust source.
---

# TickClaw Operations

This skill is a model-neutral TickClaw guide for Codex, Claude, Gemini, and
other code agents. It is only for operating and configuring an installed
TickClaw instance. Prefer the public `tickclaw` CLI and the files under
`~/.tickclaw/`.

Use this skill when you need to configure TickClaw, create or edit scheduled
tasks, send Telegram notifications, inspect task status, or enable and disable
tasks.

## Source Of Truth

- Operate only through the installed `tickclaw` command and `~/.tickclaw/` files
  described here.
- Do not require MCP for task management. TickClaw is managed by files and CLI
  commands.
- TickClaw does not use Linux `cron`; it has its own daemon scheduler.

## Essential CLI

```bash
tickclaw init
```

Create the TickClaw home directory, default config, and example tasks.

```bash
tickclaw check
```

Validate `~/.tickclaw/config.yaml` and all task definitions under
`~/.tickclaw/tasks/`.

```bash
tickclaw daemon
```

Start the scheduler loop. When Telegram is configured, the daemon also starts
the Telegram long-polling command loop.

```bash
tickclaw task list
```

List all tasks with enabled state, type, runner, schedule description, latest
state, next run time, and counters.

```bash
tickclaw task status <task>
```

Show one task's `task.yaml` fields, `agent.md` or `run.sh` content, latest
`state.json`, and the first 20 lines of the latest log.

```bash
tickclaw task run <task>
```

Run one task immediately through the public task-management surface.

```bash
tickclaw task enable <task>
tickclaw task disable <task>
```

Set `enabled: true` or `enabled: false` in the task's `task.yaml`.

## Telegram Notifications

Use Telegram for progress updates when operating remotely.

```bash
tickclaw telegram 'MarkdownV2 *message* text'
```

Send a MarkdownV2 message to the configured Telegram chat. Escape dynamic
content before inserting it into MarkdownV2. Put task names, paths, and other
dynamic values in code spans when possible.

```bash
tickclaw telegram --photo /path/to/image.png --caption 'MarkdownV2 caption'
tickclaw telegram --document /path/to/file --caption 'MarkdownV2 caption'
```

Send a photo or document with an optional MarkdownV2 caption. Use documents for
large logs, scripts, reports, and arbitrary text blocks.

Operational guidance:

- Telegram secrets live only in `~/.tickclaw/config.yaml`.
- Never print, commit, or copy real bot tokens or chat ids into shared files.
- CLI `tickclaw telegram ...` messages and captions are MarkdownV2-formatted.
- TickClaw sanitizes CLI-authored Telegram text before sending so ordinary
  punctuation does not break Telegram parsing.
- Escape MarkdownV2 control characters in user-controlled text:

  ```text
  _ * [ ] ( ) ~ ` > # + - = | { } . ! \
  ```
- Daemon-generated compact summaries use Telegram MarkdownV2 internally and must
  escape user-controlled content.
- Agent tasks may call `tickclaw telegram ...` themselves when a notification is
  useful. The daemon does not auto-notify successful agent tasks.
- The daemon sends fallback Telegram notifications for task failures, timeouts,
  and lock conflicts.
- Shell task success sends Telegram only when stdout is non-empty. Shell task
  failure sends Telegram even when stdout is empty.

Telegram ingress commands handled by the daemon:

```text
/help
/task_list
/task_status <task>
/task_run <task>
/task_enable <task>
/task_disable <task>
```

Use slash commands only. Do not rely on bare text aliases such as `tasklist`.

## Runtime Layout

```text
~/.tickclaw/
  config.yaml
  telegram_state.json
  tickclaw.log
  tasks/
    <task-name>/
      task.yaml
      agent.md      # agent tasks only
      run.sh        # command tasks only
      data/
      logs/
      state.json
      .tickclaw.lock
```

Task-owned files:

- `task.yaml`
- `agent.md`
- `run.sh`

Task execution or sub-agent-owned files:

- `data/`

Runtime-owned files:

- `state.json`
- `logs/`
- `.tickclaw.lock`
- `~/.tickclaw/telegram_state.json`

Do not manually edit runtime-owned files or `data/` unless explicitly debugging
runtime state corruption or task output issues.

## Configuring TickClaw

Local config file:

```text
~/.tickclaw/config.yaml
```

Minimal Telegram config:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Code-agent runners are configured under `code_agents.<runner>`:

```yaml
code_agents:
  gpt-5.3-codex-spark:
    command: /usr/bin/codex
    args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - danger-full-access
      - "-m"
      - gpt-5.3-codex-spark
      - "--skip-git-repo-check"
      - "-"
    resume_args:
      - exec
      - resume
      - "{sessionId}"
      - "-m"
      - gpt-5.3-codex-spark
      - "--skip-git-repo-check"
      - "-"
```

Place runner model, sandbox, and safety flags only in `code_agents` command
args. Do not put runner-specific flags in `task.yaml`.

Common template runner keys:

- `gemini-3.1-flash-lite`
- `gpt-5.3-codex-spark`
- `gpt-5.5`

## Task YAML Rules

Cron expressions in YAML must always be quoted:

```yaml
schedule: "*/10 * * * *"
```

Unquoted `*` can be parsed as YAML alias syntax.

Common fields:

```yaml
name: task-name
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 1800
```

Allowed `type` values:

- `command`: runs `run.sh` through shell from the task directory.
- `agent`: reads `agent.md` as the prompt and runs a configured code-agent
  runner.

Do not add these fields to `task.yaml`:

- `workspace`
- `notify`
- `concurrency`
- Codex, Gemini, Claude, model, sandbox, or safety argument blocks

Every task runs from its own directory:

```text
~/.tickclaw/tasks/<task-name>/
```

## Command Task Template

Directory:

```text
~/.tickclaw/tasks/regular-check/
```

`task.yaml`:

```yaml
name: regular-check
enabled: true
schedule: "*/10 * * * *"
type: command
timeout: 1800
```

`run.sh`:

```bash
#!/usr/bin/env bash
set -euo pipefail

printf 'regular-check ok: %s\n' "$(date --iso-8601=seconds)"
```

Command tasks must not define `runner`.

## Agent Task Template

Directory:

```text
~/.tickclaw/tasks/smoke-task/
```

`task.yaml`:

```yaml
name: smoke-task
enabled: false
schedule: "0 9 * * *"
runner: gpt-5.3-codex-spark
type: agent
session: independent
timeout: 3600
```

`agent.md`:

```markdown
Inspect this task directory and report any important findings.

Use `tickclaw telegram "..."` only when the result is useful to send.
```

Agent task fields:

- `runner`: required key under `code_agents.<runner>` in
  `~/.tickclaw/config.yaml`.
- `session`: `independent` starts a new agent session every run. `reuse`
  resumes the previous successful session when the runner supports resume.

## Editing Workflow

When creating or changing a task:

1. Send a short Telegram note if useful:

   ```bash
   tickclaw telegram 'TickClaw: updating task `<task-name>`'
   ```

2. Create or edit files under:

   ```text
   ~/.tickclaw/tasks/<task-name>/
   ```

3. Validate all TickClaw-controlled files:

   ```bash
   tickclaw check
   ```

4. Inspect the task:

   ```bash
   tickclaw task status <task-name>
   ```

5. Run the task manually when safe:

   ```bash
   tickclaw task run <task-name>
   ```

6. Check the task list:

   ```bash
   tickclaw task list
   ```

7. Send the result or attach a log if useful:

   ```bash
   tickclaw telegram 'TickClaw: task `<task-name>` updated and validated'
   tickclaw telegram --document ~/.tickclaw/tasks/<task-name>/logs/<log>.log --caption 'latest log'
   ```

## Scheduler Semantics

- TickClaw does not backfill missed scheduled runs.
- On daemon startup or schedule change, `next_run_at` is recomputed as the first
  future occurrence after now.
- Manual `tickclaw task run <task>` updates last-run fields and counters, but it
  does not shift a future scheduled `next_run_at` unless the task was already
  due.
- Task success, failure, timeout, and lock conflict all record state. Scheduled
  attempts advance `next_run_at`.
- Existing task locks are reported as `Run Failure`. There is no queue,
  parallel execution, or skip policy.
