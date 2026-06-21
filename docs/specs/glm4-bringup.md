# GLM-4 bringup (MLX-native port)

## Overview

Add GLM-4 (Zhipu/Z.ai, dense) to rozum's catalog by porting the architecture into
the native MLX runtime — the same path as the gpt-oss and Qwen3-Coder bringups
([[project-gptoss-native-port]]). GLM-4 dense is architecturally close to Qwen3
(GQA + q/k/v bias + RMSNorm) with two GLM specifics: **partial RoPE** (only the
first half of each head's dims are rotated) and GLM's **norm placement / weight
names / chat template**. The MLX-native path is chosen over the (already-present)
mistral.rs `glm4` loader for speed — candle/Metal is ~5–10× slower
([[project-mistralrs-mlx-direct]]); mistral.rs is the quick "does it run" check only.

Targets that fit a 36 GB Mac (4-bit): **GLM-4-9B-0414** (bring-up) and
**GLM-4-32B-0414** (the real target). MoE GLMs (4.5-Air/4.5/5) are out of scope (RAM).

## Interface

- New module `glm4` in the vendored mlx-lm crate
  (`.vendor/mlx-lm/mlx-lm/src/models/glm4.rs`), exposing `load_glm4_model(dir)`,
  `Model`, `ModelInput` (mirroring `qwen3`'s surface).
- Register in `.vendor/mlx-lm/mlx-lm/src/models/mod.rs`.
- Dispatch arm in `src/mlx_native_backend.rs`: `"glm4" => glm4::load_glm4_model(dir)`.
- Catalog entry in `src/models.rs` (GLM-4-9B first, then 32B).
- Chat template: GLM's `[gMASK]<sop>` + `<|user|>` / `<|assistant|>` turns
  (verify against the checkpoint's `tokenizer_config.json` / `chat_template`).

## Architecture (from `zai-org/GLM-4-9B-0414` config.json — verified)

```
model_type=glm4  architectures=[Glm4ForCausalLM]  hidden=4096  layers=40
heads=32  kv_heads=2 (GQA)  head_dim=128  vocab=151552  intermediate=13696
partial_rotary_factor=0.5   rope_theta=10000   rms_norm_eps=1e-5
attention_bias=true   tie_word_embeddings=false
```

Reuse from existing ports: **partial RoPE** (`qwen3_5`/`qwen3_5_moe`), **q/k/v bias**
(`qwen3`), **RMSNorm** (`qwen3`).

### Exact structure (from `mlx_lm.models.glm4`, 181 lines — saved at `glm4_ref.py`)

The blueprint — `glm4.rs` mirrors `qwen3.rs` and changes exactly these:

- **Partial, traditional RoPE**: `RoPE(dims = head_dim * partial_rotary_factor = 64,
  base = rope_theta, traditional = TRUE)`. Only the first 64 of 128 head dims are rotated,
  and it's the **interleaved/traditional** RoPE (not the GPT-NeoX half-split). The remaining
  64 dims pass through. (`qwen3_5` has partial rope but check the traditional flag.)
- **q/k/v bias = true, o_proj bias = false** (`attention_bias`); GQA 32/2.
- **Sandwich norm — FOUR RMSNorms per layer** (the distinctive GLM-4 trait):
  ```
  x = x + post_self_attn_layernorm( self_attn( input_layernorm(x) ) )
  x = x + post_mlp_layernorm( mlp( post_attention_layernorm(x) ) )
  ```
  i.e. a norm BEFORE each sublayer (input_/post_attention_) AND a norm on each sublayer's
  OUTPUT before the residual add (post_self_attn_/post_mlp_). qwen3 has only the two
  pre-norms; GLM adds the two post-norms.
- **MLP**: SwiGLU (gate/up/down), like qwen3.
- Final `norm` (RMSNorm), `embed_tokens`, untied `lm_head`.

## Behavior

- [ ] `config.json model_type: "glm4"` loads (no "unsupported model_type").
- [ ] Partial RoPE: only `head_dim * partial_rotary_factor` (= 64) dims rotated;
      the rest pass through unrotated — matches `mlx_lm.models.glm4`.
- [ ] q/k/v projections carry bias (`attention_bias=true`); o_proj per the reference.
- [ ] GLM norm placement (input + post-attn; confirm sandwich post-mlp/post-self-attn
      against the reference) reproduced exactly.
- [ ] Weight-name remap is exact (the gpt-oss "garbage bug" risk) — q/k/v/o, gate/up/down,
      norms, embed, lm_head all bound to the right tensors.
- [ ] **Byte-exact greedy parity** vs Python `mlx_lm` on GLM-4-9B for a fixed prompt
      (`scripts/mlx_ref.py` — logits/`||x||` per-layer, then identical token stream).
- [ ] Chat template renders a clean single-turn reply and a tool call.
- [ ] Runs through `rozum launch` on 36 GB at 4-bit; GLM-4-32B-0414 likewise.

## Out of scope

- GLM MoE (GLM-4.5-Air 106B, GLM-4.5 355B, GLM-5/5.1 744B): too big for 36 GB; GLM-5
  is DeepSeek-style sparse-attention MoE, a separate (much larger) port. See BACKLOG
  `glm-model-landscape`.
- Perf tuning (batched decode / prefix reuse) — reuse the generic native-runtime paths;
  optimize later if GLM becomes a primary model.

## Design / Decisions

- **MLX-native port over mistral.rs** — speed (the North Star is MLX-native top-of-chain).
  mistral.rs `Glm4ForCausalLM` is the quick validation path (`ROZUM_FORCE_MISTRALRS=1`),
  not the shipping path.
- **Mirror `qwen3.rs`** — closest existing port (GQA + qkv bias + RMSNorm); splice in the
  partial-RoPE from `qwen3_5`. Rejected: from-scratch (reinvents the shared scaffolding).
- **Validation = byte-parity vs Python `mlx_lm.glm4`** — the proven bringup gate; localizes
  any weight-remap/norm/rope bug to a specific layer before trusting end-to-end output.

## Results

<!-- fill after implementation: parity gate, tokens/s, agentic-matrix vs gpt-oss/Qwen3.6 -->
