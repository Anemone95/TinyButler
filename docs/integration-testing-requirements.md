# Integration Testing Requirements

This document defines TinyButler's manual and semi-automated integration test requirements. It is the checklist for behavior that cannot be fully proven by `cargo test` alone.

Run integration tests from the repository root after building the current binary:

```bash
cargo build
```

Use temporary TinyButler homes under the repository and remove them after each run. Do not run destructive tests against the user's real `~/.tinybutler/tasks`.

## Baseline Checks

Before behavior testing, run the standard Rust verification:

```bash
cargo fmt
cargo check
cargo test
cargo clippy -- -D warnings
```

Then run the public temporary-home smoke test:

```bash
rm -rf /home/wenyuan/TinyButler/.tinybutler-test
cargo run -- --home /home/wenyuan/TinyButler/.tinybutler-test init
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test check
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test tasks
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test task list
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test task status smoke-task
rm -rf /home/wenyuan/TinyButler/.tinybutler-test
```

Expected result: `check` succeeds, `tasks` opens or prints the initialized example tasks depending on terminal context, `task list` prints script-friendly task summaries, and `task status smoke-task` prints formatted runtime state. Do not use old low-level `run`, `state`, or `logs` commands for documented behavior checks.

## Local Tasks Menu

Exercise the real interactive terminal menu with a PTY:

```bash
rm -rf /home/wenyuan/TinyButler/.tinybutler-menu-test
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-menu-test init
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-menu-test check
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-menu-test tasks
```

In the menu:

- Select `regular-check`.
- Confirm the detail view shows formatted task fields and the human-readable schedule, without raw task definition or script content.
- Choose `run`; confirm it finishes with `success`.
- Choose `status`; confirm formatted runtime state and latest-log preview are shown.
- Choose `disable`; confirm the action changes to `enable`.
- Choose `enable`; confirm the action changes back to `disable`.
- Quit with `q`.

Post-check:

```bash
grep -q 'enabled: true' /home/wenyuan/TinyButler/.tinybutler-menu-test/tasks/regular-check/task.yaml
grep -q '"last_status": "success"' /home/wenyuan/TinyButler/.tinybutler-menu-test/tasks/regular-check/state.json
test -n "$(find /home/wenyuan/TinyButler/.tinybutler-menu-test/tasks/regular-check/logs -name '*.log' -print -quit)"
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-menu-test check
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-menu-test task list
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-menu-test task status regular-check
rm -rf /home/wenyuan/TinyButler/.tinybutler-menu-test
```

## Scheduler Daemon

The scheduler must be tested with a real daemon loop, not only manual task runs.

Use a temporary home with second-level schedules:

```bash
set -euo pipefail
HOME_DIR=/home/wenyuan/TinyButler/.tinybutler-scheduler-full-test
rm -rf "$HOME_DIR"
target/debug/tinybutler --home "$HOME_DIR" init
sed -i 's#schedule: "\*/10 \* \* \* \*"#schedule: "*/1 * * * * *"#' "$HOME_DIR/tasks/regular-check/task.yaml"
```

Add a locked task to verify concurrency handling:

```bash
mkdir -p "$HOME_DIR/tasks/locked-task"
cat >"$HOME_DIR/tasks/locked-task/task.yaml" <<'YAML'
name: locked-task
enabled: true
schedule: "*/1 * * * * *"
type: command
timeout: 10
YAML
cat >"$HOME_DIR/tasks/locked-task/run.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'locked task should not actually run\n'
SH
chmod +x "$HOME_DIR/tasks/locked-task/run.sh"
: >"$HOME_DIR/tasks/locked-task/.tinybutler.lock"
```

Start the daemon, then add a task while it is running to verify per-tick rescans:

```bash
target/debug/tinybutler --home "$HOME_DIR" daemon --interval-seconds 1 > /tmp/tinybutler-scheduler-daemon.out 2> /tmp/tinybutler-scheduler-daemon.err &
daemon_pid=$!
sleep 3
mkdir -p "$HOME_DIR/tasks/dynamic-task"
cat >"$HOME_DIR/tasks/dynamic-task/task.yaml" <<'YAML'
name: dynamic-task
enabled: true
schedule: "*/1 * * * * *"
type: command
timeout: 10
YAML
cat >"$HOME_DIR/tasks/dynamic-task/run.sh" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf 'dynamic task ran at %s\n' "$(date --iso-8601=seconds)"
SH
chmod +x "$HOME_DIR/tasks/dynamic-task/run.sh"
sleep 5
kill -INT "$daemon_pid" 2>/dev/null || true
wait "$daemon_pid" || true
```

Expected checks:

```bash
grep -q '"last_status": "success"' "$HOME_DIR/tasks/regular-check/state.json"
grep -q 'regular-check ok:' "$HOME_DIR/tasks/regular-check/logs"/*.log
grep -q '"last_status": "success"' "$HOME_DIR/tasks/dynamic-task/state.json"
grep -q 'dynamic task ran' "$HOME_DIR/tasks/dynamic-task/logs"/*.log
grep -q '"last_status": "Run Failure"' "$HOME_DIR/tasks/locked-task/state.json"
grep -q 'task is already locked' "$HOME_DIR/tasks/locked-task/logs"/*.log
rm -rf "$HOME_DIR"
```

## Agent Runner Paths

At minimum, cover scheduled agent `new_args` and `resume_args` with a fake local runner so the test is deterministic:

- Add a fake backend group and `fake-agent` model under temporary `config.yaml`.
- Create an agent task with `session: reuse`.
- Run it once through `tinybutler tasks`; confirm `state.json` stores `session-fresh`.
- Inspect it with `tinybutler task status <task>`; confirm the output shows formatted runtime state and latest-log preview.
- Run it again; confirm logs show resume with `session-fresh` and `state.json` stores `session-resumed`.

When real credentials and time allow, test each configured real runner:

- Plain scheduled mode: `new_args`.
- Resume scheduled mode: `resume_args` with `session: reuse`.
- Interactive stream mode: `tinybutler chat new` for runners supported by the current Rust adapter.

Cover the `agy` adapter deterministically with a fake executable that emits the documented NDJSON protocol. At minimum, test initial conversation-id capture, assistant and tool deltas, successful completion, explicit resume arguments, terminal failures, malformed events, and abort behavior.

Also confirm that non-empty `stream_args` enables chat, that generic handling expands documented placeholders and otherwise preserves the complete configured argument vector, and that adapter selection uses the configured command executable rather than inspecting `stream_args`.

For a real Gemini smoke test, use `agy` 1.1.15 or newer and a configured model from `agy models`. Confirm that `tinybutler chat new --runner gemini/<model>` records the `init.conversation_id`, streams at least two turns in one local REPL, resumes the same conversation with `tinybutler chat session`, and leaves the session resumable after Ctrl+C aborts an active turn.

## Check Command Failure Cases

Break a temporary config or task file and confirm `tinybutler check` fails with a useful error:

- Remove a required runner `command`.
- Add obsolete fields such as `workspace`, `notify`, or `concurrency` to `task.yaml`.
- Remove `run.sh` from a command task.
- Point an agent task at a model name not listed under any `code_agents.<group>.models`.

Restore or delete the temporary home after the failure check.

## Telegram Bot API

Telegram tests need real local `telegram.bot_token` and `telegram.chat_id`. Use a temporary TinyButler home and copy only local config; never commit secrets.

If the installed user service is already polling the same bot, stop it during the test to avoid Telegram Bot API `409 Conflict`, then restore it:

```bash
systemctl --user is-active tinybutler.service
systemctl --user stop tinybutler.service
# run temporary daemon Telegram tests
systemctl --user start tinybutler.service
systemctl --user is-active tinybutler.service
```

Before starting the temporary daemon, seed `telegram_state.json` with the latest `update_id` from `getUpdates` so historical commands are not replayed.

Test slash command routing by sending messages to the bot:

- `/tasks` should reply `Select a task:`.
- `/new` should reply `Select a model:` when streaming-capable Codex runners are configured.
- `/session` should either show a session menu or reply `No resumable chat sessions`.
- `/abort` should abort a busy turn or reply `No active chat turn to abort`.
- `/restart` should validate config and task files, acknowledge the restart request, and then send the daemon startup notification after restart.

The available `mcp-telegram` tool can send and read messages but cannot click Telegram inline buttons. For callback-query coverage, verify the menu message is delivered and then click task/action buttons manually in a Telegram client, or use a future test tool that can trigger callback queries.

Test outbound delivery from the current binary:

```bash
target/debug/tinybutler telegram 'TinyButler integration test: Markdown **text** path.'
target/debug/tinybutler telegram --attachment /tmp/tinybutler-test-picture.png --caption 'TinyButler integration test picture.'
target/debug/tinybutler telegram --attachment /tmp/tinybutler-test-audio.mp3 --caption 'TinyButler integration test audio.'
```

Expected result: text, picture, and audio messages arrive in the configured Telegram chat.

## MCP Telegram

When `mcp-telegram` is available, separately test its command and media paths:

- Send a text message to `me`.
- Send an image attachment to `me`.
- Send an audio attachment to `me`.

If sending image and audio together fails with `Media invalid`, send them as separate messages. That still covers the file-upload paths used by the available MCP tool.

## Cleanup

After any integration run:

```bash
rm -rf /home/wenyuan/TinyButler/.tinybutler-test \
       /home/wenyuan/TinyButler/.tinybutler-menu-test \
       /home/wenyuan/TinyButler/.tinybutler-scheduler-full-test \
       /home/wenyuan/TinyButler/.tinybutler-telegram-menu-test
rm -f /tmp/tinybutler-*.out /tmp/tinybutler-*.err /tmp/tinybutler-*.png /tmp/tinybutler-*.mp3
systemctl --user is-active tinybutler.service 2>/dev/null || true
```
