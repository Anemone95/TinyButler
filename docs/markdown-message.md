# Markdown Delivery

This document is the authoritative design note for TinyButler's Markdown output convention, Telegram text formatting, attachment delivery, and chat-agent attachment directives.

## Delivery Path

TinyButler uses one Telegram-facing reply delivery path for TinyButler's own replies, shell task summaries, and agent-visible replies.

## Markdown Output

TinyButler output is authored as Markdown, whether it is printed to the CLI or sent to Telegram, and whether it comes from TinyButler itself or from task output summaries.

Shell command task stdout must also be authored as Markdown text. The daemon may use that stdout directly as a Telegram notification summary when it is non-empty, so shell scripts should print concise Markdown rather than terminal-only formatting or raw control sequences.

Telegram parse mode is `MarkdownV2`, so Telegram-bound Markdown is converted to safe Telegram MarkdownV2 before Bot API delivery.

This conversion applies to `tinybutler telegram` text messages, attachment captions, command/status/error messages, dynamic task names, paths, logs, prompts inserted into templates, shell task summaries, and chat-bridge code-agent replies.

Use the shared Markdown-to-Telegram conversion helper instead of ad hoc escaping. `escape_markdown_v2` is only a low-level fallback for conversion failures or already-plain text chunking.

Put dynamic task names, paths, log paths, and similar literal values in ordinary Markdown code spans before conversion.

## Size Handling

Large or arbitrary text blocks should be chunked or sent as documents instead of one oversized Telegram Markdown message.

Converted fenced code blocks are acceptable only for short previews.

## Attachment Uploads

Telegram attachments use one CLI surface and one multipart upload helper.

TinyButler chooses the Telegram Bot API send method from file type: image files use `sendPhoto`, animated GIF or silent animation-style files may use `sendAnimation`, MPEG4 video files use `sendVideo`, MP3/M4A audio files use `sendAudio`, and all other allowed files use `sendDocument`.

Captions follow the same Markdown-to-Telegram conversion path as text messages.

`tinybutler telegram` accepts `--task <task-name>` for task-originated messages. When present, TinyButler prefixes text messages and attachment captions with `**TinyButler task:** <task-name>` using a Markdown code span for the task name. This makes Telegram replies carry the originating task context back into the chat bridge and keeps manual debugging tied to the task that sent the message.

Telegram also has specialized displays such as `sendVoice`, `sendVideoNote`, and `sendSticker`. Add those only when TinyButler can validate their stricter format and semantic requirements instead of guessing from an ordinary attachment path.

## Attachment Directives

A code agent may request an outbound attachment by putting `ATTACH:<path-or-url>` on its own line in the final assistant-visible answer.

TinyButler must parse `ATTACH:` lines, remove them from visible Telegram text, validate the referenced files, and send allowed local files using the most suitable Telegram attachment method for their type.

Local `ATTACH:` paths may be absolute, home-relative with `~/`, or relative to the chat agent working directory. Chat agents run from the TinyButler home, for example `~/.tinybutler`, so relative attachment paths resolve there. Resolve and validate local paths before upload.

Do not treat arbitrary plain text or secret-like files as sendable attachments.

Captions should come from the remaining visible text, not from the `ATTACH:` marker itself. Keep Telegram caption limits in mind; long text should be sent separately.

TinyButler should expose the same behavior directly through `tinybutler telegram --attachment <path>` for agents that choose to send files themselves.

Agents running inside a TinyButler task receive `TINYBUTLER_TASK_NAME` in the process environment and should pass it through, for example `tinybutler telegram --task "$TINYBUTLER_TASK_NAME" 'message'`.

As a compatibility fallback for code agents that create a screenshot but forget the `ATTACH:` marker, TinyButler may detect existing local image paths mentioned plainly in the final assistant-visible answer and upload those supported image files once. `ATTACH:` remains the preferred explicit protocol.

## Transport Boundary

Outbound-only Telegram messages and attachments should use direct Telegram Bot HTTP API calls or a lightweight wrapper.

Inbound command routing, polling, callback authorization, and bot menu registration are owned by [telegram-ingress.md](telegram-ingress.md).
