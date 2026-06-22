# Constrained tool delivery for gpt-oss (harmony) — close the format-guarantee gap

Status: **spec / in progress** (plucky-finch 2026-06-22). Goal: a model that CAN solve the
task should also reliably DELIVER it. Helps green-matrix `matrix-gptoss-codex-{build,test,debug}`.

## The finding (root cause, our stack — not just a model ceiling)

rozum's MLX-native masked decoder forces **schema-valid tool-call output** and is **on by
default** (`ROZUM_MLX_CONSTRAIN`). The code itself records that it is decisive: on
Qwen3.6-35B, with constraints **OFF** codex `fix`/`debug` both **fail** (the model's
malformed `<tool_call>` never executes); **ON**, both **pass**
(`mlx_native_backend.rs::constrain_enabled` doc).

But `should_constrain` gates on `is_dense(model) || is_hybrid_arch(model)`:
- `is_dense` = Qwen3 / Qwen3Moe / Llama / Qwen2
- `is_hybrid_arch` = Qwen35 / Qwen35Moe

**`LoadedModel::GptOss` is in NEITHER set** → gpt-oss decodes **unconstrained**. So every other
matrix model gets the format guarantee that makes tool calls land, and **gpt-oss alone does
not** — it is free to drift into the malformed delivery we measured (Finding 6: invents JSON,
drops `*** Begin Patch`; my N-run: `codex×gpt-oss×{build,test}` = 0/3). The capability is there
(the content is valid Rust); the **delivery format** is what breaks, and we never gave gpt-oss
the lever that fixes it. This is an OUR-STACK gap, not (only) a model limit.

## Why it isn't a one-line `is_dense += GptOss`

gpt-oss speaks **harmony**, a channel format:
```
<|channel|>analysis<|message|>{chain-of-thought}<|end|>            ← must flow FREE (reasoning)
<|channel|>commentary to=functions.X <|constrain|>json<|message|>{args}<|call|>   ← the tool call
<|channel|>final<|message|>{answer}<|return|>
```
A constraint that forces a tool call from the first token would kill the analysis reasoning
gpt-oss needs. The constraint must be **harmony-aware**: let the analysis + channel-header
tokens flow free, **anchor** at the `<|message|>` of a `commentary to=functions.X` header, then
force the `{args}` JSON to tool X's schema until `<|call|>` (200012). This mirrors the existing
anchors — Qwen's `<tool_call>` and GLM's `name\n{json}` line-anchor (`find_glm_tool_call`).

## Design

1. **Eligibility:** add `GptOss` to the constrained-eligible set. `dense_forward` already has a
   `GptOss` arm and gpt-oss uses the dense `ConcatKeyValueCache`, so `run_constrained_dense` /
   `dense_step` already drive its forward — no new forward/prefill code.
2. **Harmony-aware `ToolConstraint`:** a harmony trigger mode (like `uses_glm_envelope`): free
   prefix until a `commentary to=functions.<name>` header + `<|message|>`, then JSON-schema-force
   the args until `<|call|>`. The tool name selects the schema. A pure-reasoning/final answer
   (no tool channel) stays free, so non-tool turns survive.
3. **Egress (already exists):** the now-well-formed `{path,content}`/`{patch}` flows through the
   gateway's codex path (`codex_lean_keep` + `apply_patch_block_to_fuzz`/`synth_create_command`)
   → a file write codex applies. The model never has to hand-serialize V4A under load.

## Validation (before declaring victory — no premature conclusions)

- **Model-only probe:** send gpt-oss a codex-style create/edit tool request, constraint OFF
  (today) vs ON (this change); count malformed vs schema-valid tool calls. Expect OFF≈drifts,
  ON=valid.
- **Matrix REPS:** `codex × gpt-oss × {build,test,debug}` at `REPS=3` before/after. Expect the
  0/3 build/test to lift. If it does NOT lift, the residual is genuinely model-content/timeout
  (gpt-oss reasons long → separate lever), and we say so honestly.

## Scope / coordination

MLX-native only (`crates/rozum-mlx`): the constraint + GptOss eligibility. The gateway egress
(codex paths) already exists and is the sibling's area — no overlap expected (I touch the MLX
constraint driver, not gateway.rs codex code). Behaviour-preserving for every other model
(GptOss is purely additive to the eligible set). `ROZUM_MLX_CONSTRAIN=0` still disables globally.
