# Native MLX runtime: port mlx_lm to Rust over mlx-rs

## Goal

Build rozum's **own** in-process MLX runtime on top of [`mlx-rs`](https://crates.io/crates/mlx-rs) — Apple's MLX core ops exposed as a Rust crate — so we are not blocked on `mistralrs` or `llama-cpp-2` release cycles when a new Qwen / Llama / Mistral variant ships. The Python reference (`ml-explore/mlx-lm`) becomes our spec.

This is the **most expensive** and **most strategic** of the three upstream tracks. It pays back when:
- new models drop weekly and we want them within hours, not weeks
- we want rozum to be a single-binary deployment with no Python and no C++
- we are willing to maintain ~5-10k LoC of model code

Until the port reaches feature parity for the models we care about, mistralrs (in-process) and mlx_lm.server (subprocess) remain the production paths — this is an *additional* backend, not a replacement.

## Scope

- New module `src/mlx_native/` under feature flag `mlx-native` (off by default — heavy compile, big code surface).
- Dependencies added under that feature: `mlx-rs`, `safetensors`, `minijinja`, `hf-hub`. (`tokenizers` is already a dep of `local-models`; we'll lift it to its own feature flag if needed.)
- Backend implements `ChatBackend` from `chat-backend-spi.md` — no SPI changes, slots into the existing chain in `build_gateway_backend` between mistralrs and mlx_lm.server.

## Reference

- `ml-explore/mlx-lm` Python package. Architecture file per model in `mlx_lm/models/`.
- `oxideai/mlx-rs` Rust bindings to MLX core ops.
- `huggingface/text-generation-inference` for sampling reference.

## Phased delivery

This task is **deliberately split** into phases so each one has a useful deliverable and an exit criterion. Don't start phase N+1 until phase N produces correct output for at least one real model.

### Phase 0 — bootstrap (~3 days)

- [ ] Add `mlx-native` Cargo feature with `mlx-rs`, `safetensors`, `minijinja`, `hf-hub` as deps.
- [ ] `src/mlx_native/loader.rs`: read safetensors files (sharded), build an `HashMap<String, mlx_rs::Array>` of model weights. Test on a 1B-class model that fits in RAM.
- [ ] `src/mlx_native/tokenizer.rs`: thin wrapper around `tokenizers::Tokenizer` to encode/decode with proper added-tokens and offsets.
- [ ] `src/mlx_native/chat_template.rs`: apply Qwen-style ChatML / Llama-3 jinja templates from `tokenizer_config.json` via `minijinja`.
- [ ] `src/mlx_native/sampling.rs`: greedy + temperature + top_p + top_k + repetition_penalty against `mlx_rs::Array` logits.
- [ ] Exit criterion: load a 1B model's weights and tokenizer, render a chat template, no model forward yet.

### Phase 1 — first working model: Qwen3-4B dense (~1 week)

The smallest dense Qwen3 is the proof-of-concept. No MoE, no linear-attention, no quantisation — just transformer + GQA + RoPE.

- [ ] `src/mlx_native/models/qwen3.rs`: port `mlx_lm/models/qwen3.py` line-by-line.
  - Embedding, RMSNorm, GQA, RoPE, SwiGLU MLP, final norm + LM head.
  - One-pass forward (no KV-cache yet).
- [ ] `src/mlx_native/cache.rs`: KV-cache that grows by 1 row per decode step.
- [ ] `src/mlx_native/generate.rs`: prefill + token-by-token decode loop, yielding `ChatEvent::TextDelta` over `tokio::sync::mpsc`.
- [ ] **Numerical correctness gate**: load Qwen3-4B safetensors from `mlx-community/Qwen3-4B-4bit`. Generate 50 tokens with `temperature=0` for a fixed prompt. Compare against `mlx_lm.generate --temp 0` on the same prompt. **Tokens must match byte-for-byte.** If they don't, debug layer-by-layer using `mlx.save/load` to dump intermediate activations.
- [ ] Wire as a `ChatBackend` impl: `MlxNativeBackend` in `src/mlx_native/backend.rs`, register in `build_gateway_backend` chain.
- [ ] Exit criterion: `rozum launch --model mlx-community:Qwen3-4B-4bit claude` works end-to-end with correct outputs.

### Phase 2 — Qwen3 MoE (~1 week)

- [ ] Port `mlx_lm/models/qwen3_moe.py` — adds top-k expert routing and parallel expert MLPs.
- [ ] Implement scatter-gather for expert dispatch on top of `mlx-rs` ops.
- [ ] Numerical correctness test against `mlx-community/Qwen3-30B-A3B-Instruct-4bit`.
- [ ] Exit criterion: 30B MoE runs on M-series at ≥ 30 tok/s with correct outputs.

### Phase 3 — Qwen3.5 / 3.6 hybrid linear-attention (~2 weeks)

- [ ] Port `mlx_lm/models/qwen35moe.py` — adds the linear-attention layer and per-block schedule dispatch.
- [ ] Add state-space cache alongside KV-cache; both must reset together on `req.cancel`.
- [ ] Numerical correctness test against `mlx-community/Qwen3.6-35B-A3B-4bit`.
- [ ] Exit criterion: Qwen3.6 runs in-process at ≥ 25 tok/s with correct outputs. This is the **first concrete user-facing win** of the entire mlx-native track.

### Phase 4+ — additional models (~3-5 days per family)

Llama-3, Mistral, Gemma, etc. Each port follows the same recipe: copy Python file, translate to Rust, prove numerical match, register.

## Architectural decisions

- **`mlx-rs` over hand-rolled Metal kernels** — chosen because mlx-rs covers all the ops we need (matmul, softmax, layer_norm, rope, scatter/gather) and is maintained against MLX core releases. Writing our own Metal kernels is a year of work, not a sprint task.
- **Python reference, not paper reference** — chosen because Python `mlx_lm` is what actually runs and what users compare against. Paper math is necessary but not sufficient for byte-for-byte token match.
- **Off-by-default feature flag** — chosen because the code size + compile time is heavy and most users will be happy with mistralrs or mlx_lm.server. Power users opt in.
- **One model per file, copy-paste structure** — chosen to mirror `mlx_lm/models/*.py` exactly. Future maintainers should be able to diff `qwen3.rs` against `qwen3.py` in seconds.

## Risks / sharp edges

- **Numerical drift is the killer**. Plan for 50% of each phase's time being spent on debugging activation mismatches.
- **mlx-rs API churn**: it's a 0.x crate, breaking changes possible. Pin a known-good version, bump explicitly.
- **Quantisation formats**: Q4 / Q8 / DWQ / MXFP4 all have different dequantisation paths. Phase 1 picks one (Q4) and supports only that; later phases extend.
- **Maintenance burden**: every new model family adds a Rust file we own. If we add fast, model code becomes the bulk of rozum. Decide explicitly which families we support and which we don't.
- **Numerical correctness gate is non-negotiable**. Skipping it produces "grammatically plausible but semantically wrong" outputs that look fine in smoke tests and silently degrade real agent usage.

## Estimated total cost

| Phase | Active days | Calendar weeks |
|-------|-------------|----------------|
| 0 — bootstrap | 3 | 1 |
| 1 — Qwen3-4B dense | 5-7 | 1-2 |
| 2 — Qwen3 MoE | 5-7 | 1-2 |
| 3 — Qwen3.6 hybrid | 8-15 | 2-3 |
| **Total to feature parity with mistralrs for our target models** | **~3-4 weeks active, 5-8 calendar weeks** | |

This is the single most expensive sprint item in the project so far. Budget realistically and don't start until the cheaper PRs (llama-cpp + mistralrs) are at least submitted upstream — they may unblock the immediate Qwen3.6 need long before phase 3 lands here.

## Results

(Filled in incrementally as phases complete.)
