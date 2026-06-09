# Turn Response Start Timeout

## Overview

Round-robin timeouts measure how long the active participant takes to notice
their turn and begin responding. They must not measure the whole time spent
drafting, sampling, or submitting the response.

## Interface

- `active_turn.deadline_at` is nullable. A value means the room is still
  waiting for the participant to start responding. `null` means the participant
  has started responding and the round-robin timeout is stopped.
- `active_turn.response_started_at` records when the room observed response
  start.
- `active_turn.timer_state` is `"waiting"` while the reaction timer is running
  and `"responding"` after it is stopped.
- `meeting.wait_my_turn` implicitly starts the response timer state when it
  returns `your_turn: true` to the active participant.
- `sampling/createMessage` dispatch implicitly starts the response timer state
  for sampling-capable participants.
- In the TUI, the human turn timer stops on the first composing key in normal
  reply input, not merely when the input panel opens.

## Behavior

- [x] Active turns still expire if no participant starts responding before the
  configured deadline.
- [x] Once a response has started, `expire_active_turn_if_due` does not skip the
  turn because of the original reaction deadline.
- [x] TUI `Esc` or empty submit after composing resumes the human turn timeout
  for the same turn.
- [x] TUI slash commands that are not an actual answer resume the timeout when
  they leave the same human turn active.
- [x] Polling agents do not need a new mandatory MCP tool call; receiving
  `your_turn: true` is the response-start signal.
- [x] Sampling agents stop the round-robin timeout before generation starts.

## MCP Notes

MCP does not define a generic client-to-server "typing started" notification
for arbitrary tool clients. `sampling/createMessage` is suitable for
server-initiated agent generation because the room knows exactly when it sends
the request. `notifications/progress` is request-scoped progress reporting, not
a room-level typing signal, so rozum models response start as meeting state.

## Results

Implemented in `src/meeting/state.rs`, `src/meeting/app.rs`,
`src/meeting/mcp_server.rs`, and `src/tui/mod.rs`.
