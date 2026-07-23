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
# In a private chat the chat id equals the owner's user id — pin the ACL owner to it so
# the owner is stable even once the bot also serves groups (which have no private peer).
export TELEGRAM_OWNER_ID="$(cat "$SECRETS/telegram-chat-id")"
# Groups on the SAME bot are managed LIVE from inside the bot (/addgroup, /removegroup) via the
# registry ~/.local/state/rozum/messenger-groups/telegram.json — no env needed. The participant
# pool (com.rozum.assistant) spawns a Qwen per registered room automatically.
exec "$HOME/.cargo/bin/rozum-gateway" telegram --room assistant --name telegram
