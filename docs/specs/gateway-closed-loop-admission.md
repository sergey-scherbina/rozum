# Gateway closed-loop admission

**Status:** phase 1 (measured feedback + observability) SHIPPED; phase 2 (mid-load measured
abort) DESIGNED, not yet implemented.

## Problem

Admission is **open-loop**: before a load, the gateway *predicts* a footprint
(`runtime_footprint_bytes` = weights + KV, tightened toward prior measured peaks) and refuses
if it exceeds the host budget. A prediction errs in two directions:

- **over-refuse** — a model that would actually fit is refused → lost capability (e.g. the 35B
  refused at 21.6 GiB estimate vs 21.75 GiB free: we never learned if it would truly fit).
- **under-refuse** — the estimate is too low, the load proceeds, and the first prefill's transient
  activation spike drives real memory past RAM → kernel jetsam / **reboot** (BUG-003). This is the
  dangerous direction.

The `safe-multi-model-program.md` names the fix: "the GUARANTEE needs measured closed-loop control."

## What already exists (the measured-feedback half)

- **smmr-D**: on unload (`MlxNativeBackend::drop`) the backend records the REAL
  `get_active_memory()` + `get_peak_memory()` for `(model)`; `footprint::tighten` folds that into
  the NEXT admission estimate (capped at the structural estimate, floored at weights+full-KV). So a
  model's estimate self-corrects toward its observed peak across loads.
- **`shed` governor**: reacts to the OS jetsam pressure level AFTER load — under host pressure it
  unloads the idle model, turning a would-be panic into graceful degradation.

## Phase 1 — measured feedback + observability (SHIPPED)

At the existing Drop measurement, compare the REAL peak against the estimate basis this model was
admitted against (the prior recorded peak) and, when the real peak exceeded it, emit a
`footprint_underestimate { model, prior_estimate_mb, measured_peak_mb, exceeded_by_mb }` obs event.
`record_peak` (a running max) has already corrected the cache upward so the *next* load is safe;
the event surfaces that a correction happened so a persistent open-loop gap is visible in
`~/.rozum/gateway.jsonl` instead of staying silent until an OOM. Zero behavioural change to loads.

## Phase 2 — mid-load measured abort (DESIGNED)

The remaining gap is the **first-load blind spot** + the **under-refuse reboot**: a brand-new
`(model, n_ctx)` with no measured history rides a pure structural estimate; if it's too low, the
first prefill OOMs before any measurement is taken. Close it with a MEASURED cut-off during the
load itself, not a pre-load guess:

1. **Post-weights checkpoint.** After weights materialize but before the first prefill, read
   `get_active_memory()`. If `active + keep_free > total_ram`, the weights alone leave no headroom
   → the prefill *will* OOM → abort now (unload + clean `ResidencyDenied`-style error), converting a
   guaranteed reboot into a refusal. (Caveat: MLX is lazy — force an eval of the weights first, or
   sample after the first token, so the reading reflects real resident bytes.)
2. **Chunked-prefill watermark.** Prefill already runs in chunks (`prefill_chunk_size`). Between
   chunks, sample `get_active_memory()`; if a chunk pushes it past `total_ram - keep_free`, stop and
   fail the request BEFORE the next chunk allocates over the line. Bounds the transient spike by
   measurement, not prediction.
3. **Record eagerly.** Record the measured active/peak after the FIRST successful prefill (not only
   at Drop) so the calibration survives a later crash and the first observation is never lost.

**Why not rushed:** these touch the no-reboot invariant and the hot prefill path, and the failure
path (approaching OOM) can't be exercised without risking the host. Implement behind a flag
(`ROZUM_CLOSED_LOOP_ADMISSION`), validate on a machine that can safely be pushed to jetsam, and roll
out default-on only once the abort path is proven to fire before the kernel does.
