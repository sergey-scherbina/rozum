# Native x86 runtime (integrated-GPU, unified-memory, zero-copy) — Vulkan compute

## One-line

Bring the **MLX architectural recipe** — compute on the **integrated GPU**,
**unified memory** (no host↔device copy), **zero-copy `mmap` of the weight file**
— to **commodity x86** as a new in-process `ChatBackend` leaf, on **cross-vendor
Vulkan compute** (Intel Xe/Arc + AMD APU iGPUs). Same gateway, rooms, launch,
catalog above the seam; a new engine below it.

## Why — and why it is a NEW leaf, not the paths we already have

rozum already runs off Apple Silicon two ways, but **neither is the MLX recipe on
x86**:

- **GGUF / llama.cpp + Vulkan** already does iGPU compute with `mmap` weights on
  x86 today (`portability-cuda-gguf`). It is the *pragmatic* non-Mac GPU path and
  we keep it. But it is **llama.cpp's** engine: its model zoo, its quant formats
  (GGUF k-quants), its kernels. We don't own the graph, can't add a day-one
  architecture from our `model-reference/` specs, and can't share our MLX-side
  quant layout (AFQ / MXFP4) or the per-family forward math.
- **MLX's own x86 story is CUDA** — a *discrete* NVIDIA GPU with separate VRAM and
  PCIe copies. That is the **opposite** of the unified-memory / zero-copy thesis:
  no `mmap`-and-read-in-place, no shared RAM. It does not satisfy "the same
  approach as MLX" on commodity x86.

So to get *MLX's* advantages — we own the whole graph, weights live once in shared
RAM and the GPU reads them in place, new models land from a spec not a vendor —
on x86 iGPUs, we need **our own native engine**. It slots in below the existing
one-trait seam exactly like the MLX leaf; everything above it is unchanged. The
portability spec already anticipates this leaf by name ("a future Vulkan-native
Rust engine"): see [`portability-and-the-backend-spi.md`](portability-and-the-backend-spi.md).

## The hardware thesis (what makes the recipe transfer)

Apple Silicon's edge for LLM inference is **UMA**: CPU and GPU share one physical
pool, so an `mmap`'d weight file is *directly* GPU-addressable with no copy, and
the practical model size is bounded by total RAM, not a separate VRAM budget.

Commodity x86 integrated GPUs have the **same property**:

- **Intel Xe / Arc iGPU** (in nearly every recent Intel CPU) and **AMD APU**
  (Ryzen-with-Radeon) are on-die GPUs that read **system RAM** directly. There is
  no discrete VRAM; the "GPU memory" *is* the DDR the CPU uses.
- In **Vulkan** terms this surfaces as memory heaps flagged
  `HOST_VISIBLE | HOST_COHERENT | DEVICE_LOCAL` — i.e. memory the CPU can map AND
  the GPU treats as local. That is UMA. (On a discrete GPU these flags are split;
  on an iGPU they coincide — which is precisely why the recipe transfers to iGPUs
  but not to dGPUs.)
- **Zero-copy `mmap`** is realized by **`VK_EXT_external_memory_host`**: `mmap` the
  safetensors/`.gguf`, then `vkImportMemoryHostPointerInfoEXT` imports that host
  pointer as `VkDeviceMemory` and a `VkBuffer` is bound to it. The compute shaders
  read the **packed quantized weights straight out of the page cache** — no
  staging buffer, no upload. This is the literal analog of MLX reading an `mmap`'d
  `Array` in place, and of `gather_qmm` dequantizing packed weights on the fly.

The bound on model size becomes **total system RAM** (minus the OS), same as on a
Mac — so a 32 GB mini-PC runs the 20–35B 4-bit MoEs we already serve.

> **Reality check on speed.** MLX is fast because of *Apple-tuned* kernels plus
> whole-graph ownership. On Vulkan we write our own kernels and will not match MLX
> on day one. The win we bank first is **correctness + day-one models + zero-copy
> memory**; competitive throughput is an iterative kernel-tuning effort (tiling,
> subgroup/`shaderInt8` dot-product, fp16 math), benchmarked against llama.cpp's
> Vulkan backend on the same iGPU as the bar to clear.

## Where it sits — reuse the durable layer, write only L5

Using the portability spec's L0–L5 taxonomy, an x86 leaf **reuses L1–L4 and writes
only L5** (the engine):

| Layer | What | x86 leaf |
|---|---|---|
| L1 serving (engine-agnostic Rust) | chat-template render, `parse_tool_calls`, the **harmony adapter** (`src/harmony.rs`), UTF-8 detok, multi-EOS, KV/RAM preflight | **reuse** (lift shared per `portability-*` backlog) |
| L2 CPU sampler | materialize the final logit vector → shared sampler | **reuse** (GPU→CPU copy of one vocab vector/token; negligible for op-bound decode) |
| L3 model-reference | per-family forward math + checkpoint conventions (`docs/specs/model-reference/`, `mlx-weight-layout-and-afq.md`) | **reuse** — implement Qwen3/gpt-oss/… from the spec, not from a checkpoint |
| L4 standalone kernels | architecture-specific fused kernels | **new** — Metal source does not port; Vulkan needs its own (see kernels below) |
| **L5 engine** | tensors, memory, the op kernels, the dispatch loop | **new — this leaf** |

Net new surface is the **L5 Vulkan engine**; everything above the `ChatBackend`
seam (gateway, rooms, launch, orchestration, model-source, catalog) is untouched.

## The engine (L5) — components

A feature-gated crate path, **off by default**, compiled only with
`--features x86-native` (Vulkan SDK + a SPIR-V toolchain present):

1. **Device & memory.**
   - Enumerate physical devices, **prefer an integrated GPU** with a
     `HOST_VISIBLE | DEVICE_LOCAL` heap; fall back / refuse clearly if none.
   - **Weights — zero-copy:** `mmap` the model file, import via
     `VK_EXT_external_memory_host` (honoring `minImportedHostPointerAlignment`),
     bind buffers per tensor. No upload; the GPU reads packed 4-bit weights in
     place. (Fallback for drivers lacking the extension: a one-time host-visible
     staged copy — slower start, same steady state.)
   - **Activations / KV cache:** allocate in `HOST_VISIBLE | DEVICE_LOCAL` so the
     CPU sampler reads logits and the GPU writes KV with no staging.

2. **Minimal tensor + op layer.** A thin `Tensor` over a `VkBuffer` + shape +
   dtype (fp16 / fp32 / packed-quant), and a small op set — `matmul`,
   `quant_matmul` (gather variant for MoE), `rms_norm`, `rope`, `softmax`,
   `scaled_dot_product_attention`, elementwise/activation, `embedding`,
   `argmax`/`top-k` prep. Eager dispatch first; a fuse/record pass later. This is
   the layer the per-family forward calls — the same role `mlx-rs` ops play for
   the MLX leaf.

3. **Compute kernels (SPIR-V).** The make-or-break work:
   - **Quantized matmul** for the formats our catalog uses, decoded on the fly:
     **AFQ affine 4/8-bit** (`group_size`, scales+biases) and **MXFP4** (E2M1 +
     E8M0 block scale, no zero-point) — per
     [`mlx-weight-layout-and-afq.md`](mlx-weight-layout-and-afq.md) and the
     gpt-oss MXFP4 layout. A **gather** variant selects per-token experts (MoE).
   - **Attention:** GQA, with **attention sinks** and **sliding-window** masking
     (gpt-oss) — start with an explicit `softmax(QKᵀ)·V` + mask, optimize to a
     fused/flash kernel later.
   - **`rms_norm`, `rope`** (incl. YaRN frequencies), **SwiGLU/clamped-SwiGLU**.
   - Use `shaderInt8`/`VK_KHR_shader_integer_dot_product` and subgroup ops where
     available for the quant dot-products; fp16 accumulate paths gated on
     `shaderFloat16`.

4. **Model-agnostic forward + decode loop.** Per-family modules (Qwen3, gpt-oss,
   …) built from the L3 reference math, driving prefill (chunked) + single-token
   decode over an external KV cache — mirroring the MLX leaf's `Generate`
   iterator, streaming + cancel friendly.

## Interface

```rust
// Feature gate: #[cfg(feature = "x86-native")] (off by default; needs Vulkan + SPIR-V).
pub struct X86NativeOptions {
    pub n_ctx: u32,            // env ROZUM_X86_N_CTX; default = model max, RAM-bounded
    pub temperature: f32,
    pub top_p: f32,
    pub device_index: Option<u32>, // ROZUM_X86_DEVICE; default = best integrated GPU
}

// Implements the existing seam unchanged:
impl ChatBackend for X86NativeBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    fn context_window(&self) -> u32;
    fn label(&self) -> &'static str { "x86-native" }
    fn concurrency_capacity(&self) -> Option<usize> { Some(1) } // serialize like MLX
    fn count_tokens(&self, text: &str) -> Option<usize>;        // reuse the L1 tokenizer
}
```

Registration: a `build_gateway_backend` arm + an `engine = "x86-native"` in
`config.rs::ACCEPTED_ENGINES` (the add-a-backend checklist). Selected on x86, after
HTTP backends, ahead of CPU-only fallbacks.

## Phased plan (each phase independently shippable + benchmarked)

- **P0 — Probe & decision record.** Stand up Vulkan device + compute queue from
  Rust (`ash`/`vulkano`); confirm an `HOST_VISIBLE | DEVICE_LOCAL` heap and
  `VK_EXT_external_memory_host` on a target Intel Xe and an AMD APU; `mmap` →
  import → read a tensor back. Pick the Rust Vulkan binding. **Gate:** zero-copy
  import demonstrated on both vendors.
- **P1 — MVP forward, one dense model.** Qwen3-4B (smallest catalog model) at
  fp16: matmul/rmsnorm/rope/sdpa/softmax kernels, greedy decode, **greedy parity
  vs MLX** on a fixed prompt. Quant can be CPU-dequant-to-fp16 at load for P1 to
  isolate the forward. **Gate:** byte/greedy parity.
- **P2 — Quant kernels (zero-copy).** AFQ affine 4/8-bit `quant_matmul` reading
  packed weights in place; drop the P1 dequant. **Gate:** parity holds, memory
  footprint ≈ MLX (weights not duplicated).
- **P3 — MoE + gpt-oss family.** Gather-`quant_matmul`, MXFP4, attention sinks,
  sliding-window, YaRN; reuse the **harmony adapter** (L1). **Gate:** gpt-oss
  greedy parity + a one-turn tool call through the gateway.
- **P4 — Perf.** Tiling, subgroup int-dot, fp16, kernel fusion, pipelined decode.
  **Gate:** within a target factor of llama.cpp-Vulkan on the same iGPU.
- **P5 — Catalog + ship.** `src/models.rs` entries, `--features x86-native` build
  docs, the agentic matrix on an x86 iGPU box.

## Parity & correctness

Every model gates on **greedy parity vs a reference** (MLX on Mac, or
`mlx_lm`/llama.cpp) on fixed prompts — the same discipline that caught the gpt-oss
bias-remap bug. The forward math comes from `model-reference/`, so a divergence is
a kernel bug, isolatable op-by-op (dump intermediate tensors, compare to MLX).

## Non-goals

- **Not** replacing the GGUF+Vulkan leaf — that stays the zero-effort non-Mac GPU
  path; this is the *native* engine for when we want graph ownership / day-one
  models / shared quant.
- **Not** NVIDIA discrete / CUDA (that's copies, not UMA — out of thesis; MLX-CUDA
  or gguf-cuda already cover dGPUs).
- **Not** training / LoRA. Inference forward only.
- **Not** a general tensor framework — only the ops our catalog's forwards need.

## Risks & open questions

- **Kernel performance** is the headline risk: no Apple-tuned kernels; matching
  llama.cpp-Vulkan needs real GPU-kernel work (P4). Mitigate by shipping P1–P3 on
  correctness and treating perf as a separate, measurable track.
- **Driver portability** across Intel/AMD Vulkan stacks (extension availability,
  subgroup sizes, fp16/int8 support) — probe both in P0; feature-detect and pick
  kernel variants at runtime.
- **Writing a forward engine is large.** Open question carried from the decision:
  build kernels fully from scratch vs. lean on an existing Vulkan-compute kernel
  library for the plumbing (still our quant kernels + forward). Revisit at P0 with
  a concrete shortlist.
- **`mmap` alignment** for `VK_EXT_external_memory_host` (`minImportedHostPointer
  Alignment`) — safetensors data offsets may need padding; have the staged-copy
  fallback ready.

## Decisions (locked 2026-06-17)

- **Compute API:** Vulkan compute, **own kernels** (cross-vendor; zero-copy via
  `VK_EXT_external_memory_host`). Chosen over wgpu (too abstract for host-memory
  import / memory control), vendor-specific oneAPI/ROCm (not cross-vendor), and a
  burn/candle framework (gives up native quant/kernel control).
- **Target hardware:** **cross-vendor iGPU** (Intel Xe/Arc + AMD APU) from P0 — the
  widest commodity-x86 reach.
