# Backlog

## Optional Model Adapters

Model adapters are optional. They must not be required for the default build,
default CLI startup, meeting rooms, round-robin moderation, or manual moderation.

- [x] candle-backend - Implement a real Candle adapter behind `InferenceBackend`.
  - Prefer pure Rust and keep heavyweight features gated.
  - Compare output and latency against `llama-gguf`.

- [ ] native-gguf-backend - Superseded by sprint task `gguf-backend` (in-process via llama-cpp-2). Remove this entry when that task is complete.

- [ ] llama-gguf-library-backend - Superseded by sprint task `gguf-backend`. Remove when complete.

- [ ] external-command-backend - Superseded; OpenAI-HTTP client backend covers the Ollama/LMStudio HTTP use case if needed.

- [ ] mlx-native-backend - Native MLX inference via `mlx-rs` (no Python, no subprocess).
  - Only worthwhile if benchmarks show >10% throughput gain over llama-cpp-2 Metal.
  - Requires porting Qwen3-30B-A3B forward pass + KV-cache + chat-template to Rust.
  - Plugs directly into the `ChatBackend` trait from `chat-backend-spi.md` — no SPI change needed.
  - Spec: `docs/specs/mlx-native-backend.md` (to be written when scheduled).

- [ ] candle-real-streaming - Stream tokens from Candle via `TokenOutputStream` instead of one-shot.
  - Low priority: Candle-Metal is slower than llama-cpp-2 on the target models.

## Native MLX runtime — performance (ports from the mistralrs work)

The native MLX runtime (`docs/specs/mlx-native-runtime.md`) shipped correctness +
the GatedDeltaNet prefill kernel. These carry over optimizations proven in the
mistralrs backend that the native runtime does NOT yet have. (Concurrency,
admission, backpressure and the OOM circuit breaker already apply generically
through `concurrency::admit_wrap`, so they are not relisted.)

- [~] mlx-hand-fused-gdn-kernels — **PROBED 2026-06-14: low reward, deferred.** Re-measured
  the MoE hybrid decode (`mlx_qwen35_moe_decode_bench`, 35B-A3B — the e2e model): **~59-60 t/s**,
  serial==pipe (pipelining gives only 1.02× — see why below), and the SPLIT timing is
  **`build=15.65ms/tok, eval=1.31ms/tok` → 92% of per-token time is CPU graph-build / FFI**,
  only 8% GPU. Dumped the decode-step graph (`ROZUM_DUMP_DOT`): **122 primitive nodes**, and
  the hot elementwise ops are **already auto-fused by MLX** at eval — the gate sigmoid·multiply
  shows up as `CompiledSigmoidBroadcastBroadcastMultiply` (5×), `RMSNorm` is fused (7×), and
  there are **no stray `AsType`** (the bf16-stream fix held). So the original premise — that
  `compute_g`/gate are *unfused* and need hand-written `metal_kernel`s — no longer holds; MLX's
  automatic elementwise fusion already collapses them. Custom kernels would duplicate MLX and
  carry the hybrid byte-exactness risk for ~no gain. **The bottleneck is the 92% build/FFI
  cost** (≈0.13 ms × 122 op-launches/token of Rust→C→C++), which pipelining can't hide (build ≫
  eval). The obvious lever for that is `mx.compile` (trace once + reuse) — **but it's confirmed
  dead in mlx-rs (see `mlx-native-perf-compile` below): re-probed plain `compile` on Qwen3-4B
  (7× bigger build than the original 0.6B probe) and it's STILL net-negative (0.64×); mlx-rs's
  `compile` adds more overhead than the per-token build it saves, independent of model size.**
  So the build cost isn't reducible via the available APIs (MLX already auto-fuses the
  elementwise ops; mlx-rs compile doesn't deliver the Python `mx.compile` win). Decode at
  ~59 t/s is already fast and the dominant agentic latency (prefill) is solved by prefix-KV
  reuse. **Don't pull hand kernels; don't pull compile.** (Probe was the MoE; the dense 27B
  hybrid runs all params per token and is slower — re-probe it separately if it becomes the
  primary model.) Diagnostics:
  `ROZUM_DUMP_DOT=/tmp/d.dot … mlx_qwen35_moe_decode_bench` + a DOT label histogram.

- [ ] mlx-native-batched-decode — true parallel serving (multiple concurrent sessions).
  **PROBED 2026-06-14 — viable + high value (`mlx_batched_decode_probe`, dense Qwen3-4B):** a
  B=2 batched `forward` is **byte-exact per sequence** (each row == running that sequence alone)
  and runs **2 sequences at 212.7 tok/s vs 108.7 serial = 1.96×** (near-linear). **Why so close
  to linear: decode is 92% CPU graph-build (FFI), and batching does ONE build for B sequences
  instead of B** → it amortizes exactly the per-token build that `mlx-native-perf-compile` tried
  (and failed) to eliminate. So batched decode is BOTH the multi-session throughput lever AND
  the real answer to the build-bound decode — the two perf threads converge here. **Remaining
  work to ship (the big part):** (1) a worker scheduler that collects up to B jobs + continuous
  batching (admit/retire mid-decode); (2) **ragged lengths** — sequences differ in length, so
  the batched cache needs left-padding + a per-row additive attention mask (mask the pad region),
  or per-sequence offsets; (3) per-sequence EOS/stop/cancel + streaming; (4) raise
  `concurrency_capacity()` to a memory-budgeted `B`. (5) **Hybrid is the hard part** — the
  GatedDeltaNet recurrence can't be left-padded (padding pollutes the running state), so its
  batching needs per-row state without feeding pad tokens through the recurrence (conv cache +
  recurrent state on the batch axis, byte-exact per row). Recommend: ship dense first (the probe
  proves it), hybrid second. The probe (`src/mlx_native_backend.rs`) is the reusable foundation.
  TODAY: the native MLX backend is capacity-1 — one OS worker thread owns the `!Send` model
  and runs jobs strictly serially (`worker_main`'s `while blocking_recv { run_job }`);
  `concurrency_capacity()=Some(1)`, so `admit_wrap` admits 1 and queues the rest (bounded
  `ROZUM_ADMIT_QUEUE_MAX`=32, shortest-job-first + fast lane, HTTP 429 on overflow). That's
  fine for ONE active CC/Codex session; many simultaneous sessions serialize (queued, not
  parallel). To actually serve N in parallel, add **continuous/batched decode** to the
  native runtime: batch B sequences in one `forward` (MLX has the batch dim), a per-sequence
  KV cache stacked on the batch axis (extend `ConcatKeyValueCache` / the GatedDeltaNet conv
  + recurrent state to a batch axis), ragged prefill admission, and per-sequence
  EOS/stop/cancel + streaming. Then raise `concurrency_capacity()` to a memory-budgeted
  `budgeted_max_num_seqs` (the budget machinery already exists; mistralrs uses it). Big:
  touches `Generate`, every model's `forward`, all KV/conv/recurrent caches, and the
  admission wiring. Throughput win scales with B until memory/Metal-bandwidth bound;
  single-stream latency unchanged. Only pull when concurrent multi-session serving is a real
  requirement (today's queue+SJF+429 is a reasonable single-GPU answer). Hybrid (Qwen3.6)
  is the hard part — the gated_delta kernel + conv cache must batch correctly (byte-exact
  per sequence vs the B=1 path).

- [x] mlx-native-chunked-prefill - DONE. `Model::prefill` chunks the prompt
  (`ROZUM_MLX_PREFILL_CHUNK`, default 2048), bounding the full-attention
  `[chunk, ctx]` causal-mask + SDPA peak instead of `[T, T]`; caches advance and
  are eval'd between chunks to free activations. `lm_head` runs only on the final
  position (`Model::project`), dropping the per-chunk `[1,chunk,vocab]` ~600MB
  logits transient too. Byte-identical to single pass
  (test `mlx_qwen35_chunked_prefill_matches_single_pass`, Δ=0). See SPRINT.

- [x] mlx-native-mem-bound - DONE (preflight). `run_job` estimates the request's KV
  footprint (`kv_bytes_per_position * (prompt_len + max_tokens)`, full-attention
  layers only — GatedDeltaNet state is O(1)) and rejects with a clear "context too
  large … lower --n-ctx / max_tokens … fits ~N tokens" `ModelError` when it exceeds
  75% of `available_ram_bytes()` (vm_stat), instead of letting Metal OOM. Unit test
  `kv_bytes_per_position_estimate`. FOLLOW-UP: a bounded/rotating KV cache to cap
  resident KV for very long sessions (only if the preflight isn't enough). See SPRINT.

- [x] mlx-native-decode-bug - RESOLVED. The custom-kernel "needs a blocking eval
  per call" rule is a buffer-donation hazard: the kernel's lazy `state_out` gets
  donated/reused by the ~60 later layers before it materializes, corrupting the
  recurrent state (decode diverges at token 2). The per-call eval forces it
  concrete and fixes it. A/B benched: the eval is FREE (decode is op-launch-bound,
  not sync-bound — 12 vs 12 t/s with/without). NOT a path to faster decode, and the
  obvious fusion lever (`mlx-native-compile`) turned out a measured dead end — see
  below; decode is FFI/per-op-overhead bound. See SPRINT `mlx-native-perf`.

- [x] mlx-native-compile - `compile_with_state` is net-NEGATIVE (measured), but this
  only rules out ONE of mlx-rs's two compile APIs. Probe `mlx_compile_probe` (dense
  Qwen3-4B): T=1 0.51x (8.79->17.34ms), T=16 0.85x — because `compile_with_state`
  re-marshals + sorts all ~400 params per call. **Plain `compile` (`compile.rs:344`)
  marshals only the args and captures referenced weights into the trace** — the way
  Python `mlx_lm` reaches ~22 t/s vs our ~12 — and was never probed. See
  `mlx-native-perf-compile` below; the fixed-shape-cache prereq is NOT moot.

- [x] mlx-native-perf-pipeline - **DONE (merged).** Decode-speed root cause settled:
  it was PIPELINING, not compile/cache. `stream_generation` now `async_eval`s step n+1
  before blocking on step n (dense arches: Qwen3/Qwen3-MoE/Llama/Qwen2; hybrid stays
  serial). Qwen3-4B **114→128 t/s = 96.5% of Python**; byte-exact all arches. Compile
  probes (`mlx_compile_probe_plain`) showed plain `compile` is 0.69× — not the lever;
  the fixed-cache + compiled-decode redesign is shelved. Spec: mlx-native-runtime.md
  "Performance — decode parity".

- [ ] mlx-native-perf-hybrid-mlxbump - **Hybrid (Qwen3.6) decode ~12 → ~22 t/s.** The
  GatedDeltaNet kernel needs a blocking `eval` per layer (~48/token) to dodge a
  metal_kernel buffer-donation bug → the forward self-blocks, so it can't pipeline.
  Removing the eval OR `async_eval` both give garbage on our **MLX 0.30.6**; Python's
  kernel has NO eval and is correct on **MLX 0.31.2** → the bug is fixed upstream. Fix
  = bump `mlx-sys` 0.30.6 → 0.31.2 (needs the fft.cpp-exclude + ops.cpp patch from the
  earlier attempt), drop `gated_delta.rs:250`'s eval, then pipeline the hybrid like the
  dense path + flip its `pipeline=false` to true. Heavy: ~15-min MLX C++ rebuild, build
  patches, 27B byte-exact validation, reboot-risky (run on the small models where
  possible; the SPRINT RESUME CHECKPOINT + small-model discipline applies). High
  probability but unproven (the earlier 0.31.2 attempt was for the rope bug, never
  tested gated_delta).

- [ ] mlx-native-perf-compile - the top remaining decode lever (~2× potential): a
  capture-based plain-`compile`d decode step closing over the weights, taking only
  `(token, cache)` as args. **Prereq:** fixed-shape KV cache (preallocate + in-place
  slice-update; today's `ConcatKeyValueCache` grows by concat and forces a recompile
  each step). Correctness-critical (byte-exact) and intersects the GatedDeltaNet
  buffer-donation hazard (compile + in-place cache + buffer-donating kernel = where
  the token-2 divergence lived). Needs a clean machine for trustworthy A/B (current
  numbers ~30% degraded by session memory pressure). Do as a dedicated session. See
  SPRINT `mlx-native-perf` + the spec's mx.compile section.

### Native MLX runtime — catalog expansion (more architectures)

Each architecture port is now cheap: the AFQ-quant loader + the model-agnostic
sampler are shared (import from `qwen3.rs`), so a new dense model ≈ a copy of
`llama.rs`/`qwen2.rs` with the right attention/norm quirks + a `LoadedModel` arm
+ a byte-exact oracle sweep vs Python `mlx_lm`. (Quick near-free ones — Mistral
alias, Llama variants, fp16 — are in SPRINT.) Out-of-scope ones (DeepSeek/MLA,
vision) and why: `docs/specs/mlx-native-catalog-non-goals.md`.

- [ ] mlx-native-gemma - Gemma 2 / Gemma 3 (`model_type: "gemma2"`/`"gemma3"`). Own
  fork model file: RMSNorm with a **+1 weight convention**, **attention logit
  soft-cap** + final-logit soft-cap (Gemma2), embedding scaled by `sqrt(hidden)`,
  tied embeddings, GQA. Reuse the AFQ loader + shared sampler. Moderate (the soft-cap
  + norm convention are the only real deltas). Validate vs oracle.

- [ ] mlx-native-phi3 - Phi-3 / Phi-3.5 (`model_type: "phi3"`). Dense, close to Llama
  but with a **fused `qkv_proj`** and **fused `gate_up_proj`** (split after the matmul),
  partial RoPE on some variants. Own file (the fused projections need splitting at load
  or in forward). Validate vs oracle. (Phi-4 if its config differs.)

- [ ] mlx-native-mixtral - Mixtral / Mistral-MoE (`model_type: "mixtral"`). Sparse MoE
  on the Mistral block — reuse the `qwen3_moe` SwitchGLU routing pattern + the Mistral
  attention (from `mlx-native-mistral`). Bigger than the dense ports; do after Mistral
  + Gemma land. Validate vs oracle.

- [ ] mlx-native-recommend-catalog - As architectures land, curate `models::RECOMMENDED`
  (the launch picker / `rozum models` list) with a few good defaults per family
  (coder, small, mid) so users get a sensible menu, not just whatever they type.

### Native MLX runtime — domain fine-tuning (OFFLINE, exploratory)

All **offline** (train with `mlx_lm.lora`/`fuse`, serve the merged checkpoint — the
host stays inference-only). The full feasibility/memory/eval write-up is
`docs/specs/training-and-lora-exploration.md`. Reality check on size: QLoRA on
**0.5–4B is plenty for FORMAT / STYLE / narrow-domain PATTERNS** (the three items
below), but NOT for raw reasoning — that stays on a big/remote model. Step up to
7–14B (still QLoRA-able on a 32–64 GB Mac) only if a tune must also carry capability.
Every item is gated by a **held-out eval** (domain set + a general probe to catch
forgetting) — non-negotiable; without it you can't tell "improved" from "quietly
degraded".

- [ ] tune-toolcall-format - **Highest value/effort.** SFT/QLoRA a small model
  (0.5–1.5B) on correct `<tool_call>{…}</tool_call>` traces to raise tool-call
  format adherence (small models sometimes botch the JSON). Narrow, low-risk,
  trivially measurable (format-valid rate on a held-out set). Pure format learning —
  a tiny model is enough.

- [ ] tune-domain-coder - QLoRA `Qwen2.5-Coder-1.5B/7B` on this repo's conventions
  (FIM / signature+docstring→body / diff→commit-message) for fast, private, on-device
  **autocomplete + boilerplate** in our style. NOT a replacement for the agent model
  — it's the "small local handles the rote 80%, big/remote handles the hard 20%"
  tier (rozum's multi-backend routing already fits this). 1.5–4B for completion;
  7B if it should also carry a bit of domain reasoning.

- [ ] tune-room-agent-style - Light QLoRA for a consistent room-agent voice/format
  (tone, structure of replies, meeting etiquette). Style/persona is exactly what a
  small model picks up; 0.5–4B is enough.

- [ ] tune-minimal-experiment - **The one-day proof.** Offline QLoRA
  `mlx-community/Qwen2.5-Coder-1.5B-Instruct-4bit`: ~1–5k `(prompt, completion)` pairs
  from the repo (10% held out), rank 16, target `q/k/v/o + gate/up/down`, LR 1e-4,
  2 epochs, seq 2048, batch 1 + grad checkpointing → `mlx_lm.fuse` → `rozum launch
  --model <merged-dir>`. Fits in 16–32 GB, ~an afternoon. Eval: held-out
  exact-match/edit-distance + a small general probe. Decides yes/no on "helped my
  domain without breaking general use" before investing in the items above. Spec §6.

### Portability / hardware-agnostic core (keep the durable layer durable)

The hardware-agnostic abstraction already exists — the `ChatBackend` SPI and
everything above it (gateway, rooms, launch, orchestration, model infra). MLX is
one swappable leaf; GGUF/llama.cpp already carries non-Mac (Linux/Windows, CUDA/
ROCm/Vulkan/CPU). Full write-up: `docs/specs/portability-and-the-backend-spi.md`.
These items turn "portable in principle" into "portable by `cargo build`".

- [ ] portability-platform-features - Make the default build platform-aware. Today
  `default = ["mlx-native", "gguf"]` assumes macOS; `cargo build` on Linux tries to
  compile MLX (Apple toolchain) and fails. Default the MLX leaf on macOS only (e.g.
  `[target.'cfg(target_os = "macos")']` deps + a no-op feature elsewhere, or a
  documented build matrix), and make a clean cross-platform default (gguf + a
  CUDA/Vulkan passthrough feature on `llama-cpp-2`, which currently hard-codes
  `["metal"]`). Goal: "not a Mac" is a first-class `cargo build`, not a flag dance.

- [ ] portability-shared-model-source - Lift the backend-agnostic model infra above
  the SPI. Auto-download + hf_hub/ModelScope cache (`src/hf_hub.rs`,
  `src/modelscope.rs`) + spec resolution + the RAM preflight are hardware-agnostic
  and useful to ANY safetensors backend (mistralrs, a future runtime), but are wired
  through the MLX path today (`ensure_model_dir` lives in `mlx_native_backend`).
  Factor a `model_source` layer so a new leaf reuses fetching/cache/preflight for
  free instead of re-implementing them.

- [ ] portability-new-backend-checklist - Write the "add a new runtime/hardware
  backend" recipe down (implement the 5 `ChatBackend` methods; bring your own chat
  template + tokenizer + cache; slot into `build_gateway_backend` + the config
  chain). Turns the seam from folklore into a checklist; complements the portability
  spec.

- [ ] portability-cuda-gguf - Concrete non-Mac GPU path: expose `gguf-cuda` /
  `gguf-vulkan` features that pass the matching `llama-cpp-2` backend feature
  through, so a Linux/CUDA user gets GPU GGUF inference without editing Cargo.toml.
  (Cheapest real "runs on someone else's non-Mac hardware" deliverable.)

#### Extractions — pull leaf-bound work into modules keyed by their *true* dependency

The taxonomy + rationale is in `docs/specs/portability-and-the-backend-spi.md`
("Taxonomy by dependency" / "What to extract"). Each item below pulls something out
of the MLX leaf into a module that depends only on hardware, or only on the model,
or on nothing — so any engine can reuse it.

- [ ] extract-shared-serving-helpers - **L1.** Lift the engine-agnostic per-request
  logic into a `serving` module every leaf calls, instead of re-implementing it.
  First target: `parse_tool_calls` is **already duplicated** (own copy in `gguf.rs`
  AND `mlx_native_backend.rs`) — unify it. Then tool-history rendering
  (`message_text`), UTF-8-safe incremental detokenize, multi-EOS stop logic, and the
  KV/RAM preflight (pure arithmetic from `config.json` + free RAM). Depends only on
  the model's text/config conventions, not the engine.

- [ ] extract-shared-sampler - **L2.** Define the sampler (top-p / top-k /
  repeat-penalty / seed / categorical) over a plain logit slice (`&[f32]`) + RNG, in
  a shared module. Each leaf materializes the final-position logits and calls it; the
  per-token GPU→CPU copy of one vocab vector is negligible for our op-launch-bound
  decode. Removes the per-leaf sampler duplication (today it lives inside the MLX
  fork, bound to mlx `Array`).

- [ ] extract-model-reference-specs - **L3.** Capture the model *knowledge* as
  engine-independent reference docs (one per family): the forward math + the
  checkpoint conventions we reverse-engineered — RMSNorm +1, AFQ
  `.weight↔.inner.weight` / `.bias↔.inner.bias` remap pattern, f32 delta-scan,
  Qwen2 bias / optional `head_dim`, multimodal `text_config` unwrap, safetensors-index
  sharding. The code stays per-tensor-lib; the spec lets a new leaf implement from
  fact instead of re-deriving from a checkpoint (this is where the real time went).

- [ ] extract-metal-kernels - **L4.** Factor the GatedDeltaNet fused-scan (and future)
  Metal kernels' MSL source into a standalone hardware-only module, so any Metal
  engine (mlx, a candle-metal path, mistralrs-metal) binds the same `.metal` instead
  of re-deriving it. Depends only on Metal + the architecture, not the engine's logic.

- [ ] extract-l5-track-upstream - **L5 (no extraction — discipline only).** Engine
  -binding fixes (RoPE reshape, zero-buffer, buffer-donation/`eval`, `mx.compile`
  finding, the `metal_kernel` mlx-c binding) are irreducibly engine-specific. Keep
  pushing them upstream so the *ecosystem* carries them (done: 4 mistralrs PRs + the
  mlx-rs fork fixes); this item is just the standing reminder to upstream, not vendor.

### Agent integration (busi) — DISTRIBUTED-FIRST

**busi is the agent; rozum is a stateless model service it calls over HTTP.** The
orchestration/session state lives in busi (so rozum scales + fails over for free);
the agent loop + the generic plumbing live in a **scalascript "agent SDK"** (generic,
reusable by any app), and the accounting tools/prompts/eval are busi on top. Design +
the three contracts (model-call API / agent loop / tool) + the generic-vs-domain
layering: `docs/specs/integration.md`. The rozum items here are
just the model-service side; the SDK + tools are owned by the scalascript/busi side.

- [ ] rozum-gateway-tool-contract - **P0b (rozum).** Stabilize + document the
  Contract-1 surface the SDK targets: `/v1/chat/completions` (+ `/v1/messages`) with
  `tools` (JSON-Schema), `tool_choice`, `temperature`, `stream`; response `tool_calls`
  (id/name/arguments) vs text + `finish_reason`; SSE tool-call argument deltas. Mostly
  exists (tool-use + multi-turn history + SSE) — harden it as a stable contract +
  conformance tests so the scalascript SDK can build against it confidently.

- [ ] rozum-distributed-readiness - **P0b/P1 (rozum).** The gateway as a deployable,
  horizontally-scalable, stateless service: health/readiness endpoints, clean
  load-balancing (any instance serves any request), a model pool/router, graceful
  drain. Partly exists (shared-gateway daemon, `concurrency::admit_wrap`, the launch
  proxy's replay/retry) — consolidate into a documented "run rozum as a service" path.

- [ ] rozum-agent-runtime - **P0b (rozum, optional, DUAL-PURPOSE).** A Rust reference
  implementation of the agent loop (Contracts 2–3): `(backend, system, user,
  tool_source, budget)` → model call → `tool_use` → execute via tool source → feed
  result → repeat. Serves two purposes: (a) the in-process **embedded mode** (small
  model, no network), and (b) the **executable spec** the scalascript SDK mirrors.
  `ToolSource` trait + adapters: MCP-client (reuse `rmcp`) and direct callback.

- [ ] rozum-embed-crate - **P2 (rozum, optional).** Stable minimal public crate
  (`rozum-embed`) for the in-process embedded mode (Rust busi component + small model):
  build a backend, run the reference agent-runtime, pick a tool source. Not the primary
  path (distributed HTTP is) — the small-model optimization.

- [ ] structured-output-for-tools - **P2 (rozum).** Constrained / structured decoding
  that enforces the model's tool-argument output against the app's JSON tool schemas
  during decoding → tool-arg reliability for small local models (can't emit an invalid
  arg). Supersedes the older Runtime/UX `structured-output`; now driven by a concrete
  consumer. Native MLX sampler-level work + a schema→constraint compiler. Exposed over
  Contract-1 so the SDK just passes schemas.

- [ ] busi-eval-and-tune - **P1→P3 (busi-side; rozum hooks only).** busi/scalascript
  build the eval harness (20–50 real flows + task-success metric) to pick the smallest
  model that clears the bar; then QLoRA a small model on collected `(prompt →
  tool-call)` traces (offline; see `tune-toolcall-format`) → a fast, private,
  on-device busi model. rozum side: serve the merged checkpoint (already works) +
  decode determinism (`temperature:0`) for reproducible eval.

  NOTE: the **generic scalascript agent SDK** (model HTTP/SSE client, agent loop, tool
  framework, schema derivation, endpoint pool/retry — the "build once, reuse in any
  app" layer) is owned by the scalascript/busi side, not rozum — full design + public
  API in `docs/specs/agent-sdk.md`. rozum provides the gateway contract +
  the optional Rust reference runtime as its executable twin.

### Native MLX runtime — backend feature parity (vs mistralrs)

Audit 2026-06-11 (`docs/specs/mlx-native-runtime.md` "Backend feature parity"):
features the mistralrs backend shipped that the native backend does NOT yet have.

- [x] mlx-native-cancel-prefill - DONE (fork `fb263995` + rozum `b022dc4`). The
  hybrid `Generate` polls a `should_cancel` predicate between prefill chunks
  (`prefill_cancellable` -> `Ok(None)`); rozum wires it to `job.cancel`, so a
  cancel/disconnect on a long prompt is honored DURING prefill, closing the
  native-side analog of the mistralrs large-prompt stall. Test
  `mlx_qwen35_prefill_cancels_mid_prefill`.

- [x] mlx-native-sampling - DONE: top_p/top_k/seed (fork `f36c8c3a` + rozum
  `510c760`) AND repeat_penalty (fork `e970b23a` + rozum `3597abe`). `sample_with`
  ported from mlx_lm, threaded through all Generate; greedy stays argmax
  (byte-exact). repeat_penalty applies over a 256-token window (take/put_along_axis,
  O(window)); Generate keeps a token history only when penalty != 1.0. Unit test
  pins top_k=1/tiny-top_p == argmax + that a hard penalty moves the argmax.

- [x] mlx-native-tool-use - DONE (fork `1fc66029`/`e316dbf7` + rozum `09dfbcc`).
  `mlx-lm-utils` `ApplyChatTemplateArgs` gained a `tools` field -> minijinja context
  (+ enabled minijinja `json` feature for the `tojson` filter). Rozum: `Job` carries
  `req.tools`; `render_prompt` builds OpenAI-style schemas; `stream_generation`
  suppresses `<tool_call>` from text and parses it into `ToolUse*` events +
  `stop_reason=ToolUse`. E2E `mlx_tool_use_weather` (get_weather call) + unit
  `parse_tool_calls_extracts`.

- [x] mlx-native-tool-history - DONE (rozum-only, pin unchanged). `message_text`
  renders assistant `ToolUse` blocks back as `<tool_call>` markup (inverse of
  `parse_tool_calls`) instead of dropping them, so multi-turn tool loops carry the
  prior call in history. Unit `tool_use_round_trips_into_history`.

- [x] mlx-native-multi-eos - DONE (rozum `b022dc4`). `read_config` collects the full
  `eos_token_id` set; `stream_generation` stops on any (Qwen3: `<|im_end|>` 151645 +
  `<|endoftext|>` 151643).

- [ ] gguf-tool-use-non-qwen - Extend GgufBackend tool-use parser to Llama-3.1 and Mistral chat-template formats.

- [ ] ui-streaming-ws-tui - Propagate `ChatEvent` stream to web WebSocket and TUI for partial token rendering.

- [ ] openai-http-client-backend - `ChatBackend` that calls the OpenAI Chat Completions API (client side).
  - Shares SSE parsing logic with the gateway server side.
  - Useful as a fallback when no local model is available.

- [ ] anthropic-http-client-backend - `ChatBackend` that calls the Anthropic Messages API (client side).
  - Shares SSE parsing logic with the gateway server side.
  - Complements / supersedes the `remote-api-backends` sprint task (which predates the new SPI).

## Runtime And UX

- [x] gateway-openai-responses-api — **DONE.** `POST /v1/responses` (the OpenAI Responses API)
  so the **Codex CLI** (≥ 0.137, which dropped `wire_api="chat"`) can use the gateway.
  `responses_handler` parses the Responses request (`instructions` → system; `input` items —
  messages / `function_call` / `function_call_output`; flat `tools`; `max_output_tokens`) into
  the internal `ChatBackend`, and streams back the typed Responses event protocol
  (`response.created` → `output_item.added`/`content_part.added` → `output_text.delta` →
  `output_text.done`/`content_part.done`/`output_item.done` → `function_call` items
  (`arguments.delta`/`.done`) → `response.completed`, each event with `type` +
  `sequence_number`); non-stream returns the final `response` object with `output[]` + `usage`.
  Reuses the same backend stream as `/v1/chat/completions` (our event order — text then whole
  tool calls then Done — maps onto a message item + function_call items). Stateless (Codex
  sends the full `input` each turn). Tests: input/tool conversion, response-object shape, SSE
  smoke. The e2e Codex runner (`scripts/e2e_codex_gateway.sh`) now connects via
  `wire_api="responses"` (Codex ignores `OPENAI_BASE_URL`, so it sets `-c model_provider`).

- [x] mlx-native-prefix-kv-cache — **DONE for dense arches.** Reuse KV across agentic turns:
  the cap-1 worker now persists the previous request's prompt ids + KV (`PrefixCache`), and
  when the next prompt strictly extends it (the append-only agentic-loop case) it truncates the
  cache to the shared prefix and prefills only the **new suffix** instead of re-prefilling the
  whole growing conversation. Byte-exact: the kept `[0,reuse)` KV is exactly what a fresh
  prefill computes, and `create_attention_mask` builds the causal mask from the cache offset
  (integration test `mlx_prefix_reuse_byte_exact` asserts reuse output == fresh prefill). New
  fork method `ConcatKeyValueCache::truncate` (mlx-rs fork rev `c8517814`). `ROZUM_PREFIX_CACHE=0`
  disables. Dense only — Qwen3 / Qwen3-MoE / Llama / Qwen2 (they own the KV cache externally).
  **Follow-up below for hybrid (Qwen3.6).**

- [x] mlx-native-prefix-kv-cache-hybrid — **DONE.** Prefix reuse for the **hybrid** Qwen3.6
  arches (Qwen35 + Qwen35Moe). The `Full(KV)` layers truncate to the shared prefix like dense;
  the `Linear` GatedDeltaNet layers carry a recurrent state that can't be truncated, so it's
  **deep-snapshotted** (`Array::deep_clone` → own buffer, survives decode buffer donation) at the
  **end of prefill** (offset == prompt len) and restored on the next reuse. Fork (rev
  `fd284599`): `LayerCache::{truncate, snapshot→LinearSnap, restore}`, `Generate::with_cache`
  (start from a pre-populated cache + suffix) snapshotting right after the prefill step, and
  `into_cache_and_snapshot()`. rozum: `stream_generation` returns the iterator so the hybrid arms
  reclaim cache+snapshot; the worker persists `HybridPrefix{ids, cache, snap}` and on reuse
  truncates Full + restores Linear + prefills only the suffix. **Byte-exact** vs a fresh prefill
  (integration test `mlx_prefix_reuse_byte_exact_hybrid` on the deterministic Qwen3.6-27B; the
  35B-A3B MoE shares the exact reuse logic). `ROZUM_PREFIX_CACHE=0` disables.

- [x] mlx-native-runaway-stop — **DONE.** Bound a single runaway generation so one greedy loop
  can't pin the cap-1 worker for minutes (the e2e `test` task hit a 600 s hang, `result=None`).
  Two guards in the backend: (a) `DEFAULT_OUTPUT_CEILING=8192` clamps the effective `max_tokens`
  regardless of the client value (`ROZUM_MAX_OUTPUT_TOKENS` overrides; 0 disables) — a backstop;
  (b) `is_runaway_loop` in `stream_generation` stops when the last 64 generated tokens are
  exactly periodic with period ≤16 (a short block repeated ≥4×) — the principled fix, catches a
  greedy loop in ~64 tokens with no false positives on real text (`ROZUM_REPEAT_GUARD=0`
  disables). Unit test `runaway_loop_detection`. `--max-turns` does NOT help (it bounds the
  agentic loop, not one generation's tokens) — this does.

- [x] rozum-native-channels-tier3 - DONE (`feature/piggyback-wakeup`). Tier-3
  gateway piggyback wakeup, keyed by project + agent name. mcp-proxy drops each
  room transcript delta to `$XDG_RUNTIME_DIR/rozum/piggyback/<project>/<agent>.log`;
  the launch-local HTTP proxy drains it into the next chat request as an
  out-of-band system note (Anthropic `system` / OpenAI `system` message; tool JSON
  + SSE untouched). Fallback rung: auto-off when Tier-1 channels are active, on
  otherwise; `--no-piggyback` forces off, `ROZUM_PIGGYBACK=1` forces on. New
  `src/meeting/piggyback.rs` +
  hooks in `src/meeting/proxy.rs` (writer) and `src/proxy.rs` (reader). Reaches
  agents that take neither Tier-1 channels nor a Tier-2 `wait_my_turn` loop. Spec:
  `docs/specs/rozum-native-channels.md`.

- [ ] streaming-output - Stream model output token by token.
  - Add CLI support without breaking non-streaming evals.

- [ ] structured-output - Add JSON/schema-constrained output validation.
  - Required for reliable tool routing.
  - Start with parse/repair/retry before grammar decoding.

- [ ] tool-routing - Add a small tool registry and let the model select simple tools.
  - First tools: echo, time, file lookup, model catalog.

- [ ] memory-store - Add local memory storage.
  - Start with append-only facts and retrieval by exact key.

- [ ] rag-lite - Add a local retrieval layer.
  - Keep embeddings/backend choice configurable.
  - Start with small text documents and lexical fallback.

### Concurrency & scheduling (follow-ups to `mistralrs-concurrency-scheduling`)

Stretch items deliberately out of scope of the initial A→B+C→D delivery. See
`docs/specs/mistralrs-concurrency-scheduling.md` (Out of scope).

- [ ] concurrency-engine-yield - Make the fork yield between prefill chunks so a
  long prefill does not monopolise an engine step. Today chunking is internal to
  `pipeline::step` (commit `698bccf1f`) — memory-bounded but not preemptible — so
  the Phase B+C fast lane only reorders *admission*, not in-flight progress.
  Moving the chunk loop up to the scheduler (re-queue the seq as a running prompt
  after each chunk) would let an admitted fast request interleave with a big
  prefill. Upstreamable into `mistralrs-chunked-prefill`.

- [ ] concurrency-preemption - Preempt/swap-out a running sequence to admit a
  higher-priority one (vLLM-style). Needs mistralrs engine support it does not
  currently expose — revisit if SJF + fast lane prove insufficient for tail latency.

- [ ] concurrency-cost-tokenizer - Tokenizer-accurate `RequestCost` instead of the
  char/word heuristic, if class boundaries (interactive vs bulk) turn out fuzzy.

- [ ] concurrency-multi-instance - Size-class routing across more than one loaded
  model (e.g. a small fast model lane + a big model lane), with a shared memory
  budget. Heavy; only if a single-model fast lane is not enough.

- [ ] concurrency-cross-process - Coordinate the concurrency budget across several
  `rozum` processes sharing one GPU (e.g. a host-wide semaphore), instead of each
  process budgeting in isolation.

- [ ] concurrency-observability - Expose queue depth, admission limit, fast-lane
  hits, and shed/429 counts via `obs` so the scheduler is tunable from data.

- [ ] shared-gateway-multislot - Allow more than one resident model behind the
  shared gateway when memory permits, gating a second model on `ConcurrencyBudget`
  (Phase A) saying both fit. Keys the registry/port by model. Follow-up to
  `shared-gateway` (which keeps a single resident model). See
  `docs/specs/shared-gateway.md` (Out of scope).

- [ ] shared-gateway-service - Optionally install the shared gateway as a
  launchd/systemd service for always-warm startup, instead of lazy spawn +
  idle-exit. Follow-up to `shared-gateway`.

## Model Quality

- [ ] model-catalog-refresh - Expand and verify tiny model catalog.
  - Include current small Qwen/Gemma/Phi candidates with exact file sizes.
  - Record license and expected strengths.

- [ ] benchmark-baseline - Record latency, disk size, and smoke eval score for each backend/model pair.
  - Use the eval harness once available.

- [ ] prompt-policy - Define system prompts and safety/style constraints per model.
  - Keep raw mode available for debugging.

- [ ] distillation-plan - Design a later LoRA/QLoRA or distillation path.
  - Do not implement until evals provide a baseline.

## Project Hygiene

- [ ] commit-initial-project - Commit the current initial project state once the user is ready.
  - Include submodule, specs, Rust project, tiny model scripts, and backend abstraction.
  - Do not commit `.tools/`, `target/`, or `models/*.gguf`.

- [ ] ci-smoke - Add a lightweight CI path.
  - Run fmt/test/build without requiring model downloads.
  - Keep real model smoke tests opt-in.

- [ ] docs-bootstrap - Add a concise setup guide.
  - Include clone, submodule init, build, first room, and MCP proxy setup.
