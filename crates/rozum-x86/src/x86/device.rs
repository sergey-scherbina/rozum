//! `x86::device` — Vulkan instance / physical-device / compute-queue selection.
//!
//! **Contract (fill in at x86 P0, `docs/specs/x86-native-runtime.md`):**
//! - Create a Vulkan instance + logical device + a compute queue (binding TBD: `ash`).
//! - **Prefer an integrated GPU** exposing a `HOST_VISIBLE | HOST_COHERENT | DEVICE_LOCAL`
//!   heap — that flag coincidence *is* UMA, and is why the recipe transfers to iGPUs but
//!   not dGPUs. Refuse clearly (not silently) if no such device exists.
//! - **NUMA:** on a multi-socket box, prefer the iGPU whose local memory node backs the
//!   `mmap`'d weights (the placement the residency layer in
//!   `docs/specs/multi-device-residency.md` will drive via `device_index`).
//! - Feature-detect `shaderFloat16`, `shaderInt8` / `VK_KHR_shader_integer_dot_product`,
//!   and subgroup size, so [`super::kernels`] can pick the right variant at runtime.
//!
//! **Test to write:** enumerates + selects a UMA device on both an Intel Xe/Arc iGPU and an
//! AMD APU; reports the correct caps; refuses cleanly on a dGPU-only / no-UMA host.

#![allow(dead_code)]

/// Hardware features the kernels branch on. Populated by [`select_device`].
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceCaps {
    /// `shaderFloat16` — fp16 math/accumulate paths.
    pub fp16: bool,
    /// `shaderInt8` / integer dot-product — fast quant dot-products.
    pub shader_int8: bool,
    /// Subgroup width (for subgroup-reduced matmul/softmax).
    pub subgroup_size: u32,
    /// The selected memory heap is `HOST_VISIBLE | DEVICE_LOCAL` (true UMA).
    pub uma: bool,
}

/// Select the compute device (best integrated GPU, or `index` if given) and return its caps.
pub fn select_device(_index: Option<u32>) -> Result<DeviceCaps, String> {
    Err("x86::device::select_device not implemented (x86 P0)".into())
}
