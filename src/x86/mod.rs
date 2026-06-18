//! Native x86 runtime — the **cross-vendor Vulkan iGPU engine** slot.
//!
//! This brings the MLX recipe (compute on the integrated GPU, unified memory, zero-copy
//! `mmap` of the weight file) to commodity x86 (Intel Xe/Arc + AMD APU iGPUs) as a new
//! in-process engine **below the existing one-trait seam** ([`crate::engine::LocalEngine`]).
//! Everything above the seam — gateway, rooms, launch, catalog, the shared
//! decode/serving loop — is reused unchanged. See `docs/specs/x86-native-runtime.md` and
//! `docs/specs/native-engine-spi.md`.
//!
//! ## Status: SCAFFOLDED SLOT, not yet implemented.
//!
//! This module is the **empty slot, pre-shaped so the real engine drops in without
//! structural rework**. It compiles on any host (no Vulkan deps yet) so the default CI
//! continuously type-checks the contract: an [`X86Engine`] that implements `LocalEngine`,
//! an [`X86NativeBackend`] that implements [`ChatBackend`], the `X86NativeOptions` knobs,
//! and the five compact components the spec decomposes the engine into:
//!
//! - [`device`]  — Vulkan instance/device/queue; prefer an iGPU with a UMA heap.
//! - [`memory`]  — zero-copy `mmap` import (`VK_EXT_external_memory_host`).
//! - [`tensor`]  — `VkBuffer` + shape + dtype.
//! - [`kernels`] — the SPIR-V op set (quant matmul, sdpa, rmsnorm, rope, swiglu…).
//! - [`model`]   — per-family forward, built from `model-reference/` math; `impl LocalEngine`.
//!
//! **To fill the slot** (each is independently shippable + gated on greedy parity vs MLX):
//! implement the `todo!()`/`Err`-stub bodies in the components per the spec's P0–P5 plan,
//! add the Vulkan binding under the `x86-native` Cargo feature, and wire `X86NativeBackend::chat`
//! through [`crate::engine::drive`] (the shared loop) once `drive` lands (native-engine-spi A3).
//! No caller above the seam changes.

#![allow(dead_code)] // scaffold: the component types are the contract to fill, not yet used.

pub mod device;
pub mod kernels;
pub mod memory;
pub mod model;
pub mod tensor;

use std::sync::Arc;

use async_trait::async_trait;

use crate::backend::{ChatBackend, ChatRequest, ChatStream, ModelError, ModelResult};

/// A short, stable message used everywhere the slot is reached, so selecting x86-native
/// today is **self-documenting**, never a silent failure.
pub const NOT_IMPLEMENTED: &str = "x86-native engine slot is scaffolded but NOT implemented yet \
    (P0–P5 pending real Vulkan kernels + hardware; needs --features x86-native). \
    See docs/specs/x86-native-runtime.md.";

/// Construction knobs for the x86 engine (mirrors `EngineOptions`, plus sampling defaults).
/// `device_index` is the **placement hook** the device-aware residency layer
/// (`docs/specs/multi-device-residency.md`) drives — on a multi-socket (NUMA) box this
/// selects the iGPU whose local memory node holds the `mmap`'d weights.
#[derive(Clone, Debug)]
pub struct X86NativeOptions {
    /// Context window; env `ROZUM_X86_N_CTX`, default = model max (RAM-bounded).
    pub n_ctx: u32,
    pub temperature: f32,
    pub top_p: f32,
    /// Compute device; env `ROZUM_X86_DEVICE`, default = best integrated GPU.
    pub device_index: Option<u32>,
}

impl Default for X86NativeOptions {
    fn default() -> Self {
        Self { n_ctx: 0, temperature: 0.0, top_p: 1.0, device_index: None }
    }
}

/// The outward backend the gateway sees. Implements the unchanged [`ChatBackend`] seam;
/// once the engine is real its `chat` bridges async↔the GPU worker thread and calls the
/// shared `drive` loop. Today it errors clearly.
pub struct X86NativeBackend {
    opts: X86NativeOptions,
}

impl X86NativeBackend {
    pub fn new(opts: X86NativeOptions) -> Self {
        Self { opts }
    }
}

#[async_trait]
impl ChatBackend for X86NativeBackend {
    async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
        Err(ModelError::BackendUnavailable(NOT_IMPLEMENTED.into()))
    }
    fn context_window(&self) -> u32 {
        self.opts.n_ctx
    }
    fn label(&self) -> &'static str {
        "x86-native"
    }
    /// Serialize like the MLX leaf (one GPU stream, single resident worker).
    fn concurrency_capacity(&self) -> Option<usize> {
        Some(1)
    }
}

/// Try to build the x86 backend for `model`. The slot is scaffolded but the engine isn't
/// implemented, so this logs the reason and returns `None` — the gateway's build chain then
/// falls through to the next engine (e.g. native MLX on a Mac). When the engine is filled in,
/// this returns `Some(Arc::new(X86NativeBackend::new(..)))`.
pub fn try_build_x86_backend(_model: &str, _n_ctx: u32) -> Option<Arc<dyn ChatBackend>> {
    eprintln!("x86-native: {NOT_IMPLEMENTED}");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_default_is_sane() {
        let o = X86NativeOptions::default();
        assert_eq!(o.device_index, None);
        assert_eq!(o.top_p, 1.0);
    }

    #[test]
    fn try_build_falls_through_until_implemented() {
        // The slot is reachable but not built → None (caller falls through), never a panic.
        assert!(try_build_x86_backend("mlx-community:Qwen3-4B-4bit", 4096).is_none());
    }

    #[tokio::test]
    async fn backend_chat_errors_clearly() {
        // If a real backend is ever constructed before the engine is filled, requests fail
        // with the self-documenting message rather than hanging or returning garbage.
        let be = X86NativeBackend::new(X86NativeOptions::default());
        assert_eq!(be.context_window(), 0);
        assert_eq!(be.label(), "x86-native");
        assert_eq!(be.concurrency_capacity(), Some(1));
        match be.chat(ChatRequest::simple("hi")).await {
            Err(ModelError::BackendUnavailable(m)) => assert!(m.contains("x86-native")),
            Err(other) => panic!("unexpected error variant: {other}"),
            Ok(_) => panic!("expected a clear BackendUnavailable error, got a stream"),
        }
    }
}
