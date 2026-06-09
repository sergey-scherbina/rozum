# Responding and Polling Indicators

## Overview

Bring back lightweight per-participant presence signals in the always-on
meeting model. Two orthogonal states are tracked alongside the existing
participant list:

- **responding** — the participant is composing a reply right now.
- **polling** — the participant has a `wait_my_turn` long-poll in flight.

Replaces the per-turn `response_started_at` field that existed in the
pre-rewrite turn-based design with global per-participant markers the room
keeps independently of any active turn (since there are no turns now).

## Interface

### State

`Meeting` gains:

- `responding: HashMap<ParticipantId, u64>` — participant id → unix timestamp
  when they started responding. Stale entries (>30s) are filtered on read.
- `polling: HashMap<ParticipantId, u64>` — participant id → unix timestamp
  when their current `wait_my_turn` long-poll started. Stale entries
  (>30s, just past the 25s long-poll window) are filtered on read.

### Tool: `meeting.mark_responding`

**Parameters:** none.
**Returns:** `{ ok: true, ts: u64 }`.

Records the caller as "responding now". Idempotent: re-marking refreshes the
timestamp. Cleared automatically on the next `meeting.submit` from the same
participant, on `meeting.leave`, or after 30 s of inactivity.

### Surfaced in existing tools

`meeting.wait_my_turn` payload (both `still_waiting` and the turn case) and
`meeting.status` include `responding` and `polling` arrays:

```
responding: [
  { participant_id, display_name, started_at, age_ms }
],
polling: [
  { participant_id, display_name, started_at, age_ms }
]
```

Stale entries are omitted at read time.

### Events

- `MeetingEvent::RespondingChanged { participant_id, display_name, started: bool }`
- `MeetingEvent::PollingChanged { participant_id, display_name, started: bool }`

Emitted on mark and clear so TUI / web can update without re-reading state.

## Behavior

- [x] `Meeting::mark_responding(id)` inserts/refreshes the timestamp and emits
  `RespondingChanged { started: true }` if the entry was newly inserted.
- [x] `meeting.submit` from a participant clears their `responding` entry and
  emits `RespondingChanged { started: false }`.
- [x] `meeting.leave` clears their `responding` entry and `polling` entry.
- [x] Stale entries (now − ts > 30 s) are excluded from `wait_my_turn` and
  `meeting.status` output but the map is cleaned lazily, not eagerly.
- [x] `run_sampling` (`src/meeting/app.rs`) calls `mark_responding` when
  sampling for a participant starts, so auto-responses also show as typing.
- [x] `meeting.wait_my_turn` marks the caller as polling on entry and unmarks
  on every exit path via an RAII `PollingGuard` (handles normal returns,
  errors, and async cancellation).
- [x] TUI bottom status line shows each agent independently: "X is typing…"
  for responding, "X is waiting…" for polling-only. Typing wins over polling
  for the same participant.
- [x] No new dependencies. No protocol-breaking changes to existing fields.

## Out of scope

- TUI / web rendering of the indicator (separate change; this spec only
  exposes the data).
- Per-participant per-message identifiers (not needed for a typing dot).
- Push delivery of `RespondingChanged` to MCP clients (TUI uses the
  in-process broadcast; MCP clients poll `wait_my_turn`).

## Design

A simple `HashMap` is enough because:

- It is per-room, in-process, lock-shared with the rest of `Meeting`.
- Membership is small (≤ participants).
- Stale-filter at read time avoids a background task.

The map lives on `Meeting` instead of inside individual participant records to
keep `Participant` data static and to share one cleanup path with submit/leave.

## Decisions

- **Lazy cleanup** — chosen so we don't need a tick loop. Rejected: background
  reaper task.
- **30 s stale window** — long enough for typical model responses, short
  enough that a crashed agent stops appearing as "responding" quickly.
- **Tool name `meeting.mark_responding`** — chosen to match `meeting.*` naming
  and keep the verb visible. Rejected: `meeting.typing`, `meeting.heartbeat`.

## Results

Implemented in `src/meeting/state.rs` (responding + polling state, lifecycle,
events, tests), `src/meeting/mcp_server.rs` (tool, payload surface,
`PollingGuard`), `src/meeting/app.rs` (auto-mark in `run_sampling`),
`src/meeting/proxy.rs` (forwarded `meeting.mark_responding` tool), and
`src/tui/mod.rs` (per-agent bottom status line with typing/waiting states).
Verified with `cargo fmt`, `cargo check`, and `cargo test --lib`
(25 tests pass, including five new responding/polling cases).

