# An agent's record must outlive the process that ran it

Status: implemented 2026-08-05. Contract for `crates/nadia` (`serve`, `supervisor`) and the
Telegram bridge in `crates/rozum-meeting`.

## Why

The operator asked what was in an empty directory, `~/.nadia/tasks/2026-08-05-002343-`. The honest
answer was: it cannot be established. An agent had been started there — that path is only ever
created by a spawn — and every trace of what it did is gone, because

- agents live only in the memory of `nadia serve`,
- `nadia serve` is a plain child of the Telegram bridge, and
- a deploy that restarts the bridge kills it.

That evening it happened twice, both times by my own hand, and neither time was it visible: the
only symptom was that the next agent came back as `#1`.

nadia's own documentation already states the rule this violates — `serve` "is not a background
service: one that restarted under them would silently lose their work". The rule was written about
running the process as a service and nothing enforced it against the process that *starts* it.

Three separate costs, and only the first is obvious:

1. **The record dies.** `/status` and the result message have nothing to report, and a run that was
   in flight is never delivered to anybody.
2. **Ids restart at 1.** The Telegram watcher carries a whole `Reused` branch precisely because a
   restarted `serve` hands the same small integer to different work; a delivery keyed by id alone
   would post one operator's result into another's chat.
3. **The evidence dies with it.** A question about what an agent did has no answer, which is how a
   defect stops being investigable.

## Contract

### 1. Records are on disk, not only in memory

`nadia serve` keeps one record per agent under `~/.nadia/.agents/<id>.json`, written when the agent
starts, when the gate reports, and when it reaches a terminal phase. At startup every record is
loaded back, so `/agents` and `/agents/{id}` answer for work this process did not run.

A record whose phase is not terminal when it is loaded did not finish — the process that was running
it is gone. It is marked **`interrupted`**, which is a phase of its own: not `done` (nothing says it
finished), not `failed` (nothing says it failed), and above all not absent. *Unverified is reported
as unverified* is the same rule the verify gate lives by, one layer out.

The next id continues past the highest loaded record. Ids are then unique for the life of the
machine rather than the life of a process, which retires the reuse hazard rather than surviving it.

### 2. `serve` outlives the bridge

The bridge starts `nadia serve` in its own process group. A `launchctl bootout` of the bridge
signals the bridge's group; a child in another group is not in it, so the agents keep working
through a deploy of the thing that happens to have started them.

`ensure_running` already reuses a live `serve` over `/health`, so a restarted bridge reattaches to
the running one instead of starting a second.

### 3. An orphan must not outlive its binary

Detaching creates the opposite hazard: after `cargo install`, a still-running `serve` keeps serving
the old code, and the operator sees a fix that was deployed and did nothing — which is exactly the
confusion BUG-022 caused for one deploy, and it must not become permanent.

So `/health` reports the build actually running (`exe` and its mtime), and the bridge compares it
with the installed binary at startup:

- **stale and idle** → restart it, so a deploy takes effect;
- **stale but agents are working** → leave it alone and say so once. A deploy is not worth killing
  the operator's work for; the next idle moment gets it.

This ordering is the whole point: *the operator's work outranks our convenience about versions.*

## What this does not do

It does not make agents survive their own process — a killed `serve` still stops the work. It makes
the *record* survive, and says `interrupted` where it cannot say more. Resuming an interrupted agent
needs the transcript persisted too, which is a bigger change and is in `BACKLOG.md`.
