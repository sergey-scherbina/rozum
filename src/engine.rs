//! Internal engine seam — see `docs/specs/native-engine-spi.md`.
//!
//! `LocalEngine` is the smallest surface an **in-process** inference engine
//! exposes; the engine-agnostic decode/serving loop ([`drive`]) lives above it so
//! a new engine (e.g. the future Vulkan/x86 leaf) is "implement this trait + its
//! kernels", not "re-implement the whole leaf". The seam is at the **token/text**
//! level — the engine owns its whole forward + sampling graph, so there is no
//! per-op cross-runtime sync (the `mistralrs-mlx-direct` dead-end).
//!
//! Status: **A1 of `native-engine-spi`** — the seam is *defined* here. A2 extracts
//! [`drive`]'s body from the MLX leaf's `stream_generation` (behavior-preserving,
//! gated by the existing tests); A3 adopts it in the GGUF leaf.

use std::path::Path;

use tokio_util::sync::CancellationToken;

use crate::backend::{ChatEvent, ChatRequest, SamplingParams};

/// Static facts the shared driver needs from a loaded model.
pub struct EngineMeta {
    /// Maximum context length in tokens.
    pub n_ctx: u32,
    /// All stop-token ids (multi-EOS — e.g. Qwen `<|im_end|>` + `<|endoftext|>`,
    /// gpt-oss `<|return|>` / `<|call|>` / `<|endoftext|>`).
    pub eos: Vec<u32>,
    /// `config.json`'s effective `model_type`.
    pub model_type: String,
    /// gpt-oss **harmony** channel format (`drive` picks `harmony::parse_harmony`)
    /// vs the Qwen `<tool_call>` parser (`serving::parse_tool_calls`).
    pub harmony: bool,
}

/// Construction knobs common to local engines.
#[derive(Clone, Debug, Default)]
pub struct EngineOptions {
    /// Override the context window (else the model's max, RAM-bounded).
    pub n_ctx: Option<u32>,
    /// Pick a specific compute device (else the best integrated GPU).
    pub device_index: Option<u32>,
}

/// The smallest hardware-facing surface. Everything above it — templating,
/// tokenization, the detok→[`ChatEvent`] loop, tool-call parsing (serving +
/// harmony), EOS/cancel/max-tokens, stream assembly, sampling glue — is shared in
/// [`drive`]. The engine owns its whole forward + sampling graph; it only yields
/// tokens.
pub trait LocalEngine: Send {
    /// Load weights (zero-copy `mmap` internally) + tokenizer + config from a
    /// model directory.
    fn load(dir: &Path, opts: &EngineOptions) -> Result<Self, String>
    where
        Self: Sized;

    /// Static facts for the shared driver (context, EOS, template/format).
    fn meta(&self) -> &EngineMeta;

    /// A token iterator for a sampled generation over `prompt` (prefill → decode).
    /// The engine samples however suits its hardware — MLX on the GPU inside its
    /// graph; a CPU/Vulkan engine by materializing the last-row logits and calling
    /// [`crate::sampler::sample`]. Honors `params`; polls `cancel` and stops early
    /// (the driver emits `Done{Cancelled}`).
    fn generate<'a>(
        &'a mut self,
        prompt: &'a [u32],
        params: &'a SamplingParams,
        cancel: &'a CancellationToken,
    ) -> Box<dyn Iterator<Item = Result<u32, String>> + Send + 'a>;

    /// Opt-in: append-only prefix-KV reuse across turns (default: unsupported).
    fn supports_prefix_reuse(&self) -> bool {
        false
    }
}

/// The shared, engine-agnostic decode-control loop. Renders the prompt (chat
/// template + tokenizer via [`LocalEngine::meta`]), drives [`LocalEngine::generate`],
/// and turns the token stream into [`ChatEvent`]s — streaming non-tool /
/// `final`-channel text, detecting & emitting tool calls
/// ([`crate::serving::parse_tool_calls`] or the harmony parser per
/// [`EngineMeta::harmony`]), honoring EOS/cancel/max-tokens, finalizing with
/// `Done`. This is today's per-leaf `stream_generation` (MLX) / token loop (GGUF)
/// generalized to one place.
///
/// **A2 of `native-engine-spi`:** extract the body from `stream_generation`,
/// behavior-preserving, gated by the existing MLX + GGUF tests. Defined here as
/// the seam target so A1 type-checks the boundary.
pub fn drive<E, F>(_engine: &mut E, _req: &ChatRequest, _emit: F)
where
    E: LocalEngine,
    F: FnMut(ChatEvent),
{
    unimplemented!("native-engine-spi A2: extract from stream_generation / gguf loop")
}
