# Qwen3 (dense + MoE)

`model_type: "qwen3"` (dense, e.g. Qwen3-4B) and `"qwen3_moe"` (sparse MoE, e.g.
Qwen3-30B-A3B). MLX leaf: `models/qwen3.rs`, `models/qwen3_moe.rs`.

## Attention (shared by both)

Standard decoder attention with two Qwen3 specifics:

- **Per-head q/k RMSNorm** — a `RmsNorm` over the `head_dim` applied to queries and keys
  *before* RoPE (plain RMSNorm, **no** `+1`). `q_norm`/`k_norm` weights are per-head-dim.
- **Explicit `head_dim`** — taken from config (not `hidden_size / num_heads`). Attention scale
  is `head_dim^-0.5`.
- Full RoPE (`rope_theta` from config, optional `rope_scaling`), GQA (`num_key_value_heads`),
  causal mask. No QKV bias (contrast Qwen2, which has it).
- MLP: SwiGLU (`silu(gate(x)) * up(x)` → `down`). RMSNorm pre-attn and pre-MLP.

The dense attention block is reused **verbatim** by Qwen3-MoE and by the Qwen3.6 hybrid's
full-attention layers — it's the canonical Qwen3 attention.

## MoE MLP (`qwen3_moe`)

Each layer's MLP is replaced by a sparse block:

- A router `gate` (linear, no bias) → softmax over `num_experts` → top-`k` experts with
  normalized weights.
- The top-`k` experts run as a fused **`SwitchGLU`** (`gather_qmm` over AFQ-quantized expert
  weights), output = weighted sum.
- **Prefill optimization (byte-identical):** with many routed slots, sort tokens by expert and
  tell `gather_qmm` the indices are sorted (`sorted_indices=true`) so each expert's rows are
  accessed contiguously (mirrors Python `SwitchGLU` `_gather_sort`/`_scatter_unsort`). At decode
  (`T=1`, few slots) skip the sort. Pure memory-access win, no numeric change.

## Batched decode (per-row RoPE)

For ragged batched decode, queries/keys are roped per row at `cache.offset() − pad_offsets[i]`
(left-padded batch, each row right-aligned). Installed via a thread-local
`set_batch_pad_offsets(Some([B]))` set before the forward and cleared after; `None` (default) →
the normal single-offset B=1 path (byte-identical). See the batched-decode notes in the
CHANGELOG.

## Checkpoint

- Plain RMSNorm (no `+1`). AFQ-quantized linears/experts (the `.inner.*` remap, see README).
- `lm_head` present (untied) unless the config ties it.
