# Candle Backend

## Overview

Rozum should have a real in-process Rust backend for the same `SmolLM2-135M-Instruct-Q4_K_M.gguf` model already used by the `llama-gguf` command backend. Candle is an optional in-process adapter behind `InferenceBackend` and may run before command-based fallbacks only in explicit local-model preview paths.

## Interface

- Engine: `BackendEngine::Candle`
- Adapter: `rozum::CandleBackend`
- Model path: `BackendConfig::model_path`
- Optional runtime order: `candle-smollm2-q4` before `llama-gguf-smollm2-q4`
- Cargo feature: `local-models`
- Diagnostic CLI: none; model diagnostics are not part of the user-facing binary

## Behavior

- [x] `BackendEngine::Candle` configs create a real Candle backend instead of a placeholder.
- [x] Candle loads tokenizer metadata from the GGUF file.
- [x] Candle loads quantized LLaMA weights from `SmolLM2-135M-Instruct-Q4_K_M.gguf`.
- [x] Candle generates 32 greedy tokens through the same `InferenceBackend` contract.
- [x] Explicit local model library/configuration paths can return through `candle-smollm2-q4`.
- [x] `llama-gguf-smollm2-q4` remains available as fallback.

## Out of scope

- GPU acceleration.
- Streaming output.
- Chat template support.
- Sampling parameter configuration.
- Model/session eviction.

## Design

`CandleBackend` lazily loads model state on first generation and stores it behind a `Mutex`. This keeps startup cheap for commands that do not ask the model, while avoiding repeated model loads in a long-lived process.

The backend uses Candle's quantized GGUF LLaMA loader and tokenizer support from GGUF metadata. The generated logits are squeezed from `[1, vocab]` to `[vocab]` before greedy sampling.

## Decisions

- **In-process Candle first only in local preview plans** - chosen because it exercises the pure Rust embedded path directly without making local inference part of the default meeting runtime. Rejected: Candle as another command process, because that would not prove our library adapter boundary.
- **Keep `llama-gguf` fallback** - chosen because Candle is slower on CPU in the current implementation and may diverge on model compatibility. Rejected: replacing `llama-gguf`, because fallback is useful while Candle matures.

## Results

Originally verified with model smoke paths. The user-facing CLI no longer exposes model diagnostics; future checks should exercise this adapter through library/configuration tests with `--features local-models`.

Observed Candle response:

```text
backend: candle-smollm2-q4
prompt: Hello! How are you?
answer: I hope you're doing well.

I've been thinking about you a lot lately, and I'm glad you're here. I've been working on
```
