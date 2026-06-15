# shared-gateway-multislot — serving more than one resident model

Phase 1 (the decision core, `src/resident.rs`) is shipped + tested. This spec is **Phase 2**: wiring
that core into the live `Switchboard` so the shared gateway can keep more than one model resident.

## Status (2026-06-15): IMPLEMENTED (mock-tested), real-model validation pending

The additive warm cache below is **implemented in `src/gateway.rs`** and unit-tested with the
mock-builder harness (`test_sb_cfg` + a deterministic `WarmConfig`): serve-a-second-model,
fall-back-when-it-doesn't-fit, skip-unknown/remote, evict-idle-to-make-room. It is **on by default**
(the user's choice), `ROZUM_MULTISLOT=0|false|off` opts out, and it is a **strict no-op for
single-model traffic** (the common Claude-Code/Codex case) — so existing behavior is unchanged unless
you actually request a second model.

Both earlier follow-ups are now done too: **idle-timeout eviction** (the lifecycle watchdog sweeps
warm entries idle past `unload_idle_secs` and frees their RAM — `sweep_idle_warm`, gated on
`inflight == 0` and last-activity age), and **persisted usefulness** (`UsageStats` is opened at
`$XDG_STATE_HOME/rozum/gateway/warm-usage.jsonl`, so the warm set's frequency×recency ranking
survives a restart; tests stay in-memory).

What still needs **real-model** confirmation (it changes the live serving path and the
memory / `!Send`-worker-drop behavior can't be exercised by the feature-free tests) — see the
validation checklist at the bottom.

## Goal (recap of the user's policy)

Keep the most *useful* (frequency × recency) **small** models co-resident **without thrashing**;
evict the least useful (idle only) to make room; a model too big to co-reside falls back to a swap
(thrash is unavoidable for big models). Pick the best arrangement possible under the memory budget.
The decision is already implemented: `resident::plan_residency` + `resident::UsageStats`.

## Design: additive **warm cache**, gated by `ROZUM_MULTISLOT` (default off)

Do **not** rewrite the single-resident core. Keep the existing `backend: RwLock<Option<Arc<…>>>`
("the **primary** resident") and all of its swap / drain / unload / idle-watchdog logic **exactly as
is**. Add, alongside it, a **warm cache** of *secondary* residents:

```rust
struct Switchboard {
    backend: RwLock<Option<Arc<dyn ChatBackend>>>,   // primary — UNCHANGED
    warm: tokio::sync::Mutex<HashMap<String, WarmEntry>>, // NEW: secondary residents
    usage: resident::UsageStats,                     // NEW: learned utility (persisted)
    // … everything else unchanged …
}
struct WarmEntry {
    backend: Arc<dyn ChatBackend>,
    weight_bytes: u64,
    inflight: Arc<AtomicU64>,   // its own in-flight count (NOT the primary `generating`)
    last_used: AtomicU64,
}
```

Gate the whole warm path behind `ROZUM_MULTISLOT` (truthy). **Off ⇒ byte-for-byte today's
behavior** — zero risk to existing users; the feature is opt-in until validated.

### Routing: `enter(requested: Option<&str>)`

The handlers already have `req.model`; thread it in (`chat_completions`, `responses`, `messages` →
`state.sb.enter(req.model.as_deref())`). Decision in `enter`:

1. `!multislot_enabled()` **or** `requested` is empty / equals the primary's `model_id` → the
   **existing primary path** (unchanged: park-on-drain → `ensure_loaded` → take a `generating`
   token).
2. Else, only consider warming a model that is a **known cached local spec**
   (`models::scan_all_installed` has it → we know it's buildable *and* its weight) — a `claude-…` /
   unknown string is **not** warmable and falls through to the primary path (so `req.model` stays
   informational, exactly as today, for everything we can't cheaply serve).
3. For a warmable model: `ensure_warm(model)` (below). On success, return a lease bound to the warm
   backend with `warm_inflight = Some(entry.inflight)`. On failure (won't fit / build failed), fall
   through to the primary path.

### `ChatLease` generalization

Add `warm_inflight: Option<Arc<AtomicU64>>`. `Drop`: decrement `warm_inflight` if `Some`, **else**
the primary `generating`. ⇒ **a warm request never counts against the primary `generating`**, so a
primary `switch`/`unload` drain is unaffected by warm traffic (no new deadlocks; the warm path is
independent of the primary's swap machinery).

### `ensure_warm(model)` — build/admit/evict via the planner

1. Already in `warm` → bump `last_used`, return it.
2. Serialize concurrent first-builds of the same model (a per-build lock / the existing pattern).
3. Compute the **memory plan**: residents = primary `(weight, busy = generating>0)` + each warm
   `(weight, busy = inflight>0)`; `budget_bytes = total_ram × safety_frac`; weights from
   `scan_all_installed`. Call `resident::plan_residency(req, |m| usage.utility(m, now))`.
4. `oversubscribed` ⇒ **don't warm** (return `None` → primary path; the inevitable big-model thrash).
5. Else: evict the plan's `evict` list — but **idle warm entries only** (`inflight == 0`); drop each
   on `spawn_blocking` (the MLX `Drop` joins its `!Send` worker; never drop under the map lock or on
   a runtime thread — mirror the existing `unload` care). Then `builder(model, n_ctx, None)` →
   insert the new `WarmEntry`. Record `usage.record(model, weight, now)`.

### Idle eviction of warm entries

A warm entry idle for `ROZUM_MULTISLOT_IDLE_SECS` (default = the primary `unload_idle_secs`) with
`inflight == 0` is dropped by the existing idle watchdog (extend it to also sweep `warm`), freeing
its RAM. The primary keeps its own idle-unload untouched.

### Registry / discovery

`active.json` stays keyed by the **primary** (the daemon's identity). Optionally add a
`resident: [..]` list for observability. Launch-reuse logic is unchanged (it matches the primary);
warming is a within-daemon optimization, not a new discovery surface. (Cross-process fleet
coordination remains `concurrency-cross-process`.)

## Testability seams (so the logic is mock-tested without real models)

- Inject the **weight lookup** and **budget** (don't read real RAM/model sizes in tests):
  `ensure_warm` takes them from small fns (`fn warm_budget_bytes() -> u64`, `fn model_weight(spec)
  -> Option<u64>`) that tests override (as `with_headroom_probe` does for the adaptive ceiling).
- The existing `test_sb(Some(ok_builder()), …)` harness builds a `Switchboard` with a mock
  `HelloBackend` builder → drive `enter(Some("m1"))` / `enter(Some("m2"))` and assert: both become
  resident when the (injected) budget fits; the low-utility idle one is evicted when it doesn't; a
  busy warm entry is never evicted; `ROZUM_MULTISLOT` off ⇒ always the primary path.
- The planner itself (`resident::plan_residency`) is already fully unit-tested.

## What needs **real-model** validation (the handoff)

Mock tests can't cover these — run them on the target machine:

1. Two real small models co-resident (e.g. Qwen3-4B + Qwen2.5-Coder-7B): both serve concurrently,
   no thrash; `mlx_memory_mb` shows both loaded; throughput is sane (they share one GPU — watch for
   contention; the per-backend `admit_wrap` ceilings still apply, but there is **no shared
   cross-resident GPU gate** yet — see `concurrency-multi-instance`).
2. Eviction frees real RAM (the `spawn_blocking` drop joins the MLX worker; confirm no stall).
3. A big model requested while small ones are warm → `oversubscribed` → clean swap, no OOM.
4. `ROZUM_MULTISLOT` **off** ⇒ identical to today (regression check).

## Out of scope (tracked elsewhere)

- A shared **cross-resident GPU admission** gate (two residents both prefilling contend for one
  Metal device) — `concurrency-multi-instance`.
- Cross-process coordination — `concurrency-cross-process`.
- Making `active.json` a multi-model discovery surface — only if a real need appears.
