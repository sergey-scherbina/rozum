# Telegram Bridge

> Superseded for the active runtime by
> `docs/specs/messenger-bridges-daemon.md`. This file records the original
> legacy per-room prototype; its round-robin and `room_socket` behavior is not
> the current daemon contract.

## Overview

A `rozum telegram` subcommand that bridges a Telegram Bot chat to a rozum
meeting room. The bridge joins the room as an MCP participant named "telegram",
forwards room turns to the Telegram chat, and submits Telegram messages to the
room when the round-robin gives the bridge its turn.

## Interface

### CLI

```
rozum telegram --room <name> [--name <display-name>]
```

| Arg | Env | Default | Description |
|---|---|---|---|
| `--room` | — | required | Room name to join (must be running) |
| `--name` | — | `telegram` | Display name in the room |
| — | `TELEGRAM_BOT_TOKEN` | required | Bot API token |
| — | `TELEGRAM_CHAT_ID` | required | Target chat/group numeric ID |

The bridge exits with a non-zero status if the room is not found or the token
is invalid. All other errors are logged to stderr and the bridge retries or
continues.

### Runtime contract

- One bridge process = one room + one Telegram chat.
- Bridge participant name in the room is `--name` (default `telegram`); on name
  collision the room appends `#2`, `#3`, etc.
- Telegram messages are forwarded as `[FirstName]: text` (using Telegram's
  `from.first_name`; falls back to `from.username` or "user" if absent).
- Room turns from other participants are forwarded to Telegram as
  `[display_name]: content`.
- The bridge uses Telegram Bot API long-polling (`getUpdates`, 30 s timeout).
- The bridge only reads messages from the configured `TELEGRAM_CHAT_ID`; it
  silently ignores updates from other chats.

## Behavior

- [ ] `rozum telegram --room <name>` joins the named room or exits with error if not found.
- [ ] Bridge reads `TELEGRAM_BOT_TOKEN` and `TELEGRAM_CHAT_ID` from env or exits with error.
- [ ] Telegram messages in the target chat are queued and submitted to the room on the bridge's turn.
- [ ] If the queue is empty when it's the bridge's turn, the turn is skipped.
- [ ] Room turns from participants other than the bridge are forwarded to Telegram.
- [ ] The bridge ignores its own room turns to avoid echo loops.
- [ ] A batch of pending Telegram messages is joined with newlines into one room submit.
- [ ] The bridge exits cleanly when the room ends (`ended: true` from wait_my_turn).
- [ ] Telegram send errors are logged to stderr and do not crash the bridge.
- [ ] Room call errors (e.g. connection lost) are logged and the bridge exits with a non-zero status.

## Out of scope

- Webhook mode (long-polling only).
- Multi-chat or multi-room fan-out from one process.
- Rich Telegram message types (stickers, photos, etc.) — text only.
- Thread-aware routing.
- Telegram inline bot commands.
- Auto-reconnect on room disconnect.
- Sampling / wake-up (the bridge uses the pull model: wait_my_turn → submit).

## Design

```
┌─────────────────────────────────────┐
│  rozum telegram process             │
│                                     │
│  TelegramPoller task                │
│    GET /getUpdates (30 s poll)      │
│    filter by chat_id                │
│    push to: Arc<Mutex<VecDeque>>    │
│                                     │
│  RoomLoop task (main)               │
│    RoomConnection (unix socket)     │
│    loop:                            │
│      wait_my_turn(since_seq)        │
│        → transcript_delta           │
│            → send to Telegram       │
│        → your_turn                  │
│            → drain queue            │
│            → submit or skip        │
│        → ended → exit              │
└─────────────────────────────────────┘
```

Uses `RoomConnection` from `meeting::room_client` (raw JSON-RPC over unix
socket). The Telegram poller runs as a separate tokio task; it only touches the
shared queue. The room loop runs in the main task and owns the connection.

## Decisions

- **Pull model (wait_my_turn / skip)** — chosen because the bridge has no
  inference capability and cannot predict when a Telegram message will arrive;
  skipping an empty turn is cheaper than blocking the round-robin.
  Rejected: sampling / push — would need Anthropic API fallback wired in, adds
  complexity for a dumb relay.
- **Long-polling over webhooks** — chosen to avoid requiring a public HTTPS
  endpoint; a local bridge process can start with just a bot token.
  Rejected: webhooks — simpler server-side but requires TLS termination.
- **`reqwest` for Telegram API** — already a crate dependency; no new deps needed.
  Rejected: `teloxide` — would add a large transitive dep tree.
- **One process per room** — matches the rozum architecture (one process = one room).
  Rejected: multi-room multiplexer — adds fan-out complexity with no clear benefit.
