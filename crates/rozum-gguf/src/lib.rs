// rozum-gguf — the in-process GGUF / llama.cpp ChatBackend engine. Extracted from
// the `rozum` monolith as part of Phase 2 of the workspace split
// (docs/specs/workspace-split.md). The module name is preserved and the upstream
// `rozum-core` modules it uses are re-exported into this crate's root so the moved
// file's `crate::backend::…` / `crate::engine::…` / `crate::sampler::…` /
// `crate::serving::…` paths resolve unchanged.
pub mod gguf;

pub(crate) use rozum_core::{backend, engine, sampler, serving};
