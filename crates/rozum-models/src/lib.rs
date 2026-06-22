// rozum-models — model catalog, source resolution, and on-disk residency.
// Extracted from the `rozum` monolith as part of Phase 2 of the workspace split
// (docs/specs/workspace-split.md). A pure leaf (no SPI/engine deps). Module names
// are preserved so internal `crate::hf_hub::…` paths resolve unchanged and the
// `rozum` crate re-exports these modules under their original paths.
pub mod hf_hub;
pub mod model_source;
pub mod models;
pub mod modelscope;
pub mod resident;
