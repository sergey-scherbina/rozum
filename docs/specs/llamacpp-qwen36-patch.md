# Upstream patch: llama.cpp Qwen3.6 hyperparam support

## Goal

Submit a fix to **upstream llama.cpp** so that the `qwen35moe` architecture in GGUF files exported by Alibaba's Qwen3.6 release loads successfully. Once merged and pulled into `llama-cpp-2`, rozum's `--features gguf` path runs Qwen3.6 (and the entire Qwen3.5/3.6 line) without any Python or Ollama in the runtime — just `ollama pull qwen3.6:35b-a3b && rozum launch --model qwen3.6:35b-a3b claude`.

This is the **cheapest** of the three upstream-fix tracks. The bug is a single hyperparameter array length mismatch, not a new model architecture.

## Scope

- Upstream: `ggerganov/llama.cpp` (or current llama.cpp maintainer's repo).
- Downstream: `utilityai/llama-cpp-rs` only needs a version bump once upstream merges.
- rozum: only needs `cargo update -p llama-cpp-sys-2` once a new `llama-cpp-2` is published; no code change.

## Concrete error

```
llama_model_load: error loading model: error loading model hyperparameters:
key qwen35moe.rope.dimension_sections has wrong array length;
expected 4, got 3
```

Reproducer:
```bash
ollama pull qwen3.6:35b-a3b
rozum launch --model qwen3.6:35b-a3b claude
```
The model file is `~/.ollama/models/blobs/sha256-f5ee307a...`, GGUF V3, exported by Alibaba/unsloth (`unsloth/Qwen3.6-35B-A3B-GGUF`).

## Root cause hypothesis

llama.cpp's `qwen3moe` (and now `qwen35moe`) hyperparam loader hard-codes the expected length of `rope.dimension_sections` to 4, matching what older Qwen3 MoE checkpoints used. Qwen3.6 ships with length 3 (one fewer rope section), most likely because the linear-attention layers don't use the same RoPE block layout as full-attention layers.

The fix is **not** to allow arbitrary lengths — the loader needs to accept length 3 OR 4 and dispatch accordingly when applying RoPE in the attention kernel.

## Tasks

- [ ] Reproduce upstream: clone latest `ggerganov/llama.cpp` master, build, run with the same GGUF file, confirm the same error message (or, if already fixed in master, just pull the fix into llama-cpp-2 via a version bump).
- [ ] Open an issue in upstream llama.cpp titled `qwen35moe: rope.dimension_sections length 3 from Qwen3.6 not accepted`, link the model file (`unsloth/Qwen3.6-35B-A3B-GGUF`), include the full error and `metadata` dump.
- [ ] Locate the hyperparam validation: grep `dimension_sections` in `llama.cpp/src/llama-model-loader.cpp` (or the file that owns Qwen3 MoE arch). Confirm hardcoded `== 4`.
- [ ] Patch: accept both lengths 3 and 4; thread the actual length through to the RoPE kernel call site so attention layers without a 4th rope section don't read garbage.
- [ ] Test: load the GGUF with the patched build, run a sample chat completion, compare token outputs against the reference mlx_lm Python on the same prompt + sampling=greedy (token-by-token match).
- [ ] Submit PR upstream. Reference the issue.
- [ ] After upstream merge: bump `llama-cpp-rs` submodule pointer (PR in `utilityai/llama-cpp-rs` if no automation), wait for new published version on crates.io.
- [ ] In rozum: `cargo update`, verify `rozum launch --model qwen3.6:35b-a3b claude` works end-to-end, mark gguf path "supports Qwen3.6" in `docs/specs/gguf-backend.md`.

## Out of scope

- Adding new architectures (e.g. truly novel hybrid attention layers). This PR is strictly the dimension_sections length fix.
- Optimising Qwen3.6 Metal kernels. Upstream maintainers track that separately.
- Vision/multimodal variants of Qwen3.6.

## Decisions

- **Upstream-first, not vendored** — chosen because a fork would force us to maintain a parallel build and miss future fixes. The patch is small (≤50 LoC) and architecturally trivial; upstream review should be fast.
- **Both length 3 and length 4 accepted** — chosen so older Qwen3 MoE files don't regress. Rejected: silently coercing to length 4 (would corrupt RoPE math on Qwen3.6).

## Risks / sharp edges

- Upstream review cycle is unpredictable; budget 1-4 weeks.
- If upstream rejects the patch (e.g. wants a wider refactor first), our fallback is to vendor the patch via a `[patch.crates-io]` entry on a fork of `llama-cpp-rs` until upstream lands a proper fix.
- llama-cpp-rs version bumps lag upstream llama.cpp by ~1-2 weeks; once both land we are unblocked.

## Results

(Filled in after PR lands and rozum picks up the new version.)
