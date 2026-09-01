// rozum-agent — the intelligence layer (agent loop, tool dispatch, cost-tier
// cascade, small-model router / RAG worker). Extracted from the `rozum` monolith as
// part of Phase 3 of the workspace split (docs/specs/workspace-split.md). Module
// names are preserved (so internal `crate::agent::…` / `crate::router::…` paths
// resolve unchanged) and the upstream `rozum-core` / `rozum-models` modules these
// use are re-exported into this crate's root so `crate::backend::…`, the SPI types
// at the crate root (`crate::ChatBackend` …), `crate::constrain::…`,
// `crate::share::…` and `crate::models::…` resolve unchanged.
pub mod agent;
pub mod builtin_tools;
pub mod cascade;
pub mod mcp_tool_source;
pub mod memory_store;
pub mod rag_chunk;
pub mod rag_embed;
pub mod rag_mcp;
pub mod rag_lite;
pub mod router;
pub mod verify;

pub(crate) use rozum_core::{backend, constrain, share};
pub(crate) use rozum_core::backend::*;
pub(crate) use rozum_models::models;
