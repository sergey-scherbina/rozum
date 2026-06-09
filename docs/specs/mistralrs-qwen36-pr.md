# Upstream PR: mistralrs Qwen3.6 support

## Goal

Add Qwen3.6 (35B-A3B MoE + 27B dense + future variants) to `EricLBuehler/mistral.rs` so that rozum's in-process MLX backend (already wired up via `--features mistralrs`, on by default) can load native MLX safetensors directly — no Python, no Ollama, no llama.cpp.

This is the **highest-leverage** of the three upstream tracks. mistralrs is the only mainstream Rust LLM runtime with full MLX support; getting Qwen3.6 in opens the door for the entire Rust ecosystem (rozum, llmcord, rig, anything embedding mistralrs).

## Scope

- Upstream: `EricLBuehler/mistral.rs` (active, single maintainer, fast review cycles).
- Downstream: rozum picks up the new version with a single Cargo.toml bump.

## Concrete error

```
warning: mistralrs load failed: backend unavailable: mistralrs:
  failed to load mlx-community/Qwen3.6-35B-A3B-4bit:
  shape mismatch for language_model.model.layers.0.linear_attn.in_proj_qkv.weight,
  expected: [8192, 2048], got: [8192, 256]
```

Reproducer:
```bash
cargo build  # mistralrs is the default feature
rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude
```

## Root cause

mistralrs auto-loader maps the repo to its internal `Qwen3MoE` architecture, which:

- Assumes every block is full-attention (RoPE + GQA).
- Allocates `in_proj_qkv` weight with the full-attention shape `[hidden, 3 * head_dim * num_heads]` = `[8192, 2048]`.

Qwen3.6 introduces **hybrid attention**: roughly half the blocks are full-attention as before, the other half use a **linear-attention layer (Mamba-style state space)** with a smaller `in_proj_qkv` of `[hidden, head_dim * 1]` = `[8192, 256]`. mistralrs doesn't have a linear-attention module wired into its Qwen3 pipeline yet, so weight loading fails on the first hybrid block.

This is **not** a small fix — it's adding a new layer type. But the scaffolding (RoPE, GQA, MoE expert routing) is already in mistralrs's Qwen3 implementation; we'd be adding **one** new layer module and a per-block dispatch.

## Tasks

- [ ] Read `unsloth/Qwen3.6-35B-A3B-MLX-8bit` `config.json` and source weights to enumerate exactly which blocks are linear-attn vs full-attn (Qwen3.6 publishes a fixed schedule, e.g. every 3rd block).
- [ ] Read the reference Python forward pass: `mlx-lm/mlx_lm/models/qwen35moe.py` (or whichever file is the canonical Qwen3.5/3.6 model in mlx-community/mlx-lm). Extract the linear-attention module signature: input/output shapes, internal state (recurrent step), how it interacts with the KV-cache.
- [ ] Open an upstream issue in `EricLBuehler/mistral.rs` titled `Add Qwen3.6 (hybrid linear-attention) support`. Link this spec, link mlx_lm reference, attach the shape-mismatch error.
- [ ] In a fork, add `mistralrs-core/src/layers/linear_attention.rs` (or wherever the maintainer's layer files live). Implement:
  - struct with state cache field
  - `forward(x, &mut state) -> output` matching numerical reference
  - integration with the existing `KvCache`-style trait so cancel/reset still work
- [ ] In `mistralrs-core/src/models/qwen3_6.rs` (new file or extend `qwen3.rs`):
  - Per-block dispatch: linear_attn vs full_attn based on the model's block_schedule field.
  - Register `Qwen3.6ForCausalLM` so the auto-loader matches `architectures: ["Qwen35MoeForCausalLM"]` in HF config.json.
- [ ] Unit-test against a tiny scratch model (random weights, 2 layers, 1 linear-attn + 1 full-attn) — assert no panic on load, no NaN in logits.
- [ ] Numerical correctness test: load the real `mlx-community/Qwen3.6-35B-A3B-4bit`, generate 50 tokens with `temperature=0`, compare against Python `mlx_lm.generate --temp 0` on the same prompt. Tokens must match.
- [ ] Open PR upstream. Coordinate with Eric on naming/architecture conventions.
- [ ] After merge: bump mistralrs version in rozum Cargo.toml.
- [ ] Smoke-test: `rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude` end-to-end.

## Out of scope

- Optimising the linear-attention Metal kernel for throughput. First land correctness, then a perf pass can follow as a separate PR.
- Multi-token prediction (MTP / speculative decoding) — Qwen3.6 ships MTP variants; that's a separate feature.
- Vision Qwen3.6 variants — text-only first.
- Other models that use similar linear-attention (Mamba, Jamba, RWKV) — out-of-scope; the implementation should be generic enough that they're easier to add later, but we don't ship them in this PR.

## Decisions

- **Upstream rather than vendored fork** — chosen because mistralrs has an active maintainer and a clean trait-based layer system that's designed for this kind of extension. A long-lived fork would diverge fast.
- **Reference Python directly** — chosen as the correctness oracle. The mlx_lm Python implementation is the canonical Qwen3.6 reference; everything else (vLLM, SGLang, etc.) is a clone of that math.
- **One PR, not split into "scaffolding + Qwen3.6"** — chosen because the linear-attention layer is meaningless without a model that uses it; reviewers prefer the working slice over a refactor with no consumer.

## Risks / sharp edges

- Numerical correctness debugging is the bulk of the work. Budget 1-3 weeks of focused effort with a tight feedback loop (generate small forward pass, compare to Python, diff layer by layer).
- mistralrs's Qwen3 module may need refactoring to accept per-block layer dispatch. Coordinate with Eric early to confirm the right architectural seam.
- Qwen3.6 hybrid schedule may differ between checkpoints (e.g. coding variant vs instruct variant) — implementation should read the schedule from `config.json`, not hard-code it.
- If maintainer is unavailable, fallback is the same as for the llama.cpp PR: `[patch.crates-io]` to a personal fork until upstream catches up.

## Estimated cost

- Reading + understanding mlx_lm reference: 2-3 days
- Linear-attention layer in Rust + tests: 3-5 days
- Model integration + numerical match: 5-10 days
- PR review cycle: 1-3 weeks calendar time
- **Total active effort: 2-3 weeks**

## Results

(Filled in after PR lands and rozum picks up the new version.)
