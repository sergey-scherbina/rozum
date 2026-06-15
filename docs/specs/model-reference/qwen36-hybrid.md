# Qwen3.6 / Qwen3-Next (hybrid)

`model_type: "qwen3_5"` / `"qwen3_5_text"` (dense, e.g. Qwen3.6-27B) and `"qwen3_5_moe"` /
`"qwen3_5_moe_text"` (MoE, e.g. Qwen3.6-35B-A3B). MLX leaf: `models/qwen3_5.rs`,
`models/qwen3_5_moe.rs`, `models/gated_delta.rs`. This is the user's primary model.

## Hybrid backbone

The decoder stack interleaves two layer kinds, chosen by `full_attention_interval`:

- **Full-attention layers** — every `full_attention_interval`-th layer (e.g. every 4th).
  Output-gated attention with **partial RoPE** (RoPE applied to only the first
  `partial_rotary_factor` of head_dim). Reuses the Qwen3 attention shape.
- **GatedDeltaNet linear-attention layers** — the rest. A depthwise `Conv1d` short-conv
  followed by the **delta-rule recurrent scan** (linear attention; see below).

So the per-layer **cache is heterogeneous**: KV (keys/values + offset) for full-attention
layers, `(conv_state, recurrent_state)` for the linear layers. A new leaf must model the cache
as a per-layer enum, not a uniform KV cache.

## GatedDeltaNet recurrence (`gated_delta.rs`)

The linear-attention scan, ported from Python `mlx_lm.models.gated_delta`. Two numerically
identical paths:

- **`gated_delta_kernel`** — a custom Metal kernel (`mx.fast.metal_kernel`) doing the whole
  `T`-step scan in one GPU dispatch (~3× faster prefill). Requires `Dk % 32 == 0`.
- **`gated_delta_ops`** — the pure-ops reference (sequential per-token scan). The validated
  oracle and the `ROZUM_GD_OPS=1` escape hatch.

The scan's accumulator math runs in **f32** (the "f32 delta-scan" — bf16 here drifts). Single
stream (batch 1, no padding) → the SSM mask is always `None` and the conv cache is just the
trailing `kernel-1` positions. For batched decode the fixed-size conv + recurrent state simply
**stack on the batch axis** (row-independent — no padding/rope/mask), which is why hybrid
batched decode needed no kernel change.

## RMSNorm `+1` — folded at load

Qwen3.6 RMSNorm weights use the `+1` convention. The MLX leaf **adds 1.0 to the stored weights
at load** so the runtime uses a plain RMSNorm. (Getting this wrong → plausible-but-wrong
output; it was the original root-cause of the port's numeric drift.)

## MoE variant (`qwen3_5_moe`)

Same hybrid backbone (full-attn + GatedDeltaNet, reused verbatim), but every layer's MLP is a
sparse MoE block:

- A router `gate` over `num_experts` + a fused `SwitchGLU` of the top-k (reused from
  `qwen3_moe`), **plus a shared expert** gated by `sigmoid(shared_expert_gate(x))`.
- **Mixed quantization:** the router `gate` and the shared-expert gate are **8-bit** while the
  rest of the model is 4-bit. They're held as raw quantized linears (`quantized_matmul`)
  **outside** `nn::quantize` (which is single-bit-width), and loaded as such.

## Checkpoint

- Multimodal-wrapper form (`text_config` + `language_model.` prefix) for the bigger sizes — see
  README. AFQ `.inner.*` remap. Tied or untied per checkpoint.
