# Rozum Global Spec

## Purpose

Rozum is a local runtime for live agents, models, and a human operator. A
dedicated meeting daemon hosts many disk-backed meeting rooms; a separate model
gateway serves local models (unchanged); together they form one collaborative
dev environment. External CLI agents join rooms through MCP, and the human
attaches a TUI client to any room. Spec: `docs/specs/agent-meetings-daemon.md`.

## Runtime Contract

- The project exposes a Rust library crate and a binary with the same package name, `rozum`.
- Bare `rozum` attaches a TUI client to a daemon-hosted meeting room (a room
  picker when launched without project context); the TUI itself does not run
  model inference.
- Meeting rooms live in a dedicated meeting daemon (separate from the model
  gateway, which is unchanged) — many rooms per process, not one process per
  room — and are disk-backed in the project at `.rozum/room/` (append-only
  per-day transcript logs). The meeting daemon is the single writer; local
  clients read transcripts directly from disk. Spec:
  `docs/specs/agent-meetings-daemon.md`.
- There is no turn-taking and no moderator. Any participant may submit at any
  time; messages are posted immediately, in arrival order. There is no
  round-robin, no turn expiry, no speaker scheduling, and no manual or
  LLM-backed moderation.
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
  network dependency, model file dependency, or local inference dependency.
- Meeting rooms are disk-backed by default (append-only transcript logs); this
  is the only persistence and carries no model/network dependency. Spec:
  `docs/specs/agent-meetings-daemon.md`.
- The GGUF catalog is metadata only unless a developer explicitly opts into
  local model features.
- New capabilities must be described in a feature spec before implementation.
- Runtime behavior changes must keep this global spec and the relevant feature specs in sync.

## Local LLM hosting (optional, parallel to meeting-room runtime)

- `rozum gateway` exposes the active `ChatBackend` as a local HTTP server speaking OpenAI Chat Completions and Anthropic Messages dialects on `127.0.0.1`. Spec: `docs/specs/api-gateway.md`.
- `rozum launch <program>` starts a gateway, sets `ANTHROPIC_*` and `OPENAI_*` env vars on the child, and runs the agent CLI (Claude Code, Codex, aider) already connected to a local model — without touching the user's OAuth credentials. Spec: `docs/specs/launch-wrapper.md`.
- The model-serving gateway is a **shared, single-instance, detached process**: multiple `rozum launch` clients discover and reuse one resident model (single-owner election via TCP-port bind + advisory lock, transparent failover on the same stable port, idle shutdown via client leases). Each launch runs a small model-free local proxy in the request path that absorbs daemon restarts — replaying a request when the daemon dies before the first streamed token, refusing crash-looping "poison" prompts, and retrying with backoff that honors backpressure. This makes the resident model/backend swappable transparently (`rozum gateway switch` / `reload` / `unload`: in-place drain → unload → load, no second model resident). `--model` is optional — omitted reuses a running gateway or shows an interactive picker (cached models first); `--dedicated` opts out into a private gateway. `rozum models rm` deletes a cached model. Spec: `docs/specs/shared-gateway.md`.
- Default model resolution chain for `rozum gateway` / `rozum launch` (highest first): **in-process native MLX** (`--features mlx-native`, on by default — the primary in-process backend, covering the Qwen3 / Qwen3.6 / Qwen2(.5) / Llama families with a full native MLX forward, no Python; auto-downloads an MLX snapshot from HuggingFace or ModelScope when not cached), **in-process GGUF** (`--features gguf`, **now in the default build** — the GGUF/llama.cpp fallback for local `.gguf` files, `lmstudio:<repo>`, and `ollama:<name>[:<tag>]` cached blobs), in-process MLX via `mistralrs` (`--features mistralrs`, opt-in broader-catalog candle fallback), LM Studio HTTP, then (opt-in) the Python `mlx_lm.server` HTTP backend — only when `ROZUM_MLX_HTTP` is set (default port 8080) — and finally `ROZUM_BACKEND_URL` env. The `mlx_lm.server` step is off the default path (superseded by the native runtime) but available: set `ROZUM_MLX_HTTP`, or force it with `--backend mlx-server`. Note: an Ollama model now requires an explicit `ollama:` prefix (`ollama:qwen3:8b`); a bare `name:tag` is no longer auto-interpreted as Ollama. If none reachable, both subcommands exit with code 1 rather than serving a placeholder.
- `rozum launch --backend-url <URL>` (CLI equivalent of `ROZUM_BACKEND_URL`) points the agent at an external OpenAI-compatible server — e.g. Ollama (`http://localhost:11434/v1`), vLLM, any `/v1` endpoint. It **forces** that backend (skips the local GGUF/MLX chain) and runs a lightweight in-process gateway (no shared daemon, no model load); the upstream model name comes from `--model` (e.g. `--model qwen3:8b`).
- The native MLX runtime is pure Rust on the vendored `mlx-lm` fork (`.vendor/mlx-lm`): MLX `Array`s are `!Send`, so the model is owned for life by a dedicated worker thread; greedy output is validated byte-for-byte against Python `mlx_lm`. Spec: `docs/specs/mlx-native-runtime.md`.
- The in-process `mistralrs` backend serves requests under an adaptive concurrency policy: engine capacity is budgeted from the model footprint vs available unified memory, and a rozum-side admission scheduler adds shortest-job-first ordering, a reserved fast lane for short interactive requests, and bounded-queue backpressure. Defaults must stay safe on the 24–36 GB target band and are never required for the default (no-`mistralrs`) build. Spec: `docs/specs/mistralrs-concurrency-scheduling.md`.
