# GatedDeltaNet "needs per-call eval" bug — bottom-up Python↔Rust log

Goal: find why the `gated_delta` custom metal-kernel needs a blocking per-call
`eval` inside the full Qwen3.6 forward (else garbage from token 2), while Python
`mlx_lm` runs the SAME kernel with no per-call eval and is correct.

Method (per user, 2026-06-12): build the SAME absolutely-minimal program on **raw
MLX** (no mlx_lm, no our mlx-lm crate, no server) in **Python and Rust**, starting
from the lowest level, climb level by level, compare at each. The level where Rust
needs a per-call eval but Python doesn't = the culprit. Log everything.

Versions pinned EQUAL to remove that confound: **MLX 0.30.6** both sides.
- Python: `/tmp/mlx306/bin/python` (pip `mlx==0.30.6`).
- Rust: fork `.vendor/mlx-lm` @ `rozum-hybrid-decode`, mlx-sys @ MLX 0.30.6.

## Already established (don't re-test)
- Removing the per-call eval = +30-40% decode (12.9→16.1 t/s on 27B); it IS the gap.
- NOT fixed by: async_eval per call; end-of-token eval of states as graph outputs;
  decode pipelining (real path); genuine clean MLX 0.31.2. All still garbage.
- Ruled out by code: T-as-Array (MLX scalar-by-value keys on ndim==0); input
  contiguity (ensure_row_contiguous=true).
- Instrumented MLX 0.30.6 MetalAllocator on the REAL 27B: state_out is NEVER reused
  while held (0). So NOT a state_out buffer donation.
- Real-model eval-subset A/B: evaling ANY live per-layer intermediate fixes it
  (kernel inputs q/k/v alone, or g/beta alone, or either output); evaling the
  already-concrete cache `state` does NOT. ⇒ what matters is a per-layer GPU SYNC.
- NOCACHE (disable buffer reuse) does NOT fix. MLX_MAX_OPS_PER_BUFFER=2e9 (one
  command buffer) does NOT fix.
- Rust standalone repro (`gated_delta_donation_repro`): kernel + conv cache + KV
  concat + MLP + 4-bit quant + real dims (d=5120, 64 layers) + 8 tokens, NO per-call
  eval → byte-exact (max|Δ|=0). Only the FULL real 27B/35B model triggers garbage.

## The open question this log attacks
Rust synthetic (even full-fidelity) is CORRECT without per-call eval. The real model
is GARBAGE. mlx_lm Python (real model) is CORRECT without per-call eval (but it
PIPELINES via async_eval; our Rust real path is serial). So: is a minimal *serial*
*Python* kernel forward (no per-layer eval) correct? And does it stay correct as we
climb toward the real model — and where does Rust diverge from Python?

## Levels (identical program both sides)
- L0: kernel + plain matmuls, serial decode, NO per-layer eval. A/B per-call-eval.
- L1: + conv-cache concat (depthwise short-conv state).
- L2: + a growing ConcatKV cache on every 4th layer.
- L3: + MLP (large intermediate).
- L4: + 4-bit AFQ quantized matmuls.
- L5: real dims (d=5120, 64 layers).
- L6+: real ops — fast.rms_norm on q/k, RMSNormGated, real conv1d, partial RoPE
  attention, … then real loaded weights.

## Results

| level | Python 0.30.6 (no per-eval) | Rust 0.30.6 (no per-eval) | notes |
|-------|------------------------------|----------------------------|-------|
| L0 kernel+matmul, serial | **OK** (Δ=0) | **OK** (Δ=0) | `py/l0.py`; Rust = `gated_delta_donation_repro` (its minimal form). Both correct without per-call eval. |
| L1..L5 +conv +KV +MLP +4bit-quant +real-dims | (Rust already OK at full synthetic) | **OK** (Δ=0) | Rust repro carries all of these and is byte-exact. So the bug is a REAL op the synthetic lacks. |

## ★★ ROOT CAUSE FOUND (2026-06-12) — unretained command-buffer references ★★
MLX's metal `Device::get_command_buffer` uses
`queue->commandBufferWithUnretainedReferences()` — Metal does NOT keep the buffers
referenced by the command buffer alive; MLX relies on its OWN lifetime tracking.
For the `gated_delta` custom kernel in the large real-model graph, a kernel INPUT
buffer is freed (and its memory reused/released) BEFORE the in-flight GPU dispatch
reads it → garbage from token 2.

DECISIVE TEST: patched `get_command_buffer` to `queue->commandBuffer()` (RETAINED
refs) under `ROZUM_RETAIN=1`. Then `ROZUM_GD_NONE=1 ROZUM_RETAIN=1` (NO per-call
eval) on the real 27B → **"Here's a thinking process:" (CORRECT)**. So retained
references alone fix it, no eval needed.

This explains ALL prior evidence:
- per-layer eval (commit+WAIT) = GPU finishes before the buffer is freed (workaround);
- NOCACHE / one-command-buffer don't fix it (it's premature FREE, not reuse-from-cache
  or buffer boundaries);
- the standalone repro is too small / low-memory to free a buffer before the GPU
  reads it, so it never reproduced;
- evaling ANY live per-layer intermediate fixes it (forces that layer's GPU to finish).

SPEED: `ROZUM_GD_NONE=1 ROZUM_RETAIN=1` decode bench 27B: **serial 16.0 / pipelined
17.3 t/s** (n=128), 16.1 (n=512) — vs eval-ON baseline 12.9/10.9. So retained refs +
drop the per-call eval = **+25-40% decode AND correct.** The shippable win.

TARGETED FIX ATTEMPT — FAILED. Retaining the custom kernel's own inputs
(`add_temporaries(checked_inputs)`, `ROZUM_RETAIN_IN=1`, unretained cmd buf) →
STILL garbage. So the prematurely-freed buffer is NOT a direct kernel input — it's
some UPSTREAM op's buffer (a producer of the kernel's input, computed wrong because
ITS input was freed). Only GLOBAL retained references (`commandBuffer()`) catch it.
So the bug is a general unretained-reference premature-free in the large graph that
becomes VISIBLE through the custom kernel (its output feeds the recurrent cache, so
the corruption persists across tokens instead of washing out). Dense models (no
custom kernel) don't show it — TBD why (pipelined? corruption washes out?).

SHIPPABLE FIX = global retained command-buffer references + drop the per-call eval.
MLX-core one-liner (`mlx/backend/metal/device.cpp::get_command_buffer`):
`queue->commandBufferWithUnretainedReferences()` → `queue->commandBuffer()`.
Saved as `mlx-retain-command-buffer.patch`. Decode 27B: **16-17 t/s vs ~12 baseline
(+30-40%), correct.** prefill ~10% slower (retain overhead, n=512: 150 vs 165 tok/s).

## FINAL — root cause + fix (settled)
ROOT CAUSE: MLX metal `commandBufferWithUnretainedReferences()` (Metal doesn't keep
referenced buffers alive; MLX tracks lifetimes itself). In the large Qwen3.6 forward
an UPSTREAM buffer feeding the gated_delta kernel's input is freed/reused before the
in-flight GPU dispatch consumes it → garbage from token 2. Visible only via the
custom kernel (its output feeds the recurrent cache → corruption persists). Confirmed
by switching to retained refs (`commandBuffer()`) → correct with NO per-call eval.
NOT fixed by: retaining the kernel's own inputs (upstream buffer, not the inputs);
NOCACHE; one command buffer; async/end-eval/pipelining; clean 0.31.2.

SHIPPING OPTIONS (open, for the human):
1. MLX-core patch via FetchContent PATCH_COMMAND in the mlx-c submodule's CMake
   (MLX core is FetchContent'd, not in the submodule) — unconditional retained refs;
   measure dense-model regression first.
2. Gated: a runtime flag in MLX core (settable from Rust via a new mlx-rs fn) so the
   backend turns retained refs ON only for qwen3_5/qwen3_5_moe; dense stays
   unretained. + drop the gated_delta per-call eval for hybrid. Cleanest, more work.
3. Upstream: file at ml-explore/mlx (custom metal_kernel + unretained refs +
   large/serial graph → premature input-buffer free). Keep the per-call eval until
   fixed upstream.

How to reproduce the fix (current build): `ROZUM_GD_NONE=1 ROZUM_RETAIN=1` env on any
hybrid run = no per-call eval + retained refs = correct & fast. (Env hooks live in
the gitignored `_deps` MLX build copy; the patch file is the persistent artifact.)

## "How does it work in Python?" — direct A/B (2026-06-12)
THE decisive one: ran the REAL Qwen3.6-27B in pure Python via mlx_lm (model only),
with MY OWN loop = our exact Rust pattern: forward → eval(token) → argmax → repeat,
**serial, NO per-call eval, NO pipelining** (`py/real_serial.py`):
- **Python serial, no-eval → CORRECT** ("Here's a thinking process:"). Pipelined too.
- Rust serial, no-eval → garbage. SAME pattern, same unretained MLX.
⇒ It is NOT pipelining (Python is correct serial too). It's a genuine
Python(mlx_lm) vs Rust(our model + mlx-rs) difference at the MLX op level.

Ruled OUT as the difference (each tested directly on the real 27B, no-eval, no retain):
- pipelining — Python serial correct, Rust pipelined garbage.
- host-array lifetime — holding q,k,v,g,beta,z,conv_out,qkv alive (`ROZUM_GD_HOLDALL`)
  did NOT fix Rust. So it's not those arrays being dropped early.
- conv-cache contiguity — storing the conv cache as a contiguous copy (Python does
  `mx.contiguous`, we stored a view) did NOT fix Rust.
- threading — ran the real decode on the MAIN thread (examples/lm hacked to qwen3_5):
  Rust main-thread no-eval STILL garbage `[284,198,3840,198,91,91,…]`. Not a worker-
  thread artifact.

STILL OPEN: the exact op-graph difference. mlx_lm's Python model emits an MLX graph
that doesn't free the offending buffer; our Rust/mlx-rs model's graph does. Both
compute byte-identical values (validated with eval). Finding it needs op-level graph
diffing of the two runtimes — deep. The PROVEN fix (global retained refs, +30%) does
not depend on it.

## (superseded) Plan from here: add REAL ops to the Rust repro until it breaks
Real qwen3_5 GatedDeltaNet ops my synthetic lacks, in suspicion order:
1. `fast::rms_norm` (weightless) on q,k right before the kernel.
2. RMSNormGated on the kernel output (z gate).
3. real depthwise `Conv1d` short-conv (not a plain matmul).
4. the every-4th full-attention layer: `fast::sdpa` + partial RoPE + KV cache.
5. the lm_head projection / real vocab.
Each: add to Rust repro → run; if garbage, found it, then confirm Python w/ same op
is correct (⇒ Rust/mlx-rs-specific). If correct, next.

