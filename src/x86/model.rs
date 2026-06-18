//! `x86::model` — per-family forward (Qwen3, gpt-oss, …) built from the `model-reference/`
//! math over [`super::kernels`], exposing the engine as a [`LocalEngine`]. This is where the
//! decode loop (chunked prefill → single-token decode over an external KV cache) lives —
//! mirroring the MLX leaf's `Generate` iterator.
//!
//! **Contract (fill in at x86 P1–P3):** implement [`LocalEngine::load`] (zero-copy weights via
//! [`super::memory`]), [`LocalEngine::meta`] (n_ctx / EOS / model_type / harmony from config),
//! and [`LocalEngine::generate`] (prefill+decode, sampling via [`crate::sampler::sample`] over
//! the materialized last-row logits). Then the shared [`crate::engine::drive`] loop turns its
//! token stream into `ChatEvent`s for free. **Gate:** greedy parity vs MLX, op-by-op.
//!
//! Defining [`X86Engine`] as a real `impl LocalEngine` here — even as a stub — is the point of
//! the slot: it proves the token-level seam is implementable by a non-MLX engine, so filling it
//! is replacing these bodies, not reshaping the trait.

#![allow(dead_code)]

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::backend::SamplingParams;
use crate::engine::{EngineMeta, EngineOptions, LocalEngine};

/// The x86 Vulkan engine. Holds the loaded model + device state (TBD); a stub today.
pub struct X86Engine {
    meta: EngineMeta,
}

impl LocalEngine for X86Engine {
    fn load(_dir: &Path, _opts: &EngineOptions) -> Result<Self, String> {
        Err(super::NOT_IMPLEMENTED.into())
    }

    fn meta(&self) -> &EngineMeta {
        &self.meta
    }

    fn generate<'a>(
        &'a mut self,
        _prompt: &'a [u32],
        _params: &'a SamplingParams,
        _cancel: &'a CancellationToken,
    ) -> Box<dyn Iterator<Item = Result<u32, String>> + Send + 'a> {
        // Yields a single error rather than panicking, so a partial fill-in degrades cleanly.
        Box::new(std::iter::once(Err(format!(
            "x86::model::generate not implemented (x86 P1): {}",
            super::NOT_IMPLEMENTED
        ))))
    }
}
