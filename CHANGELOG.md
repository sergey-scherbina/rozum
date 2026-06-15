# Changelog

## agent — reference agent runtime (Contracts 2–3): the tool loop, in Rust
Completed: 2026-06-15
A Rust reference implementation of the agentic loop (`rozum-agent-runtime`, P0b), in `src/agent.rs`.
Dual purpose: it powers the in-process embedded mode (small local model, no network) and is the
executable spec the scalascript agent SDK mirrors. It speaks only the `ChatBackend` SPI, so it runs
against any backend (native MLX, GGUF, a remote HTTP client backend). Completes the P0b contract
trio (Contract 1 gateway ✓, the tool contract ✓, now the agent loop).
- **Contract 3 — Tool** (`ToolSource` trait): `fn tools() -> Vec<ToolDef>` (the schemas advertised to
  the model) + `async fn dispatch(name, args) -> Result<Value, ToolError>`. `CallbackToolSource` is
  the direct in-process adapter — register `(ToolDef, handler)` pairs; a `ToolError` is a recoverable
  message handed back to the model as the tool result so it can self-correct.
- **Contract 2 — the loop** (`run_agent(backend, system, user, tools, budget) -> AgentOutcome`):
  `[system, user] → model → (tool calls → dispatch → append results)* → final text`, bounded by
  `Budget {max_steps, max_tokens, wall_time, temperature}` (temperature 0 by default for reproducible
  runs). `AgentOutcome` carries `{text, stop, steps, operations, transcript}` — the full audit trail
  of executed side effects + the conversation. `AgentStop ∈ {Done, BudgetSteps, BudgetTime, Error}`.
- **Validated model-free** (a scripted `MockBackend`): the full tool loop with the result fed back on
  the next step, the budget capping a runaway tool-calling loop, and recovery from both an
  unknown-tool call and a handler validation error (the `ToolError` reaches the model). **And e2e
  against native MLX** (`agent_loop_real_backend`, Qwen3-4B): the model calls `add(3,5)`, gets
  `{sum:8}`, and answers "The result of 3 + 5 is 8." — with `ROZUM_MLX_CONSTRAIN` guaranteeing the
  args are valid. 154/0.
- **Follow-up**: an MCP-client `ToolSource` adapter (over `rmcp`) so the runtime can use tools from an
  external MCP server (the trait is ready), and the `rozum-embed` public crate.

## gateway — distributed readiness: /health + /ready + graceful shutdown (run rozum as a service)
Completed: 2026-06-15
Makes the gateway safe to run as N identical instances behind a load balancer with zero-downtime
rolling deploys (`rozum-distributed-readiness`, P0b/P1). Spec: `docs/specs/distributed-readiness.md`.
- **`GET /health`** (liveness — 200 while the process serves HTTP, never touches the model) and
  **`GET /ready`** (readiness — 200 when servable, 503 while draining; body `{ready, loaded,
  shutting_down, model}`). The split is the standard one: health → restart decisions, ready → routing
  decisions. A transient model **swap**-drain (`/control/switch`) does NOT flip readiness — those
  requests park for the brief swap and still succeed, so the instance stays in rotation; only a
  shutdown (or an unloaded `--dedicated` model that can't rebuild) reads not-ready.
- **Graceful shutdown** on SIGTERM/SIGINT via `axum::serve(...).with_graceful_shutdown(...)`: flip the
  instance to not-ready and reject new chats (`enter()` returns 503 `shutting_down` instead of
  parking), wait `ROZUM_SHUTDOWN_GRACE_SECS` (default 3) so the LB deregisters, then axum stops
  accepting and drains the in-flight streams to completion before exit. Rolling deploys bleed an old
  instance out cleanly while new ones absorb traffic.
- **Stateless** is now documented as a property: the prefix-KV cache is a per-instance latency
  optimization, not session affinity — any instance serves any request, so no sticky sessions are
  needed (round-robin / least-connections is fine).
- Tests: `readiness_reflects_servability`, `shutdown_flips_readiness`,
  `enter_rejects_new_chats_while_shutting_down` (no leaked `generating` token). 149/0. Follow-ups
  (noted): a multi-model pool/router and cross-instance admission coordination.

## gateway — tool contract (Contract-1) hardened + documented; `tool_choice` honored
Completed: 2026-06-15
The HTTP tool surface the scalascript agent SDK builds against (`rozum-gateway-tool-contract`, P0b) is
now a stable, documented, conformance-tested contract. The tool-use machinery mostly existed
(`tools` → `tool_calls`/`finish_reason`/SSE deltas across `/v1/chat/completions`, `/v1/messages`,
`/v1/responses`); the one real gap was `tool_choice` — parsed nowhere, silently ignored.
- **`tool_choice` now parsed + honored** on all three routes, normalized across dialects into
  `ToolChoice::{Auto, None, Required, Named}` (OpenAI string/object, Responses flat `{type,name}`,
  Anthropic `{type: auto|any|none|tool}`). Honored by transforming the tool set the backend sees — no
  SPI change: `none` empties the tools (text-only), `named` restricts to that one tool, `auto` passes
  through. `required` is accepted but best-effort (the model isn't *forced* to start a call) and
  documented as such rather than silently dropped.
- **Documented** as a stable contract in `docs/specs/api-gateway.md` — a Tool-use/Contract-1 section
  with the `tool_choice` cross-dialect table, the non-streaming + streaming response shapes, the
  `finish_reason`/`stop_reason` mapping, and the `ROZUM_MLX_CONSTRAIN` arg-reliability note (the
  constrained decoding is transparent to the contract — the SDK just gets conformant `arguments`).
- **Conformance tests** (model-free, mock streams): `tool_choice` parsing per dialect +
  `apply_tool_choice` semantics, and the actual tool-call response JSON for both dialects
  (`oai_collect_tool_call_shape`: `tool_calls[].{id,type,function}` + `finish_reason:"tool_calls"`;
  `anthropic_collect_tool_use_shape`: `tool_use` block + `stop_reason:"tool_use"`). 146/0.

## mlx-native — constrained tool decoding reaches Qwen3.6 (hybrid path + XML tool format)
Completed: 2026-06-15
The constrained-decoding v1 only covered dense arches and the JSON Hermes tool format — so it didn't
actually help the user's primary model: Qwen3.6 is a **hybrid** (GatedDeltaNet) arch AND it emits tool
calls as **XML**, not JSON. Both gaps are now closed.
- **Hybrid path**: extracted the masked decode into a generic `constrained_decode_loop<C>` and added
  `run_constrained_hybrid` over the heterogeneous `LayerCache` (mirror of the dense
  `run_constrained_dense`). `should_constrain` now routes both dense and hybrid arches.
- **XML tool format**: Qwen3.6 emits `<function=NAME><parameter=KEY>\nVALUE\n</parameter>…</function>`
  rather than `{"name":…,"arguments":…}`. Added an XML prefix-acceptor (`xml_prefix`) and a unified
  `Constraint::{Json, Xml}` enum; the decode loop picks the format from the first body char after
  `<tool_call>` (`{` → JSON, `<` → XML) and constrains accordingly — `NAME` ∈ tool names, `KEY` ∈ the
  tool's properties (no dupes, all required before `</function>`), `enum` `VALUE`s restricted to their
  literals. 3 new model-free unit tests.
- **Validated on the real model** (`mlx_constrained_tool_call_hybrid`, Qwen3.6-35B-A3B): the prompt
  asks for "celsius" but the schema enum is `["kelvin","rankine"]` → output
  `{"location":"Paris","unit":"kelvin"}`, i.e. the mask bit on the hybrid XML path exactly as on the
  dense JSON path. The discovery that surfaced this (the dense JSON e2e passed but the hybrid one came
  back `unit:"celsius"`) is itself why each addition is run, not assumed. 141/0.

## mlx-native — constrained tool-argument decoding (small models can't emit an invalid tool call)
Completed: 2026-06-15
Tool-use was post-hoc: render `tools` into the prompt, generate freely, then parse
`<tool_call>{json}</tool_call>` after the fact — so a small model could emit malformed JSON, a
hallucinated key, a wrong type, or a missing required arg, and the parse would fail or yield garbage.
Now, behind `ROZUM_MLX_CONSTRAIN`, the sampler is **masked to the tool's JSON schema during decode**,
so the arguments object physically cannot violate it. v1 of `structured-output-for-tools`; spec at
`docs/specs/constrained-tool-decoding.md`.

- **Engine** (`src/constrain.rs`, pure Rust, no MLX): a JSON-Schema subset compiles to an incremental
  **prefix acceptor** — `Schema::prefix(s)` returns Complete / Partial / Invalid for the partial JSON
  so far. Subset: object (properties + required, keys restricted to declared props), string (+`enum`/
  `const`), integer, number, boolean, array-of-scalar, nested object; anything it can't model relaxes
  to generic well-formed JSON (it never over-rejects). It's stateless — re-validates the whole suffix
  each step (args are short), which also lets the caller swap the schema mid-stream for free. 6
  model-free unit tests cover required keys, enums, types, completion, and the relax path.
- **Sampler mask** (`mlx_native_backend.rs`): a B=1 dense decode loop (`run_constrained_dense`) that,
  once the model opens a `<tool_call>{`, keeps only the top-K candidate tokens whose decoded piece
  leaves the JSON a valid prefix (widen 256→4096→full vocab, argmax fallback), forbids the rest (−∞),
  then runs the existing `sample_with` among the allowed (temp/top-p/top-k/penalty still apply). The
  Hermes envelope `{"name": <enum tool names>, "arguments": <schema>}` is enforced; `arguments`
  resolves to the chosen tool's schema as soon as the `name` literal is read; the constraint releases
  when the object closes. Covers every dense arch (Qwen3/Qwen2/Llama/Gemma 3) via `dense_forward`.
- **OFF by default** → the free-decode + post-hoc-parse path is byte-identical; constrained jobs are
  also kept out of the batched path (they need the B=1 masked loop).
- **Validated** with a discriminating e2e (`mlx_constrained_tool_call_conforms`, Qwen3-4B): the prompt
  asks for "celsius" but the schema's `unit` enum is `["kelvin","rankine"]` — the output is
  `{"location":"Paris","unit":"kelvin"}`, i.e. the mask redirected the model off its *preferred but
  invalid* token onto a legal enum literal. Proves the constraint actually bites, not that the model
  happened to comply. 138/0.

Follow-ups (BACKLOG): hybrid Qwen3.6 constrained decode (v1 is dense; hybrid falls back to post-hoc),
full JSON-Schema (`oneOf`/`$ref`/patterns), and a general `response_format: json_schema` request field
reusing the same engine.

## mlx-native — the bigger Gemma 3 sizes load (4B/12B/27B multimodal wrapper) + catalog mid-tier
Completed: 2026-06-15
Only the tiny text-only Gemma 3 1B (`model_type: "gemma3_text"`) loaded before; the genuinely useful
4B/12B/27B ship as the **multimodal wrapper** (`model_type: "gemma3"`) and failed at load. The 4B was
added to the catalog, validation caught the failure (exactly why each addition is run, not assumed),
and the loader was fixed — four general changes in `gemma3.rs`, all validated end-to-end on
gemma-3-4b-it-4bit (answers correctly, `mlx_gemma3_wrapper_chat`):
- **Config nesting:** the text model lives under `text_config` (with `quantization` at the top level,
  grafted on). The wrapper omits most head fields, so `ModelArgs` now carries serde defaults matching
  HF `Gemma3TextConfig` (heads 8, kv 4, head_dim 256, sliding_window_pattern 6, query_pre_attn_scalar
  256, rope_theta 1e6, rope_local 1e4, vocab 262208) — verified to reconstruct 4B (heads→8), 12B
  (head_dim→256) and 27B exactly. Model-free unit test `wrapper_text_config_fills_gemma3_defaults`.
- **Weight prefix:** strip `language_model.` and skip `vision_tower.*` / `multi_modal_projector.*` so
  the text params line up; no materialized lm_head → tied embeddings (already handled).
- **RoPE scaling:** the wrapper sets linear scaling `{factor 8}`, which Gemma 3 applies to the GLOBAL
  layers only (local sliding-window layers stay unscaled). Threaded through `Attention::new` /
  `DecoderLayer::new`.
- **Stale index (general robustness):** some mlx-community uploads ship a `model.safetensors.index.json`
  that names sharded files (`model-0000N-of-…`) after the weights were consolidated into a single
  `model.safetensors`. Trust the index only when every shard it names exists; otherwise load every
  `*.safetensors` actually present. Fixes the load for any repo with a stale index, not just Gemma.

Catalog (`mlx-native-recommend-catalog`): with all the architectures now landed, `models::RECOMMENDED`
gained mid-tier entries so the picker has coder/general at a fits-16GB size, not just tiny + heavy —
**Qwen2.5-Coder 7B** (mid coder, same family as the 32B) and **Gemma 3 4B** (mid general). Both
validated by actually loading + answering. Fork rev `8c18fd23`.

## mlx-native — Gemma 3 batches too → EVERY dense family now serves concurrently
Completed: 2026-06-15
The last serial dense arch joins the batched path: two concurrent Gemma 3 sessions now share one
forward instead of queueing. Gemma 3 was the hold-out because its LOCAL layers attend only to the
last `sliding_window` (512) keys, and that per-layer windowed mask had to be threaded through the
batched decode (the per-row RoPE port — `BATCH_PAD_OFFSETS` + `set_batch_pad_offsets` in `gemma3.rs`
— is the same mechanism as Llama/Qwen2). The window plumbing is cheap because of the cache geometry:
at decode every row is right-aligned in the left-padded KV cache, so "keep the last `window` keys"
is a single uniform key-axis mask (`build_window_keep`: `kpos ≥ total − window`) AND-ed with the
per-row pad mask — no per-row window math. `dense_forward` gained a `Gemma3` arm, `is_batchable_arch`
includes it, and `run_batch` sets `gemma3::set_batch_pad_offsets` alongside the others (each model
reads only its own thread-local — harmless no-ops). OFF by default → B=1 serial path byte-identical
(`mlx_gemma3_chat` unchanged; windowing math still covered by `sliding_window_mask_bands_local_attention`).
Validated end-to-end (`mlx_gemma3_batched_two_concurrent`, cached gemma-3-1b-it-4bit): two concurrent
requests land in ONE `run_batch` call with distinct correct answers (`Paris` / `Tokyo`). **So now
EVERY dense family batches** — Qwen3 / Qwen3-MoE, Qwen2/2.5, Llama / Mistral / Phi-3 / SmolLM, and
Gemma 3 — plus the Qwen3.6 hybrid via its own `run_batch_hybrid`. No dense arch is left serial.
131/0. Fork rev `06fd0421`.

## mlx-native — the Llama family batches too (Mistral / Phi-3 / SmolLM / Llama-3.x)
Completed: 2026-06-15
Batched decode used to be Qwen-only — every dense Llama-family model (Llama 3.x, Mistral, Phi-3,
SmolLM, all of which load into `LoadedModel::Llama`) ran strictly serially, so concurrent sessions on
them queued instead of sharing a forward. Now they batch: ported qwen3's per-row-RoPE mechanism into
`llama.rs` (a `BATCH_PAD_OFFSETS` thread-local + `set_batch_pad_offsets`; `Attention` ropes q/k at
`cache.offset() − pad_i` per row via `forward_dynamic` when set, else the normal scalar offset). The
key-pad mask was already threaded through `AttentionInput.mask`, so it needed no change. `dense_forward`
gained a `Llama` arm and `is_batchable_arch` includes it; `run_batch` sets both arches' thread-locals
(only the loaded model reads its own — the extra setter is a harmless no-op). OFF by default → the B=1
serial path is byte-identical (`mlx_llama_chat` unchanged). Validated end-to-end
(`mlx_llama_batched_two_concurrent`, Llama-3.2-1B): two concurrent requests land in ONE `run_batch`
call with distinct correct answers (`Paris` / `Tokyo`). **Qwen2 / Qwen2.5 / Qwen2.5-Coder got the same
treatment** (identical per-row-RoPE port in `qwen2.rs`; `mlx_qwen2_batched_two_concurrent` on a cached
Qwen2.5-0.5B). So EVERY dense family now serves concurrent sessions in parallel with continuous
batching + per-row sampling — Qwen3 / Qwen3-MoE, Qwen2/2.5, Llama, Mistral, Phi-3, SmolLM (plus the
Qwen3.6 hybrid via its own path). 131/0. Fork rev `341ebb2c`. (Only Gemma 3 — its per-layer
local/global windowed masks need threading into the batched path — remains serial; a follow-up.)

## mlx-native — Gemma 3 sliding-window attention (local layers window correctly at long context)
Completed: 2026-06-15
Finishes the one deferred Gemma 3 gap. Its LOCAL (non-global) layers are supposed to attend only to
the last `sliding_window` (512) keys, but the initial port approximated them with full attention —
exact for short prompts, but diverging on long contexts (which coding agents hit). Now each layer
gets the right additive mask: GLOBAL layers stay full causal, LOCAL layers additionally drop keys
older than the window. Both masks are built over ABSOLUTE positions (`build_gemma_masks`) so they're
correct at decode (`offset > 0`), and they coincide whenever the whole context fits the window — so
short prompts are byte-unchanged (`mlx_gemma3_chat` still clean). A deterministic unit test proves
the local mask bands the causal mask to the window (prefill) and windows correctly at decode (no
model needed). The mask keeps the full KV cache, so memory is still O(context) — a bounded windowed
KV cache is a later optimization, not a correctness gap. Fork rev `f3e66904`.

## mlx-native — Gemma 3 (text) support — own model file + three general template/EOS/lm_head fixes
Completed: 2026-06-14
Opens Google's Gemma 3 family (`model_type: "gemma3_text"` / multimodal-wrapper `gemma3`). Unlike
Mistral/Phi-3 this is a genuine port (`gemma3.rs`): the `(1 + weight)` RMSNorm convention (f32),
embedding `sqrt(hidden)` scaling, per-head q/k RMSNorm, GELU(tanh) MLP, four norms per layer,
alternating local/global attention (per-layer RoPE base `rope_local_base_freq` vs `rope_theta`,
every `sliding_window_pattern`-th layer global), and `query_pre_attn_scalar^-0.5` scale; own
`Generate`, `LoadedModel::Gemma3`. Validated end-to-end (`mlx_gemma3_chat`, gemma-3-1b-it-4bit):
*"Paris is the capital of France."* (clean). Getting there required three fixes that are GENERAL
improvements, not Gemma-specific: (1) mlx-community 4-bit conversions ship a SEPARATE quantized
`lm_head` even for tied models (its quant params differ from the embedding's) — detect an `lm_head.*`
key and use it; (2) chat templates that emit `{{ bos_token }}` themselves (Gemma) got an empty BOS
because the minijinja context lacked it — `bos_token`/`eos_token` are now threaded into the render
context (a BOS-sensitive model was pure garbage without the leading `<bos>`); (3) the assistant
turn-end token `<end_of_turn>` (106) wasn't in EOS (config eos is only `<eos>`=1), so the model
over-ran into garbage past its answer — the worker now adds the tokenizer's turn-end token to EOS.
Added to `models::RECOMMENDED`; 131/0. Deferred: the 512 sliding window is approximated by full
attention (exact within the window); Gemma 2's logit soft-cap and the vision tower are separate.
Fork rev `9b4c844f`.

## mlx-native — Phi-3 support (fused-projection split into the Llama path, no new model file)
Completed: 2026-06-14
Opens Microsoft's Phi-3 family (`model_type: "phi3"`). Phi-3 is the Llama architecture but ships
FUSED projections — one `qkv_proj` and one `gate_up_proj` per layer instead of separate
`q/k/v_proj` + `gate/up_proj`. Instead of a whole new model file, `llama::load_phi3_model` splits
each fused tensor along the OUTPUT axis into the separate weights the Llama structure expects, then
returns a `llama::Model`. The 4-bit AFQ packing is along the INPUT axis, so row-slicing the
weight/scales/biases is exact (no unpacking). Phi-3 then runs on the existing Llama path — Generate,
batched decode, sampling, everything — with no new runtime variant (`"phi3" => load_phi3_model →
LoadedModel::Llama`). Validated end-to-end first try (`mlx_phi3_chat`, Phi-3-mini-4k-instruct-4bit):
*"The capital of France is Paris."* Added to `models::RECOMMENDED`; `supported_model_type` + the
dense-classification guard updated; 131/0. (Phi-3-mini-4k uses full RoPE; the 128k su/longrope
variant would need `rope_scaling` threaded — a small follow-up. Phi-3.5-mini is the same arch.)
Fork rev `0bfa4bd6`.

## mlx-native — verified Llama-family aliases + non-quantized (bf16) load (SmolLM2)
Completed: 2026-06-14
Closes two "quick & cheap" catalog verifies with one model. `mlx-community/SmolLM2-1.7B-Instruct` is
a non-Llama-3 `model_type: "llama"` checkpoint AND a non-quantized bf16 model, so running it
(`mlx_smollm_chat` → *"The capital of France is Paris."*) confirms both `mlx-native-llama-aliases`
(the wider Llama family runs on the shared llama path) and `mlx-native-fp16-verify` (the AFQ loader's
`quantization = None` branch loads full-precision MLX checkpoints, just using more RAM). The recent
`head_dim` config-tolerance fix held for SmolLM2 too. Added to `models::RECOMMENDED` as a tiny,
light, non-Qwen option.

## gateway — admission counters in /stats (fast-lane hits, shed/429, queued, admitted)
Completed: 2026-06-14
Finishes `concurrency-observability`. The `/stats` `admission` block now carries, alongside the
instantaneous window (limit / in-use / waiting / free), four cumulative scheduler counters:
`admitted` (total requests that got a slot), `fast_lane` (the subset that took a reserved fast-lane
slot), `shed` (rejected with HTTP 429 because the queue was full), and `queued` (had to wait for a
slot). They live in the `AdmissionScheduler`'s `State` under its existing mutex (no new atomics):
`take()` bumps `admitted` (+`fast_lane`), the full-queue path bumps `shed` before returning
`Overloaded`, a queued arrival bumps `queued`, and a pumped-but-cancelled waiter decrements back so
the count stays exact. Exposed through `AdmissionSnapshot` + a new `AdmissionScheduler::counters()`.
Test `counters_track_admit_fastlane_queue_and_shed` walks the full flow. The admission policy
(limit, fast-lane reservation, queue depth) is now tunable from data.

## mlx-native — Mistral / Mistral-Nemo support (Llama-path alias) — VALIDATED + 2 config fixes
Completed: 2026-06-14
Opens the Mistral family (`model_type: "mistral"`) at near-zero cost and validated it end to end
(*"Paris is the capital of France."* on Mistral-7B-Instruct-v0.3-4bit). Mistral / Mistral-Nemo are
architecturally Llama and upstream `mlx_lm` serves them with the *llama* class, so `LoadedModel::load`
routes `"llama" | "mistral" => llama::load_llama_model` and `supported_model_type` admits `"mistral"`
— no new fork model file. (The one delta, Mistral's 4096 sliding-window attention, is approximated by
the llama path's full attention: identical except beyond the window — fine for agents, bounded by the
KV preflight.) Running it surfaced two GENERAL config quirks (not the alias itself), both fixed in the
fork so the whole Llama family is more config-tolerant: (1) Mistral's `config.json` omits `head_dim`
→ `llama::ModelArgs.head_dim` is now `Option`, default `hidden_size/num_attention_heads`; (2) Mistral
ships `chat_template` as the older list-of-`{name,template}` form → `load_model_chat_template_from_str`
parses both the string and list forms (picking the `"default"` entry), with a unit test (and the
pre-existing broken `mlx-lm-utils` tests fixed along the way). Added to `models::RECOMMENDED`; fast
guards + `mlx_mistral_chat` network test; 131/0. Fork rev `3f230b2a`.

## mlx-native — idle-unload proven to reclaim memory (100%) + non-blocking unload + memory in /stats
Completed: 2026-06-14
Follow-through on the worker-join `Drop`: proves it actually frees the model's RAM and stops it
blocking the runtime. (1) **Proof it reclaims memory.** MLX weights live in unified-memory Metal
buffers that process RSS does NOT capture (an `ps -o rss` probe saw the load add only ~600 MB and free
~0). Added a fork `mlx_rs::memory` module wrapping mlx-c's `mlx_get_active_memory` / peak / cache, and
a test (`mlx_drop_reclaims_memory`) using MLX's own counter: load a model, chat, drop the backend —
`active before=0MB → after_load=2197MB → after_drop=0MB`, i.e. **100% of the model's Metal memory is
reclaimed** on drop. So idle-unload genuinely returns the RAM. (2) **Non-blocking unload.** The
`Drop` now joins the worker (blocks until buffers free), so `unload()` doing `*backend = None` inline
under the `backend` RwLock would stall every concurrent `current()` reader and block a tokio thread.
Fixed: take the backend out of the lock first (so `current()` reports unloaded immediately), then free
it on `spawn_blocking`. (3) **Observability.** `/stats` now reports `mlx_memory_mb` (active / peak /
cache) — watch the model's footprint, and watch `active` drop to ~0 after an idle-unload.

## mlx-native — verified batching fires through the admission layer + batch observability
Completed: 2026-06-14
Closes the loop on the batched-decode work: confirmed that concurrent load actually batches through
the real production path, and exposed it operationally. The gateway serves every request via
`concurrency::admit_wrap(backend)` with `limit = concurrency_capacity() = batch_cap()`, so a new test
(`mlx_admit_wrap_batches_e2e`) wraps the REAL MLX backend the same way, asserts the admission limit is
2, and fires two concurrent requests — admission lets BOTH reach the worker and they land in ONE
`run_batch` (`Paris`/`Tokyo`, `run_batch calls=1`). So the batching isn't serialized by admission; it
fires end-to-end. Observability: promoted the batched-decode counters to real metrics
(`mlx_native_backend::batch_stats()` → runs / rows / mid-decode admits / peak batch size) updated in
`run_batch`/`run_batch_hybrid` + on each continuous admit, and surfaced them in the gateway `/stats`
JSON alongside a new `admission` block (limit / in-use / waiting / free). `/stats` now answers "how
many concurrent requests actually share a forward" — `batch.avg_occupancy = rows/runs`, `batch.max`
the high-water size, `batch.admits` the continuous admissions. Verified live: after a 2-row batch,
`batch_stats()` reports `runs=1 rows=2 admits=0 max=2`. Default (no-feature) build returns `None`.

## mlx-native — join the worker thread on drop (deterministic unload + model swap)
Completed: 2026-06-14
`MlxNativeBackend` spawned its `!Send` model-owning worker thread **detached** (the `JoinHandle`
was discarded) with no `Drop`, so dropping a backend only closed the job channel and let the worker
free its ~8–15 GB of MLX buffers **asynchronously**. A subsequent model load then raced that teardown
on the shared single-stream Metal context — so an unload didn't deterministically reclaim RAM, and a
load→unload→load swap could corrupt MLX state. Fix: keep the worker `JoinHandle` and add a `Drop` that
closes the channel (the sender is now an `Option`, taken first so `blocking_recv` returns and the
worker exits) then **joins** the thread — the model's buffers are fully freed before drop returns.
Validated by `mlx_sequential_backend_loads`: load a backend, run a **batched** decode, drop it
(join+free), then load a *second* backend in the same process and chat — both answer correctly
(previously the second load hit corrupt MLX state). This is the model-unload-on-idle / model-swap
path. (Note: running multiple model `#[tokio::test]`s in one `cargo test` process still crashes —
each test spins its own tokio runtime; that's a harness artifact, not the production path. Run model
e2e tests individually, as the `#[ignore]` markers already imply.)

## mlx-native — batched sampling (temperature/top-p/top-k requests batch too, not just greedy)
Completed: 2026-06-14
Batched decode is no longer greedy-only. The new fork sampler
`qwen3::sample_rows(logits[B,vocab], temp[B], top_k[B], top_p[B])` samples one token per row with
each row honoring its OWN temperature / top-k / top-p, via a single unified nucleus path: `top_k <= 0`
and `top_p >= 1` keep all tokens (plain temperature-categorical), and `temp == 0` is a per-row argmax
override — so a single batch can MIX greedy and sampling requests. The batching gate relaxed from
`is_greedy` to `is_batchable`: any request batches unless it needs a repetition penalty (per-row
history scattered into the logits) or a fixed seed (per-row RNG keys), which stay on the serial path.
`run_batch`/`run_batch_hybrid` build per-row `[B]` param arrays from each row's `SamplingParams` and
call `sample_rows` in place of argmax at every selection point (the decode step, mid-decode admission,
and the first token after prefill). Validated: the fork's `sample_rows_per_row_collapses_to_argmax`
proves a mixed per-row batch each collapses to its own argmax deterministically; the existing greedy
end-to-end tests now route through `sample_rows` at temp 0 and stay byte-exact (`Paris`/`Tokyo`/
`Berlin`); and `mlx_batched_sampling_two_concurrent` confirms two `temperature=0.7` requests batch
(`run_batch calls=1`, previously they fell back to serial) and stream coherent output. This widens how
many concurrent requests actually share a forward — real agents often run with temperature > 0.

## mlx-native — continuous batching (admit queued requests into a live batch mid-decode)
Completed: 2026-06-14
Batched decode no longer waits for the whole batch to drain before serving the next request. While
a batch decodes, `run_batch`/`run_batch_hybrid` now ADMIT queued greedy jobs from the worker channel
into freed or spare slots (up to `ROZUM_BATCH`): a short request that finishes frees its slot, and a
waiting request is prefilled and stacked into the batch on the next step instead of idling — better
GPU utilization under uneven response lengths and bursty arrivals. The decode loop tracks the KV
`width` and each row's pad explicitly (invariant `pad_i = width − len_i`); admitting a row grows the
shared width (left-padding existing rows) only if the new prompt is longer, then concatenates it on
the batch axis (dense KV or the heterogeneous hybrid `LayerCache`). It's byte-exact by the same
argument as the initial ragged assembly — the new row's left-pad is masked and its RoPE offset is
its true position — so an admitted row decodes identically to running alone. Non-greedy jobs pulled
from the queue are run serially afterward; a lone greedy request still goes serial (keeping the
prefix-KV LRU). Validated end-to-end (`mlx_continuous_admit_three`): three concurrent requests with
`ROZUM_BATCH=2` — the first two batch, the third is admitted into a freed slot mid-decode (ONE
`run_batch` call, `BATCH_ADMIT_COUNT` confirms the admission), and each returns its own correct,
uncontaminated answer (`Paris` / `Tokyo` / `Berlin`). All dense + hybrid byte-exact and scheduler
tests remain green.

## mlx-native — batched/parallel decode for hybrid Qwen3.6 (the primary coding model)
Completed: 2026-06-14
Extends batched decode to the hybrid Qwen3.6 arches (dense `Qwen35` + MoE `Qwen35Moe`) — the models
that actually run the coding agents — so two+ concurrent sessions share one forward. The feared
blocker ("the GatedDeltaNet recurrence can't be left-padded") was a non-issue: we prefill each
sequence separately, so no pad token ever advances the recurrence, and the GDN state is fixed-size
per row. The GatedDeltaNet turned out to be **already batch-generic and row-independent** (kernel
grid spans `b*hv`, conv+recurrent state is `[B,…]`) — proven byte-exact by a synthetic probe with no
model load (`gated_delta_batches_row_independent`). So hybrid batched decode is just: the dense
ragged path for the full-attention layers (left-pad+stack KV, per-row RoPE + key-pad mask, ported to
`qwen3_5::Attention` via two thread-locals — OFF by default, B=1 byte-identical) **plus stacking the
fixed-size conv + recurrent state on the batch axis for the GatedDeltaNet layers** (no padding, rope,
or mask). `run_batch_hybrid` assembles the heterogeneous `qwen3_5::LayerCache` and serves both hybrid
arches (shared Model API); the worker routes hybrid greedy batches to it via `is_hybrid_arch`.
Validated on the real Qwen3.6-27B: **byte-exact** per row vs serial decode incl. the padded row
(`mlx_hybrid_batched_ragged_byte_exact`); two concurrent sessions batch into ONE call with distinct,
uncontaminated answers — `"Paris"` / `"Red"` (`mlx_hybrid_batched_scheduler_two_concurrent`); and
**2.30× throughput** at B=2 (`mlx_hybrid_batched_decode_throughput`, test profile — even higher than
dense's 1.98× because hybrid decode launches more ops per token for batching to amortize). With
single-stream hybrid decode already maxed (~90% of Python), batching is now the only lever that
scales hybrid throughput, and it works. Fork rev `9a3b3949`.

## mlx-native — batched/parallel decode (dense Qwen3): 2 concurrent sessions in one forward
Completed: 2026-06-14
The native MLX backend was capacity-1: one worker thread ran jobs strictly serially, so two
sessions (Claude Code + Codex, or several meeting-room agents) serialized — the second queued
behind the first. It now **batches concurrent greedy requests through one `forward`**. With
`ROZUM_BATCH=N` (default 1 = the proven serial path), `worker_main` drains up to N already-admitted
jobs within a small `ROZUM_BATCH_WINDOW_MS` window (default 10ms), batches the greedy (argmax) ones
(≥2) via `run_batch`, and runs everything else — non-greedy requests, single jobs, non-batchable
arches (Llama/Qwen2/hybrid Qwen3.6) — on the existing serial prefix-KV path. `run_batch` prefills
each sequence separately (correct per-sequence KV, keeps prefix reuse), assembles one left-padded
batched cache (`ConcatKeyValueCache::{kv_used, from_kv}`), then decodes all rows together: per-row
RoPE via `qwen3::set_batch_pad_offsets` + a per-row left-pad mask, argmax per row, per-sequence
detok/stream (`BatchSeq`), and retires a row on EOS/max-tokens/runaway by slicing it out
(`take_axis`) and re-assembling the mask/offsets. `concurrency_capacity()=Some(ROZUM_BATCH)` so
admission admits B. **Why it's a real win:** decode is ~92% CPU graph-build, and batching does ONE
build for B sequences — it amortizes exactly the cost `mlx-native-perf-compile` couldn't reduce
(`mx.compile` was net-negative here), so the two perf threads converge. Validated: B=2 throughput
**126.3 vs 63.9 t/s = 1.98×** (`mlx_batched_decode_probe`); ragged forward byte-exact to 1 bf16 ulp
(`mlx_batched_ragged_byte_exact`); end-to-end `mlx_batched_scheduler_two_concurrent` — two
concurrent requests batch into ONE `run_batch` call and each row gets its own uncontaminated answer
(`France="Paris." Japan="Tokyo"`). B=1 path is byte-identical to before (per-row rope OFF by
default) — zero regression. Continuous batching (admit a queued job mid-decode) and hybrid Qwen3.6
batching are follow-ups.

## launch — `rozum launch codex` works out-of-box (+ quiet /v1/models)
Completed: 2026-06-14
Codex now launches against the local gateway like Claude already does. Codex **ignores
`OPENAI_BASE_URL`** and (≥ 0.137) needs the Responses API, so `rozum launch` detects a `codex`
program and injects the `-c` overrides on top of the user's `~/.codex` (left intact):
`model_provider=rozum`, `model_providers.rozum.base_url=…/v1`, `wire_api="responses"`,
`env_key="OPENAI_API_KEY"`, and `-m local` (only if the user didn't pass a model). Verified:
`rozum launch --model <spec> -- codex exec "…" --dangerously-bypass-approvals-and-sandbox` →
Codex connects (`provider: rozum`) and answers, `rc=0`. Also: `/v1/models` now returns an empty
`models: []` next to the OpenAI `data` so Codex's model-list refresh stops logging a non-fatal
"failed to refresh available models" warning (its `Model` entries have many required fields, but
the launch forces `-m local`, so the list is unused).

## mlx-native — prefix-KV cache: per-session LRU (interleaved sessions each reuse)
Completed: 2026-06-14
The prefix cache kept a single slot per worker, so *interleaved* conversations thrashed it:
session A's turn → session B's turn evicts A → A's next turn re-prefills from scratch (no
reuse for anyone). This matters whenever more than one conversation shares a gateway — several
meeting-room agents, or Claude Code + Codex at once. Replaced the single slot with a small
**LRU** (`PrefixStore`, default 4 slots, `ROZUM_PREFIX_CACHE_SLOTS`): each request reuses the
stored conversation it extends via a **longest-prefix match** (`best_match`), content-based so
no per-dialect session id is needed; the matched entry is replaced at MRU, an unmatched (new)
conversation inserts + evicts the LRU. A worker serves one model, so only the dense or the
hybrid LRU is populated. Verified live (small dense model, A1/B1/A2/B2 interleaved):
`SLOTS=4 → 2 reuse fires` (both A2 and B2 reuse their own prefix), `SLOTS=1 → 0` (thrash).
Each slot holds a conversation's KV, so it costs memory — lower the slot count for very long
contexts. Unit test `prefix_store_best_match`; byte-exact reuse tests still green.

## mlx-native — prefix-KV cache: key on the conversation boundary (make reuse fire)
Completed: 2026-06-14
The prefix-KV cache (dense + hybrid) was keyed on the **full prompt**, so reuse never
actually fired: the trailing generation prompt — especially the thinking-off
`<think></think>` prefill — does NOT recur next turn (the same turn is later re-rendered
as a *completed* message), so consecutive prompts share only the **conversation** prefix
(measured: LCP 3525/3529 — they diverge in the last 4 tokens). With `starts_with(full
prompt)` the match failed every time (`reuse_len=0`), and the byte-exact tests passed
*vacuously* (fresh == fresh). Fix: persist + key on the **conversation boundary** (the
prompt rendered without the generation prompt, `render_prompt_opt(add_gen=false)`); the
next turn `starts_with` that and reuses it. For hybrid, the Linear-state snapshot is now
taken at that boundary too — prefill the conversation part, snapshot, then forward the
tiny generation-prompt tail (`Generate::set_gen_prompt_len`, fork rev `c9ee1940`),
byte-exact (the split is position-local + causal). **Now reuse fires** (e.g.
`reuse=3522/3547, prefill 25 new tokens`) and a turn-2 prefill on a ~3.5k-token context
drops **2.62s → 0.13s (~20×)**. Byte-exact tests now genuinely exercise reuse.

## Gateway — `POST /v1/responses` (OpenAI Responses API): Codex now works
Completed: 2026-06-14
Codex CLI ≥ 0.137 dropped `wire_api="chat"` and **requires** the OpenAI Responses API; the
gateway only had `/v1/chat/completions` (+ Anthropic `/v1/messages`), so Codex got 404 and was
**fully blocked**. Added `responses_handler`: it translates the Responses request
(`instructions` → system; `input` items — messages / `function_call` / `function_call_output`;
flat `tools`; `max_output_tokens`) into the internal `ChatBackend` and streams the typed
Responses SSE protocol (`response.created` → `output_item.added`/`content_part.added` →
`output_text.delta` → `output_text.done`/`content_part.done`/`output_item.done` →
`function_call` items (`arguments.delta`/`.done`) → `response.completed`; non-stream returns the
final `response` object). One render fix was needed: Codex sends a top-level `instructions`
**and** a `developer` message — two system turns — which the Qwen3.6 template rejects
("System message must be at the beginning."); the conversion now folds all system/developer text
into one leading system message. **Codex e2e build task PASSES end-to-end** (`reverse-cli`,
`cargo run -- hello` → `olleh`, `rc=0`, ~71 s). Tests: input/tool conversion, multi-system fold,
response-object shape, SSE smoke.

## mlx-native — prefix-KV cache reuse for the hybrid Qwen3.6 arches
Completed: 2026-06-14
Extends prefix reuse to the hybrid Qwen3.6 models (Qwen35 + Qwen35Moe — the models the e2e
runs). Their `Full(KV)` layers truncate to the shared prefix like dense; their `Linear`
GatedDeltaNet layers carry a **recurrent** state that can't be truncated, so it is deep-copied
(`Array::deep_clone` → own buffer, survives decode buffer donation) at the **end of prefill**
(offset == prompt len) and restored on the next reuse. Fork (`fd284599`):
`LayerCache::{truncate, snapshot, restore}` + `LinearSnap`, `Generate::with_cache` (start from a
pre-populated cache, snapshot the Linear state right after the prefill step) +
`into_cache_and_snapshot`. rozum: `stream_generation` returns the iterator so the hybrid arms
reclaim the cache + snapshot; the worker persists `HybridPrefix{ids, cache, snap}`, and on reuse
truncates Full + restores Linear + prefills only the new suffix. **Byte-exact** vs a fresh
prefill (integration test `mlx_prefix_reuse_byte_exact_hybrid` on the deterministic Qwen3.6-27B).
Now every agentic turn — dense OR hybrid — skips re-prefilling the growing conversation.

## mlx-native — prefix-KV cache reuse across agentic turns (dense)
Completed: 2026-06-14
Every Claude Code / Codex turn used to re-prefill the **entire growing conversation** (a fresh
cache per request) — the dominant agentic latency, not decode. The cap-1 worker now persists the
previous request's prompt ids + KV; when the next prompt strictly extends it (the append-only
agentic-loop case) it truncates the cache to the shared prefix and prefills only the **new
suffix**. Byte-exact — the kept `[0,reuse)` KV is exactly what a fresh prefill computes, and
`create_attention_mask` builds the causal mask from the cache offset (integration test
`mlx_prefix_reuse_byte_exact`: reuse output == fresh prefill). Dense arches (Qwen3 / Qwen3-MoE /
Llama / Qwen2); needs the new fork method `ConcatKeyValueCache::truncate`. Hybrid (Qwen3.6) is a
scoped follow-up (its recurrent state needs snapshotting, not truncation). `ROZUM_PREFIX_CACHE=0`
disables.

## mlx-native — runaway-stop: bound a single runaway generation (reliability)
Completed: 2026-06-14
One greedy generation could loop (repeat a short block / never emit EOS) and generate to the
client's large `max_tokens`, pinning the cap-1 worker for minutes (the e2e `test` task hit a
600 s hang, `result=None`). Two guards in the backend: a hard `max_tokens` ceiling
(`DEFAULT_OUTPUT_CEILING=8192`, `ROZUM_MAX_OUTPUT_TOKENS` overrides) and `is_runaway_loop` in
`stream_generation` — stop when the last 64 generated tokens are exactly periodic with period
≤16 (a short block repeated ≥4×), which catches a greedy loop in ~64 tokens with no false
positives on real text (`ROZUM_REPEAT_GUARD=0` disables). `--max-turns` does NOT help (it bounds
the agentic loop, not one generation). Unit test `runaway_loop_detection`.

## Gateway — parse Qwen3.6's `<function=>` XML tool-call format (agentic coding fix)
Completed: 2026-06-13
Qwen3.6 emits tool calls in EITHER the JSON form
(`<tool_call>{"name":…,"arguments":…}</tool_call>`) OR the Hermes-style XML form
(`<tool_call><function=NAME><parameter=K>V</parameter>…</function></tool_call>`), chosen
nondeterministically. The backend only parsed the JSON form, so the XML calls were
silently dropped — the `<tool_call>` opener suppressed text streaming, the parse then
failed, and the client got an **empty response** with the tokens lost. For agentic
coding (Claude Code / Codex, which live in multi-step tool loops) this meant tool calls
randomly failing. Now `parse_tool_calls` accepts both forms, tolerates a missing
`</tool_call>` (model hit EOS after a complete body), and falls back to emitting the raw
run as text if a `<tool_call>` appeared but nothing parsed — so tokens are never silently
swallowed. Verified read→write_file end-to-end (5/5 OpenAI, 3/3 Anthropic).

## Gateway — CC/Codex compatibility fixes (audit)
Completed: 2026-06-13
A synthetic audit of the gateway against the OpenAI (Codex) and Anthropic (Claude Code)
dialects found the core protocol solid (streaming SSE, non-stream JSON, tool-use, stop
reasons, 422 validation). Two fixes:
- **stream default**: an absent `stream` field defaulted to SSE; the OpenAI/Anthropic
  specs default to non-streaming JSON. A client that omits `stream` now gets JSON, not an
  unparseable SSE stream. (Streaming clients — CC, Codex — always send `stream:true`.)
- **`--enable-thinking` flag (reasoning OFF by default)**: reasoning models (Qwen3) emit
  `<think>…</think>` — even an empty `<think></think>` — which leaked into CC/Codex content.
  The gateway now renders the chat template with `enable_thinking=false` by default (the
  prompt prefills a closed `<think></think>`, so the generated output is clean); pass
  `rozum gateway --enable-thinking` (or set `ROZUM_ENABLE_THINKING`) to turn reasoning back on.
- (`/v1/models` id `claude-rozum-<spec>` is intentional — `rozum launch` exports it as
  `ANTHROPIC_MODEL` so CC pre-selects the local model.)

## Gateway — hybrid decode now pipelines (prod path 62 → ~96 t/s)
Completed: 2026-06-13
The in-process gateway path (`MlxNativeBackend.chat`) decoded the Qwen3.6 hybrid models
~30% slower than the raw engine because `stream_generation` ran each token's GPU sync
(`eval` + `token.item()` host readback) serially, with `pipeline=false` left over from
when the GatedDeltaNet kernel blocking-eval'd its state per call. The retain fix
(`ROZUM_MLX_RETAIN`) removed that eval, so the hybrid models now pipeline like the dense
ones — the next token's forward `async_eval`s while the current token's id is read back.
Prod `backend.chat` decode 62 → ~96 t/s (the per-token sync 14ms → 0); byte-identical
output. (Profiling showed detokenization was never the cost — 0.03 ms/token.) Adds a
prod-path perf test (`mlx_moe_backend_chat_tps`) + a `hybrid_models_need_retain` guard.

## MLX native runtime — pre-allocated KV cache
Completed: 2026-06-13
`ConcatKeyValueCache` now pre-allocates its key/value buffers in 256-position blocks and
writes each decode step in place (`slice_update`), returning a `[:offset]` view — instead
of `concatenate`-ing (and reallocating) the entire history every step (mirrors Python
`mlx_lm`'s `KVCache`). The per-step O(context) copy becomes an amortised O(1) write (one
growth concat every 256 steps); decode t/s is flat across context. Decode output is
byte-identical (greedy IDs unchanged, all chat tests pass); chunked-vs-single prefill
stays argmax-exact (~1 bf16 ulp from the strided-slice SDPA on non-step-aligned single
passes). For long sessions this removes the realloc churn. Fork `d197d1da`.

## MLX native runtime — decode perf root-caused & fixed (+2.7× MoE)
Completed: 2026-06-13
Closed the native-MLX decode gap vs Python `mlx_lm` for the Qwen3.6 hybrid models.
- **Root cause:** `GatedDeltaNet` scaled q/k by `Array::from_f32(inv_scale)` — a *strong*
  f32 0-dim array — which promoted the whole hidden stream bf16→f32 at the first GDN
  layer (Python multiplies by a python float, staying bf16). The f32 stream then forced
  ~1000 bf16→f32 casts/token on the quantized scales/biases at every matmul and ran the
  matmuls in f32. Fix: scale by a scalar cast to q/k's dtype (one line each).
- **Also:** MoE expert-sort for prefill (`SwitchGLU` `_gather_sort`/`sorted_indices`),
  and `fast::rms_norm_no_weight` (null-weight kernel) for the weightless GDN norm.
- **Results (byte-exact, all chat tests pass):** Qwen3.6-35B-A3B-4bit decode 33→~88 t/s,
  prefill 943→~1215 (= Python 1180); dense 27B decode 16→~19.6.
- Tooling added: `mlx_export_to_dot` (mlx-c) + rust wrapper + `count_prims.py` for
  per-token graph-primitive counting. Full log: `docs/mlx-gd-bug/LOG.md`.
- Pins mlx fork `0d4b3729` (mlx-c `d71809d`); reproducible git-rev build verified.

## channel-wakeup fixes + rozum-native-channels (Tier 2)
Completed: 2026-06-11
Two corrections/extensions to the channel-wakeup launch flag that landed via the
`gateway-switch` build-fix:
- **Detection fix:** `ChannelWakeup::flags_for` probed `claude --help` for the
  flag string, but the research-preview `--dangerously-load-development-channels`
  flag is **hidden from `--help`** (verified empirically) — so detection always
  failed and channel wakeup silently never activated. Switched to a
  `claude --version` ≥ 2.1.80 gate (`claude_version_supports_channels`, unit-tested).
- **Server name via env:** `--channel-mcp-name` is now `Option<String>` resolving
  flag → `ROZUM_CHANNEL_MCP_NAME` → default `rozum`, so the name can be set in a
  shell profile/wrapper. Both `--channel-mcp-name` and `--no-channel-wakeup` are
  now hoisted by `reorder_launch_args` like the other launch flags.
- **rozum-native-channels Tier 2:** the mcp-proxy `instructions` now pin the
  Anthropic-independent fallback — if the agent isn't receiving `<channel>` events
  (client without channel support), keep a `meeting.wait_my_turn` long-poll
  outstanding while idle; it returns the instant someone speaks, so no turn is
  missed without channels. This makes `wait_my_turn` the universal native channel
  (Tier 2); `claude/channel` is the Tier-1 optimization, gateway piggyback the
  Tier-3 last resort. Spec: `docs/specs/rozum-native-channels.md`. No new deps.

## gateway-unload-on-idle — free model RAM when agents are attached but idle
Completed: 2026-06-11
The shared gateway now auto-`unload`s the resident model after a long idle window
while keeping the daemon alive, for the case the existing idle-exit deliberately
skips: agents attached (leases held) but not generating. idle-exit only fires at
`live_leases == 0` (process exit); this fills the `leases > 0`-but-idle gap by
dropping just the model's RAM and lazily reloading on the next chat. Implemented
on the **same 30 s idle watchdog tick** (`src/gateway.rs`): evaluate idle-exit
first (frees most when truly abandoned), then idle-unload when the model is
resident, nothing is `generating`, and `last_active` is older than
`ROZUM_GATEWAY_UNLOAD_IDLE_SECS` (default 900 s / 15 min; `0` disables). Reuses
`gateway-switch`'s `Switchboard::unload()` + serialized lazy reload; a new
`is_loaded()` guard makes it fire once (no per-tick re-drain/log spam) and
`can_reload()` keeps a `--dedicated` gateway (no builder) from ever auto-unloading.
Emits a `gateway_idle_unload` obs event. Spec: `docs/specs/model-unload-on-idle.md`.
Follow-ups (need a real model on Metal): cold-vs-warm reload measurement to decide
any fast-reload tier beyond the OS page cache, and pre-warm on a turn signal.
No new deps.

## runtime-config — declare backends, policy & default model in `rozum.toml`
Completed: 2026-06-11
The gateway's backend selection and default model can now be declared once in a
`rozum.toml` instead of re-typed as `--model` / `--backend` every session. A new
`src/config.rs` (`RuntimeConfig`, serde + `toml`) is resolved from `$ROZUM_CONFIG`
→ `./rozum.toml` → `$XDG_CONFIG_HOME/rozum/rozum.toml`; a malformed file (or a
`$ROZUM_CONFIG` that points at a missing one) is a hard error rather than a silent
fall-back, because a config the user deliberately wrote must surface. The schema is
a `[runtime]` block (`model`, `n_ctx`, `policy`, `backend`) plus an ordered list of
`[[backend]]` tables (`id`, `engine`, optional `model`/`n_ctx`/`url`/`enabled`).
Policies: `single` / `fallback` / `fanout`. Engine names span everything rozum can
build — the gateway engines `gguf`/`mistralrs`/`lmstudio`/`mlx`/`url` and the sync
meeting-room engines `hello`/`candle`/`llama-gguf`/`native-rust`/`external-command`
(the latter map to a placeholder in the sync `BackendRegistry`; the gateway builds
the HTTP/native ones for real).

`RuntimeConfig::default()` **is** the old auto-detect chain in code — `Fallback`
over `[gguf, mistralrs, lmstudio, mlx, url]` — so a user who never writes a config
sees zero behaviour change. The daemon's initial model load and every `gateway
switch` now walk this chain (`main.rs::build_from_config` / `build_choice`,
returning the first backend that builds), with the config injected into the
`Switchboard`'s `BackendBuilder` from `gateway-switch`. `--backend B` still
force-bypasses the chain to a single engine. `[runtime].model` / `[runtime].n_ctx`
fill in when `--model` / `--n-ctx` are omitted on `rozum gateway`; per-backend
`url` pins an explicit endpoint for an `lmstudio`/`mlx`/`url` entry. The
library/binary split from `gateway-switch` is preserved: the plan
(`gateway_chain()`) lives in the library, the async build stays in the binary. 12
Metal-free unit tests; lib suite 101 passing. No new deps (`toml` was already in).

### Build fix bundled with this work
The `gateway-switch` commit had swept in stray, incomplete `channel-wakeup` WIP
(`exec_agent` / `exec_agent_anthropic` call sites passing a `&channels` argument
the signatures never accepted), so `master` did not build on default features. A
separate fix commit threads `ChannelWakeup` through and applies `flags_for()`,
which also completes the `channel-wakeup-launch-flag` mechanism: a capable
`claude` now gets `--dangerously-load-development-channels server:<name>` appended
at launch (`--no-channel-wakeup` suppresses; `--channel-mcp-name` sets the name).

## gateway-switch — transparent in-place model/backend switch, reload & unload
Completed: 2026-06-11
`rozum gateway switch --model Y [--backend B] [--n-ctx N]` swaps the resident
model of the running shared daemon **in place**: it drains in-flight work, drops
the old model (never two resident — the memory constraint), loads the new one,
bumps a new `generation`, and resumes. Clients' launch-local proxies hold their
queued requests across the gap (`/v1/admit` advertises a closed window while
draining, so it looks like backpressure, not a failure) and a request already
mid-flight is held in the daemon until the swap finishes — so the swap is
transparent, just slower. The daemon now holds its backend in a `Switchboard`
(swap cell + an injected `BackendBuilder` closure over `rozum`'s own
backend-selection chain), and every chat handler takes a `ChatLease` for the
whole stream so a switch waits for streaming to finish before swapping. Drain
tracks a dedicated `generating` counter (the idle-watchdog `in_flight` counter
can't be used — it's held for parked requests and would deadlock the drain),
bounded by `ROZUM_GATEWAY_DRAIN_SECS` (default 120). `--backend` forces an engine
(`gguf`/`mistralrs`/`lmstudio`/`mlx`/`url`); on a build failure the switch reverts
the spec so the next request lazily reloads the old model.

`rozum gateway reload` drains then re-execs the current binary (transparent
daemon/binary upgrade after a `rozum` upgrade); the brief port gap rides the
proxies' existing replay path. `rozum gateway unload` drops the model to free RAM
but keeps the daemon listening — the next chat lazily reloads it (serialized so
racing requests reload once). `generation` was added to the `active.json`
registry (`#[serde(default)]`, continued monotonically across respawns) so a
proxy can tell "the daemon I was talking to was replaced" from a transient blip;
`rozum gateway status` shows it as `gen:`. Control plane is auth-gated localhost
`POST /control/{switch,unload,reload}`. A `--dedicated` gateway has no builder, so
all three are cleanly refused. No new deps.

## launch-no-model — `rozum launch --no-model` (upstream Anthropic, no gateway)
Completed: 2026-06-11
`rozum launch` can now run an agent with no local model at all: `--no-model`
(and a new first **"Anthropic (cloud — no local model)"** entry in the interactive
picker) bypass the gateway entirely — no daemon spawn, no lease, no launch-local
proxy, and none of the `ANTHROPIC_*`/`OPENAI_*` gateway/model env overrides. The
child inherits the operator's own Anthropic auth (`ANTHROPIC_API_KEY` / claude.ai
OAuth), exactly like a bare `claude`; only rozum's agent-context defaults
(`CLAUDE_CODE_DISABLE_*`, each applied only if unset) still apply. Resolution is
modeled as `LaunchTarget::{Local(spec), Anthropic}`; `--no-model` `conflicts_with`
`--model`/`--dedicated`/`--n-ctx`/`--port` (clap-enforced) and is hoisted by
`reorder_launch_args` like the value flags (also fixing `--dedicated` placement
after the program name). This is the mode that makes Claude Code features
requiring real Anthropic auth — notably **channels** — available to a
rozum-launched agent (empirically a local-gateway base URL does *not* block
channels, but no-model is the clean path). Spec: `docs/specs/launch-wrapper.md`.
No new deps.

## shared-gateway-poison — soft, graduated poison-prompt protection
Completed: 2026-06-11
A request that repeatedly crashes the shared daemon is now handled gently instead
of either retrying forever or hard-banning a possibly-good prompt. The proxy
fingerprints each request (`share::fingerprint`, a hash of the raw body bytes it
forwards verbatim — so the proxy and daemon agree without dialect normalization).
Crash-attribution is precise: an upstream send error is blamed on the prompt only
when the connection was established and then died (`!is_connect()`); a pure connect
failure is a failover gap and stays on the wait-for-health replay path. On a
crash-attributed failure the proxy degrades (the retry takes an exclusive `lane`
write-lock, serializing the risky prefill so no neighbour competes for memory —
clearing most big-prompt OOMs), counts per fingerprint, and after `ROZUM_POISON_MAX`
(default 3) attempts returns a soft, retryable 422 (`poison_refused`). When those
graduated retries are exhausted *and* the crash was the sole in-flight request
(`admit.stats().in_use <= 1`), the fingerprint is confirmed machine-wide to a TTL'd
`poison.json` (`ROZUM_POISON_TTL_SECS`, default 3600); ambiguous concurrent crashes
stay local. A confirmed entry is fast-refused both by the proxy before forwarding
and by the daemon's new `poison_layer` before running the model (defense-in-depth
that survives the very crash it guards against), and decays on the next clean (2xx)
prefill, both locally and machine-wide. Tunables: `ROZUM_POISON_MAX`,
`ROZUM_POISON_TTL_SECS`. No new deps.

## shared-gateway-replay-retry (part 2) — two-tier admission
Completed: 2026-06-11
The daemon now advertises its admission state and each launch's proxy holds its
client's requests at the edge instead of bouncing them off a full daemon.
Tier-1 (global): `GET /v1/admit` reports `{limit,in_use,waiting,free}` from the
daemon's `AdmittingBackend` via a new defaulted `ChatBackend::admission_stats()`
(ungated backends report an always-free window). Tier-2 (per client): each proxy
runs its own `concurrency::AdmissionScheduler` (SJF + reserved fast lane, cost
estimated from body size, unbounded queue — a proxy never sheds its own client)
over the single agent's parallel requests, and `wait_for_window` polls `/v1/admit`
to hold a queued request until the daemon signals room (bounded; fail-open on a
probe failure, so the `429`/`Retry-After` backstop still applies). The local
admission guard is held for the whole stream. Env: `ROZUM_PROXY_ADMIT` (4),
`ROZUM_PROXY_FASTLANE_TOKENS` (1024). Reuses the one `concurrency` module at both
tiers. Completes `shared-gateway-replay-retry`. No new deps.

## shared-gateway-replay-retry (part 1) — replay before first token + smart retry
Completed: 2026-06-11
The launch-local proxy now makes a daemon crash transparent to the agent. The
`forward` path buffers the request body once and re-sends it on a replay loop:
a connection failure *before any response byte reaches the agent* is safe to
replay, so the proxy waits for re-election to bring the daemon back on the same
stable port (`wait_for_health`) and retries — the agent sees a slower response,
not an error. Once a `Response` is returned (status+headers committed), a
mid-stream death surfaces the error instead (we can't un-send tokens). Retries
use capped exponential backoff + ±50% jitter (no `rand` dep — wall-clock nanos),
a per-request attempt cap, wait-for-health between tries, and honor the daemon's
`429`/`Retry-After` by holding and retrying rather than bouncing it back. Tunable
via `ROZUM_PROXY_MAX_ATTEMPTS` (6), `ROZUM_PROXY_BACKOFF_MS` (150),
`ROZUM_PROXY_HEALTH_WAIT_SECS` (60). 3 new tests (backoff math + an end-to-end
replay-after-daemon-returns test). No new deps. (Two-tier admission follows in
part 2.)

## shared-gateway-proxy — launch-local reverse proxy in the request path
Completed: 2026-06-11
New `src/proxy.rs`: a model-free launch-local reverse HTTP proxy (gateway analog
of the mcp-proxy). `proxy::serve` forwards every request to the shared daemon's
stable port and streams the response back verbatim (SSE token streams included),
buffering the request body (the seed for future replay), stripping hop-by-hop and
framing headers both ways, with a no-timeout client. An unreachable daemon yields
a clean 502; `daemon_port` lives in an AtomicU16 so a later phase can re-point it
at a respawned daemon. `rozum launch` (`start_launch_proxy`) binds an ephemeral
loopback port, spawns the proxy, and points the agent at it (failover watchdog +
lease heartbeat still target the daemon); falls back to the daemon directly if the
proxy can't bind. Foundation for replay / poison / two-tier backpressure /
transparent swap. 5 new tests incl. two real end-to-end tokio tests. No new deps.

## models-rm — delete a cached model from disk
Completed: 2026-06-11
`rozum models rm <spec> [-y]` frees disk by deleting a cached model. It
exact-matches the spec against `scan_all_installed()`, refuses if it is the
active gateway model (reads `active.json` + health-probes), prints what will be
freed, and confirms (`--yes`/`-y` skips; a non-TTY without `--yes` is refused).
HuggingFace (`models--owner--name`) and LMStudio (the repo dir holding the
`.gguf`) directories are removed directly; Ollama is delegated to `ollama rm`
(its blobs are content-addressed and shared) and refused if the binary is absent.
Dependency-free `which` helper added. No new deps.

## launch-model-picker — optional --model, interactive picker, takeover-if-idle
Completed: 2026-06-11
`rozum launch --model` is now optional. `resolve_launch_model`: given → use it;
omitted + a healthy gateway running → reuse its model (`using running model: …`);
omitted + nothing running on a TTY → interactive `pick_model_interactive` (cached
models first, `(cached, size)`; then not-cached `RECOMMENDED`, `(not cached, ~GB)`;
a not-cached pick re-confirms the download); omitted + non-TTY → error. Model
mismatch now does **takeover-if-idle** in `ensure_shared_gateway`: a different
running model with no live client leases is SIGTERM'd and replaced on the same
port; with live leases it is reused-with-warning (don't steal a live session).
`--dedicated` still bypasses sharing. No new deps.

## shared-gateway-leases — client leases drive daemon lifetime + status/stop
Completed: 2026-06-11
Third phase of `shared-gateway`. Each launch holds a `leases/<pid>` file
heartbeated every 15s (mtime = liveness); `share::live_lease_count` counts fresh
leases and reaps dead ones. The daemon's idle watchdog now stays up while any
lease is fresh OR a request is in flight OR there was recent HTTP, and idle-exits
(ROZUM_GATEWAY_IDLE_SECS, default 900) only when all are quiet — so leases, not
raw HTTP traffic, are the primary keep-alive for launch clients, while a manually
run `rozum gateway` is still kept alive by traffic. Added `rozum gateway status`
(model/port/pid/n_ctx/uptime/clients) and `rozum gateway stop [--force]` (SIGTERM,
refused while clients attached); `gateway --model` is now optional (required only
to run the daemon). No new deps.

## shared-gateway-failover — respawn the shared daemon on death
Completed: 2026-06-11
Second phase of `shared-gateway`. `share::try_spawn_lock` adds an O_EXCL
`spawn.lock` with stale-steal + drop-release (best-effort anti-stampede; the TCP
bind remains the hard single-owner guarantee). `spawn_failover_watchdog` runs in
each launch alongside the agent: it polls the daemon every 5s and, after two
consecutive misses, respawns it on the same port under the spawn lock (rechecking
health first), waiting up to 120s. Simultaneous watchdogs are damped by the lock
and deduped by the port bind, so a crashed/killed daemon comes back without the
user relaunching; the agent reconnects over the brief gap via its own retry (same
stable URL). No new deps.

## shared-gateway-mvp — share one model daemon across launches
Completed: 2026-06-11
First phase of `shared-gateway`. `rozum launch` no longer always loads its own
in-process model (two launches → two models → OOM). New `src/share.rs` registry
(`active.json` under `$XDG_STATE_HOME/rozum/gateway/`, atomic write +
remove-if-mine, `health_ok` probe, `is_reusable`, stable `DEFAULT_GATEWAY_PORT`
8089). `rozum gateway` publishes the registry and idle-exits after
`ROZUM_GATEWAY_IDLE_SECS` (default 900) when nothing is in flight (in-flight-aware
via an Activity counter in the auth layer, so long generations don't trip it).
`rozum launch` reuses a healthy running gateway (or a different-model one with a
warning), else spawns a detached `rozum gateway` (own process group, stdio →
gateway.log) and waits for health; the TCP-port bind is the single-owner
guarantee. `--dedicated` keeps the old private in-process gateway. Deferred to
later phases: flock anti-stampede + crash re-election, client-pid leases, the
launch-local proxy / replay / poison / two-tier backpressure, switch/reload/
unload, gateway status/stop, the model picker, and `models rm`. 3 share unit
tests (no Xcode); fmt + feature build clean.

## concurrency-backend-abstraction — generic admission for any backend
Completed: 2026-06-11
Lifted the concurrency machinery (scheduler, memory budget, fast lane,
backpressure, circuit breaker) out of the mistralrs modules into a generic
`src/concurrency` module (renamed from `mistralrs_admission`), and re-applied it
as a decorator. `ChatBackend` gained an optional `concurrency_capacity() ->
Option<usize>` hook (default `None`); `concurrency::admit_wrap` wraps a backend in
`AdmittingBackend` iff it advertises a capacity, and passes remote / self-
serializing backends through untouched (the safe default). `MistralrsBackend`
now reports `Some(max_num_seqs)` and its `chat()` is plain inference again — the
decorator owns admission. The budget math (`budgeted_max_num_seqs`,
`ConcurrencyBudget`, `per_seq_prefill_peak`) moved to `concurrency` and is reusable
by any in-process backend. Admission env renamed to generic `ROZUM_ADMIT` /
`ROZUM_ADMIT_FASTLANE_TOKENS` / `ROZUM_ADMIT_QUEUE_MAX`. `build_gateway_backend`
routes every selected backend through `admit_wrap`. 13 concurrency unit tests on
the default build (no Xcode); feature build + fmt clean. The new mlx-rs backend is
the first intended consumer: implement inference + return a capacity, get
admission/fast-lane/backpressure/breaker for free.

## concurrency-load-shedding — backpressure + OOM circuit breaker (Phase D)
Completed: 2026-06-11
Final phase of `mistralrs-concurrency-scheduling`. `AdmissionScheduler.admit`
now returns `Result<AdmitGuard, AdmitError>`: a full wait queue
(`ROZUM_MISTRALRS_QUEUE_MAX`, default 32, 0=unbounded) sheds with `Overloaded`.
`MistralrsBackend::chat()` acquires the slot before returning the stream, so an
overloaded backend surfaces as a genuine HTTP 429 + `Retry-After` (new
`ModelError::Overloaded`, mapped in the gateway for both the OpenAI and Anthropic
dialects). Circuit breaker: `trip()` lowers the live admission limit (floor 1) on
a detected Metal allocation failure and a 30 s cooldown `recover_step()` raises
it back toward capacity; the OOM'd request is surfaced (not auto-retried, to
avoid re-OOM) and detection is best-effort substring matching. Per-class
`max_tokens` was dropped as redundant (cost already weights `max_tokens`). 7
scheduler unit tests (no Xcode); feature build + fmt clean. This completes the
concurrency feature (A+B+C+D); follow-ups — chiefly `concurrency-engine-yield`
for true mid-prefill interleaving — are in BACKLOG.

## concurrency-admission — admission scheduler + fast lane (Phase B+C)
Completed: 2026-06-11
Second phase of `mistralrs-concurrency-scheduling`. New engine-agnostic
`src/mistralrs_admission.rs`: an `AdmissionScheduler` that gates actual
concurrency in front of the static engine `max_num_seqs`, with a runtime
`set_limit` (for Phase D), shortest-job-first queue ordering, and one reserved
fast-lane slot so short interactive requests jump ahead of queued big ones.
`admit(RequestCost) -> AdmitGuard`; the guard is held for the whole `chat()`
stream and releases the slot on completion/disconnect, waking the next waiter
(dead/cancelled waiters are skipped and their slot reclaimed). Config from
`ROZUM_MISTRALRS_ADMIT` (limit ≤ capacity) and `ROZUM_MISTRALRS_FASTLANE_TOKENS`
(default 1024, 0 off). 5 async unit tests, no Xcode needed; feature build clean.

Finding recorded: the fork does **not** yield between prefill chunks (chunking
is internal to `pipeline::step`), so the fast lane gives admission-order
responsiveness but not mid-big-prefill preemption — engine-yield filed as
`concurrency-engine-yield` in BACKLOG. Phase D (backpressure + circuit breaker)
remains.

## concurrency-budget — load-time budgeted engine max_num_seqs (Phase A)
Completed: 2026-06-11
First phase of `mistralrs-concurrency-scheduling`. Replaces the total-`hw.memsize`
1/2 ladder with a footprint budget: `budgeted_max_num_seqs(ConcurrencyBudget)`
(pure, in the lib) returns `clamp((0.8·available − weights − kv_pool) /
per_seq_peak, 1, ceiling)`, where `per_seq_peak = prefill_chunk × ~465 KB/token`
(constant under chunked prefill) and `ceiling` defaults to 8 (Metal is one GPU —
past a handful of concurrent prefills you gain tail latency, not throughput).
`resolve_max_num_seqs` in `main.rs` gathers the footprint from the existing
preflight helpers and applies env overrides (`ROZUM_MISTRALRS_MAX_SEQS` forces,
`ROZUM_MISTRALRS_SEQS_CEILING` caps, `MISTRALRS_PREFILL_CHUNK` sizes the per-slot
cost), logging a `concurrency_budget` obs event. `MistralrsOptions::default()`
now carries a plain serialised floor of 1. 6 lib unit tests (no Xcode), feature
build clean. Phases B+C (admission scheduler + fast lane) and D (backpressure +
circuit breaker) remain in SPRINT.md.

## mistralrs-adaptive-concurrency — memory-adaptive default for max_num_seqs
Completed: 2026-06-11
The mistralrs backend's concurrent-prefill cap (`max_num_seqs`) default is no
longer a fixed `1`. A new pure `default_max_num_seqs(total_ram)` policy keeps
the serialised `1` floor on the 24–36 GB Apple Silicon target band (where two
concurrent large-prompt prefills can OOM the Metal command buffer) and lifts it
to `2` on machines with ≥ 48 GB total unified memory, where PagedAttention +
chunked prefill + the disconnected-seq reaping fix make real concurrency safe.
The gate is on total `hw.memsize` rather than instantaneous free memory (which
over-predicts runtime headroom at load time). `ROZUM_MISTRALRS_MAX_SEQS`
overrides. Rationale + trade-offs documented in
`docs/specs/mistralrs-backend.md`.

## web-basic-auth — HTTP Basic Auth on the web bridge
Completed: 2026-06-06
The web bridge now requires HTTP Basic Auth for `/`, `/ws`, and `/transcript`.
The password must equal the room name; the username is unconstrained and is
used as the participant's alias in the chat. The server stamps every outgoing
`meeting.submit` with the authenticated alias regardless of any client-supplied
`name` field, so a tampered client cannot post under a different name. The
auth username is sent to the client via a new `{kind:"hello",name:...}` WS
envelope right after connect; the page-side name input is removed.

## tui-soft-wrap — soft-wrap long input lines in the TUI
Completed: 2026-06-06
Custom render of the input area: `tui-textarea 0.7` still holds the data and
processes input events, but its renderer is bypassed. `draw_input` builds
visual rows by wrapping each logical line at `inner_width` and places the
cursor manually via `f.set_cursor_position`. Autosize now counts wrapped
visual rows, so a single long line grows the input chunk upward instead of
scrolling horizontally.

## mcp-proxy-auto-mark — auto-emit mark_responding from mcp-proxy
Completed: 2026-06-06
`ProxyState` gained a `heartbeat_task` handle. When `meeting.wait_my_turn`
returns `your_turn:true`, the proxy fires an immediate `meeting.mark_responding`
and spawns a background task that refreshes it every 15 s. The task is aborted
on the agent's next `submit`/`leave` and on a fresh `your_turn:true` (which
restarts the heartbeat). Manual `meeting.mark_responding` calls from the agent
still work and refresh the timer identically.

## mcp-proxy-reconnect — transparent reconnect of mcp-proxy after rozum restart
Completed: 2026-06-06
`ProxyState` remembers the joined room name; `call_room_tool` now
catches transport failures and calls a new `try_reconnect_current_room`
that sleeps a capped backoff (`200ms…5s`, ~18 s total) waiting for the
Unix socket to reappear, reconnects, re-issues `_join_internal` with
the same display name, and retries the original tool call. The agent's
MCP session no longer sees `Transport closed` during a `rozum --room R`
restart.

## room-transcript-persist — room transcript persisted across rozum restarts
Completed: 2026-06-06
`Meeting` gained `persist_path: Option<PathBuf>` and an
`enable_persistence` method that loads
`$XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl` on
construction and re-numbers seq. `post_submission` appends every Turn
as one JSON line. A new top-level `--no-persist` flag disables both
(independent of the existing `rozum web --no-persist`). Web bridges
pick up the loaded history through their normal
`wait_my_turn(since_seq:0)` path. With `rozum --room R` the same room
name reopened after a restart resumes with full transcript intact.

## web-transcript-persist — bridge transcript persisted to disk
Completed: 2026-06-06
The web bridge now appends every `msg` envelope to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl` (one JSON line per
turn). On startup the bridge loads the last `TRANSCRIPT_CAP=2000` lines back
into the in-memory ring so a page reload after a rozum restart still shows
recent history. A new `--no-persist` flag on `rozum web` disables both the
write and the load. Client-side deduplication now keys on `(seq, ts)` so
persisted entries from earlier sessions — where seq numbering restarts — do
not collide with current-session entries.

## web-transcript-history — transcript replay on connect + lazy older-history paging
Completed: 2026-06-06
The web bridge keeps a bounded in-memory transcript ring (cap 2000). A new
`GET /transcript?from_seq=&limit=` REST endpoint returns slices for paging.
On WebSocket connect the bridge sends a `kind:"history"` envelope with the
last 200 entries; the client replays them through the normal append path with
seq-based deduplication. Scrolling within 60 px of the log top triggers a
fetch of the next older 200 entries and prepends them while preserving the
viewport. `web-transcript-persist` (separate slug) will lift the in-memory
2000 cap by reading from `transcript.jsonl`.

## tui-arrow-scroll — Arrow Up/Down always scrolls the transcript
Completed: 2026-06-06
Dropped the `textarea.lines().len() <= 1` guard so the Up/Down arrows scroll
transcript history even when the input area is multi-line. Textarea cursor
navigation moves to `Ctrl+Arrow` / `Home` / `End`. Per operator request.

## tui-autosize-input — TUI input area grows with multi-line composition
Completed: 2026-06-06
Replaced fixed `Constraint::Length(3)` with a dynamic
`(textarea.lines().len() + 2).clamp(3, max(3, area.height/3))` so the input
area grows upward when the user enters multi-line content via `Alt+Enter`.
Up/Down arrows now scroll the transcript history (in addition to PgUp/PgDn).
Soft-wrap of a single overflowing line is **not** in this slug — split into
`tui-soft-wrap` because `tui-textarea 0.7` has no native wrap.

## web-scrollback-sticky — sticky-bottom scroll, "↓ N new" pill, long-message collapse
Completed: 2026-06-06
`#log` now tracks `data-stick` on scroll; new messages auto-scroll only when
the user is within 40 px of the bottom, otherwise a sticky `↓ N new` pill
appears and clicking it snaps to bottom. Messages whose body exceeds 6 wrapped
lines or 600 characters render collapsed with an `[expand ▾]` / `[collapse ▴]`
toggle. Pure client-side change in `src/web/index.html`.

## web-presence-row — presence row, joined/left, tagged envelopes for the web bridge
Completed: 2026-06-06
`src/web/mod.rs` `room_loop` now emits tagged JSON envelopes
(`kind:"msg"|"presence"|"joined"|"left"`) instead of raw transcript JSON.
`src/web/index.html` dispatches on `env.kind`: presence line above the input
with `✏️` / `⏳` glyphs, header chips for participants, dim system lines for
join/leave. Display names are rendered with `textContent` (no innerHTML) so
they cannot inject HTML.

## web-autosize-input — Claude-style autosizing textarea in the web client
Completed: 2026-06-06
Replaced the single-line `<input id="msg">` with a `<textarea rows="1">` that
grows upward on input up to `30vh` (`20vh` on mobile). `Enter` sends,
`Shift+Enter` inserts a newline, `Esc` clears, no horizontal scroll, collapses
back to one row after send. Verified live by the operator.
