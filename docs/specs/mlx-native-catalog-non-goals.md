# Native MLX catalog — non-goals (what we deliberately will NOT port, and why)

The native MLX runtime exists to serve **local coding/agent models on Apple
Silicon** for Claude Code / Codex / aider through the rozum gateway. That purpose
is the lens for every "should we support model X" decision. The architectures and
features below are **explicitly out of scope** — not "not yet", but "the cost is
real and the benefit to our use case is ~zero". Recorded here so nobody
re-litigates them or sinks a week into one by reflex.

Things that ARE worth doing live in `BACKLOG.md` (catalog expansion) and
`SPRINT.md` (quick wins). This file is only the negatives.

## 1. DeepSeek-V2 / V3 and other MLA-attention models — NO

- **What it'd take.** DeepSeek uses **Multi-head Latent Attention (MLA)** — a
  fundamentally different attention with a compressed latent KV cache and
  decoupled RoPE. It is not a quirk on top of the Llama/Qwen block; it's a new
  attention implementation, a new KV-cache type, and (for speed) new fused
  kernels. That's a multi-week port with its own validation surface.
- **Why it's pointless for us.** The models that use it are 200B–680B parameters.
  Even at 4-bit they need hundreds of GB; they do not fit on the Apple-Silicon
  machines rozum targets (a dev laptop / Studio). DeepSeek's small "distill"
  variants are **Qwen/Llama architectures** (not MLA) — those already work via our
  Qwen/Llama paths. So MLA buys us only the models we can't run anyway.
- **Verdict.** Skip MLA entirely. If someone needs a giant DeepSeek, that's a
  remote endpoint (`--backend-url`), not in-process MLX.

## 2. Vision / multimodal (Qwen2.5-VL, Llama-Vision, Pixtral, …) — NO

- **What it'd take.** A vision tower + multi-modal projector + image
  preprocessing pipeline + the multimodal chat template + threading image inputs
  through the gateway's text-only request/response types. Large, and it touches
  the whole stack, not just one model file.
- **Why it's pointless for us.** rozum drives **text** coding agents. The gateway
  speaks Anthropic/OpenAI **chat** (text + tool-calls); Claude Code / Codex / aider
  send text. There is no path for an image to even reach the backend, and no agent
  use case that wants one. We already strip multimodal checkpoints to their
  `text_config` and run the language model — that is exactly the right amount of
  "vision support" for us (zero).
- **Verdict.** Text-only. Multimodal is a different product.

## 3. More download sources beyond HuggingFace + ModelScope — NO

- HF + ModelScope already host essentially every MLX-safetensors checkpoint in
  existence (mlx-community is HF-native; ModelScope mirrors it + the CN Qwen
  builds). Adding Kaggle / direct-git / S3 / etc. is per-source API + auth + cache
  layout maintenance for ~no coverage gain.
- A model that lives *only* somewhere else can always be pointed at as a **local
  directory** (download it however, pass the path) or run via `--backend-url`.
- **Verdict.** Two hubs is the right number. A local-dir spec is the universal
  escape hatch.

## 4. Training / fine-tuning / LoRA application at load — NO (for the host)

- rozum is an **inference host**. mlx-lm can train, but that's a separate tool and
  workflow with no place in a gateway whose job is to serve a resident model to an
  agent. (Applying a pre-merged LoRA is just loading the merged checkpoint — which
  already works; *runtime* LoRA stacking is the out-of-scope part.)
- **Nuance:** *improving* a model for a domain via **offline** QLoRA → merge →
  serve is genuinely useful and already works (serve the merged dir). It's only
  *training inside the host* (online/continual) that's out of scope. The full
  landscape — what's feasible, the memory math, where it's useful vs a trap — is in
  `docs/specs/training-and-lora-exploration.md`.
- **Verdict.** Host stays inference-only. Tune offline, then serve the result.

## 5. GGUF / llama.cpp models through the *native MLX* runtime — NO (already covered)

- **We DO have our own GGUF backend.** To be unambiguous: rozum ships a real,
  opt-in in-process GGUF backend — `crates/rozum-gguf/src/gguf.rs`, enabled with
  `--features gguf`, running llama.cpp in-process via the `llama-cpp-2` Rust bindings
  (not a subprocess), with streaming
  + tool-use, resolving local `.gguf` files / `lmstudio:` / `ollama:` specs. So
  "GGUF support" is **not** the non-goal.
- The non-goal is narrower: (a) teaching the **native MLX runtime** to read the
  GGUF format (GGUF is llama.cpp's format, not MLX safetensors — that would
  duplicate the GGUF backend in the wrong place), and (b) writing a **from-scratch
  GGUF reader / inference engine** (that's reinventing llama.cpp, a mature, fast
  Metal engine, for zero benefit — same logic as not reinventing MLX).
- **Verdict.** GGUF stays in the GGUF backend (llama.cpp); native MLX stays
  MLX-safetensors (mlx-rs). Each format keeps its own proven engine.

## 6. Non-quantized giant models "just because" — NO

- The non-quantized (bf16/fp16) load path exists and works, but it is for *small*
  models; loading a 70B fp16 (140 GB) is a RAM wall, and the KV preflight will
  reject it anyway. We don't add anything to "support" this — physics already
  decides it.
- **Verdict.** No work item. Use the 4-bit AFQ build, or a remote endpoint.

---

**Summary.** Native MLX's catalog should grow along the **dense decoder-only,
Apple-Silicon-sized, text** axis (Mistral, Gemma, Phi, Mixtral — see BACKLOG).
Everything above trades a large, ongoing engineering cost for models we either
can't run, can't feed, or already serve elsewhere.
