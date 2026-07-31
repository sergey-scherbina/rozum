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

The bridge is a client of the meeting daemon. The daemon starts automatically,
but the named room must already exist (check with `rozum list`); a typo does not
create a room.

```bash
# Inject TELEGRAM_BOT_TOKEN from a secret manager or hidden prompt.
export TELEGRAM_CHAT_ID=...                 # numeric; normally negative for groups
# Required for groups; optional in a private chat:
export TELEGRAM_ALLOWED_USER_IDS=123456789  # comma-separated IDs, or explicit "*"
rozum telegram --room bright-finch --name telegram
```

Startup validates the token with `getMe`, requires `getWebhookInfo` to report no
active webhook, and validates the target with `getChat` before joining the room.
For a group or supergroup it also requires privacy mode to be disabled through
BotFather or the bot to be an administrator, so ordinary allowed messages can
actually reach `getUpdates`. In a private chat, omitting
`TELEGRAM_ALLOWED_USER_IDS` permits only that peer. Groups and supergroups
require an explicit allowlist; `*` deliberately trusts every sender in the
configured chat.

Allowed text appears in the room under the bridge participant (`telegram`, or
the chosen `--name`) with a stable sender ID in its body, for example
`[Alice #123456789]: hello`. Media, edits, and channel posts are ignored.

Telegram exposes one global `getUpdates` stream per bot, so use one dedicated
bot for each bridge. On the bot's first attachment, pending updates are skipped.
Later restarts resume the durable per-bot cursor at
`$XDG_STATE_HOME/rozum/messenger-cursors/telegram/<bot-user-id>.offset`. The
cursor is bound to the configured chat ID and committed only after the room
append succeeds: changing targets attaches from now instead of reusing another
chat's acknowledgement, while a crash in the narrow append-before-cursor window
can duplicate a message but cannot acknowledge an unappended message.

To obtain the chat ID, start a bot conversation and call Telegram's
`getUpdates` Bot API method after sending a message. Do not paste the
token into logs, chat, or a committed file, and remove any webhook before
starting the bridge.

## Discord bridge

```bash
# Inject DISCORD_BOT_TOKEN from a secret manager or hidden prompt.
export DISCORD_CHANNEL_ID=...
export DISCORD_ALLOWED_USER_IDS=123456789012345678  # required; CSV or explicit "*"
rozum discord --room bright-finch --name discord
```

The named room must already exist. Startup validates the bot identity, target
channel, and Gateway endpoint before joining it. Enable the privileged
**Message Content** intent for the application, and grant the bot
`VIEW_CHANNEL` plus `SEND_MESSAGES` (`SEND_MESSAGES_IN_THREADS` for a thread).
Use the bot token, not the application client secret.
The allowlist is always required; `*` explicitly trusts every non-bot sender in
the configured channel.

Only non-empty `MESSAGE_CREATE` text from the selected channel and allowed
human senders is accepted. Bot and webhook authors are ignored, and outbound
messages disable all Discord mention parsing. Gateway reconnects use a fresh
identify and wait at least five seconds; non-reconnectable authentication,
sharding, API-version, and intent close codes stop the bridge with an actionable
error instead of reconnecting forever.

Both messenger bridges export only room turns appended after they join, never
the room's existing history, and suppress their own room messages to avoid an
echo. If Telegram and Discord are both attached to one room, each bridge sees
the other's submissions as new room text, so allowed external messages are
mirrored between the two services. Long outbound text is split on UTF-8-safe
boundaries; HTTP rate limits honor `retry_after`. Transport errors are sanitized
so bot tokens are not included. Treat either bridge as an explicit trust
boundary: it exports new room text to an external service and lets allowed
external senders submit to the room.

Keep credentials in the bridge process environment only. Do not put tokens in
the repository, shell startup files, service definitions, or command-line
arguments; unset them after a manual run or inject them from a secret manager.

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
- **Telegram receive cursor**:
  `$XDG_STATE_HOME/rozum/messenger-cursors/telegram/<bot-user-id>.offset`.
  This is an update offset, not a transcript or credential.
- `$XDG_STATE_HOME` defaults to `~/.local/state` if unset.

---

## Local models: gateway, launch & sandbox

rozum can host local LLMs and route coding agents (Claude Code, Codex, opencode)
at them — no cloud, no API keys.

### The gateway

```bash
# Serve a model behind an OpenAI (/v1) + Anthropic (/) API on 127.0.0.1:
rozum gateway --model mlx-community:gpt-oss-20b-MXFP4-Q4
rozum gateway status        # model, port, pid, uptime, clients
rozum gateway stop
```

The two curated local models are **Qwen3.6-35B-A3B** (strongest local coder) and
**gpt-oss-20b** (OpenAI reasoning MoE). List them with `rozum models list`
(`--all` adds the extended fallback catalog); a model downloads on first use. Any
HuggingFace / MLX / GGUF spec works too.

### Launching an agent

`rozum launch <agent>` starts (or reuses) a gateway and runs the agent already
wired to it — Claude Code via Anthropic env, Codex via injected provider flags,
opencode via a written provider config:

```bash
rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude
rozum launch --model mlx-community:gpt-oss-20b-MXFP4-Q4 codex
rozum launch opencode                  # reuse a running gateway
rozum launch                           # interactive model picker
```

### The sandbox

Every launched agent runs **jailed** by default (macOS Seatbelt): file writes
are confined to its workspace (cwd) + toolchain caches, secrets are denied, and
only the local gateway is reachable off-box — with no per-action approval
prompts. Opt out per launch:

```bash
rozum launch --no-sandbox …                  # or ROZUM_SANDBOX=0
ROZUM_SANDBOX=/path/to/ws rozum launch …      # jail to an explicit workspace
ROZUM_SANDBOX_BACKEND=docker rozum launch …   # container backend (any OS)
```

### nadia — the coding agent that ships here

The agents above are third-party CLIs pointed at the gateway. `nadia`
(`crates/nadia`, `cargo install --path crates/nadia`) is one this repo owns: six
tools, a jailed workspace, and a model that has to *run* the build before it may
claim the task is done.

```bash
nadia run "add a --json flag to the CLI and a test for it"   # headless, in cwd
nadia                                                        # interactive
nadia serve                                                  # subagents over HTTP :8790
```

It reads `OPENAI_BASE_URL` / `ROZUM_GATEWAY_URL`, so `rozum launch nadia run …`
wires it up with no flags. Writes and commands ask first in interactive mode
(`/approve auto` to stop asking); `bash` runs confined with the network denied
unless `--allow-net`. Subagents are spawned and steered from the REPL
(`/spawn`, `/agents`, `/status`, `/tell`, `/stop`, `/kill`) or from the Telegram
bot with the same commands, gated by the chat's `write` + `shell` grants.

Full reference: [docs/nadia.md](docs/nadia.md).

---

## Local models in a room: the conference

A local model can join a meeting room as a **live participant** — it reads the
room and replies like any human or agent, with no moderator or turn-taking.

```bash
# join room "demo" as `gpt-oss`, replying when @mentioned:
rozum meetings participant \
  --model mlx-community:gpt-oss-20b-MXFP4-Q4 --room demo --as gpt-oss \
  --gateway-url http://127.0.0.1:8089/v1 \
  --persona "You are gpt-oss, a helpful assistant in our chat."
```

- `--reply-policy` — `mention` (default; reply only on `@handle`), `always`
  (reply to any human message), or `manual`.
- `--persona` / `--persona-file` — context (who it is, the topic) so it answers
  on-topic instead of generically.
- `--peer <handle>` — other models in the room, so `always` never loops
  model↔model.

### A whole conference in one command

`scripts/demo-conference.sh` brings up the model side of a conference — a gateway
+ participant per local model, each joined to one room with a persona — and
prints how humans (TUI/web) and a cloud Claude join:

```bash
LOCAL=qwen3.6 scripts/demo-conference.sh             # one model (safe on 36 GB)
LOCAL="qwen3.6 gpt-oss" scripts/demo-conference.sh   # both (needs ~32 GB)
ROOM=townhall scripts/demo-conference.sh
```

Then a human joins with `rozum meetings attach --room conference` (TUI) or
`rozum meetings web --room conference` (browser); a cloud Claude joins by running
Claude Code in the repo. `@mention` a model to talk to it. `Ctrl-C` stops
everything the script started.

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
| `TELEGRAM_ALLOWED_USER_IDS` | `rozum telegram`     | Sender IDs; required for groups, private peer by default |
| `DISCORD_BOT_TOKEN`     | `rozum discord`          | Bot token                                     |
| `DISCORD_CHANNEL_ID`    | `rozum discord`          | Numeric channel ID                            |
| `DISCORD_ALLOWED_USER_IDS` | `rozum discord`       | Required sender IDs; comma-separated or `*`   |
| `ROZUM_SANDBOX`         | `rozum launch`           | Agent jail: on (default) / `0` off / a path = workspace |
| `ROZUM_SANDBOX_BACKEND` | `rozum launch`           | `seatbelt` (macOS, default) or `docker`       |

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
