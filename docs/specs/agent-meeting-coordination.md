# Agent meeting coordination — the collaboration system

## Goal (set by the user, 2026-06-18)

Make the meeting room the place **all the user's agents coordinate their work when they
need to**, while the **human can see what's happening at any moment and step in with their
own messages**. The meeting daemon stops being "a chat room" and becomes a **collaboration
hub** with **equal, pluggable clients** and a **stable identity model**:

1. **Agents coordinate** — every agent the user runs (Claude Code, Codex, opencode, …),
   however it's launched, auto-joins the right room and coordinates through it at its own
   discretion (announce work, check before clashing, ask when stuck, report done).
2. **The human observes** — at any moment, through any client, the human sees the live
   transcript across the rooms that matter.
3. **The human intervenes** — posts into the room from any client; agents see it as a
   first-class message (and idle agents are woken).

Forward-looking (design for it now, don't fully build yet, per the user):
- **Clients are equal front-ends**: TUI (first), web, Telegram, Discord, future remote —
  all are clients of the one daemon over the same contract. None is privileged.
- **One human, many clients**: the same person on TUI + web + Telegram is **one identity**,
  not three participants.
- **Many humans, possibly remote**: multiple distinct people, some far away, eventually
  join the same rooms. Not the priority now, but nothing in the foundation may preclude it.

## Foundation (already shipped — `agent-meetings-daemon.md`)

- The **meeting daemon** (`rozum meetings`) hosts many disk-backed rooms (`.rozum/room/`,
  daily JSONL, single-writer, direct-read clients).
- **`rozum mcp-proxy`** (the daemon proxy) gives an agent `meeting.{wait_my_turn,submit,
  mark_responding,status,leave}` + `rooms.{list,join}`, auto-detects the project, auto-spawns
  the daemon, auto-joins the project room, tracks the read cursor.
- **`rozum launch <agent>`** wires the proxy + the `channel-wakeup` flag into the agent.
- **channel-wakeup** pushes `notifications/claude/channel` to an idle interactive Claude Code
  session (ported into the default daemon proxy, 2026-06-18).
- **Identity today**: opaque `ParticipantId` + per-project `handle` + a `session_token`
  reconnect key, persisted in `roster.json`. **One token = one participant.**
- **Human/other clients today**: the TUI (`rozum` / `rozum meetings attach`), daemon web
  client (`rozum meetings web`), and daemon-backed Telegram/Discord bridges. The separate
  legacy `rozum web` escape hatch still uses the in-process room.

## The gaps this spec closes

- **A — "all agents".** Only an agent launched via `rozum launch` (or with the proxy in its
  MCP config) joins. Bare `claude`/`codex` runs don't. Need both paths zero-config.
- **B — "when they need to".** Coordination is etiquette text the model may ignore. Need
  strong instructions **plus** hard lifecycle hooks at the few points that must always fire.
- **C — rooms.** Per-project only. The user wants per-project **and** a shared global room
  for cross-project coordination + a single overview.
- **D — identity.** A token = a participant, so one human on three clients is three
  "people". Need a **Principal** above sessions: one human = one Principal across all clients;
  agents are Principals too; multi-user/remote slot in later.
- **E — observation.** The TUI shows one room at a time. The human needs a **multi-room
  overview** (what's happening everywhere) and equal access from any client.
- **F — wakeup parity.** Only interactive CC gets a true idle push; codex/opencode rely on
  polling / Tier-3 piggyback.

## Design

### 1. Identity — the `Principal` layer (the load-bearing addition)

A **`Principal`** is a stable identity that **many sessions/clients map to**:

```
Principal { id: PrincipalId, kind: Human | Agent, display: String, auth: AuthRef }
Session   { session_token, principal_id, client: Tui|Web|Telegram|Discord|Mcp|Remote }
```

- The daemon resolves every connection to a `Principal` (today's `session_token` becomes a
  *session* key, not the identity). A room's roster lists **Principals**, not raw sessions —
  so one human on TUI+web+Telegram appears once.
- **Local default**: with no auth configured, the local OS user is one implicit `Human`
  Principal (`$USER`), and each launched agent is an `Agent` Principal keyed by
  `(project, agent-name)`. Zero-config, single-operator — works today's way.
- **Pluggable auth (`AuthRef`)**: later, a client presents a credential (a token, an OAuth
  identity, a device key) that maps to a Principal. This is the seam multi-user + remote
  need; v1 ships the abstraction with the local-default resolver only.
- `roster.json` gains the session→principal binding; `principals.json` records known
  Principals (display, kind, auth). Backward compatible: an unbound session mints a
  Principal as today.

### 2. Rooms — project + global

- **Project room** (current): `(git root) → one canonical room`, auto-joined.
- **Global room**: a well-known named room (`commons`, configurable) every agent can also
  join and the human watches for cross-project activity. Auto-join policy is configurable
  (`rozum.toml [meeting] auto_join = ["project", "commons"]`); default = project always +
  global opt-in.
- Rooms already support named/ad-hoc creation (`rooms.new`) + discovery (`rooms.list`); the
  global room is just a daemon-level well-known room created on first use.

### 3. Agents join — both launch paths, zero-config

- **`rozum launch`** (already wires the proxy): keep, strengthen defaults (always register
  the proxy + auto-join project [+ global per config]).
- **Bare agents**: a one-time **global MCP registration** — `rozum mcp install [--agent
  claude|codex|opencode|all]` writes the `rozum` MCP server (= `rozum mcp-proxy`) into that
  agent's **user-level MCP config** (CC `~/.claude.json`/settings, Codex `~/.codex/config`,
  opencode config). Then even a bare `claude` run auto-joins. Idempotent; `rozum mcp
  uninstall` reverts. This is the "mix" path the user picked.

### 4. Coordination — instructions + proxy-emitted presence

- **Instructions (discretion, "when they need to")**: the proxy's `instructions` + the
  `rozum` etiquette skill, strengthened: *announce* `working: <what>` before going heads-down,
  *check* the room for a sibling on the same file before editing, *ask* when blocked, post
  `done:`/`blocked:` on finish. The agent decides when — guided, not forced.
- **Presence is emitted by the mcp-proxy itself, not per-client hooks** (decided 2026-06-18
  during build — strictly better than the CC-hooks-into-settings.json approach): on its first
  join the proxy `meeting.submit`s a `joined:` line, and on session end a `left:` line, **over
  the agent's own session** — so the presence line carries the agent's handle (unified with its
  messages), it works for **every** agent (not just Claude Code), and nothing edits the user's
  `settings.json`. The earlier CC `SessionStart`/`SessionEnd` hooks are removed (would
  double-post + only covered CC + edited a user config file).

### 5. Clients — equal front-ends over one contract

The daemon is the single source of truth; every client is equal:

| Client | Read | Write | Status |
|---|---|---|---|
| MCP proxy (agents) | disk tail + `wait_my_turn` | `meeting.submit` | shipped |
| TUI (`rozum`) | disk tail | `meeting.submit` | shipped (build now: multi-room overview) |
| Web (`rozum meetings web`) | daemon read API | daemon submit | shipped; legacy `rozum web` remains |
| Telegram / Discord | disk tail + `wait_my_turn` | `meeting.submit` | shipped as bridge Principals |
| Remote | day-scoped REST + authed submit | authed submit | P4 |

Each presents a `Principal` (its human's, or the agent's). The **client contract** =
`{ resolve Principal, join/list rooms, read transcript (disk or REST), submit, observe
turn/presence }`. New clients implement this; nothing privileged.

### 6. Observation + intervention

- **Multi-room overview (TUI, build now)**: a top-level view listing active rooms with
  last-activity + unread, drill into any; the existing per-room TUI + picker is the base.
- The human's submit is an ordinary message (their Human Principal as author); agents see it
  via `wait_my_turn`/channel push and react. No moderator, no turn-locking (per SPEC.md).

## Phased plan (each phase: spec-aligned, tested, merged)

- **P1 — coordination works locally, one operator (BUILD NOW).**
  - Global room + configurable auto-join (`[meeting] auto_join`); proxy joins project [+
    global].
  - `rozum mcp install/uninstall` for bare agents (CC first; codex/opencode best-effort) +
    the CC SessionStart/Stop coordination hooks.
  - Strengthen proxy `instructions` + the `rozum` etiquette skill; add an AGENTS.md/CLAUDE.md
    convention so project agents know to coordinate.
  - TUI multi-room overview.
  - Local-default identity (OS user = Human Principal; agent = Agent Principal) — the
    `Principal` type + resolver, roster binding, **no auth yet**.
- **P2 — `Principal` unifies one human across clients.** Session→Principal binding for the
  TUI + a configured local key, so the same human on multiple local clients is one identity.
- **P3 — web/Telegram/Discord as equal daemon clients.** Daemon web and the Telegram/Discord
  transports are built. Remaining: migrate the separate legacy `rozum web` escape hatch and
  resolve messenger sessions to the operator's Human Principal instead of a bridge Principal.
- **P4 — remote + multi-user.** Authenticated network transport (Principal `AuthRef`
  resolver), day-scoped REST read + authed submit; multiple remote humans.

## Decisions

- **Daemon is the hub; clients are equal + pluggable** — chosen so TUI/web/bridges/remote
  share one source of truth and one contract; no client is the "real" one. (Matches the
  daemon's single-writer/direct-read design.)
- **Identity is a `Principal` above sessions, local-default first** — chosen so one-human-
  many-clients and multi-user/remote are a resolver swap, not a re-architecture; ships
  zero-config single-operator today (the premature-abstraction guard: build the seam +
  the one real resolver, not speculative auth backends).
- **Coordination = strong instructions + proxy-emitted presence** — the model's discretion is
  the point ("when they need to"), and join/leave presence always fires from the mcp-proxy
  itself (one mechanism, every agent, unified handle, no user-config edits). Chosen over
  per-client lifecycle hooks (CC-only, dual-handle, edits `settings.json`).
- **Per-project + one global room** — within-project coordination stays local to the repo;
  the global room is the cross-project channel + the human's single overview.

## Non-goals (v1)

- Turn-taking / moderation (SPEC.md: anyone posts any time, no moderator).
- Full multi-user auth / remote transport (P4 — only the `Principal`/`AuthRef` seam now).
- Mapping Telegram/Discord accounts to the operator's Human Principal; the daemon-backed
  transports currently preserve the stable external sender ID in message content and use a
  bridge Principal.

## Agent participants: `agent-participant` vs `participant`

Two kinds of automated room member, both joining via the same daemon and sharing one
join/poll/reply-policy loop (`crates/rozum-meeting/src/meeting/participant_loop.rs`):

- **`rozum meetings participant`** — a CHAT model: answers by calling the local
  gateway's `/v1/chat/completions`, optionally with file/shell tools confined to
  `--sandbox <dir>` (`sandbox_tools.rs`).
- **`rozum meetings agent-participant`** — a real CODING AGENT (`claude` by default,
  or `nadia`/`codex`/`opencode`): each reply is a full `rozum launch --model
  <local-spec> --no-room-bridge <agent> -p <prompt> …` subprocess run with real
  file/shell access in a **working directory**, not a chat completion. Workdir
  defaults to `~/.local/state/rozum/agent-rooms/<sanitized-room>` (stable across
  restarts) or an explicit `--workdir`.

Because the Telegram/Discord bridges are agent-agnostic (they relay whatever is in a
room), pointing a chat at an `agent-participant`'s room (`/addgroup`, or
`TELEGRAM_EXTRA_CHATS`) is the whole integration — no bridge code changes needed.

**This is a real risk increase over a chat-only participant**: a permitted sender can
make the agent edit files autonomously, no per-action prompts (same headless shape as
`coders.rs`'s existing UCC jobs). Bounded by two independent things: the Seatbelt jail
`rozum launch` puts the agent under by default (`docs/specs/model-sandbox.md` —
confined to the workdir + toolchain caches, no write outside it, loopback-only
network), and `--acl <path>` gating WHO can even trigger a turn (checked against the
sender's `shell` capability). Verified end-to-end 2026-08-28: a room message asking to
create a file produced a real file in the workdir and a natural-language confirmation
back in the room, via the real Telegram-facing meeting daemon.

## Open questions

- Global room: should an agent be in its project room AND `commons` *simultaneously* (needs
  the daemon's single-room session → multi-room), or is the env-selected single room enough?
- Unifying the human across web/Telegram/remote: the `Principal` `AuthRef` resolver (P3/P4) —
  shape it from real multi-client use.

## Status (build progress, 2026-07-20)

- **P1.1** post transport (`rozum meetings post`) + author shown in the transcript — DONE.
- **P1.2** shared room via `ROZUM_MEETING_ROOM` (proxy + post honor it) — DONE; true
  simultaneous multi-room deferred (daemon model change).
- **P1.3** `rozum mcp install/uninstall` (claude+codex via their own `mcp add`) — DONE.
- **P1.4** coordination contract in the proxy `instructions` + AGENTS.md — DONE.
- **Presence** emitted by the mcp-proxy (`joined:`/`left:`, every agent, unified handle) — DONE;
  superseded the CC settings.json hooks.
- **P1.6** stable local identity (`rozum identity whoami/set-name`, `~/.config/rozum/identity.json`)
  — DONE (local-default `Principal`).
- **P1.5** multi-room TUI dashboard — the enriched room picker covers visibility; a dedicated
  overview is interactive-shaped polish.
- **P3 transport** — daemon web and Telegram/Discord are DONE. Messenger bridges use thin,
  engine-free commands, join existing daemon rooms as `kind=bridge`, export no startup
  history, and enforce sender allowlists. The legacy `rozum web` escape hatch remains.
- **P2/P3 identity + P4** — cross-client Human Principal unification, auth, and remote
  multi-user remain shaped by dogfooding.
