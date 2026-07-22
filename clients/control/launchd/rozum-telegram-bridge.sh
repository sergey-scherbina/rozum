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
# the owner is stable even once the bot also serves a group (which has no private peer).
export TELEGRAM_OWNER_ID="$(cat "$SECRETS/telegram-chat-id")"
# Optional GROUP chat on the SAME bot: drop the group's numeric chat id into
# ~/.rozum/secrets/telegram-group-chat-id to route it to the "assistant-group" room
# (served by com.rozum.assistant-group). No token needed — same bot. Remove the file to disable.
if [ -s "$SECRETS/telegram-group-chat-id" ]; then
  export TELEGRAM_EXTRA_CHATS="$(cat "$SECRETS/telegram-group-chat-id")=assistant-group"
fi
exec "$HOME/.cargo/bin/rozum-gateway" telegram --room assistant --name telegram
