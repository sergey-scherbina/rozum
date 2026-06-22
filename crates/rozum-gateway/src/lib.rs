// rozum-gateway — the outward-facing serving layer (the OpenAI/Anthropic-compatible
// HTTP gateway + the upstream HTTP provider shims). Extracted from the `rozum`
// monolith as part of Phase 3 of the workspace split (docs/specs/workspace-split.md).
// Module names are preserved and the upstream `rozum-core` / `rozum-models` modules
// it uses are re-exported into this crate's root so the moved files'
// `crate::backend::…` / `crate::obs::…` / `crate::models::…` paths resolve unchanged.
// It reads MLX telemetry through `crate::obs` (the rozum-core hook), never the engine.
pub mod anthropic_http;
pub mod gateway;
pub mod openai_http;

pub(crate) use rozum_core::{backend, concurrency, obs, share};
pub(crate) use rozum_models::{models, resident};
