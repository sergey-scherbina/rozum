# Rich rooms: lifecycle, kind, and membership roles

Status: spec (2026-08-07) — **plan for review; no storage change until the operator has seen it**
Owner: `mtg-rich-rooms`
Backlog entry: "a richer room model beyond the daily-file chat … the hinge the rest hangs on"

## What exists today, measured rather than remembered

The backlog entry says "Today: one flat daily room per project." That undersells it by a lot, and
the difference decides how much of this task is real.

**Per room, on disk:**

| File | Contents |
|---|---|
| `meta.json` | `{name, topic, project, phase, created_at, budget_chars}` |
| `roster.json` | participants: `{id, handle, base_name, kind, project, session_token}` |
| `YYYY-MM-DD.jsonl` | the append-only message log, one file per day |

**Already present, and it matters:**

- **A lifecycle field.** `Phase` is `Active | Ended` (`room.rs:63`).
- **A participant record with durable identity** — a UUID, a room-unique handle, a reconnect token.
- **Threads with everything the entry wants a ROOM to have**: an id, an anchor message, a title, a
  `kind` (`topic | incident`), a state machine (`open → triaging → escalated → resolved → closed`),
  an assignee, pinning, links, and per-severity SLA windows with a staleness metric.

**Genuinely missing:**

1. **Room kind** — nothing distinguishes a chat room from a support queue from an incident room.
2. **Roles.** `RosterEntry.kind` is `mcp | human | bridge`, which is *what kind of client this is*,
   not *what this participant is here to do*. There is no reporter / assignee / on-call / observer.
3. **Lifecycle beyond two states.** `Active | Ended` cannot express a queue that is paused, or an
   incident room that is resolved but kept for reference.

## The question this spec exists to answer

**Does a room need a `kind` at all, when threads already have one?**

An incident today is a thread: it has a state machine, an owner, an SLA, and links. A room holding
incident threads already behaves like an incident room. So "room kind" risks being a second, weaker
copy of a concept that already works — and the failure mode is specific and familiar: two places to
set state, which drift, and a reader who cannot tell which one is authoritative.

**Recommendation: do NOT add `kind: chat | queue | incident` as a stored room field.** Instead:

- A **queue** is a VIEW: "open threads in this room, ordered by severity then age, with the stale
  ones flagged". Every input already exists — `thread_metrics` computes staleness today. This is a
  read model, and read models cannot drift from the data they read.
- An **incident room** is a room whose threads are incidents. If that distinction ever needs to be
  declared rather than observed, it belongs in `meta.json` as a **default** for new threads
  (`default_thread_kind`), not as a room type that constrains what the room may contain.

This is the smaller half of the entry, and it is the half that would have been wrong to build big.

## What IS worth building, in order

### R1 — roles in the roster (the real gap)

Add `roles: Vec<Role>` to `RosterEntry`, defaulting to empty. `Role` is
`Reporter | Assignee | OnCall | Observer | Admin`.

- **Why roles and not one role:** the operator is on-call AND the assignee of two incidents. A
  single-valued field forces a lie the first time that happens.
- **Additive and back-compatible:** `#[serde(default)]` means every existing `roster.json` loads
  unchanged with an empty vector, which reads as "no declared role" — the status quo — rather than
  as a wrong role.
- **What consumes it:** `meeting.escalate` currently takes a free-text `to`. With roles it can
  resolve "on-call" to a participant, which is the whole point of `mtg-escalation` and the reason
  this entry is called the hinge.

### R2 — lifecycle states that a queue can actually be in

Extend `Phase` to `Active | Paused | Resolved | Archived`, keeping `Ended` as a deserialisation
alias for `Archived` so existing `meta.json` files load.

- `Paused` — the room exists, is readable, and accepts no new messages. Today the only way to stop
  a room is to end it, which is destructive-feeling and nobody does it.
- `Resolved` vs `Archived` — resolved keeps it in listings (it is recent and referenced), archived
  drops out of the default list.
- **The migration is a rename with an alias, not a rewrite.** No day file is touched.

### R3 — the queue view

`store::room_queue(root) -> Vec<QueueItem>`: open threads, severity-then-age ordered, staleness
flagged, assignee resolved through the roster. Surfaced the same three ways every feature in this
subsystem is: daemon tool, CLI, REST. No new storage.

## Migration, stated plainly

**Nothing rewrites a day file, and nothing rewrites `rooms.json`.** Both changes are additive fields
on JSON records that already deserialise with `serde`:

- `RosterEntry.roles` — `#[serde(default)]`, absent means empty.
- `Phase` — a new enum with `#[serde(alias = "Ended")]` on `Archived`.

An older binary reading a newer `roster.json` ignores `roles`, and a newer binary reading an older
one sees an empty vector. There is no flag day and no down-migration to write, which is the only
reason this is worth doing while a live daemon serves 15 rooms.

**Verification before anything ships:** a test that loads the operator's ACTUAL `meta.json` and
`roster.json` shapes — the pre-change bytes, checked in as fixtures — and asserts they still
deserialise. Not a synthetic record; the ones on this host.

## Out of scope, deliberately

- Room kind as a stored type (see above — the recommendation is not to).
- Any change to message storage. `mtg-message-metadata` covers that and is separate.
- Escalation policy itself. R1 gives it the roles it needs; `mtg-escalation` is where it belongs.
- A room registry rewrite. `rooms.json` stays `{name, root, project}` — the per-room `meta.json` is
  where room facts live, and splitting them across two files is how they get out of sync.
