# Native MLX: MLA attention + DeepSeek-V2 (shared kernel for low-footprint coders)

Status: in progress (operator 2026-06-27). Serves `green-matrix-min-footprint`: MLA unlocks two
low-peak coders — DeepSeek-Coder-V2-Lite (16B-**A2.4B**) and GLM-4.7-Flash (`glm4_moe_lite`).
Multi-repo: the model lives in the vendored `mlx-lm` fork (`feature/mla-deepseek-v2`).

## Why MLA

Multi-head **Latent** Attention compresses K/V (and Q) through low-rank projections, so the KV cache
is tiny → low peak RAM, which is exactly the footprint lever this program wants. It's the attention
of DeepSeek-V2/V3 and (with GLM extras) GLM-4.7-Flash. We have NO MLA in the native runtime; this is
the shared port.

## Reference

Port faithfully from Python `mlx_lm.models.deepseek_v2` (available via `uv run --with mlx-lm`):
`…/site-packages/mlx_lm/models/deepseek_v2.py`. (Routing for the GLM/V3 variant: `deepseek_v3.py`.)

## MLA attention — exact forward (from the reference)

Config: `q_lora_rank` (768 GLM-Flash / 1536 DSv2), `kv_lora_rank` (512), `qk_nope_head_dim` (192/128),
`qk_rope_head_dim` (64), `v_head_dim` (256/128), `num_attention_heads`, `rope_theta`, YaRN
`rope_scaling`. `q_head_dim = qk_nope_head_dim + qk_rope_head_dim`. `scale = q_head_dim**-0.5` (×
YaRN mscale² when `mscale_all_dim`).

Projections: `q_a_proj`(D→q_lora) → `q_a_layernorm`(RMS) → `q_b_proj`(q_lora→H·q_head_dim);
`kv_a_proj_with_mqa`(D→kv_lora+qk_rope) → split → `kv_a_layernorm`(RMS over kv_lora) →
`kv_b_proj`(kv_lora→H·(qk_nope+v_head_dim)); `o_proj`(H·v_head_dim→D).

Forward (B,L,D):
1. `q = q_b_proj(q_a_layernorm(q_a_proj(x)))` → `[B,L,H,q_head_dim]` → transpose → split
   `q_nope[qk_nope] | q_pe[qk_rope]`.
2. `ckv = kv_a_proj_with_mqa(x)` → split `compressed_kv[kv_lora] | k_pe[qk_rope]`; `k_pe`→`[B,1,L,qk_rope]` (MQA, one head).
3. `kv = kv_b_proj(kv_a_layernorm(compressed_kv))` → `[B,L,H,·]` → transpose → split `k_nope[qk_nope] | values[v_head_dim]`.
4. rope (YaRN) on `q_pe` & `k_pe` (offset = cache.offset); `k_pe = repeat(k_pe, H, axis=1)`.
5. `keys = concat(k_nope, k_pe)`; `queries = concat(q_nope, q_pe)`; cache stores `(keys, values)`.
6. `out = SDPA(queries, keys, values, scale, mask)` → transpose → `[B,L,H·v_head_dim]` → `o_proj`.

YaRN rope: `DeepseekV2YarnRotaryEmbedding` (mscale = yarn_get_mscale(factor, mscale)/yarn_get_mscale(
factor, mscale_all_dim); interpolated freqs). Transcribe from the reference; reuse our rope utils
where they match, else add the YaRN variant.

## Plan (resume-cold)

1. [fork] `deepseek_v2.rs`: ModelArgs (above) + `MlaAttention` (forward above) + YaRN rope +
   DeepSeek MoE block (reuse `qwen3_moe::{QSwitchLinear, SwitchGlu}`; routing = softmax/greedy top-k
   for DSv2 — the V3 sigmoid+correction-bias variant is for glm4_moe_lite) + dense first_k_dense
   layers + Model/load (AFQ remap; experts pre-stacked `mlp.switch_mlp.*`). Register in mod.rs.
2. [fork] `cargo build -p mlx-lm` (heavy, MLX C++). Push.
3. [rozum] dispatch `"deepseek_v2" => deepseek_v2::load_…`, `LoadedModel::DeepseekV2`, Generate arm;
   bump fork rev.
4. [validate] byte-parity vs Python `mlx_lm` on DeepSeek-Coder-V2-Lite (slot-gated). MoE-quant
   near-tie divergence acceptable ([[project-spec-decode-moe-numerics]]).
5. [reuse] glm4_moe_lite (GLM-4.7-Flash): MLA kernel + `embed_q`/`unembed_out` + V3 routing.

## Footprint payoff

DeepSeek-Coder-V2-Lite: 16B total but **A2.4B active** + MLA-compressed KV → peak well under the
dense 30-32B coders. A prime candidate for the lowest-peak green-matrix config (standalone or as a
pipeline tier).
