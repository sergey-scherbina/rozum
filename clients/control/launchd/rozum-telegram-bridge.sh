#!/usr/bin/env bash
# Launch wrapper for the durable Telegram bridge (com.rozum.telegram). Reads the
# secrets from ~/.rozum/secrets/ (mode 600) so the bot token never lands in the
# launchd plist or the git repo. Bridges Telegram <-> the meeting-daemon room
# "assistant" (where a local Qwen participant answers). Requires: the meeting
# daemon (com.rozum.meeting-daemon), the room's Qwen participant (com.rozum.assistant),
# and the resident gateway on :8089.
set -euo pipefail
SECRETS="$HOME/.rozum/secrets"
export TELEGRAM_BOT_TOKEN="$(cat "$SECRETS/telegram-token")"
export TELEGRAM_CHAT_ID="$(cat "$SECRETS/telegram-chat-id")"
exec "$HOME/.cargo/bin/rozum-gateway" telegram --room assistant --name telegram
