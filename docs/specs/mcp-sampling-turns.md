# MCP Sampling Turns

## Overview

Rozum should be able to wake capable MCP clients when their room turn starts.
Instead of requiring every agent to continuously poll `meeting.wait_my_turn`,
the room can request an LLM response through MCP `sampling/createMessage` and
write the returned text as the participant's turn.

## Interface

### Proxy

`rozum mcp-proxy` remains the single MCP server configured in external agents.
When an agent joins a room, the proxy opens a bidirectional MCP client
connection to the room and forwards room `sampling/createMessage` requests to
the upstream agent client. The proxy may advertise sampling optimistically so
clients that implement but do not advertise sampling can still be woken.

When the upstream client does not support `sampling/createMessage` (returns
`method_not_found` or never declared sampling capability), the proxy falls back
to the Anthropic Messages API using `ANTHROPIC_API_KEY` from the environment.
The model can be overridden with `ANTHROPIC_MODEL` (default:
`claude-haiku-4-5-20251001`). If neither the upstream nor the API is available,
the proxy returns an error and the turn falls back to normal polling.

### Room

Room connections advertise whether the joined participant supports
`sampling/createMessage`. When an active turn belongs to a sampling-capable MCP
participant, the room may call sampling with a prompt built from the meeting
topic, transcript, active speaker name, and turn id. A successful text response
is submitted as that participant's turn using the active `turn_id`.

Agents that do not support sampling keep the existing pull contract:
`meeting.wait_my_turn` followed by `meeting.submit`.

## Behavior

- [x] The proxy records the upstream agent `Peer<RoleServer>` during MCP
  initialize.
- [x] The proxy connects to rooms as an MCP client handler, not as a raw
  one-way JSON-RPC caller.
- [x] The proxy forwards room `sampling/createMessage` requests to the upstream
  agent client and returns the upstream result to the room.
- [x] The proxy has end-to-end coverage proving that a room-side
  `sampling/createMessage` request reaches the upstream MCP client through the
  stored proxy connection.
- [x] Room participants record whether their connection supports
  `sampling/createMessage`.
- [x] When a sampling-capable participant becomes active, the room requests a
  sampled message without waiting for the participant to poll.
- [x] A successful sampled text response is submitted with the active `turn_id`.
- [x] Sampling failures are transcript-visible system events and the turn is
  skipped or allowed to time out without blocking the meeting indefinitely.
- [x] Non-sampling clients continue to work with `wait_my_turn` and `submit`.
- [x] A `method_not_found` sampling response keeps the active turn open for
  normal polling instead of immediately skipping it.
- [x] When upstream does not support sampling, the proxy falls back to the
  Anthropic Messages API (`ANTHROPIC_API_KEY`) to generate the response.
- [x] The Anthropic model used for fallback is configurable via
  `ANTHROPIC_MODEL` (default: `claude-haiku-4-5-20251001`).

## Out of scope

- Requiring all MCP clients to support sampling.
- Streaming partial sampled responses into the transcript.
- Letting sampled responses call room tools during generation.
- Removing `meeting.wait_my_turn`.

## Design

The room keeps a per-participant sampling client handle. The handle points to
the room-side connection peer, which may be the proxy. The proxy's room-client
handler forwards `create_message` to the upstream agent peer that connected to
the proxy. This preserves the current one-proxy MCP configuration while making
the room-to-agent request path bidirectional.

## Decisions

- **Bidirectional proxy** -- chosen because configured agents already connect
  to `rozum mcp-proxy`, not directly to room sockets. Rejected: room directly
  sampling the proxy as if it were the model.
- **Sampling is opportunistic** -- chosen so clients that support sampling but
  do not advertise it can still be woken, while clients that reject
  `sampling/createMessage` still participate through polling. Rejected: making
  sampling mandatory for room joins.
- **Anthropic API fallback** -- chosen because Claude Code does not currently
  declare MCP sampling capability, so the proxy handles wake-up directly via
  the Anthropic Messages API when `ANTHROPIC_API_KEY` is set. Rejected:
  requiring every agent to implement native sampling.

## Results

Implemented in `src/meeting/proxy.rs`, `src/meeting/app.rs`,
`src/meeting/mcp_server.rs`, and `src/meeting/room_client.rs`. Verified with
`cargo fmt -- --check`, `cargo check`, and `cargo test`.
Added unit coverage for legacy sampling capability detection, task-based
sampling capability detection, proxy-to-room capability advertisement, and
method-not-found fallback. Added integration coverage for room-to-proxy-to-agent
sampling forwarding. Added Anthropic API fallback in `proxy.rs` (`anthropic_sample`)
for agents that do not support native MCP sampling.
