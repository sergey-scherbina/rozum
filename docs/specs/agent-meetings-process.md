# Agent Meetings — Process Topology

## Overview

One `rozum` process = one meeting room. No registry daemon. Multiple rooms = multiple processes.

## Socket Layout

```
$XDG_RUNTIME_DIR/rozum/rooms/
  <name>.sock        ← primary socket for room <name>
  <old-name>.sock    ← symlink alias during rename TTL (~60 s)
```

Fallback when `XDG_RUNTIME_DIR` is unset: `~/.run/rozum/rooms/`.

`rozum list` scans this directory, pings each `.sock`, prints live rooms.

## Process Modes

### `rozum [--room NAME] [--topic TEXT] [--as NAME] [--moderator round-robin|manual]`

1. Generate or use the given room name.
2. Create `$ROOMS_DIR/<name>.sock` (error if already exists — room name taken by another live `rozum` process).
3. Start `tokio` runtime.
4. Initialise `Meeting` with human participant.
5. Spawn two tasks in parallel:
   - MCP server on the unix socket (`rmcp`)
   - TUI render loop (`ratatui` + `crossterm`)
6. Both share `Arc<Mutex<Meeting>>` + `broadcast<MeetingEvent>`.
7. On `[q]` / SIGINT / SIGTERM: send `{ended:"server-shutdown"}` to all pending `wait_my_turn`, flush transcript to `meetings/<name>-<ts>.md`, delete socket, exit.

### `rozum mcp-proxy`

Launched by each agent's MCP config as a stdio MCP server (the standard way Claude Code and Codex launch MCP servers). Each agent instance = one `mcp-proxy` process. The proxy:

1. Reads the MCP `initialize` from stdin and records `clientInfo.name`.
2. Exposes `rooms.list`, `rooms.join`, `meeting.*` to the agent.
3. `rooms.list`: scans rooms directory, pings each socket, returns list.
4. `rooms.join(name)`: resolves `<name>.sock`, opens a unix-socket MCP client connection to the room, sends `_join_internal` with the agent's `clientInfo.name`, stores as `RoomConn`.
5. `meeting.*`: forwards JSON-RPC to `RoomConn`, returns response.
6. `meeting.leave` / process exit: closes `RoomConn`, room removes participant.

### `rozum list`

1. Scan rooms directory.
2. For each `.sock` (skipping symlinks that are aliases): connect, call `room_info`, print.
3. Print summary table.

## Lifecycle

```
User: rozum --topic "X"
  → room "rapid-finch" created
  → TUI shown, human participant added

User (in codex session): rooms.join("rapid-finch")
  → proxy opens socket to rapid-finch
  → codex participant added to Meeting
  → MeetingEvent::ParticipantJoined broadcast

Moderator loop running in Meeting:
  → round-robin picks "codex"
  → codex's pending wait_my_turn returns {turn}
  → codex calls meeting.submit
  → MeetingEvent::TurnAdded broadcast → TUI renders turn
  → next speaker chosen

User in TUI: [q]
  → Meeting::end() called
  → all pending wait_my_turn return {ended:"server-shutdown"}
  → socket deleted
  → transcript saved
  → process exits
```

## Internal MCP Tool: `_join_internal`

Not in `tools/list` exposed to agents. Called by the proxy immediately after opening the room socket. Parameters: `{ client_info_name: string }`. The room uses this to set the participant's display name and registers it. Returns: `{ participant_id: string }`.

## Rename Flow

1. `Meeting::rename("new-name")` called (from TUI `/name <new>` command).
2. Create new socket at `<new-name>.sock`.
3. Rename old socket to `<old-name>.sock` as a symlink pointing to `<new-name>.sock`.
4. After 60 s: remove symlink.
5. `room_info` reflects new name immediately.
6. Already-connected agents: their `RoomConn` still uses the open file descriptor — unaffected by socket rename. New connections use new name. `rooms.list` shows new name.
