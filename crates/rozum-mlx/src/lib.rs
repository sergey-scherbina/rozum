// rozum-mlx — the pure-Rust native MLX ChatBackend engine + speculative decoding.
// Extracted from the `rozum` monolith as part of Phase 2 of the workspace split
// (docs/specs/workspace-split.md). Module names are preserved (so the moved files'
// internal `crate::specdecode::…` paths resolve unchanged) and the upstream
// `rozum-core` / `rozum-models` modules these use are re-exported into this crate's
// root so `crate::backend::…` / `crate::model_source::…` etc. resolve unchanged.
pub mod mlx_native_backend;
pub mod specdecode;
pub mod specdecode_backend;
pub mod specdecode_plookup;

pub(crate) use rozum_core::{
    backend, concurrency, constrain, engine, footprint, harmony, serving,
};
pub(crate) use rozum_models::model_source;
// mlx_native_backend.rs refers to the SPI types at the crate root (e.g.
// `crate::ChatBackend`), as they were re-exported from the monolith's lib root —
// mirror that here so those paths resolve unchanged.
pub(crate) use rozum_core::backend::*;
