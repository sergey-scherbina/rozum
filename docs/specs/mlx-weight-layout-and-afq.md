# MLX weight layout and AFQ quantization: integration findings

## Purpose

Distilled, prescriptive spec that captures **everything we learned** while
integrating `mlx-community/Qwen3.6-35B-A3B-4bit` into `mistralrs`. The goal is
that the next person (us or another team) wiring a fresh MLX checkpoint into a
Rust LLM runtime can start from this document instead of from a blank page.

Scope: MLX safetensors layout, AFQ (Adaptive Filter Quantization) on-disk and
in-Metal representation, the per-tensor quantization override mechanism, the
weight-row-ordering convention difference between MLX and PyTorch checkpoints,
and a diagnostic methodology proven to localize numerical drift in
quantized forward passes.

This spec is descriptive of facts about MLX, not aspirational. Every section
is grounded in either Apple's `mlx-lm` Python source or in instrumented
side-by-side runs we performed.

---

## 1. AFQ on-disk format

### 1.1 Three-tensor representation

Every quantized linear weight is stored as three sibling tensors under the
same prefix:

```
<prefix>.weight       uint32   (rows, cols * bits / 32)
<prefix>.scales       bf16     (rows, cols / group_size)
<prefix>.biases       bf16     (rows, cols / group_size)
```

Where `(rows, cols)` is the **logical** weight shape (`out_features`,
`in_features` for a `nn.Linear`, or `vocab_size`, `hidden_size` for an
embedding). `bits` is the per-tensor quantization width (typically 4 or 8).
`group_size` is 64 by default in Qwen3.6's MLX checkpoint.

Storage size:
- `weight` packs `32 / bits` 4-bit nibbles (or 8-bit bytes, etc) per uint32.
  For `bits = 4`, `cols` logical columns occupy `cols / 8` uint32 slots.
- `scales` and `biases` are bf16, one value per group of `group_size`
  contiguous columns within each row.

### 1.2 Nibble packing inside uint32 (4-bit case)

This is the single most error-prone fact: **MLX packs nibbles LSB-first
inside each uint32**, and Metal then reads the buffer as `uint8_t*` and
unpacks two nibbles per byte low-nibble-first.

Concretely, for one uint32 value `W = 0xN7 N6 N5 N4 N3 N2 N1 N0` (in MSB-down
hex notation), the eight logical 4-bit values it represents are:

```
out[0] = (W >>  0) & 0xF   = N0
out[1] = (W >>  4) & 0xF   = N1
out[2] = (W >>  8) & 0xF   = N2
out[3] = (W >> 12) & 0xF   = N3
out[4] = (W >> 16) & 0xF   = N4
out[5] = (W >> 20) & 0xF   = N5
out[6] = (W >> 24) & 0xF   = N6
out[7] = (W >> 28) & 0xF   = N7
```

Worked verification: row 198 of `embed_tokens.weight` in
`Qwen3.6-35B-A3B-4bit`, column 0:

```
w_q[198, 0]   = 4007591150 = 0xEEDEFCEE
scales[198, 0] = 0.0014801025390625
biases[198, 0] = -0.020751953125

LSB-first nibbles: [0xE, 0xE, 0xC, 0xF, 0xE, 0xD, 0xE, 0xE]
                 = [14, 14, 12, 15, 14, 13, 14, 14]

dequantized [0..8] = scales * d + biases for each d above
                   = [-3.05e-5, -3.05e-5, -0.003, 0.0014, -3.05e-5,
                       -0.0015, -3.05e-5, -3.05e-5]
```

This matches Python `mlx.dequantize(...)` exactly within bf16 LSB rounding,
and it matches the bytes Metal reads when the buffer is interpreted as
`uint8_t*` (a single uint32 maps to four little-endian bytes, each byte holds
two nibbles low-first - same nibble sequence by either reading discipline).

### 1.3 Dequantization formula

For row `i` column `j`:

```
g = j / group_size
w[i, j] = scales[i, g] * unpack4(w_q[i, j / 8])[j % 8] + biases[i, g]
```

There is **no zero-point subtraction**: AFQ is the additive `scale * q + bias`
affine form, not the subtractive `scale * (q - zp)` form. If you see code
that subtracts a zero point, that is GPTQ / AWQ / bnb territory, not AFQ.

### 1.4 Metal kernel sanity

`mistralrs-quant/src/metal_kernels/quantized.metal::affine_dequantize` treats
the weight buffer as `const device uint8_t*` and indexes by byte. For
`bits = 4`, each thread reads one byte and emits two output values. We
verified that the Metal kernel reproduces the bit-exact dequantization above;
the kernel is not the source of any of the bugs we hit.

---

## 2. Per-tensor quantization overrides

### 2.1 The MLX convention

Qwen3.6's `config.json` `quantization_config` block carries the global
defaults at the top level and per-path overrides as **sibling map entries**:

```jsonc
"quantization_config": {
  "group_size": 64,
  "bits": 4,
  "language_model.model.layers.0.mlp.gate":               { "group_size": 64, "bits": 8 },
  "language_model.model.layers.0.mlp.shared_expert_gate": { "group_size": 64, "bits": 8 },
  "language_model.model.layers.1.mlp.gate":               { "group_size": 64, "bits": 8 },
  ...
}
```

Each override key is the **full dotted path** of the linear module whose
weight uses a different quantization. In Qwen3.6, the MoE routers
(`mlp.gate`, `mlp.shared_expert_gate`) run at 8-bit while everything else is
4-bit. That dramatically affects packed shape: an 8-bit
`(256, 2048)` weight is stored as `(256, 512)` uint32; a 4-bit one would be
`(256, 256)`. Using the wrong width produces a shape mismatch on load.

### 2.2 Required Rust-side wiring

Three pieces, none of which a one-bits/one-group_size deserializer gives you
for free:

1. **Deserializer** that retains both the global `(bits, group_size)` and an
   `overrides: HashMap<String, (bits, group_size)>` populated from every
   sibling object-valued key under `quantization_config`.
2. **Path-aware lookup** at every `AfqLayer::afq_linear_b` /
   `afq_packed_linear_b` call site. The lookup uses the `VarBuilder`'s
   accumulated prefix (`vb.prefix()`) verbatim as the override key, falling
   back to the global defaults if absent.
3. **Skip path-resolved override entries in ISQ collection**. In-situ
   quantization scans tensors that need to be runtime-quantized; AFQ-loaded
   layers must be excluded.

### 2.3 Where the overrides live in Qwen3.6

Empirically (from the model card and config inspection):

- Every `layers.*.mlp.gate` is 8-bit.
- Every `layers.*.mlp.shared_expert_gate` is 8-bit.
- Everything else (input projections, conv1d if quantized, attention
  projections, expert MLPs, lm_head, embed_tokens) is 4-bit.

The model-wide default in the same `quantization_config` block remains
`bits=4, group_size=64`, so the overrides are only listed for the 8-bit
exceptions.

---

## 3. Top-level vs nested config blocks

### 3.1 The nesting trap

MLX checkpoints derived from multimodal-capable architectures put text-only
hyperparameters under a nested `text_config` block but keep
`quantization_config` at the **top level** of `config.json`. mistralrs's
`vision_models/qwen3_5_moe::Config` derives `Deserialize` straight onto
`text_config`, so the top-level `quantization_config` silently never reaches
the text submodule and every quantized layer falls back to an
"unquantized" branch that then fails to find the `.weight` tensor at the
expected non-packed shape.

### 3.2 Fix

Hand-rolled `Deserialize` on the outer `Config` that:

1. Parses the JSON into a `serde_json::Map`.
2. Lifts `quantization_config` into `text_config.quantization_config` when
   the latter is missing.
3. Delegates to the original derived `Deserialize` for the populated map.

This is a one-time pattern but you must remember to apply it for every
multimodal-shape MLX repo (Qwen3.5 family, Qwen3.6 family, future MLX VLMs).

---

## 4. lm_head path

Qwen3.6 MLX stores the LM head under `language_model.lm_head.{weight,
scales, biases}` rather than the bare `lm_head.*` that `qwen3_5_moe`
upstream looks for. The fix is a fallback `vb.pp("language_model").pp("lm_head")`
when the first attempt errors with "tensor not found".

This is not unique to lm_head: the entire model lives under `language_model.`
in MLX multimodal-capable architectures, even when the checkpoint is
text-only. Treat it as "always try the bare path first, fall back to
`language_model.` prefix on failure".

---

## 5. Conv1d weight axis convention

mistralrs / candle expects 1D convolution weights in PyTorch layout:

```
conv1d.weight shape: (out_channels, in_channels / groups, kernel_size)
```

MLX ships:

```
conv1d.weight shape: (out_channels, kernel_size, in_channels / groups)
```

For the `GatedDeltaNet.conv1d` (depthwise, `groups = out_channels`, so the
middle dim is 1):

- PyTorch / candle: `(8192, 1, 4)`
- MLX: `(8192, 4, 1)`

Fix: on load, if the tensor comes back with shape `(out, kernel, 1)`, do
`permute(0, 2, 1)` to land in candle's expected `(out, 1, kernel)`. Detecting
which checkpoint you got by trying the native shape first and permuting on
shape-mismatch error is a workable strategy; a config-driven discriminator
would be cleaner if MLX ever flips conventions again.

---

## 6. **The big one**: weight-row ordering for fused QKV-style projections

This is the bug that ate days of debugging. It deserves its own section and
its own warning.

### 6.1 Two valid conventions

Any linear projection that produces **multiple per-head outputs concatenated
into one tensor** can lay its rows in two different orders:

**Convention A: per-head-interleaved** (used by Qwen3-Next non-MLX
checkpoints, used by mistralrs's `Qwen3NextLoader::load_split_qkvz`,
implicit in the `from_packed` slicer):

```
W rows: [h0_q, h0_k, h0_v, h0_z,
         h1_q, h1_k, h1_v, h1_z,
         ...
         h15_q, h15_k, h15_v, h15_z]

shape: (num_heads * (qd + kd + vd + zd), hidden)
```

The activation is computed as one big matmul, reshaped to
`(B, S, num_heads, qd + kd + vd + zd)`, then sliced along the last axis.

**Convention B: flat-per-type** (used by MLX safetensors, used by Python
`mlx_lm/models/qwen3_5.py`):

```
W_qkv rows: [q for all heads, k for all heads, v for all heads]
W_z   rows: [z for all heads]

shapes: W_qkv (key_dim*2 + value_dim, hidden), W_z (value_dim, hidden)
```

The activation is computed as **separate matmuls per type**, split by type,
then each piece is independently per-head reshaped, and finally concatenated
along the per-head dimension.

### 6.2 Why this matters

Both conventions are mathematically valid; they only differ in **the order
in which rows appear in the stored tensor**. A loader that assumes
Convention A and reads Convention B weights produces numerically wrong
matmul outputs even though every weight value is loaded correctly.

The dequantized tensors are bit-identical to Python (we verified `w_q`,
`scales`, `biases` byte-for-byte for `embed_tokens.weight` row 198), but the
matmul interprets them under the wrong layout convention and the rest of the
forward pass diverges.

### 6.3 Diagnostic: top-1 logit drift signature

After loading Qwen3.6 with the wrong convention and running a fixed
11-token chat-template prompt:

```
                 top-1 id     top-1 logit    Python top-1
unfixed          95886        14.19          8160 ('Here'), logit 22.0
wrong merge      22           14.88          8160
day-5 split fix  220          17.38          8160
```

Top-1 changing and top logit growing toward the Python reference is the
signature of progressively peeling off layout bugs. When top logits reach
the Python magnitude (~22) and top-1 stabilizes on the real token, the bug
is gone.

### 6.4 Where to expect the bug in any MLX model

The bug appears in **every fused projection** that bundles multiple
per-head logical tensors into one storage tensor. In Qwen3.6 specifically:

1. **GatedDeltaNet `in_proj_qkv`** (every linear-attention block, ~75% of
   layers in Qwen3.6).
2. **FullAttention `qkv_proj`** (every fourth layer). The non-MLX
   Qwen3-Next path uses Convention A; MLX uses Convention B with
   `q_proj`, `k_proj`, `v_proj` as **separate** tensors.
3. **MoE fused `switch_mlp.{gate_proj, up_proj, down_proj}`**: shapes
   `(num_experts, out, in)`. Convention A would interleave experts;
   Convention B keeps each expert's projection contiguous. MLX is
   Convention B but stores each expert as a separate `experts.<i>.<name>`
   tensor that an MLX loader fuses, while mistralrs upstream expects the
   already-fused layout from non-MLX checkpoints.

### 6.5 Fix pattern

For every fused-projection module, split the work along Convention B:

```rust
// One matmul per logical output type.
let qkv_out = qkv.forward(x)?;          // (..., key_dim*2 + value_dim)
let z_out   = z.forward(x)?;            // (..., value_dim)
let b_out   = b.forward(x)?;            // (..., num_v_heads)
let a_out   = a.forward(x)?;            // (..., num_v_heads)

// Split the flat qkv output by type along its last axis.
let last = qkv_out.dims().len() - 1;
let q = qkv_out.narrow(last, 0,            dims.key_dim)?;
let k = qkv_out.narrow(last, dims.key_dim, dims.key_dim)?;
let v = qkv_out.narrow(last, 2 * dims.key_dim, dims.value_dim)?;

// Per-head reshape each piece independently.
let q = q.reshape((.., dims.num_k_heads, dims.head_k_dim))?;
let k = k.reshape((.., dims.num_k_heads, dims.head_k_dim))?;
let v = v.reshape((.., dims.num_k_heads, dims.v_per_group * dims.head_v_dim))?;
let z = z_out.reshape((.., dims.num_k_heads, dims.v_per_group * dims.head_v_dim))?;
let b = b_out.reshape((.., dims.num_k_heads, dims.v_per_group))?;
let a = a_out.reshape((.., dims.num_k_heads, dims.v_per_group))?;

// Concatenate along the per-head axis, re-flatten to the layout the
// downstream Convention-A slicer expects.
let qkvz = Tensor::cat(&[q, k, v, z], D::Minus1)?
    .contiguous()?
    .reshape((.., dims.qkvz_out_dim()))?;
let ba = Tensor::cat(&[b, a], D::Minus1)?
    .contiguous()?
    .reshape((.., dims.ba_out_dim()))?;
Tensor::cat(&[qkvz, ba], D::Minus1)
```

Doing the dequant-then-merge equivalent on the **weight** side instead of
the activation side is mathematically equivalent but allocates the entire
full-precision merged weight every forward pass: O(weight bytes) extra
memory and bandwidth per token. Always prefer the activation-side fix.

### 6.6 What does NOT fix it

We tried these and they did not work; document them so we don't re-try:

- Reshape `qkv_w` of shape `(key_dim*2 + value_dim, hidden)` directly into
  `(num_heads, kd*2 + vd_per_group * head_vd, hidden)`. This treats flat
  rows as if they were per-head interleaved and silently scrambles q, k, v
  values between heads. Symptom: garbage output with multilingual
  hallucinations.
- Tweaking the conv1d permute. The permute fix is real and necessary, but
  it does not interact with the QKV layout bug.
- Disabling tool-use formatting, changing chat template, varying sampling
  temperature. The model picks garbage tokens deterministically; sampling
  is not involved.

---

## 7. Embedding lookup with AFQ weights

For an AFQ-quantized `embed_tokens.weight`:

1. Construct an `AfqLayer::afq_linear_b(hidden_size, vocab_size, ...)` with
   `bias = false`. Note the **swapped** order of arguments: the layer's
   in_dim is `hidden_size` and out_dim is `vocab_size`, which is the
   opposite of what `Linear::new(in, out)` would suggest. This matches
   `(vocab_size, hidden_size)` row-major weight shape.
2. Dequantize the layer's weight to a full-precision `(vocab_size,
   hidden_size)` tensor with `afq_layer.dequantize_w()`.
3. Wrap in candle's `Embedding::new(weight, hidden_size)` and call
   `forward(input_ids)`.

This produces the same per-token embedding vectors as Python's
`QuantizedEmbedding.__call__(input_ids)` to within bf16 rounding. We
verified this end-to-end for the actual chat-template prompt; the bug is
**not** in the embedding lookup.

The convention-mismatch bug from section 6 does **not** apply to embedding
because there are no fused per-head logical tensors here - the embedding
weight is a single `(vocab, hidden)` matrix with no internal structure.

---

## 8. Diagnostic methodology

This is the workflow that converged. Future MLX integrations should reach
for it on the first sign of "model loads but predicts garbage":

### 8.1 Side-by-side Python reference

Write a small script (`scripts/mlx_ref.py` in this repo is the canonical
template) that:

1. Loads the exact same MLX repo via `mlx_lm.load(...)`.
2. Renders the exact same prompt via `tokenizer.apply_chat_template`.
3. Prints the prompt token ids, the embedding for the last position, the
   embedding stats (`min`, `max`, `mean`, `||x||`), and the top-10 logits
   with token strings.

This becomes the ground truth oracle for every comparison below.

### 8.2 Mirror dumps in the Rust runtime

Tap the equivalent points in mistralrs with environment-variable-gated
`eprintln!`:

- `INPUT_IDS` immediately before `embed_tokens.forward(input_ids)` to
  confirm the tokenizer matches.
- `EMBED last-pos[0..8]` and `EMBED last-pos stats` after the embedding
  call to confirm the AFQ load + dequant matches.
- `top-10` after `lm_head` to confirm the full forward matches.

The byte-for-byte dump of one row of `w_q`, `scales`, `biases` for a
specific token id (we used 198) is the load-time correctness check. If
that diverges, the loader is wrong. If it matches but the embedding
output diverges, the dequant kernel is wrong (unlikely; we verified it
is correct). If embedding matches but logits diverge, every forward
module is suspect - go layer by layer.

### 8.3 Single-prompt determinism

Use one fixed prompt across all dumps. We used `"Hello"` rendered through
the chat template, which yields exactly 11 tokens for Qwen3.6. Greedy
decoding with `temperature = 0`. Any change in any tensor across runs
must come from a code change, not from sampling.

### 8.4 Progress signal

When debugging a layout bug, the **top logit magnitude** drifting toward
the Python reference is a reliable signal that you are peeling off the
right bugs. A purely random forward gives top logits ~1-3 standard
deviations above the mean (~3-5 in absolute value for bf16 logits). A
correctly-tuned forward produces top logits in the 15-25 range. Anything
in between means you have some layout right and some wrong.

---

## 9. Per-tensor debug instrumentation

Keep these env-gated prints in place behind feature-test guards; they are
expensive but only fire when explicitly requested:

- `ROZUM_AFQ_DEBUG=1` - log every `AfqLayer::afq_linear_b` call with
  prefix, bits, group_size, weight shape. Catches missing overrides.
- `ROZUM_AFQ_DUMP_EMBED=<token_id>` - on the embedding layer load, print
  `w_q[token_id, 0..5]`, `scales[token_id, 0..5]`, `biases[token_id, 0..5]`.
  Catches loader bugs.
- `ROZUM_FWD_DEBUG=1` - print `INPUT_IDS`, `EMBED last-pos`, `top-10`
  at every forward pass. Catches forward bugs at the model boundaries.

These cost nothing at runtime when unset (one `var()` lookup per call),
and they shave days off the next debugging session.

---

## 10. Recommended sequence for the next MLX integration

When wiring up an unfamiliar MLX checkpoint into a Rust runtime:

1. Read its `config.json`. Note `model_type`, `architectures`, the
   nested `text_config` shape, the full `quantization_config` block with
   overrides, the `layer_types` array if present, the `rope_parameters`
   block, and any conv-layer hyperparameters.
2. Read its `model.safetensors.index.json`. Cross-reference every tensor
   name against what the Rust loader expects. Catalog missing tensors,
   extra tensors, and shape mismatches before touching code.
3. Locate the corresponding `mlx_lm/models/<name>.py`. This is your
   canonical reference for what `forward` actually computes.
4. Implement the per-tensor-override deserializer **before** wiring any
   AFQ layer. The overrides will bite you mid-load otherwise.
5. Audit every fused-projection module on both sides. For each one, list
   whether MLX uses Convention A or Convention B (it is almost always B)
   and whether the Rust runtime's loader assumes A. Plan the
   activation-side split fix before running anything.
6. Stand up the Python side-by-side script and the env-gated dump
   instrumentation before chasing any specific divergence.
7. Verify in order: tokenizer, embedding, layer 0, every fourth layer
   thereafter, lm_head. The first divergence is the next bug.

This order has the property that each step's exit criterion is testable
in isolation, so you can pause work at any layer boundary without losing
context.

---

## 11. Cross-references

- `docs/specs/mistralrs-qwen36-pr.md` - the upstream-PR-shaped writeup of
  what an end-state mistralrs change would look like.
- `docs/specs/mistralrs-backend.md` - rozum's in-process mistralrs wrapper.
- `docs/specs/mlx-native-port.md` - phased plan for an own MLX runtime
  built on `mlx-rs`; section 1 of this spec applies to any such effort
  verbatim.
- `patches/mistralrs-qwen36-afq-wip.patch` - the actual code that
  implements every fix listed here.
- `patches/README.md` - the day-by-day debugging log; this spec is the
  permanent distillation, that file is the timeline.
- `scripts/mlx_ref.py` - the side-by-side Python reference script
  template.

---

## 12. Open questions

These remain unverified and are the obvious next experiments:

- Does the FullAttention `q_proj` / `k_proj` / `v_proj` triple in
  Qwen3.6 MLX use Convention B for each? Confirmed by reading
  `mlx_lm/models/qwen3_next.py::Qwen3NextAttention`, but not yet
  byte-for-byte verified against the Rust load.
- Does the MoE fused `switch_mlp` need expert-by-expert reconstruction
  from MLX's per-expert tensors, or does the MLX checkpoint already ship
  the fused `(num_experts, out, in)` shape that mistralrs expects?
- Does the `rope_parameters.mrope_section: [11, 11, 10]` block apply at
  all in a text-only forward? Python `mlx_lm` appears to ignore it for
  text-only inputs; mistralrs's `Qwen3VLRotaryEmbedding` may not handle
  the all-text case identically. Worth a focused comparison once the
  layout bugs are out.

Each of these is a candidate divergence point and should be the next
focus of the side-by-side dump methodology in section 8.
