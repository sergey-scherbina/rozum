# Agent Meetings — MCP Protocol

## Overview

Two-stage MCP API. Stage 1 (discovery): `rooms.*` implemented by `rozum mcp-proxy`. Stage 2 (meeting): `meeting.*` implemented by the room process and forwarded by the proxy once a room is joined.

## Tool Schemas

### `rooms.list`

**Implemented by:** proxy  
**Parameters:** none  
**Returns:** `{ rooms: [ { name, topic, participants: [string], moderator_mode } ] }`

Scans `$XDG_RUNTIME_DIR/rozum/rooms/` for `.sock` files, does a short MCP ping + `room_info` request to each, collects live rooms. Dead sockets (no response within 2 s) are omitted.

### `rooms.join`

**Implemented by:** proxy  
**Parameters:** `{ name: string }`  
**Returns:** `{ topic, participants: [string], moderator_mode, budget }`  
**Errors:** `RoomNotFound` if no socket for name; `AlreadyJoined` if session already has a `RoomConn`.

Opens a unix-socket MCP connection to the room. Stores the connection as `RoomConn` in per-session proxy state. Sends the agent's `clientInfo.name` (from the original MCP `initialize`) to the room as the participant display name. On name collision the room appends `#2`, `#3`, etc.

The room connection is bidirectional. If the original agent client advertises
`sampling/createMessage`, the proxy advertises sampling to the room and forwards
room sampling requests back to the original agent client.

### `meeting.wait_my_turn`

**Implemented by:** room (forwarded by proxy)  
**Parameters:** `{ since_seq?: number }`  
**Returns (one of):**
- `{ still_waiting: true, seq: number, active_turn }` — not this participant's turn yet; call again immediately
- `{ turn: { seq: number, transcript_delta: [Turn], your_turn: boolean, turn_id?: number, active_turn, instruction: string } }` — it's this participant's turn or new transcript messages arrived; `transcript_delta` contains all turns since `since_seq`; `turn_id` is present only for the active speaker; `instruction` is the system prompt insert for this speaker
- `{ ended: true, reason: string }` — meeting is over

**Timeout:** 25 s wall-clock. After 25 s without a turn or ended signal the tool returns `still_waiting`. The agent is instructed in the tool description to immediately call again.

### `meeting.submit`

**Implemented by:** room (forwarded)  
**Parameters:** `{ content: string, turn_id?: number }`  
**Returns:** `{ seq: number }`  

Adds the turn to the transcript. Unblocks any waiting `wait_my_turn` calls for participants who should now receive this turn in their `transcript_delta`. Signals the moderator that this participant's turn is done.

**Errors:** `NotYourTurn` if called when it is not this participant's turn; `StaleTurn` if `turn_id` is supplied and no longer matches the active turn.

### `meeting.skip`

**Implemented by:** room (forwarded)  
**Parameters:** none  
**Returns:** `{ ok: true, seq: number }`

Skips the caller's current active turn and appends a system transcript turn
describing the skip. Operator skips are available in the TUI via `n` or
`/skip`.

**Error:** `NotYourTurn` if called by a participant that is not the active
speaker.

### `meeting.leave`

**Implemented by:** room + proxy  
**Parameters:** none  
**Returns:** `{ ok: true }`

Room removes the participant. Proxy closes `RoomConn` and resets session state (allowing a future `rooms.join` to a different room). If the room drops below 2 participants, its phase becomes `Ended` and remaining `wait_my_turn` calls return `{ ended: "insufficient-participants" }`.

### `meeting.status`

**Implemented by:** room (forwarded)  
**Parameters:** none  
**Returns:** `{ name, topic, phase, participants: [{ id, display_name, kind, liveness }], active_turn, last_turns: [Turn], moderator_mode, budget: { tokens_used, chars_used, max_total_chars } }`

`active_turn` is either `null` or `{ participant_id, display_name, turn_id,
started_at, deadline_at, response_started_at, timer_state, age_ms,
remaining_ms }`. `deadline_at` and `remaining_ms` are `null` after the active
participant starts responding. Participant `liveness`
contains `joined_at`, `last_poll_at`, poll age, `last_submit_at`, submit age,
and a coarse state: `active`, `polling`, `operator`, or `stale`.

### `room_info` (internal ping tool)

Used by `rooms.list` and `rozum list` to inspect a running room without joining. Not exposed to external agents (not in the `tools/list` response to agents).

## Participant Identity

- Agent participants: `clientInfo.name` from MCP `initialize` handshake (e.g. `claude-code` → "claude", `codex` → "codex"); proxy passes this to the room in a `meeting._join_internal` meta-call.
- Human participant: added at room startup with display name from `--as` (default `$USER`).
- Name collision: suffix `#2`, `#3` appended by the room.

## Tool Availability by State

| State | rooms.list | rooms.join | meeting.* |
|---|---|---|---|
| No room joined | ✓ | ✓ | ✗ (error: NotJoined) |
| Room joined, active | ✓ | ✗ (AlreadyJoined) | ✓ |
| Room joined, ended | ✓ | ✗ | only `meeting.leave` and `meeting.status` |

## Error Codes

`RoomNotFound`, `AlreadyJoined`, `NotJoined`, `NotYourTurn`, `StaleTurn`, `MeetingEnded`, `BudgetExceeded`.

## Behavior Notes

- All `meeting.*` tools must be idempotent on the network side: if the proxy loses the room connection mid-call the agent receives a transport error and must handle it gracefully (typically by calling `rooms.join` again or reporting to the user).
- `wait_my_turn` is re-entrant: if the previous call timed out (returned `still_waiting`) the agent calls again with the same or updated `since_seq`; the room deduplicates based on participant identity, not call-id.
- When `wait_my_turn` returns `your_turn: true`, the room treats that as the
  participant starting to respond and stops the round-robin reaction timeout
  for that turn.
- Sampling-capable participants may be auto-called by the room through
  `sampling/createMessage` when their active turn starts. Dispatching the
  sampling request starts the response and stops the round-robin reaction
  timeout. Non-sampling clients keep the pull-based `wait_my_turn`/`submit`
  flow.
