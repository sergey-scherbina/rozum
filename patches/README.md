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

### Day 3: layout fix in SplitAfq.forward (still EOS-on-first-token)

Tried a layout fix in `GdnInProj::SplitAfq.forward`: the four per-AFQ-layer
matmuls produce **flat** outputs (`[q_all_heads ‖ k_all_heads ‖ v_all_heads]`),
not the per-head sequential layout the merged-weight path produces. The fix
splits qkv into q/k/v, reshapes each into per-head form, and concatenates
along the last axis so `GdnProjection::from_packed` downstream sees the same
layout it would have seen from the merged matmul.

The Python reference (`mlx_lm/models/qwen3_5.py`) confirms qkv flat layout:
```python
qkv = self.in_proj_qkv(inputs)       # (B, S, key_dim*2 + value_dim) flat
...
q, k, v = [t.reshape(B, S, h, d) for t in mx.split(qkv, [key_dim, 2*key_dim], -1)]
```

After applying the layout fix, the model still picks EOS on the first
generated token (empty `content`, `completion_tokens=0`). The numerical bug
is somewhere else. Verified shapes against the actual safetensors:
- `linear_attn.conv1d.weight` is `[8192, 4, 1] BF16` (MLX layout, permute fix correct)
- `linear_attn.in_proj_qkv.weight` is `[8192, 256] U32` (4-bit packed, `out × in*bits/32`)
- `mlp.gate.weight` is `[256, 512] U32` (8-bit packed, our per-tensor override correct)
- `embed_tokens.weight` is `[248320, 256] U32` (4-bit packed, `layers::embedding` is AFQ-aware)

Likely remaining culprits (in decreasing order of suspicion):
- **mrope_section**: Qwen3.6's `rope_parameters.mrope_section: [11, 11, 10]`
  describes a 3D positional embedding for multimodal text+image+video. Our
  text-only path may apply 1D RoPE when the model expects multi-section.
- **gated_delta_update**: the SSM-style recurrent update path may have its
  own numerical edge case when the inputs come from `GdnInProj::SplitAfq`.
- **chat template**: Qwen3.6 uses a "thinking-mode" template; if the template
  applier sends the wrong system prompt, the model may emit EOS expecting
  no user turn.

### Day 4 plan

*Instrumented activation diff.* The cleanest probe is to load the same model
via Python `mlx_lm` and our patched mistralrs, run the same single-token
prompt through both, and dump intermediate tensors at every layer boundary
(post-embed, post-linear_attn, post-MoE, post-norm, pre-lm_head, post-lm_head).
First divergence point pinpoints the bug. Estimated 4-8 hours focused.

### Day 4 results: bug is NOT in our patches

Direct test through `cargo run -p mistralrs --bin mistralrs -- run -m
mlx-community/Qwen3.6-35B-A3B-4bit` (CLI, bypassing our gateway):

- Model loads cleanly.
- Generates tokens at **66 tok/s on M4 Max** — clearly using Metal.
- **Output is multilingual garbage** (Korean / Chinese / Cyrillic / English /
  emoji randomly interleaved) — text-grammar nonsense at every position.

This rules out:
- Our chat-template path (we bypassed our gateway).
- Our split-then-reshape `SplitAfq.forward` (we also tried a
  dequantise-then-merge variant — same garbage).
- Weight-loading errors (debug instrumentation confirms every AfqLayer
  loads with the right path / bits / group_size, including the 8-bit
  overrides for `mlp.gate` and `mlp.shared_expert_gate`, and the fused
  experts under `switch_mlp.{gate_proj,up_proj,down_proj}`).

What remains: the bug is in mistralrs's **forward computation** for
Qwen3.6, not in our load-time patches. Candidates:

1. **mrope_section: [11, 11, 10]** — 3D multimodal RoPE applied incorrectly
   in text-only path. mistralrs's `Qwen3VLRotaryEmbedding` (used by
   `vision_models/qwen3_5_moe`) may not handle the text-only case the way
   the MLX Python reference does.
2. **gated_delta_update** SSM kernel — numerical edge case.
3. **MoE expert routing** — fused `switch_mlp` path may have a layout drift
   we can't see from outside.

Resolution path requires **Python side-by-side instrumentation**:
1. `pip install mlx-lm`
2. Load the same model in Python: `from mlx_lm import load, generate`
3. Tap intermediate activations at every layer boundary.
4. Run the same single-token prompt through our patched mistralrs binary
   with `eprintln!` taps at the matching boundaries.
5. First divergence point pinpoints the bug.

Estimated 1-2 day project for a focused session with the Python env ready.

### Production Qwen3.6 today

The patch in this directory is **not** ready to use for inference. For real
work, route through LM Studio (already shipped as `lmstudio-http-backend`):

```bash
# In LM Studio app:
#   - Search tab → install mlx-community/Qwen3.6-35B-A3B-4bit
#   - Developer tab → start server on port 1234
rozum launch --model qwen/qwen3.6-35b-a3b claude
```

LM Studio's native MLX runtime handles Qwen3.6 correctly because it shares
the mlx_lm Python forward path. rozum auto-detects port 1234 and proxies
to it.
