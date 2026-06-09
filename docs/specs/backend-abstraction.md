# Backend Abstraction

> **Superseded by [`chat-backend-spi.md`](chat-backend-spi.md).**
> The sync `InferenceBackend::generate` trait has been replaced with the async
> streaming `ChatBackend` trait with tool-use support.
> This document is kept as historical context.


## Overview

Rozum needs a stable internal shape before adding real local inference. This feature introduces a backend trait, preserves the current built-in hello behavior, and records tiny GGUF model metadata so the project can add a real GGUF runtime without changing the high-level model entrypoint.

## Interface

- Trait: `rozum::InferenceBackend`
- Method: `InferenceBackend::generate(&self, input: &str) -> rozum::ModelResult<String>`
- Built-in backend: `rozum::HelloBackend`
- Model wrapper: `rozum::AiModel<B>`
- Constructor: `AiModel::from_backend(backend)`
- Tiny model catalog: `rozum::TINY_GGUF_MODELS`
- Download helper: `scripts/download-tiny-model.sh`

## Behavior

- [x] `AiModel::new().respond(input)` still returns exactly `hello!` for every input.
- [x] `AiModel::new().generate(input)` returns `Ok("hello!")` for every input.
- [x] `TINY_GGUF_MODELS` includes the smallest tested GGUF candidate and a better tiny Q4 candidate with exact byte sizes.
- [x] `scripts/download-tiny-model.sh` downloads the recommended tiny GGUF file into `models/`.
- [x] GGUF files under `models/` are ignored by git.

## Out of scope

- Loading GGUF weights in-process.
- Calling `llama.cpp`, ONNX Runtime, or any other inference engine.
- Downloading model files during tests or normal CLI execution.
- Exposing model commands in the user-facing meeting CLI.

## Design

The backend trait is intentionally small and synchronous. That keeps the current embedded API simple while leaving room for later backends such as `llama.cpp`, ONNX Runtime GenAI, or a native Rust GGUF runtime.

Tiny GGUF model metadata lives in code because it is part of the optional adapter contract we want to test. Actual model files stay outside git and are fetched explicitly with a script.

## Decisions

- **Metadata before inference** - chosen because wiring a C++/GGUF runtime changes build complexity and should be isolated. Rejected: adding `llama.cpp` bindings immediately, because the base crate should stay easy to build while the API settles.
- **SmolLM2-135M Q4 as recommended tiny download target** - chosen because it is much smaller than Qwen3-0.6B while remaining more practical than Q2 quantization. Rejected: Q2 as default, because the quality loss is usually too high even though the file is smaller.

## Results

Verified with `cargo fmt --check` and `cargo test`.
