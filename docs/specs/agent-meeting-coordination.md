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
- **Human/other clients today**: the TUI (`rozum` / `rozum meetings attach`); legacy
  Telegram/Discord/web bridges (built against the *legacy* in-process room, not the daemon).

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

### 4. Coordination — instructions + hard hooks

- **Instructions (discretion, "when they need to")**: the proxy's `instructions` + the
  `rozum` etiquette skill, strengthened: *announce* `working: <what>` before going heads-down,
  *check* the room for a sibling on the same file before editing, *ask* when blocked, post
  `done:`/`blocked:` on finish. The agent decides when — guided, not forced.
- **Hard hooks (always-fire lifecycle points)** — Claude Code hooks (`SessionStart`, `Stop`,
  optionally `SubagentStop`): `rozum mcp install` also installs hooks that
  - **SessionStart** → `meeting.submit` a `joined: <cwd/task>` presence line (so the human
    + siblings see the agent arrive), and
  - **Stop** → `meeting.submit` a `done:`/`idle:` line (so the room reflects it stopped).
  These are the two points that must not depend on the model remembering. Codex/opencode:
  no equivalent hook system → instructions-only there (documented), with the proxy's
  auto-join covering "joined".

### 5. Clients — equal front-ends over one contract

The daemon is the single source of truth; every client is equal:

| Client | Read | Write | Status |
|---|---|---|---|
| MCP proxy (agents) | disk tail + `wait_my_turn` | `meeting.submit` | shipped |
| TUI (`rozum`) | disk tail | `meeting.submit` | shipped (build now: multi-room overview) |
| Web | (daemon read API) | (daemon submit) | rebuild on daemon (P3) |
| Telegram / Discord | daemon read | daemon submit | rebuild on daemon (P3) |
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
- **P3 — web/Telegram/Discord as equal daemon clients.** Rebuild the bridges on the daemon
  (not the legacy room), each presenting a Principal; the deferred daemon read/submit API.
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
- **Coordination = strong instructions + two hard hooks** — the model's discretion is the
  point ("when they need to"), but join/leave presence must always fire, so hooks cover
  exactly those two points; not blanket per-tool hooks (noise).
- **Per-project + one global room** — within-project coordination stays local to the repo;
  the global room is the cross-project channel + the human's single overview.

## Non-goals (v1)

- Turn-taking / moderation (SPEC.md: anyone posts any time, no moderator).
- Full multi-user auth / remote transport (P4 — only the `Principal`/`AuthRef` seam now).
- Replacing the legacy bridges before P3 (they keep working against the legacy room until
  rebuilt on the daemon).

## Open questions

- Global room name + default auto-join policy (`commons`? project-only by default with an
  opt-in flag, or both by default?).
- Codex/opencode hook story: confirm neither has a usable session-lifecycle hook; if so,
  document instructions-only for them (the proxy auto-join still covers presence).
- Hook transport: the SessionStart/Stop hook needs to `meeting.submit` — call the daemon
  socket directly via a tiny `rozum meeting post <text>` CLI (no MCP round-trip).
</content>
