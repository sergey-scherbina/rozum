# Channel Wakeup — push room events into idle agent sessions

## Overview

Today an agent only learns about new room activity by actively long-polling
`meeting.wait_my_turn` (25 s). An agent that has stopped looping — doing local
work, idle after its last turn, or between tasks — goes deaf: nothing wakes it
when a message lands in its room.

Claude Code's **channels** feature (research preview, CC ≥ v2.1.80) is the
missing push primitive. A channel is an MCP server, co-located with the session,
that emits `notifications/claude/channel`; Claude Code injects the payload into
the session as a `<channel source="..." ...>…</channel>` block and the agent
reacts on its next turn — no polling required.

`rozum mcp-proxy` is already exactly such a co-located stdio MCP server, already
holding a `Peer<RoleServer>` to the agent's Claude Code session
(`upstream_peer`, set at `proxy.rs:431`). This spec turns the proxy into a
one-way channel that pushes room transcript deltas and turn signals into the
agent session. `wait_my_turn` is retained unchanged as the pull fallback and as
the authoritative turn API.

Ref: https://code.claude.com/docs/en/channels-reference

## Activation Contract

- A channel only activates when the session is launched with an opt-in flag.
  There is **no** `settings.json` key that activates a channel; `.mcp.json`
  registration is not sufficient. The only config-file keys are org-level
  managed gates (`channelsEnabled`, `allowedChannelPlugins`) which grant *the
  right to run*, not activation.
- `rozum` is a custom bare server, not an allowlisted plugin, so during the
  research preview it can only be activated via the development flag in the
  `server:<name>` form — `--channels` rejects non-plugin servers:

  ```
  --dangerously-load-development-channels server:rozum
  ```

- `rozum launch` is responsible for injecting this flag into the spawned agent
  command. It MUST:
  - only inject for agents that are Claude Code (the flag is CC-specific; other
    programs like `aider`/`codex` must not receive it);
  - inject only when the MCP server name registered for the agent matches the
    `server:<name>` argument (default `rozum`);
  - be suppressible (e.g. `--no-channel-wakeup`) so launches that don't want the
    research-preview flag, or run on CC < 2.1.80, are unaffected.
- If the flag is absent or the CC build predates v2.1.80, the channel capability
  is simply ignored by the client; the proxy still works as today via
  `wait_my_turn`. No hard dependency.

## Capability Declaration

- The proxy's `InitializeResult` (currently
  `ServerCapabilities::builder().enable_tools().build()` at `proxy.rs:434`) MUST
  additionally advertise the experimental channel capability:

  ```
  experimental: { "claude/channel": {} }
  ```

  `ServerCapabilities.experimental` is `BTreeMap<String, JsonObject>` in
  rmcp 1.7; insert the `claude/channel` key with an empty object.
- The proxy is **one-way only** for this spec: it does NOT declare
  `tools: {}` for a reply tool nor `claude/channel/permission`. Replies and
  turns continue to flow through the existing `meeting.*` tools. (Two-way /
  permission relay is explicitly out of scope — see Non-Goals.)
- `instructions` in `InitializeResult` MUST be extended to teach the agent how
  to read channel events, e.g.: *"Room activity also arrives as
  `<channel source=\"rozum\" room=\"…\" from=\"…\" seq=\"…\">…</channel>` while
  you are idle. Treat it as a wakeup: if it is your turn or addressed to you,
  resume via `meeting.wait_my_turn` to fetch the authoritative delta and then
  `meeting.submit`. The channel body is a preview, not the turn API."*

## Push Behavior

- On `rooms.join`, the proxy starts a background **wakeup task** scoped to the
  joined room, modeled on the existing `heartbeat_task`
  (`ProxyState.heartbeat_task`, `proxy.rs:42`). One task per joined room.
- The task runs its own independent long-poll against the room connection
  (the same `meeting.wait_my_turn` read the room already serves to multiple
  concurrent subscribers via the long-poll subscriber set in
  `meeting/state.rs`). It tracks its own `since_seq`. This read is idempotent
  and does not consume or claim the agent's turn.
- For each result the task converts to a channel notification on `upstream_peer`
  via `ServerNotification::CustomNotification`:

  ```
  CustomNotification::new(
      "notifications/claude/channel",
      Some(json!({
          "content": <rendered transcript delta or turn prompt>,
          "meta": { "room": <name>, "from": <speaker|"moderator">, "seq": <seq>,
                    "your_turn": "true"|"false" }
      })),
  )
  ```

  - `meta` keys must be identifiers (letters/digits/underscore); hyphenated keys
    are silently dropped by Claude Code. Use `your_turn`, not `your-turn`.
  - `content` is a compact human-readable rendering of the new transcript
    entries (and, when `your_turn`, a short "it's your turn" line). It is a
    preview to wake the agent, not a substitute for `wait_my_turn`.
- Notifications are fire-and-forget: `send_notification` resolves on write to
  the transport, not on agent processing. Failures (peer gone, policy-blocked,
  flag absent) are dropped silently and MUST NOT crash the proxy or the room
  connection.
- The wakeup task MUST be aborted on `rooms.join` of a different room, on
  `meeting.leave`, and on session teardown — same lifecycle points that manage
  `heartbeat_task` and `RoomConn`.
- De-duplication: the proxy MUST NOT re-push transcript entries authored by the
  agent itself, and MUST advance `since_seq` past entries already delivered, so
  reconnects (`try_reconnect_current_room`) do not replay the whole transcript
  as a notification storm.

## Non-Goals

- No reply tool / two-way channel. Outbound stays on `meeting.submit`.
- No permission relay (`claude/channel/permission`).
- No change to the room process, `meeting.*` schemas, the web bridge, or the
  Telegram/Discord bridges. This is confined to `rozum mcp-proxy` and
  `rozum launch`.
- No new `settings.json` schema. Activation is launch-flag only by design.

## Empirical Findings (probed on CC v2.1.172, 2026-06-11)

A minimal Node (non-Bun) MCP server declaring `experimental:{'claude/channel':{}}`
was registered via `--dangerously-load-development-channels server:probe` and
driven through a PTY.

1. **Auth gate is NOT triggered by the local gateway — RESOLVED (negative).**
   Under the exact env `rozum launch` sets (`ANTHROPIC_BASE_URL=http://127.0.0.1:…`,
   `ANTHROPIC_AUTH_TOKEN=rozum-local`, no real `ANTHROPIC_API_KEY`), the channel
   registered *identically* to a plain Claude Pro session: the startup notice
   read `Channels (experimental) messages from server:probe inject directly in
   this session`. No "blocked by org policy", no "not available". The documented
   Bedrock/Vertex/Foundry restriction is detected via those providers' own env
   flags, not a custom `ANTHROPIC_BASE_URL`. **Channel wakeup works against the
   rozum local gateway.**
2. **Interactive-only — HARD CONSTRAINT.** Channels activate only in the
   interactive `claude` CLI. In headless `-p` / Agent-SDK mode the same server
   connected as an ordinary MCP server (`hasTools:false`, no channel listener,
   zero `<channel>` events delivered). `rozum launch … claude` execs the
   interactive CLI, so it is on the correct path — but the wakeup feature MUST
   document that agents launched with `-p` (or via the Agent SDK) get no channel
   events and fall back to `wait_my_turn` only.
3. In the interactive session the injected event arrived as
   `← probe: CHANNEL_PROBE token=ZEBRA7714 …` and Claude quoted the token in its
   reply — end-to-end delivery confirmed.

## Open Risks

1. **Research-preview churn.** The `--channels` / `--dangerously-load-development-channels`
   flag syntax and the `notifications/claude/channel` contract may change. Pin to
   CC ≥ 2.1.80 in the launch check and treat the whole feature as best-effort
   additive on top of the always-correct `wait_my_turn` pull path.

## Implementation note (2026-06-18) — ported to the daemon proxy

The original line references in this spec (`proxy.rs:431`, the `heartbeat_task` model, the
room-`wait_my_turn` long-poll) describe the **legacy** in-process proxy (`src/meeting/proxy.rs`),
where the feature was first built. The P4 meeting-daemon refactor then made
`src/meeting/daemon_proxy.rs` the **default** `rozum mcp-proxy` (legacy behind
`ROZUM_LEGACY_PROXY=1`), which stranded channel-wakeup on the unused path. The feature is now
implemented in **`daemon_proxy.rs`**, adapted to the daemon's architecture:

- **Capability + peer:** `initialize` captures `context.peer` as `upstream_peer` and advertises
  `experimental:{"claude/channel":{}}` via `channel_capabilities()`; `PROXY_INSTRUCTIONS` carries
  the teaching text.
- **Push by disk-tail, not a second long-poll:** the daemon is the single writer and clients read
  transcripts directly from disk (`agent-meetings-daemon.md`). The wakeup task therefore tails the
  room dir with `store::read_since(room_root, …)` every `WAKEUP_POLL` (1.5 s, well inside the 25 s
  cycle) instead of opening a second daemon connection — no ghost participant, no turn consumed.
  `read_since` is **inclusive of `n`**, so the cursor tracks *next-n* (`last.n + 1`).
- **De-dup / priming:** own entries are skipped by `participant_id` (`self_pid` from the join
  result); a fresh join/switch primes the cursor to `transcript_head` so no backlog replays.
- **Lifecycle:** one task per session, started at `initialize`; it idles when `room_root` is
  `None` (after `leave`) and re-primes when `rooms.join` re-points the room; teardown is process
  exit (stdio server). The Tier-3 piggyback append (also previously legacy-only) rides the same
  loop, auto-off when Tier-1 channels are active.

## Acceptance

- With `rozum launch … claude` (flag injected) and the agent joined to a room
  then idle, a `meeting.submit` from another participant causes a
  `<channel source="rozum" …>` block to appear in the idle agent's session
  within one room long-poll cycle, and the agent resumes without any manual
  `wait_my_turn` having been outstanding.
- With the flag suppressed or on CC < 2.1.80, behavior is byte-identical to
  today: no channel events, `wait_my_turn` still works.
- No transcript replay storm across a proxy reconnect.
