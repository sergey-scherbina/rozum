# Optional Local Models

## Overview

Rozum's primary product surface is the agent meeting room. Local inference stays
available as an optional adapter surface, but it must not be part of the default
build path, default room startup, default moderator, or user-facing CLI.

## Interface

- Cargo feature: `local-models`
- Default features: none
- Default runtime config: `ModelRuntimeConfig::default()` uses only `HelloBackend`
- Explicit local preview config: `ModelRuntimeConfig::tiny_multi_backend_preview()`
- User-facing CLI commands: none
- Meeting moderator modes: `round-robin`, `manual`

## Behavior

- [x] `cargo test` builds without Candle, tokenizers, or GGUF model files.
- [ ] Bare `rozum` starts a round-robin meeting room without model inference.
- [ ] `rozum --moderator manual` starts a room that waits for operator-selected speakers.
- [x] `rozum --help` exposes only meeting-management commands.
- [x] `rozum ask`, `rozum models`, `rozum backends`, and `rozum hello` are not accepted commands.
- [x] TUI mode controls expose only round-robin and manual moderation.
- [x] `/mode manual` and `/next <participant>` let the operator choose the next speaker.
- [x] `local-llm` moderator code is not compiled unless `local-models` is enabled.

## Out of scope

- Designing smart moderator speaker-selection policy.
- Making any local model a default participant or default moderator.
- Removing existing model adapter experiments.
- Exposing model adapter diagnostics in the user-facing CLI.
- Remote API backends.

## Design

The meeting runtime remains model-free by default. Optional local model code is
kept behind an explicit Cargo feature and library/configuration APIs. The manual
moderator is deterministic from the runtime's point of view: it waits until the
operator chooses a participant.

## Decisions

- **Model-free default** - chosen because the current product is agent
  coordination, not embedded inference. Rejected: using SmolLM2/Candle for the
  default moderator, because the speaker-selection policy is still undefined.
- **Manual before smart moderation** - chosen because the operator can make
  high-quality routing decisions today. Rejected: keeping `local-llm` in the TUI,
  because it looks product-ready while the selection semantics are unresolved.

## Results

Verified with `cargo fmt --check`, `cargo test`, `cargo build --no-default-features`,
`cargo check --features local-models`, `cargo tree --no-default-features | rg "candle|tokenizers|onig"`,
`cargo run --quiet -- --help`, `cargo run --quiet -- list`, and clap rejection for
`cargo run --quiet -- ask`, `models`, `backends`, and `hello`.
