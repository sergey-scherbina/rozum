# mistralrs Concurrency & Scheduling

## Overview

The in-process mistralrs backend currently serves requests under a single blunt
knob — `max_num_seqs` (default `1`, adaptively `2` on ≥48 GB; see
`mistralrs-backend.md`). That floor exists because two concurrent large-prompt
prefills can OOM the Metal command buffer. It has two weaknesses: (1) it leaves
throughput on the table when there is real memory headroom, and (2) at `1` it
serialises everything, so a small interactive request (Claude Code's quick
follow-up) waits behind a 20k-token context read.

This feature replaces the blunt knob with a **layered concurrency model** that
is responsive, scales sensibly with memory, and stays safe under load. Four
ideas delivered synergistically:

- **A — budgeted engine capacity.** Derive the engine's `max_num_seqs` at load
  time from the *actual* model footprint vs available unified memory, using the
  now-constant per-prefill activation cost (chunked prefill bounds it to the
  chunk size — see `mistralrs-chunked-prefill.md`: ~465 KB/token × chunk).
  Clamp to a compute-bound ceiling, not a memory one.
- **B — admission limit ≠ engine capacity.** mistralrs fixes `max_num_seqs` at
  model-build time. Give the engine a generous budgeted ceiling, but gate the
  *actual* concurrency with a rozum-side semaphore that can move at runtime.
- **C — priority scheduling + fast lane.** A rozum-side admission queue orders
  by estimated cost (shortest-job-first) and reserves ≥1 concurrency slot for
  short interactive requests, so they never queue behind big batch jobs.
- **D — backpressure + load shedding.** Bound the queue, reject with a clear
  error when overloaded, and back the admission limit off (circuit breaker) on
  runtime memory pressure instead of crashing.

The constraint shaping all of this: Metal is a **single GPU device**. Memory
sets an upper bound on concurrency; GPU compute sets the practical sweet spot.
Past a small N, more concurrent prefills raise tail latency without adding
throughput — and for an interactive coding assistant, p95 latency is the metric
that matters. So we scale modestly and solve responsiveness with scheduling, not
with a large N.

## Interface

All knobs are env vars (consistent with the existing `ROZUM_MISTRALRS_*`
family); none are required, and none affect the default (no-`mistralrs`) build.

```
# A — engine capacity (build-time ceiling)
ROZUM_MISTRALRS_MAX_SEQS=<n>      # force exact engine max_num_seqs; unset = budgeted
ROZUM_MISTRALRS_SEQS_CEILING=<n>  # compute sweet-spot cap on the budgeted value (default 8)

# B — runtime admission limit (<= engine capacity)
ROZUM_MISTRALRS_ADMIT=<n>         # force admission limit; unset = engine capacity

# C — fast lane / priority
ROZUM_MISTRALRS_FASTLANE_TOKENS=<n>  # request cost (prompt+max_tokens) below this
                                     # is "interactive" and uses the reserved slot
                                     # (default 1024); 0 disables the fast lane

# D — backpressure / shedding
ROZUM_MISTRALRS_QUEUE_MAX=<n>     # max queued (not-yet-admitted) requests before
                                  # backpressure (default 32); 0 = unbounded
```

Key internal API (signatures may evolve; this is the contract callers depend on):

```rust
// A — budgeted capacity, pure & unit-tested.
pub struct ConcurrencyBudget {
    pub available_ram: Option<u64>, // bytes
    pub weights: Option<u64>,       // bytes
    pub kv_pool: Option<u64>,       // bytes (paged pool sized from n_ctx)
    pub per_seq_peak: u64,          // bytes: prefill_chunk * ~465 KB/token
    pub ceiling: usize,             // compute sweet-spot cap
}
pub fn budgeted_max_num_seqs(b: &ConcurrencyBudget) -> usize; // clamp(headroom/per_seq, 1, ceiling)

// B/C/D — the admission scheduler wrapping the engine.
pub struct AdmissionScheduler { /* private */ }
impl AdmissionScheduler {
    pub fn new(engine_capacity: usize, cfg: AdmissionConfig) -> Self;
    /// Acquire a slot before calling the engine. Returns Err(Overloaded) when
    /// the queue is full, or resolves when admitted (honouring priority + the
    /// reserved fast lane). The returned guard releases the slot on drop.
    pub async fn admit(&self, cost: RequestCost) -> Result<AdmitGuard, AdmitError>;
    /// D: nudge the live admission limit down/up (circuit breaker).
    pub fn set_admit_limit(&self, n: usize);
}
pub struct RequestCost { pub prompt_tokens: usize, pub max_tokens: usize }
```

## Behavior

### Phase A — budgeted engine capacity (`concurrency-budget`)

- [x] `budgeted_max_num_seqs` returns `clamp(headroom / per_seq_peak, 1, ceiling)`
      where `headroom = safety_frac * available - weights - kv_pool`.
- [x] `per_seq_peak` is derived from the active prefill chunk size × ~465 KB/token
      (`PREFILL_PEAK_BYTES_PER_TOKEN`, via `per_seq_prefill_peak(chunk)`), so the
      per-slot cost is ~constant regardless of prompt length.
- [x] The floor is `1`; the value lifts to `≥2` only when headroom covers at
      least one extra `per_seq_peak` (so a fast lane is physically possible).
- [x] `ROZUM_MISTRALRS_MAX_SEQS` overrides the budgeted value exactly;
      `ROZUM_MISTRALRS_SEQS_CEILING` caps it (default 8).
- [x] Decision is made at load time from the *actual* model (`resolve_max_num_seqs`
      in `main.rs` reuses `cached_weights_bytes` / `kv_cache_bytes` /
      `available_ram_bytes`), not a machine-class guess. Replaces the
      24–36 GB → 1 / ≥48 GB → 2 ladder.
- [x] Unit-tested across (available, weights, kv, per_seq, ceiling) tuples with
      no `mistralrs` feature / no Xcode (pure function).

### Phase B + C — admission scheduler + fast lane (`concurrency-admission`)

- [x] The engine is built with the budgeted capacity; actual concurrency is
      gated by an `AdmissionScheduler` semaphore whose limit defaults to that
      capacity and is overridable via `ROZUM_MISTRALRS_ADMIT`.
- [x] Every `chat()` acquires an `AdmitGuard` before touching the engine and
      releases it on completion/cancel/drop.
- [x] Requests are ordered shortest-job-first by `RequestCost`
      (`prompt_tokens + max_tokens`); a large request never starves a small one
      that arrived later (bounded by the fast lane below).
- [x] One slot is reserved as a **fast lane**: a request whose cost is below
      `ROZUM_MISTRALRS_FASTLANE_TOKENS` (default 1024) may use the reserved slot
      even when all general slots are busy with big jobs. `=0` disables it.
- [x] At engine capacity `1` the fast lane is inert (no slot to reserve) but SJF
      ordering of the queue still applies — small jobs run first.
- [~] With chunked prefill on and capacity `≥2`, a fast-lane request admitted
      alongside a running large prefill is interleaved by the engine.
      **Finding: the fork does NOT yield between prefill chunks** — chunking is
      internal to `pipeline::step` (commit `698bccf1f`), so a prompt's whole
      chunked prefill runs in one engine step. The fast lane therefore gives
      *admission-order* responsiveness (admitted as soon as the current step
      frees a slot, ahead of queued big jobs; not blocked on the big request's
      full generation), but **not** mid-big-prefill preemption. True interleaving
      is deferred to the engine-side `concurrency-engine-yield` backlog item.
- [x] Cancel-on-disconnect still works: a queued-but-not-admitted request whose
      client drops is removed from the queue (`admit` future drop / dead-receiver
      reclaim); an admitted one cancels within one decode step (preserves the
      `mistralrs-large-prompt-stall` `select!` + guard drop).

### Phase D — backpressure + load shedding (`concurrency-load-shedding`)

- [ ] The admission queue is bounded by `ROZUM_MISTRALRS_QUEUE_MAX` (default 32);
      exceeding it returns `AdmitError::Overloaded`, surfaced as HTTP 429 by the
      gateway (with `Retry-After`) rather than buffering unboundedly.
- [ ] A runtime Metal allocation failure is caught (not fatal): the scheduler
      drops the live admission limit by one (min 1) via `set_admit_limit`, the
      failed request is retried after the in-flight count falls, and the limit
      recovers toward the engine capacity after a cooldown.
- [ ] Per-class `max_tokens` defaults: fast-lane requests get a lower cap than
      batch requests, so an interactive turn can't silently become a long job.
- [ ] No deadlock: limit changes, cancels, and the bounded queue interact
      without losing or double-counting slots (concurrency invariants tested).

## Out of scope

- **Preemption / swap-out** of an already-running sequence to admit a
  higher-priority one (vLLM-style). mistralrs does not expose this; tracked in
  `BACKLOG.md` as `concurrency-preemption`.
- **Multiple model instances** / size-class routing (a "small model" lane). One
  loaded model only; backlog `concurrency-multi-instance`.
- Cross-process coordination (several `rozum` processes sharing one GPU budget).
  Backlog `concurrency-cross-process`.
- Token-exact cost estimation via the real tokenizer — Phase C uses a cheap
  estimate (char/word heuristic + requested `max_tokens`); a tokenizer-accurate
  estimator is backlog `concurrency-cost-tokenizer`.
- Changing mistralrs's own batching/attention internals (that is
  `mistralrs-chunked-prefill` / `mistralrs-mlx-direct` territory).

## Design

### Two control points

```
            ROZUM_MISTRALRS_MAX_SEQS / budget          ROZUM_MISTRALRS_ADMIT
                        │                                      │
          ┌─────────────▼──────────────┐        ┌──────────────▼───────────────┐
 request →│ AdmissionScheduler (rozum)  │ admit →│ mistralrs engine (max_num_seqs│→ tokens
          │  • bounded queue + 429       │ guard │  = budgeted capacity, static)  │
          │  • SJF priority              │        │  • continuous batching         │
          │  • reserved fast lane        │        │  • chunked prefill             │
          │  • runtime limit (circuit)   │        └────────────────────────────────┘
          └──────────────────────────────┘
```

- **Engine capacity** (`max_num_seqs`) is *static* — fixed when the model is
  built. So it is set once, generously, from the memory budget (Phase A). It is
  the hard physical ceiling.
- **Admission limit** is *dynamic* — a rozum-side semaphore `≤` engine capacity.
  This is where priority, the fast lane, backpressure, and the circuit breaker
  live, because they need to react per-request and at runtime, which the engine
  knob cannot.

This split is the crux: it lets us widen the engine for headroom while keeping a
nimble, observable, runtime-adjustable scheduler in front.

### Why the per-slot cost is now constant (enables A)

`mistralrs-chunked-prefill.md` established the prefill activation peak at
~465 KB/token and bounded it to the **chunk size** (`MISTRALRS_PREFILL_CHUNK`,
default 2048–4096), independent of prompt length. KV is a paged pool sized from
`n_ctx`; decode is cheap. So each *additional* concurrent prefill costs ~one
chunk's worth of activations (~1–2 GB at the default chunk) — a stable constant.
That makes `headroom / per_seq_peak` a meaningful capacity estimate, which it
would not be if the peak still scaled with prompt length.

### Why a compute ceiling, not just memory

A 64–128 GB Mac has memory for ~10–15 concurrent prefill slots, but Metal is one
device: beyond a handful of concurrent prefills the GPU is saturated, so extra
slots add scheduling overhead and **tail latency** without throughput. The
default `ceiling` (8) caps the budgeted value at the compute sweet spot; raise
it via `ROZUM_MISTRALRS_SEQS_CEILING` for throughput-oriented batch use.

### Fast lane requires capacity ≥ 2

Reserving an interactive slot only works if there are ≥2 slots (1 batch + 1
fast). So Phase A's budget intentionally lifts the floor to 2 whenever headroom
covers the extra `per_seq_peak` — the minimum useful concurrency for
responsiveness is 2, not 1. On a genuinely tight machine that only affords 1,
the fast lane is inert and we degrade to SJF-ordered serialisation (small jobs
first) — still better than pure FIFO.

### Cost estimation (Phase C)

`RequestCost` is cheap: `prompt_tokens` from a char/word heuristic over the
rendered prompt, plus the requested `max_tokens` (or the `DEFAULT_MAX_TOKENS`
fallback). SJF sorts the wait queue by this; the fast-lane test is
`cost < FASTLANE_TOKENS`. A tokenizer-accurate estimator is a backlog refinement
— the heuristic is sufficient to separate "quick follow-up" from "20k context
read", which is all the scheduler needs.

### Composition with existing work

- Sits **above** the engine: PagedAttention, chunked prefill, and the planned
  MLX-direct path are all engine-side and untouched.
- Reuses the `main.rs` memory preflight helpers for Phase A (no new RAM probing).
- Preserves the `mistralrs-large-prompt-stall` cancel/reap behaviour: the
  `AdmitGuard` drop path and the engine cancel both fire on client disconnect.

## Decisions

- **Split admission limit from engine capacity** — chosen because mistralrs's
  `max_num_seqs` is build-time-static, so a runtime-reactive scheduler (priority,
  fast lane, circuit breaker) must live in front of it. Rejected: driving
  everything through the engine knob (cannot react per-request or at runtime);
  rebuilding the model to change concurrency (absurd cost).
- **Budget against constant per-slot cost, clamp to a compute ceiling** — chosen
  because chunked prefill made the per-prefill peak constant, so memory budgeting
  is now sound; the ceiling reflects that Metal is one device and interactive
  workloads want low tail latency, not maximum batch throughput. Rejected: fixed
  ladder (the current 1/2 — leaves big machines underused, gives small ones no
  responsiveness story); scaling N straight to the memory limit (tail-latency
  blowup, no throughput gain past saturation).
- **Reserved fast lane + SJF over true preemption** — chosen as the
  responsiveness win achievable without engine support: a reserved slot plus
  shortest-job-first ordering keeps interactive turns snappy. Rejected (for now):
  preemption/swap-out (not exposed by mistralrs; backlog).
- **Heuristic cost estimate** — chosen so admission has zero tokenizer cost on
  the hot path; the heuristic is enough to class interactive vs bulk. Rejected:
  exact tokenization at admission (cost on every request for precision the
  scheduler does not need).
- **Backpressure as 429, not unbounded buffering** — chosen so overload degrades
  predictably (clients back off) instead of latency/memory blowing up. Rejected:
  unbounded queue (turns overload into an OOM/timeout cascade).

## Risks / sharp edges

- **Engine-internal interleaving is an assumption.** The fast-lane responsiveness
  win at capacity ≥2 depends on the fork re-entering the scheduler between
  prefill chunks (continuous batching). If a long prefill is run to completion
  before other sequences advance, the fast lane only helps at *admission*, not
  *progress*. Must verify against the fork and record in Results; if false,
  escalate to `mistralrs-chunked-prefill` to yield between chunks.
- **Runtime Metal OOM is often fatal**, not a catchable `Result`. The circuit
  breaker is best-effort; prevention via a conservative `safety_frac` in the
  budget is the primary defence.
- **SJF starvation of big jobs** — bound by the fast lane being a *single*
  reserved slot and by big jobs still getting all non-fast slots; add an
  age-based priority bump if starvation shows up in practice.
- **`per_seq_peak` is an estimate.** Use a conservative constant from the
  chunked-prefill findings and a `safety_frac` < 1; refine empirically.

## Results

### Phase A — budgeted engine capacity (done)

Constants chosen (`src/mistralrs_backend.rs`):
- `PREFILL_PEAK_BYTES_PER_TOKEN = 465 KiB` (from `mistralrs-chunked-prefill.md`).
- `BUDGET_SAFETY_FRAC = 0.8` (commit 20% of free RAM to OS/slack/spikes).
- `DEFAULT_SEQS_CEILING = 8`; per-slot cost at the paged default chunk (4096)
  ≈ 1.82 GiB.

`budgeted_max_num_seqs(ConcurrencyBudget)` is pure and lives in the lib;
`resolve_max_num_seqs(model_id, n_ctx)` in `main.rs` gathers the footprint
(reusing the preflight helpers) and applies the env overrides
(`ROZUM_MISTRALRS_MAX_SEQS` force, `ROZUM_MISTRALRS_SEQS_CEILING` cap,
`MISTRALRS_PREFILL_CHUNK` for the per-slot cost). The decision is logged as a
`concurrency_budget` obs event. `MistralrsOptions::default()` now carries a
plain serialised floor of `1`; the budgeted value is set at the load-time call
site. Worked examples (20 GiB weights + 4 GiB KV, chunk 4096):

| available | headroom (0.8·avail − 24 GiB) | slots | result |
|-----------|-------------------------------|-------|--------|
| 32 GiB    | ~1.6 GiB                      | 0→1   | 1 (serialised) |
| 36 GiB    | ~4.8 GiB                      | 2     | 2 (fast lane possible) |
| 48 GiB    | ~14.4 GiB                     | 7     | 7 |
| 64 GiB    | ~27 GiB                       | 14    | 8 (ceiling) |

Verification: 6 lib unit tests green without the `mistralrs` feature (no Xcode);
`cargo check --features mistralrs` clean; `cargo fmt --check` clean.

### Phase B+C — admission scheduler + fast lane (done)

`src/mistralrs_admission.rs` (engine-agnostic, no `mistralrs` types, so it
builds + unit-tests without the feature / Xcode): `AdmissionScheduler` (cheaply
cloneable, `Arc<Mutex<State>>`), `admit(RequestCost) -> AdmitGuard`,
`set_limit(n)` (Phase D circuit breaker), `stats()`. Slot accounting reserves
one fast-lane slot when `limit ≥ 2` and the fast lane is on; the hard total
`limit` is always enforced first (`can_admit`). Waiters are woken by a `pump`
that scans for the best *admittable* waiter (fast → SJF → FIFO) and transfers an
already-counted `AdmitGuard` over a `oneshot`; a cancelled waiter's dead receiver
is skipped and its slot reclaimed inline (disarmed guard ⇒ no re-entrant drop).
Wired into `MistralrsBackend`: built from the Phase-A capacity via
`AdmissionConfig::from_engine_capacity` (`ROZUM_MISTRALRS_ADMIT` /
`ROZUM_MISTRALRS_FASTLANE_TOKENS`), and `chat()` acquires the guard inside the
stream (racing cancellation) so it's held for the stream's lifetime and released
on completion/disconnect.

Env: `ROZUM_MISTRALRS_ADMIT` (limit, ≤ capacity), `ROZUM_MISTRALRS_FASTLANE_TOKENS`
(default 1024, `0` off). Cost = `chars/4` prompt estimate + requested
`max_tokens`.

**Key finding — no engine yield between prefill chunks.** The fork's chunked
prefill (`mistralrs-chunked-prefill.md`, commit `698bccf1f`) loops chunks
*inside* `pipeline::step`, not across scheduler iterations. So at capacity ≥2 the
fast lane delivers admission-order responsiveness (a short request runs as soon
as the current step frees a slot, without waiting for the big request's full
generation) but not mid-prefill preemption. Closing that gap requires moving the
chunk loop up to the scheduler — filed as `concurrency-engine-yield` in BACKLOG.

Verification: 5 async unit tests (limit/queue, fast-lane jump, single-slot SJF,
cancelled-waiter reclaim, limit-raise) green without the feature; `cargo check
--features mistralrs` clean; `cargo fmt --check` clean.

### Phase D

(Phase D records the circuit-breaker recovery behaviour under an induced
allocation failure.)
