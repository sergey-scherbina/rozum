# Resolving: what a thread's numbers must mean

Status: implemented 2026-08-08. Contract for `crates/rozum-meeting/src/meeting/store.rs`
(`Thread`, `thread_metrics`) and everything that reads them — CLI, REST, MCP, the console.

## What already exists, and why this spec is short

`mtg-resolving` asked for "an incident state machine (open → triaging → escalated → resolved →
closed), resolution records, reopen, and metrics (time-to-resolve, escalation rate)". Reading the
code before writing any: **the state machine, escalation with an assignee and a note, resolution
records as a `resolution`-kind message, and a metrics endpoint all exist and are tested.** The
backlog entry had aged past its own subject.

What is left is small, and one part of it is worse than missing — it is wrong.

## 1. Time-to-resolve must be measured to the resolution

`thread_metrics` computes `updated_ts - created_ts` for terminal threads. `updated_ts` moves on
**any** change: a later message in the thread, a pin, a link, an owner change. So a thread resolved
in four minutes and commented on the next morning reports a time-to-resolve of a day.

A number that grows while nothing happens is worse than no number: it will be read as a trend and
acted on. The thread records **when it reached a terminal state** (`resolved_ts`), and the metric
uses that.

## 2. Reopen is a fact, not an absence

Setting a resolved thread back to `open` is already possible and already the right gesture. Nothing
records that it happened, so a thread reopened three times looks exactly like one that was solved
first time — and "solved first time" is the thing anyone measuring resolution actually wants to
know.

A thread carries `reopened: u32`. Reopening clears `resolved_ts`: the clock restarts, because the
work restarted. The next resolution measures from the ORIGINAL creation — an incident that took
three tries took as long as it took, and hiding that would flatter the number.

## 3. Escalation rate

`by_state` counts threads sitting in `escalated` *now*. That is not the escalation rate: a thread
escalated and then resolved leaves no trace in it. Threads record `escalations: u32`, incremented
on each transition INTO `escalated`, and the metric reports how many threads ever escalated over
how many exist.

## What this does not do

No SLA timers, no per-owner leaderboards, no alerting on the numbers. Metrics here answer "how is
this room doing" for a person already looking; anything that pages someone belongs with the
liveness check (`docs/specs/service-liveness.md`), which has the discipline for it — confirm before
you shout.

## Compatibility

Existing `threads.json` files have none of the new fields. They read back as `resolved_ts: None`,
`reopened: 0`, `escalations: 0`, and a thread already terminal keeps reporting its old
`updated_ts`-based duration until it moves again — stated here so nobody reads a mixed history as a
change in behaviour. `repair-threads` (the rebuild-from-log path) reconstructs the new fields from
the message log, which is where the state transitions are recorded anyway.
