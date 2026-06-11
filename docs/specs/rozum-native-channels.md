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

## Tier 3 — gateway piggyback (last resort) — IMPLEMENTED

Implemented at the **launch-local proxy** (`src/proxy.rs`) with the transcript
tail dropped by the **mcp-proxy** (`src/meeting/proxy.rs`) through a shared file,
keyed by **project + agent name**. Module: `src/meeting/piggyback.rs`.

- **Activation — `ROZUM_PIGGYBACK=1` (opt-in).** Room text enters the model
  context (a prompt-injection surface), so Tier 3 is off unless the env var is
  set. One variable arms both ends: the launch-local proxy reads its own env, and
  the agent inherits it through `exec_agent` (`StdCommand` inherits the parent
  env), so the agent's mcp-proxy writer turns on too. No new launch flag.
- **Keying — `<project>/<agent>`.** Both processes run in the project dir, so the
  project slug = cwd basename matches on both ends (the same derivation as the
  room display-name prefix `<project>-<agent>`). Drops live at
  `$XDG_RUNTIME_DIR/rozum/piggyback/<project>/<agent>.log` (sibling of room
  sockets; tmpfs where available). The launch-local proxy drains *all* agent
  files under its project (one launch = one agent in a cwd, but coalescing is
  robust if more appear).
- **Writer (mcp-proxy):** the existing channel-pusher loop already long-polls the
  room and renders each new transcript delta to push as a Tier-1 channel event;
  when `piggyback::enabled()` it *also* `append`s that rendered line to the drop
  file. No new room read — it rides the pusher.
- **Reader (launch-local proxy):** in `forward`, after fingerprinting (so the
  injected room text never perturbs the poison identity) and once per request
  (not per retry), `maybe_inject_room_activity` drains the project's drops and
  folds them into the request as an out-of-band system note:
  - Anthropic `/v1/messages`: prepended to the top-level `system` (string,
    content-block array, or absent — `inject_anthropic_system`).
  - OpenAI `/v1/chat/completions`: a `system` message prepended to `messages`
    (`inject_openai_system`).
  - Tool-call JSON and SSE framing are never touched; non-chat paths
    (`/v1/models`, …) are zero-touch and never drained.
  - Drain happens only once a successful injection is guaranteed (shape checked
    first), so a parse miss never loses pending lines. Draining is a
    rename-then-read so an `append` racing the drain is preserved for next time.
- **Caps:** ≤ 4 KiB of the most recent pending text per injection
  (`MAX_INJECT_BYTES`); an undrained drop file is trimmed to its 16 KiB tail
  (`MAX_FILE_BYTES`) so a busy room can't grow it unbounded.
- **Note text:** `[rozum] Room activity arrived while you were busy …` + the
  rendered lines + "Use meeting.wait_my_turn … this note is a preview, not the
  turn API." — a wakeup preview, not a substitute for the turn API.
- **Reach:** any harness using our gateway (Codex/aider/opencode/older Claude).
  Not a true idle wake — lands at the agent's next inference call.

## Relationship to existing specs

- Complements `channel-wakeup.md` (Tier 1) — does not replace it.
- Tier 2 is the same `wait_my_turn` machinery; this spec just elevates it to "the
  rozum-native channel" and pins the agent-loop contract.
- Tier 3 reuses the `src/proxy.rs` request path from `shared-gateway-proxy.md`.

## Open Questions

1. ~~Tier-3 transcript-tail plumbing~~ — **RESOLVED: shared file**
   (`$XDG_RUNTIME_DIR/rozum/piggyback/<project>/<agent>.log`), written by the
   mcp-proxy's existing channel pusher, drained by the launch-local proxy. No new
   socket or endpoint; rides the long-poll the pusher already holds.
2. Tier-3 trigger policy: current behavior drops **every** new transcript delta
   (same set the Tier-1 channel pusher renders) and injects whatever is pending on
   the agent's next chat request. If this proves noisy, narrow to
   `your_turn`/@mention at the writer (`piggyback::append` call site) — the reader
   needs no change.
3. Tier 2: should the proxy *itself* keep a background `wait_my_turn` open and
   convert to a local nudge for harnesses with any injection hook, or is the
   agent-loop contract enough?

## Build order

1. **DONE** — Tier 2 contract: proxy `instructions` (`feature/rozum-native-channels`).
2. **DONE** — Tier 3 piggyback (`feature/piggyback-wakeup`): drop file keyed by
   project + agent (`src/meeting/piggyback.rs`), mcp-proxy writer on the channel
   pusher, launch-local proxy reader injecting an out-of-band system note into
   Anthropic/OpenAI chat requests. Opt-in via `ROZUM_PIGGYBACK=1`.

## Out of scope

- Modifying the agent client / reimplementing Claude Code's in-session injection.
- Any mechanism that mutates tool-call JSON or SSE framing.
