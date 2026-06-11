# Native MLX runtime (on the oxideai `mlx-lm` Rust crate)

## Overview

rozum's in-process, pure-Rust MLX runtime: run MLX-community checkpoints through
a **full native MLX forward pass** (no candle, no Python, no subprocess) so we
get MLX's real advantages -- kernel fusion, no cross-runtime sync, Apple-tuned
quant/attention kernels, and day-one support for new architectures -- in a
single Rust binary.

This **supersedes two earlier tracks**:

- `mistralrs-mlx-direct` (the targeted candle->MLX quant-op bridge) is a proven
  dead end: it hit a structural per-op cross-runtime GPU-sync floor (~12 T/s vs
  candle's ~100 T/s on Qwen3-4B-4bit) because candle and MLX own separate Metal
  queues and can't order each other's work without a CPU stall. MLX's speed is
  all-or-nothing: you only get it when MLX owns the whole graph.
- `mlx-native-port` (port mlx_lm from scratch over raw mlx-rs ops) is no longer
  necessary from a blank page: the [`oxideai/mlx-rs`](https://github.com/oxideai/mlx-rs)
  workspace already ships a Rust `mlx-lm` crate (the scaffolding **and** Qwen3
  dense + Llama), so we build on it instead of rewriting it.

It also lets us **retire `mlx_lm.server`** (the Python subprocess MLX path):
native MLX gives the same MLX speed + parity + day-one models, but in-process
and Python-free.

## What already exists upstream (probed 2026-06-11, oxideai/mlx-rs)

- `mlx-rs 0.25.3` (crates.io): MLX core + `nn` (`Linear`, `QuantizedLinear`,
  `RmsNorm`, `Embedding`, RoPE, activations, `transformer`, `recurrent`,
  `convolution`) + `fast::{rope, scaled_dot_product_attention, rms_norm,
  layer_norm}` + `ModuleParametersExt::load_safetensors`. All the primitives an
  mlx_lm model needs, including conv1d/recurrent for hybrid attention.
- `mlx-lm` crate (in-repo, **v0.0.1, not on crates.io**): `models::{qwen3,
  llama}`, `cache` (`ConcatKeyValueCache`), `sampler` (greedy at `temp==0`,
  categorical otherwise), `generate/`. `Generate::new(&mut model, &mut cache,
  temp, &prompt)` is a **token iterator** (streaming + cancel friendly).
- `mlx-lm-utils` crate: HF `Tokenizer` wrapper with
  `apply_chat_template_and_encode` + `decode`.
- Examples `lm` and `mistral` are working LLM drivers; `lm` is our integration
  template (load model dir -> tokenizer + chat template -> `Generate` iterator
  -> decode loop).

## Interface

```rust
// Feature gate: #[cfg(feature = "mlx-native")] (off by default; Apple-Silicon only).

pub struct MlxNativeOptions {
    pub n_ctx: u32,          // default 32_768; env ROZUM_MLX_N_CTX
    pub temperature: f32,
    pub top_p: f32,
    pub top_k: u32,
    pub max_tokens: usize,
}

pub struct MlxNativeBackend { /* private: loaded model + tokenizer + chat template */ }

impl MlxNativeBackend {
    /// `model_spec`: "/abs/dir", "hf:<user>/<repo>", or "mlx-community:<repo>".
    /// Resolves to a local dir via hf-hub, then loads the matching model arch.
    pub fn new(model_spec: &str, opts: MlxNativeOptions) -> ModelResult<Self>;
}

#[async_trait]
impl ChatBackend for MlxNativeBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    fn context_window(&self) -> u32;
}
```

- New crate feature `mlx-native` pulling the vendored `mlx-lm` (+ transitively
  `mlx-rs`/`mlx-sys`). Off by default (heavy compile, builds MLX from source).
- Vendored fork at `.vendor/mlx-lm`, pinned by git rev in `Cargo.toml`
  (`[patch]` or direct `git=` dep), mirroring the mistral.rs setup. Registered
  in `REPOS.md`.
- Resolution chain (`build_gateway_backend`): native MLX becomes the **top**
  entry for MLX-format specs (above in-process GGUF for `.gguf`, above the
  now-deprecated `mlx_lm.server` HTTP). candle/mistralrs stays for GGUF and as a
  fallback + parity oracle.

## Behavior

- [x] `MlxNativeBackend::new` loads weights + tokenizer + chat template once (on
      the worker thread); every `chat()` reuses them.
- [ ] Auto-download via `hf-hub` for `hf:`/`mlx-community:` specs into a local
      dir, then load. (Today: `resolve_model_dir` reuses an already-downloaded
      HF snapshot; download is the open gap.)
- [x] Loads **AFQ-quantized** mlx-community checkpoints (4-bit g64) via the
      fork's config-driven `nn::quantize` + remapped `load_safetensors`.
- [x] Streaming: drive the `Generate` token iterator, map each token to
      `ChatEvent::TextDelta`; `ChatEvent::Done` on EOS / max-tokens / cancel.
- [x] EOS: stop on the model's `eos_token_id` from `config.json` (int or list),
      falling back to Qwen3 `<|im_end|>`.
- [x] Per-token cancel via `req.cancel`: checked between iterator steps; stops
      within one decode step.
- [~] Sampling: temperature + max_tokens honored. top_p/top_k/repetition_penalty
      still need the upstream `Generate`/sampler extended (open gap).
- [x] Chat template + system prompt: rendered via `mlx-lm-utils` from the repo's
      `tokenizer_config.json` (system/user/assistant/tool roles).
- [ ] Tool-use: reuse `crate::gguf::ToolUseParser` for Qwen-hermes `<tool_call>`
      blocks; emit `ToolUseStart`/`Delta`/`End`. (Open gap.)
- [x] **Parity gate (Qwen3-4B-4bit):** greedy decode byte-identical to Python
      `mlx_lm` (proven in Phase 0); the SPI streams the same "Paris" answer.
- [x] **Perf gate:** ~106 T/s decode >= the candle path (~100) on the same Mac.
- [x] `cargo build` (default, no `mlx-native`): unaffected, no MLX/Python deps.
- [x] `cargo build --features mlx-native`: builds with MLX from source on
      aarch64-apple-darwin (full Xcode required).

## Out of scope

- Non-Apple platforms (Apple-Silicon only).
- Training / fine-tuning / LoRA.
- Vision / multimodal / audio / diffusion models (text LLMs only this track).
- Quantization *conversion* (consume pre-quantized mlx-community checkpoints).
- Speculative decoding / MTP (later, if upstream gains it).

## Design

### Build on the crate, port only the missing models

The upstream `mlx-lm` crate gives us the entire non-model scaffolding (cache,
sampler, generate loop, tokenizer, chat template, quantized loading) plus Qwen3
dense and Llama. Our work is:

1. A thin `MlxNativeBackend` adapting the `Generate` iterator to rozum's
   `ChatBackend`/`ChatStream` (streaming, cancel, EOS, tool-use, sampling glue).
2. Porting the model architectures we need into the vendored fork, each copying
   the structure of `models/qwen3.rs` and validated against the Python
   `mlx_lm/models/<arch>.py` + our existing AFQ/Qwen3.6 findings.

### Integration shape (from the `lm` example)

```text
new(spec): hf-hub resolve -> dir; Tokenizer::from_file + chat template;
           load_<arch>_model(dir)  [once]
chat(req): render prompt (chat template + system + tools) -> token ids
           Generate::new(&mut model, &mut cache, temp, &prompt)  [iterator]
           for tok in generate:  check req.cancel; EOS? -> Done;
                                  decode delta -> ChatEvent::TextDelta
```

`Generate` being a pull iterator gives streaming and cancel for free (stop
iterating). KV cache is `ConcatKeyValueCache` (per request).

### Models to port (broad-catalog target)

Ordered by user value and difficulty:

1. **Qwen3 dense** -- already upstream; Phase 0 just wires + validates it.
2. **Qwen3 MoE (30B-A3B)** -- port `qwen3_moe.rs` (top-k routing + parallel
   experts; `nn` + `gather_qmm` cover it). Reference: `mlx_lm/models/qwen3_moe.py`.
3. **Qwen3.6 hybrid (27B dense + 35B-A3B MoE)** -- the hard one: GatedDeltaNet
   linear-attention + conv1d + hybrid (linear/full) KV schedule + MoE. We have
   strong tailwinds: mlx-rs `nn::{recurrent, convolution}`, the Python
   `qwen3_5.py`/`qwen3_5_moe.py` reference, AND our hard-won knowledge from the
   mistralrs integration (RMSNorm `+1` convention, AFQ nibble/layout, hybrid KV
   cache) captured in `docs/specs/mlx-weight-layout-and-afq.md`.
4. **Llama / Qwen2.5 / Qwen2.5-Coder** -- Llama is upstream; Qwen2.5 is a small
   delta from Qwen3. Round out the catalog.

### Gaps in upstream to fill (our fork / PRs)

- Sampler: add top_p / top_k / repetition_penalty if missing (only greedy +
  temp categorical confirmed).
- EOS-driven stop (example uses a fixed token count).
- HF auto-download to a dir (example assumes a local dir).
- Quantized-checkpoint load path verified for AFQ widths beyond the example's
  bf16 model.
- Streaming/eval cadence tuned for low latency (the example evals every 20
  tokens; interactive wants per-token or small batches).

### Composition / assets reused

- **Two parity oracles:** `scripts/mlx_ref.py` (Python mlx_lm) and the
  candle/mistralrs path -- diff layer activations + final tokens against both.
- The mistralrs/candle backend stays for GGUF and as fallback/oracle; only the
  *MLX* path changes owner.
- `gguf::ToolUseParser`, `hf-hub` resolvers, and `ChatBackend` SPI are reused
  unchanged.

## Phased delivery

Each phase has a numerical-parity + perf exit gate; don't start N+1 until N
passes on at least one real model.

- **Phase 0 -- wire crate + Qwen3 dense + ChatBackend.** Vendor-fork `mlx-lm`,
  add `mlx-native` feature, build `MlxNativeBackend` on the `Generate` iterator.
  Gate: `mlx-community/Qwen3-4B-4bit` runs end-to-end via `rozum`/gateway,
  byte-for-byte greedy vs both oracles, decode T/s >= candle. Confirms the whole
  thesis (real MLX speed, no bridge).
- **Phase 1 -- Qwen3 MoE.** Port `qwen3_moe`; gate on `Qwen3-30B-A3B-4bit`.
- **Phase 2 -- Qwen3.6 hybrid.** Port `qwen3_5` (27B dense) then `qwen3_5_moe`
  (35B-A3B); gate on the cached `mlx-community/Qwen3.6-{27B,35B-A3B}-4bit`. This
  is the headline: the models the user actually runs, in a pure-Rust binary.
- **Phase 3 -- broaden catalog.** Llama, Qwen2.5, Qwen2.5-Coder, etc.
- **Phase 4 -- promote + retire `mlx_lm.server`.** Make native MLX the default
  top-of-chain for MLX specs; remove the Python subprocess path; update SPEC.md
  resolution chain.

## Decisions

- **Build on oxideai `mlx-lm`, not port from scratch** -- the scaffolding +
  Qwen3 + Llama already exist; rewriting them is waste. Rejected:
  `mlx-native-port`'s blank-page plan (superseded).
- **Vendor-fork (`.vendor/mlx-lm`), pin git rev** -- we must add models, so we
  need a writable fork; the local vendored checkout matches the mistral.rs
  workflow and keeps iteration fast. PR the new models upstream. Rejected:
  read-only git-dep (can't host our model ports without push+bump churn).
- **Top-of-chain, retire `mlx_lm.server`** -- native MLX delivers the same
  capability in-process and Python-free; keeping the subprocess path long-term
  is redundant. candle/mistralrs stays for GGUF + as oracle.
- **Broad-catalog target** -- aim past Qwen3.6 to Llama/Qwen2.5/Coder so the
  native path covers the mlx-community spectrum, not just the user's current
  models.
- **Full MLX forward, never hybrid** -- the mlx-direct post-mortem: mixing
  candle + MLX at op granularity is a hard perf floor. The forward stays 100%
  MLX; candle is only an external oracle, never in the graph.

## Risks / sharp edges

- **`mlx-lm` is v0.0.1** -- early, unpublished, API churn likely, feature gaps
  (sampler, prompt cache, cancellation, streaming ergonomics). Phase 0 doubles
  as a maturity probe; pin a known-good rev and bump intentionally.
- **Quantized load must work** -- the example model is bf16; verify AFQ 4-bit
  load via `QuantizedLinear`/`load_safetensors` early (Phase 0 blocker if not).
- **Hybrid (Qwen3.6) is genuinely hard** -- linear attention + conv1d + hybrid
  cache. Mitigated by mlx-rs primitives + Python reference + our prior findings,
  but budget the most time here; numerical drift is the killer (plan ~50% of the
  phase on activation-diff debugging against the oracles).
- **MLX-from-source build** -- mlx-sys compiles MLX (cmake + Metal), needs full
  Xcode; first build ~minutes. Keep the feature off by default so meeting-room /
  GGUF builds are unaffected.
- **Numerical parity is non-negotiable** -- gate every model on byte-for-byte
  greedy tokens vs the oracles; "plausible but wrong" output silently degrades
  agent use.

## Results

### Phase 0 -- IN PROGRESS (2026-06-11). Fork `sergey-scherbina/mlx-rs` branch
`rozum-mlx-native`, commit `1205b164`. Vendored at `.vendor/mlx-lm`.

- **Speed thesis PROVEN.** `mlx-community/Qwen3-4B-4bit` via the upstream
  `Generate` iterator (release): loads in ~0.3s, **decode ~121 T/s** -- faster
  than candle (~100) and ~10x the targeted bridge (~12). Full MLX forward, no
  candle in the graph. This is the whole point of the pivot, confirmed.
- **AFQ loading: 3 upstream gaps found + fixed** (the "does it load AFQ as-is"
  Phase 0 risk -- answer was NO, now yes):
  1. `ModelArgs` ignored config.json's `quantization` block -> `Model::new`
     built plain `Linear`. Now read it and `nn::quantize(model, gs, bits)`
     before load so the QuantizedLinear structure matches.
  2. `load_qwen3_model` required `model.safetensors.index.json`; single-file
     checkpoints ship only `model.safetensors`. Added a fallback.
  3. mlx-rs `QuantizedLinear/Embedding` nest the packed weight at
     `<p>.inner.weight`, but checkpoints store `<p>.weight`; `load_safetensors`
     is **non-strict** (silently skips unmatched keys -> random weights ->
     garbage). Custom loader remaps `<p>.weight -> <p>.inner.weight` when a
     sibling `<p>.scales` exists. **904/904 params load.**
- **Forward bug #1 (fixed): dead KV cache.** `Qwen3Model::forward` initialized
  cache slots to `None`, but `Attention::forward` only reads/writes the cache on
  the `Some` branch -- so every decode step ran cache-less (no history, wrong
  position), degenerating into repetition. Fixed by initializing slots to
  `Some(C::default())` (commit `1bbe6e52`). Output became coherent.
- **Forward bug #2 (FIXED): RoPE reshape-to-3D corrupts decode.** After the
  cache fix, greedy still diverged from `mlx_lm` at the 2nd token. An exhausting
  bisection (env-gated `ROZUM_{LOGIT,LAYER,ATTN,QPROJ}_DEBUG` dumps + a Python
  `mx.fast.*` monkeypatch oracle) finally pinned it:
  - Full 26-token prefill matches `mlx_lm` byte-for-byte; the model is correct.
  - Decode diverges at layer 0. `q_proj` output and the **post-q_norm**, pre-RoPE
    query match Python; the **post-RoPE** query does NOT: head 0 is rotated, but
    head 31 is left **un-rotated** (post-RoPE == pre-RoPE for that head).
  - **Root cause:** mlx-rs `nn::Rope::forward` reshapes `[B, n_heads, L, head_dim]`
    to `[-1, L, head_dim]` before `mx.fast.rope`. For the single-position decode
    (`L == 1`) case the resulting `[B*n_heads, 1, head_dim]` trips an MLX fast-rope
    bug that rotates only the first batch row -> every head past the first keeps
    an un-rotated query -> wrong attention -> garbage decode. Python `mlx_lm`
    calls `mx.fast.rope` on the 4D tensor directly and is unaffected.
  - **Fix:** drop the reshape; apply RoPE on the input shape directly (in mlx-rs
    `nn::Rope::forward` and the mlx-lm `RopeVariant`). Decode now byte-matches
    `mlx_lm` (`STEP2 argmax 198`), and `mlx-community/Qwen3-4B-4bit` generates
    *"The capital of France is Paris."* identically to Python at **~106 T/s**.
  - **NOT the cause (ruled out, each cost real time):** MLX version (0.31.2
    reproduces the bug), the mask (None/zeros/bool/causal), attention sinks,
    array layout/contiguity, device (both GPU), and the SDPA kernel itself --
    the SDPA was fine; its *query input* was corrupt. The MLX 0.31.2 bump (the
    chosen path) was carried out (mlx-c `fft.cpp` excluded + `ops.cpp`
    `global_scale` args patched so MLX 0.31.2 builds) but did **not** fix decode,
    disproving the version hypothesis. Reverted to MLX 0.30.6 since the RoPE fix
    is version-independent and keeps the mlx-c submodule unpatched.
  - Bonus correctness fix (kept): mlx-rs `fast::scaled_dot_product_attention` now
    passes a null-ctx `mlx_array` (not `mlx_array_new()`, an empty-but-non-null
    array) for the no-mask / no-sinks case, matching Python's `mask=None`
    semantics. (Not the decode bug, but the old behavior was wrong.)
- **Phase 0 dense correctness + speed: DONE.** Qwen3-4B-4bit loads AFQ
  (904/904), generates byte-identical to `mlx_lm`, decodes ~106 T/s (> candle
  ~100, ~10x the mistralrs bridge). The native-MLX thesis is fully proven.
- **`MlxNativeBackend` wired (DONE, `b25497c`).** MLX is `!Send` (a single
  Metal stream), so a thin handle cannot hold the model; instead a dedicated
  worker thread owns it for life — it loads the weights itself (they never
  cross a thread boundary) and serves jobs off an `mpsc` queue, streaming
  `ChatEvent`s back over a per-request channel. The backend is the Send+Sync
  handle. It renders the model chat template (system/user/assistant/tool roles
  — our own role strings, since the helper's `Role` enum omits system/tool),
  runs the `Generate` iterator, and stops on EOS / max-tokens / cancel.
  Detokenization is incremental and UTF-8-safe: re-decode the run each step and
  emit the new suffix, holding back a trailing replacement char so an
  incomplete multi-byte sequence (e.g. mid-Cyrillic) never leaks. `mlx-native`
  is an off-by-default cargo feature (path deps on the fork now; git-rev pin at
  merge, like mistralrs); `build_gateway_backend` tries it before mistralrs for
  MLX checkpoints, and `concurrency_capacity() = 1` lets `admit_wrap` gate it.
  Verified end-to-end through the real SPI: streams a correct "Paris" answer in
  ~3.7s incl. load.
- **Phase 1 — `qwen3_moe` (Qwen3-30B-A3B): DONE.** Dense Qwen3 attention reused
  verbatim; the sparse MoE MLP is a router `gate` (quantized Linear) ->
  `softmax(precise)` -> `argpartition` top-8 -> `take_along_axis` scores ->
  `SwitchGLU` experts via `gather_qmm` -> score-weighted sum. AFQ experts are 3D
  `[num_experts, out, in]` raw `Param<Array>` (not `nn::quantize`'d); the load
  remap is target-aware (add `.inner.weight` only where that param exists, so
  QuantizedLinear leaves get it and the experts keep `.weight`). Token-sorting
  (a memory-access optimization in Python `mlx_lm`) is skipped — `gather_qmm` is
  numerically identical sorted or not. The backend dispatches `qwen3`/`qwen3_moe`
  by `config.json` `model_type` via a `LoadedModel` enum feeding one generic
  streaming loop. **Greedy output byte-for-byte identical to Python `mlx_lm`**:
  `<think>\n\n</think>\n\nThe capital of France is Paris.` (1351 params loaded;
  ~4.6s load+gen). All 48 layers sparse (`mlp_only_layers=[]`); dense MoE layers
  fail loud for now.
- Next gaps: hf-hub auto-download (today reuses an already-downloaded HF
  snapshot via `resolve_model_dir`); sampler top_p/top_k/rep-penalty (the fork
  `Generate` only takes temp); tool-use streaming; EOS list from config. Then
  Phase 2 (Qwen3.6 hybrid: `qwen3_5` 27B + `qwen3_5_moe` 35B-A3B).

### Gaps fixed in the fork (upstream PR candidates to oxideai/mlx-rs)

Config-driven AFQ quantization on load; single-file safetensors fallback; the
`.inner.weight` key remap; **the `nn::Rope` L=1 reshape bug**; the no-mask/sinks
null-array fix; KV-cache slot init. Still TODO: top_p/top_k/rep-penalty sampler,
EOS-from-config, hf-hub download.

## Performance

### Measured (Qwen3.6-27B-4bit, M-series; oracle = pip mlx_lm 22 t/s decode)

| prompt | prefill (ops) | prefill (kernel) | decode |
|--------|---------------|------------------|--------|
| 128    | 4.8s          | 1.3s             | ~7 t/s |
| 512    | 8.9s          | 3.4s             | ~7 t/s |
| 1024   | 20.9s         | 7.1s (**2.9x**)  | ~7 t/s |

A 4-bit 27B reads ~15 GB/token, so ~27 t/s is the memory-bandwidth ceiling and
Python's 22 t/s is near-optimal. Our 7 t/s decode is **overhead-bound** (kernel
launches / syncs), not bandwidth-bound.

### Done: GatedDeltaNet Metal kernel (prefill ~2.9x)

The Qwen3.6 linear-attention scan was the prefill bottleneck (O(T) ops scan).
Bound `mx.fast.metal_kernel` in mlx-rs (`fast::MetalKernel` + `TemplateArg`, over
the mlx-c `mlx_fast_metal_kernel_*` API) and ported the Python gated-delta kernel
verbatim (`models::gated_delta::gated_delta_kernel`): the whole T-step scan in one
GPU dispatch. Default path; `ROZUM_GD_OPS=1` forces the ops reference. Greedy
output stays byte-identical to Python on 27B + 35B-A3B.

**Known bug (drives the decode gap):** the custom-kernel primitive returns garbage
unless each call's outputs are **blocking-`eval`'d immediately**; any deferral
(`async_eval`, batched eval, per-token eval only) corrupts. Python's same kernel
works deferred, so this is an MLX/mlx-c deferred-custom-kernel issue or a binding
gap, not a lifetime bug (ruled out: shared kernel object, input lifetime, config
lifetime, fresh-per-call). The blocking eval is correct but costs ~half the decode
throughput; fixing it unlocks the `async_eval` path (~17 t/s decode).

### TODO (see SPRINT `mlx-native-perf` + BACKLOG)

1. **Decode-bug dig** — diff the `MetalKernel::apply` binding vs Python
   `mx.fast.metal_kernel` (row-contiguity, output↔input graph linkage, init
   values); build a minimal repro (N tiny custom kernels, deferred eval). Unlocks
   ~17 t/s decode.
2. **mx.compile the forward + small-op fusion** — the deeper decode lever; blocked
   on (1) (the custom kernel inside a compiled graph) and the stateful caches.
3. **Chunked prefill** (port mistralrs `f7efae2`) — bound the full-attention
   `[T,T]` activation peak so large prompts don't OOM.
4. **Large-context memory bounding** — KV-pool bound / preflight (analog of the
   mistralrs RAM preflight + context budgeting); native `ConcatKeyValueCache`
   grows unbounded.
