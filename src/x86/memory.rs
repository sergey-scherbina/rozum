//! `x86::memory` — the UMA core: zero-copy `mmap` import + host-visible allocation.
//!
//! **Contract (fill in at x86 P0/P2, `docs/specs/x86-native-runtime.md`):**
//! - **Weights, zero-copy:** `mmap` the safetensors/`.gguf`, then import the host pointer as
//!   `VkDeviceMemory` via `VK_EXT_external_memory_host` (honor `minImportedHostPointer
//!   Alignment` — safetensors data offsets may need padding) and bind a `VkBuffer` per tensor.
//!   The compute shaders read the **packed quantized weights straight out of the page cache** —
//!   no staging buffer, no upload (the literal analog of MLX reading an `mmap`'d `Array`).
//!   *Fallback* for drivers lacking the extension: a one-time host-visible staged copy (slower
//!   start, identical steady state).
//! - **Activations / KV cache:** allocate `HOST_VISIBLE | DEVICE_LOCAL` so the CPU sampler reads
//!   logits and the GPU writes KV with no staging.
//!
//! **Test to write:** `mmap` a file → import → read the bytes back GPU-side; assert they equal
//! the source bytes (the zero-copy round-trip gate from x86 P0).

#![allow(dead_code)]

use std::path::Path;

/// Import a model file's bytes as device-readable memory with no copy. Returns an opaque handle
/// the tensor layer binds buffers against.
pub fn import_weights_zero_copy(_path: &Path) -> Result<(), String> {
    Err("x86::memory::import_weights_zero_copy not implemented (x86 P0; \
         VK_EXT_external_memory_host)"
        .into())
}
