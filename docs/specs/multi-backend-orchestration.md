# Multi Backend Orchestration

## Overview

Rozum must not be tied to one inference engine. Optional model paths should be able to use Candle, `llama-gguf`, a future native engine, an external process, or several of them together through one project-owned abstraction. The default meeting runtime must not require any of them.

## Interface

- Backend trait: `rozum::InferenceBackend`
- Backend engines: `rozum::BackendEngine`
- Backend config: `rozum::BackendConfig`
- Runtime config: `rozum::ModelRuntimeConfig`
- Backend registry: `rozum::BackendRegistry`
- Orchestrator: `rozum::BackendOrchestrator`
- Execution policy: `rozum::BackendPolicy`
- Detailed response: `rozum::GenerationResponse`
- Per-backend attempt: `rozum::BackendAttempt`

## Behavior

- [x] The default orchestrator uses the built-in hello backend and returns `hello!`.
- [x] Runtime config can declare Candle, `llama-gguf`, native Rust, external command, and hello backends.
- [x] Fallback policy tries configured backends in order and returns the first successful response.
- [x] Fanout policy can run all selected backends and return the first successful response in configured order.
- [x] Fanout attempts expose each backend result so future reducers can compare, rank, or vote.
- [x] Missing backend ids produce a clear `BackendNotFound` error.
- [x] Optional multi-backend preview remains available through library/configuration paths.

## Out of scope

- Real native GGUF inference.
- Async streaming.
- Voting, ranking, or answer synthesis across multiple successful outputs.

## Design

The project-owned abstraction keeps backend selection above concrete engine crates. Each concrete runtime becomes an adapter implementing `InferenceBackend`. The orchestrator owns a registry and a policy, so callers can use one model entrypoint while the runtime decides whether to call one backend, fall back through several, or fan out to several.

Candle and `llama-gguf` are real optional adapters. Candle runs in-process through Candle's quantized GGUF LLaMA loader when the `local-models` feature is enabled; `llama-gguf` runs the pure-Rust `llama-gguf` command as an external process. Native Rust and external command entries remain declarative placeholders until their adapters are implemented. This lets configuration and routing stay stable while each concrete runtime is implemented behind the same trait.

## Decisions

- **Project-owned orchestrator** - chosen so backend crates never shape Rozum's public API. Rejected: exposing Candle or `llama-gguf` types directly, because that would make later backend swaps invasive.
- **Placeholder backend configs** - chosen to make backend plans explicit before implementation. Rejected: hiding future backends until their adapters exist, because it would defer important routing design.
- **Synchronous trait for now** - chosen because the current model API is tiny and easy to test. Rejected: async trait immediately, because no current backend needs it and it would add dependencies or boxing.

## Results

Verified with `cargo fmt --check`, `cargo test`, and model smoke tests through Candle and `llama-gguf`. The user-facing CLI no longer exposes backend diagnostics.
