# Spec: safe multi-model residency (co-resident when it fits, fast swap when it doesn't)

Status: 2026-06-22 — **A + B LANDED then CORRECTED** (`d7fd456` smmr-B, `95b98d6` smmr-A,
cap-semantics fix follow-up). The structural safety lever is conservative **admission**
(the v2 RAM-ledger), NOT a per-process cap — `set_memory_limit` is soft (Findings below).
A = soft hint; B's footprint now budgets the cache term. **smmr-D (live measurement of
active-vs-cache) is the open item that confirms whether co-residency is truly safe**; C
(fast swap) remains. Operator vision: **run several models at once when they fit, or swap
between them very fast when they don't — with safety (never OOM/reboot) as the HARD
invariant.** North Star (device-aware residency; `SPEC.md` § North Star, memory
`project-rozum-north-star`).

Owners (rozum room, n=25–42): admission *mechanism* = `sunny-civet`; admission
*numbers* + per-process cap + safety validation = `nimble-raven`. Builds on the
reboot fix (`docs/specs/gateway-residency-singleflight.md`, memory
`project-reboot-watchdog-oom`).

## The hard invariant

> At no instant may the sum of all resident models' real memory use exceed a safe
> fraction of host RAM. Violations on this box do not degrade — they **reboot the Mac**
> (watchdog kernel panic from vm-compressor exhaustion; proven 2026-06-22). Therefore
> safety is enforced *structurally* (admission + per-process hard caps), never by
> discipline, and every estimate errs **conservative** (footprint rounded up, budget
> rounded down).

## Where we are (master 644e8e8)

- **v1** (`3bcee03`): host-wide `flock` single-flight — one resident model, 2nd refused.
- **v2** (`644e8e8`): RAM-ledger — each gateway reserves an *estimated* footprint
  (`residents/<pid>` flock file) before load; admit iff **sole OR `in_use + footprint ≤
  total_ram × RAM_BUDGET_FRAC` (0.65)**. Footprint = `main.rs::estimate_model_footprint_bytes`
  = catalog `size_bytes × inflate + base`. Co-residency is now **on by default**, budget-gated.

## The safety gap (why v2 alone can still reboot) — DATA-BACKED

Two coupled problems, both measured from real runs (`scripts/bench/results/**/*.gateway.log`,
`/usr/bin/time -l` peak resident; and the reboot's JetsamEvent 24.8/18.7/18.0 GB):

**1. Peak footprint is NOT a function of model size — it's dominated by the MLX cache.**
The per-process MLX cap is `total−8 ≈ 28 GB` (`cap_mlx_memory`,
`crates/rozum-mlx/src/mlx_native_backend.rs:~363`); MLX caches freed Metal buffers up to
that, *regardless of model size*. Measured peak resident (MAX across runs):

| model (4-bit) | on-disk weights | measured peak resident | peak ÷ weights |
|---|---|---|---|
| Qwen2.5-0.5B | ~0.4 GB | **14.9 GB** | ~37× |
| Qwen3-4B | ~2.5 GB | **26.9 GB** | ~11× |
| Qwen2.5-Coder-7B | ~4.3 GB | **26.9 GB** | ~6× |
| gpt-oss-20b MXFP4 | ~11 GB | 13.2 – **21.5 GB** | ~2× |
| Qwen3.6-27B | ~15 GB | 14.3 – **24.7 GB** | ~1.6× |
| Qwen3-30B-A3B | ~17 GB | 19.1 – **25.9 GB** | ~1.5× |
| Qwen3.6-35B-A3B | ~19 GB | 18.4 – **25.2 GB** | ~1.3× |
| GLM-4-32B | ~18 GB | **25.4 GB** | ~1.4× |

So `estimate = size_bytes × inflate + base` is structurally wrong for small models: a
0.5 B model is "tiny" by weights but can sit at ~15 GB resident. **~~The dominant lever is
the cap, not the estimate.~~** (CORRECTED — see Findings: there is no hard cap;
`set_memory_limit` is soft. The lever is conservative **admission** on an estimate that
explicitly budgets the cache; `set_cache_limit` bounds the cache term.)

**2. v2 admits by estimate but the cap is still per-process `total−8` (v3 deferred).**
Two "small" models pass the budget by their (under-)estimates, then each balloons toward
~28 GB via the uncapped cache → host overcommit → reboot. v2's `RAM_BUDGET_FRAC` 0.65
helps (won't admit two big models) but does **not** close the small-model cache-balloon
hole. **This is the present, live risk on master.**

## Design — three coupled pieces, safety first

### A. MLX soft memory-limit hint — ✅ LANDED + CORRECTED (`95b98d6` + cap-semantics fix) — `nimble-raven`
> **Corrected per the Findings below** (sunny-civet's allocator audit, which I independently
> re-verified against the pinned fork mlx-rs `12fac5c`, `memory.rs:64`: *"Set the **soft**
> memory limit … allocations beyond it wait or relax rather than grab more"*). So
> `set_memory_limit` is **not** a hard cap — it evicts cache / waits but still allocates. A is
> therefore **defense-in-depth** (a hint nudging MLX to stay near the model's share), NOT the
> enforcement. The structural safety lever is conservative **admission** (B's footprint ≤
> budget); the real per-model bound on the *cache* term is `set_cache_limit`.
- `rozum-mlx`: `set_memory_cap_bytes` + pure, unit-tested `select_mlx_mem_limit_bytes`
  (precedence: explicit `ROZUM_MLX_MEM_GB` > smmr share > default `total−8`); docs relabeled
  "soft hint". `set_cache_limit` (default 4 GiB) stays the real cache bound.
- `main.rs::try_build_mlx_native_backend` sets the soft limit = the residency reservation
  (same estimate), before the worker loads.
- **Known limitation:** even this hint is MLX-only; `gguf`/`mistralrs` have no equivalent →
  their co-residency rests on admission alone (follow-up).

### B. Conservative, calibrated footprint — ✅ LANDED + CORRECTED (`d7fd456` + cache-term fix) — `nimble-raven`
`rozum_models::runtime_footprint_bytes(spec, n_ctx, weight)` = `weights +
kv_bytes_per_position(config)·n_ctx + activation_reserve`, reusing the existing
`kv_bytes_per_position` (hybrid-aware). **Because admission (not a hard cap) is the safety
lever, this MUST be ≥ the model's real resident peak = active (weights+KV+prefill) + the
bounded cache.** Per the Findings' recommendation, the reserve now explicitly budgets the
cache: `activation_reserve = max(6 GiB, weights/4)` ≈ ~4 GiB cache (`set_cache_limit`) +
~2 GiB prefill — fixing the original 3 GiB catch-all that was *smaller than the cache limit
alone* (the bug the audit flagged). Unit-tested; interim 14 GB floor (`40048ba`) dropped.
> **Open (decided by smmr-D):** whether this reserve is truly ≥ real peak hinges on the
> active-vs-cache split (Findings "crux"). If peak is cache-dominated, B+`set_cache_limit` is
> safe; if active-dominated, co-residency must stay single-flight + fast-swap (C) until
> prefill is provably bounded. D measures `get_active` vs `get_cache` vs RSS live.

### C. Fast safe swap (the "very fast sequentially" half) — `sunny-civet` (claimed)
When the ledger says `oversubscribed` (two big models can't co-reside), swap — but
**never transiently resident-both** (that simultaneous footprint is the OOM). This is
likely the **higher-value lever** than A/B on this box: the 27–35B agentic models can't
co-reside on 36 GiB by *need* alone (weights ~18 GB + KV), so the common case is swap, not
co-residency.

**Invariant:** at no instant are both models GPU-resident. Sequence:
1. Drain the old model (in-flight requests finish; reuses `gateway switch`'s clean drain).
2. **Free** the old model + GPU settle (Drop joins the MLX worker; `GPU_SETTLE`,
   `project-matrix-kernel-panic`) — old residency reservation released here.
3. Load the new model (acquires its reservation).
Dead time = (drain tail) + free + (cold load). The lever C optimizes is the **cold load**,
whose cost is dominated by reading weights off disk.

**Page-cache prewarm (LANDED, slot-free) — `rozum-core::prefetch::warm_dir_page_cache`.**
Reads the next model's files into the OS page cache so step 3 reads weights from RAM, and
runs it *during* steps 1–2 (the old model's drain/settle) to overlap the fetch with
otherwise-idle time. Page cache is **reclaimable** and is **not** GPU residency, so a
prewarm never counts against the RAM budget (no overcommit risk — that is what makes it
safe to overlap). Best-effort + cancellable (abort the moment the old model finishes
draining). Portable sequential `read()` v1; `madvise(WILLNEED)` is a later optimization.
Unit-tested (sums regular files, skips subdirs, cancel aborts, missing-dir no-op).

**Prewarm wired into the swap — ✅ DONE (`sunny-civet`).** `Switchboard::switch` now spawns
`prefetch::warm_dir_page_cache(new_model_dir)` (fire-and-forget) **before** `begin_drain`,
so the new model's weights warm into the OS page cache *during* the drain — the rebuild
below then reads from RAM, not disk. The safe ordering was already correct (drop old at
`*backend = None` **before** rebuilding — never two resident); the prewarm only adds
overlap, and page cache is reclaimable + off-budget so it can't overcommit while the old
model is still resident during the drain. `ROZUM_SWAP_PREWARM=0` disables; only warms an
already-cached model. Additive, gated; 4/4 switch tests still pass.

**Remaining (slot-gated):** measure swap latency with vs without prewarm on two real
models (load A → switch to B, time the rebuild); and the `oversubscribed`-triggered
auto-swap (reuse `plan_residency` `oversubscribed`) is a separate follow-up.

### D. Safety-validation harness — `nimble-raven` (capstone, needs the model slot)
Prove the invariant holds end-to-end: drive admission across co-residency + swap and
assert host peak RAM never exceeds the safe fraction. SAFELY: derive admission from the
calibrated footprints; the worst case to *measure* is one admitted set at a time. Never
load an un-admitted combination to "see if it reboots."

**Progress (nimble-raven, 2026-06-22):**
- ✅ **Raw-alloc probe run live** (`mlx_mem_probe`, slot-free): `set_memory_limit=512MB`
  yet live `active=2048MB` (4× past it) → **`set_memory_limit` is SOFT, confirmed
  empirically** (matches the source audit); after dropping the arrays `cache` settled to
  exactly `set_cache_limit` → **`set_cache_limit` is the real cache bound, confirmed**.
- ✅ **Admission gate validated live (incidental):** while a sibling's 35B was resident
  (30.7 GB reserved), an attempt to load a 4B was **refused by the gate** ("would
  overcommit … budget ~23961 MB"), not allowed to co-load → no reboot. The structural
  lever (admission) demonstrably works under real concurrent load.
- ✅ **Model-mode RAN (Qwen3-4B, exclusive slot, 2026-06-22) — crux SETTLED.** cap applied
  live (`mem(soft)=14077MB cache_limit=4GB`, confirms smmr-A wiring). Real resident: after
  load `active=2159MB cache=0`; after 3×500-tok gens `active=2267MB peak=2310MB cache=255MB`;
  process RSS 2.32 GB. **The 4B uses ~2.3 GB, NOT 26.9 GB; cache settled at 255 MB ≪ the
  4 GB `set_cache_limit`.** So the historical 26.9 GB does NOT reproduce under the current
  `set_cache_limit` — it was uncapped-cache from old runs, not a live risk. **VERDICT:
  resident = active (weights + KV, bounded by `n_ctx`) + cache (bounded by `set_cache_limit`);
  neither is unbounded ⇒ co-residency is SAFE by construction** (admission reserves ≥ real
  peak). Caveat: a big-model FULL-context/full-prefill peak wasn't directly measured (the
  structural bound + chunked prefill cover it; nice-to-confirm later).
- ✅ **LIVE CO-RESIDENCY PROVEN (2026-06-22) — the headline goal end-to-end.** Two distinct
  models in two gateways (Qwen3-4B :8298 + GLM-4-9B :8299, n_ctx 8192): B was **admitted
  alongside A → both resident, both served chat simultaneously** (reply ok each); host free
  RAM 26.6 → **21.1 GB with both loaded** (active 2159 + 5043 MB ≈ 7 GB), no danger; graceful
  SIGINT teardown (no SIGKILL) → 28.8 GB free. Multiple models run at once, safely.
- ✅ **IN-PROCESS CO-RESIDENCY PROVEN (2026-06-22) — validates the whole residency-unify stack live.**
  ONE gateway (Qwen3-4B primary + `ROZUM_WARM_MODELS=GLM-4-9B`, n_ctx 8192) held **both models in a
  single process**: `/stats resident_models = [4B, GLM-9B]`; **both served chat** (primary + warm);
  `memory_pressure=normal`; free RAM 21.9 → 16.8 GB w/ both loaded. The two `cap_mlx_memory` log
  lines — `smmr-share=8957MB` (4B) + `=11015MB` (GLM-9B) — show **U1's host-aware per-model footprint
  sizing working live** (each model's soft cap = its own footprint, Σ ≤ budget). So U1 (admission +
  caps + republished reservation), U2 (governor), and U3 (declarative preload + `resident_models`
  visibility) are all confirmed end-to-end. Graceful teardown → 24.9 GB free.
- 🆕 **Follow-up surfaced — footprint is OVER-conservative (`footprint-overconservative`,
  see task).** It reserved ~14 GB for the 4B (full-`n_ctx` KV ~4.8 GB + 6 GB reserve) vs
  ~2.3 GB real → two small models can't co-reside (28 > budget 23.4) even though they fit
  easily. Safe but defeats the operator's "multiple models at once" goal — tune the reserve
  / KV-at-realistic-ctx down to ADMIT more, keeping the `n_ctx`-bounded worst case as the ceiling.
- 🐞 **Footgun found — `footprint-before-download`** (see task): `estimate_model_footprint_bytes`
  runs *before* `ensure_model_dir`, so an **un-cached** model scans empty → returns the
  unknown-size sentinel (`u64::MAX/4`) → reserves ~4.4e12 MB for its whole process life,
  **blocking every other model load** (a tiny uncached 4B blocked a sibling's matrix).
  Safe (over-reserves) but badly over-conservative. Fix: compute footprint *after* the
  model dir is resolved/downloaded (size known). Touches the admission call site →
  coordinate with `sunny-civet` (ledger owner).

## Findings: MLX cap enforcement — source audit (`sunny-civet`, 2026-06-22)

Auditing the MLX metal allocator C++ (`mlx-sys` … `mlx/backend/metal/allocator.cpp`)
to verify A's enforcement claim. **The mechanism A and B rely on is not what enforces
the bound** — the *conclusion* may still hold, but for a different reason, and one
unverified fact decides it.

**`set_memory_limit` is SOFT — it does not cap a process (source-proven).**
- `set_memory_limit(limit)` sets `block_limit_` and derives `gc_limit_ =
  min(block_limit_, 0.95·recommendedMaxWorkingSetSize)` (allocator.cpp:76-83).
- In `malloc` (allocator.cpp:96-164), `block_limit_`/`gc_limit_` are used in **exactly
  one place**: `if (mem_required >= gc_limit_) release_cached_buffers(...)` (124-127) —
  it **frees cache**, then **allocates the buffer anyway** (`device_->newBuffer`, 141).
  The only hard failures are the per-buffer `maxBufferLength` (103) and a null from
  `newBuffer` = **physical** device OOM (143-146). So a process's *active* memory grows
  unbounded up to physical RAM regardless of `set_memory_limit`; mlx-rs documents it
  plainly as the "**soft** memory limit … allocations beyond it wait or relax"
  (`mlx-rs/src/memory.rs:63`).
- ⇒ A's `set_memory_limit(reservation)` is **not** a "Hard memory ceiling" (its code
  comment) and does **not** "enforce" the footprint (B's premise). Capping `set_memory_limit`
  to the share just makes a process start evicting cache sooner. **No MLX API hard-caps a
  process below physical RAM** (there is no fail-fast cap), so per-process caps cannot *be*
  the safety guarantee — conservative **admission** (B) is the only structural lever.

**What actually bounds resident footprint: `set_cache_limit` (cache only).**
`set_cache_limit(limit)` sets `max_pool_size_` (allocator.cpp:70-74); `malloc` keeps
`get_cache_memory() ≤ max_pool_size_` (159-162). So the **cache** term is genuinely
bounded; the **active** term (weights + KV + prefill activation) is not bounded by
anything but the model itself + chunked prefill.

**The crux that decides B's safety (unverified): is the uncapped balloon CACHE or ACTIVE?**
B sets `footprint = need (~6 GB for a 4B)`, declaring the uncapped ~27 GB "prevented by
the cap." Resident = `active + cache`. Two cases:
- **If the 27 GB is cache** → `set_cache_limit` bounds it → real peak ≈ `need + cache_limit`
  → B is ~right but **under-counts by the cache term**: `cap_mlx_memory` allows a 4 GB cache
  (`ROZUM_MLX_CACHE_GB` default 4), yet B folds cache+activation into a **3 GB** floor
  (`activation_reserve = max(weights/5, 3 GiB)`) — smaller than the cache limit alone. So B
  should add the explicit `set_cache_limit` bytes, not a 3 GB catch-all.
- **If the 27 GB is active** (KV + prefill activations) → **nothing bounds it** (soft
  `set_memory_limit`, and `set_cache_limit` only touches cache) → B's ~6 GB severely
  under-counts → admitting a 2nd model can still overcommit → reboot.
- **Contradiction to resolve:** `cap_mlx_memory` already sets `set_cache_limit=4 GB` on
  **every** load (`mlx_native_backend.rs:319,1801`). If that was in effect when the table
  was measured, a 0.5B model could not sit at 14.9 GB *of cache* — pointing at the active
  case (dangerous) **or** at the measurements predating/bypassing the cap. Either way the
  table can't be taken at face value.

**Decisive measurement (smmr-D, `nimble-raven`, owns the slot):** load one model under
the live cap and read **`get_active_memory()` vs `get_cache_memory()` vs RSS at peak**
(`mlx-rs` exposes both; `reset_peak_memory`/`get_peak_memory` too). If peak is
cache-dominated and ≤ `set_cache_limit` over `need`, co-residency is safe with B + an
explicit cache term. If active-dominated, co-residency is unsafe and must stay
single-flight + fast-swap (C) until prefill activation is provably bounded.

**Recommendations (no code changed here — handing this to A/B's owner):**
1. Relabel A: it's a *cache-eviction hint*, not a hard ceiling; the bound is admission (B)
   + `set_cache_limit`, not `set_memory_limit`.
2. B: add the explicit `set_cache_limit` bytes to `runtime_footprint` (per resident),
   instead of a 3 GB floor that's below the cache limit.
3. Gate **default** co-residency on smmr-D's active-vs-cache result; keep it opt-in until
   then (matches "safety is the hard condition").
4. C (fast-swap) likely deserves promotion over A/B for the 27–35B agentic models, whose
   *need* alone (weights ~18 GB + KV) already precludes co-residency on 36 GiB.

**Empirical confirmation (slot-free, `crates/rozum-mlx/examples/mlx_mem_probe.rs`).** Ran
the raw-alloc probe: with `set_memory_limit=512 MB`, allocating 1024 MB of *live* f32
arrays gives `active=1024 MB` — **the limit was exceeded 2×**, so `set_memory_limit` is
**SOFT** (measured, not just read). With `set_cache_limit=256 MB`, after dropping the
live arrays the retained `cache=256 MB` exactly — so `set_cache_limit` **does** bound the
cache term. Source + measurement agree: the cap is `set_cache_limit` (cache only);
`set_memory_limit` does not cap a process.

**The probe is the smmr-D harness.** Its *model mode* (documented in the example header)
wraps a real model load + prefill with `reset_peak_memory()` → `get_active_memory()` /
`get_cache_memory()` / `get_peak_memory()` to split a model's peak into active vs cache.
Run it under the live cap (needs the slot) to make the co-residency decision:
```text
cargo run -p rozum-mlx --example mlx_mem_probe --features mlx-native   # raw-alloc (slot-free)
```

(Method: source read of the vendored MLX allocator + mlx-rs, plus a slot-free raw-alloc
measurement. The active-vs-cache split *for a real model* still needs the slot = D.)

## Findings: gguf/mistralrs are not an unenforced reboot vector (`sunny-civet`, 2026-06-22)

smmr-A's "Known limitation" says gguf/mistralrs co-residency "relies on the footprint
estimate without an enforced cap → follow-up". Investigated (idea #2): that **understates
their actual safety** — there is no MLX-style cache-balloon to cap, and admission already
covers them. So this is largely a **non-gap**; no per-process cap needs porting.

- **Admission is engine-agnostic.** `acquire_residency` (the ledger) runs in `run_gateway`
  / `run_launch_dedicated` **before** engine selection (`build_from_config` etc.), so every
  engine — MLX, gguf, mistralrs — reserves its estimated footprint and is refused on
  overcommit *at load time*. The reboot vector (BUG-003) is closed for all of them already.
- **Why MLX needed `set_cache_limit` and the others don't.** MLX's resident footprint is
  *cache-dominated* — freed Metal buffers are retained up to the (soft) limit, so a small
  model can balloon to ~the cap (the § Findings table). `set_cache_limit` bounds that.
  **gguf (candle) has no such retained-cache pool** — it allocates/frees per op, so its
  footprint ≈ weights + KV(n_ctx), which the admission estimate (`runtime_footprint_bytes`
  = weights + kv·n_ctx + reserve) already captures. There is nothing to balloon, hence no
  analog cap is required.
- **mistralrs is, if anything, *better* bounded than MLX.** `mistralrs_backend.rs`:
  PagedAttention pools the KV to `MemoryGpuConfig::ContextSize(n_ctx)` (a fixed block pool,
  not a soft hint); the auto device-mapper **refuses before weights load** when "model + KV
  exceeds Metal's working-set budget" and steps `n_ctx` down; `max_num_seqs = 1` serializes
  prefill; and `main.rs` runs a config-driven RAM preflight before the in-process load.
  Its KV — the term that grows at runtime — is hard-bounded by the paged pool, and the
  pool size ≈ the admission estimate's `kv·n_ctx`.

**Residual (smaller, real):** co-residency can't *pack as tightly* for gguf/mistralrs (no
cap to a sub-budget share like smmr-A does for MLX) — but since they don't balloon, their
actual footprint ≈ the reservation, so co-residency is as safe as the estimate (the same
basis smmr-D validated). If two non-MLX models ever need to co-reside tightly, the lever is
lowering their `n_ctx` (shrinks the paged pool / KV), not a cache cap. **Recommendation:**
correct smmr-A's "Known limitation" wording; treat gguf/mistralrs as admission-covered, not
"unenforced". No code change needed for safety; a `runtime_footprint` calibration check
against a measured gguf/mistralrs peak (like the MLX table) is the only optional follow-up.

## Acceptance / done-when

- **A:** with two ledger-admitted models co-resident, each MLX process's hard cap sums
  to ≤ `total × FRAC`; verified the peak host footprint stays under budget (no balloon
  past the share). Unit test on the share math; live check with a big+small pair.
- **B:** `runtime_footprint(spec, n_ctx) ≥ measured peak` for every model in the table
  (unit test reads the calibration constants); ledger uses it; no small-model under-count.
- **Invariant test (D):** no admitted scenario exceeds safe RAM; `cargo check` default &
  `--no-default-features` green; rozum-mlx + rozum-models + core suites pass.

## Interim safety (until A lands) — RAISE WITH OPERATOR
v2 co-residency is live but the cap is not share-aware, so a 2-small-model admit can still
balloon. Until A: either (i) keep `RAM_BUDGET_FRAC` low enough that even cache-ballooned
co-residents fit (very conservative, ~0.4 — but that mostly disables co-residency), or
(ii) keep co-residency opt-in (hard single-flight default) until the cap is safe. Prefer
(ii) — matches "safety is the main condition." A makes co-residency safe-by-default.

## Reboot-safety protocol
Unchanged and load-bearing — see `SPRINT.md` "🛑 REBOOT-SAFETY PROTOCOL": one slot claim
in-room + `ps`/lockfile check before any model load; never run two matrices at once.

## Shared-reserve accounting (admit more co-residents, still reboot-safe) — 2026-06-23
**Problem.** `runtime_footprint_bytes` = weights + KV + a 5.5 GiB activation reserve (MLX buffer
cache + prefill spike). The reserve is calibrated **per process**, but the in-process Switchboard
published its total reservation as `Σ runtime_footprint_bytes(model_i)` — i.e. one reserve **per
model**. The MLX buffer cache is a single process-global pool (`set_cache_limit` is process-wide)
and prefill serializes under `max_num_seqs`, so for N co-resident models only ONE reserve is
physically real. The naive sum over-reserved by (N-1)×5.5 GiB and made other gateways (and the
host ledger) needlessly refuse co-residents that actually fit — fighting the "run several models
at once" goal.

**Fix (numbers + U1 republish wiring — nimble-raven).**
- `rozum_models::model_source`: split `runtime_footprint_bytes` into `runtime_active_bytes`
  (weights + KV, the genuinely per-model part) + `process_reserve_bytes(max_weight)` (the shared
  cache+prefill pool, counted once). `runtime_footprint_bytes == active + reserve` exactly (unit
  test) → the single-model admission gate is byte-unchanged.
- `rozum-gateway::published_reservation(primary_fp, &warm_fps)` (both republish sites,
  `ensure_warm` + `sweep_idle_warm`): publishes `Σ fp_i − (N-1)·process_reserve_bytes(0)` =
  `Σ active_i + ONE reserve`. Subtracting the **smallest possible** reserve makes the published
  total **provably never below** the real co-resident peak (`Σ active + max reserve`), so admission
  stays reboot-safe — it just stops over-refusing. Single-model (no warm) ⇒ bare `primary_fp`,
  unchanged. Unit-tested (`published_reservation_counts_shared_reserve_once`).

**Effect.** Each extra in-process co-resident now needs only its own weights+KV (not +5.5 GiB),
and the cross-process `committed_by_others` total is accurate → siblings admit more too. On a
36 GiB host (~27 GiB budget) this is the difference between admitting a 2nd small model or not.

**Admission-mechanism wiring — DONE (sunny-civet, 2026-06-23).** `plan_residency` no longer sums
full per-model footprints. `ResidentRequest` gains a `process_reserve_bytes` input; the planner
bills each model's `weight − reserve` (its genuine `runtime_active_bytes`, since the caller still
passes full footprints) against `budget − one reserve`, charging the shared activation pool a
**single** time instead of once per co-resident. So one gateway's own multislot now admits as many
co-residents as `Σ active_i + ONE reserve ≤ budget` allows — no longer conservative by (N-1)
reserves.

The reserve is **injectable** (`WarmConfig.reserve`) so it stays consistent with the `weight`
model: production passes `process_reserve_bytes(0)` alongside `runtime_footprint_bytes` weights;
the reserve-less test stubs pass `0`. Crucially the full footprints are **unchanged** at the call
site, so the values flowing to `published_reservation` (the cross-process ledger) still carry their
reserve and stay **reboot-safe** — this change only relaxes the *in-process* admit decision.

Provably single-model-identical: the lone-request keep test `requested − reserve ≤ budget − reserve`
⇔ `requested ≤ budget`. Tests: `resident::tests::{shared_reserve_counted_once_admits_a_second_model,
single_model_gate_is_identical_with_or_without_reserve}` (unit) +
`gateway::tests::warm_admits_a_co_resident_by_counting_reserve_once` (end-to-end through
`ensure_warm`).
