---
name: tinybutler-operations
description: Operate and configure TinyButler. Use this when configuring TinyButler, creating scheduled command or agent tasks, sending Telegram notifications from code agents, inspecting task status, enabling or disabling tasks, or editing files under ~/.tinybutler; this guide is self-contained for agents that cannot read the Rust source.
---

# TinyButler Operations

This skill is a model-neutral TinyButler guide for Codex, Claude, Gemini, and
other code agents. It is only for operating and configuring an installed
TinyButler instance. Prefer the public `tinybutler` CLI and the files under
`~/.tinybutler/`.

Use this skill when you need to configure TinyButler, create or edit scheduled
tasks, send Telegram notifications, inspect task status, or enable and disable
tasks.

## Source Of Truth

- Operate only through the installed `tinybutler` command and `~/.tinybutler/` files
  described here.
- Do not require MCP for task management. TinyButler is managed by files and CLI
  commands.
- TinyButler does not use Linux `cron`; it has its own daemon scheduler.

## Essential CLI

```bash
tinybutler init
```

Create the TinyButler home directory, default config, and example tasks.

```bash
tinybutler check
```

Validate `~/.tinybutler/config.yaml` and all task definitions under
`~/.tinybutler/tasks/`.

```bash
tinybutler daemon
tinybutler restart
```

Start the scheduler loop. When Telegram is configured, the daemon also starts
the Telegram long-polling command loop.

Use `tinybutler restart` after editing `~/.tinybutler/config.yaml` so the
daemon reloads runner and Telegram settings. Task definition updates do not
require a restart; the scheduler picks them up on the next tick. The restart
command validates config and scheduled tasks first, then signals the running
daemon to re-exec itself in place without calling `systemctl restart`.

```bash
tinybutler tasks
tinybutler task list
tinybutler task status <task>
```

Open the task selector. In an interactive terminal, choose a task with the
keyboard, inspect its `task.yaml` plus `agent.md` or `run.sh`, then choose
`status`, `run`, `enable`, `disable`, or `exit`.

Use `tinybutler task list` in scripts and agent workflows to print all tasks
with enabled state, type, agents, schedule description, latest state, next run
time, and counters. Use `tinybutler task status <task>` to print task details,
`state.json`, and the latest-log preview. For long logs, the preview shows the
first seven lines, an ellipsis, and the last seven lines, with each displayed
line truncated to 80 characters.

```bash
tinybutler chat new
tinybutler chat session
```

Start a local interactive code-agent chat session or resume a previous one.
Choose a model/session from the prompt, then type messages. Use `/exit` to
detach from the REPL without deleting the resumable session. Use Ctrl+C while a
turn is running to abort that turn. Local REPL sessions should print local
artifact paths; Telegram bridge sessions can use TinyButler's attachment delivery
rules below.

## Telegram Notifications

Use Telegram for progress updates when operating remotely.

```bash
tinybutler telegram 'Markdown **message** text'
```

Send an ordinary Markdown message to the configured Telegram chat. TinyButler
converts it to Telegram MarkdownV2 before delivery. Put task names, paths, and
other dynamic values in Markdown code spans when possible.

```bash
tinybutler telegram --task "$TINYBUTLER_TASK_NAME" 'Markdown **message** text'
```

When a task process sends its own Telegram message, pass the current task name
through `--task`. TinyButler sets `TINYBUTLER_TASK_NAME` for shell and agent
task runners. The `--task` value is added to the visible Telegram message so
future replies include the originating task context for the chat bridge and for
debugging.

```bash
tinybutler telegram --task "$TINYBUTLER_TASK_NAME" --attachment /path/to/file --caption 'Markdown caption'
```

Send an attachment with an optional Markdown caption. TinyButler chooses the
Telegram display from the file type and falls back to document-style delivery for
large logs, scripts, reports, and arbitrary files. Include `--task` for
task-originated attachments so the caption carries the task name.

When you are talking through the TinyButler Telegram chat bridge and need to
return an image, screenshot, or other artifact, create a local file and either
call `tinybutler telegram --attachment <path> --caption '<short caption>'`
directly or put `ATTACH:<path>` on its own line in your final answer.
TinyButler removes the `ATTACH:` marker from visible text and uploads supported
attachments to Telegram.
TinyButler can also detect an existing local image path in the final answer as a
fallback, but `ATTACH:<path>` is the preferred explicit protocol because it avoids
ambiguity.
Do not expose hidden chain-of-thought or scratchpad content in Telegram replies.

Operational guidance:

- Telegram secrets live only in `~/.tinybutler/config.yaml`.
- Never print, commit, or copy real bot tokens or chat ids into shared files.
- CLI `tinybutler telegram ...` messages and captions are ordinary Markdown
  inputs converted to Telegram MarkdownV2 by TinyButler.
- Daemon-generated summaries, shell task summaries, and chat-bridge replies are
  ordinary Markdown inputs converted to Telegram MarkdownV2 by TinyButler.
- Shell command tasks must print Markdown text on stdout. Non-empty stdout may
  become the Telegram task summary, so write concise Markdown and avoid
  terminal-only formatting or raw control sequences.
- Agent tasks may call `tinybutler telegram --task "$TINYBUTLER_TASK_NAME" ...`
  themselves when a notification is useful. The daemon does not auto-notify
  successful agent tasks.
- The daemon sends fallback Telegram notifications for task failures, timeouts,
  and lock conflicts.
- Shell task success sends Telegram only when stdout is non-empty. Shell task
  failure sends Telegram even when stdout is empty.

Telegram ingress commands handled by the daemon:

```text
/help
/tasks
/restart
/new
/session
/abort
```

Use `/tasks` for task management. It opens a Telegram button menu, and selecting
a task opens its detail view with `status`, `run`, `enable` or `disable`, and
`exit` actions. Use `/restart` to validate config and task files, then restart
the daemon. After `/new` or `/session` selects a code-agent session, bare text in
the configured Telegram chat is redirected to that active session until it is
replaced. `/abort` interrupts the active turn without deleting the resumable
session.

## Runtime Layout

```text
~/.tinybutler/
  config.yaml
  telegram_state.json
  chat_state.json
  chat.lock
  tinybutler.log
  tasks/
    <task-name>/
      task.yaml
      agent.md      # agent tasks only
      run.sh        # command tasks only
      data/
      logs/
      state.json
      .tinybutler.lock
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
- `.tinybutler.lock`
- `~/.tinybutler/telegram_state.json`
- `~/.tinybutler/chat_state.json`
- `~/.tinybutler/chat.lock`

Do not manually edit runtime-owned files or `data/` unless explicitly debugging
runtime state corruption or task output issues.

TinyButler keeps task logs by calendar month: the current month plus the
previous five months are retained, completed retained months are compressed into
`logs/YYYY-MM.tgz`, and older raw logs and archives are deleted.

## Configuring TinyButler

Local config file:

```text
~/.tinybutler/config.yaml
```

Minimal Telegram config:

```yaml
telegram:
  bot_token: "123456789:..."
  chat_id: "123456789"
```

Code-agent backends are configured under `code_agents.<group>`, where the group
is a backend label such as `codex` or `gemini`:

```yaml
code_agents:
  codex:
    command: /usr/bin/codex
    models:
      - gpt-5.3-codex-spark
      - gpt-5.5
    new_args:
      - exec
      - "--json"
      - "--color"
      - never
      - "--sandbox"
      - danger-full-access
      - "-m"
      - "{model}"
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "{prompt}"
    resume_args:
      - exec
      - resume
      - "--json"
      - "-m"
      - "{model}"
      - "-c"
      - sandbox_mode="danger-full-access"
      - "-c"
      - service_tier="fast"
      - "--skip-git-repo-check"
      - "{sessionId}"
      - "{prompt}"
```

Place backend model, sandbox, and safety flags only in `code_agents` command
argument templates. Do not put runner-specific flags in `task.yaml`.

Task `agents` entries are `group/model` references to models listed under
`code_agents.<group>.models`. TinyButler resolves each reference to its configured
backend group. In `new_args` and `resume_args`, use `{prompt}` where the prompt
should be inserted as an argument, or include one literal `{stdin}` argument to
send the prompt through standard input. TinyButler removes the `{stdin}` marker
before spawning the command.

For scheduled task runners, `{model}` is replaced with the unqualified model
name from the matched `group/model` reference.

When starting a chat session, TinyButler shows grouped model references such as
`codex/gpt-5.5`; `{model}` in `stream_args` is expanded to the unqualified model
name from that reference.

Common template model names:

- `gemini/gemini-3.1-flash-lite`
- `codex/gpt-5.3-codex-spark`
- `codex/gpt-5.5`

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
- `agent`: reads `agent.md` as the task-owned prompt, prepends TinyButler
  runtime context, and runs a configured code-agent runner.

Do not add these fields to `task.yaml`:

- `workspace`
- `notify`
- `concurrency`
- Codex, Gemini, Claude, model, sandbox, or safety argument blocks

Every task runs from its own directory:

```text
~/.tinybutler/tasks/<task-name>/
```

## Command Task Template

Directory:

```text
~/.tinybutler/tasks/regular-check/
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

printf '**regular-check ok:** `%s`\n' "$(date --iso-8601=seconds)"
```

Command tasks must not define `agents`. Write stdout as Markdown text because
TinyButler may use non-empty stdout directly as the Telegram task summary.

## Agent Task Template

Directory:

```text
~/.tinybutler/tasks/smoke-task/
```

`task.yaml`:

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

`agent.md`:

```markdown
Inspect this task directory and report any important findings.

Use `tinybutler telegram "..."` only when the result is useful to send.
```

Agent task fields:

- `agents`: required ordered list of `group/model` references listed under
  `code_agents.<group>.models` in `~/.tinybutler/config.yaml`. Use a one-element
  list for a single model. TinyButler resolves each model to its backend group,
  tries each model in order, and reports task failure only after the final agent
  fails.
- `session`: `independent` starts a new agent session every run. `reuse`
  resumes the previous successful session when the runner supports resume.

## Editing Workflow

When creating or changing a task:

1. Send a short Telegram note if useful:

   ```bash
   tinybutler telegram 'TinyButler: updating task `<task-name>`'
   ```

2. Create or edit files under:

   ```text
   ~/.tinybutler/tasks/<task-name>/
   ```

3. Validate all TinyButler-controlled files:

   ```bash
   tinybutler check
   ```

4. Inspect the task:

   ```bash
   tinybutler task status <task-name>
   ```

5. Run the task manually when safe:

   ```bash
   tinybutler tasks
   ```

6. Check the task list:

   ```bash
   tinybutler task list
   ```

7. Send the result or attach a log if useful:

   ```bash
   tinybutler telegram 'TinyButler: task `<task-name>` updated and validated'
   tinybutler telegram --attachment ~/.tinybutler/tasks/<task-name>/logs/<log>.log --caption 'latest log'
   ```

## Scheduler Semantics

- TinyButler does not backfill missed scheduled runs.
- On daemon startup or schedule change, `next_run_at` is recomputed as the first
  future occurrence after now.
- Manual runs from the `tinybutler tasks` selector update last-run fields and
  counters, but they do not shift a future scheduled `next_run_at` unless the
  task was already due.
- Task success, failure, timeout, and lock conflict all record state. Scheduled
  attempts advance `next_run_at`.
- Existing task locks are reported as `Run Failure`. There is no queue,
  parallel execution, or skip policy.
