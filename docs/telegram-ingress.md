# Telegram Ingress

This document is the authoritative design note for TinyButler's inbound Telegram polling, authorization, slash command routing, and bot menu registration.

## Configuration

Telegram ingress runs only when `telegram.bot_token` and `telegram.chat_id` are configured in local `~/.tinybutler/config.yaml`.

TinyButler must accept Telegram messages and callback queries only from the configured `telegram.chat_id`.

Telegram secrets are local configuration and must not be committed to the repository.

## Polling State

`tinybutler daemon` starts the Telegram ingress loop automatically when Telegram is configured.

Telegram long polling persists the highest handled `update_id` to `~/.tinybutler/telegram_state.json` so daemon restarts do not execute old commands again.

```json
{
  "last_update_id": 123456789,
  "last_poll_at": "2026-05-16T20:00:00+02:00"
}
```

On startup, TinyButler must call `getUpdates` with the persisted offset.

If no persisted state exists, TinyButler may start from the first returned update and persist offsets as commands are handled.

If a Telegram webhook is configured for the bot, `getUpdates` long polling will not work until the webhook is removed.

## Command Mapping

Every user-facing slash command must map to local CLI behavior first.

| Local CLI | Telegram | Meaning |
| --- | --- | --- |
| `tinybutler check` | `/check` | Validate TinyButler-controlled config and task files |

Task command mappings are defined in [tasks.md](tasks.md). Chat command mappings are defined in [chatbridge.md](chatbridge.md).

The bot also handles `/help` for the configured chat.

## Command Names

Telegram Bot API command names allow only lowercase English letters, digits, and underscores.

Use underscore command names such as `/task_list` for multiword commands.

Telegram bot menu descriptions should be short and must not include the project name, so the mobile command menu stays compact.

## Menu Refresh

On each daemon startup, TinyButler must refresh the Telegram bot menu with the current slash-command surface.

TinyButler should call `setMyCommands` for both the default bot-command scope and the configured chat scope.

Refreshing both scopes keeps Telegram clients from showing stale command descriptions after project renames or command additions.

## Text Routing

The bot must not respond to bare text aliases such as `tasklist`; users should use slash commands.

Bare non-command text is accepted only by the interactive chat bridge when a chat session is already active.

Bare text outside an active chat bridge session is ignored or answered with a short instruction when the current state is waiting for a menu selection.

## Callback Queries

Every Telegram callback query must be authorized against `telegram.chat_id`.

Every callback query must be acknowledged.

TinyButler must revalidate callback data against current config and runtime state before mutating state.

## Transport Boundary

Simple command polling may use direct Telegram Bot HTTP API calls.

Ingress features that need callback queries, inline menus, dialogue state, or streaming chat state may use `teloxide`.

Outbound Markdown conversion and attachment delivery are owned by [markdown-message.md](markdown-message.md).
