# Program: max safety × max capability × max performance × flexibility

Operator goal (2026-06-22): **run as many models at once as we need and as the hardware
can hold — and GUARANTEE no problems (never OOM/reboot/corrupt).** Maximize, in priority
order, **safety ≥ capability ≥ performance ≥ flexibility**. Method: enumerate every
obstacle to that goal and remove it. This doc is the living **obstacle register** + the
proposed path. Companion to `docs/specs/safe-multi-model-residency.md` (the residency
mechanism) and `SPEC.md` § North Star.

## The core insight (what "guarantee" requires)

Today safety is **estimate-based**: admission reserves a *predicted* footprint and refuses
if the sum exceeds a budget. That is necessary but not a *guarantee* — an estimate can be
wrong (a pathological prompt, a backend without a cache bound, a model that genuinely needs
more than predicted). A guarantee requires **measured closed-loop control**: continuously
watch the *actual* host free RAM + per-model usage and *act* (shed load) before the danger
threshold — so reality, not a prediction, is the safety authority. **That feedback governor
is the single highest-leverage missing piece**, and it is what lets us safely push capability
(more co-residency) and performance (higher budgets) without fear.

## Obstacle register (grouped; status: ✅ done · 🔜 in flight · ⛔ open)

### A. Memory safety (the reboot domain)
- ✅ Host residency admission gate (BUG-003 v1/v2) — refuse-before-load, flock ledger.
- ✅ Conservative calibrated footprint (smmr-B) + cache-tied reserve + budget 0.75 (smmr-D).
- ✅ Cap semantics understood: `set_memory_limit` SOFT, `set_cache_limit` bounds cache,
  admission is the lever ([[reference-mlx-memory-cap-semantics]]).
- 🔜 **Measured feedback** — admission is open-loop; the closed loop is **`rozum-core::shed`**
  (sunny-civet, on master): a watchdog in the gateway lifecycle loop that keys on the OS
  jetsam ladder (`kern.memorystatus_vm_pressure_level`) and, under real host pressure,
  unloads this gateway's own idle model (act). nimble-raven added `/stats memory_pressure`
  observability. REMAINING: cross-process utility-ranked eviction (gated on residency-unify).
  (My earlier parallel `govern` module was removed — `shed`'s OS signal + watchdog placement
  are better; we converged. Room MCP being down caused the brief duplication.)
- ⛔ **Static worst-case KV reservation** blocks co-residency of models that won't fill
  their context. → elastic/lazy KV accounting + governor-driven eviction under real pressure.
- ⛔ **`footprint-before-download` footgun** — uncached model over-reserves, blocks others
  (SPRINT task). → estimate after resolve.
- ⛔ **Non-MLX backends (gguf/mistralrs) have no cache bound** → their co-residency is
  admission-only, no enforcement. → per-backend memory bounding, or single-flight them.
- ⛔ **A genuine single-model OOM still panics the kernel** (Metal, process-fatal → host
  reboot). → chunked-everything (prefill done) + OS-level containment: set the gateway's
  jetsam priority / an RLIMIT so the OS kills *our* process (recoverable) instead of
  panicking; the governor sheds before that.

### B. Capability (as many models as fit)
- ✅ Co-residency on by default, budget-gated; two small models now co-reside.
- 🔜 **Fast safe swap** for the doesn't-fit case (smmr-C, sunny-civet; page-cache prewarm landed).
- ⛔ **Co-residency = N separate gateway processes** (one model each) — heavy + coarse
  cross-process flock accounting. → **unify into the in-process Switchboard** (`plan_residency`
  + utility eviction already exist) so one process holds N models with exact shared accounting
  and instant in-process swap; keep the flock ledger only as the cross-process backstop.
- ⛔ **No request→model routing policy surfaced** (cascade/router exist internally). → expose
  a routing/utility policy so "run what's needed" is automatic.

### C. Performance
- ✅ Batched decode (shipped); chunked prefill (shipped).
- ⛔ **GPU is time-shared** — co-resident models contend; concurrent full-throughput is
  impossible on one GPU. → co-residency targets *latency / instant availability*; schedule by
  priority; throughput stays per-model via batching.
- ⛔ **Cold-load / swap latency** → fast-swap (B) + page-cache prewarm + keep-hot residency.
- ⛔ **Prefix-cache reuse** across turns underused → reuse (hybrid path has it; generalize).

### D. Flexibility (any hardware, any model)
- ⛔ **MLX/Apple-Silicon is the only first-class engine** (x86/CUDA/CPU are stubs). → the
  `LocalEngine` SPI + `rozum-hardware` (workspace Phase 4) so residency + governor logic is
  hardware-agnostic; per-hardware memory-accounting adapters (MLX/CUDA/host-RAM differ).
- ⛔ **Memory semantics are hardcoded MLX** (the cap findings) → abstract "memory accounting"
  behind a trait the governor consumes.

## Proposed path (my recommendation)

Sequence by leverage-on-the-guarantee, safety first:

1. **Memory governor (the guarantee) — `rozum-core::shed`, LANDED.** The gateway lifecycle
   watchdog keys on the OS jetsam ladder (`kern.memorystatus_vm_pressure_level` — the kernel's
   own signal, better than a homemade free-bytes estimate) and, under real host pressure,
   unloads this gateway's idle model (lazy-reloads on the next request) → a reboot becomes
   graceful degradation. Observable via `/stats memory_pressure` (normal/warn/critical).
   REMAINING: cross-process, utility-ranked eviction (which model sheds, not just "self if
   idle") — folds into the Switchboard unify (step 3).
2. **OS-level containment.** Jetsam priority / RLIMIT_AS on gateway processes so a breach
   degrades to a recoverable process kill, never a kernel panic. Cheap, huge safety upside.
3. **Unify residency in-process (Switchboard).** One gateway, N models, exact accounting,
   instant swap — supersedes process-per-model. Folds B's co-residency + C's fast-swap.
4. **Elastic KV + footprint-after-resolve.** Remove the static-worst-case and footgun
   obstacles → more co-residency at the same safety.
5. **Hardware abstraction (`rozum-hardware`, Phase 4).** Generalize to x86/CUDA/CPU.

Each step is **measured + matrix/probe-gated**; the governor (1) is the backstop that makes
the later, more aggressive capability/performance steps safe to take.

## Done-when (program-level)
The operator can launch any set of models; rozum admits exactly those that fit, keeps the
most useful resident, swaps the rest in fast, and **a measured governor guarantees the host
never crosses the danger threshold** — on whatever hardware is present. Obstacles above all ✅.
