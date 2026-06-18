//! `x86::tensor` — a thin `Tensor` = `VkBuffer` + shape + dtype. Trivial; no logic.
//!
//! **Contract (fill in at x86 P1):** wrap a device buffer (from [`super::memory`]) with a shape
//! and a dtype; this is the value [`super::kernels`] ops consume and produce, the same role
//! `mlx_rs`'s `Array` plays for the MLX leaf. No math lives here.

#![allow(dead_code)]

/// Element type of a [`Tensor`]. Packed-quant variants carry weights read in place by the
/// quant-matmul kernels (no dequant-to-fp16 at rest).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dtype {
    F32,
    F16,
    /// AFQ affine 4-bit (group scales + biases) — `mlx-weight-layout-and-afq.md`.
    Afq4,
    /// AFQ affine 8-bit.
    Afq8,
    /// MXFP4 (E2M1 + E8M0 block scale, no zero-point) — gpt-oss layout.
    Mxfp4,
}

/// A device tensor: a buffer handle (TBD `VkBuffer`) + shape + dtype.
pub struct Tensor {
    pub shape: Vec<usize>,
    pub dtype: Dtype,
    // buffer: VkBuffer  // filled at x86 P1
}
