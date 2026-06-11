# Backend-agnostic concurrency & admission

## Overview

The admission scheduling built for mistralrs (`mistralrs-concurrency-scheduling.md`:
budgeted capacity, runtime-adjustable limit, SJF + reserved fast lane, bounded-
queue backpressure, OOM circuit breaker) is **not mistralrs-specific** — every
in-process Metal backend (the new `mlx-rs` one first) wants the same protection.
This feature lifts that machinery out of the mistralrs module into a generic
`concurrency` module and re-applies it to mistralrs through a backend decorator,
so any backend gets it by opting in, with a safe fallback when a backend can't
supply the inputs.

## Interface

### Generic module `src/concurrency.rs` (no backend types; builds without any feature)

Moved here verbatim from the mistralrs modules (now generic):
- `AdmissionScheduler`, `AdmissionConfig`, `AdmitError`, `AdmitGuard`, `RequestCost`.
- Budget math: `ConcurrencyBudget`, `budgeted_max_num_seqs`, `per_seq_prefill_peak`,
  `PREFILL_PEAK_BYTES_PER_TOKEN`, `DEFAULT_SEQS_CEILING`.

New:
```rust
/// Estimate a request's cost from the rendered prompt (~4 chars/token) + max_tokens.
pub fn estimate_cost(req: &ChatRequest) -> RequestCost;

/// Decorator: adds admission / backpressure / circuit-breaker to ANY ChatBackend.
pub struct AdmittingBackend { /* inner: Arc<dyn ChatBackend>, scheduler */ }
impl AdmittingBackend {
    pub fn new(inner: Arc<dyn ChatBackend>, cfg: AdmissionConfig) -> Self;
}

/// Wrap `inner` iff it advertises a concurrency capacity; otherwise return it
/// unchanged (passthrough — the safe default for remote / self-serializing
/// backends). Reads the generic env knobs to build the config.
pub fn admit_wrap(inner: Arc<dyn ChatBackend>) -> Arc<dyn ChatBackend>;
```

### `ChatBackend` trait — one optional hook

```rust
/// A backend's self-assessed safe concurrent-request capacity. `None` (default)
/// means "don't gate me" — `admit_wrap` leaves the backend untouched.
fn concurrency_capacity(&self) -> Option<usize> { None }
```

### Env (generic; renamed from the `ROZUM_MISTRALRS_*` admission knobs)

```
ROZUM_ADMIT=<n>                 # admission limit override (clamped to capacity)
ROZUM_ADMIT_QUEUE_MAX=<n>       # wait-queue bound before 429 (default 32; 0 = unbounded)
ROZUM_ADMIT_FASTLANE_TOKENS=<n> # fast-lane cost threshold (default 1024; 0 = off)
```

Engine-internal knobs stay mistralrs-specific: `ROZUM_MISTRALRS_MAX_SEQS`
(engine batch size / budget override), `ROZUM_MISTRALRS_SEQS_CEILING`.

## Behavior

- [ ] `concurrency` module compiles and unit-tests with **no** features (no Xcode):
      scheduler + budget tests move over and still pass.
- [ ] `AdmittingBackend::chat` admits before delegating: `Overloaded` → `Err(ModelError::Overloaded)`
      (→ gateway 429); otherwise holds the `AdmitGuard` for the inner stream's
      lifetime and trips the breaker on a resource-exhaustion error from the inner stream.
- [ ] `AdmittingBackend` delegates `context_window`, `label`, and `concurrency_capacity`
      to the inner backend.
- [ ] `admit_wrap` wraps iff `inner.concurrency_capacity().is_some()`; a `None`-capacity
      backend (remote HTTP, hello, gguf-as-is) is returned unchanged.
- [ ] mistralrs implements `concurrency_capacity() -> Some(engine max_num_seqs)`; its
      `chat()` no longer does inline admission (the decorator owns it). The engine's
      internal `max_num_seqs` budget (Phase A) is unchanged.
- [ ] `build_gateway_backend` routes every selected backend through `admit_wrap`, so
      mistralrs (and the future mlx backend) are gated while remote/hello pass through.
- [ ] Fallback: a backend that advertises capacity but whose budget inputs are unknown
      still gets a safe serialized limit of `1` (via `budgeted_max_num_seqs`'s floor).

## Out of scope

- Per-backend custom cost estimators (the `chars/4 + max_tokens` heuristic is shared).
- Auto-wrapping remote backends (they self-serialize server-side; capacity stays `None`).
- The engine-yield work (`concurrency-engine-yield`) — orthogonal, still in BACKLOG.

## Design

`AdmittingBackend` is a decorator over `Arc<dyn ChatBackend>`. Its `chat()`:
1. `estimate_cost(&req)`;
2. `scheduler.admit(cost).await` — `Overloaded` short-circuits to `Err` (429) before
   any stream starts; otherwise yields a guard;
3. `inner.chat(req).await` for the inner stream;
4. wrap that stream so the guard is held until it ends/drops and any `Err` item trips
   the breaker (`note_backend_error`, the moved OOM-substring heuristic + cooldown recovery).

The decorator is the single place admission lives, so each backend stays focused on
inference. A backend opts in purely by returning `Some(capacity)` from
`concurrency_capacity()`; everything else (remote, hello) passes through untouched.

mistralrs keeps computing its engine `max_num_seqs` budget (Phase A) at load time —
that number is both the engine's batch size and the value it reports via
`concurrency_capacity()`, which becomes the decorator's default admission limit.

## Decisions

- **Decorator over per-backend wiring** — one generic implementation wraps any backend;
  backends don't each re-implement admission. Rejected: a shared helper each backend calls
  in its own `chat()` (duplicated boilerplate, drift).
- **Opt-in via `concurrency_capacity()`, passthrough on `None`** — the safe default for
  remote/self-serializing backends is *no* local gating; only backends that know a safe
  in-process limit get gated. Rejected: gating everything at a default of 1 (would wrongly
  serialize remote backends).
- **Generic `ROZUM_ADMIT*` env, mistralrs-specific `ROZUM_MISTRALRS_*` for the engine knob**
  — admission is now cross-backend; the engine batch size remains mistralrs's own.

## Results

(Filled after implementation.)
