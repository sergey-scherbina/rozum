# rozum-native Channels — an Anthropic-independent wakeup ladder

## Motivation

`channel-wakeup.md` wakes an idle agent by pushing room activity through Claude
Code's **channels** (`notifications/claude/channel`) — a research-preview,
Claude-Code-only, ≥ 2.1.80 feature behind a `--dangerously-load-development-channels`
flag. We do not want the meeting wakeup to *depend* on that: it can break, get
gated, or simply not exist for Codex / aider / opencode / older Claude / headless.

Goal: own the wakeup end to end with rozum-controlled mechanisms, so a meeting
agent still gets woken when Anthropic's channel feature is unavailable.

## The unavoidable constraint (design from this)

We do **not** control the agent's client (Claude Code et al.), and an HTTP model
endpoint is **reactive** — it only produces bytes in response to a request the
agent made. Therefore:

- The only way to reach a **truly idle** session (no in-flight request, not
  looping) unsolicited is a message the *client* chooses to inject into its
  conversation. For Claude Code that injection point is exactly its `claude/channel`
  feature — which is the Anthropic dependency we're trying to avoid. We cannot
  reimplement *that specific push* without their client cooperating.
- What we **can** own: (a) a long-poll the agent itself holds open — we complete
  it the instant there's activity; (b) injecting room context into any model
  request the agent makes through our gateway.

So "rozum's own channel" is not a magic push into a dead-idle Claude session;
it's a **ladder** of rozum-controlled mechanisms, best-available-wins, that
together cover every realistic case without requiring Anthropic's channel.

## The wakeup ladder (best available wins)

| Tier | Mechanism | Owned by us? | Wakes a *truly idle* agent? | Works for non-Claude? |
|---|---|---|---|---|
| 1 | Anthropic `claude/channel` push (`channel-wakeup`) | no (Anthropic) | **yes** | no |
| 2 | **rozum long-poll channel** — agent holds `meeting.wait_my_turn`; we complete it on activity | **yes** | yes, *while the agent keeps the poll open* | **yes** (any MCP agent) |
| 3 | **gateway piggyback** — inject pending room context into the agent's next model request/response | **yes** | no (pull-time only) | **yes** (any gateway client) |

- **Tier 1** stays the preferred path when present (zero extra agent effort).
- **Tier 2 is the rozum-native core.** `wait_my_turn` is *already* a long-poll we
  complete the moment a turn/delta appears — that **is** a channel we own, working
  for every MCP client, independent of Anthropic. The only "cost" vs Tier 1 is
  that the agent must keep the poll outstanding while idle. We make that the
  documented contract (proxy `instructions`): when channels are unavailable, loop
  `wait_my_turn`; an agent that loops is never deaf.
- **Tier 3 (piggyback) is the explicit last resort** (per decision): for an agent
  that neither supports Tier-1 channels nor keeps a Tier-2 poll open, surface room
  activity by injecting it into whatever model traffic it *does* send through our
  gateway. Reaches the agent at its next inference call, never a truly idle one.

## Tier 2 — rozum long-poll channel (primary native path) — IMPLEMENTED

Already 90% built (`meeting.wait_my_turn` + the `channel-wakeup` pusher). Made a
robust standalone wake when Tier 1 is off:

- **Done:** the proxy `instructions` (`initialize`, `src/meeting/proxy.rs`) now
  tell the agent: if you are NOT receiving `<channel>` events (client doesn't
  support them), keep a `meeting.wait_my_turn` poll outstanding the whole time
  you are idle — it returns the instant someone speaks, so you never miss a turn
  without channels; this long-poll is rozum's own wakeup, don't stop looping it
  while in the room.
- No protocol change — this tier is a documentation + behavior contract over
  existing tools. It is the fallback `channel-wakeup` already names.

## Tier 3 — gateway piggyback (last resort)

When approved as the last resort, implement at the **launch-local proxy**
(`src/proxy.rs`), which already buffers requests and streams responses:

- **Delivery:** prepend a clearly-delimited, out-of-band system note to the
  forwarded request (preferred over rewriting the response — never touch
  tool-call JSON or SSE framing). Example injected system text:
  `[rozum] While you were busy: <name> said "…" in room <X>. Use meeting.wait_my_turn for the full thread.`
- **Source of pending context:** the proxy needs a read path to the room
  transcript tail, which it does not have today (only the mcp-proxy talks to
  rooms). New plumbing: a small per-launch "which room is this agent in + tail
  since seq N" lookup. Likely a lightweight local IPC or a shared file the
  mcp-proxy writes when its agent joins/sees turns. **This is the bulk of Tier-3
  work** and is why it's last-resort.
- **Scope guards:** opt-in only; gate on room membership (prompt-injection
  surface — room text enters the model context); coalesce/cap so a busy room
  can't flood; never alter tool JSON or stream framing; strip the injected note
  from anything persisted as real conversation if feasible.
- **Reach:** any harness using our gateway (Codex/aider/opencode/older Claude).
  Not a true idle wake.

## Relationship to existing specs

- Complements `channel-wakeup.md` (Tier 1) — does not replace it.
- Tier 2 is the same `wait_my_turn` machinery; this spec just elevates it to "the
  rozum-native channel" and pins the agent-loop contract.
- Tier 3 reuses the `src/proxy.rs` request path from `shared-gateway-proxy.md`.

## Open Questions

1. Tier-3 transcript-tail plumbing: shared file vs local socket vs the mcp-proxy
   exposing a tail endpoint? (Decide when/if Tier 3 is scheduled.)
2. Tier-3 trigger policy: every new turn, only `your_turn`/@mention, or only
   after the agent has been silent for some interval?
3. Tier 2: should the proxy *itself* keep a background `wait_my_turn` open and
   convert to a local nudge for harnesses with any injection hook, or is the
   agent-loop contract enough?

## Build order

1. **DONE** — Tier 2 contract: proxy `instructions` (`feature/rozum-native-channels`).
2. Tier 3 only if a concrete agent appears that supports neither Tier 1 nor a
   Tier-2 loop — start with the transcript-tail plumbing, then conservative
   system-note injection.

## Out of scope

- Modifying the agent client / reimplementing Claude Code's in-session injection.
- Any mechanism that mutates tool-call JSON or SSE framing.
