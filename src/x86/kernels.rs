//! `x86::kernels` — the SPIR-V op set, one small shader + dispatch fn each. The make-or-break
//! work. Each op is filled independently and **tested op-by-op against a CPU reference**.
//!
//! **Contract (fill in at x86 P1–P4, `docs/specs/x86-native-runtime.md` + `model-reference/`):**
//! - [`quant_matmul`] — AFQ affine 4/8-bit and MXFP4, **decoded on the fly** from packed weights
//!   ([`super::tensor::Dtype`]); a **gather** variant selects per-token experts (MoE).
//! - [`sdpa`] — GQA attention with **attention sinks** + **sliding-window** masking (gpt-oss);
//!   explicit `softmax(QKᵀ)·V` first, a fused/flash kernel later.
//! - [`rms_norm`], [`rope`] (incl. YaRN frequencies), [`swiglu`] (clamped variant for gpt-oss),
//!   [`softmax`], [`embedding`].
//! - Use `shaderInt8` / subgroup ops for the quant dot-products and fp16 accumulate where the
//!   device ([`super::device::DeviceCaps`]) supports them.
//!
//! **Test to write (per op):** dispatch the kernel and compare its output to a scalar CPU
//! reference within tolerance — the op-by-op parity discipline that isolates a kernel bug.

#![allow(dead_code)]

use super::tensor::Tensor;

type K = Result<Tensor, String>;
const TODO: &str = "x86::kernels: not implemented (x86 P1+)";

/// Dequantize-on-the-fly matmul over packed quant weights. `experts` non-`None` → MoE gather.
pub fn quant_matmul(_x: &Tensor, _w: &Tensor, _experts: Option<&[u32]>) -> K {
    Err(TODO.into())
}
/// GQA scaled-dot-product attention with optional sinks + sliding-window mask.
pub fn sdpa(_q: &Tensor, _k: &Tensor, _v: &Tensor, _sliding_window: Option<u32>) -> K {
    Err(TODO.into())
}
pub fn rms_norm(_x: &Tensor, _weight: &Tensor, _eps: f32) -> K {
    Err(TODO.into())
}
pub fn rope(_x: &Tensor, _positions: &[u32]) -> K {
    Err(TODO.into())
}
/// SwiGLU; `clamp` set for gpt-oss's clamped variant.
pub fn swiglu(_gate: &Tensor, _up: &Tensor, _clamp: Option<f32>) -> K {
    Err(TODO.into())
}
pub fn softmax(_x: &Tensor) -> K {
    Err(TODO.into())
}
pub fn embedding(_ids: &[u32], _table: &Tensor) -> K {
    Err(TODO.into())
}
