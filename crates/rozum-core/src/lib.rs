// rozum-core — the durable, hardware-agnostic core (the `ChatBackend` SPI and the
// pieces every engine / serving layer builds on). Extracted from the `rozum`
// monolith as Phase 0 of the workspace split (docs/specs/workspace-split.md).
// Module names are preserved so the moved files' internal `crate::backend::…`
// paths resolve unchanged and the `rozum` crate re-exports these modules under
// their original paths.
pub mod backend;
// The bug-ledger guard lives in a LIB because CI only runs `cargo test --workspace --lib`; a guard
// that does not run reads as coverage while providing none. See the module docs.
/// Re-exported so callers say `rozum_core::build_stamp::…` regardless of which crate owns it.
pub use rozum_stamp as build_stamp;
pub mod bug_ledger;
pub mod concurrency;
pub mod constrain;
pub mod engine;
pub mod footprint;
pub mod harmony;
pub mod door;
pub mod obs;
pub mod prefetch;
pub mod sampler;
pub mod serving;
pub mod share;
pub mod shed;
