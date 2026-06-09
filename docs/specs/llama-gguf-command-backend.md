# Llama GGUF Command Backend

> **Superseded by [`gguf-backend.md`](gguf-backend.md).**
> The external-process command backend is replaced by an in-process FFI backend
> (`llama-cpp-2`) with Metal acceleration, streaming, tool-use, and prompt-cache.
> This document is kept as historical context.


## Overview

The `llama-gguf` command backend uses the pure-Rust `llama-gguf` command as an external process behind Rozum's `InferenceBackend` abstraction. This gives the project a working GGUF fallback without binding the public API to `llama-gguf` internals.

## Interface

- Engine: `BackendEngine::LlamaGguf`
- Adapter: `rozum::LlamaGgufCommandBackend`
- Binary path: `ROZUM_LLAMA_GGUF_BIN`, default `.tools/bin/llama-gguf`
- Model path: `BackendConfig::model_path`
- User-facing CLI: none; model diagnostics are not part of the meeting binary

## Behavior

- [x] `BackendEngine::LlamaGguf` configs create a real command backend instead of a placeholder.
- [x] The backend runs `llama-gguf run <model> -p <prompt> -n 32 --temperature 0`.
- [x] The backend removes the echoed prompt and trailing `Generated N tokens` line from stdout.
- [x] Missing model files return a clear backend error.
- [x] Explicit local model library/configuration paths use the fallback plan and can return a generated model response.

## Out of scope

- In-process `llama-gguf` library integration.
- Candle inference.
- Streaming token output.
- Chat template management beyond raw prompt smoke testing.
- User-configurable sampling parameters.

## Design

The adapter uses `std::process::Command` without a shell. That keeps prompt handling simple and avoids coupling to command-line parsing beyond the `llama-gguf run` contract. It is intentionally isolated so a later in-process adapter can replace it behind the same engine id.

## Decisions

- **External command first** - chosen because it gives a real model response quickly while preserving the backend boundary. Rejected: directly depending on `llama-gguf` internals, because those APIs are less stable than the CLI for a first smoke test.
- **Greedy 32-token smoke output** - chosen for repeatability and speed. Rejected: default random sampling, because hello-world backend tests should be easy to reproduce.

## Results

Originally verified with model smoke paths. The user-facing CLI no longer exposes model diagnostics; future checks should exercise this adapter through library/configuration tests.

Observed smoke response:

```text
backend: llama-gguf-smollm2-q4
prompt: Hello! How are you?
answer: I'm doing great, thanks for asking. It's always a pleasure to chat with you and share our experiences together. I've been working in the tech industry
```
