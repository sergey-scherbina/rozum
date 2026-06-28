# Spec: meetings → product-support / incident platform (foundation)

Status: 2026-06-28 — DRAFT (owner: `pipeline-swap-settle`). The strategic direction (operator): rozum
meetings become **product support with escalation + resolving + per-incident context collection**, with
AI agents as first-class participants. Builds on the existing meeting stack (`agent-meetings-daemon.md`,
`meeting-identity-roster.md`, `meeting-mention-inbox.md`, `meetings-rest-read.md`; daily disk-backed
rooms, session-token identity, single-writer daemon + direct-read). This spec designs the **data-model
foundation** that the support verbs (ops / escalation / resolving / incident-context) build on. BACKLOG:
the `## Meetings → product-support / incident platform` section.

## Why a foundation spec first

Every support verb (assign, escalate, resolve, search, gather-context) needs two things the current model
lacks: **structured message metadata** and **threads** (an incident = a thread). The current message is
`StoredTurn{date, n, participant_id, display_name, content, ts}` (a flat daily line). Bolting verbs onto a
flat log is the wrong order — design the typed model once, back-compat, then the verbs are thin.

## Hard constraint: back-compat + the single writer

- **Plain rooms keep working.** Old `StoredTurn` lines (no metadata) must still parse + render. New fields
  are `#[serde(default)]` — a v1 reader sees them as absent, a v1 line reads as a plain note. No migration
  forced; `index.json` stays a rebuildable accelerator.
- **Single-writer invariant holds.** The daemon is the only writer (`agent-meetings-daemon.md`); metadata
  writes/edits go through it. Direct readers (TUI/REST) read the same files. No new lock surface.
- **Agent-native.** Every verb is an MCP tool + a TUI/REST surface, so an AGENT can triage/escalate/resolve/
  gather-context, not just a human. This is the leverage (an agent assembles an incident's context bundle).

## The model (the hinge)

### Message — extend `StoredTurn` (P1)
Add, all `#[serde(default)]`:
- `id: String` — a stable per-message id (`<date>/<n>` is already unique; surface it as the id). The anchor
  for reply/edit/react/resolve/link.
- `kind: MsgKind` — `note` (default) | `question` | `event` | `alert` | `resolution`. Drives rendering +
  filtering. Default `note` so old lines read as notes.
- `thread_id: Option<String>` — the incident/topic this belongs to (None = the room's main stream).
- `in_reply_to: Option<String>` — a reply edge (message id), for reply-chains within a thread.
- `meta: MsgMeta` — `{ severity: Option<Sev>, status: Option<MsgStatus>, assignee: Option<String>,
  tags: Vec<String>, links: Vec<String> }` (links = artifact/log refs / other message ids). All optional.
- `edited_ts / redacted: Option<...>` — for edit/redact (P-later), append-only (a new line supersedes by id;
  never rewrite history — the daily file stays append-only, the reader resolves latest-by-id).

### Thread — a new per-room artifact (P2)
A thread IS an incident/topic. `Thread { id, room, title, kind: ThreadKind(topic|incident), state:
ThreadState(open|triaging|escalated|resolved|closed), owner: Option<String>, severity, created_ts,
updated_ts, sla: Option<...>, message_ids: Vec<String> }`. Stored in a per-room `threads.json` (or a
`threads/` dir), rebuildable from the daily lines' `thread_id` (like `index.json`). State transitions are
the resolving state machine (P5 of the BACKLOG).

### Room — extend `Meta` (P3)
Add `kind: RoomKind` — `chat` (default, today's behavior) | `queue` (a support intake) | `incident` (a
room scoped to one incident). Plus `members: Vec<Member{handle, role: reporter|assignee|oncall|observer}>`.
Default `chat` + empty members → today's flat room is unchanged.

## Phases (each its own build, gated, back-compat)
- **P1 — message metadata.** Extend `StoredTurn` (serde-default fields), thread the id + kind + meta
  through the write path (daemon) + the read/render path (TUI/REST/MCP). Acceptance: a plain room is
  byte-identical to today; a message posted with `kind/severity/tags` round-trips; an old line reads as a
  note. THIS is the hinge — do it first, alone, prove zero regression on plain rooms.
- **P2 — threads.** `thread_id`/`in_reply_to` + `threads.json` (rebuildable) + a thread-aware reader. A
  message can open/join a thread; the reader can show a thread as a unit.
- **P3 — room kinds + members.** `RoomKind` + members/roles on `Meta`.
- **P4+ — the verbs** (separate specs): message-ops (search/link/react/edit/resolve/assign/pin),
  escalation (route by severity/tier/on-call → an agent/stronger-model/human; ties into the model-chain),
  resolving (the thread state machine + metrics), incident-context (auto-gather logs/obs/repro/related into
  a thread — the highest agent-leverage piece; an agent assembles the bundle).

## Non-goals (for the foundation)
The verbs themselves (P4+) — designed separately once the model lands. Not a ticketing UI; not a new
storage engine (extend the daily-file + json-accelerator model). Not breaking plain chat rooms — ever.

## Acceptance for the foundation (P1–P3)
Plain rooms unchanged (byte-identical daily files for a metadata-less post); a metadata-rich message +
a thread round-trip through write→file→read on the daemon; the TUI/REST/MCP surfaces the new fields without
breaking old clients; `index.json`/`threads.json` rebuild from the daily lines (the lines are the source of
truth, the json files are accelerators).
