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
| `GET`  | `/health` | liveness (200 while the process serves HTTP) |
| `GET`  | `/ready` | readiness (200 servable / 503 draining) |
| `GET`  | `/v1/models` | OpenAI |
| `POST` | `/v1/chat/completions` | OpenAI |
| `POST` | `/v1/messages` | Anthropic |

Bind address is always `127.0.0.1`. CORS is not required (local use only).

`/health` and `/ready` separate liveness from readiness for an orchestrator / load
balancer (see `distributed-readiness.md`): `/health` never touches the model, `/ready`
returns 503 while the instance is draining for shutdown so traffic stops being routed to it.

### OpenAI Chat Completions — Request subset handled

```jsonc
{
  "model": "string",         // matched to backend id; falls back to default
  "messages": [...],         // roles: system, user, assistant, tool
  "tools": [...],            // optional; function-calling format
  "tool_choice": "auto",     // "auto" | "none" | "required" | {"type":"function","function":{"name":"f"}}
  "response_format": {"type":"json_schema","json_schema":{"schema":{...}}},  // or {"type":"json_object"} — constrains the whole reply (native MLX)
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
  "tool_choice": {"type": "auto"},  // "auto" | "any" | "none" | {"type":"tool","name":"f"}
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

### Tool use (Contract-1) — the stable surface the agent SDK targets

This is the contract `rozum-gateway-tool-contract`: the request/response shape an SDK can
build against without reading the implementation. Conformance is unit-tested
(`gateway::tests::{tool_choice_*, oai_collect_tool_call_shape, anthropic_collect_tool_use_shape}`).

**Request — tools.** `tools[].function.{name, description, parameters}` (OpenAI/Responses) or
`tools[].{name, description, input_schema}` (Anthropic) map to the SPI `ToolDef`. `parameters` /
`input_schema` is a JSON Schema, passed through verbatim.

**Request — `tool_choice`** (normalized across dialects; honored by transforming the tool set the
backend sees — no SPI change):

| intent            | OpenAI / Responses                                   | Anthropic                      | effect                                              |
|-------------------|------------------------------------------------------|--------------------------------|-----------------------------------------------------|
| model decides     | `"auto"` / absent                                    | `{"type":"auto"}` / absent     | tools passed through (default)                      |
| no tools          | `"none"`                                             | `{"type":"none"}`              | tool set emptied → text-only reply                  |
| must call *a* tool| `"required"`                                         | `{"type":"any"}`               | accepted; **best-effort** (not forced) — tools kept |
| must call tool X  | `{"type":"function","function":{"name":"X"}}` (flat `{"type":"function","name":"X"}` for Responses) | `{"type":"tool","name":"X"}` | tool set restricted to X (empty if X undeclared)    |

**Response — non-streaming.** OpenAI:
`choices[0].message.tool_calls[] = {id, type:"function", function:{name, arguments:"<json string>"}}`,
`message.content:null` when tool calls are present, `finish_reason:"tool_calls"`. Anthropic:
`content[] += {type:"tool_use", id, name, input:<json object>}`, `stop_reason:"tool_use"`.

**Response — streaming.** OpenAI: `delta.tool_calls[].function.{name, arguments}` deltas, terminal
`finish_reason:"tool_calls"`, then `[DONE]`. Anthropic: a `tool_use` `content_block_start`, then
`input_json_delta` `content_block_delta`s, `content_block_stop`, and `message_delta` with
`stop_reason:"tool_use"`.

**`finish_reason` / `stop_reason`** (from SPI `StopReason`): `EndTurn`→`stop`/`end_turn`,
`ToolUse`→`tool_calls`/`tool_use`, `MaxTokens`→`length`/`max_tokens`.

**Argument reliability.** When the native MLX backend runs with `ROZUM_MLX_CONSTRAIN`, tool
arguments are schema-constrained *during decode* (the model cannot emit a malformed/out-of-schema
argument object). This is a server-side reliability feature, transparent to the contract — the SDK
just gets conformant `arguments`. See `constrained-tool-decoding.md`.

## Behavior

- [x] `rozum gateway --port 8089 --model "/path/to/model.gguf"` starts an HTTP server on `127.0.0.1:8089` with the specified backend loaded.
- [x] `GET /v1/models` returns `{"object":"list","data":[{"id":"<backend-id>","object":"model"},...]}`.
- [x] `POST /v1/chat/completions` with `"stream":true` returns an SSE stream as described above.
- [x] `POST /v1/chat/completions` with `"stream":false` returns a non-streaming JSON completion object.
- [x] `POST /v1/messages` returns an Anthropic-format SSE stream or synchronous response.
- [x] Both routes map `messages` and `tools` to `ChatRequest` using the types from `chat-backend-spi.md`.
- [x] `tool_choice` parsed + honored on all three routes (`auto`/`none`/`required`/named); see the Tool-use contract above. Conformance unit tests cover parsing, application, and the tool-call response shapes for both dialects.
- [x] Tool events from `ChatStream` serialized into the correct SSE format for each dialect.
- [x] Context overflow → HTTP 400 with `{"error":{"message":"...","type":"context_length_exceeded"}}`.
- [x] Backend overloaded (`ModelError::Overloaded`, e.g. mistralrs admission queue full) → HTTP 429 + `Retry-After` header, `type:"overloaded"`. See `mistralrs-concurrency-scheduling.md`.
- [x] Client disconnect → `CancellationToken.cancel()` via `CancelOnDrop` wrapper on stream drop.
- [x] Generation inactivity timeout: every backend stream is wrapped (`with_gen_timeout`, all dialects, streaming + non-streaming) so a wedged in-process generation can't hang a client. If no event arrives within `ROZUM_GEN_TIMEOUT_SECS` (default 180; `0` disables), the job is cancelled and the stream ends with `ModelError::Timeout` → HTTP 504. Backstop for a Metal eval that blocks inside one FFI call under memory pressure, where the per-token cancel check can't run. 3 unit tests.
- [x] `ROZUM_GATEWAY_TOKEN` → 401 if missing or wrong; no auth if env var absent.
- [x] Binds only to `127.0.0.1`.
- [x] `GET /health` (liveness) and `GET /ready` (readiness, 503 while draining) for load-balanced deploys; see `distributed-readiness.md`.
- [x] SIGTERM/SIGINT → graceful shutdown: flip `/ready` to 503, grace period, drain in-flight streams, exit (rolling-deploy safe).
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
