# Model Driver Hints

## Overview

UCC model selection must expose that model capability is relational: a model can be reliable under one
agent driver and poor under another. The model picker should therefore show both a model adequacy rating
and a terse driver hint, while the rating exporter should avoid letting narrow diagnostic runs overwrite
matrix-level conclusions.

## Interface

- `/control/status` `installed[]` rows include `driver_hint: string`.
- `clients/control/center.ssc` displays `driver_hint` as a `Driver` column in model pickers.
- `scripts/bench/export_model_ratings.py` scans matrix-like result directories by default.
- `scripts/bench/export_model_ratings.py --include-slices` restores the old all-result-dir scan.

## Behavior

- [x] Known broadly compatible models, such as Qwen3.6-35B, show `any`.
- [x] Known model-driver pair preferences are surfaced tersely, such as `claude` or `claude/opencode`.
- [x] Unknown models show an empty driver hint rather than an invented recommendation.
- [x] Rating export excludes diagnostic/slice result directories by default.
- [x] Rating export records how many result directories were included or skipped.
- [x] The old slice-inclusive export remains available through `--include-slices`.

## Out of Scope

- Automatic driver switching.
- Blocking launch of a known-poor model-driver pair.
- Re-running GPU matrix cells when the model slot is busy.
