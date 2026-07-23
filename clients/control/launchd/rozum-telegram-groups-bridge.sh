#!/usr/bin/env bash
# Launch wrapper for the SECOND Telegram bot (@rozumia_bot), dedicated to GROUPS
# (com.rozum.telegram-groups). Reads its token from ~/.rozum/secrets/telegram-groups-token
# (mode 600). Its primary chat is the owner's DM with @rozumia_bot (chat id = the owner's
# user id, same value as telegram-chat-id) → room "rozumia"; groups are managed live from
# the bot (/addgroup) via its OWN registry TELEGRAM_REGISTRY=telegram-groups, so it never
# clashes with @Rozum_chat_bot. The pool com.rozum.assistant-groups runs one Qwen per room.
set -euo pipefail
SECRETS="$HOME/.rozum/secrets"
export TELEGRAM_BOT_TOKEN="$(cat "$SECRETS/telegram-groups-token")"
export TELEGRAM_CHAT_ID="$(cat "$SECRETS/telegram-chat-id")"
export TELEGRAM_OWNER_ID="$(cat "$SECRETS/telegram-chat-id")"
export TELEGRAM_REGISTRY="telegram-groups"
exec "$HOME/.cargo/bin/rozum-gateway" telegram --room rozumia --name rozumia
