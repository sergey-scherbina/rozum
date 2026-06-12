# Portability — the durable layer vs the swappable runtime

A recurring, healthy question: we lean hard on MLX (Apple Silicon) right now —
what is *ours* and survives a hardware/format/model change? If someone isn't on a
Mac, or a new runtime appears, what do we keep and what do we swap?

Short answer: **we already have the hardware-agnostic layer — it's the
`ChatBackend` SPI and everything above it. That layer *is* rozum. The MLX runtime
is one swappable leaf below the line.** This doc names that boundary explicitly so
we design to it on purpose.

## The two halves

```
            ┌─────────────────────────────────────────────────────────┐
 DURABLE    │  Protocol gateway  (Anthropic /v1/messages +             │
 (ours,     │     OpenAI /v1/chat/completions, SSE, tool-call mapping)  │
 hardware-  │  Meeting-room agent system (MCP rooms, turns,            │
 agnostic,  │     piggyback/channels wakeup)                            │
 "useful    │  Launch wrapper (env, shared daemon, proxy, replay/      │
 always")   │     poison, --backend-url)                                │
            │  Multi-backend orchestration + concurrency/admission     │
            │  Model infra (spec resolution, auto-download,            │
            │     hf_hub/ModelScope cache, RECOMMENDED)                 │
            └───────────────────────────  ChatBackend SPI  ────────────┘   ← the seam
 SWAPPABLE  │  native MLX (Apple Metal)   GGUF/llama.cpp (cross-platform)│
 (a leaf    │  mistralrs/candle (CPU/CUDA/Metal)   HTTP (LM Studio,      │
 per        │     Ollama, mlx_lm.server, any /v1)   Hello/Placeholder    │
 runtime)   │                                                           │
            └───────────────────────────────────────────────────────────┘
```

The seam is one small trait (`src/backend.rs`):

```rust
trait ChatBackend: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    fn context_window(&self) -> u32;
    fn label(&self) -> &'static str;            // observability
    fn concurrency_capacity(&self) -> Option<usize>;  // admission hint
    fn admission_stats(&self) -> Option<AdmissionSnapshot>;
}
```

`ChatRequest` (messages + tools + sampling) goes in; a `ChatStream` of `ChatEvent`
(text deltas, tool-use start/delta/end, stop reason) comes out. **Everything
hardware/model/format-specific — quantization, KV cache, the chat template,
tokenization, the Metal/CUDA kernels — lives *inside* a leaf, never above the
seam.** That is exactly the abstraction the question is asking for, and it already
exists and is exercised by ~9 implementations (MLX, GGUF, mistralrs, HTTP, the
orchestrator, the admission wrapper, two test doubles).

## "What if it's not a Mac / not MLX?" — already answered today

Portability is not hypothetical; rozum already runs off Apple Silicon:

- **GGUF / llama.cpp backend** (`--features gguf`) is **cross-platform**: Linux /
  Windows / Mac, on CPU, CUDA, ROCm, Vulkan, or Metal. This is the immediate
  non-Mac path — same gateway, rooms, launch, everything above the seam unchanged.
- **mistralrs / candle** (`--features mistralrs`) also targets CPU/CUDA/Metal.
- **HTTP backends** reach anything that speaks OpenAI/Anthropic (LM Studio, Ollama,
  vLLM, a remote box) — zero local compute. `--backend-url` is the universal hatch.
- **native MLX** is the *only* Apple-only leaf — a high-performance specialization
  for the hardware most of us run, not a dependency of the design.

So a non-Mac user today builds `--no-default-features --features gguf` (with their
GPU's llama.cpp feature) and gets the full rozum host. The MLX leaf simply isn't
compiled.

## Adding a new model vs a new runtime — both stay below the seam

- **A new model architecture** (Gemma, Phi, Mistral…) is a leaf-*internal* concern:
  a model file inside a runtime (e.g. our MLX fork), plus a `LoadedModel` arm. The
  SPI doesn't move. (See the catalog backlog.)
- **A new runtime / hardware** (a future Vulkan-native Rust engine, a new Apple
  thing, a CUDA-native path) is a **new leaf**: implement `ChatBackend`, bring your
  own template/tokenizer/cache, register it in the resolution chain. Nothing above
  the seam changes. The recipe is: *make it satisfy the five methods, slot it into
  `build_gateway_backend` / the config chain, done.*

## What we should still do to fully realize this (backlog)

The abstraction exists; a few sharp edges keep it from being clean portability:

1. **Platform-aware build.** `default = ["mlx-native", "gguf"]` assumes macOS —
   a `cargo build` on Linux tries to compile MLX (Apple toolchain) and fails. We
   want the MLX leaf to default on macOS only, and a clean cross-platform default
   elsewhere (gguf + a CUDA/Vulkan passthrough feature), so "not a Mac" is a
   first-class build, not a flag incantation.
2. **Lift the shared model infra above the seam.** Auto-download +
   hf_hub/ModelScope cache + spec resolution are hardware-agnostic and useful to
   *any* safetensors backend (mistralrs, a future runtime), but today they're wired
   through the MLX path. Factor them into a backend-agnostic "model source" layer so
   a new leaf reuses fetching/cache/preflight for free.
3. **This doc + a "write a new backend" checklist** so the recipe above is written
   down, not folklore.

These are tracked in `BACKLOG.md` (Portability / hardware-agnostic core). None are
urgent — the seam already works — but they turn "portable in principle" into
"portable by `cargo build`".

## "But all the optimizations and fixes we did — do they port?"

Fair question, and the honest answer is *it depends which one* — they fall into
three buckets. The split is healthy: the non-portable work is exactly the work that
*should* be leaf-local, and nothing durable is lost when you swap a leaf.

**Bucket 1 — Already above the SPI → portable to every backend & machine, today.**
The protocol gateway, agent rooms, launch wrapper, multi-backend orchestration,
concurrency/admission — these never knew what runtime was under them. They work for
GGUF, mistralrs, HTTP, MLX, on any OS, unchanged.

**Bucket 2 — Portable *concept*, currently leaf-bound.** A chunk of our work is
hardware-agnostic in principle but lives inside the MLX leaf (`mlx_native_backend`)
because the SPI boundary is at *text events*, not logits — so per-request
rendering, sampling, parsing, and preflight all happen *inside* a backend:
- tool-call parsing (`parse_tool_calls`) + multi-turn tool-history rendering,
- the sampler (top-p / top-k / repeat-penalty / seed),
- per-token + mid-prefill **cancellation**, multi-EOS,
- the **RAM/KV preflight**,
- auto-download + hf_hub/ModelScope cache (already separate modules, MLX-wired).
The *ideas and most of the code* would carry to another in-process runtime, but
today they'd be re-implemented per leaf (GGUF already has its own tool parser —
duplication). Lifting these into a shared layer is the
`portability-shared-model-source` (plus a future "shared serving helpers") backlog
work. **Portable, just not yet *shared*.**

**Bucket 3 — Genuinely leaf/hardware/model-specific → does NOT port, and must not.**
The Metal kernels (GatedDeltaNet fused scan, fused causal SDPA), the chunked-prefill
+ last-position-projection plumbing, the `mlx-rs` binding fixes (RoPE reshape,
AFQ `.weight→.inner.weight` / `.bias→.inner.bias` remap, the zero-buffer and
buffer-donation/`eval` hazards), and the model-arch quirks (RMSNorm +1, f32 delta
scan, Qwen2 optional `head_dim`) are all bound to *MLX + a specific checkpoint*.
On CUDA/llama.cpp they would be different code — or **non-issues**, because that
runtime already solved them its own way.

**The key insight.** Bucket 3 is the *price of running our own MLX leaf* — it buys
peak Apple-Silicon speed and day-one architectures (we had Qwen3.6 hybrid working
natively before most tooling did). It is deliberately *quarantined below the seam*.
Move to other hardware and you **lose our MLX-specific implementations but inherit
that runtime's** (llama.cpp is one of the most optimized inference engines in
existence — you trade our kernels for theirs, not for nothing), while keeping all of
Bucket 1 and the *concepts* of Bucket 2. Nothing durable is lost.

And the non-portable *code* still leaves portable **knowledge**: e.g. "a quantized
backend's checkpoint keys rarely match the framework's param tree — check the
remap", or "lazy-eval runtimes can donate a not-yet-materialized buffer — force
`eval` on recurrent state". Those lessons transfer to the *next* leaf even when the
code doesn't. (Several fixes also went upstream as PRs — ecosystem-portable by
definition.)

## Takeaway

rozum's durable identity is the **host/orchestration layer**: a local
Anthropic/OpenAI gateway with agent rooms, multi-backend routing, and model
infrastructure — all above a one-trait seam. The MLX kernels are a fast leaf for
the hardware we happen to run; on other hardware the llama.cpp leaf already carries
the same host. We don't need to *invent* a general level — we need to keep being
disciplined about the one we have, and lift the last few shared pieces above the
seam.
