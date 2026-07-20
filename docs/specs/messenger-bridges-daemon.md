# Daemon-backed Telegram and Discord bridges

## Overview

`rozum telegram` and `rozum discord` connect one external chat to one room in
the disk-backed meeting daemon. They are thin, engine-free clients of
`meeting.sock`: incoming messenger text is submitted immediately through the
daemon and newly appended room turns are read from the room's canonical day
files and forwarded to the messenger. This replaces the obsolete per-room
socket/round-robin bridge path without changing the public commands.

The bridge is an explicit trust boundary. Attaching it exports new room text to
the configured external chat and lets an allowed external sender inject text
into the room. Startup therefore validates credentials and the target, and
inbound sender access is deny-by-default outside a private Telegram chat.

## Interface

### CLI and routing

```text
rozum telegram --room <name> [--name <display-name>]
rozum discord  --room <name> [--name <display-name>]
```

The umbrella `rozum` dispatcher routes both commands to the engine-free
`rozum-meet` binary. `rozum-gateway telegram|discord` remains compatible during
the binary-split migration, but the normal path does not link or rebuild model
engines.

The meeting daemon is auto-started when absent. `--room` names an existing
daemon room; a typo is an error and never creates a new room implicitly.

### Telegram configuration

| Environment variable | Required | Meaning |
|---|---:|---|
| `TELEGRAM_BOT_TOKEN` | yes | Bot token issued by BotFather. |
| `TELEGRAM_CHAT_ID` | yes | Numeric private-chat, group, or supergroup ID. |
| `TELEGRAM_ALLOWED_USER_IDS` | for groups | Comma-separated numeric sender IDs, or `*` to explicitly trust every sender in the target chat. In a private chat, omission restricts input to that chat's user ID. |

Startup calls Telegram `getMe` and `getChat`; invalid credentials, an invalid
chat ID, or a group without an explicit allowlist fail before the bridge joins
the room. Bot tokens must never appear in returned/logged transport errors.

### Discord configuration

| Environment variable | Required | Meaning |
|---|---:|---|
| `DISCORD_BOT_TOKEN` | yes | Discord bot token (not the application client secret). |
| `DISCORD_CHANNEL_ID` | yes | Target channel/thread snowflake. |
| `DISCORD_ALLOWED_USER_IDS` | yes | Comma-separated sender snowflakes, or `*` to explicitly trust every non-bot sender in the target channel. |

Startup validates the bot token and target channel through Discord REST before
joining the room. The application must enable the privileged Message Content
intent and grant the bot `VIEW_CHANNEL` and `SEND_MESSAGES` (plus
`SEND_MESSAGES_IN_THREADS` for a thread).

### Room-side contract

- The bridge connects to `meeting_sock()` and selects the named room with
  `rooms.join(kind="bridge")` using one stable session token for its action and
  poll connections.
- Incoming allowed text is submitted immediately as
  `[<messenger display name>]: <text>`; there is no turn, queue-drain, or skip
  phase.
- A dedicated second daemon connection holds `meeting.wait_my_turn`; content is
  then read through the canonical store API. Incoming submit is never blocked
  behind the long-poll.
- The initial cursor is the current high-water. Existing room history is not
  replayed to a newly started bridge; only turns appended after it joins are
  exported.
- Stored turns authored by the bridge's own participant ID are not sent back to
  the messenger.
- A daemon disconnect ends the bridge with a non-zero result so a supervisor can
  restart it. Platform receive loops reconnect with bounded exponential backoff.

### Platform delivery contract

- Text only. Telegram media/edits/channel posts and Discord non-message events
  are ignored.
- Telegram accepts updates only from `TELEGRAM_CHAT_ID` and the resolved sender
  allowlist.
- Discord accepts `MESSAGE_CREATE` only from `DISCORD_CHANNEL_ID` and the
  resolved sender allowlist. Bot-authored and webhook-authored messages are
  always ignored, preventing REST-send echo loops.
- Outbound room text is prefixed `[display_name]: ` and split on UTF-8-safe
  boundaries using conservative per-platform limits (4000 UTF-16 units for
  Telegram; 1900 for Discord) with newline/whitespace preference.
- Discord sends `allowed_mentions.parse=[]`; room text can never trigger user,
  role, or `@everyone` mentions.
- HTTP 429 responses honor `retry_after`; transient receive/delivery errors use
  bounded backoff. A failed outbound chunk is logged without secrets and does
  not crash the room bridge.
- Discord Gateway heartbeats carry the latest dispatch sequence, server
  heartbeat requests are answered immediately, missing ACKs reconnect, and
  `RECONNECT`/`INVALID_SESSION` leave the current session for a fresh identify.

## Behavior

- [ ] Both public commands use `rozum-meet`, auto-start the daemon, join an
      existing named room as `kind=bridge`, and do not require model features.
- [ ] Existing room history is not exported on bridge startup; a newly appended
      non-self turn is exported exactly once.
- [ ] An allowed Telegram/Discord text message lands in the daemon room exactly
      once without waiting for the room poll.
- [ ] Wrong chat/channel, unauthorized sender, bot/webhook author, empty text,
      and malformed platform payloads are ignored.
- [ ] Missing/invalid credentials, IDs, allowlists, targets, or Discord intent
      setup fail with actionable errors that contain no bot token.
- [ ] Long outbound messages are split without invalid UTF-8; Discord mentions
      remain disabled; rate-limit retry is bounded.
- [ ] Discord uses the latest sequence in heartbeats, acknowledges server
      heartbeat requests, and reconnects on a missing ACK or reconnect opcode.
- [ ] Platform receive errors back off; a daemon/store error exits non-zero for
      supervisor recovery.
- [ ] Unit/integration tests cover the shared daemon adapter, replay/self
      suppression, allowlists/parsers, chunking, sanitized errors, Discord
      Gateway actions, and thin CLI routing without live credentials.

## Out of scope

- Creating Telegram/Discord accounts, applications, servers, groups, or
  channels on behalf of the operator.
- Persisting bot tokens in the repository, shell startup files, plist files, or
  systemd unit files.
- Managed launchd/systemd bridge installation and secret lifecycle; live smoke
  runs use process-scoped credentials, and durable supervision is a separate
  follow-up after the transport is proven.
- Mapping each Telegram/Discord account to the local human Principal. In this
  phase the bridge is the daemon Principal and preserves the external sender in
  message content.
- History synchronization, attachments, edits, reactions, threads/topics,
  slash commands, rich embeds, or messenger-to-messenger fan-out.
- Migrating the separate legacy `rozum web` bridge.

## Design

The shared room adapter extends `MeetingClient` with an explicit client kind and
participant ID. Its default constructors remain human/TUI clients; a bridge
constructor joins with `kind="bridge"`. The existing two-connection
`spawn_poll` design is reused so disk cursors, room lookup, and the daemon's
single-writer/direct-read invariant stay in one implementation.

Each platform receive task produces normalized `IncomingMessage` values over an
`mpsc` channel. The platform bridge's main task owns the action client and
selects between that inbox and the daemon poll stream. This removes the legacy
`Arc<Mutex<RoomConnection>>` held across a 35-second room long-poll.

Platform protocol code remains small and dependency-light (`reqwest` plus the
existing `tokio-tungstenite`), with pure parsers/actions extracted for offline
tests. No framework SDK is added.

## Decisions

- **Daemon client, not a compatibility per-room socket.** The daemon and its
  canonical store are the source of truth. Rejected: keeping a hidden legacy
  room alive solely for messenger bridges.
- **Deny-by-default sender policy.** External text can reach coding agents, so a
  target channel alone is not authorization. Rejected: silently trusting every
  member. `*` remains an explicit operator escape hatch.
- **No startup replay.** Attaching a bridge must not exfiltrate or duplicate old
  room history. Rejected: cursor zero / full transcript replay.
- **Thin-binary routing.** Messenger transport has no engine dependency.
  Rejected: routing routine chat I/O through `rozum-gateway`.
- **Fresh Discord identify after reconnect for this phase.** Correct sequence,
  ACK, and reconnect behavior are required; full session Resume state is not
  required for a single small bridge and can be added if reconnect frequency
  makes identify limits material.

## Results

To be filled after verification.
