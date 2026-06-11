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
- [ ] AFQ weights cross to MLX exactly once (at load / first use) and are
      cached MLX-side; no per-token weight copy. Activations copy at the
      boundary (baseline) with bounded, measured cost (decode negligible;
      prefill quantified in Phase 4).
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

### The crux: candle ↔ MLX interop (API findings)

This is the one genuinely novel piece, and probing the APIs settled the
mechanism question — with the opposite conclusion to the naive "they share
unified memory so just alias the pointer" intuition.

**API facts (verified in source):**

- `mlx-rs` ops are a clean 1:1 match for `afq/ops.rs`:
  `quantize_device(w, group_size, bits)`,
  `dequantize_device(w, scales, biases, group_size, bits)`,
  `quantized_matmul_device(x, w, scales, biases, transpose, group_size, bits)`,
  `gather_qmm_device(x, w, scales, biases, lhs_indices, rhs_indices, transpose,
  group_size, bits, sorted_indices)`. `bits`/`group_size` are runtime args, so
  one generic path covers every bit-width.
- **Every safe `mlx_rs::Array` constructor copies.** The only non-copying entry
  is `unsafe Array::from_raw_data(*const c_void, …)` → `mlx_array_new_data`,
  which in mlx-c **also copies** (`mlx_array_set_data` → `array((T*)data,…)`
  allocates an MLX buffer and memcpy's).
- A genuine zero-copy adopt path exists **in mlx-c** —
  `mlx_array_new_data_managed(void* data, …, dtor)` adopts the pointer and runs
  a destructor callback — **but** (a) `mlx-rs` does not wrap it (custom FFI
  needed) and (b) it still takes a raw `void*`, not an `id<MTLBuffer>`.
- **candle allocates tensors `StorageModePrivate` on macOS**
  (`candle-core 0.10.2 metal_backend/device.rs`: "Uses StorageModePrivate on
  macOS for faster GPU access"). A Private buffer has **no CPU-visible
  `.contents()`** — there is no `void*` to hand the adopt path. candle's
  buffer is reachable as `MetalStorage::buffer() -> &metal::Buffer` (and
  `MetalStorage::new(Arc<Buffer>, …)` lets us rebuild a candle tensor around
  one), so we can get the `MTLBuffer` *object* — but the managed C API wants a
  memory pointer, not an MTLBuffer handle.

**Conclusion: true zero-copy sharing through the public API is not reachable.**
It needs MLX to adopt the `id<MTLBuffer>` object itself (possible only via
custom C++ glue into MLX's `allocator::Buffer` + a no-op deleter + manual
cross-queue synchronization) and it fights candle's Private storage. That is a
research spike (deferred to Phase 4), **not** the baseline — and it buys very
little (see cost analysis). The baseline is an on-device copy at the boundary.

### Cost of the boundary copy (why it is acceptable)

"Copy on device" = a `memcpy`/blit **inside unified memory** — no host
transfer, no PCIe. Two distinct kinds of data, very different cost:

- **Weights** (`w_q`/`scales`/`biases`, multi-GB): converted to `mlx_rs::Array`
  **once at load** (or loaded straight into MLX so candle never holds them) and
  kept resident MLX-side. **Zero per-token weight copies.** This is the bulk of
  the bytes and it never re-crosses.
- **Activations** (what actually crosses each quant op): ~one copy each
  direction, of a tiny tensor.
  - *Decode* (hot loop, 1 token): activation ≈ `[1, hidden]` ≈ **~10 KB**
    (Qwen3.6 hidden≈5120, bf16). ~40 layers × ~7 quant matmuls ≈ 280 crossings
    × 10 KB ≈ ~2.8 MB/token; at ~30 tok/s ≈ ~84 MB/s against ~200–400 GB/s
    unified bandwidth → **<0.05% of bandwidth. Negligible.**
  - *Prefill* (P tokens): activation ≈ `[P, hidden]`; at P=2000 ≈ 20 MB/crossing
    × 280 ≈ ~5.6 GB ≈ **~28 ms** total at ~200 GB/s — small next to the prefill
    matmuls themselves, but **measurable**. Chunked prefill already caps `P`.

**The real cost is not the bytes — it is the sync point.** Crossing the
boundary forces serialization: candle must finish the kernel that produced the
activation (reading it to a CPU-visible staging buffer waits on candle's command
buffer) and MLX must `eval()` its graph. The two runtimes therefore do **not**
pipeline across a quant op. In decode (latency-bound, one token) this is free;
in prefill it costs the lost overlap. This, not the copy, is what Phase 4
benchmarking must quantify.

### Bridge mechanism (baseline) and the zero-copy spike

`afq/mlx_bridge.rs` (feature-gated), **baseline = copy**:

1. `candle_to_mlx(&Tensor) -> Array`: ensure the source is contiguous and
   CPU-readable (Private → one blit to a Shared staging buffer, or allocate
   boundary tensors Shared up front), then `Array::from_raw_data` (copy into an
   MLX buffer). Forces a candle sync first.
2. `mlx_to_candle(&Array, &Device) -> Tensor`: `array.eval()`, read its buffer,
   build a candle Metal tensor (copy in). 
3. Weights: a one-time `candle_to_mlx` (or direct MLX load) cached on the
   `AfqLayer`, never repeated.

The prototype still must pin down: contiguity handling, dtype mapping
(bf16/f16/f32), and the exact staging-buffer strategy. The chosen specifics get
written back into Results before Phase 1.

**Deferred zero-copy spike (Phase 4, only if prefill copy/sync shows up):**
custom FFI to `mlx_array_new_data_managed` adopting candle's `id<MTLBuffer>`
with a no-op deleter and explicit cross-queue synchronization. Tracked as a
research item, not a dependency of the parity gates. If prefill overlap turns
out to matter, the cheaper lever is usually to **widen the MLX region** (keep
adjacent activations MLX-side so fewer ops cross) rather than hack the
allocators — but that drifts toward `mlx-native-port` and is out of this
feature's scope.

### Phased delivery

Each phase has a hard exit criterion; do not start N+1 until N's gate passes.

**Phase 0 — bridge prototype + single-op parity (~3–5 days).**
- Add the `mlx-direct` feature + pinned `mlx-rs` dep, Apple-Silicon-gated.
- Implement `mlx_bridge.rs` with the **copy baseline** (`candle_to_mlx` via
  staging + `from_raw_data`; `mlx_to_candle` via `eval` + read-back). Settle
  contiguity, dtype mapping, and staging-buffer strategy.
- Implement `mlx_direct::dequantize` + `quantized_matmul`.
- Gate: a standalone test dequantizes one real AFQ weight and runs one
  quantized matmul, both byte-for-byte vs `mx.*` and vs the legacy candle op.
  *This proves the bridge before any model code depends on it.* No zero-copy
  work here — correctness first.

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

**Phase 4 — perf, optional zero-copy spike & default flip (~3–5 days).**
- Benchmark MLX-direct (copy baseline) vs legacy on Qwen3.6 (tok/s prefill +
  decode, peak RSS). Decode is expected to already meet-or-beat legacy; the
  question is prefill, where the boundary copy + lost cross-runtime overlap
  could show.
- **Only if prefill regresses:** spike the zero-copy adopt path (custom FFI to
  `mlx_array_new_data_managed`, candle `id<MTLBuffer>` adoption, no-op deleter,
  cross-queue sync) and/or widen the MLX region for adjacent hot ops. Time-box
  it; the copy baseline ships if the spike does not clearly pay off.
- When MLX-direct meets-or-beats legacy *and* all parity gates are green, flip
  the build-time default and keep the env switch as an escape hatch.

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

- **The bridge is the whole risk — but the copy baseline de-risks it.** True
  zero-copy is *not* reachable through the public API (candle's Private storage
  + two separate allocators/queues; mlx-c's adopt path wants a `void*`, not an
  `id<MTLBuffer>`), so the baseline is an on-device copy whose cost is
  negligible in decode and bounded in prefill. Phase 0 proves correctness with
  the copy path before any model wiring. The boundary also forces a
  cross-runtime sync point (no pipelining across a quant op) — the main perf
  question for prefill, measured in Phase 4.
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

### API probe (pre-implementation, 2026-06-11)

- `mlx-rs` exposes `quantize/dequantize/quantized_matmul/gather_qmm` with a 1:1
  fit to `afq/ops.rs`; `bits`/`group_size` are runtime args (one generic path).
- All safe `Array` constructors copy; `from_raw_data`/`mlx_array_new_data` copy
  too. Zero-copy adopt exists only as mlx-c `mlx_array_new_data_managed`
  (unwrapped by mlx-rs; takes `void*`, not an MTLBuffer).
- candle (0.10.2) tensors are `StorageModePrivate` on macOS → no CPU-visible
  pointer to adopt; buffer reachable as `MetalStorage::buffer() -> &Buffer`.
- **Decision from the probe:** copy baseline is the mechanism; zero-copy is a
  deferred Phase-4 spike. Cost analysis: weights cross once at load; activation
  copies are <0.05% bandwidth in decode, ~28 ms total at P=2000 in prefill; the
  cross-runtime sync point (lost pipelining) is the real prefill cost to watch.

### Per-phase results

(Filled in per phase. Phase 0 must record the final bridge specifics —
contiguity/dtype/staging strategy — and the single-op parity numbers before
Phase 1 begins.)
