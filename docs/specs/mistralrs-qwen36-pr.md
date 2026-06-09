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

mistralrs auto-loader maps the repo to its internal `Qwen3MoE` architecture, which assumes every block is full-attention. Qwen3.6 uses **hybrid attention**: ~75% linear-attention (`GatedDeltaNet`, Mamba-style state-space) interleaved with ~25% full-attention, indicated by `layer_types` in the config. The full-attention `in_proj_qkv` is `[hidden=8192, 3*head_dim*n_heads=2048]`; the linear-attention `in_proj_qkv` is `[8192, head_dim=256]`. Loader picks the wrong block type, weight shape mismatches, load fails.

### Key insight from upstream survey

**The Qwen3.5/3.6 architecture is already implemented in mistralrs — under a different name.** mistralrs ships `mistralrs-core/src/models/qwen3_next.rs` (1044 LoC) with:

- `GatedDeltaNet` linear-attention layer (with `conv1d` + state-space update via `gated_delta_update`)
- `FullAttention` block
- `SparseMoeBlock` MoE expert routing
- `Config::layer_types()` deriving the hybrid schedule from `full_attention_interval` (currently hard-coded to "every 4th block is full-attention" — which matches Qwen3.6's `full_attention_interval: 4` exactly).

The Python reference confirms architectural identity: `mlx_lm/models/qwen3_5.py` literally imports `Qwen3NextAttention`, `Qwen3NextMLP`, `Qwen3NextRMSNormGated`, `Qwen3NextSparseMoeBlock` from `qwen3_next.py` and uses them unchanged.

What is missing in mistralrs is therefore **not** the layer code — it is:

1. Three lines in `mistralrs-core/src/pipeline/loaders/normal_loaders.rs` registering the new identifiers (`model_type: "qwen3_5_moe"`, `architectures: ["Qwen3_5MoeForConditionalGeneration"]`) to dispatch to the existing `Qwen3NextLoader`.
2. Config parser tolerance for the **nested `text_config` block** (Qwen3.6 nests text-only fields under `text_config`; Qwen3-Next has them flat) and for the **explicit `layer_types: [...]` array** (Qwen3-Next derives the same schedule from `full_attention_interval`).
3. (Optional) Honour `attn_output_gate: true` if it changes the FullAttention output projection. Inspect `qwen3_5.py` to confirm whether this is a real semantic change vs an always-true default in Qwen3-Next.

## Tasks

- [x] Confirm config.json shape for Qwen3.6 (`model_type: qwen3_5_moe`, `architectures: [Qwen3_5MoeForConditionalGeneration]`, `text_config.layer_types: [linear_attention | full_attention]`, `full_attention_interval: 4`, `attn_output_gate: true`).
- [x] Confirm mistralrs already has all the layer code in `qwen3_next.rs` (GatedDeltaNet, FullAttention, SparseMoeBlock, MoE routing, `Config::layer_types()`).
- [x] Confirm mlx-lm's `qwen3_5.py` re-uses `qwen3_next.py` classes verbatim.
- [ ] Open an upstream issue in `EricLBuehler/mistral.rs` titled `Add Qwen3.5/3.6 support (alias of qwen3_next + nested text_config)` with this spec attached.
- [ ] In a fork, edit `mistralrs-core/src/pipeline/loaders/normal_loaders.rs`:
  - Add `#[serde(rename = "qwen3_5_moe")] Qwen3_5Moe` variant (and optionally `qwen3_5`).
  - Register architecture strings `"Qwen3_5MoeForConditionalGeneration"` and `"Qwen3_5ForCausalLM"`.
  - Dispatch both to the existing `Qwen3NextLoader` (no new loader needed).
  - Update the architectures error message and the Display impl.
- [ ] In `mistralrs-core/src/models/qwen3_next.rs`:
  - Make the config deserialiser accept **either** flat fields (existing Qwen3-Next layout) **or** a nested `text_config: {…}` object (Qwen3.6 layout). Implement via a custom `Deserialize` or a thin `#[serde(flatten)]` + fallback wrapper.
  - Add `layer_types: Option<Vec<LayerType>>` to `Config` and prefer it over `full_attention_interval` when present. The two must agree on the existing test models.
  - (If `attn_output_gate` proves to be a behavioural difference) thread an `attn_output_gate: bool` flag through `FullAttention` and apply the extra gate before the output projection. If it is always-true in qwen3_next today, document it and skip.
- [ ] Unit-test the loader: feed it a tiny stub `config.json` with the Qwen3.6 layout and assert the model loads without panic.
- [ ] Numerical correctness gate: load real `mlx-community/Qwen3.6-35B-A3B-4bit` via the patched mistralrs, generate 50 tokens with `temperature=0` for a fixed prompt, compare against `mlx_lm.generate --temp 0` on the same prompt. Tokens must match byte-for-byte for the first 20+ tokens (mismatches after that are usually sampler precision, acceptable).
- [ ] Open PR upstream referencing the issue.
- [ ] After merge: bump mistralrs version in rozum's Cargo.toml, drop the `cargo update`-able `[patch.crates-io]` workaround if we used one in the meantime.
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

Revised down sharply after discovering the qwen3_next re-use:

- Loader registration (auto-detect + arch strings): **~0.5 day**
- Config deserialiser tolerance (flat ↔ nested `text_config`, optional `layer_types`): **~1 day**
- `attn_output_gate` investigation + conditional wiring: **~1 day**
- Numerical correctness verification vs Python `mlx_lm`: **~2-3 days**
- PR + review cycle: **1-2 weeks calendar time**
- **Total active effort: ~1 week** (down from the original 2-3 week estimate)

The original estimate assumed adding a new linear-attention layer module from scratch. After confirming mistralrs already has all of that in `qwen3_next.rs`, the work collapses to plumbing.

## Results

(Filled in after PR lands and rozum picks up the new version.)
