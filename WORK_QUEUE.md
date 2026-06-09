# Work Queue

Current sprint focus: (1) make Rozum a reliable local meeting room for live agents and a human operator; (2) make Rozum a local LLM provider for Claude Code and Codex via an outward OpenAI/Anthropic-compatible gateway backed by an in-process GGUF engine on Apple Silicon Metal.

## Sprint

- [ ] remote-api-backends - Add configurable OpenAI and Anthropic API backends.
  - Add backend engines for OpenAI Responses API and Anthropic Messages API.
  - Configure provider, model id, base URL, max tokens, and credential source through config.
  - Keep API keys out of committed config; support env-variable references and/or ignored local secrets.
  - Include default model entries for OpenAI ChatGPT/GPT and Anthropic Claude.
  - Do not require live API calls in normal tests.
  - Spec first: `docs/specs/remote-api-backends.md`.

- [ ] agent-meetings - Let live Claude Code and Codex sessions join a moderated meeting room.
  - `rozum` is the meeting-room agent: one process = one named room.
  - `rozum mcp-proxy` (stdio) is added once to each agent's MCP config; agents discover rooms via `rooms.list` and join with `rooms.join(name)`.
  - Human participates directly through the TUI as a first-class participant.
  - Moderator modes: round-robin and manual/operator-selected.
  - Budget control: soft per-turn warning, hard total-chars limit.
  - Hotkeys and slash commands for pause, stop, rename, kick, mode-switch.
  - Spec: `docs/specs/agent-meetings.md` + `agent-meetings-mcp.md` + `agent-meetings-process.md` + `agent-meetings-tui.md`.

- [ ] meeting-cli-surface - Keep the binary focused on meeting management.
  - Supported commands: bare room launch, `list`, `mcp-proxy`.
  - Do not expose model diagnostics through user-facing CLI commands.
  - Spec: `docs/specs/optional-local-models.md`.

- [ ] runtime-config - Load backend policy and backend list from `rozum.toml`.
  - Support `single`, `fallback`, and `fanout` policies.
  - Support backend engines already defined in code: `hello`, `candle`, `llama-gguf`, `native-rust`, `external-command`.
  - Provide a default config equivalent to the current tiny fallback plan.
  - Spec first: `docs/specs/runtime-config.md`.

- [ ] eval-harness - Add a minimal local eval runner.
  - Add `evals/smoke.toml` or `evals/smoke.json`.
  - Include greeting, summary, sentiment, JSON extraction, and simple route-intent cases.
  - Report pass/fail and observed model output.
  - Keep tests deterministic where possible.
  - Spec first: `docs/specs/eval-harness.md`.

- [ ] smollm2-chat-template - Prompt SmolLM2-Instruct with an explicit chat template.
  - Add a prompt formatting layer before backend execution.
  - Keep raw prompt mode available for debugging.
  - Verify `Hello! How are you?` still produces a sensible response.
  - Spec first: `docs/specs/smollm2-chat-template.md`.


- [x] idle-cpu-reduction - Eliminate busy-polling in TUI and room loops so rozum uses near-zero CPU when idle.
  - TUI render loop currently `poll(50ms)` + `try_recv` every 50 ms regardless of activity — replace with a select on `events_rx`, crossterm events, and a 100ms ticker for the presence timeout only.
  - Room/app loop: audit for any spin-loops or short-sleep busywaits; replace with async `tokio::select!` on actual wakeup sources (transcript_notify, broadcast channel, Unix accept).
  - Web bridge `room_loop`: already blocks on `wait_my_turn` (35 s timeout), verify no additional spin path.
  - Goal: `top`/`Activity Monitor` shows `rozum` at ~0% CPU when no messages arrive, no agents are polling, and no keys are pressed.
  - Spec first: `docs/specs/idle-cpu-reduction.md`.

- [ ] chat-backend-spi - Replace sync InferenceBackend with async streaming ChatBackend (tools, sampling, cancel).
  - New trait `ChatBackend` with `chat(req) -> Stream<ChatEvent>` and `context_window() -> u32`.
  - Content blocks (`Text` / `ToolUse` / `ToolResult`) so tool-use lives in the SPI from day 1.
  - `CancellationToken` in `ChatRequest`; drop-of-stream stops backend work.
  - Helper `collect_to_string` for meeting call-sites that still need a final `String`.
  - Adapt existing backends (Hello, Placeholder, Candle, LlamaGgufCommand) via `spawn_blocking` one-Delta-then-Done.
  - Rewrite `BackendOrchestrator` (Fallback / FanOut / Single) onto async streams.
  - Delete the old sync trait; migrate all `.generate(` call-sites to `chat().await + collect_to_string`.
  - Spec: `docs/specs/chat-backend-spi.md`.

- [ ] gguf-backend - In-process GGUF inference on Metal via llama-cpp-2 (replaces llama-gguf-command).
  - New crate feature `gguf` with `llama-cpp-2` (metal) dep; default build unaffected.
  - `GgufBackend` loads model once; `chat()` prefill+decode with sampling, tool-use parser, cancel between tokens.
  - Prompt-cache: reuse KV by `session_id` (trim only divergent tail on prefix match).
  - Path resolvers: `lmstudio:<repo>` → `~/.cache/lm-studio/models/...`, `ollama:<name>` → parse manifest → blob, or absolute path.
  - Env overrides: `ROZUM_GGUF_N_CTX`, `ROZUM_GGUF_GPU_LAYERS`, `ROZUM_GGUF_QUANT_PREF`, `ROZUM_LMSTUDIO_HOME`, `ROZUM_OLLAMA_HOME`.
  - Recommended models documented: Qwen2.5-Coder-32B Q4_K_M, Qwen3-30B-A3B Q4_K_M.
  - Integration test under `#[ignore]` runs against a real GGUF on M-series.
  - Spec: `docs/specs/gguf-backend.md`.

- [ ] api-gateway - Outward HTTP gateway (OpenAI + Anthropic dialects) for Claude Code and Codex.
  - `axum` server, bind 127.0.0.1, optional bearer via `ROZUM_GATEWAY_TOKEN`.
  - `GET /v1/models`, `POST /v1/chat/completions` (OpenAI SSE with tool_calls), `POST /v1/messages` (Anthropic event-stream with tool_use blocks).
  - Tool-use mapping in both directions (Anthropic ↔ internal, OpenAI ↔ internal).
  - Context-overflow → HTTP 400 with a clear error; no silent truncation.
  - Cancel: propagate client HTTP disconnect → `CancellationToken.cancel()`.
  - CLI: `rozum gateway --port <p> --model <spec>`.
  - E2E: Claude Code via `ANTHROPIC_BASE_URL` and Codex via `OPENAI_BASE_URL` both complete a tool-using task on the local model.
  - Spec: `docs/specs/api-gateway.md`.

- [x] launch-wrapper - `rozum launch <program>` starts gateway + sets env vars + runs the agent CLI.
  - One-shot wrapper: bind a free port, start gateway, spawn child with `ANTHROPIC_BASE_URL`, `ANTHROPIC_AUTH_TOKEN`, `ANTHROPIC_MODEL` (and `OPENAI_*` for Codex/aider).
  - Pre-selects the local model in Claude Code via `ANTHROPIC_MODEL` + the four `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` slots — no manual `/model` pick.
  - Uses `ANTHROPIC_AUTH_TOKEN` (rank-2 in Claude Code auth precedence) to outrank the user's OAuth login without `claude /logout`.
  - Enables `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` so the model shows up in `/model` picker with its real spec as `display_name`.
  - Argument reordering pre-parser supports both `rozum launch --model X claude` and `rozum launch claude --model X`; `--` separator forwards remaining args verbatim to the child.
  - Listener bound before child spawn — no startup race.
  - Spec: `docs/specs/launch-wrapper.md`.

- [ ] mistralrs-backend - In-process native-MLX backend via the `mistralrs` Rust crate.
  - New crate feature `mistralrs` with the `mistralrs` dep + Metal kernels on Apple Silicon.
  - Removes Ollama and Python (`mlx_lm.server`) from the runtime for MLX-format models.
  - Loads MLX safetensors directly: `mlx-community:<repo>` and `hf:<user>/<repo>` specs, auto-download via `hf-hub`.
  - Streaming token-by-token mapped to `ChatEvent::TextDelta` / `Done`; per-token cancel.
  - Reuses `crate::gguf::ToolUseParser` for Qwen-hermes `<tool_call>` blocks.
  - Inserts between in-process GGUF and Ollama in `build_gateway_backend` priority chain.
  - CLI: `rozum gateway --model mlx-community:Qwen2.5-Coder-32B-Instruct-4bit`.
  - Spec: `docs/specs/mistralrs-backend.md`.

## Done Criteria

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo build --release` passes.
- Bare `rozum` starts a meeting room without model inference.
- User-facing CLI commands are limited to meeting management.
- Specs for completed items have checked behavior boxes and results.
