# API Gateway (OpenAI + Anthropic Dialects)

## Goal

Expose rozum's `ChatBackend` registry as a local HTTP server that speaks both the OpenAI Chat Completions dialect and the Anthropic Messages dialect. This lets Claude Code (via `ANTHROPIC_BASE_URL`) and OpenAI Codex / aider / opencode (via `OPENAI_BASE_URL`) use any rozum backend — including the in-process GGUF backend — as their model provider without any code changes on the agent side.

## Scope

- `src/gateway.rs` — axum server with OpenAI and Anthropic routes.
- `src/main.rs` — new `rozum gateway` subcommand.
- `Cargo.toml` — optional crate feature `gateway` (no new deps beyond already-used `axum`, `reqwest`, `serde_json`).

## Interface

### CLI

```
rozum gateway [OPTIONS]

Options:
  --port <PORT>     Listen port on 127.0.0.1 [default: 8089]
  --model <SPEC>    Model spec passed to BackendConfig::gguf or other backend
                    (e.g. "/path/to/model.gguf" or "lmstudio:<user>/<repo>")
  --n-ctx <N>       Context size forwarded to the backend [default: 32768]

Environment:
  ROZUM_GATEWAY_TOKEN   Optional bearer token; if set, all requests must supply
                        "Authorization: Bearer <token>"
```

### HTTP Routes

| Method | Path | Dialect |
|--------|------|---------|
| `GET`  | `/v1/models` | OpenAI |
| `POST` | `/v1/chat/completions` | OpenAI |
| `POST` | `/v1/messages` | Anthropic |

Bind address is always `127.0.0.1`. CORS is not required (local use only).

### OpenAI Chat Completions — Request subset handled

```jsonc
{
  "model": "string",         // matched to backend id; falls back to default
  "messages": [...],         // roles: system, user, assistant, tool
  "tools": [...],            // optional; function-calling format
  "stream": true,            // always treated as true
  "temperature": 0.7,
  "top_p": 0.9,
  "max_tokens": 2048
}
```

### OpenAI Chat Completions — SSE response

Each chunk:
```jsonc
data: {"id":"...","object":"chat.completion.chunk","choices":[{"delta":{"content":"token"}}]}
```
Tool call chunk:
```jsonc
data: {"id":"...","choices":[{"delta":{"tool_calls":[{"index":0,"id":"...","function":{"name":"fn","arguments":"{\"k\":"}}]}}]}
```
Terminator: `data: [DONE]`

### Anthropic Messages — Request subset handled

```jsonc
{
  "model": "string",
  "system": "string",
  "messages": [...],
  "tools": [...],
  "max_tokens": 2048,
  "temperature": 0.7,
  "stream": true
}
```

### Anthropic Messages — SSE response events

```
event: message_start
data: {"type":"message_start","message":{"id":"...","type":"message","role":"assistant","content":[],"stop_reason":null}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"token"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

// tool_use block:
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_...","name":"fn","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"k\":"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}

event: message_stop
data: {"type":"message_stop"}
```

## Behavior

- [x] `rozum gateway --port 8089 --model "/path/to/model.gguf"` starts an HTTP server on `127.0.0.1:8089` with the specified backend loaded.
- [x] `GET /v1/models` returns `{"object":"list","data":[{"id":"<backend-id>","object":"model"},...]}`.
- [x] `POST /v1/chat/completions` with `"stream":true` returns an SSE stream as described above.
- [x] `POST /v1/chat/completions` with `"stream":false` returns a non-streaming JSON completion object.
- [x] `POST /v1/messages` returns an Anthropic-format SSE stream or synchronous response.
- [x] Both routes map `messages` and `tools` to `ChatRequest` using the types from `chat-backend-spi.md`.
- [x] Tool events from `ChatStream` serialized into the correct SSE format for each dialect.
- [x] Context overflow → HTTP 400 with `{"error":{"message":"...","type":"context_length_exceeded"}}`.
- [x] Backend overloaded (`ModelError::Overloaded`, e.g. mistralrs admission queue full) → HTTP 429 + `Retry-After` header, `type:"overloaded"`. See `mistralrs-concurrency-scheduling.md`.
- [x] Client disconnect → `CancellationToken.cancel()` via `CancelOnDrop` wrapper on stream drop.
- [x] `ROZUM_GATEWAY_TOKEN` → 401 if missing or wrong; no auth if env var absent.
- [x] Binds only to `127.0.0.1`.
- [x] Unit tests: SSE stream lengths verified (6 tests pass), message/tool/system parsing, context overflow estimate.
- [ ] E2E with real GGUF model (requires `--features gguf` + cmake + a local .gguf file).
- [ ] E2E Claude Code / Codex (requires real GGUF loaded).

## Out of scope

- Listening on a non-loopback interface (security boundary; add a flag only if requested).
- TLS / HTTPS (local use; agents connect via loopback).
- Multimodal (image) content blocks.
- Tool-choice / parallel tool-use (handled by the model; gateway passes through tool definitions unchanged).
- Request batching.
- Rate limiting.
- Anthropic `count_tokens` and `beta` endpoints.
- Remote API client backends (OpenAI HTTP client, Anthropic HTTP client) — separate BACKLOG item.

## Design

The gateway is a thin translation layer: it parses the wire format of each dialect, builds a `ChatRequest`, drives `ChatBackend::chat()`, and serialises `ChatEvent`s back into the appropriate SSE envelope. No business logic lives in the gateway.

Context overflow detection is approximate (whitespace-split token estimate), not exact tokenisation. This avoids pulling in a full tokenizer for every request and is conservative enough to catch obvious overflows before the model hangs.

Both dialects share a single internal routing function `dispatch(req: ChatRequest) -> ChatStream` so that the translation logic is tested independently of the HTTP layer.

`axum` is already a dependency from the web bridge, so no new HTTP framework is introduced.

## Decisions

- **Both dialects in one PR** — chosen because the `ChatEvent` → SSE mapping is the same work; separating them would require wiring the gateway twice. Rejected: OpenAI-only first, because Claude Code is the primary use case and it requires the Anthropic dialect.
- **stream:false supported for OpenAI** — chosen for compatibility with sync clients (some SDKs do not default to streaming). Gateway collects the stream internally and writes a single response.
- **Cancel on client disconnect** — chosen because uncontrolled generation wastes GPU time. axum detects connection drop via `on_upgrade` / body drop; the gateway propagates via `CancellationToken`.
- **HTTP 400 on context overflow** — chosen because a silent truncation would produce nonsense responses. Agents should retry with a shorter context or fewer tools.

## Results

Implemented in `src/gateway.rs` + `src/main.rs`.

- `cargo check` (default): ✓ (1 false-positive `unused_assignments` warning in async_stream loop)
- `cargo build`: ✓
- `cargo test gateway::`: 6/6 pass

**Smoke-tested** with `HelloBackend`:
```
rozum gateway --port 8089 --model hello
# GET /v1/models → {"object":"list","data":[{"id":"hello",...}]}
# POST /v1/chat/completions stream:false → {"choices":[{"message":{"content":"hello!",...}}]...}
# POST /v1/messages stream:false → {"content":[{"type":"text","text":"hello!"}]...}
# OpenAI SSE: data: chunks + data: [DONE]
# Anthropic SSE: message_start, content_block_start, content_block_delta, content_block_stop, message_delta, message_stop
```

**Production use** (with GGUF model):
```bash
cargo build --features gguf  # requires brew install cmake
huggingface-cli download Qwen/Qwen2.5-Coder-32B-Instruct-GGUF \
  qwen2.5-coder-32b-instruct-q4_k_m.gguf --local-dir ~/models
./target/debug/rozum gateway --port 8089 --model ~/models/qwen2.5-coder-32b-instruct-q4_k_m.gguf

export ANTHROPIC_BASE_URL=http://localhost:8089
export ANTHROPIC_API_KEY=dummy
claude  # Claude Code now uses local Qwen2.5-Coder via rozum
```
