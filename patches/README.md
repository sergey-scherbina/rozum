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

### What's still missing (known)

After applying, the load progresses through `linear_attn.in_proj_*` and
`linear_attn.conv1d.weight` correctly but then hits:

```
shape mismatch for language_model.model.layers.0.mlp.gate.weight,
expected: [256, 2048], got: [256, 512]
```

This is the **per-layer quantization override** problem: Qwen3.6's MLX checkpoint
runs the MoE router (`mlp.gate.weight`) at **8-bit** AFQ while the rest of the
model is **4-bit**. The MLX config encodes this via per-layer keys:

```jsonc
"quantization_config": {
  "group_size": 64,
  "bits": 4,
  "language_model.model.layers.0.mlp.gate": { "group_size": 64, "bits": 8 },
  ...
}
```

mistralrs's `QuantizedConfig::Afq { bits, group_size }` is a single model-wide
value with no per-tensor override mechanism. Adding it would touch the
`QuantizedConfig` deserializer + every loader that calls `AfqLayer::afq_linear_b`
to consult the per-tensor table by full tensor path.

This is a multi-day extension. The patch as it stands is committed for future
continuation; the immediate Qwen3.6 use case is unblocked via the
`lmstudio-http-backend` path in `rozum launch`.
