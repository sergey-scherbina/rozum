# Native MLX: GLM-4.7-Flash (`glm4_moe_lite`) — absorbed MLA + DeepSeek-V3 routing

Status: scoped + scaffolded (operator 2026-06-27). Builds on the VALIDATED `deepseek_v2` MLA port
(master `b83003c`). Reference: Python `mlx_lm.models.glm4_moe_lite` + `mlx_lm.models.mla`
(MultiLinear) + `deepseek_v3` (routing). Target: GLM-4.7-Flash 4bit (16.9 GB, fits 36 GiB) — a
low-peak GLM-family coder that (unlike DeepSeek-V2-Lite/Devstral) is more likely tool-capable.

## Why it's a NEW port (not just deepseek_v2 reuse)

GLM-4.7-Flash's MLA is the **ABSORBED** form (low-memory), not deepseek_v2's naive form:
- `embed_q`, `unembed_out` are **`MultiLinear`** (per-head batched matmul, weight
  `[num_heads, out, in]`, `__call__(x, transpose=True)`) — absorb `kv_b_proj` into the attention.
- The KV **cache stores the COMPRESSED latent** `kv_latent [B,1,L,kv_lora_rank]` + `k_pe
  [B,1,L,qk_rope]` (single MQA head), NOT full per-head k/v → tiny cache. The standard two-array
  `update_and_fetch(kv_latent, k_pe)` likely reuses the existing cache (both concat on the seq axis)
  — VERIFY this fits `KeyValueCache`; if not, a small custom cache is needed.
- The rope part is scored separately and passed as an additive MASK: `pe_scores = (q_pe*scale) @
  k_pe.T`, then `SDPA(q_nope, k, v, scale, mask=pe_scores)`.
- **Decode (L==1) vs prefill (L>1) branch differently:** L==1 → `q_nope = embed_q(q_nope); k=v=kv_latent;
  … output = unembed_out(output)`. L>1 → `k = embed_q(kv_latent, transpose=False); v = unembed_out(kv_latent)`.

## Config (GLM-4.7-Flash)

q_lora_rank=768 (HAS q low-rank), kv_lora_rank=512, qk_nope=192, qk_rope=64, v_head_dim=256,
num_heads=20, partial_rotary_factor=1.0, n_routed_experts=64, num_experts_per_tok=4,
n_shared_experts=1, first_k_dense_replace=1, routed_scaling_factor=1.8, norm_topk_prob=true,
**topk_method=noaux_tc, scoring_func=sigmoid**, n_group=1.

## MoE routing (DeepSeek-V3 `noaux_tc`, from `group_expert_select`)

```
scores = sigmoid(gates.f32)
orig   = scores
scores = scores + e_score_correction_bias          # selection only
# n_group==1 here → no grouping
inds   = top_k(scores)                              # argpartition(-scores)[:k]
w      = take_along_axis(orig, inds)                # gather ORIGINAL sigmoid scores (not bias)
if norm_topk_prob: w = w / (w.sum(-1,keepdims) + 1e-20)
w     *= routed_scaling_factor                      # 1.8
y      = Σ w · switch_mlp(x, inds)  +  shared_expert(x)
```
`mlp.gate` carries a learned `e_score_correction_bias` param `[n_routed_experts]` (load it).

## Reuse vs new

- REUSE from `deepseek_v2.rs`: `DeepseekV2MLP` (dense + shared expert), `SwitchGlu` experts, the
  `mlp_moe`/`mlp_dense` Option per-layer pattern + load remap, Model/Inner/load/Generate, the YaRN
  scale boost. q_lora_rank Option path (Flash HAS q_lora=768).
- NEW: `MultiLinear` (per-head batched matmul — like a dense QSwitchLinear; mlx-rs may lack it →
  implement via `matmul`/`addmm` with a `[H,out,in]` weight). The absorbed-MLA forward (pe_scores
  mask + L-branch + embed_q/unembed_out). The V3 sigmoid+correction-bias gate (vs deepseek_v2's
  softmax). The compressed-KV cache wiring. Checkpoint naming: `self_attn.embed_q`/`unembed_out`,
  `mlp.gate.e_score_correction_bias`, `mlp.shared_experts.*`, `mlp.switch_mlp.*`.

## Plan (resume-cold)

1. [fork] `glm4_moe_lite.rs`: copy `deepseek_v2.rs`; swap MLA → absorbed form (MultiLinear embed_q/
   unembed_out + pe_scores-mask SDPA + L-branch); swap MoEGate → V3 sigmoid+correction-bias; keep
   MLP/MoE/Model/load/Generate. Register in mod.rs.
2. [fork] build green → push → bump rozum rev.
3. [rozum] dispatch `"glm4_moe_lite" => glm4_moe_lite::load_…`, `LoadedModel::Glm4MoeLite`, Generate.
4. [validate] byte-parity vs Python `mlx_lm` on GLM-4.7-Flash-4bit (slot-gated) — same probe harness
   as deepseek_v2 (which caught 4 bugs). Then agentic smoke: GLM-family is the best low-peak
   tool-driver hope (test with the new tool-injection if its template is template-less).
5. Land after fork branch → fork main + stable rev (as deepseek_v2 did, master `b83003c`).

## Effort

Larger than deepseek_v2 (the absorbed MLA + MultiLinear + L-branch are intricate). The MoE/Model/
load scaffolding is ~free (reuse). Budget a focused parity-iteration pass (the probe found 4 bugs in
deepseek_v2; expect similar here).
