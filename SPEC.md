# Rozum Global Spec

## Purpose

Rozum is a local meeting-room runtime for live agents and a human operator.
One running `rozum` process owns one named room, exposes it to external CLI
agents through MCP, and lets the human participate directly through the TUI.

## Runtime Contract

- The project exposes a Rust library crate and a binary with the same package name, `rozum`.
- Bare `rozum` launches a meeting room; it does not run model inference.
- The default moderator is deterministic round-robin.
- Active meeting turns are server-owned and may be skipped or expired so a
  joined-but-idle participant cannot permanently block the meeting loop.
- Turn expiry measures time until the active participant starts responding;
  after response start, drafting or sampling is not charged to the round-robin
  reaction timeout.
- Manual/operator moderation is supported without model inference.
- Smart or LLM-backed moderation is optional future behavior and must not be
  enabled by default.
- The legacy public model type `rozum::AiModel` remains available for smoke
  tests and optional model-adapter experiments.
- `AiModel::new()` constructs a model without I/O, heap setup, network access,
  external service calls, model file access, or local inference engine startup.
- `AiModel::respond(&self, input: &str) -> &'static str` returns the exact ASCII
  string `hello!` for every input.
- `AiModel` may be constructed from any backend implementing `rozum::InferenceBackend`.
- `InferenceBackend::generate(&self, input: &str)` is the fallible dynamic response
  path for optional inference backends.
- The built-in `HelloBackend` is the default backend and has no external model file.
- Backend selection is project-owned through `BackendOrchestrator`,
  `BackendRegistry`, `BackendConfig`, and `BackendPolicy`.
- The default runtime config must use only `HelloBackend`.
- Concrete engines such as Candle, `llama-gguf`, native Rust, external commands,
  or remote APIs are optional backend adapters behind `InferenceBackend`.
- Local model dependencies and GGUF runtime paths are optional and must not be
  required for the default build, default CLI, room startup, or meeting loop.
- GGUF model files are local runtime assets and must not be committed to the repository.

## Constraints

- The default meeting runtime has no learned weights, reasoning engine,
  persistence, network dependency, model file dependency, or local inference
  dependency.
- The GGUF catalog is metadata only unless a developer explicitly opts into
  local model features.
- New capabilities must be described in a feature spec before implementation.
- Runtime behavior changes must keep this global spec and the relevant feature specs in sync.

## Local LLM hosting (optional, parallel to meeting-room runtime)

- `rozum gateway` exposes the active `ChatBackend` as a local HTTP server speaking OpenAI Chat Completions and Anthropic Messages dialects on `127.0.0.1`. Spec: `docs/specs/api-gateway.md`.
- `rozum launch <program>` starts a gateway, sets `ANTHROPIC_*` and `OPENAI_*` env vars on the child, and runs the agent CLI (Claude Code, Codex, aider) already connected to a local model — without touching the user's OAuth credentials. Spec: `docs/specs/launch-wrapper.md`.
- The model-serving gateway is a **shared, single-instance, detached process**: multiple `rozum launch` clients discover and reuse one resident model (single-owner election via TCP-port bind + advisory lock, transparent failover on the same stable port, idle shutdown via client leases). `--model` is optional — omitted reuses a running gateway or shows an interactive picker (cached models first); `--dedicated` opts out into a private gateway. `rozum models rm` deletes a cached model. Spec: `docs/specs/shared-gateway.md`.
- Default model resolution chain for `rozum gateway` / `rozum launch` (highest first): in-process GGUF (`--features gguf`), in-process MLX via `mistralrs` (`--features mistralrs`, planned), mlx_lm.server HTTP, `ROZUM_BACKEND_URL` env. If none reachable, both subcommands exit with code 1 rather than serving a placeholder.
- The in-process `mistralrs` backend serves requests under an adaptive concurrency policy: engine capacity is budgeted from the model footprint vs available unified memory, and a rozum-side admission scheduler adds shortest-job-first ordering, a reserved fast lane for short interactive requests, and bounded-queue backpressure. Defaults must stay safe on the 24–36 GB target band and are never required for the default (no-`mistralrs`) build. Spec: `docs/specs/mistralrs-concurrency-scheduling.md`.
