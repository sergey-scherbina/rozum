# Model Assets

Model files are optional runtime assets and are not committed to this repository.
They are not needed for default meeting rooms.

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
