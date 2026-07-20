# Messenger Bridges

> The original document below describes the legacy per-room bridge prototype.
> The active Telegram/Discord daemon contract, security policy, and migration
> are normative in `docs/specs/messenger-bridges-daemon.md`. The legacy
> round-robin/`skip` design is retained here only as design history.

## Overview

Each messenger bridge is a standalone `rozum <messenger>` subcommand (or
separate binary) that connects a third-party chat platform to a rozum meeting
room. All bridges share the same architecture: join the room as an MCP
participant, forward room turns to the messenger, and submit messenger messages
to the room on the bridge's round-robin turn.

## Common Interface

Every bridge implements the same two operations against a running rozum room:

```
IncomingMessage { sender_name: String, text: String }
outgoing: String  // formatted as "[display_name]: content" from the room
```

The room-side contract is identical for all bridges:
1. Connect to room unix socket.
2. Call `_join_internal` with the bridge's display name.
3. Loop: `wait_my_turn` → forward `transcript_delta` to messenger → if `your_turn` → drain inbox → `submit` or `skip`.
4. Exit on `ended`.

## Supported Platforms

### Telegram ✅ (implemented)

| Item | Detail |
|---|---|
| API | Telegram Bot API (REST + long-polling) |
| Auth | `TELEGRAM_BOT_TOKEN` (bot token from @BotFather) |
| Targeting | `TELEGRAM_CHAT_ID` (numeric chat or group ID) |
| Setup effort | Low — no approval, no infra |
| Incoming | `getUpdates` long-polling (30 s) |
| Outgoing | `sendMessage` |
| Notes | Works with private chats, groups, supergroups. Bot must be added to the group to read messages. |

### Slack ⬜ (planned)

| Item | Detail |
|---|---|
| API | Slack Web API + Socket Mode |
| Auth | `SLACK_BOT_TOKEN` (xoxb-…), `SLACK_APP_TOKEN` (xapp-…) |
| Targeting | `SLACK_CHANNEL_ID` |
| Setup effort | Medium — need Slack App in workspace, workspace admin |
| Incoming | Socket Mode WebSocket (event: `message` in channel) |
| Outgoing | `chat.postMessage` |
| Notes | Socket Mode avoids public HTTPS requirement. Bot needs `chat:write` and `channels:history` (or `groups:history` for private channels). |

**CLI:** `rozum slack --room <name> [--name <display-name>]`

### Discord ⬜ (planned)

| Item | Detail |
|---|---|
| API | Discord Gateway (WebSocket) + REST |
| Auth | `DISCORD_BOT_TOKEN` |
| Targeting | `DISCORD_CHANNEL_ID` |
| Setup effort | Low — create bot in Discord Developer Portal, invite to server |
| Incoming | Gateway `MESSAGE_CREATE` event |
| Outgoing | `POST /channels/{id}/messages` |
| Notes | No approval required. Bot needs `Read Messages` + `Send Messages` permissions. |

**CLI:** `rozum discord --room <name> [--name <display-name>]`

### Microsoft Teams ⬜ (planned)

| Item | Detail |
|---|---|
| API | Bot Framework SDK / Teams REST API |
| Auth | Azure App Registration (`TEAMS_APP_ID`, `TEAMS_APP_PASSWORD`) |
| Targeting | `TEAMS_CONVERSATION_ID` (team + channel or 1:1 conversation ref) |
| Setup effort | High — Azure tenant, app registration, Teams admin approval |
| Incoming | Bot Framework webhook (requires HTTPS endpoint) or Polling via Graph API |
| Outgoing | Bot Framework `send_activity` or Graph API `POST /chats/{id}/messages` |
| Notes | Polling via Microsoft Graph API avoids the HTTPS webhook requirement but has rate limits. Bot Framework webhook requires TLS termination (e.g. ngrok for dev). |

**CLI:** `rozum teams --room <name> [--name <display-name>]`

### WhatsApp ⬜ (planned)

| Item | Detail |
|---|---|
| API | Meta WhatsApp Business Cloud API |
| Auth | `WHATSAPP_TOKEN` (System User access token), `WHATSAPP_PHONE_ID` |
| Targeting | `WHATSAPP_TO` (recipient phone number in E.164 format) |
| Setup effort | High — Meta Business account, phone number registration, webhook verification |
| Incoming | Webhook `POST` (requires HTTPS, Meta verifies endpoint at setup) |
| Outgoing | `POST /v17.0/{phone-id}/messages` |
| Notes | Free tier: 1,000 conversations/month. Requires Meta App Review for production scale. Webhook requires a public HTTPS endpoint. |

**CLI:** `rozum whatsapp --room <name> [--name <display-name>]`

## Not Possible

| Platform | Reason |
|---|---|
| **Google Messages (RCS)** | Google Business Messages was deprecated and shut down in July 2024. No replacement consumer API exists. |
| **iMessage** | Apple provides no public API for iMessage. Third-party access requires physical device automation (fragile, unsupported). |
| **Instagram DM** | Instagram Messaging API is restricted to verified businesses and requires Meta App Review for DM access. Effectively unavailable for self-hosted tools. |
| **Signal** | No official API. Third-party libraries (signal-cli) use reverse-engineered protocols and may break without notice. |

## Shared Architecture

All bridges share the same room-side code. The platform-specific part is a
trait:

```rust
trait MessengerBridge: Send {
    async fn next_message(&mut self) -> Option<IncomingMessage>;
    async fn send(&mut self, text: &str) -> Result<()>;
}
```

The generic room loop in `src/bridge/room_loop.rs` drives any `MessengerBridge`
implementation. Each platform module implements the trait and adds its own
`run_<platform>` entry point.

## Out of scope

- LLM-backed response generation in bridges.
- Bridging between two messengers without a rozum room in the middle.
- Message history sync on bridge startup.
- Rich message types (images, files, reactions) — text only.
- End-to-end encryption between messengers.

## Decisions

- **One bridge = one room = one chat** — chosen to match the rozum one-process-
  per-room model; fan-out adds complexity with no clear benefit for the
  primary use case. Rejected: multi-chat multiplexer per bridge process.
- **Pull model (skip on empty turn)** — all bridges use `wait_my_turn` / skip
  rather than sampling, because bridges are dumb relays with no inference.
  Rejected: sampling fallback — bridges don't generate responses.
- **Separate subcommands per platform** — chosen over a plugin system to keep
  the binary self-contained and avoid dynamic linking. Rejected: plugin/dylib
  architecture.
