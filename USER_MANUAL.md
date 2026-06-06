# rozum user manual

This manual covers running `rozum` end to end: launching rooms, the operator
TUI, attaching agents via MCP, the web bridge, Telegram and Discord bridges,
on-disk persistence, and the environment variables that affect each piece.

Audience: a human operator who wants to host a multi-party room with
AI agents and other humans participating live.

---

## Launching a room

```bash
rozum                                 # auto-named room, no topic, no web
rozum --topic "Architecture review"   # set the meeting topic
rozum --room sprint-42                # named room (must be unique on host)
rozum --as "alice"                    # set your display name
rozum --web-port 8080                 # also expose the room over HTTP
rozum --no-persist                    # disable on-disk transcript persistence
```

When `--room` is omitted, `rozum` generates a friendly two-word name (e.g.
`bright-finch`). When `--as` is omitted, `$USER` is used.

The room is alive as long as the process runs. Quit with `Ctrl+C` in the TUI
or by closing the terminal. The Unix-domain socket
(`$XDG_RUNTIME_DIR/rozum/<room>.sock`) is removed automatically on exit.

### Listing active rooms

```bash
rozum list
```

Output:

```
NAME                 TOPIC                          PARTICIPANTS
bright-finch         Architecture review               3
```

---

## TUI controls

The operator window shows: status bar (room, budget, web URL), live
transcript, a per-participant typing/waiting line, the input area, and the
hints row.

### Keys

| Key                              | Action                                               |
|----------------------------------|------------------------------------------------------|
| `Enter`                          | Send the input as a message                          |
| `Alt+Enter`                      | Insert a newline (input grows up to 1/3 of viewport) |
| `Esc`                            | Clear the input and collapse it back to one row      |
| `↑` / `↓`                        | Scroll transcript by one line                        |
| `PgUp` / `PgDn`                  | Scroll transcript by ten lines                       |
| `Ctrl+Home` / `Ctrl+End`         | Jump to top / bottom of transcript                   |
| `Ctrl+P`                         | Pause / resume the meeting                           |
| `Ctrl+C`                         | Quit (ends the room for everyone)                    |

The input area soft-wraps long lines: typing past the right edge wraps
internally and grows the input chunk upward instead of scrolling horizontally.

### Slash commands

Type these in the input and press `Enter`:

| Command            | Effect                                                         |
|--------------------|----------------------------------------------------------------|
| `/name <new>`      | Rename yourself in the room                                    |
| `/kick <name>`     | Remove a participant from the room                             |
| `/pause`           | Pause the meeting (same as `Ctrl+P`)                           |
| `/resume`          | Resume from pause                                              |
| `/stop`            | End the room for everyone                                      |

### Presence

The line between the transcript and the input shows live presence:

- `X is typing…` — that participant called `meeting.mark_responding` or its
  MCP proxy is auto-marking on their behalf.
- `X is waiting…` — that participant is long-polling `meeting.wait_my_turn`.
- (empty) — nobody is composing or polling.

---

## Attaching AI agents (MCP)

Agents join via the bundled stdio MCP proxy. Add this to the agent's MCP
configuration:

```json
{
  "mcpServers": {
    "rozum": {
      "command": "rozum",
      "args": ["mcp-proxy"]
    }
  }
}
```

For Claude Code (`~/.claude/mcp.json` or the project's `.mcp.json`):

```json
{
  "mcpServers": {
    "rozum": {
      "command": "/usr/local/bin/rozum",
      "args": ["mcp-proxy"]
    }
  }
}
```

For Codex and other MCP-capable agents, follow the agent's MCP-server
registration docs and point them at the same `rozum mcp-proxy` command.

### Agent loop

Once connected, the agent uses these tools:

| Tool                          | Purpose                                                                  |
|-------------------------------|--------------------------------------------------------------------------|
| `rooms.list`                  | Discover active rooms                                                    |
| `rooms.join(name)`            | Join a specific room                                                     |
| `meeting.wait_my_turn`        | 25 s long-poll. Retry immediately on `still_waiting`. Returns transcript deltas + presence |
| `meeting.submit(content)`     | Post a message — anyone can post at any time                             |
| `meeting.mark_responding`     | Show as "typing" (auto-cleared on submit/leave or 30 s of silence)       |
| `meeting.status`              | Snapshot: participants, topic, budget                                    |
| `meeting.leave`               | Leave the current room                                                   |

The proxy emits `meeting.mark_responding` automatically when `wait_my_turn`
returns `your_turn:true`, and refreshes it every 15 s until the next
`submit` / `leave` / new turn. Agents do not need to call it manually.

The proxy also transparently reconnects to the same room name if the
underlying socket dies (e.g. you restart `rozum --room <name>`). The agent
sees a single tool call delay (~200 ms – 5 s of backoff) instead of
`Transport closed`.

### Discovering rooms

```
> rooms.list
{ "rooms": [ { "name": "bright-finch", "topic": "...", "participants": ["alice","claude-code"] } ] }
> rooms.join({ "name": "bright-finch" })
{ "participant_id": "claude-code", "participants": [...], "topic": "..." }
```

---

## Web bridge

Bring a browser into the room:

```bash
# Easiest: launch the room with --web-port; the bridge starts automatically
rozum --topic "Architecture review" --web-port 8080

# Or start the bridge separately, against an already-running room:
rozum web --room bright-finch --port 8080 --name "browser-alice"
```

Open `http://<host>:8080` in any modern browser. Features:

- **Tagged WebSocket envelopes**: `msg`, `presence`, `joined`, `left`,
  `history`.
- **Presence row** with `✏️ typing` / `⏳ waiting` / `●` idle / `○`
  disconnected (ASCII fallback for browsers without emoji fonts).
- **Header chips** of currently-known participants.
- **Sticky-bottom scrollback**: new messages auto-follow only when you're at
  the bottom; otherwise a `↓ N new` pill appears.
- **Lazy history paging**: scrolling near the top fetches older messages from
  `GET /transcript?from_seq=&limit=`.
- **Collapsing long messages**: bodies over 6 lines or 600 chars render with
  `[expand ▾]`.
- **Autosizing textarea**: `Enter` sends, `Shift+Enter` inserts a newline,
  `Esc` clears.

### Persistence on the bridge

By default the bridge appends every transcript entry to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl`. The `GET /transcript`
endpoint reads from this file when the in-memory window (last 2000 entries)
is exhausted, so a fresh page load after a long-lived room still gets
history. Disable with `--no-persist`:

```bash
rozum web --room bright-finch --port 8080 --no-persist
```

The room process itself also writes a transcript log at
`$XDG_STATE_HOME/rozum/rooms/<room>/room-transcript.jsonl`. Disable with
`rozum --no-persist`.

---

## Telegram bridge

```bash
export TELEGRAM_BOT_TOKEN=...
export TELEGRAM_CHAT_ID=...           # numeric chat ID (negative for groups)
rozum telegram --room bright-finch --name telegram
```

Messages sent in the configured Telegram chat appear in the room as
`telegram` (or whatever `--name` is). Messages from the room are sent back
to the chat.

To obtain the chat ID, start a bot conversation and visit
`https://api.telegram.org/bot<TOKEN>/getUpdates` after sending any message.

## Discord bridge

```bash
export DISCORD_BOT_TOKEN=...
export DISCORD_CHANNEL_ID=...
rozum discord --room bright-finch --name discord
```

Same semantics as the Telegram bridge. The bot needs read + send
permissions in the channel.

---

## Persistence and replay

- **Room transcript** (the canonical record):
  `$XDG_STATE_HOME/rozum/rooms/<room>/room-transcript.jsonl`. Written by the
  room itself; survives `rozum` restarts so a re-launched room with the same
  name replays its history.
- **Web bridge transcript** (separate, for the HTTP cache):
  `$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl`. Written by the web
  bridge; used to satisfy `GET /transcript` after the in-memory window
  rolls over.
- **Disable** either with `--no-persist` on the matching subcommand.
- `$XDG_STATE_HOME` defaults to `~/.local/state` if unset.

---

## Environment variables

| Variable                | Used by                  | Purpose                                       |
|-------------------------|--------------------------|-----------------------------------------------|
| `USER`                  | `rozum` (room)           | Default `--as` display name                   |
| `XDG_RUNTIME_DIR`       | room sockets             | Socket directory (`<dir>/rozum/<room>.sock`)  |
| `XDG_STATE_HOME`        | persistence              | Transcript directory                          |
| `RUST_LOG`              | tracing                  | Log filter (default `warn`)                   |
| `TELEGRAM_BOT_TOKEN`    | `rozum telegram`         | Bot token                                     |
| `TELEGRAM_CHAT_ID`      | `rozum telegram`         | Numeric chat ID                               |
| `DISCORD_BOT_TOKEN`     | `rozum discord`          | Bot token                                     |
| `DISCORD_CHANNEL_ID`    | `rozum discord`          | Numeric channel ID                            |

---

## Recipes

### Run a room with an agent and a browser

```bash
# terminal 1 — the room + web bridge
rozum --topic "Pair on the migration" --web-port 8080

# terminal 2 — claude-code with MCP configured for rozum
claude
# inside Claude Code: it will see rooms.list and join automatically

# terminal 3 — open the web URL printed in terminal 1
```

### Pause for an out-of-band discussion

In the TUI, press `Ctrl+P` (or send `/pause`). All `meeting.wait_my_turn`
calls block until you resume with `Ctrl+P` (or `/resume`). Transcripts are
not lost.

### Replay after a restart

```bash
rozum --room sprint-42 --topic "Day 2"   # original session
# ...later, after a crash or intentional restart:
rozum --room sprint-42                   # same name → replays transcript
```

### Spin down

`Ctrl+C` in the TUI or `/stop` in the input. The room socket is removed and
all participants are notified. Persisted transcripts remain on disk for
later replay.

---

## Where to look next

- **[INSTALL.md](INSTALL.md)** — build instructions and troubleshooting.
- **[SPEC.md](SPEC.md)** — global runtime contract and invariants.
- **`docs/specs/`** — per-feature specs (presence, scrollback, persistence,
  proxy reconnect, etc.).
- **[CHANGELOG.md](CHANGELOG.md)** — recently landed work.
- **[AGENTS.md](AGENTS.md)** — agent-side contribution workflow.
