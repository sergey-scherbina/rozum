# Spec: residency admission queue — event-driven, priority, preemptive

Status: 2026-06-28 — DRAFT (owner: `pipeline-swap-settle`). Extends the v2 RAM-ledger
(`docs/specs/safe-multi-model-residency.md`, `crates/rozum-core/src/share.rs`) and the
reboot fix (`gateway-residency-singleflight.md`). Closes the contention-jetsam hole that
`residency-gate-cap-mlx-sibling-aware` (BACKLOG) only half-addresses.

## Problem (data-backed)

Today admission is **check-then-spin**: a loading gateway grabs `residency.lock`, checks the
ledger + actual-free-RAM (`share::admits`), and if it doesn't fit it **polls every 2 s for up
to 240 s** ("Waiting … for it to free"), then loads or refuses. Independent processes race;
whoever loads first wins, and a sibling that loads **after** I'm admitted can push the host
into overcommit → **jetsam kills a live gateway**.

Proven 2026-06-28 (isolate): a Qwen3-4B matrix showed **19 dead cells** (codex+opencode,
`0.0s/agent=0MB`). I first blamed `clients_gone` (gateway self-exit) — **wrong**: replay
probes proved the gateway stayed alive (pid stable + `/v1/models` OK, 4×). Re-running the
SAME matrix on a **free** host → **0 dead cells**, valid pass-rates. Root cause = a concurrent
`green-matrix` sibling loading models → memory pressure → jetsam/launch-disruption. The ledger
admitted each load against a momentary snapshot; nothing **coordinated** or **ordered** them,
and nothing let an interactive load **preempt** a batch sibling. Brute force, not a queue.

## The hard invariant (unchanged)

> At no instant may the sum of resident models' real memory exceed the safe host budget —
> violations **reboot the Mac** (vm-compressor watchdog panic). Safety stays **structural**
> (admission), every estimate **conservative**. The queue must never admit into overcommit.

## Design — event-driven admission QUEUE over the existing ledger

Keep what works: the **daemon-less, lock-based, crash-safe** ledger (readable
`residents/<pid>` metadata + `residents/.<pid>.lock` lifetime sidecar + `residency.lock`;
the OS releases locks on death incl. SIGKILL — no stuck live reservation, no SPOF).
Add coordination **on top**, same style.

**Recommendation change (A→B), honest:** in discussion I leaned toward a **broker daemon** (A)
for a clean queue + socket callbacks. After reading `share.rs` I switched to **distributed (B)**:
a daemon owning the budget is a **single point of failure** — if it dies, no gateway can load,
and we'd lose the flock auto-release crash-safety that makes the current design reboot-proof.
B keeps that property and still gives a real queue + event-driven wait. A stays the fallback if
B's cross-process ordering proves too racy.

### 1. Wait queue (ordered, crash-safe)
A waiter that doesn't fit **enqueues** instead of spin-polling: write the lock-held
`waiters/<prio>.<seq>.<pid>.<footprint>` ticket (the body repeats footprint for legacy readers).
Publishing footprint in the filename is required on Windows, where another process cannot read
through the live ticket lock. Order = `(prio, seq)`. A waiter is **eligible to try** when it is the
lowest `(prio, seq)` whose footprint fits the *current* free budget. Serialized under
`residency.lock` so two waiters cannot both pass.

### 2. Event-driven wake (kqueue, no poll)
Replace the 2 s/240 s poll with a `kqueue` (`EVFILT_VNODE`) watch on `residents_dir()` +
`waiters_dir()`: a resident **freeing** (file removed) or the queue changing wakes blocked
waiters, which re-evaluate "am I next AND does it fit now?". `async`/tokio: the load path
`await`s a `tokio::sync::Notify` fed by the kqueue thread — the gateway never blocks its
runtime. This is the same admit-then-queue pattern we already run **in-process**
(`concurrency.rs::admits_up_to_limit_then_queues`), lifted **cross-process**.

### 3. Grant = ledger AND actual-free-RAM (sees non-participants)
The grant check stays `share::admits` (v3): admit iff **ledger** (`in_use + footprint ≤ budget`)
**AND** `footprint + min_free ≤ actual_free_now`. The actual-free term is what catches
**non-participants** — the `uv mlx_lm` oracle and any non-rozum RAM — that a pure ledger can't
see. Conservative peak reservation (cache-dominated) depends on **smmr-D** (below).

### 4. Priority + cooperative preemption
`prio`: `interactive` (a `rozum launch` agent request) **>** `batch` (a bench/matrix sweep), tagged
at gateway start (env/launch flag → the waiter/resident file). FIFO within a tier. **Preemption:**
a high-prio waiter that still doesn't fit after others drain may write `preempt/<victim-pid>` for
the lowest-prio **idle** resident; that gateway watches `preempt/`, and if `inflight==0` it
**gracefully unloads** (drain → drop → free → remove reservation), waking the waiter. Never
preempts a model mid-generation (the `inflight>0` guard — same rule that already blocks idle-shed).
A batch sweep thus *yields* to an interactive load instead of getting jetsam'd.

### Dependency (parallel, not blocking): smmr-D
The grant is only as safe as the footprint estimate. MLX peak is **cache-dominated** (a 4B peaks
~27 GB). Without **smmr-D** (live active-vs-cache split → honest peak), the queue can still admit a
pair that *estimates* fit but *spikes* into overcommit. The queue is **strictly safer than today**
regardless (it orders + preempts instead of racing), but true zero-jetsam needs smmr-D. Tracked
separately; this spec links it.

## Validation — the matrix under REAL contention (the acceptance test)

This is the point of the build, per the operator: prove it under load, not in a clean room.

1. **Baseline (reproduce the failure):** run the agentic matrix (claude+codex+opencode × Qwen3-4B)
   WHILE a scripted "antagonist" sibling repeatedly loads/unloads a big model (GLM-32B) on a loop —
   mimicking green-matrix. Expect today: jetsam / `0.0s` dead cells / a killed gateway.
2. **With the queue:** same run + antagonist. **Assert:** 0 jetsam, 0 dead cells (`0.0s/agent=0MB`),
   the bench gateway survives the whole run, loads **serialize** (never two big models co-resident
   beyond budget — check `/usr/bin/time -l` peaks + ledger), and the interactive matrix gateway
   **preempts** the batch antagonist (antagonist yields; matrix proceeds). Pass-rates match the
   free-host run (codex 1/6, opencode 4/6, claude 3/6 — no infra contamination).
3. **No-reboot over the whole campaign** (the non-negotiable).

## Phases

- **P1** — queue + kqueue event-wait (replace poll-240s); ordered admit. Kills busy-wait + the herd.
- **P2** — actual-free-RAM in the grant (mostly v3 already) verified against a non-participant load.
- **P3** — priority tag (interactive vs batch).
- **P4** — cooperative preemption (evict idle low-prio resident).
- **P5** — contention validation harness (antagonist + the asserts above). **This is the deliverable.**

## Non-goals
General multi-tenant scheduling; GPU-time fairness; Linux now (kqueue → inotify is a later port).
Lossless co-residency safety beyond the conservative estimate (that's smmr-D).
