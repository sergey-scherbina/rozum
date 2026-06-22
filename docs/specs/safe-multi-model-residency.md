# Spec: safe multi-model residency (co-resident when it fits, fast swap when it doesn't)

Status: in progress (2026-06-22). Operator vision: **run several models at once when
they fit, or swap between them very fast when they don't — with safety (never
OOM/reboot) as the HARD invariant.** Maps to the North Star (device-aware residency,
remove waste/OOM; `SPEC.md` § North Star, memory `project-rozum-north-star`).

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
0.5 B model is "tiny" by weights but can sit at ~15 GB resident. **The dominant lever is
the cap, not the estimate.**

**2. v2 admits by estimate but the cap is still per-process `total−8` (v3 deferred).**
Two "small" models pass the budget by their (under-)estimates, then each balloons toward
~28 GB via the uncapped cache → host overcommit → reboot. v2's `RAM_BUDGET_FRAC` 0.65
helps (won't admit two big models) but does **not** close the small-model cache-balloon
hole. **This is the present, live risk on master.**

## Design — three coupled pieces, safety first

### A. v3 — share-bounded MLX cap (TOP PRIORITY; the missing safety net) — `nimble-raven`
Make `cap_mlx_memory` **sibling-aware**: instead of a flat `total−8`, cap each resident
model's `set_memory_limit` (and `set_cache_limit`) to **its share of the budget** =
`budget − committed_by_others` (from the v2 ledger), so the sum of all residents' hard
caps ≤ `total_ram × RAM_BUDGET_FRAC`. Then no set of co-resident models can collectively
exceed the budget *no matter how the cache grows* — the structural guarantee.
- Read `committed_by_others` from the ledger sunny-civet added (`share.rs`
  residents/<pid>); the bin passes the model's reserved share into the MLX worker.
- Floor each cap at the model's **minimum need** (weights + KV + prefill activation):
  capping *below* that makes MLX OOM the process (Metal OOM is process-fatal — memory
  `project-mlx-35b-prefill-oom`). So admission must also ensure each admitted model's
  share ≥ its minimum need (couples to B).
- Env overrides preserved (`ROZUM_MLX_MEM_GB`/`ROZUM_MLX_CACHE_GB`); default becomes
  share-derived, not `total−8`.

### B. Conservative, calibrated footprint — `nimble-raven`
Replace weights-only `estimate_model_footprint_bytes` with a calibrated
`rozum_models::runtime_footprint(spec, n_ctx)` that the v2 ledger calls:
`footprint = weights + kv_bytes(n_ctx, layers, kv_heads, head_dim, dtype) + activation/cache
reserve`, with a **minimum floor** so a small model is never under-counted (e.g. ≥ its
measured peak class). Calibrate so the estimate ≥ measured peak for *every* row in the
table above (validate against the logs as a unit test). Clean interface so sunny-civet's
ledger and the cap (A) share one footprint source (no double-owning the main.rs formula —
agreed in room n=42).

### C. Fast safe swap (the "very fast sequentially" half) — track, owner TBD
When the ledger says `oversubscribed` (two big models can't co-reside), swap — but
**never transiently resident-both** (that is the OOM). Safe-fast swap = unload old
(graceful teardown, GPU settle) → load new, minimizing dead time via: weights warm in
the OS page cache (mmap, no re-read from disk), fast graceful teardown, optional prefetch
of the next model's file into page cache *during* the old model's drain (page cache is not
GPU residency, so it doesn't count against the budget). Reuses `gateway switch` (clean
drain→swap) + `plan_residency` `oversubscribed`. Spec'd later; lower priority than A/B.

### D. Safety-validation harness — `nimble-raven` (capstone, needs the model slot)
Prove the invariant holds end-to-end: drive admission across co-residency + swap and
assert host peak RAM never exceeds the safe fraction. SAFELY: derive admission from the
calibrated footprints; the worst case to *measure* is one admitted set at a time. Never
load an un-admitted combination to "see if it reboots."

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
