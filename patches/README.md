# Local patches awaiting upstream

## `mistralrs-qwen36-afq-wip.patch`

Work-in-progress patch against `EricLBuehler/mistral.rs` master
(commit `e1dd7c8` at time of writing) adding MLX AFQ-quantized weight
support to the GDN (Gated Delta Net) module so that `mistralrs` can
load Qwen3.5 / Qwen3.6 MLX checkpoints from `mlx-community/*-4bit`
in-process on Apple Silicon Metal.

### How to apply locally

```bash
git clone https://github.com/EricLBuehler/mistral.rs .vendor/mistral-rs
cd .vendor/mistral-rs
git apply ../../patches/mistralrs-qwen36-afq-wip.patch
```

Then in rozum's `Cargo.toml` switch the `mistralrs` dep to a path:

```toml
mistralrs = { path = ".vendor/mistral-rs/mistralrs", features = ["metal"], optional = true }
```

### What's in the patch

1. **`gdn/weights.rs`** — new `GdnInProj` enum with `Merged(Arc<dyn QuantMethod>)`
   (legacy path) and `SplitAfq { qkv, z, b, a }` (MLX path that loads each
   `in_proj_*` as its own `AfqLayer::afq_linear_b`). Combines outputs on the
   activation side in `GdnInProj::forward` so the downstream `GdnProjection::from_packed`
   sees the same packed layout the merged path would have produced.

2. **`gdn/weights.rs`** — `conv1d.weight` MLX layout fix: MLX ships
   `(out, kernel, in=1)` instead of candle's `(out, in=1, kernel)`. Permute on load.

3. **`gdn/layer.rs`** — switch `GatedDeltaNet.in_proj` field type to `GdnInProj`,
   route `project()` and `residual_input_projection_tensors()` through the new enum.

4. **`gdn/mod.rs`** — re-export `GdnInProj`.

5. **`models/qwen3_next.rs`** + **`vision_models/qwen3_5_moe/text.rs`** — ISQ
   collection skips `SplitAfq` (already quantised; no runtime quantisation needed).

6. **`vision_models/qwen3_5_moe/config.rs`** — custom `Deserialize` for `Config`
   that propagates the top-level `quantization_config` into `text_config.quantization_config`
   when the latter is missing (MLX checkpoints put AFQ config at the top level only).

### Day 2: per-tensor quantization overrides, MoE router, lm_head path

Following the day-1 patch the load progressed through `linear_attn` and then
hit `mlp.gate.weight` (256 experts × 8-bit AFQ packed) and `lm_head` (under
`language_model.lm_head.*`, not `lm_head.*`). The day-2 changes add:

1. **`mistralrs-quant/src/lib.rs`** — `QuantizedConfig::Afq` carries a new
   `overrides: HashMap<String, (bits, group_size)>` field; deserialiser scans
   sibling object-valued keys of the `quantization_config` map (MLX-style:
   `"language_model.model.layers.0.mlp.gate": {"bits": 8, "group_size": 64}`).
   Helper `afq_params_for_path(path)` returns the override if present,
   model-wide defaults otherwise.
2. **`mistralrs-quant/src/afq/mod.rs`** — `AfqLayer::afq_linear_b` and
   `afq_packed_linear_b` use `vb.prefix()` as the lookup key, so each layer
   picks up its per-tensor `(bits, group_size)`.
3. **`vision_models/qwen3_5_moe/text.rs`** — the MoE router (`gate`,
   `shared_expert_gate`) now loads through `ColumnParallelLayer::new` when
   `quantization_config` is present (was plain `linear_no_bias` before, which
   bypassed AFQ entirely).
4. **`vision_models/qwen3_5_moe/text.rs`** — `lm_head` falls back to
   `vb.pp("language_model").pp("lm_head")` when the root checkpoint stores it
   there (MLX layout).

### What works now

```
mistralrs: 'mlx-community/Qwen3.6-35B-A3B-4bit' ready
backend: mistralrs (in-process, Metal) — model: mlx-community/Qwen3.6-35B-A3B-4bit
```

Model loads end-to-end (load time ≈ 240 s the first time, ≈ 37 s warm), HTTP
chat completions return `200 OK` with valid JSON envelopes.

### What still doesn't (next: numerical correctness)

Responses come back with empty `content` and `completion_tokens: 0` — the
model immediately picks EOS. This points to a numerical error somewhere in
the new `SplitAfq` activation path (most likely the per-head interleave
in `GdnInProj::forward`'s SplitAfq branch — the four AFQ matmuls give the
right shapes but the cat/reshape order may not match the merged-weight
layout that `GdnProjection::from_packed` expects to slice).

Day 3 plan: dump activations of `GdnInProj::forward` for both paths on a
random input + the same model, diff. The merged path produces correct
logits (verified with non-MLX 4-bit GPTQ models earlier in mistralrs's
own test suite), so the SplitAfq path needs to match it tensor-equal.

Until day 3 lands, production Qwen3.6 use goes through the
`lmstudio-http-backend` path in `rozum launch`.
