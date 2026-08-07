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

**CORRECTION, 2026-08-07 — this section was written without noticing that the field already
existed.** `store.rs` carried `RoomKind { Chat, Queue, Incident }` and `Member { handle, role }`
from an earlier phase (marked "P3"), with working setters that **nothing ever called** — no daemon
tool, no CLI, no REST — so no room ever had either field (0 of the operator's 14 `meta.json` files).
The argument below is unchanged and still holds; what changes is that it is an argument for
REMOVING something built, not for declining to build it. Both were removed on the operator's call,
and the second one matters more: R1 had already added roles to `RosterEntry`, so the codebase
briefly had TWO role mechanisms — exactly the drift this spec warns about, introduced by this spec's
own author.

**Recommendation: do NOT keep `kind: chat | queue | incident` as a stored room field.** Instead:

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

### R2 — make the room's lifecycle survive a restart, and add `Paused`

**REWRITTEN 2026-08-07. The version above was wrong in its premise, and what replaces it is a bug
fix rather than a feature.** What measuring found:

- **A room's phase is not persisted at all.** `DaemonRoom::end()` sets `Phase::Ended` in memory and
  emits an event; `meta.json` carries `phase` as a plain **String**, initialised `"Active"`, with no
  setter anywhere. So an ended room comes back ACTIVE after a daemon restart, silently.
- **There are two `Phase` enums.** `state.rs` has `Active | Paused | Ended` with working
  pause/resume — for the TUI's in-memory session — and `room.rs` has `Active | Ended` for the
  daemon-hosted room. `Paused` already exists; it just does not exist where rooms live, and is not
  saved either.
- So the migration described above — "a serde alias on the enum" — **cannot apply**: nothing
  deserialises that field into an enum. It is a string, and the work is to start reading it.

**What R2 is, then:**

1. Persist the daemon room's phase through the writer, fixing the restart amnesia.
2. Give `room.rs::Phase` a `Paused` variant, reaching the model that rooms actually use.
3. Parse the on-disk string back into the enum with a deliberately SAFE fallback: an unrecognised
   value logs a warning and reads as `Active`. Erring towards "the room works" is recoverable;
   erring towards `Ended` silently stops a room nobody can see is stopped.

**`Resolved` and `Archived` are dropped.** I invented them for hypothetical listing behaviour and
there is no consumer for either. Building them would repeat exactly what was removed from this
codebase earlier the same day: a persisted concept with no surface that reaches it.

### R3 — the queue view — DONE 2026-08-07

`store::room_queue(root, now) -> Vec<QueueItem>`: open threads, severity-then-least-recently-updated,
`stale` and `overdue_secs` already computed. Daemon tool `meeting.queue`, `rozum meetings queue`,
`GET /rooms/{n}/queue`. No new storage.

Two decisions worth keeping:

- **It calls `thread_is_stale`/`sla_secs` rather than computing its own SLA.** A third place holding
  one piece of state is what this task spent the day deleting.
- **`now` is a parameter, not a clock read inside**, so the ordering is testable without waiting for
  time to pass.

The assignee is left as the handle rather than resolved through the roster: in a room, the handle IS
the identity, and a second copy of the participant record inside a queue row would go stale the
first time somebody is renamed.

**Honest limit on the verification:** no room on this host has a single thread — the incident
machinery exists and has never been used with real data — so the queue could only be exercised
against a `threads.json` written for the purpose. Unit tests cover the ordering, the SLA arithmetic
and the "closed is not queued" filter; nothing has yet proven it against organically created data.

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
