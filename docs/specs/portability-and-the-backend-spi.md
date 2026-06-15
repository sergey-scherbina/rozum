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

## Add-a-backend checklist (write a new runtime/hardware leaf)

The recipe, concretely. A new backend is a type implementing `ChatBackend` (`src/backend.rs`); only
two methods are **required**, the rest are opt-in hooks the rozum machinery uses if you provide them.

1. **`async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>`** *(required).* The core.
   Render `req.messages` + `req.tools` with **your** chat template, run inference, and yield
   `ChatEvent`s incrementally: `TextDelta`* then, for each tool call, `ToolUseStart` →
   `ToolUseDelta`* → `ToolUseEnd`, ending with `Done { input_tokens, output_tokens, stop_reason }`.
   Honor `req.sampling` (temperature/top-p/penalties/seed/max_tokens) and, if you can, the optional
   `response_schema` (constrained decode). Respect `req.cancel` (stop and emit `Done{Cancelled}`).
2. **`fn context_window(&self) -> u32`** *(required).* The model's max context (0 if unknown).
3. **Optional hooks** — implement the ones that apply:
   - `label() -> &'static str` — a short id for `/stats` + the JSONL log.
   - `concurrency_capacity() -> Option<usize>` — `Some(n)` if you have a safe concurrent-request
     limit ⇒ `concurrency::admit_wrap` puts admission control (+ the adaptive ceiling) in front of
     you. Remote / self-serializing backends return `None` (the default) to pass through ungated.
   - `count_tokens(text) -> Option<usize>` — exact token count if your tokenizer is cheaply
     reachable (makes the admission cost estimate exact instead of the char heuristic).
   - `report_quality(ok)` — usually leave default; `AdmittingBackend` overrides it.
4. **Bring your own** template, tokenizer, and KV cache — they live *inside* the leaf (above the seam
   nothing knows about them). Reuse the hardware-agnostic model source (hf_hub/ModelScope fetch +
   cache + spec resolution) where you can.
5. **Register it in the resolution chain** — add a builder arm in `main.rs`
   (`build_gateway_backend_forced` / `build_choice`) and an `engine` name in
   `config.rs::ACCEPTED_ENGINES`, so `--backend <name>` and `rozum.toml` can select it. Wrap it in
   `concurrency::admit_wrap` if it advertises a capacity.
6. **Test it** feature-free where possible: a unit test with a scripted backend, and (behind your
   feature, `#[ignore]`) a real-model smoke test. The `HelloBackend` in `backend.rs` is the minimal
   reference implementation.

Nothing above the seam (gateway, agent runtime, cascade, concurrency, config) changes.

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
3. ~~**This doc + a "write a new backend" checklist** so the recipe above is written
   down, not folklore.~~ **DONE** — see *Add-a-backend checklist* above.

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

## Taxonomy by dependency — the real axis

"Portable / not portable" is too coarse. The useful question is **what does each
piece fundamentally depend on?** Sort by that and the extraction strategy falls
out: anything that depends on *less than the engine* can become a module reusable
by *any* engine that shares that one dependency (hardware, or model, or nothing).

| Level | Depends on | Examples from our work | Today lives in | Reusable by |
|---|---|---|---|---|
| **L0 Host** | nothing | gateway, rooms, launch, orchestration, admission | above SPI | everything, already |
| **L1 Format/protocol logic** | the model's *text* conventions (tool-call format, chat template, tokenizer), not engine/hw | `parse_tool_calls`, tool-history rendering, chat-template render, UTF-8 detok, multi-EOS, KV/RAM preflight, spec resolution, auto-download + hf_hub/ModelScope cache | mostly **inside the MLX leaf** (some in separate modules) | any in-process leaf |
| **L2 Sampling** | a logit vector + RNG, not engine/hw | top-p / top-k / repeat-penalty / seed / categorical | inside the MLX fork (operates on mlx `Array`) | any leaf, if it hands logits to a shared CPU sampler |
| **L3 Model architecture & checkpoint conventions** | the *model*, not engine/hw | the forward math (Qwen3.6 hybrid, Qwen2…), RMSNorm +1, AFQ `.weight↔.inner.weight` / `.bias↔.inner.bias` remap *pattern*, f32 delta-scan numerics, Qwen2 bias/`head_dim`, multimodal `text_config` unwrap, safetensors-index sharding | the MLX fork (as Rust math) + tribal knowledge | any engine implementing that model — as **reference**, code re-written per tensor lib |
| **L4 Hardware kernels** | the *hardware* (Metal) + maybe a model, not the engine's logic | GatedDeltaNet fused-scan Metal kernel, chunked-prefill kernel (MSL source) | the MLX fork via `mx.fast.metal_kernel` | any Metal engine — the MSL source is engine-independent |
| **L5 Engine binding internals** | the specific engine / tensor lib | RoPE-reshape fix, zero-buffer fix, buffer-donation/`eval` hazard, `mx.compile` finding, the `metal_kernel` mlx-c binding | the MLX/mistralrs forks | only that engine — stays put (already upstreamed where possible) |

The dividing line for "can it be a standalone module?" is **L0–L4 yes, L5 no.**
L5 is the irreducible cost of a given engine; everything above it depends on less
than the engine and can, in principle, be shared.

## What to extract (and into what)

These are the concrete "pull it out of the leaf into a module that depends only on
its true input" moves. All are tracked in `BACKLOG.md` (Portability section).

- **L1 → shared serving helpers (engine-agnostic Rust).** `parse_tool_calls` is
  *already duplicated* (GGUF leaf + MLX leaf have their own); tool-history
  rendering, chat-template render, UTF-8 detok, multi-EOS, and the KV/RAM preflight
  are pure logic over text/config. Lift them into a `serving` module every leaf
  calls. (Auto-download + cache is the same idea — its module exists, just MLX-wired;
  that's `portability-shared-model-source`.)
- **L2 → a shared CPU sampler.** Define the sampler over a plain logit slice
  (`&[f32]` / small ndarray) + RNG; each leaf materializes the final logit vector
  and calls it. The GPU→CPU copy of one vocab-sized vector per token is negligible
  for our op-launch-bound decode, and it deletes per-leaf sampler duplication.
- **L3 → model-reference specs.** Capture the *knowledge* (forward math +
  checkpoint conventions per family) as engine-independent reference docs, so a new
  leaf implements Qwen3.6/Qwen2/… from a spec instead of reverse-engineering a
  checkpoint. The code stays per-tensor-lib; the *spec* is the portable artifact.
- **L4 → a standalone Metal-kernel module.** The GatedDeltaNet (and any future)
  fused-scan kernel is plain Metal Shading Language; factor the `.metal` source out
  so any Metal engine (mlx, a candle-metal path, mistralrs-metal) can bind the same
  kernel instead of re-deriving it. Depends only on Metal + the architecture.
- **L5 → leave it; track upstream.** Engine-binding fixes belong in that engine's
  fork; our discipline is to push them upstream (done: 4 mistralrs PRs + the mlx-rs
  fork fixes) so the *ecosystem* carries them, not us.

## Takeaway

rozum's durable identity is the **host/orchestration layer**: a local
Anthropic/OpenAI gateway with agent rooms, multi-backend routing, and model
infrastructure — all above a one-trait seam. The MLX kernels are a fast leaf for
the hardware we happen to run; on other hardware the llama.cpp leaf already carries
the same host. We don't need to *invent* a general level — we need to keep being
disciplined about the one we have, and lift the last few shared pieces above the
seam.
