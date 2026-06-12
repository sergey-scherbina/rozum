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
- [x] Auto-download for `hf:`/`mlx-community:`/`owner/repo` (HuggingFace) and
      `modelscope:<owner>/<repo>` (ModelScope) specs, then load — `src/hf_hub.rs` /
      `src/modelscope.rs` (`reqwest` streaming, no `hf-hub` crate). `ensure_model_dir`
      = cached-or-download, dispatched by spec prefix; `config.json` is fetched first
      and gated on a supported `model_type` before the weights; live per-file progress;
      `HF_TOKEN` / `MODELSCOPE_API_TOKEN` honored. Each writes the hub's **native cache
      layout** so downloads are shared with that hub's own tools — HF: proper
      `blobs/<etag>` + `snapshots/<commit>/<file>` symlinks + `refs/main` (verified:
      `huggingface_hub` / `mlx_lm` load OFFLINE against a rozum-written cache); ModelScope:
      flat files under `$MODELSCOPE_CACHE/hub/<owner>/<name>/`. `resolve_model_dir` stays
      the cache-only resolver (HF snapshot or ModelScope dir).
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
  resolution chain. (Update: the `mlx_lm.server` HTTP backend was later restored
  as **opt-in** — `try_mlx_server` in `openai_http.rs`, tried in the auto-chain
  only when `ROZUM_MLX_HTTP` is set, or forced via `--backend mlx-server`. It is
  off the default path; native MLX remains the default. For anyone who prefers to
  run their own `python -m mlx_lm.server`.)

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

## Backend feature parity vs mistralrs (audit 2026-06-11)

The native MLX backend (`src/mlx_native_backend.rs`) was audited side-by-side with
the mistralrs backend (`src/mistralrs_backend.rs`). Most request-handling gaps are
now closed; status (highest-impact first):

1. **Mid-prefill cancellation — DONE** (fork `fb263995` + rozum `b022dc4`). mistralrs
   races `tokio::select!{ cancel vs upstream.next() }`; the native equivalent: the
   hybrid `Generate` polls a `should_cancel` predicate between prefill chunks
   (`prefill_cancellable` -> `Ok(None)` ends the run, rozum emits `Done{Cancelled}`),
   wired to `job.cancel`. So a cancel/disconnect on a long prompt is now honored
   DURING prefill, not only per decode token after it — closing the native-side
   analog of the mistralrs large-prompt stall (`mistralrs-large-prompt-stall.md`); an
   abandoned long request no longer blocks the `concurrency_capacity()=1` worker. Test
   `mlx_qwen35_prefill_cancels_mid_prefill` (bails at chunk 3 of ~6, deterministic).
2. **Sampling params — DONE (top_p/top_k/seed + repeat_penalty)** (fork
   `f36c8c3a`/`e970b23a` + rozum `510c760`/`3597abe`). `sample_with(SamplerOpts)`
   (ported from mlx_lm: top-k mask -> top-p nucleus -> categorical; temp 0 stays
   argmax, oracle byte-exact) threaded through all four `Generate` via `set_sampler`;
   `seed` -> MLX RNG. `repeat_penalty` (HF convention) over a `REPEAT_CONTEXT=256`
   window via `take_along_axis`/`put_along_axis` (O(window)); each `Generate` keeps a
   token history only when penalty != 1.0 (greedy path untouched, skips the per-token
   id eval). Unit test pins top_k=1/tiny-top_p == argmax + a hard penalty moving the
   argmax. FOLLOW-UP: none required for single-turn.
3. **Tool use — DONE** (fork `1fc66029`/`e316dbf7` + rozum `09dfbcc`). `mlx-lm-utils`
   `ApplyChatTemplateArgs` gained a `tools` field threaded into the minijinja context
   (and minijinja's `json` feature enabled for the `tojson` filter the Qwen3 template
   uses). Rozum: `Job` carries `req.tools`; `render_prompt` builds OpenAI-style
   schemas (`tools_json`) for the template; `stream_generation` suppresses
   `<tool_call>` markup from the text stream and parses the run into
   `ToolUseStart/Delta/End` + `stop_reason=ToolUse` (`parse_tool_calls`; cancelled
   runs skip parsing). E2E `mlx_tool_use_weather` (model emits a `get_weather` call ->
   `stop=ToolUse`) + unit `parse_tool_calls_extracts`. Multi-turn tool history:
   `message_text` renders assistant `ContentBlock::ToolUse` blocks back into the
   prompt as `<tool_call>\n{json}\n</tool_call>` markup (inverse of `parse_tool_calls`),
   so a tool loop carries its prior call in history; `tool` results fold in as text
   via `ToolResult`. Unit `tool_use_round_trips_into_history` (render -> re-parse).
4. **Multiple EOS — DONE** (rozum `b022dc4`). `read_config` collects the full
   `eos_token_id` set; `stream_generation` stops on any (Qwen3: `<|im_end|>` 151645 +
   `<|endoftext|>` 151643).
5. **Load-time memory preflight / context retry — none** (folded into
   `mlx-native-mem-bound`). mistralrs retries a smaller `n_ctx` when the device map
   refuses + uses PagedAttention to bound the prefill peak.

Already at parity (generic, not per-backend): concurrency admission / backpressure /
OOM circuit breaker (via `concurrency::admit_wrap`, `concurrency_capacity()=1`);
streaming `ChatEvent`s; system/user/assistant/tool role rendering; `<think>`
reasoning is streamed (as plain text, since native emits raw tokens); max-tokens cap.
Not used by either backend: `session_id` prompt-prefix caching (advisory, unwired).

## Performance

**Status (2026-06-11).** Prefill is fast and its large-prompt memory peak is now
bounded (kernel + chunking + last-position projection, all byte-identical). Decode
(~12 vs Python ~22 t/s) is **mlx-rs per-op/per-call FFI-overhead bound** — both
obvious levers were tested and rejected: removing the custom-kernel per-call eval is
a no-op (decode isn't sync-bound), and `mx.compile` is net-negative in this binding
(probe below). What's done / next: (1) **`mlx-native-mem-bound`** — DONE (KV
preflight rejects an over-large context with a clear "lower --n-ctx" message instead
of OOM); (2) **SDPA `Causal` mode** in prefill (drop the explicit mask) — DONE
(fork `ef5cbca9`, byte-identical); (3) decode throughput is HARD and deprioritized
(needs hand-written fused Metal kernels or fork-level work to cut mlx-rs per-op
overhead). Details below.

### Measured (Qwen3.6-27B-4bit, M-series; oracle = pip mlx_lm 22 t/s decode)

| prompt | prefill (ops) | prefill (kernel) | decode  |
|--------|---------------|------------------|---------|
| 128    | 4.8s          | 1.3s             | ~13 t/s |
| 512    | 8.9s          | 3.4s             | ~12 t/s |
| 1024   | 20.9s         | 7.1s (**2.9x**)  | ~12 t/s |

(decode is noisy over 16 steps, ~8-13 t/s.) A 4-bit 27B reads ~15 GB/token, so
~27 t/s is the memory-bandwidth ceiling and Python's 22 t/s is near-optimal. Our
~12 t/s decode is **overhead/op-launch-bound** (~450 tiny matmul/conv dispatches
per token at T=1), not bandwidth-bound.

### Done: GatedDeltaNet Metal kernel (prefill ~2.9x)

The Qwen3.6 linear-attention scan was the prefill bottleneck (O(T) ops scan).
Bound `mx.fast.metal_kernel` in mlx-rs (`fast::MetalKernel` + `TemplateArg`, over
the mlx-c `mlx_fast_metal_kernel_*` API) and ported the Python gated-delta kernel
verbatim (`models::gated_delta::gated_delta_kernel`): the whole T-step scan in one
GPU dispatch. Default path; `ROZUM_GD_OPS=1` forces the ops reference. Greedy
output stays byte-identical to Python on 27B + 35B-A3B.

**Resolved: why the custom kernel needs a per-call `eval` (and why it's free).**
The kernel's `state_out` is a lazy buffer; without an immediate `eval` the ~60
later layers of the forward donate/reuse it before it materializes, silently
corrupting the recurrent state — decode diverges at **token 2** (the prefill's
first token is fine because it doesn't depend on `state_out`). The per-call `eval`
forces it concrete. It is a **buffer-donation hazard in a large deferred graph,
not an MLX-primitive bug** — confirmed by a 64-deep chained-kernel repro that is
correct deferred when no heavy ops run between calls. **The eval is FREE:** A/B
benched (decode tok/s, with vs without the 48 syncs/token) shows overlapping noise
(12 vs 12 at 1024 tok) — decode is op-launch/FFI-overhead-bound, identical either
way. So the per-call eval stays (correct + free); eval removal is not a decode lever
(and neither is `mx.compile` — see the dead-end note below). (The old `async_eval`
"garbage" was a separate concurrency artifact: MLX's single default stream raced a
2nd thread; the real worker is single-threaded.)

### Done: chunked prefill (bound the large-prompt peak)

`Model::prefill` (dense + MoE) processes the prompt in chunks of
`ROZUM_MLX_PREFILL_CHUNK` (default 2048): each chunk is a forward over `[1, chunk]`
with the caches carried, so the full-attention layers bound their SDPA peak to
`[chunk, ctx]` instead of `[T, T]`. (The explicit `[T, ctx]` causal mask array is
now gone entirely — the hybrid attention uses MLX's **fused causal SDPA mode**
(fork `ef5cbca9`), which handles the cache offset and skips the masked upper
triangle; byte-identical to the masked path on the oracle + chunked tests.) Between
chunks all caches are eval'd
(`LayerCache::collect_eval`) to materialize the chunk's forward and free its
activations, keeping the deferred graph from spanning the prompt; GatedDeltaNet is
already O(1) memory. **`lm_head` runs only on the final position** (`Model::project`):
the per-chunk hidden states feed only the caches, so the big vocab projection never
runs on discarded positions — this also avoids a `[1, chunk, vocab]` logits
transient (~600 MB at chunk 2048, vocab 151808) per chunk, another large slice off
the prefill peak. **Byte-identical to a single pass** (per-position attention +
sequential delta scan are position-local): `mlx_qwen35_chunked_prefill_matches_single_pass`
on a 3000-tok prompt gives `max|Δlogit|=0.000e0` (chunk 512 vs single-pass). Port of
mistralrs `f7efae2` (the native exact scan is faithful across chunks, unlike
mistralrs's chunked GatedDeltaNet which reorders FP reductions — so we can assert
byte-identity, they could not).

### mx.compile: `compile_with_state` is net-negative; plain `compile` is the untested lever

`mx.compile` is the presumed decode lever (decode is op-launch-bound). **A go/no-go
probe (`mlx_compile_probe`, dense Qwen3-4B, fixed shapes, fresh cache so the graph
is reused) measured it SLOWER, not faster:** T=1 uncompiled 8.79 ms vs compiled
17.34 ms (**0.51×**); T=16 35.6 vs 41.8 ms (0.85×). **But that probe used the wrong
API.** mlx-rs has two compile entry points and they marshal inputs very differently:

- `compile_with_state` (what the probe used): the returned closure re-marshals the
  whole `Updatable` state **every call** — `call_mut_with_state_inner` builds a fresh
  `VectorArray` of `args + updatable_states()`, i.e. flattens (and sorts) **all ~400
  model params per call** (`compile_with_state.rs:396`). The per-call binding overhead
  exceeds any fusion benefit for a model-sized forward → the 0.51× above.
- plain `compile` (`compile.rs:344`): `call_mut_inner` marshals **only `args`** —
  `VectorArray::try_from_iter(args.iter())`. Weight arrays referenced *inside* the
  compiled closure are captured into the traced graph once (as MLX captured inputs),
  exactly like Python `mx.compile`. On steady-state calls only the changing inputs
  (token id + cache) cross the FFI boundary. This path was **never probed.**

So the earlier "mx.compile is a dead end" was true only for `compile_with_state`. The
real decode lever is a **capture-based plain-`compile`d decode step** closing over the
weights and taking only `(token, cache)` as args — the same shape that lets Python's
`mlx_lm` sustain ~22 t/s vs our ~12. **Hard prerequisite:** a **fixed-shape KV cache**
(preallocate to max ctx + in-place slice-update with an offset), because today's
`ConcatKeyValueCache` grows by concat each step, changing shapes and forcing a recompile
(or shapeless overhead) every token. So the fixed-size-cache redesign is **not moot** —
it is the gate for the one decode win that has a chance of ~2×.

**Risk / why it is not yet implemented:** correctness-critical (must stay byte-exact),
and it intersects the GatedDeltaNet custom-kernel buffer-donation hazard — compile +
in-place cache + a kernel that donates buffers is precisely where the earlier token-2
divergence lived. Needs a clean machine for trustworthy A/B (current bench numbers are
degraded ~30% by session-long memory pressure: decode 9.5–14 vs the clean ~22 baseline).
Recommended as a dedicated `mlx-native-perf-compile` task, not a tail-end change.

### Done: large-context KV preflight (`mlx-native-mem-bound`)

`run_job` estimates the request's KV growth — `kv_bytes_per_position * (prompt_len +
max_tokens)`, where `kv_bytes_per_position = 2 (k+v) * full_attn_layers * n_kv_heads *
head_dim * 2 (bf16)` parsed from `config.json` (`text_config` for the hybrid wrapper;
only the full-attention layers hold KV — `full_attention_interval` selects them — the
GatedDeltaNet conv/recurrent state is O(1) in context). If it exceeds
`KV_SAFETY_FRAC=0.75` of `available_ram_bytes()` (macOS `vm_stat`), the request is
rejected with a clear `ModelError` ("context too large … lower --n-ctx / max_tokens …
fits ~N tokens now") instead of letting `ConcatKeyValueCache` grow into a Metal OOM.
Skipped when either term is unknown (no false negatives). Unit test
`kv_bytes_per_position_estimate`. FOLLOW-UP: a bounded/rotating KV cache to actually
cap resident KV for very long sessions (only worthwhile if the preflight isn't enough;
the fixed-size-cache redesign is otherwise moot — `mx.compile` is a dead end).

### TODO (see SPRINT `mlx-native-perf` + BACKLOG)

(Prefill is now fully optimized: GatedDeltaNet kernel + chunking + last-position
projection + fused causal SDPA, all byte-identical. The KV preflight bounds the
large-context memory. Only the decode lever remains, and it's hard.)

1. **Decode (~12 t/s) is per-call FFI-overhead-bound** — eval-removal is free/no-op;
   `compile_with_state` is net-negative (re-marshals ~400 params/call). The one
   untested lever with ~2× potential is a **capture-based plain-`compile`d decode
   step** (weights captured into the trace, only token+cache marshaled per call) —
   matching how Python `mlx_lm` hits ~22 t/s. Gated on a **fixed-shape KV cache**
   (preallocate + in-place slice-update; today's `ConcatKeyValueCache` grows by
   concat and defeats compile). See the mx.compile section. Tracked as
   `mlx-native-perf-compile`; needs a clean machine to A/B. The fallback lever
   (manual Metal-kernel op-fusion to cut ~450 dispatches/token) is larger and lower-
   priority. Decode is usable today; this is a speedup, not a correctness gap.
