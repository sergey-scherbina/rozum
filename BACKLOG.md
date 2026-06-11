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
