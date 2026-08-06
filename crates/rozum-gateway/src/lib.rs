// rozum-gateway — the outward-facing serving layer (the OpenAI/Anthropic-compatible
// HTTP gateway + the upstream HTTP provider shims). Extracted from the `rozum`
// monolith as part of Phase 3 of the workspace split (docs/specs/workspace-split.md).
// Module names are preserved and the upstream `rozum-core` / `rozum-models` modules
// it uses are re-exported into this crate's root so the moved files'
// `crate::backend::…` / `crate::obs::…` / `crate::models::…` paths resolve unchanged.
// It reads MLX telemetry through `crate::obs` (the rozum-core hook), never the engine.
pub mod anthropic_http;
pub(crate) mod codex_patch;
pub mod control;
pub mod defaults;
pub mod errors;
pub mod paths;
pub mod private_store;
pub mod view_tokens;
pub mod matrix;
pub mod switchboard;
pub mod gateway;
pub(crate) mod codex_lean;
pub(crate) mod loopbreak;
pub(crate) mod oai_api;
pub(crate) mod anthropic_api;
pub(crate) mod responses_api;
pub mod openai_http;

pub(crate) use rozum_core::{backend, concurrency, obs, share, shed};
pub(crate) use rozum_models::{models, resident};
