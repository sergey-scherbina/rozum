# Gemma 3 (text)

`model_type: "gemma3_text"` (flat text-only, e.g. gemma-3-1b-it) and `"gemma3"` (multimodal
wrapper, e.g. gemma-3-4b/12b/27b-it). MLX leaf: `models/gemma3.rs`. A **distinct** architecture
— NOT a Llama alias. Ported from Python `mlx_lm.models.gemma3_text`.

## Forward math (the quirks)

- **RMSNorm `(1 + weight)`, in f32** everywhere (`GemmaRmsNorm`) — including the per-head q/k
  norms. (See README's `+1` note; Gemma applies it at compute time, not at load.)
- **Embedding scaled by `sqrt(hidden_size)`** (cast to the activation dtype) after lookup.
- **Per-head q/k RMSNorm** (GemmaRmsNorm over `head_dim`) before RoPE.
- **Four norms per layer:** pre- and post-norm around **both** attention and the MLP
  (`input_layernorm`, `post_attention_layernorm`, `pre_feedforward_layernorm`,
  `post_feedforward_layernorm`).
- **MLP is GELU(tanh approx)**, not SiLU.
- **Attention scale is `query_pre_attn_scalar^-0.5`** — NOT `head_dim^-0.5` in general.
- **Tied embeddings** (lm_head = embedding-as-linear) — *unless* a separately-quantized
  `lm_head.*` is materialized in the checkpoint (some uploads), in which case use it. Detect by
  key presence.

## Alternating local/global attention

Every `sliding_window_pattern`-th layer (default 6) is **GLOBAL** (RoPE base `rope_theta`, full
causal attention); the rest are **LOCAL** (RoPE base `rope_local_base_freq`) and additionally
mask out keys older than `sliding_window` (default 512/1024). The two masks coincide when the
whole context fits the window, so short prompts are unaffected; long contexts need the windowed
local mask (`build_gemma_masks` over absolute positions; correct at decode `offset > 0`). The KV
cache is still full (a bounded windowed cache is a later optimization, not a correctness gap).

## RoPE scaling (the bigger sizes)

The 4B/12B/27B set `rope_scaling: {"rope_type":"linear","factor":8}`, applied to the **GLOBAL**
layers only (the local sliding-window layers stay unscaled). The 1B has none.

## Multimodal wrapper (4B/12B/27B) — `model_type: "gemma3"`

The useful sizes ship as the vision-language wrapper (see README's general unwrap). Gemma
specifics:

- Text params nest under `text_config`; `quantization` is top-level (graft it in).
- Language weights are `language_model.*` (strip the prefix); skip `vision_tower.*` /
  `multi_modal_projector.*`. No materialized `lm_head` → tied.
- The nested `text_config` **omits most fields** → the arg struct must default them to the HF
  `Gemma3TextConfig` values. Verified to reconstruct 4B/12B/27B exactly:

  | field | default | field | default |
  |-------|---------|-------|---------|
  | `num_attention_heads` | 8 | `rope_theta` | 1 000 000 |
  | `num_key_value_heads` | 4 | `rope_local_base_freq` | 10 000 |
  | `head_dim` | 256 | `sliding_window_pattern` | 6 |
  | `query_pre_attn_scalar` | 256 | `vocab_size` | 262 208 |
  | `rms_norm_eps` | 1e-6 | `max_position_embeddings` | 131 072 |

  (4B omits heads → 8; 12B provides 16 but omits `head_dim` → 256; 27B provides
  `head_dim: 128` and `query_pre_attn_scalar`. The defaults only fill what a given size omits.)

## Checkpoint

AFQ `.inner.*` remap; the consolidated-`model.safetensors`-with-stale-shard-index case showed
up here (the 4B), so the stale-index fallback (README) is required to load it.
