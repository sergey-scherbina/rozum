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

## Takeaway

rozum's durable identity is the **host/orchestration layer**: a local
Anthropic/OpenAI gateway with agent rooms, multi-backend routing, and model
infrastructure — all above a one-trait seam. The MLX kernels are a fast leaf for
the hardware we happen to run; on other hardware the llama.cpp leaf already carries
the same host. We don't need to *invent* a general level — we need to keep being
disciplined about the one we have, and lift the last few shared pieces above the
seam.
