# Escalation: resolve on-call, and stop claiming an escalation that did not happen

Status: spec (2026-08-07)
Owner: `mtg-escalation`
Depends on: `mtg-rich-rooms` R1 (roster roles) and R3 (the queue), both shipped 2026-08-07

## The defect, which is worse than the missing feature

`meeting.escalate` takes an optional free-text `to`. When it is absent
(`daemon.rs`, the `escalate` tool):

```rust
let to = p.to.clone().unwrap_or_else(|| "on-call".into());
…
format!("escalated to {to}")
```

The literal string `"on-call"` goes into the audit message, and `set_thread_owner` is **never
called**. So the room records "escalated to on-call", the thread's owner stays empty, and nobody is
responsible for an incident that everyone can now see was escalated. It reads like a page that went
out. Nothing went out.

That is the thing to fix. "Cannot resolve who is on call" is the feature; "says it did" is the bug.

## What already exists and must not be rebuilt

- **The audit trail.** Every escalation posts an `Event` message into the thread carrying
  `thread_op { state, owner }`, authored by the caller, in the append-only log. Who and when are
  already recorded — a second audit store would be a second truth.
- **Roles.** `RosterEntry.roles` includes `OnCall`, and `Roster::with_role` returns **everyone**
  holding it.
- **Load.** `store::room_queue` lists open threads with their owners, so "who is carrying the least"
  is a question the data already answers.

## The decision this spec exists to make

`with_role(OnCall)` deliberately returns a list, because two people on call is normal and the roster
refused to pick one — that choice belongs here. So: **when several are on call, who gets it?**

| Option | Why not / why |
|---|---|
| Name them all, assign nobody | Honest, and the worst outcome in practice: an incident everyone can see and nobody owns is the bystander effect with an audit trail. |
| First in roster order | Deterministic and arbitrary. The same person is paged every time, which is how one on-call quietly becomes the only on-call. |
| **Least open work, ties broken by handle** | **Chosen.** The data exists (`room_queue`), the rule is explainable to the person paged, and it self-balances. Deterministic given the same room state, so it is testable. |

**When nobody is on call, the escalation still happens — state becomes `Escalated` — but the owner
stays empty and the message says so plainly**: `escalated, but nobody is on call in this room`. An
escalation that quietly does nothing is exactly today's bug; refusing outright would be worse, since
the state change is the operator's signal that something needs attention.

An explicit `to` always wins over the policy. Someone naming a target knows something the roster
does not.

## Out of scope, with reasons

- **"Escalate to a stronger model."** The entry mentions it, and it needs the model-chain: a
  different subsystem, and untestable on this host, which is frozen on one model. Routing to a
  PERSON is what the roles were built for, and it is the half that works today.
- **Escalation tiers / policies per severity.** No consumer asked for one, and the last two
  speculative concepts in this subsystem were deleted the day they were found unreachable.
- **Notification delivery** (paging a phone, a bridge message). That is the messenger's job and it
  already has a room-to-Telegram path; wiring the two is its own task.

## Verification

- Nobody on call → state escalates, owner stays `None`, and the message says nobody is on call.
- One on call → that participant becomes the owner.
- Several on call → the one with the fewest open threads in this room; a tie goes to the lower
  handle, so the result is stable.
- An explicit `to` overrides all of the above.
- The audit event still carries `state` and the resolved `owner`, so the trail records what actually
  happened rather than what was requested.
