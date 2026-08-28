# Spec: elastic context on demand — grow a resident model's served n_ctx live, shrink it back when idle

Status: 2026-08-28 — DRAFT (operator-proposed). Builds on the shipped
`docs/specs/residency-admission-queue.md` (cooperative preemption, event-driven wait queue)
and `docs/specs/safe-multi-model-residency.md` (the RAM ledger, `crates/rozum-core/src/share.rs`).

## Overview

Today the context window (`n_ctx`) a resident model serves is fixed once, at load time
(`adapt_n_ctx_to_fit` in `src/main.rs`), by how much RAM was free *at that moment*. Raising it later
needs a human to run `rozum gateway switch --model <same spec>` (drain → unload → reload) or edit a
plist and restart the service. The operator asked for it to instead grow **on demand** — triggered
by an actual request that needs more room, not by continuous polling — and to give back what it
isn't using, so idle capacity elsewhere gets reclaimed automatically instead of sitting reserved.

This is feasible **without a model reload** on the `mlx-native` backend specifically, verified in
code before writing this spec (see Decisions): `context_window()` (backed by the stored `n_ctx`) is
used **only** as a request-fitting/trim boundary at the gateway layer
(`crates/rozum-gateway/src/gateway.rs::fit_to_context`) — nothing in the MLX load path pre-allocates
a KV buffer or a RoPE table sized to it. The model is always loaded against its real architectural
max (`max_position_embeddings` from `config.json`); the served ceiling is a policy number, capped at
load time by adaptive-load's RAM estimate. A policy number can be changed live.

## Interface

- No required CLI/config change — the behavior is automatic once enabled. `ROZUM_ELASTIC_CTX=0`
  disables it (falls back to today's fixed-at-load behavior), matching the existing
  `ROZUM_GATEWAY_ADAPTIVE_LOAD` opt-out convention.
- `rozum-mlx`: the backend's stored `n_ctx: u32` becomes an `AtomicU32` (or an equivalent interior-
  mutable cell); `context_window()` reads it fresh on every call instead of returning a value fixed
  at construction.
- `rozum-core/src/share.rs`: a new ledger operation, `update_footprint(pid, new_footprint) -> bool`
  — rewrites an **already-admitted** resident's `residents/<pid>` entry in place, under the same
  `residency.lock` and the same `admits` check the initial `acquire_residency` uses (grow is a
  second admission decision, not a bypass). Shrink is unconditional (always safe to reserve less).
- Reuses, unchanged: the `preempt/<pid>` cooperative-preemption protocol (`request_preemption`,
  `preempt_requested`, `clear_preemption`, `pick_preempt_victim`) from the admission queue.

## Behavior

- [ ] A request whose estimated prompt tokens exceed the currently served `n_ctx`, but fit within
      the model's real architectural max, triggers a grow attempt **before** `fit_to_context` trims
      history — trimming stays the fallback, not the first move.
- [ ] Grow attempt: ask the ledger whether raising this resident's footprint by the delta still
      satisfies `admits` (ledger sum **and** actual-free-RAM, same two-lever check the initial load
      uses). If yes: `update_footprint` + flip the atomic `n_ctx` — no drain, no reload, in-flight
      requests unaffected, this request proceeds at the new ceiling.
- [ ] If it doesn't fit: look for an **idle** (`inflight == 0`), lower-or-equal-priority sibling and
      request preemption via the existing `preempt/<pid>` file — same mechanism the admission queue
      already uses for a new model's load, retargeted at "make room for a grow" instead of "make room
      for a new resident." Bounded wait (short — this is in the hot path of a live request, not a
      cold load); on timeout, proceed to the fallback.
- [ ] Fallback (preemption unavailable, or nothing frees in time): trim to the **current** `n_ctx`
      exactly as today — a grow attempt must never turn into a request failure or a hang.
- [ ] Shrink-back: a resident whose actual usage has stayed well under an elevated `n_ctx` for a
      cooldown window (idle, no growth pressure) voluntarily lowers its own reservation back toward
      the RAM-derived floor `adapt_n_ctx_to_fit` would have picked fresh — same spirit as the
      existing idle-shed behavior, so elevated reservations don't squat on RAM indefinitely.
- [ ] The hard invariant from `residency-admission-queue.md` holds at every instant: the ledger
      update and the atomic `n_ctx` flip are ordered so there is never a window where the reservation
      lags what could actually be requested (grow the ledger entry **first**, flip `n_ctx` **second**;
      shrink is the reverse order).
- [ ] `mistralrs` backend: excluded from live-grow (see Decisions) — a request there still falls
      through to today's trim-at-current-n_ctx behavior; growing that backend's ceiling still
      requires `gateway switch` (a real reload), unchanged by this spec.

## Out of scope

- Continuous/periodic RAM re-measurement — explicitly rejected by the operator; this is
  event-triggered only, on an actual request that needs more room.
- Partially shrinking an idle sibling's *own* served `n_ctx` to free room (rather than fully
  preempting/unloading it). The existing preemption primitive only knows whole-resident eviction;
  teaching a sibling to shrink itself on request is the same live-adjust machinery this spec builds,
  recursively applied to another process — worth a follow-up once whole-eviction is proven
  insufficient in practice, not built speculatively now.
- `mistralrs` / any backend whose KV pool is pre-allocated at load (PagedAttention) — needs an
  actual reload; out of scope here, `gateway switch` already covers it.
- Growing past the model's own architectural `max_position_embeddings` — never a goal; the ceiling
  this spec grows toward is always bounded by what `read_config` reports for the model.
- Cross-host / multi-machine coordination.

## Design

```
request arrives, prompt tokens > current n_ctx (but ≤ model's real max)
        │
        ▼
  try_grow(delta)                         (new, gateway.rs, before fit_to_context)
        │
        ├─ share::admits(my_pid, footprint + delta)?  ──yes──▶ update_footprint + n_ctx.store(new)
        │         │no
        │         ▼
        │  pick an idle, ≤-priority sibling (reuse pick_preempt_victim)
        │         │
        │         ├─ found ──▶ request_preemption(victim) ──▶ bounded wait for it to free
        │         │                    │ freed in time            │ timed out
        │         │                    ▼                          ▼
        │         │              retry admits() once          fall through
        │         │                                                │
        │         └─ none found ─────────────────────────────────►│
        ▼                                                          ▼
  (grown — proceed at new n_ctx)                      fit_to_context(current n_ctx) — today's path
```

Shrink-back runs on the existing idle-watch tick already present for idle-shed (whichever loop
polls `inflight`/last-activity for that purpose today) — added as one more condition on the same
tick, not a new timer.

## Decisions

- **Event-triggered, not a continuous poller** — chosen because the operator explicitly asked for
  this shape (checked only when a real request needs it), and it matches the project's existing
  event-driven admission design (`residency-admission-queue.md` P1 replaced a poll loop with
  `kqueue` for the same reason: no wasted cycles, no herd).
- **`mlx-native` only for v1** — verified, not assumed: `context_window()`'s only consumer is
  `fit_to_context`'s trim boundary (`crates/rozum-gateway/src/gateway.rs:583`); nothing in
  `crates/rozum-mlx/src/mlx_native_backend.rs`'s load path sizes a buffer against it (KV grows
  lazily, confirmed by the existing `auto_n_ctx` doc comment: "grows its KV cache lazily per actual
  token... no upfront cost, no cap"). `mistralrs` is different by its own documented contract
  (`N_CTX_AUTO_CAP` comment: "pre-allocates the PagedAttention KV pool") — growing it live would
  need the pool itself resized, which is a reload. Rejected doing both backends in one pass: the
  mistralrs path already has a working lever (`gateway switch`) and conflating "make mlx-native
  live-elastic" with "make mistralrs live-elastic too" would block the easy 90% behind the hard 10%.
- **Reuse whole-resident preemption rather than inventing partial-shrink-of-siblings** — chosen
  because whole-eviction of an idle victim is already shipped and proven
  (`residency-admission-queue.md` P4/P5); asking a sibling to shrink *itself* by some delta is the
  same live-adjust capability this spec is building, applied recursively to another process. Simpler
  to ship "evict the idle victim entirely" first and observe whether that's too coarse in practice
  (an idle sibling losing its whole resident state to free 2 GB) before building the finer-grained
  version.
- **Grow-then-flip ordering (ledger before n_ctx, not after)** — chosen to preserve the hard
  invariant from `residency-admission-queue.md`: the reservation must never lag what a request could
  actually claim. Shrinking reverses the order (flip `n_ctx` down first, then release the
  reservation) for the same reason in the opposite direction — never advertise capacity you haven't
  reserved yet.

## Results

<!-- fill in after implementation: measured grow/shrink latency, whether the preemption path was
ever exercised in practice, any cases where the fallback (trim) fired instead of growing. -->
