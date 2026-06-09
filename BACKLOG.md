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
