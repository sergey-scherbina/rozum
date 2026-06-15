# Qwen2 / Qwen2.5 (incl. Coder)

`model_type: "qwen2"` (e.g. Qwen2.5-0.5B, Qwen2.5-Coder-7B/32B). MLX leaf: `models/qwen2.rs`.
Llama-shaped decoder with **one distinguishing quirk**.

## The quirk: QKV bias

Qwen2's `q_proj`, `k_proj`, `v_proj` are built **`bias(true)`** — the checkpoint carries
`self_attn.{q,k,v}_proj.bias`. `o_proj` and the MLP linears have **no** bias. This is the single
thing that separates Qwen2 from the Llama/Qwen3 attention shape; miss it and the bias tensors go
unloaded → wrong output. (Qwen3 dropped the QKV bias and added per-head q/k norm instead.)

## Otherwise

Standard GQA attention, full RoPE (`rope_theta`, optional `rope_scaling`), SwiGLU MLP, plain
RMSNorm (**no** `+1`), scale `head_dim^-0.5`. **No** per-head q/k norm (that's Qwen3).

## Config

Qwen2 configs **often omit `head_dim`** → the loader fills it from
`hidden_size / num_attention_heads`. `tie_word_embeddings` per config (small Qwen2.5 models tie;
larger ones don't). AFQ-quantized; `.inner.*` remap as usual.

## Batched decode

Same per-row-RoPE mechanism (`set_batch_pad_offsets` thread-local on the Qwen2 attention) — batches.
