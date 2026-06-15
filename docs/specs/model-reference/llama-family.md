# Llama family (Llama 3.x / Mistral / Phi-3 / SmolLM)

One model file serves several `model_type`s that share the Llama decoder shape. MLX leaf:
`models/llama.rs`. All load into the same `Model`; the differences are config quirks and a
load-time projection split.

## Canonical Llama decoder

GQA attention (`num_attention_heads` / `num_key_value_heads`), full RoPE (`rope_theta`, optional
`rope_scaling`), **no QKV bias**, SwiGLU MLP, plain RMSNorm (**no** `+1`) pre-attn and pre-MLP.
Scale `head_dim^-0.5`.

## `model_type` aliases + their quirks

- **`llama`** — Llama 3.x, and **SmolLM** (a small Llama-arch model; also exercises the
  **bf16 non-quantized** load path — `quantization = None`, no `.inner.*` remap).
- **`mistral`** — same decoder. Two fixes were needed: **`head_dim` is `Option`** with
  `#[serde(default)]` (Mistral omits it → fall back to `hidden_size / num_attention_heads`), and
  its **chat template is a *list* form** (`[{name, template}, …]`) rather than a string — the
  template loader must accept both.
- **`phi3`** — same decoder, but the checkpoint **fuses projections**: one `qkv_proj` and one
  `gate_up_proj` per layer instead of separate `q/k/v` and `gate/up`. Handled by a dedicated
  loader (`load_phi3_model`) that **splits the fused weights along the output-row axis at load**
  (`split_fused_key`) into the canonical `q_proj`/`k_proj`/`v_proj` + `gate_proj`/`up_proj`, then
  runs the normal Llama path. The split is exact for AFQ weights because AFQ packs along the
  **input** axis, so slicing output rows of `.weight`/`.scales`/`.biases` is lossless (see the
  AFQ spec).

## Config (`ModelArgs`)

`head_dim: Option<i32>` (`#[serde(default)]` → derive from `hidden_size / num_attention_heads`
when absent). `tie_word_embeddings` default-true. Standard `num_hidden_layers`, `intermediate_size`,
`rms_norm_eps`, `vocab_size`, `rope_theta`, optional `rope_scaling`.

## Batched decode

Same per-row-RoPE mechanism as Qwen3 (a `set_batch_pad_offsets` thread-local on this attention),
so Llama/Mistral/Phi-3/SmolLM all batch.
