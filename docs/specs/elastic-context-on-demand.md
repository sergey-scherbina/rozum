# Spec: elastic context on demand — grow a resident model's served n_ctx live, shrink it back when idle

Status: 2026-08-28 — **SHIPPED (v1: grow + idle shrink-back; no preemption-on-denial — see Decisions).**
Builds on the shipped
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

- No required CLI/config change — the behavior is automatic. `ROZUM_ELASTIC_CTX=0` disables it
  (falls back to today's fixed-at-load behavior), matching the existing `ROZUM_GATEWAY_ADAPTIVE_LOAD`
  opt-out convention.
- `ChatBackend` trait (`rozum-core/src/backend.rs`) gained two default methods, so every OTHER
  backend keeps compiling unchanged: `max_context_window()` (default: same as `context_window()` —
  never grows) and `try_grow_context(want) -> u32` (default: no-op, returns `context_window()`).
  Only `mlx-native` overrides both.
- `rozum-mlx`: `MlxNativeBackend`'s `n_ctx` field is now `AtomicU32` (was `u32`); two new immutable
  fields, `arch_max_n_ctx` (the real ceiling from `config.json`, captured before any adaptive/CLI
  cap) and `weight_bytes` (on-disk size, for the footprint formula). `context_window()` reads the
  atomic fresh; `try_grow_context` delegates to a free function `grow_context` (kept free so the
  decision logic is unit-testable without a live worker thread + loaded model).
- `rozum-models/src/model_source.rs` gained three small pure exports, factored out of the existing
  `fit_model_params`/`fit_params_with_kv` rather than duplicated: `kv_per_pos_bytes(spec)`,
  `model_weight_bytes(spec)`, `footprint_for(weight_bytes, kv_per_pos, n_ctx, cache_gib)`.
  `models::dir_size` went from private to `pub` for the same reason.
- `rozum-core/src/share.rs` gained `update_own_footprint(model, new_footprint) -> bool` — updates
  the CALLING PROCESS's own `residents/<mypid>` entry via `std::process::id()`, no `ResidencyGuard`
  handle needed (the guard is held several layers away, in `main.rs`'s startup code, unreachable
  from deep inside the backend). Turned out `ResidencyGuard::update_footprint` and
  `dry_run_admission` already existed (residency-unify U1) — no NEW ledger primitive was needed for
  the admission check itself, only this pid-keyed variant of the write.
- `ChatBackend` gained a third default method, `shrink_idle_context(&self) -> Option<(u32, u32)>`
  (default: no-op `None`), called from the gateway's PRE-EXISTING idle watchdog tick (the same 2s
  loop that already drives idle-unload/pressure-shed in `gateway.rs`) — not a new timer. Gated on a
  new `elastic_shrink_idle_secs()` (`ROZUM_ELASTIC_CTX_SHRINK_IDLE_SECS`, default 120s — shorter than
  `unload_idle_secs`' 300s, so a grow gives itself back before the whole model unloads for the same
  idleness) and the same `generating == 0` guard idle-unload already uses.
- `rozum-mlx`: one more immutable field, `loaded_n_ctx` (the ceiling actually loaded with, before
  any grow) — what `shrink_idle_context` releases back down to. `shrink_context` is `grow_context`'s
  mirror: no admission check needed (reserving less is always safe), reverse ordering (`n_ctx` down
  first, ledger second).

## Behavior

- [x] A request whose estimated prompt tokens exceed the currently served `n_ctx`, but fit within
      the model's real architectural max, triggers a grow attempt **before** `fit_to_context` trims
      history — trimming stays the fallback, not the first move.
      (`crates/rozum-gateway/src/gateway.rs`, the `estimate_prompt_tokens` pre-fit check.)
- [x] Grow attempt: ask the ledger whether raising this resident's footprint to the new total still
      satisfies `dry_run_admission` (ledger sum **and** actual-free-RAM **and** kernel pressure, the
      same three-lever check the initial load uses). If yes: `update_own_footprint` + store the
      atomic `n_ctx` — no drain, no reload, in-flight requests unaffected, this request proceeds at
      the new ceiling. (`grow_context` in `mlx_native_backend.rs`.)
- [ ] ~~If it doesn't fit: preempt an idle sibling.~~ **NOT built in v1** — see Decisions. Today a
      grow that doesn't fit just falls through to the existing trim fallback.
- [x] Fallback (nothing to grow into): trim to the **current** `n_ctx` exactly as today — a grow
      attempt never turns into a request failure or a hang (every early-return in `grow_context`
      returns `current` unchanged, never an error).
- [x] Shrink-back after an idle cooldown: a resident whose `n_ctx` is above `loaded_n_ctx` releases
      the difference once idle (no in-flight generation) for `ROZUM_ELASTIC_CTX_SHRINK_IDLE_SECS`
      (default 120s) — piggybacked on the gateway's existing idle-watchdog tick, not a new timer.
      (`shrink_context` in `mlx_native_backend.rs`, called from `gateway.rs`'s watchdog loop.)
- [x] The hard invariant from `residency-admission-queue.md` holds at every instant: the ledger
      update happens FIRST (`update_own_footprint`), the atomic `n_ctx` store SECOND, and only if the
      ledger write itself succeeded — a reader can never see a served ceiling wider than what's
      actually reserved.
- [x] `mistralrs` backend: excluded from live-grow via the trait's default no-op — a request there
      still falls through to today's trim-at-current-n_ctx behavior; growing that backend's ceiling
      still requires `gateway switch` (a real reload), unchanged by this spec.
- [x] `ROZUM_ELASTIC_CTX=0` disables growth entirely (checked first in `grow_context`, before
      touching RAM/ledger at all).

## Out of scope

- Continuous/periodic RAM re-measurement — explicitly rejected by the operator; this is
  event-triggered only, on an actual request that needs more room.
- Preempting an idle sibling when a grow doesn't fit on its own — PLANNED but deferred out of v1;
  see Decisions. (Shrink-back after a cooldown IS built — see Behavior.)
- `mistralrs` / any backend whose KV pool is pre-allocated at load (PagedAttention) — needs an
  actual reload; out of scope here, `gateway switch` already covers it.
- Growing past the model's own architectural `max_position_embeddings` — never a goal; the ceiling
  this spec grows toward is always bounded by what `read_config` reports for the model.
- Cross-host / multi-machine coordination.

## Design

As shipped (v1 — no preemption branch, see Decisions):

```
request arrives, prompt tokens > current n_ctx (but ≤ model's real max)
        │                                            gateway.rs, before fit_to_context
        ▼
  backend.try_grow_context(want)
        │                                            mlx_native_backend.rs::grow_context
        ▼
  ROZUM_ELASTIC_CTX=0? ──yes──▶ return current (fallback)
        │no
        ▼
  fit_model_params(spec, weight_bytes, want, available_ram_now, min_free, current)
        │                                            reuses the EXACT load-time fitting math
        ├─ None, or fit_n_ctx ≤ current ─────────────▶ return current (fallback)
        │
        ▼ fit_n_ctx > current
  footprint_for(weight_bytes, kv_per_pos, fit_n_ctx, fit_cache_gib)
        │
        ▼
  dry_run_admission(new_footprint).admit?  ──no──────▶ return current (fallback)
        │yes
        ▼
  update_own_footprint(model, new_footprint)  ──failed──▶ return current (fallback)
        │ok
        ▼
  n_ctx.store(fit_n_ctx)  →  return fit_n_ctx (grown)
```

Every non-growth path returns `current` unchanged and falls through to `fit_to_context(current)` —
today's trim behavior, verbatim.

Shrink, on the existing idle-watchdog tick (`gateway.rs`, alongside idle-unload):

```
idle_for ≥ ROZUM_ELASTIC_CTX_SHRINK_IDLE_SECS (120s) and generating == 0?  ──no──▶ (skip this tick)
        │yes
        ▼
  backend.shrink_idle_context()                      mlx_native_backend.rs::shrink_context
        │
        ▼
  n_ctx > loaded_n_ctx?  ──no──▶ None (nothing above baseline — no-op)
        │yes
        ▼
  fit_model_params(spec, weight_bytes, loaded_n_ctx, available_ram_now, min_free, floor)
        │                                            no admission check — releasing is always safe
        ▼
  n_ctx.store(target_n_ctx)              ◀── flipped DOWN first (mirror of grow's ordering)
        │
        ▼
  update_own_footprint(model, footprint_for(...))     ledger released second
```

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
- **Preemption of idle siblings deferred out of v1, not built** — the original plan was to reuse
  whole-resident preemption (`residency-admission-queue.md` P4/P5) when a grow doesn't fit on its
  own. Cut from v1 for two reasons found while implementing, not anticipated when this spec was
  drafted: (1) preemption is currently keyed to a NEW model's cold load, not a live in-flight
  request — retargeting it needs the SAME bounded-wait-in-the-hot-path plumbing this spec already
  has to get right once (the grow attempt itself), and doing both in one pass risked neither being
  well-tested; (2) the fallback (trim to `current`) is a genuinely fine outcome, not a degraded one
  — it's exactly what happens today, unconditionally. Shipping grow-only first, observing how often
  the fallback actually fires in practice (see Results), is the same "ship the easy 90%, don't block
  it behind the hard 10%" call already made for `mlx-native`-only. Revisit as its own follow-up spec
  if the fallback rate turns out to matter.
- **Shrink-back reuses the gateway's PRE-EXISTING idle watchdog, not a new timer** — the tick
  already exists and already runs idle-unload/cache-squeeze/pressure-shed on the same 2s cadence
  (`gateway.rs`, spawned whenever `idle_exit`/`unload_on_idle`/`launch_managed`/`shed_active` apply
  — true for the durable service either way). Adding one more idle-gated condition to an existing
  loop is not the "continuous polling" the operator ruled out — that loop watches for OTHER reasons
  regardless of this feature; shrink just piggybacks on it rather than spinning up its own timer.
  Initially scoped as deferred (original draft, below) before this loop was found on a second pass.
- **Grow-then-flip ordering (ledger before n_ctx, not after)** — chosen to preserve the hard
  invariant from `residency-admission-queue.md`: the reservation must never lag what a request could
  actually claim. Shrinking reverses the order (flip `n_ctx` down first, then release the
  reservation) for the same reason in the opposite direction — never advertise capacity you haven't
  reserved yet.

## Results

Shipped as: `crates/rozum-core/src/backend.rs` (trait defaults), `crates/rozum-core/src/share.rs`
(`update_own_footprint`), `crates/rozum-models/src/model_source.rs` (`kv_per_pos_bytes`,
`model_weight_bytes`, `footprint_for`, `dir_size` → `pub`), `crates/rozum-mlx/src/mlx_native_backend.rs`
(`grow_context` + wiring), `crates/rozum-gateway/src/gateway.rs` (the pre-fit trigger).

Tests: 5 new (`grow_context_tests` in `mlx_native_backend.rs`) covering the no-op/capped/opt-out
fast paths (no I/O) and the grow/decline paths against a real isolated ledger
(`XDG_STATE_HOME` + `ROZUM_GATEWAY_AVAILABLE_RAM_BYTES` + `ROZUM_HOST_PRESSURE` pinned, following
the exact pattern `rozum-core`'s own admission tests already use to stay host-independent) — plus 1
new test each in `rozum-models` (`footprint_for` agrees with the fit boundary) confirming the
extraction didn't drift from the formula it replaced. Full existing suites re-run clean after the
change: `rozum-core` 169, `rozum-models` 26, `rozum-gateway` 161, `rozum-mlx` 49 (44 ignored/live).

One real bug caught by the "grows" test before it was a test-setup bug, not a real one, worth
recording: `update_own_footprint` correctly refused (returns `false`) when no `residents/<mypid>`
file exists yet — which is exactly the real gateway's shape (a reservation always exists first, via
`acquire_residency` at load), but the FIRST version of the test never created one. Fixed by having
the test call the real `acquire_residency` first, which is arguably the more honest test anyway (it
exercises the actual read-then-grow shape a live gateway would follow, not the update in isolation).

Not yet measured (needs live traffic, not unit tests): how often the fallback (trim) actually fires
vs. a successful grow in practice, and whether that rate justifies building the deferred preemption
half.

**Shrink-back, added same day after the operator asked for it explicitly.** Shipped as:
`ChatBackend::shrink_idle_context` (default), `MlxNativeBackend::loaded_n_ctx` +
`mlx_native_backend.rs::shrink_context`, `switchboard.rs::elastic_shrink_idle_secs`, wired into
`gateway.rs`'s existing idle-watchdog tick. 3 new tests (`shrink_context_tests`): no-op at/below
baseline, releases to baseline, falls back to the floor when even the baseline no longer fits.

One real bug caught by the test suite itself, worth recording since it's a durable lesson, not just
a fixed typo: the two test modules (`grow_context_tests`, `shrink_context_tests`) each defined their
OWN `env_lock()` — different `OnceLock` statics — so they looked serialized (each module's tests
waited on `its` lock) but weren't serialized AGAINST EACH OTHER. `cargo test` runs different test
functions on different threads by default, and a `grow` test and a `shrink` test mutating the same
process-global `ROZUM_GATEWAY_AVAILABLE_RAM_BYTES` concurrently produced a real 240s hang (one
test's "make RAM tight" stepped on another's in-flight `acquire_residency` wait). Fixed by hoisting
one `env_lock()` shared by both modules. The lesson: a lock scoped to "this file's tests" is not
lock scoped to "this global resource" — the mutex must be co-located with the RESOURCE it guards
(the env var), not with whichever module happened to need it first.

Full suites re-run clean after the fix: `rozum-core` 169, `rozum-gateway` 161, `rozum-mlx` 53
(44 ignored/live).
