# Model Assets

Model files are optional runtime assets and are not committed to this repository.
They are not needed for default meeting rooms.

## Managing what is installed

```bash
rozum models list              # what is installed, with sizes and where it came from
rozum models list --remote     # the curated download list
rozum models info <spec>       # details, installed or not
rozum models rm <spec>         # free the disk; refused if it is the active gateway model
```

A spec is either a curated name or any HuggingFace/MLX id, e.g.
`mlx-community:Qwen3.5-4B-MLX-4bit`. `rozum models rm` deletes HuggingFace and
LMStudio directories directly and delegates Ollama to `ollama rm`, whose blobs
are content-addressed and shared.

## Two engines, two kinds of file

| Engine | Format | Where it comes from | Platform |
|---|---|---|---|
| MLX (default on Apple Silicon) | MLX-quantised weights | HuggingFace / `mlx-community` | macOS, Apple Silicon |
| GGUF (`--features gguf`) | `.gguf` | HuggingFace, LM Studio, local files | portable |

Which engines a build carries is a compile-time choice — see
[INSTALL.md](../INSTALL.md) § Model-engine features.

## RAM decides what you can actually run

A model is admitted only if it fits: loading past host RAM can panic or reboot
the machine, so the gateway checks the footprint plus a keep-free margin against
what is actually available, not against what is theoretically installed. When a
model does not fit at its full context, **adaptive load lowers `n_ctx` to the
best fit** rather than failing — the startup line says so when it happens.

```bash
rozum gateway --model <spec> --dry-run      # what it would load, and whether it fits
rozum gateway --model <spec> --n-ctx 32768  # ask for less context up front
```

Levers when it is tight: `--min-free-ram-gb`, `--ram-budget-frac`,
`--mlx-cache-gb`, `--no-adaptive-load` (refuse instead of shrinking), and
`--allow-concurrent-resident` if you really do want two resident models.

Long-running quantised models can also be swapped without restarting anything —
see `rozum gateway switch` in [REFERENCE.md](REFERENCE.md) § 1.

## Which model actually works — measured, not assumed

The current numbers live in
[`scripts/bench/RESULTS.md`](../scripts/bench/RESULTS.md), regenerated from the run
CSVs rather than maintained by hand. Read that page for today's figures; the shape below
is what they have consistently shown.

From the widest run on disk (2026-07-05, 405 cells, 9 models × 3 agents, each cell a
deterministic `cargo run`/`cargo test` verdict):

| model | claude | codex | opencode |
|---|---|---|---|
| `GLM-4-32B-0414-4bit` | 15/15 | 5/15 | 3/15 |
| `Devstral-Small-2507-4bit` | 13/15 | 3/15 | 7/15 |
| `gpt-oss-20b-MXFP4-Q4` | 12/15 | 7/15 | 11/15 |
| `GLM-4-9B-0414-4bit` | 12/15 | 4/15 | 7/15 |
| `Qwen3-4B-4bit` | 11/15 | 4/15 | 11/15 |
| `Qwen2.5-Coder-7B-Instruct-4bit` | 3/15 | 3/15 | 3/15 |
| `Qwen3-0.6B-4bit` | 3/15 | 3/15 | 3/15 |

Three things that table says, and they matter more than any single row:

- **The agent around the model matters as much as the model.** `GLM-4-32B` is 15/15 with
  one agent and 3/15 with another — same weights, same tasks, same host. Choosing a model
  without saying which agent will drive it is choosing half the system.
- **There is a capability cliff, and it is not subtle.** The small models sit at 3/15 —
  which is `greet` and nothing else, the one task with no tools. Below roughly 7B the
  question is not how well a model codes but whether it can hold a tool loop at all.
- **Fast is not good.** `Qwen3-0.6B` answers in 4 seconds and passes nothing beyond the
  first rung; `GLM-4-32B` takes 219 s per cell and passes everything. Seconds are only
  interesting between models that clear the same tasks.

Newer single-model runs (2026-08-15) show `Qwen3.5-4B` and `Qwen3.5-9B` at 24/24 on the
eight-task ladder under `claude`, at 40 s and 74 s median — but those were measured on a
different binary, so read them as their own run rather than as a comparison with the table
above. That rule is why the results page reports each run as itself and refuses a
cross-run leaderboard.

## Tiny GGUF Options

| Model | Quant | File size | Use |
|---|---:|---:|---|
| `SmolLM2-135M-Instruct` | `Q2_K` | 88,202,080 bytes / 88.2 MB | Smallest tested text GGUF candidate, quality tradeoff is high. |
| `SmolLM2-135M-Instruct` | `Q4_K_M` | 105,454,432 bytes / 105.5 MB | Recommended tiny bootstrap model. |
| `gemma-3-270m-it` | `Q2_K` | 237,079,040 bytes / 237.1 MB | Larger tiny Gemma instruction candidate. |
| `Qwen3-0.6B` | `Q4_K_M` | 484,219,552 bytes / 484.2 MB | Better tiny reasoning candidate, still under 0.5 GB. |

## Download Recommended Tiny Model

```bash
./scripts/download-tiny-model.sh
```

The default download target is `SmolLM2-135M-Instruct-Q4_K_M.gguf` into `models/`.
