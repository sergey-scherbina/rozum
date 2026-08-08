# Evidence: the part of an incident that is not in the room

Status: implemented 2026-08-08. Extends `thread_context` in
`crates/rozum-meeting/src/meeting/store.rs`. Companion to
[`incident-resolving.md`](incident-resolving.md).

## What exists, and what a responder still digs for

`thread_context` already assembles everything the ROOM knows: the thread record, every message in
it, the participants, the timespan, the messages an operator linked by hand, and the lead-up before
the anchor. That is most of `mtg-incident-context` and it was built before this spec.

What it cannot answer is the question a responder actually opens an incident with: *what was the
machine doing at the time?* Today that means leaving the room — `launchctl list`, a service probe,
`~/.rozum-gateway.log`, which binary was running — and every one of those was a step in each of the
five incidents this project recorded in August. The evidence exists; it is just not attached.

## 1. The log slice comes from the incident's own timespan

An incident runs from `created_ts` to `resolved_ts` (or to now, while it is open). The gateway log
covers that window, and since 2026-08-08 its start lines are dated
(`docs/specs/service-liveness.md`) — which is what makes a slice meaningful at all. `thread_context`
attaches the lines inside that window, capped, oldest first.

**It starts before the incident does.** The window opens five minutes ahead of `created_ts`,
because an incident is always filed *after* its symptom: a window starting at the moment someone
typed the report begins just after the thing they are looking for. A minute-old incident would
otherwise carry a one-second slice.

**Capped and said so.** A slice that silently truncates is a slice that misleads: the bundle carries
how many lines matched and how many are shown. A responder who sees `showing 200 of 4,812` knows to
go and look; one who sees 200 lines does not.

## 2. The machine snapshot is taken when the incident is OPENED

Logs can be sliced afterwards because they are history. The state of the machine cannot: by the
time anyone reads the incident, the services have restarted, the binaries have been replaced, and
the answer to "what was running" is gone. So opening an incident-kind thread writes one `event`
message into it carrying that snapshot — service verdicts and the identity of the binaries serving
them.

It is a MESSAGE, not a side file: it lands in the transcript, it is part of the thread by
construction, `repair-threads` rebuilds it with everything else, and it cannot rot out of sync with
the incident it belongs to.

**Bounded, and it never holds the room.** The snapshot probes services, and a probe is slowest
exactly when the machine is sick — which is when incidents get opened. So the room lock is dropped
before it runs, and it runs under a 20s budget: an incident that waits a minute to be filed is worse
than one filed with a partial snapshot.

**Best-effort, and honest about it.** If the snapshot cannot be taken — no launchd, no doctor, a
probe that times out — the message says what failed rather than being skipped, because a missing
snapshot and a healthy machine look identical in an empty bundle.

## 3. Where a responder sees it

`meeting.thread_context` carries `log_slice` (path, window, `matched`, `shown`, lines) for agents
and the console; `rozum meetings incident show` prints the tail of it under the transcript, with the
counts, so the shell twin is not the poor relation. The snapshot needs no rendering — it IS a
message, so every surface that shows the timeline already shows it.

## 4. Nothing here reaches outside the room without being asked

The evidence is gathered when an incident is opened and when its context is read. There is no
background collector, no watcher, nothing that samples a machine because a thread exists. The
liveness check already watches the machine on a schedule and has the discipline for it; this is a
bundle for a person or an agent who has just picked up an incident.

## What this does not do

No workdir capture and no repro bundle — those need a policy about what may be copied out of a
working tree, and inventing one inside an incident feature is how a support tool grows a data-export
problem. Recorded in `BACKLOG.md` rather than half-built.
