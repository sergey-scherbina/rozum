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

- [x] mlx-native-compile - DEAD END (measured). `mx.compile` via mlx-rs
  `compile_with_state` is net-NEGATIVE on a model forward: probe `mlx_compile_probe`
  (dense Qwen3-4B) gives T=1 0.51x (8.79->17.34ms), T=16 0.85x. The binding
  re-marshals + sorts all ~400 params per call + `mlx_detail_compile` per call, so
  per-call overhead > fusion benefit. Decode (12 vs ~22 t/s) is FFI/per-op-overhead
  bound, not fusion-bound. The fixed-size-KV-cache prerequisite is moot, dropped.
  See SPRINT `mlx-native-perf`.

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
