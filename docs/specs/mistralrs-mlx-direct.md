# Native direct MLX quant-ops in mistral.rs

## Overview

Today mistral.rs runs MLX-community checkpoints through a **reimplementation**
of MLX's quantized math: candle drives the graph and `mistralrs-quant`'s own
Metal kernels (`metal_kernels/quantized.metal`, `call_afq_qmm`,
`call_affine_quantize`) perform AFQ dequant + quantized matmul. Every numeric
bug we have chased on Qwen3.6 — the RMSNorm `+1` convention, the
`new_private_buffer(0)` zero-buffer crash, the nibble-packing audit — exists
*only because* this reimplementation can silently diverge from Apple's real MLX
kernels. The reference (`mlx_lm`) runs on real MLX; our runtime does not.

This feature gives `mistralrs-quant` a **native direct MLX execution path** for
the AFQ-quantized hot ops. Instead of candle-Metal kernels, the quantized
matmul / quantize / dequantize / MoE-gather ops call Apple's MLX
(`mx.quantized_matmul`, `mx.quantize`, `mx.dequantize`, `mx.gather_qmm`)
directly through the [`mlx-rs`](https://crates.io/crates/mlx-rs) Rust bindings.
candle keeps orchestrating the model graph; only the quant ops cross over to
MLX. Because every MLX-community checkpoint — dense, MoE, hybrid, any
bit-width — funnels its quantized weights through the same `AfqLayer`
primitives, replacing those primitives is **family-agnostic by construction**:
one op-bridge covers the whole catalog with no per-model code.

The payoff: byte-for-byte parity with `mlx_lm` on the dominant compute, the end
of the candle-AFQ debugging treadmill, and MLX-grade throughput on the op that
costs ~90% of decode time.

## Interface

### Feature flag

- New Cargo feature `mlx-direct` on the `mistralrs-quant` crate (off by
  default; Apple-Silicon-only). Pulls in `mlx-rs` (pinned 0.x) under
  `#[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "mlx-direct"))]`.
- `rozum` exposes it as a passthrough feature: `rozum`'s `mistralrs` feature
  may enable `mistralrs/mlx-direct` once the engine feature is plumbed through
  `mistral.rs`'s own feature graph.

### Runtime switch

The path is selectable at runtime so a single binary can A/B the two
implementations and so we can ship it dark before flipping the default:

```
MISTRALRS_MLX_DIRECT=1   # force the MLX-direct quant path (requires the feature)
MISTRALRS_MLX_DIRECT=0   # force the legacy candle-Metal path
# unset: build-time default (Phase 1: legacy; later: MLX-direct once parity proven)
```

`rozum` mirrors it as `ROZUM_MLX_DIRECT` for symmetry with the other
`ROZUM_MISTRALRS_*` knobs, forwarding to `MISTRALRS_MLX_DIRECT`.

### New module — the op-bridge

`mistralrs-quant/src/afq/mlx_direct.rs`, gated on the feature. It exposes the
exact set of ops that `afq/ops.rs` currently implements with candle-Metal
kernels, with identical candle-facing signatures so `afq/ops.rs` chooses an
implementation behind one `if mlx_direct_enabled()` branch per op:

```rust
// All inputs/outputs are candle `Tensor`s on `Device::Metal`. Internally each
// fn bridges to `mlx_rs::Array`, runs the MLX op, and bridges back.

pub fn quantize(w: &Tensor, group_size: AfqGroupSize, bits: AfqBits)
    -> Result<(Tensor /*w_q*/, Tensor /*scales*/, Tensor /*biases*/)>;

pub fn dequantize(w_q: &Tensor, scales: &Tensor, biases: &Tensor,
    group_size: AfqGroupSize, bits: AfqBits) -> Result<Tensor>;

pub fn quantized_matmul(x: &Tensor, w_q: &Tensor, scales: &Tensor,
    biases: &Tensor, transpose: bool, group_size: AfqGroupSize, bits: AfqBits)
    -> Result<Tensor>;

pub fn gather_qmm(x: &Tensor, w_q: &Tensor, scales: &Tensor, biases: &Tensor,
    lhs_indices: Option<&Tensor>, rhs_indices: Option<&Tensor>,
    transpose: bool, group_size: AfqGroupSize, bits: AfqBits) -> Result<Tensor>;
```

### The candle ↔ MLX tensor bridge

`mistralrs-quant/src/afq/mlx_bridge.rs` (feature-gated):

```rust
/// Wrap a candle Metal tensor's storage as an mlx Array without copying,
/// exploiting Apple unified memory (shared MTLBuffer contents pointer).
/// Falls back to a device→device copy if the layout is non-contiguous.
fn candle_to_mlx(t: &Tensor) -> Result<mlx_rs::Array>;

/// Inverse: wrap an mlx Array as a candle Metal tensor on the same device.
fn mlx_to_candle(a: &mlx_rs::Array, dev: &Device) -> Result<Tensor>;
```

The exact zero-copy mechanism is fixed by the **Phase 0 prototype** (see
Design) — it is the highest-risk unknown and is gated before any model wiring.

## Behavior

- [ ] With `mlx-direct` off (default build), `mistralrs-quant` compiles and
      behaves exactly as today; no `mlx-rs` dependency is linked.
- [ ] With `mlx-direct` on and `MISTRALRS_MLX_DIRECT=0`, behavior is
      byte-identical to the legacy path (the switch genuinely routes).
- [ ] `dequantize` (MLX-direct) of a real AFQ weight matches
      `mx.dequantize` byte-for-byte within bf16 rounding, and matches the
      legacy candle `afq_dequantize_op` on the same weight (differential test).
- [ ] `quantized_matmul` (MLX-direct) for a dense linear layer matches
      `mlx_lm`'s `mx.quantized_matmul` output for a fixed `(x, w)` pair.
- [ ] `gather_qmm` (MLX-direct) for the MoE expert path matches `mlx_lm`'s
      `mx.gather_qmm` for fixed `(x, w, indices)`.
- [ ] The candle ↔ MLX bridge is zero-copy for contiguous Metal tensors
      (verified: no extra MTLBuffer allocation on the hot path) and correct for
      the non-contiguous fallback.
- [ ] **Numerical parity gate**: Qwen3-4B-4bit (dense) generates byte-for-byte
      identical tokens to `mlx_lm.generate --temp 0` over a fixed prompt, on the
      MLX-direct path.
- [ ] **Numerical parity gate**: Qwen3-30B-A3B-4bit (MoE) matches `mlx_lm`
      token-for-token on the MLX-direct path.
- [ ] **Numerical parity gate**: Qwen3.6-35B-A3B-4bit (hybrid) matches
      `mlx_lm` token-for-token on the MLX-direct path — proving the bridge
      removes the class of bug that produced the RMSNorm/zero-buffer saga.
- [ ] All AFQ bit-widths the catalog uses route correctly: 4, 8 (Phase 3 also
      2/3/6 and MXFP4 if a target checkpoint needs them), group sizes 32/64/128.
- [ ] Throughput on Qwen3.6-35B-A3B-4bit is **≥** the legacy candle-Metal path
      on the same M-series machine (target: meet or beat; measure both).
- [ ] `req.cancel` still stops within one decode step (the bridge does not
      break the existing cancel/reap machinery from the large-prompt-stall fix).
- [ ] `cargo build` / `cargo test` for `rozum` (default features) is unaffected.

## Out of scope

- Replacing candle for **non-quantized** ops (attention softmax, RMSNorm,
  RoPE, embeddings, sampling). Those stay on candle-Metal. If a *specific*
  non-quant op turns out to be a parity offender, it may be added to the bridge
  later, but this feature is scoped to the AFQ quant ops only.
- A full MLX-array model runtime (that is the separate, much larger
  `mlx-native-port` track — this feature deliberately does **not** go there).
- `Device::Mlx` inside candle (rejected — see Decisions).
- Non-Apple platforms. `mlx-direct` is Apple-Silicon-only; CUDA keeps the
  existing `afq/ffi.rs` path.
- New quantization *formats* beyond what MLX itself supports (GPTQ/AWQ/GGUF/
  bnb/fp8 keep their existing kernels; only AFQ moves to MLX-direct).
- Training / fine-tuning / LoRA over the MLX-direct path.

## Design

### Integration points (the entire surface)

Every quantized matmul in an MLX-community model already funnels through four
functions in `mistralrs-quant/src/afq/ops.rs`. That is the whole job:

| `afq/ops.rs` fn | candle-Metal call today | MLX-direct replacement |
|---|---|---|
| `afq_quantize_op` | `call_affine_quantize` | `mx.quantize` |
| `afq_dequantize_op` | `affine_dequantize` kernel | `mx.dequantize` |
| `afq_mm_op` | `call_afq_qmm` / `call_afq_qmm_splitk` | `mx.quantized_matmul` |
| `afq_gather_qmm_rhs_sorted{,_gate_up}` | gather-qmm kernels | `mx.gather_qmm` |

`AfqLayer::forward_raw` / `gather_forward_raw` / `dequantize_w` (in
`afq/mod.rs`) call these and need **no change** — they already delegate. The
switch lives one level down, inside each `afq/ops.rs` function:

```rust
pub(crate) fn afq_mm_op(/* … */) -> Result<Tensor> {
    #[cfg(all(target_os = "macos", target_arch = "aarch64", feature = "mlx-direct"))]
    if mlx_direct::enabled() {
        return mlx_direct::quantized_matmul(x, w_q, scales, biases, transpose, group_size, bits);
    }
    // …existing candle-Metal kernel path unchanged…
}
```

This keeps the legacy path intact for differential testing and rollback, and
means the diff is small and reviewable per op.

### The crux: candle ↔ MLX interop on unified memory

This is the one genuinely novel piece. candle Metal tensors are `MTLBuffer`s
owned by candle's allocator; mlx `Array`s are owned by MLX's allocator/stream.
On Apple Silicon both live in unified memory, so a buffer's `.contents()` is a
CPU-visible pointer addressable by either runtime.

**Phase 0 settles the mechanism by prototype**, in priority order of preference:

1. **Zero-copy view over the shared MTLBuffer.** Construct an `mlx_rs::Array`
   that aliases the candle tensor's `MTLBuffer` contents pointer (no copy).
   Symmetrically, read the MLX result's buffer back into a candle tensor view.
   Requires MLX to accept an externally-owned buffer and requires care around
   each runtime's `eval()`/command-buffer lifetime and synchronization.
2. **Same-device GPU copy.** If aliasing is unsafe across allocators, copy
   device→device (still no CPU round-trip). Costs one buffer copy per op
   boundary; acceptable if it still beats the candle kernel, unlikely to.
3. **CPU round-trip.** Only as a correctness-first fallback to unblock the
   parity gates; never shipped on the hot path (kills throughput).

The prototype must answer: lifetime/ownership (who frees the buffer), stream
synchronization (MLX is async-eval; candle Metal is command-buffer based — we
must `mx.eval()` / commit at the boundary), and contiguity (MLX expects
row-major contiguous; non-contiguous candle tensors take the copy fallback).
The chosen mechanism and its constraints get written back into this spec's
Results before Phase 1 starts.

### Phased delivery

Each phase has a hard exit criterion; do not start N+1 until N's gate passes.

**Phase 0 — bridge prototype + single-op parity (~3–5 days).**
- Add the `mlx-direct` feature + `mlx-rs` dep, Apple-Silicon-gated.
- Implement `mlx_bridge.rs` per the chosen interop mechanism.
- Implement `mlx_direct::dequantize` + `quantized_matmul`.
- Gate: a standalone test dequantizes one real AFQ weight and runs one
  quantized matmul, both byte-for-byte vs `mx.*` and vs the legacy candle op.
  *This proves the bridge before any model code depends on it.*

**Phase 1 — dense model parity (~1 week).**
- Wire `afq_quantize_op` / `afq_dequantize_op` / `afq_mm_op` to the switch.
- Gate: Qwen3-4B-4bit byte-for-byte token match vs `mlx_lm.generate --temp 0`.
  Then Qwen3-30B is exercised for the bigger dense-ish surface.

**Phase 2 — MoE gather path (~1 week).**
- Wire `afq_gather_qmm_rhs_sorted{,_gate_up}` to `mx.gather_qmm`.
- Gate: Qwen3-30B-A3B-4bit (MoE) and Qwen3.6-35B-A3B-4bit (hybrid MoE) both
  token-for-token vs `mlx_lm`. Qwen3.6 passing here is the headline result —
  it retires the candle-AFQ bug class for the model that motivated all of this.

**Phase 3 — generalize bit-widths & quant variants (~3–5 days).**
- Cover AFQ 2/3/6/8-bit, MXFP4, group sizes 32/128, and DWQ checkpoints, so the
  path is the whole MLX-community catalog, not just Q4/g64. Add a small
  parametric parity test over (bits, group_size) using synthetic weights.

**Phase 4 — perf & default flip (~3–5 days).**
- Benchmark MLX-direct vs legacy on Qwen3.6 (tok/s prefill + decode, peak RSS).
- Remove avoidable copies; ensure the bridge is zero-copy on the decode hot
  loop. When MLX-direct meets-or-beats legacy *and* all parity gates are green,
  flip the build-time default and keep the env switch as an escape hatch.

### Composition with existing work

- The cancel/reap machinery (`project-mistralrs-large-prompt-stall`) is above
  the quant op layer — untouched.
- PagedAttention / `max_num_seqs=1` (`project-mistralrs-oom-fix`) — untouched;
  KV cache is candle-side.
- The Qwen3.6 RMSNorm/zero-buffer/AFQ fixes stay as the **legacy** path's
  correctness baseline and the differential-test oracle. MLX-direct does not
  delete them; it makes them redundant on the MLX-direct path and gives us a
  second independent implementation to diff against (a strong correctness net).
- Upstreamability: this is additive and feature-gated, so it can become a
  mistral.rs PR (`mlx-direct` feature) independent of our other 4 PRs.

## Decisions

- **Targeted quant-op replacement, not a full native runtime** — chosen
  because >90% of MLX inference cost is the quantized matmuls, and they are the
  *only* ops that diverged from MLX in practice. Replacing just them buys the
  parity + perf win for a fraction of the code of a full `mlx-native-port`.
  Rejected: porting whole models to mlx-rs arrays (that is the separate, ~5-8
  week strategic track; not what "direct mlx in mistral" needs).
- **mlx-rs bindings** — chosen for the fastest path to real MLX ops
  (`quantized_matmul`/`quantize`/`gather_qmm` are already exposed). Pin a
  known-good 0.x and bump intentionally. Rejected for now: hand-rolled mlx-c
  FFI (more control, but writing the FFI layer is work the bindings already
  did); revisit only if mlx-rs lacks an op or its churn becomes painful.
- **In the mistral.rs fork, generalized over AFQ** — chosen because the op is
  the same for every MLX-community family, so one generic op-bridge covers
  dense/MoE/hybrid and every bit-width with zero per-model code, and it is
  upstreamable. Rejected: implementing alongside in `rozum` (would duplicate
  mistral.rs's loader/scheduler and not be "direct mlx *in mistral*").
- **Runtime env switch + off-by-default feature** — chosen so we can ship dark,
  A/B against the legacy kernels on the same binary, and roll back instantly if
  a parity gate regresses. Rejected: hard cut-over (loses the differential
  oracle and the rollback path).
- **`Device::Mlx` in candle rejected** — backing candle's entire op surface
  with MLX is months of work and out of all proportion to the goal. The
  CustomOp/bridge boundary at the quant ops is the minimal correct seam.

## Risks / sharp edges

- **The bridge is the whole risk.** candle-Metal ↔ mlx interop on shared
  buffers (ownership, async eval/commit synchronization, contiguity) is novel
  and unproven here. Phase 0 exists solely to de-risk it before model wiring;
  if zero-copy proves unsafe, the same-device-copy fallback still delivers
  parity (the primary goal) even if it dents the perf goal.
- **mlx-rs API churn** — 0.x crate; pin and bump explicitly. An op we need may
  be missing or shaped differently than `mlx_lm`'s Python; verify each against
  the Python reference, not the paper.
- **Build coupling** — both candle-Metal and MLX compile Metal; first-build
  time grows and full Xcode is required (already true today). Keep the feature
  off by default so the meeting-room and GGUF builds are unaffected.
- **Numerical parity is still non-negotiable** even though MLX is the
  reference: the *bridge* (layout, contiguity, dtype) can corrupt data that
  MLX itself would compute correctly. Every phase gates on token-for-token
  match vs `mlx_lm`, reusing `scripts/mlx_ref.py` as the oracle.
- **Two code paths to maintain** until the default flips. Mitigated by the
  differential test (legacy vs MLX-direct on the same weights) running in CI for
  the AFQ ops.

## Results

(Filled in per phase. Phase 0 must record the chosen bridge mechanism, its
zero-copy status, and the single-op parity numbers before Phase 1 begins.)
