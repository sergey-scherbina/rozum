# Model reference specs (engine-independent)

The forward math + checkpoint conventions we reverse-engineered porting each model family
into the native MLX runtime, captured as **facts** so a new leaf (a different tensor library,
a CUDA path, a fresh runtime) can implement from this instead of re-deriving from a checkpoint
— which is where the real time went.

The *code* stays per-tensor-lib (`.vendor/mlx-lm/mlx-lm/src/models/*.rs` for the MLX leaf);
these docs are the *knowledge*. Each per-family file lists `model_type`s, config quirks, the
forward math, and any load-time special-casing. Quantization layout has its own deep spec:
[`../mlx-weight-layout-and-afq.md`](../mlx-weight-layout-and-afq.md).

| Family | doc | `model_type`(s) |
|--------|-----|-----------------|
| Qwen3 dense + MoE | [`qwen3.md`](qwen3.md) | `qwen3`, `qwen3_moe` |
| Qwen3.6 / Qwen3-Next hybrid | [`qwen36-hybrid.md`](qwen36-hybrid.md) | `qwen3_5(_text)`, `qwen3_5_moe(_text)` |
| Llama family (Llama 3.x / Mistral / Phi-3 / SmolLM) | [`llama-family.md`](llama-family.md) | `llama`, `mistral`, `phi3` |
| Qwen2 / 2.5 | [`qwen2.md`](qwen2.md) | `qwen2` |
| Gemma 3 | [`gemma3.md`](gemma3.md) | `gemma3_text`, `gemma3` |

## Cross-cutting checkpoint conventions

These apply across families — get them right once.

### AFQ quantized weights — the load-time remap

A quantized `nn.Linear`/embedding is **three** sibling tensors under one prefix:
`<p>.weight` (packed `uint32`), `<p>.scales` (bf16), `<p>.biases` (bf16) — full layout in the
AFQ spec. The load-time fact that bites every leaf: after you wrap the module in the
tensor-lib's quantized container, the **dequantizable weight lives one level deeper**. The MLX
leaf builds the model, runs `nn::quantize`, then maps each checkpoint key:

```
<p>.weight  →  <p>.inner.weight   (iff <p>.scales is present in the checkpoint, i.e. quantized)
<p>.bias    →  <p>.inner.bias     (same condition)
<p>.scales / <p>.biases           →  applied as-is
```

i.e. detect "is this prefix quantized?" by the presence of `<p>.scales`, and if so route
`.weight`/`.bias` to `.inner.*`. Non-quantized tensors (norms, a bf16 `lm_head`, a small 8-bit
router held outside `nn::quantize`) pass through unremapped.

### RMSNorm `+1` convention

Some checkpoints store RMSNorm weights as the **delta from 1**, so the norm is
`x_normed * (1 + weight)`, not `x_normed * weight`. Always computed in **f32** (cast in, cast
out) regardless of activation dtype. Who uses it:

- **Gemma 3** — everywhere (its `GemmaRmsNorm`), including the per-head q/k norms.
- **Qwen3.6 / Qwen3-Next** — yes; the MLX leaf folds the `+1` in **at load** (adds 1.0 to the
  stored weights) so the runtime can use a plain RMSNorm.
- **Qwen3 / Qwen3-MoE / Qwen2 / Llama** — **no** (plain RMSNorm, weight used as-is).

A `+1` mistake is silent: the model runs and emits *plausible-but-wrong* tokens. Verify against
a reference forward, not by eyeballing output.

### Tied embeddings

When `tie_word_embeddings` (or no materialized `lm_head.*` in the checkpoint), the output
projection IS the embedding matrix read as a linear. Detect by the **absence** of `lm_head.*`
keys, not only by the config flag (some checkpoints tie implicitly). Gemma 3 and the Qwen3.6
multimodal-wrapper checkpoints are tied; a separately-quantized `lm_head` (some Gemma uploads)
is NOT — load it when present.

### safetensors sharding + stale indexes

Weights are either a single `model.safetensors` or sharded with a
`model.safetensors.index.json` (`weight_map: key → file`). **Trust the index only when every
file it names exists** — some mlx-community uploads ship a stale index that references sharded
filenames after the weights were consolidated into a single `model.safetensors`. Fallback: load
every `*.safetensors` physically present in the directory.

### Multimodal `text_config` unwrap

A vision-language checkpoint (`model_type: "gemma3"`, and the Qwen-VL family) nests the text
model under `text_config`, puts `quantization` at the **top** level, and prefixes the language
weights `language_model.*` alongside `vision_tower.*` / `multi_modal_projector.*`. To run it as
a text model: parse args from `text_config` (grafting top-level `quantization` in), strip the
`language_model.` weight prefix, and skip the vision/projector tensors. The nested
`text_config` also **omits** most fields and relies on the family's HF config defaults — so the
arg struct needs correct `serde(default)`s (see `gemma3.md`).

### MLX vs PyTorch row ordering

MLX checkpoints and PyTorch checkpoints can differ in weight-row ordering for fused/reshaped
projections; the AFQ spec documents the cases and a diagnostic methodology to localize
numerical drift. Relevant when bridging a PyTorch-exported checkpoint, not for the
mlx-community ones the native runtime targets.
