# TinyButler Agent Notes

This file is the shared development guide for Codex, Claude, Gemini, and other coding agents working on TinyButler.

Keep this file as the repository entry point: project invariants, the documentation map, and development workflow belong here. User-facing usage belongs in `README.md`; detailed internal design rules belong in the focused files under `docs/`.

## Project Goal

TinyButler is a file-managed scheduler for agent tasks. It should stay small, inspectable, and easy to manage over code-agent CLIs such as Codex, Claude, Gemini, or a Telegram-bridged code-agent CLI.

Core principles:

- Tasks are files under `~/.tinybutler/tasks/`.
- TinyButler uses its own daemon loop for local-time schedules parsed by the Rust `croner` crate, instead of relying on system cron.
- Runtime state is stored beside the feature it belongs to, not in a central database.
- The local CLI is the primary command surface.
- Telegram command behavior must be explainable through the local CLI first.
- Project skills teach code agents how to operate an installed TinyButler instance.

## Architecture

```text
TinyButler daemon / agent CLI / Telegram-bridged agent CLI
            |
            v
~/.tinybutler/tasks/*/task.yaml
            |
            v
 shell runner / agent runner -> stdout
            |
            v
 logs/ + state.json + Telegram notification
```

## CLI Surface

```bash
tinybutler init                                # create ~/.tinybutler with config and example tasks
tinybutler daemon                              # start the scheduler loop and Telegram ingress when configured
tinybutler check                               # validate TinyButler-controlled config and task files
tinybutler telegram '<message>'                # send a Telegram Markdown-authored message
tinybutler telegram --attachment <path>        # send a Telegram attachment with an optional caption

tinybutler tasks                               # open the task selector
tinybutler task list                           # print task summaries for scripts and agents
tinybutler task status <task>                  # print formatted runtime state and latest log preview

tinybutler chat new                            # start a local interactive code-agent chat session
tinybutler chat session                        # list and resume a local interactive chat session
```

## Repository Layout

```text
TinyButler/
  README.md      # user-facing usage and project overview
  AGENTS.md      # agent entry point, documentation map, and development workflow
  Cargo.toml     # Rust package metadata and dependencies
  docs/          # focused internal design notes
  templates/     # files copied into initialized TinyButler homes, including .agents/skills
  tests/         # repository-level integration tests
  src/
    lib.rs            # library module exports
    main.rs           # CLI parsing and top-level command dispatch
    config.rs         # home directory, config loading, and init support
    task.rs           # task.yaml schema, defaults, and validation
    cron_expr.rs      # cron normalization and schedule descriptions
    scheduler.rs      # daemon loop, due checks, and run orchestration
    runner.rs         # shell and agent task execution
    chat.rs           # interactive chat bridge state and adapters
    telegram.rs       # Telegram delivery and ingress helpers
    state.rs          # per-task state.json model and persistence
    lock.rs           # per-task lock file acquisition and cleanup
    log_retention.rs  # task log archive and retention maintenance
```

## Design Notes

- [docs/configuration.md](docs/configuration.md) owns the TinyButler home layout, local config boundaries, and `code_agents.<runner>` configuration.
- [docs/telegram-ingress.md](docs/telegram-ingress.md) owns inbound Telegram polling, command routing, authorization, and bot menu registration.
- [docs/markdown-message.md](docs/markdown-message.md) owns outbound Telegram Markdown conversion, attachment delivery, and `ATTACH:` directives.
- [docs/tasks.md](docs/tasks.md) owns task files, `task.yaml`, task commands, scheduling, task state, notifications, and log retention.
- [docs/chatbridge.md](docs/chatbridge.md) owns the interactive chat bridge behavior, state machine, and adapter architecture.
- [docs/integration-testing-requirements.md](docs/integration-testing-requirements.md) owns manual and semi-automated integration test requirements.

## Testing

Write focused tests before implementing behavior. Prefer repository-level integration tests under `tests/` for user-facing behavior, and use small module-level unit tests in `src/*.rs` for private parsing, validation, formatting, and normalization helpers when integration tests would be awkward.

Manual and semi-automated end-to-end testing requirements live in [docs/integration-testing-requirements.md](docs/integration-testing-requirements.md).

Before committing Rust code changes, run:

```bash
cargo fmt
cargo check
cargo test
cargo clippy -- -D warnings
```

For behavior changes, also run a temporary-home smoke test through the public task-management surface:

```bash
rm -rf /home/wenyuan/TinyButler/.tinybutler-test
cargo run -- --home /home/wenyuan/TinyButler/.tinybutler-test init
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test check
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test tasks
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test task list
target/debug/tinybutler --home /home/wenyuan/TinyButler/.tinybutler-test task status smoke-task
rm -rf /home/wenyuan/TinyButler/.tinybutler-test
```

Do not use old low-level `run`, `state`, or `logs` commands for documented behavior checks.

## Code Review

Write clear comments for human code review. Each source file should start with a brief file-level comment describing its responsibility. Public structs, enums, and non-trivial functions should have concise comments explaining purpose, inputs, outputs, side effects, and important failure behavior. Complex control flow should include short inline comments before the logic it explains.

## Makefile Targets

- `make`: build the debug binary with `cargo build`.
- `make verify`: run `cargo fmt`, `cargo check`, `cargo test`, and `cargo clippy -- -D warnings`.
- `make install`: install the release binary, run `tinybutler init` to create missing home files and refresh project skills under `~/.tinybutler/.agents/skills/`, initialize `~/.tinybutler` as a git repository when needed, write the user service, enable and restart it, and try to enable linger for boot startup.
- `make syncskill`: copy project skills from `templates/.agents/skills/` into `~/.tinybutler/.agents/skills/` and restart the TinyButler user service.
- `make uninstall`: stop and disable the user service, remove the service file, remove the installed binary, and remove the installed TinyButler project skill directory from `~/.tinybutler/.agents/skills/`.
- `make service-status`: show the user service status.
- `make sync`: commit and push `AGENTS.md` and `docs/*.md` after recording agreed decisions, requirements, and implementation rules in `AGENTS.md` or the relevant authoritative design note. Keep `README.md` and `templates/.agents/skills/` aligned when those docs change user or installed-agent guidance, and never commit real Telegram tokens or chat ids.

`CARGO_INSTALL_ARGS` defaults to `--force` for `make install`. Pass Cargo install options through it, for example `make install CARGO_INSTALL_ARGS='--root ~/.local --force'`.

## Open Work

- Add the Gemini interactive streaming adapter.
- Add the Claude runner.
