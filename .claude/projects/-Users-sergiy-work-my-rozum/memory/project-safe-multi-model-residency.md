---
name: project-safe-multi-model-residency
description: ACTIVE project — run multiple models concurrently (when they fit) or fast-swap, with no-OOM/no-reboot as the hard invariant. smmr A+B done; D next.
metadata:
  type: project
---

Operator vision (2026-06-22): **run several models at once when they fit, or swap
between them very fast when they don't — safety (never OOM/reboot) is the HARD
condition.** North-Star residency. Spec: `docs/specs/safe-multi-model-residency.md`.
Follows the reboot fix [[project-reboot-watchdog-oom]] (v1 single-flight `3bcee03`,
v2 RAM-ledger `644e8e8` by sunny-civet).

**Key finding (the safety crux):** real peak resident is **MLX-cache-dominated, NOT
weight-proportional** — measured from matrix glogs, a 4B model (weights ~2.5 GB) peaks
**26.9 GB**, a 0.5B hits 14.9 GB, because MLX caches into the per-process cap
`total−8 ≈ 28 GB`. So v2's weight-based footprint estimate under-counts small models
~6× → two "small" models pass admission but reboot the host. The fix is the **hard
cap**, not a bigger estimate.

Owners (rozum room): admission *mechanism* = sunny-civet (v2 ledger); *numbers* + cap +
validation = me (nimble-raven). DONE on master:
- **smmr-B** (`d7fd456`): `rozum_models::runtime_footprint_bytes(spec,n_ctx,weight)` =
  weights + `kv_bytes_per_position(config)·n_ctx` + activation reserve(max 3 GiB, w/5).
  The model's NEED (small for small models), reusing existing `kv_bytes_per_position`.
- **smmr-A** (`95b98d6`): each gateway PROCESS hard-caps its own MLX `set_memory_limit`
  at its model's reservation, set before the worker loads. Design: v2 co-residency = N
  separate processes (1 model each) → cap = own reservation (NOT budget−committed);
  admission's `Σ reservations ≤ total×FRAC` ⇒ `Σ caps ≤ budget` for free. So co-residency
  is structurally safe (enforced, not estimated). `rozum-mlx::set_memory_cap_bytes` +
  pure `select_mlx_mem_limit_bytes` (env `ROZUM_MLX_MEM_GB` > smmr share > total−8).
  Interim 14 GB floor (`40048ba`) dropped once the cap enforces.

**REMAINING:** smmr-D (live validation — needs the model slot: prove host peak ≤
total×FRAC across co-residency + a swap, no self-OOM at the cap; then FRAC can rise) and
smmr-C (fast safe swap: unload→GPU-settle→load, page-cache-warm weights, never transient
both-resident). **Known gap:** the hard cap is MLX-only; gguf/mistralrs co-residency is
unenforced (estimate only) — follow-up.

**Why/how to apply:** the no-reboot invariant holds because Σ per-process caps ≤ budget,
enforced by `set_memory_limit`. Capping a model *below* its need self-OOMs the process
(Metal OOM = process-fatal but **contained**, not a reboot) → smmr-D calibrates B's
reserve. Coordinate the single model slot (room down? use the board + `ps`/lockfile
check from `SPRINT.md` 🛑 REBOOT-SAFETY PROTOCOL) before any live run.
