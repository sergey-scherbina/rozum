# Native engine SPI — lift the reusable layer up, isolate hardware in small components

## One-line

Before building the x86 Vulkan engine, draw the **internal seam** that every
in-process native engine shares: lift the engine-agnostic decode/serving logic
**up** into one shared driver behind a tiny **`LocalEngine`** trait, and push all
hardware/kernel code **down** into small, isolated, independently-testable
components. Then a new engine (x86 Vulkan) is "implement `LocalEngine` + its
kernels", not "re-implement the whole leaf".

This is the **architecture step — done first, on its own**, before
`x86-native-runtime` P0. It is hardware-independent (validated on the existing MLX
+ GGUF leaves on a Mac) and benefits the whole codebase, not just x86.

## The problem: the control loop is duplicated per leaf

The `ChatBackend` seam (`src/backend.rs`) is the right *outer* boundary, and some
engine-agnostic pieces are already shared:

- `src/serving.rs` — `parse_tool_calls` (used by **both** MLX and GGUF leaves).
- `src/sampler.rs` — a CPU sampler over `&[f32]` (used by the **GGUF** leaf).

But the **decode-control loop is copy-pasted per engine**:

- MLX leaf: `stream_generation` (detok → text deltas, tool-call finalize incl. the
  harmony branch, EOS/cancel/max-tokens, `Done`) — ~200 lines.
- GGUF leaf: its own token loop in `gguf.rs` doing the same shape of work.
- A future **x86 leaf would be a third copy.**

So would prompt rendering (template + tokenizer), EOS derivation, the KV/RAM
preflight, and the harmony adapter. Every new engine re-pays for all of it, and
fixes (e.g. the harmony recipient-on-wrong-channel bug) have to be applied N times.

## The fix: one shared driver behind a tiny engine trait

Define the smallest surface an engine must expose, and write the
detok→`ChatEvent` control loop **once** above it.

```rust
// src/engine.rs — the internal seam below ChatBackend, above the kernels.

/// Static facts the shared driver needs from a loaded model.
pub struct EngineMeta {
    pub n_ctx: u32,
    pub eos: Vec<u32>,           // multi-EOS (Qwen <|im_end|>, gpt-oss <|return|>/<|call|>…)
    pub model_type: String,
    pub harmony: bool,           // gpt-oss channel format vs Qwen <tool_call>
    // tokenizer + chat template live here too (shared render/detok call into them).
}

/// What an in-process engine must provide. EVERYTHING above this — templating,
/// tokenization, the detok→event loop, tool-call parsing (serving + harmony),
/// EOS/cancel/max-tokens, stream assembly, sampling *glue* — is shared.
pub trait LocalEngine: Send {
    fn load(dir: &Path, opts: &EngineOptions) -> Result<Self, String> where Self: Sized;
    fn meta(&self) -> &EngineMeta;

    /// A token iterator for a sampled generation over `prompt` (prefill→decode).
    /// The engine samples *however suits its hardware* — MLX on the GPU inside its
    /// graph; a CPU/Vulkan engine by materializing the last-row logits and calling
    /// `crate::sampler::sample`. Honors `params`; polls `cancel`.
    fn generate<'a>(
        &'a mut self,
        prompt: &'a [u32],
        params: &'a SamplingParams,
        cancel: &'a CancellationToken,
    ) -> Box<dyn Iterator<Item = Result<u32, String>> + Send + 'a>;

    // Opt-in hooks (default = unsupported), so an engine adds capability without
    // bloating the required surface:
    fn supports_prefix_reuse(&self) -> bool { false }
    fn constrain(&self) -> Option<&dyn ConstraintSupport> { None } // response_schema
}

/// The shared decode-control loop — written ONCE, engine-agnostic. Renders the
/// prompt (template+tokenizer via `meta`), drives `engine.generate`, and turns the
/// token stream into `ChatEvent`s: stream `final`-channel/non-tool text, detect &
/// emit tool calls (`serving::parse_tool_calls` or `harmony::parse_harmony`), honor
/// EOS/cancel/max-tokens, finalize with `Done`. This is today's `stream_generation`
/// generalized — the harmony flag picks the parser.
pub fn drive<E: LocalEngine>(engine: &mut E, req: &ChatRequest, emit: impl FnMut(ChatEvent));
```

The async `ChatBackend::chat` impl stays per-leaf (it owns the worker-thread bridge
— MLX must run all GPU work on one dedicated thread), but it shrinks to: *bridge
async↔worker, call `drive`, forward `ChatEvent`s*. The **substance** (the loop,
parsing, EOS, harmony, sampling glue) lives once in `drive`.

> **This is NOT the per-op cross-runtime dead-end.** The engine still owns its
> whole forward + sampling graph (MLX keeps Apple-tuned whole-graph speed; see
> `mistralrs-mlx-direct.md`). Only the **text-level** control loop is shared — it
> touches tokens and strings, never per-op GPU dispatch. Zero cross-runtime sync.

## Up vs down — the durable/hardware split, concretely

**Lifted UP (shared, engine-agnostic — `drive` + these call into them):**

| Piece | Today | Target |
|---|---|---|
| Tool-call parse | `serving::parse_tool_calls` (shared ✓) | shared ✓ |
| Harmony adapter | `src/harmony.rs` (in MLX leaf path) | shared module, any engine |
| CPU sampler | `sampler::sample` (GGUF only) | shared; engines that don't sample on-device use it |
| Decode-control loop | duplicated (MLX `stream_generation`, GGUF loop) | **one `drive`** |
| Prompt render (template+tokenizer) | in MLX leaf | shared `render` over `EngineMeta` |
| EOS derivation, UTF-8 detok, multi-EOS | per-leaf | shared |
| Model source: download/cache/resolve, KV/RAM preflight | MLX-wired | shared (`portability-shared-model-source`) |

**Pushed DOWN (hardware-specific — small, isolated, behind `LocalEngine`):**
the model **load** (weights/quant/mmap), the **forward** (the kernels), and the
**sampling implementation** (on-device or via the shared CPU sampler). That's it.

## The x86 engine (L5) as small compact components

Under `LocalEngine`, the x86 Vulkan engine decomposes into small, individually
testable pieces — the user's "isolate hardware in compact components":

- **`x86::device`** — Vulkan instance/device/queue; prefer an integrated GPU with a
  `HOST_VISIBLE | DEVICE_LOCAL` heap; feature-detect (`fp16`, `shaderInt8`,
  subgroups). *Test:* enumerates + selects on Intel and AMD.
- **`x86::memory`** — zero-copy `mmap` import (`VK_EXT_external_memory_host`) +
  host-visible buffer allocation. The UMA core. *Test:* mmap→import→read-back
  round-trip equals the bytes.
- **`x86::tensor`** — `Tensor` = `VkBuffer` + shape + dtype (fp16/fp32/packed-quant).
  Trivial; no logic.
- **`x86::kernels`** — the SPIR-V op set, **one small shader + dispatch fn each**:
  `quant_matmul` (AFQ 4/8-bit, MXFP4; gather variant for MoE), `sdpa` (GQA, sinks,
  sliding-mask), `rms_norm`, `rope` (YaRN), `swiglu`, `softmax`, `embedding`.
  *Test:* each op vs a CPU reference, op-by-op.
- **`x86::model::<family>`** — the forward, written from the `model-reference/`
  math over `x86::kernels`; implements `LocalEngine`. *Test:* greedy parity vs MLX.

Each component depends only on its true input (device on Vulkan; kernels on
device+tensor; model on kernels+reference-math) — so they're swappable and the
hardware blast radius is contained.

## Plan (the architecture step — sequenced before x86 P0)

- **A1 — Define the seam.** `src/engine.rs`: `LocalEngine`, `EngineMeta`,
  `EngineOptions`, the `drive` signature. Compiles `--no-default-features` (no
  hardware). *Done when:* the trait set type-checks and is documented.
- **A2 — Extract `drive` from the MLX leaf.** Generalize `stream_generation`
  (incl. the harmony branch) into `drive`; the MLX leaf implements `LocalEngine`
  (wrapping its `Generate`) and `MlxNativeBackend::chat` calls `drive`. *Done when:*
  the full MLX test suite + the agentic matrix pass unchanged, **no perf
  regression** (MLX still owns forward+sampling).
- **A3 — Adopt in the GGUF leaf + lift the rest.** GGUF implements `LocalEngine`,
  deletes its private loop (dedup bonus). Lift prompt-render, EOS, harmony, and the
  model-source/preflight into shared modules `drive`/loaders call. *Done when:*
  both leaves run through one `drive`; the only per-leaf code is `load` + the
  worker bridge.
- **Then** `x86-native-runtime` P0+ implements `LocalEngine` for Vulkan — and
  reuses A1–A3 for free.

> **Update 2026-06-18 — the x86 slot is scaffolded** (`src/x86/`, `x86-native-slot`).
> `X86Engine` is a real (stub) `impl LocalEngine` — the **second implementor**, which
> validates A1's seam shape against a non-MLX engine *without hardware* and pins the contract
> the Vulkan kernels fill. It compiles in the default CI, so the seam can't silently rot.
> A2's formal MLX `impl LocalEngine` + the A3 `drive` lift are still deliberately deferred to
> be shaped against this now-concrete x86 consumer.

## Non-goals

- Not a per-op tensor-framework abstraction shared at runtime across engines (the
  proven perf dead-end). The seam is at the **token/text** level, not the op level.
- Not changing the `ChatBackend` outer seam — it stays exactly as specced in
  `portability-and-the-backend-spi.md`.
- Not the kernels themselves (that's `x86-native-runtime` P1+). This step only
  draws the seam and lifts the shared layer.

## Risks

- **A2 is a refactor of the working, perf-tuned MLX leaf** (pipelined `async_eval`,
  prefix reuse, harmony, constrained decode). Do it behavior-preserving, gated by
  the existing tests + a before/after decode-throughput check; keep `stream_
  generation` callable until `drive` is proven at parity.
- The worker-thread bridge (async↔single GPU thread) may or may not generalize
  cleanly; A2 keeps it per-leaf if sharing it adds risk — the *substance* still
  moves into `drive`.

## Decisions (locked 2026-06-17)

- The internal seam is **`LocalEngine` at the token level** (engine yields tokens;
  the shared `drive` does everything text-level). Engines sample on whatever
  hardware suits them; the shared CPU sampler is the default for those that don't.
- The architecture step runs **first**, validated on MLX+GGUF, before any x86
  kernel work. Prerequisite of `x86-native-runtime`.
