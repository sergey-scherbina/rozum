# Chat Backend SPI

## Goal

Replace the synchronous `InferenceBackend::generate(&str) -> String` with an async, streaming, tool-capable `ChatBackend` trait. The new SPI is the foundation for in-process GGUF inference, the outward API gateway, and future native MLX execution. Tool-use support is a first-class requirement because Claude Code and Codex agents require it to function.

## Scope

- `src/backend.rs` — new types, trait, and helper; deletion of the old sync trait and all its implementors/call-sites.
- All existing backends (Hello, Placeholder, Candle, LlamaGgufCommand) adapted via `spawn_blocking` wrappers.
- `BackendOrchestrator` (Fallback / FanOut / Single) rewritten onto async streams.
- `Cargo.toml` — new async-infrastructure dependencies (non-optional).

## Interface

```rust
pub enum Role { System, User, Assistant, Tool }

pub enum ContentBlock {
    Text       { text: String },
    ToolUse    { id: String, name: String, input: serde_json::Value },
    ToolResult { tool_use_id: String, content: String, is_error: bool },
}

pub struct Message { pub role: Role, pub content: Vec<ContentBlock> }

pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value, // JSON Schema object
}

#[derive(Default)]
pub struct SamplingParams {
    pub temperature:    Option<f32>,
    pub top_p:          Option<f32>,
    pub top_k:          Option<u32>,
    pub repeat_penalty: Option<f32>,
    pub seed:           Option<u64>,
    pub max_tokens:     Option<u32>,
}

pub struct ChatRequest {
    pub messages:   Vec<Message>,
    pub tools:      Vec<ToolDef>,
    pub sampling:   SamplingParams,
    pub cancel:     tokio_util::sync::CancellationToken,
    pub session_id: Option<String>, // prompt-cache hint for backends that support it
}

pub enum StopReason { EndTurn, MaxTokens, ToolUse, Cancelled }

pub enum ChatEvent {
    TextDelta    { text: String },
    ToolUseStart { id: String, name: String },
    ToolUseDelta { id: String, input_json_delta: String },
    ToolUseEnd   { id: String },
    Done         { input_tokens: u32, output_tokens: u32, stop_reason: StopReason },
}

pub type ChatStream = Pin<Box<dyn Stream<Item = ModelResult<ChatEvent>> + Send>>;

#[async_trait::async_trait]
pub trait ChatBackend: Send + Sync {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream>;
    /// Maximum context length in tokens this backend accepts.
    fn context_window(&self) -> u32;
}

/// Drains a ChatStream to a single String (text deltas only).
pub async fn collect_to_string(stream: ChatStream) -> ModelResult<String>;
```

## Behavior

- [x] `HelloBackend::chat` emits one `TextDelta { text: "hello!" }` followed by `Done { EndTurn }`.
- [x] `PlaceholderBackend::chat` returns `Err(BackendUnavailable)` — no stream emitted.
- [x] `CandleBackend::chat` (feature `local-models`) concatenates text blocks from `messages`, runs sync generate in the calling async context, emits one `TextDelta` and `Done { EndTurn }`. Returns `Err` for any request containing `ToolDef` (tool-use not supported).
- [x] `LlamaGgufCommandBackend::chat` follows the same pattern as Candle. Returns `Err` for tool requests.
- [x] `collect_to_string` returns the concatenation of all `TextDelta.text` values; ignores tool events; propagates `Err` from the stream.
- [x] `BackendOrchestrator::Fallback` tries each backend in order; switches to the next when `chat()` returns `Err` or `collect_to_string` returns `Err`.
- [x] `BackendOrchestrator::Single` delegates to the single configured backend unchanged.
- [x] `BackendOrchestrator::FanOut` starts all backends concurrently via `tokio::spawn`; returns all results in policy order.
- [x] All `.generate(` call-sites in `src/lib.rs` replaced with `chat().await` + `collect_to_string`; tests are `#[tokio::test]`.
- [x] `cargo check` passes on the default feature set with no `local-models` or `gguf` flag.
- [x] `cargo test` passes (34/35; 1 pre-existing proxy test failure unrelated to SPI).

## Out of scope

- Streaming text tokens to the web UI or TUI (separate task `ui-streaming-ws-tui`).
- Real token-by-token streaming from Candle or `llama-gguf` command (separate tasks).
- In-process GGUF execution (separate spec `gguf-backend.md`).
- Outward HTTP gateway (separate spec `api-gateway.md`).
- Multimodal content blocks (images).

## Design

`ContentBlock` as a Vec in `Message` allows tool-use turn sequences without a schema change later. The `session_id` field is optional and purely advisory — backends that do not implement prefix-cache silently ignore it.

`ChatStream` is a `Pin<Box<dyn Stream>>` rather than a concrete type to avoid coupling call-sites to implementation details and to allow `async_trait`-based dispatch.

For `spawn_blocking` adapters, one `TextDelta` + `Done` is emitted synchronously once the blocking call returns. This is intentionally minimal — it preserves the previous behaviour while exposing the new interface for free, and it unblocks the gateway and GGUF work.

`BackendOrchestrator` switches on the first error only so partial responses are not silently discarded. A half-delivered response is an error, not a fallback trigger.

## Decisions

- **Tool-use in SPI from day 1** — chosen because the primary use case (Claude Code / Codex as consumers) requires it. Rejected: adding it later, because that would force a second SPI breaking-change.
- **spawn_blocking for Candle/llama-gguf adapters** — chosen as the minimum-effort bridge to preserve existing behaviour while exposing the new async interface. Rejected: immediate real streaming, because it couples the SPI migration to a larger refactor.
- **Drop == cancel** — chosen for simplicity; backends should not outlive their consumers. No separate `abort()` method.

## Results

Implemented in `src/backend.rs` + `src/lib.rs`.

- `cargo check` (default): ✓
- `cargo check --features local-models`: ✓
- `cargo test`: 34 pass, 1 pre-existing proxy failure unrelated to SPI.

New async tests added: `generates_with_default_backend`, `can_construct_from_backend`, `default_orchestrator_uses_hello_backend`, `fallback_policy_returns_first_successful_backend`, `fanout_policy_exposes_all_backend_attempts`, `missing_backend_id_is_reported_clearly`.
