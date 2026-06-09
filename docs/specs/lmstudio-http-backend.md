# LM Studio HTTP backend

## Goal

Auto-detect a running LM Studio local server at `http://localhost:1234/v1` and route `rozum gateway` / `rozum launch` traffic to it as an OpenAI-compatible backend. Unlocks Qwen3.6 (and any other MLX model LM Studio ships) on Apple Silicon **today**, ahead of the in-process mistralrs AFQ work landing.

## Scope

- `src/openai_http.rs` — new `try_lmstudio_http()` probe + adapter.
- `src/main.rs` — slot it into the `build_gateway_backend` priority chain between in-process mistralrs and `mlx_lm.server`, and mention it in `print_no_backend_hints`.

## Interface

```rust
/// Default endpoint: http://localhost:1234/v1
/// Override:        env ROZUM_LMSTUDIO_HTTP=http://host:port/v1
pub async fn try_lmstudio_http(model_spec: &str) -> Option<Arc<dyn ChatBackend>>;
```

Backend chain becomes:

1. In-process GGUF (`--features gguf`)
2. In-process native MLX via `mistralrs` (default feature)
3. **LM Studio HTTP** (port 1234) ← NEW
4. `mlx_lm.server` HTTP (port 8080)
5. `ROZUM_BACKEND_URL` env

LM Studio sits above `mlx_lm.server` because it is a GUI app most macOS users already have for model management, while `mlx_lm.server` requires `pip install mlx-lm` + a manual `python -m mlx_lm.server ...`.

## Behavior

- [x] `try_lmstudio_http(spec)` probes `GET /v1/models` at `localhost:1234` with a 3 s timeout. Returns an `OpenAiHttpBackend` wired to the LM Studio endpoint on success, `None` on connection-refused / timeout / non-200.
- [x] `ROZUM_LMSTUDIO_HTTP=http://host:port/v1` overrides the endpoint (parity with `ROZUM_MLX_HTTP`, `ROZUM_OLLAMA_HTTP`).
- [x] When selected, prints `backend: LM Studio at http://localhost:1234/v1 (model: <spec>)` to stderr so the user sees which path the request took.
- [x] `print_no_backend_hints` documents the LM Studio install path (download app → install model from Search tab → start server in Developer tab → run rozum launch).
- [x] No new dependencies. Reuses the existing `OpenAiHttpBackend` SSE parser + tool-call mapping that already works against Ollama / mlx_lm.server / vLLM / OpenAI.

## Out of scope

- Auto-starting LM Studio if it isn't running. User opens the app themselves.
- Driving LM Studio's model-load API (it has one) to download/load models on demand. Phase 2 if useful.
- Direct file-system access to LM Studio's GGUF cache (`~/.cache/lm-studio/models/`) — that's the separate `lmstudio:<repo>` spec served by the in-process GGUF backend, not this one.

## Design

LM Studio's local server is OpenAI Chat Completions compatible (since LM Studio 0.2+, well documented). Our `OpenAiHttpBackend` already speaks that dialect, so the entire integration is a 30-line probe function plus one line in the dispatcher.

The model id passed in `--model` must match the id LM Studio displays in its Developer tab (e.g. `qwen/qwen3.6-35b-a3b` or whatever LM Studio chose for the local install). LM Studio echoes the same id back in chat-completion responses, so the gateway's `/v1/models` discovery presents the correct name to Claude Code.

## Decisions

- **Probe `/v1/models`, not `/health`** — chosen because `/v1/models` is the OpenAI-compatible probe LM Studio's own docs use, and our shared `OpenAiHttpBackend::probe()` already targets it. Skipping any LM-Studio-specific paths keeps the adapter trivial.
- **Insert above `mlx_lm.server`** — chosen because LM Studio is a GUI app most macOS LLM users already have running, while `mlx_lm.server` is a CLI install. Order encodes "most-likely-already-running" first.

## Results

Implemented in one pass. `cargo check` clean. Manual smoke once LM Studio is launched with a model: gateway log prints `backend: LM Studio at http://localhost:1234/v1`, chat completions stream end-to-end via the shared OpenAiHttpBackend SSE parser.
