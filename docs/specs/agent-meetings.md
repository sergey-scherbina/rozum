# Agent Meetings

## Overview

`rozum` is a meeting-room agent. One running process = one named chat room. Other agents (Claude Code, Codex) join the room from their live sessions via MCP. The human who launched `rozum` participates directly through the TUI as a first-class participant.

This replaces the `model-group-chat` backlog item, which was scoped to internal backends only. Agent Meetings extends the concept to full external CLI agents with persistent context.

## Interface

- CLI entry: `rozum [--room NAME] [--topic TEXT] [--as NAME] [--moderator round-robin|manual]`
- Discovery: `rozum list` — prints active rooms
- Agent shim: `rozum mcp-proxy` — stdio MCP server added to Claude Code / Codex MCP config
- Room naming: auto-generated kebab adjective+noun (e.g. `rapid-finch`); changeable via `/name <new>` in TUI
- Room address: `$XDG_RUNTIME_DIR/rozum/rooms/<name>.sock` (fallback `~/.run/rozum/rooms/<name>.sock`)

## Behavior

- [ ] `rozum` launches a TUI + MCP server; the human is automatically added as a participant (display name from `--as`, default `$USER`)
- [ ] `rozum --room NAME` uses the given room name; without `--room` an auto-generated name is used
- [ ] The room name is shown prominently in the TUI header and changeable with `/name <new>`
- [ ] `rozum list` prints name, topic, participant count for each active room
- [ ] `rozum mcp-proxy` (stdio) implements `rooms.list` and `rooms.join(name)`, then forwards `meeting.*` tools to the chosen room socket
- [ ] `rooms.list` returns all rooms found by scanning the sockets directory (ping-alive check included)
- [ ] `rooms.join(name)` opens a connection to the named room and returns `{topic, participants, moderator_mode}`
- [ ] `meeting.wait_my_turn` long-polls (up to 25 s) and returns `{turn}`, `{still_waiting}`, or `{ended}`
- [ ] `meeting.submit(content)` adds a turn to the transcript and signals the moderator
- [ ] `meeting.leave` removes the participant; if <2 remain the room phase becomes `Ended`
- [ ] `meeting.status` returns a snapshot of the room state
- [ ] Round-robin moderator cycles through all participants including human; human turn waits up to `human_turn_timeout` (default 90 s) for response start, then skips if no response started
- [ ] Manual moderator waits for the operator to choose next speaker via TUI
- [ ] `/next <participant>` chooses the next speaker in manual mode
- [ ] Moderator mode is switchable at runtime via TUI hotkey without stopping the meeting
- [ ] Human can type a message at any time (`[t]` key → typed turn goes into the transcript)
- [ ] Human can interject (`[i]` key) to insert a turn before the next scheduled speaker
- [ ] `[space]` pauses the meeting; `[s]` ends it (process stays alive); `[q]` / Ctrl+C ends the meeting and exits the process
- [ ] On graceful shutdown all pending `wait_my_turn` calls return `{ended: "server-shutdown"}`
- [ ] Budget: soft per-turn token warning (`max_tokens_per_turn`); hard total-chars limit (`max_total_chars`) terminates the meeting
- [ ] After `[q]` the transcript is written to `meetings/<room-name>-<ts>.md`
- [ ] Renaming a room creates a symlink alias for the old name (TTL ~60 s) so connected agents are not disrupted
- [ ] The user-facing CLI exposes only meeting launch options, `list`, and `mcp-proxy`

## Out of Scope

- HTTP / streamable-http MCP transport
- Persistent room state across process restarts
- Multi-room registry in one process
- Authentication / multi-user
- Token-level or chunked streaming of individual turns (turns are atomic)
- Web UI or remote meetings
- LLM-backed or "smart" moderator policy
- Local model inference as a default meeting dependency

## Design

Each `rozum` process owns a single `Meeting` value behind `Arc<Mutex<Meeting>>`. The MCP server and the TUI share this reference directly — no control-plane protocol needed. A `tokio::sync::broadcast<MeetingEvent>` channel propagates state changes to the TUI renderer.

The `rozum mcp-proxy` shim is a separate binary mode. Each MCP session (one per agent) carries an `Option<RoomConn>` — nil until `rooms.join` is called. After joining, `meeting.*` calls are forwarded as JSON-RPC to the room socket. The shim passes the agent's `clientInfo.name` as the participant identity when joining.

MCP uses unix sockets so no ports are required and no additional authentication is needed for a single-user local setup.

## Decisions

- **One process = one room** — matches the mental model of `claude` (each invocation is one context). Avoids the complexity of a multi-room registry daemon.
- **Discovery via `rooms.list`, not per-room MCP config** — agents see a dynamic list of rooms; the user doesn't need to reconfigure every time a new room is created. The shim is installed once.
- **stdio shim over HTTP MCP** — both Claude Code and Codex reliably support stdio MCP servers; HTTP streamable transport compatibility with Codex was not confirmed at design time.
- **Long-poll (25 s) instead of indefinite hold** — guards against MCP client timeouts; agent simply retries on `still_waiting`.
- **Human turn timeout (90 s) with skip** — prevents the meeting from deadlocking if the human is AFK. The timeout stops after the human starts typing a reply; skipped turns are logged.
- **Round-robin and manual first** — chosen because the smart moderator's speaker-selection policy is intentionally unresolved. Rejected: exposing local-LLM as a normal mode, because it would imply product semantics that have not been designed yet.

## Results

Implemented in `src/meeting/` and `src/tui/`. Verified with `cargo fmt --check`, `cargo test` (12 tests pass), `cargo build --release`. Smoke tests with real agents marked `#[ignore]` per project convention.
