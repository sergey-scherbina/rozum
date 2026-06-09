# Turn Control and Liveness

## Overview

Rozum meeting rooms must keep round-robin conversations moving even when an
agent is joined but not actively polling. Active turns therefore have stable
turn identifiers, expiry deadlines, explicit skip behavior, and liveness
metadata that operators and agents can inspect.
The expiry deadline is a reaction deadline: it runs until the participant
starts responding, then stops while the response is drafted and submitted.

## Interface

### MCP

- `meeting.wait_my_turn({ since_seq?: number })` returns `turn.turn_id` when
  `your_turn` is true and includes `active_turn` metadata when any speaker is
  active. Returning `your_turn: true` starts the active participant's response
  and stops the reaction deadline.
- `meeting.submit({ content: string, turn_id?: number })` accepts an optional
  `turn_id`. When present it must match the current active turn.
- `meeting.skip()` skips the current active turn. If the caller is the active
  speaker, the skip is allowed. Operators may also skip through the TUI.
- `meeting.status()` returns participant liveness data and active-turn
  metadata: speaker, `turn_id`, age, nullable deadline, remaining time,
  `response_started_at`, `timer_state`, and skip reason where applicable.

### TUI

- `n` skips the current active turn.
- `/skip` skips the current active turn.
- Transcript text wraps within the transcript panel instead of being truncated
  horizontally.
- The waiting line shows who is active and how much time remains when a
  deadline exists. Once the participant starts responding, it shows responding
  instead of counting down.
- Participant rows show the active speaker and whether an MCP participant has
  polled recently.

## Behavior

- [x] Active turns receive monotonically increasing `turn_id` values.
- [x] `meeting.submit` rejects a stale `turn_id`.
- [x] `meeting.wait_my_turn` returns immediately when a participant's active
  turn starts, even if no transcript turn was added.
- [x] Active turns expire automatically if the participant does not start
  responding before the configured timeout, and the moderator advances to the
  next speaker.
- [x] Started responses stop the round-robin timeout until the turn is
  submitted or skipped.
- [x] Skipping a turn emits a transcript-visible system event and unblocks
  waiting pollers.
- [x] `meeting.status` exposes active-turn deadline and participant liveness
  fields without requiring a transcript change.
- [x] TUI transcript rendering wraps long messages and long words within the
  transcript panel.

## Out of scope

- Durable room persistence across process restarts.
- Background execution inside Codex or other agent hosts.
- Network push notifications to agents.

## Design

`Meeting::active_turn` owns the current turn id and timing fields. The room
advance loop is responsible for expiring unacknowledged turns before asking the
moderator for a new speaker. MCP polling updates per-participant liveness
timestamps so status and the TUI can report whether a joined agent is actively
waiting.

## Decisions

- **Server-side timeout** -- chosen because it protects every client type,
  including agents that cannot run a continuous event loop. Rejected: relying
  only on client-side polling discipline.
- **Optional submit turn_id** -- chosen for backward compatibility with
  existing agents while allowing newer clients to avoid stale submits.
  Rejected: making `turn_id` mandatory immediately.

## Results

Implemented in `src/meeting/state.rs`, `src/meeting/app.rs`,
`src/meeting/mcp_server.rs`, `src/meeting/proxy.rs`, and `src/tui/mod.rs`.
Verified with `cargo check` and `cargo test` (22 tests passing). Added focused
unit coverage for monotonic turn ids, stale turn rejection, and timeout skips.
