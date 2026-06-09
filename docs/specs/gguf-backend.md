# GGUF Backend (In-Process, Metal)

## Goal

Run GGUF models natively in the rozum process using Metal GPU acceleration on Apple Silicon. The backend must support tool-use, long context (≥ 32 K tokens), prompt-prefix caching, real token-by-token streaming, and per-token cancellation — all required for Claude Code and Codex as consumers. It reads GGUF files from absolute paths or from the LMStudio download directory.

## Scope

- `src/gguf.rs` — `GgufBackend` struct + `impl ChatBackend`.
- `src/gguf.rs` — path resolvers (`resolve_model_path`, `resolve_lmstudio_model`).
- `Cargo.toml` — optional crate feature `gguf` that adds `llama-cpp-2` (with Metal) as a dependency.
- `src/backend.rs` — `BackendEngine::Gguf`, `BackendConfig::gguf(...)`, branch in `BackendRegistry::from_configs`.

## Interface

```rust
// Feature gate: #[cfg(feature = "gguf")]

pub struct GgufOptions {
    pub n_ctx:        u32,   // default 32_768; env ROZUM_GGUF_N_CTX
    pub n_gpu_layers: i32,   // default i32::MAX (all on Metal); env ROZUM_GGUF_GPU_LAYERS
    pub n_batch:      u32,   // default 512
    pub flash_attn:   bool,  // default true
}

impl Default for GgufOptions { ... }

pub struct GgufBackend { /* private */ }

impl GgufBackend {
    pub fn new(model_path: PathBuf, opts: GgufOptions) -> ModelResult<Self>;
}

impl ChatBackend for GgufBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    fn context_window(&self) -> u32; // returns n_ctx at load time
}

// Path resolvers — available without the gguf feature
pub fn resolve_model_path(spec: &str) -> Option<PathBuf>;
// spec forms:
//   "/absolute/path/to/model.gguf"
//   "lmstudio:<user>/<repo>"      e.g. "lmstudio:Qwen/Qwen2.5-Coder-32B-Instruct-GGUF"

// BackendConfig convenience constructor
impl BackendConfig {
    pub fn gguf(id: &str, model_spec: &str) -> Self;
    pub fn gguf_with_opts(id: &str, model_spec: &str, opts: GgufOptions) -> Self;
}
```

## Behavior

- [x] Path resolvers (`resolve_model_path`, `resolve_lmstudio_model`) are implemented in `src/gguf.rs` and tested with unit tests.
- [x] `GgufBackend::new` loads the model from the resolved path exactly once per instance. Subsequent `chat()` calls reuse the loaded weights.
- [ ] `GgufBackend::new` uses `n_gpu_layers = i32::MAX` by default, placing all layers on Metal.
- [ ] `GgufBackend::new` enables flash attention (`flash_attn = true`) to reduce KV-cache memory.
- [ ] `GgufBackend::new` logs a warning and clamps `n_ctx` if the requested value exceeds the model's `n_ctx_train`.
- [ ] `GgufBackend::new` extracts the chat template from GGUF metadata when available; falls back to `minijinja` template rendering with a bundled Qwen-hermes template.
- [ ] `ChatBackend::chat` applies the chat template to `req.messages` and any `req.tools` before tokenisation.
- [ ] Tool definitions in `req.tools` are formatted as a JSON tools-array and injected into the system prompt in Qwen hermes style (`<tools>…</tools>`).
- [ ] The backend spawns a `tokio::task::spawn_blocking` worker that runs prefill + decode. Tokens are sent over `mpsc::channel`; the stream is exposed as `tokio_stream::wrappers::ReceiverStream`.
- [ ] The worker checks `req.cancel.is_cancelled()` after each decoded token. If cancelled, it emits `Done { stop_reason: Cancelled }` and exits.
- [ ] Dropping the returned `ChatStream` before `Done` sends a cancel signal; the worker exits within one decode step.
- [ ] Plain text tokens are emitted as `TextDelta` events.
- [ ] When the model outputs a `<tool_call>` opening token (Qwen-hermes format), the backend transitions to tool-collection mode: subsequent tokens are buffered and emitted as `ToolUseDelta { id, input_json_delta }` events. On the closing token the backend emits `ToolUseEnd { id }` and `Done { stop_reason: ToolUse }`.
- [ ] `ToolUseStart { id, name }` is emitted before the first `ToolUseDelta` of each tool call. The `name` is extracted from the opening JSON fragment.
- [ ] Prompt-prefix cache: when `req.session_id` is `Some`, the backend stores the KV state keyed by session id after each request. On the next request for the same session, only the divergent suffix is re-prefilled (the common prefix is preserved in the KV cache).
- [ ] `resolve_model_path("lmstudio:Qwen/Qwen2.5-Coder-32B-Instruct-GGUF")` returns the first GGUF file under `~/.cache/lm-studio/models/Qwen/Qwen2.5-Coder-32B-Instruct-GGUF/` that matches `ROZUM_GGUF_QUANT_PREF` (default `Q4_K_M`); falls back to the largest-K quantization available if the preferred one is absent.
- [x] `BackendConfig::gguf("id", "/abs/path.gguf")` or `"lmstudio:<repo>"` in `BackendRegistry::from_configs` calls `resolve_model_path`, creates `GgufBackend::new`, and returns a `PlaceholderBackend` with a diagnostic message if resolution or load fails.
- [ ] `cargo check --features gguf` succeeds (requires cmake and clang for llama-cpp-2).
- [ ] `cargo check` (no flags) succeeds; the default build does not reference `llama-cpp-2`.
- [x] Unit tests for path resolvers pass against a temporary directory tree mimicking the LMStudio layout.
- [ ] Unit test for tool-use token parser passes on synthetic Qwen-hermes token sequences without a real GPU.
- [ ] Integration smoke test (under `#[ignore]`) loads a real GGUF on an M-series Mac and measures: load time ≤ 15 s, RSS ≤ model-size + 25%, ≥ 25 tok/s.

## Out of scope

- Non-Qwen chat templates in the tool-use parser. (Llama-3 / Mistral formats: see `gguf-tool-use-non-qwen` in BACKLOG.)
- Multi-GPU or CPU-only configurations. (`ROZUM_GGUF_GPU_LAYERS=0` works but is not a priority path.)
- Fine-tuning, LoRA adapters.
- Streaming from the LlamaGgufCommand subprocess (the old command backend is kept as-is until this backend is stable).

## Design

`GgufBackend` keeps a single `LlamaModel` and spawns a new `LlamaContext` per `chat()` call (or reuses a pooled context if the API supports it). This avoids cross-call state leakage while still benefiting from the loaded model weights.

Prompt-cache is implemented at the context level: the backend saves the KV-state pointer after completing a request and checks on the next request whether the new prompt shares a prefix. Only the differing suffix is re-prefilled. This is the single largest latency win for repeated agentic turns where the system prompt is stable.

The tool-use parser is a state machine over raw token text, not over decoded JSON. It fires on the hermes `<tool_call>` sentinel and accumulates until the matching close. This avoids buffering the entire response before emitting events.

`llama-cpp-2` is gated behind the `gguf` feature. The build requires cmake and clang (for the C++ core of llama.cpp). This is documented in the README and in `docs/models.md`.

## Decisions

- **llama-cpp-2 FFI instead of a subprocess** — chosen for in-process KV-cache reuse, real per-token streaming, and cancel-between-tokens. Rejected: keeping the subprocess backend as the primary path, because cold-start on each call is 10–30 s on a 30B model.
- **llama-cpp-2 instead of Candle-Metal** — chosen because llama.cpp has broader model support (including Qwen3-30B-A3B MoE), more mature Metal kernels, and native GGUF reading. Rejected: Candle-Metal, because MoE support and Metal optimisation lag behind.
- **Native MLX deferred to PR3** — chosen to keep PR1 scope achievable. MLX would require porting model architectures in Rust from scratch. Rejected: doing it now, because the 10–15% throughput delta over llama.cpp-Metal does not justify the risk before baseline measurements.
- **Qwen-hermes tool format** — chosen because the two recommended models (Qwen2.5-Coder-32B, Qwen3-30B-A3B) both use it. Other families (Llama-3.1) are documented as untested.

## Recommended Models

| Model | Spec | Size (Q4_K_M) | Notes |
|-------|------|---------------|-------|
| Qwen2.5-Coder-32B-Instruct | `lmstudio:Qwen/Qwen2.5-Coder-32B-Instruct-GGUF` | ~19 GB | Best open coding model; native tool-use |
| Qwen3-30B-A3B-Instruct (MoE) | `lmstudio:Qwen/Qwen3-30B-A3B-Instruct-GGUF` | ~17 GB | MoE activates ~3B params/token; faster decode |

Both fit comfortably in 36 GB unified memory on M4 Pro with 32 K context.

## Results

`src/gguf.rs` implemented with:
- Path resolvers for absolute path and `lmstudio:<repo>` — unit tests pass.
- `ToolUseParser` state machine for Qwen-hermes `<tool_call>…</tool_call>` format — 3 unit tests pass.
- `format_qwen_prompt` chat template formatter — 2 unit tests pass.
- `GgufBackend` struct and `impl ChatBackend` (under `#[cfg(feature = "gguf")]`) using `llama-cpp-2 >=0.1.100` with Metal feature.
- `BackendEngine::Gguf` wired into `BackendRegistry::from_configs`.

**Build requirement**: `cargo check --features gguf` requires `cmake` and Xcode Command Line Tools.
Install: `brew install cmake`.

The Rust code under `#[cfg(feature = "gguf")]` cannot be compile-verified without cmake (required to build llama.cpp C++ core). The llama-cpp-2 API calls follow v0.1.146 patterns; may require minor adjustments after first `cargo check --features gguf` with cmake installed.
