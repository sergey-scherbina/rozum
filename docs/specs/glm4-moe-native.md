# Native MLX support for GLM-4 MoE (`glm4_moe` / `glm4_moe_lite`)

Status: planned (operator 2026-06-27). Spec-first per `spec-dev`; the port itself is multi-repo
(the model lives in the vendored `mlx-lm` fork, not in `rozum`).

## Why / which tasks

The e2e agentic matrix (`scripts/bench/agentic.sh`: tasks `greet build fix test debug`, plus the
RPN/create-from-scratch probes) is standardized on `Qwen3.6-35B-A3B` + `gpt-oss-20b`. gpt-oss-20b
is an unreliable agentic code-writer ([[project-gptoss-agentic-codegen-unreliable]]); the GLM
family is matrix-proven (GLM-4-32B RPN 3/3; GLM-4-0414 reliable for edit/debug/create after
artifact-synth). The modern GLM coders are **MoE** (`glm4_moe`: GLM-4.5-Air, GLM-4.6;
`glm4_moe_lite`: GLM-4.7-Flash), which rozum's native MLX runtime does NOT load — it only handles
dense `glm4`. Porting the MoE family gives the matrix a **second reliable model family besides
Qwen** (de-risks model-specific flake, since the matrix is read as cross-model pass-rate —
[[project-matrix-nondeterminism]]) and a 36 GiB-fitting modern coder: **GLM-4.7-Flash 4-bit =
16.9 GB**, incl. a Claude-Opus-4.5 reasoning-distill variant aimed straight at RPN-style
reason→code.

Fit on the 36 GiB host: GLM-4.7-Flash 4bit 16.9 GB ✅; GLM-4.6 / 4.5-Air are large → catalog only.

## Where the code lives (multi-repo)

- Model: **`.vendor/mlx-lm/mlx-lm/src/models/glm4_moe.rs`** (fork `sergey-scherbina/mlx-rs`).
  Branch `feature/glm4-moe`. Reuses `glm4.rs` (attention/norms) + the MoE machinery in
  `qwen3_moe.rs` / `qwen3_5_moe.rs`.
- Wiring: **`rozum` `crates/rozum-mlx/src/mlx_native_backend.rs`** — add a `LoadedModel::Glm4Moe`
  variant, dispatch arms, and a `Generate` arm. Branch `feature/mlx-glm4-moe`.
- Pin: bump the fork rev in `crates/rozum-mlx/Cargo.toml` (`mlx-rs`/`mlx-lm`) once the fork lands.

## Reuse map (what to copy vs write new)

Base the file on `glm4.rs` and keep VERBATIM:
- `Attention` — GLM partial + traditional RoPE (rotate first `head_dim*partial_rotary_factor`
  dims), q/k/v/o, the `AttentionInput`/cache plumbing.
- The **4-norm sandwich** `DecoderLayer`: `x = x + post_self_attn_layernorm(attn(input_layernorm(x)))`
  then `x = x + post_mlp_layernorm(ffn(post_attention_layernorm(x)))`.
- `Model`, embedding, final norm, `load_glm4_model` skeleton (rename `*_glm4_moe_*`).

Replace the dense `Mlp` with a MoE FFN adapted from `qwen3_moe.rs`:
- Reuse `QSwitchLinear` + `SwitchGlu` (AFQ experts via `gather_qmm`, sorted prefill path) AS-IS.
- The router (`SparseMoeBlock`) needs GLM-specific changes — **this is the real work**, NOT a
  plain graft (qwen3_moe is flat softmax top-k; GLM-4 MoE is DeepSeek-V3-style):
  1. **Sigmoid gating** (not softmax) on router logits.
  2. **Grouped top-k**: `n_group` / `topk_group` group-limited expert selection.
  3. **`e_score_correction_bias`** added to scores for selection (bias not applied to the
     combine weights).
  4. **`routed_scaling_factor`** multiplies the combined routed output.
  5. **Shared expert(s)** (`n_shared_experts`): a dense MLP whose output is ADDED to the routed
     output (GLM/DeepSeek have it; plain Qwen3-MoE does not — but `qwen3_5_moe.rs` may already,
     check there first to reuse).
  6. **`first_k_dense_layers`**: the first k decoder layers use the dense `Mlp`, the rest MoE —
     per-layer choice in `DecoderLayer::new`.
  7. **MTP layers**: GLM-4 MoE ships multi-token-prediction layers — **drop them** for greedy
     inference (load only `num_hidden_layers`; ignore `num_nextn_predict_layers`).

`glm4_moe` vs `glm4_moe_lite`: same family; lite (GLM-4.7-Flash) = fewer layers/experts and may
omit grouped routing. Drive everything from config (`ModelArgs`), gate group-routing on
`n_group > 1`, so one module serves both `model_type`s.

## Config fields to add to `ModelArgs`

`num_experts` (a.k.a. `n_routed_experts`), `num_experts_per_tok`, `moe_intermediate_size`,
`n_shared_experts`, `norm_topk_prob`, `n_group`, `topk_group`, `routed_scaling_factor`,
`first_k_dense_layers`, `router_bits` (router may be 8-bit), `num_nextn_predict_layers` (parsed
then ignored). Pull the exact names from the model's `config.json`
(`zai-org/GLM-4.7-Flash`, `zai-org/GLM-4.6`).

## Validation (byte-parity oracle — 🛑 loads a model → slot protocol)

Same method as the existing GLM-4/gpt-oss ports: greedy byte-exact vs Python `mlx_lm` on the same
prompt/quant. Oracle = `uv venv python3.12` + `mlx_lm` (our 3.14 too new). The expected MoE-quant
near-tie divergence is acceptable ([[project-spec-decode-moe-numerics]]: `gather_qmm` not
bit-invariant to seq-length) — judge on argmax agreement, not bit-identity, for quantized MoE.
**Before any run: claim the model slot in the rozum room + obey the 🛑 REBOOT-SAFETY PROTOCOL
(one model-loaded gateway on this 36 GiB host at a time, BUG-003).**

## Steps (resume-cold)

1. [fork] `glm4_moe.rs`: copy `glm4.rs`; add MoE `ModelArgs` fields; add `SparseMoeBlock`
   (sigmoid + grouped top-k + correction-bias + routed-scaling + shared expert); per-layer
   dense/MoE via `first_k_dense_layers`; drop MTP. `load_glm4_moe_model`. Register in
   `mlx-lm/src/models/mod.rs`.
2. [fork] `cargo build -p mlx-lm` green (heavy — builds MLX C++). Push fork branch.
3. [rozum] bump `mlx-rs`/`mlx-lm` rev in `crates/rozum-mlx/Cargo.toml`.
4. [rozum] wire `mlx_native_backend.rs`: `LoadedModel::Glm4Moe`, dispatch
   `"glm4_moe" | "glm4_moe_lite" => glm4_moe::load_glm4_moe_model(dir)`, `Generate` arm.
5. [validate] byte-parity vs Python (slot protocol). Then add GLM-4.7-Flash to the matrix and
   run `agentic.sh` on `rpn build fix test debug`.
6. Land: fork PR + rozum `feature/mlx-glm4-moe:master`; prepend `CHANGELOG.md`.
