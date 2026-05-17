#!/usr/bin/env python3
"""Exercise Telegram reply flows through the mcp-telegram Telethon session."""

from __future__ import annotations

import argparse
import asyncio
import os
import secrets
import shutil
import sys
import tempfile
from pathlib import Path


DEFAULT_ENV_FILE = Path(
    os.environ.get(
        "MCP_TELEGRAM_ENV_FILE",
        str(Path.home() / "linux_dotfiles/_config/mcp-telegram/env"),
    )
)
DEFAULT_ENTITY = os.environ.get("MCP_TELEGRAM_ENTITY", "X95WynBot")
DEFAULT_STATE_DIR = Path.home() / ".local/state/mcp-telegram"


def load_env_file(path: Path) -> None:
    """Load simple KEY=VALUE lines into the current process environment."""
    if not path.exists():
        return
    for line in path.read_text().splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("#") or "=" not in stripped:
            continue
        key, value = stripped.split("=", 1)
        os.environ.setdefault(key.strip(), value.strip().strip("'\""))


def import_telethon():
    """Import Telegram client libraries after selecting the right Python env."""
    try:
        from mcp_telegram.telegram import Settings
        from telethon import TelegramClient
    except ModuleNotFoundError as err:
        raise SystemExit(
            "mcp_telegram/telethon is not importable. Run this script with the "
            "Python environment that has mcp-telegram installed."
        ) from err
    return Settings, TelegramClient


def session_base(args: argparse.Namespace) -> tuple[str, tempfile.TemporaryDirectory[str] | None]:
    """Return a Telethon session base path, copying MCP's session by default."""
    source = args.session_base.with_suffix(".session")
    if args.shared_session:
        return str(args.session_base), None
    if not source.exists():
        raise SystemExit(f"mcp-telegram session not found: {source}")
    tempdir = tempfile.TemporaryDirectory(prefix="tinybutler-mcp-telegram-")
    target_base = Path(tempdir.name) / "session"
    shutil.copy2(source, target_base.with_suffix(".session"))
    return str(target_base), tempdir


def create_client(args: argparse.Namespace):
    """Create a Telethon client with mcp-telegram credentials and session data."""
    Settings, TelegramClient = import_telethon()
    settings = Settings()  # type: ignore
    base, tempdir = session_base(args)
    client = TelegramClient(
        session=base,
        api_id=int(settings.api_id),
        api_hash=settings.api_hash.get_secret_value(),
    )
    return client, tempdir


async def list_messages(args: argparse.Namespace) -> None:
    """Print recent messages with ids and reply metadata."""
    client, tempdir = create_client(args)
    await client.connect()
    try:
        messages = await client.get_messages(args.entity, limit=args.limit)
        for message in messages:
            sender = await message.get_sender()
            reply_to = (
                message.reply_to.reply_to_msg_id
                if message.reply_to and message.reply_to.reply_to_msg_id
                else None
            )
            print(
                {
                    "id": message.id,
                    "out": message.out,
                    "sender": getattr(sender, "username", None),
                    "reply_to": reply_to,
                    "text": (message.raw_text or "")[: args.preview],
                }
            )
    finally:
        await client.disconnect()
        if tempdir is not None:
            tempdir.cleanup()


async def send_reply(args: argparse.Namespace) -> int:
    """Send a Telegram message as a reply to an existing message id."""
    client, tempdir = create_client(args)
    await client.connect()
    try:
        sent = await client.send_message(
            args.entity,
            args.message,
            reply_to=args.reply_to,
        )
        print({"sent_id": sent.id, "reply_to": args.reply_to, "text": args.message})
        return sent.id
    finally:
        await client.disconnect()
        if tempdir is not None:
            tempdir.cleanup()


async def smoke(args: argparse.Namespace) -> None:
    """Send a unique reply smoke-test prompt and poll for the robot response."""
    token = args.token or f"REPLY_OK_{secrets.randbelow(9000) + 1000}"
    args.message = (
        f"MCP reply smoke test {token}: "
        f"please reply with only {token}, and do not edit files."
    )
    sent_id = await send_reply(args)
    client, tempdir = create_client(args)
    await client.connect()
    try:
        for _ in range(args.polls):
            await asyncio.sleep(args.interval)
            messages = await client.get_messages(args.entity, limit=args.limit)
            for message in messages:
                if not message.out and token in (message.raw_text or ""):
                    print({"ok": True, "sent_id": sent_id, "reply_id": message.id, "token": token})
                    return
        print({"ok": False, "sent_id": sent_id, "token": token})
    finally:
        await client.disconnect()
        if tempdir is not None:
            tempdir.cleanup()


def build_parser() -> argparse.ArgumentParser:
    """Build the command line interface for reply-flow checks."""
    parser = argparse.ArgumentParser()
    parser.add_argument("--env-file", type=Path, default=DEFAULT_ENV_FILE)
    parser.add_argument("--entity", default=DEFAULT_ENTITY)
    parser.add_argument("--session-base", type=Path, default=DEFAULT_STATE_DIR / "session")
    parser.add_argument(
        "--shared-session",
        action="store_true",
        help="Use MCP's SQLite session directly instead of copying it first.",
    )
    subcommands = parser.add_subparsers(dest="command", required=True)

    list_cmd = subcommands.add_parser("list")
    list_cmd.add_argument("--limit", type=int, default=10)
    list_cmd.add_argument("--preview", type=int, default=240)
    list_cmd.set_defaults(func=list_messages)

    send_cmd = subcommands.add_parser("send-reply")
    send_cmd.add_argument("--reply-to", type=int, required=True)
    send_cmd.add_argument("--message", required=True)
    send_cmd.set_defaults(func=send_reply)

    smoke_cmd = subcommands.add_parser("smoke")
    smoke_cmd.add_argument("--reply-to", type=int, required=True)
    smoke_cmd.add_argument("--token")
    smoke_cmd.add_argument("--polls", type=int, default=10)
    smoke_cmd.add_argument("--interval", type=float, default=3.0)
    smoke_cmd.add_argument("--limit", type=int, default=8)
    smoke_cmd.set_defaults(func=smoke)

    return parser


def main() -> None:
    """Load credentials and run the selected Telegram reply helper."""
    parser = build_parser()
    args = parser.parse_args()
    load_env_file(args.env_file)
    if not os.environ.get("API_ID") or not os.environ.get("API_HASH"):
        raise SystemExit(f"API_ID/API_HASH not set; checked {args.env_file}")
    asyncio.run(args.func(args))


if __name__ == "__main__":
    main()
