# Hello Response

## Overview

The initial model capability is a deterministic hello response. This establishes a tiny stable runtime contract that can be expanded later without guessing how callers should interact with the model.

## Interface

- Library: `rozum::AiModel::new() -> AiModel`
- Library: `AiModel::respond(&self, input: &str) -> &'static str`
- Library: `AiModel::new()` preserves the deterministic smoke response.

## Behavior

- [x] Calling `respond` with an empty input returns exactly `hello!`.
- [x] Calling `respond` with any non-empty input still returns exactly `hello!`.
- [x] Library smoke tests cover the deterministic response.

## Out of scope

- Natural language understanding.
- State, memory, tools, skills, or multi-turn conversations.
- Model weights, embeddings, local inference acceleration, or external AI APIs.

## Design

The model is represented by a zero-sized Rust type for now. The public API is deliberately small: construction plus a single response method. This keeps future extensions free to add state or configuration without changing the initial caller path.

## Decisions

- **Library-only smoke API** - chosen so tests can keep a deterministic model-adapter baseline while the binary remains focused on meetings. Rejected: CLI smoke command, because the user-facing binary should only manage meetings.

## Results

Verified with `cargo test`.
