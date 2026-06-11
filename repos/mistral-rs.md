# mistral-rs (vendored fork)

Local checkout: `.vendor/mistral-rs` (git-ignored). Remotes:
- `origin` → EricLBuehler/mistral.rs (upstream)
- `fork`   → sergey-scherbina/mistral.rs (our PR/work fork)

`rozum/Cargo.toml` pins it via `[patch.crates-io]` to a **git rev on the fork**,
so any change here must be pushed and the rev bumped in `rozum` to take effect
in the `rozum` binary.

## Worktrees in use

| Path | Branch | Purpose |
|---|---|---|
| `.vendor/mistral-rs` | `qwen36-chunked-prefill` | integration line rozum currently pins |
| `.vendor/mistral-rs-fixes` | `qwen36-fixes` | Qwen3.6 forward fixes (upstream PR #2201) |
| `.vendor/mistral-rs-chunked` | `qwen36-chunked-prefill-v2` | chunked prefill (upstream PR #2208) |

## Build / test gotchas

- Building the Metal path needs **full Xcode** (not just Command Line Tools);
  `mistralrs-paged-attn` `build.rs` shells out to `xcrun metal`. Symptom:
  `xcrun: error: unable to find utility "metal"`. Fix:
  `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`.
- AFQ quantized matmul lives in `mistralrs-quant/src/afq/` (`mod.rs` =
  `AfqLayer`/`QuantMethod`, `ops.rs` = `afq_mm_op` + MoE gather paths,
  `metal_kernels/` = the candle-Metal kernels the MLX-direct work replaces).
- Upstream PR map for our patches: see `rozum/patches/README.md` and
  `rozum/docs/specs/mistralrs-qwen36-pr.md`.
