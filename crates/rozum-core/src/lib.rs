// rozum-core — the durable, hardware-agnostic core (the `ChatBackend` SPI and the
// pieces every engine / serving layer builds on). Extracted from the `rozum`
// monolith as Phase 0 of the workspace split (docs/specs/workspace-split.md).
// Module names are preserved so the moved files' internal `crate::backend::…`
// paths resolve unchanged and the `rozum` crate re-exports these modules under
// their original paths.
pub mod backend;
pub mod concurrency;
pub mod constrain;
pub mod engine;
pub mod harmony;
pub mod obs;
pub mod sampler;
pub mod serving;
