# Rozum restart resilience

## Overview

When the operator restarts the `rozum` process, MCP-side agents
(Claude Code, Codex) lose their connection and the room transcript
disappears. Today recovery is manual: the agent must poll
`rooms.list`, find a new (random) room, `rooms.join` again, and re-
discover the conversation from scratch. The operator-visible symptom
is "messages you write in the gap are not delivered to me" — see the
diagnostic exchange in room `crisp-glen` on 2026-06-06.

Two complementary fixes make a restart look like a 1-3 s pause instead
of a session reset:

1. **mcp-proxy-reconnect** — the proxy that already runs inside every
   agent's MCP setup transparently retries the underlying connection
   when the Unix socket disappears, so the agent never sees
   `Transport closed`.
2. **room-transcript-persist** — the `rozum` room persists its
   transcript to disk on every submit and loads it back on startup,
   so a process restart preserves history (and seq numbering).

Stable room names (`rozum --room <name>`) and socket-file reuse
(`std::fs::remove_file` before `bind`) are **already implemented**
(`src/main.rs:180`, `src/meeting/app.rs:48`). This spec covers only
the two missing pieces.

## Interface

### mcp-proxy-reconnect (`src/meeting/proxy.rs`)

The proxy currently calls `RoomConnection::connect`, drives forwarding,
and on any I/O failure propagates the error up — which terminates the
MCP stdio session. Change: wrap the connect and the forwarding loop in
a retry policy. After any failure that is plausibly a socket-died-during-
restart event (`io::Error` of kind `BrokenPipe`, `UnexpectedEof`,
`ConnectionAborted`, `ConnectionReset`, or a transport-closed marker
from the JSON-RPC layer), wait, then re-resolve the same socket path
and reconnect.

Retry policy:

- `RECONNECT_MAX_ATTEMPTS = 10`
- delays follow a capped backoff: `200ms, 200ms, 500ms, 500ms, 1s, 1s, 2s, 2s, 5s, 5s` (total ≤ 18 s)
- on every attempt: stat the socket file first; if it doesn't exist
  yet, treat as "rozum still booting" and keep retrying.
- on connect success, the proxy re-issues `_join_internal` with the
  same display name (so the room recognizes us as the same logical
  participant), then resumes forwarding.

The proxy may be invoked in two modes:

- **Auto-room mode** (current default) — the agent supplies a room
  name through `rooms.join`. The proxy remembers that name for the
  duration of the session. On reconnect it tries the **same room
  name**. If `rooms.list` shows it absent after `RECONNECT_MAX_ATTEMPTS`,
  the proxy reports the original `Transport closed` upward — caller
  decides what to do (typically: pick a different room).
- **Pinned-room mode** (new optional CLI flag) — `rozum mcp-proxy
  --room <name>` keeps a single named room across sessions. The proxy
  remembers that name from CLI and skips room discovery on reconnect.

No new MCP tool. No protocol-side change. All retry is in the
proxy↔socket leg; the agent↔proxy stdio session never breaks.

### room-transcript-persist (`src/meeting/app.rs`, `src/meeting/state.rs`)

`Meeting.transcript: Vec<Turn>` lives only in memory today
(`state.rs:106`). Persist it to disk:

- New struct field `Meeting.persist_path: Option<PathBuf>` (default
  `None`). When `RoomConfig.persist` is `true`, `app.rs` resolves the
  path to `$XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl`
  (fallback `$HOME/.local/state`).
- On `Meeting::new(config)`: if `persist_path` exists, read the file
  line by line, parse each line as a `Turn`, push into `transcript`.
  Bad lines are skipped with a `tracing::warn!`. The next `seq` is
  computed as `transcript.len()` so newly-added turns continue from
  where the file left off.
- On every successful submit (the `submit` path in `state.rs:400`+),
  if `persist_path` is set, open the file in append mode and write one
  JSON line: the same `Turn` shape used in memory.
- `RoomConfig` gains `persist: bool`, defaulting to `true`. `rozum
  --no-persist` (a single CLI flag on the bare command) disables it.

This is the same on-disk format as `web-transcript-persist` writes,
but at a **different path** (`room-transcript.jsonl` vs the bridge's
`transcript.jsonl`) and at a **different layer** — the room writes
canonical `Turn`s, the bridge writes `msg` envelopes. Both are valid
sources of history; the web client prefers the bridge file because it
sees the same envelope shape live.

Schema for a persisted line:

```json
{
  "seq": 12,
  "ts": 1780000000,
  "participant_id": "claude-code",
  "display_name": "claude-code",
  "content": "…",
  "injected": false
}
```

`Meeting` itself has no new public methods; persistence is transparent
to `mcp_server.rs` and to `wait_my_turn`.

### CLI

`src/main.rs`:

- `rozum --no-persist` — disables room transcript persistence (default
  on). Mutually independent of the existing `rozum web --no-persist`
  (one controls room-side, the other controls bridge-side).
- `rozum mcp-proxy --room <name>` (optional) — pins a room across
  reconnects.

## Behavior

- [x] After `rozum --room R` is killed and restarted within the
      reconnect window (< 18 s), every joined agent's MCP session
      resumes without surfacing `Transport closed` to the agent.
- [x] On reconnect, the agent's `wait_my_turn` resumes with
      `since_seq` it left off — no replay of messages it already saw,
      and no missing messages from the gap.
- [ ] `rooms.list` from the agent during the restart gap returns the
      proxy's last-known room name (so a manual rejoin still works
      even before auto-reconnect succeeds).
- [x] `rozum --room R` started after a previous run with the same
      name reads `room-transcript.jsonl` into memory and continues
      the seq counter from where it left off.
- [x] Every `meeting.submit` appends one JSON line to
      `room-transcript.jsonl`; lines round-trip parseable.
- [x] `rozum --room R --no-persist` does not read or write the file.
- [ ] Web bridge sees the loaded history through its normal
      `wait_my_turn(since_seq:0)` path, with no bridge-side change.
- [x] After ≥ `RECONNECT_MAX_ATTEMPTS` failures, the proxy surfaces
      the underlying error upward and exits, so the agent's MCP
      runtime can re-spawn it if configured to do so.

## Out of scope

- Replicated / multi-host rooms. This spec is single-host.
- Encryption of the persisted transcript at rest.
- Garbage-collection / rotation of `room-transcript.jsonl`.
- Migration of the existing per-bridge `transcript.jsonl` files to
  the new room-side layout.
- A "persist responding/polling presence" feature. Presence is
  intentionally ephemeral.
- Auto-naming a stable room when no `--room` was provided. The
  operator opts in.

## Design

### Why two slugs, one spec

The reconnect logic and the transcript persistence are independent in
implementation (different files, different ownership) but both
needed for the operator-visible promise "restart is invisible". One
spec keeps the contract in one place; two slugs let two agents work
in parallel.

### Reconnect lives in the proxy, not the agent

The proxy is part of `rozum`. Agents (Claude Code, Codex, anything
MCP-shaped) get the fix for free without code changes. A separate fix
on every agent would multiply work and create version skew.

### Persist canonical `Turn`s, not envelope JSON

`Meeting.transcript: Vec<Turn>` is the source of truth. Writing
envelopes here would couple the room to the web bridge's serialization
format. Re-derive envelopes on read in the bridge as today.

### Same `$XDG_STATE_HOME/rozum/rooms/<name>/` directory as the bridge

Operators expect "all the state for a room is in one directory". Two
files (`room-transcript.jsonl` from the room, `transcript.jsonl` from
the bridge) is fine — they have different lifetimes (room file
written on every submit regardless of whether a bridge is up; bridge
file written only when bridge sees the broadcast).

### Reconnect uses last-known room name, not random scan

A `rooms.list` scan during reconnect could attach to *some* room, but
the wrong one if the operator changed plans. Stick with the name we
joined as. If that name disappears for ≥ 18 s, fail loudly.

## Decisions

- **Capped exponential backoff `200ms…5s`, max 10 attempts (~18 s)** —
  long enough to cover an incremental
  `cargo build --workspace --no-default-features --bins && ./target/debug/rozum`
  restart on a workstation; short enough not to hide a real failure.
  Rejected: indefinite retry (masks bugs).
- **Re-issue `_join_internal` after reconnect** — the room treats
  participants as ephemeral; without re-join we'd be missing from
  participant list. Rejected: server-side participant resurrection
  by display name (couples server to client identity).
- **Persistence opt-out, not opt-in** — operator's expected experience
  is "history survives restart"; `--no-persist` is the escape hatch.
  Symmetric with `web-transcript-persist` decision.
- **`room-transcript.jsonl` separate from bridge's `transcript.jsonl`**
  — different layer, different write trigger; keeping them separate
  avoids interleaving issues and keeps the bridge change small (none).
- **Room-side seq continues across restarts** — required for the
  agent's `since_seq`-based long-poll to keep working after reconnect;
  otherwise seq goes backward and the agent's dedup explodes.
- **No new MCP tool, no new wait_my_turn fields** — keep the protocol
  surface stable. All recovery is local to the proxy.

## Sprint plan

Two slugs, can be claimed in parallel:

1. **`mcp-proxy-reconnect`** — retry/backoff + same-name reconnect in
   `src/meeting/proxy.rs`. Optional `mcp-proxy --room <name>` flag in
   `src/main.rs`. Touches: `src/meeting/proxy.rs`, `src/main.rs`.
2. **`room-transcript-persist`** — `Meeting.persist_path`, load on
   `new`, append on submit. `RoomConfig.persist` boolean and
   `--no-persist` CLI flag. Touches: `src/meeting/app.rs`,
   `src/meeting/state.rs`, `src/main.rs`.

`mcp-proxy-reconnect` is independent of `room-transcript-persist` — a
restarted room with empty transcript still benefits from no-Transport-
closed UX. `room-transcript-persist` is independent of reconnect — a
manual rejoin into a persisted room still sees the history.

## Results

(Fill in after implementation and verify.)
