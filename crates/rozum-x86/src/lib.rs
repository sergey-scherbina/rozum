// rozum-x86 — the x86 / Vulkan iGPU engine slot. Extracted from the `rozum`
// monolith as part of Phase 2 of the workspace split (docs/specs/workspace-split.md).
// The `x86` module is kept nested (so its internal paths are unchanged) and the
// upstream `rozum-core` modules it uses are re-exported into this crate's root so
// `crate::backend::…` / `crate::engine::…` / `crate::sampler::…` resolve unchanged.
pub mod x86;

pub(crate) use rozum_core::{backend, engine, sampler};
