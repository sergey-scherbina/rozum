# Training & LoRA on the native MLX stack — an exploration

**Status: exploratory / educational.** Not a committed feature. rozum is an
inference host; this document exists to understand *what is possible*, *how hard*,
*where it could actually be useful for us*, and *what is a trap* — so a future
"let's fine-tune a model" conversation starts from facts instead of vibes.

It pairs with `mlx-native-catalog-non-goals.md` (which lists "training/LoRA at
runtime" as a non-goal). The non-goal verdict still stands for the *host*; this
doc explains the whole landscape behind that one line.

---

## 0. TL;DR

- **Possible? Yes**, concretely. MLX is a full autodiff framework, and our
  `mlx-rs` fork already ships `value_and_grad` + optimizers (SGD, Adam family,
  Lion, RMSprop, Adafactor). The Python reference `mlx_lm.lora` already
  LoRA-tunes on Apple Silicon. The engine is not the blocker.
- **Three very different things** hide under "LoRA/training":
  - **(A) Offline tune → merge → serve.** Works *today* with zero rozum code
    (we already serve a merged checkpoint). This is the useful one.
  - **(B) Load an adapter at serve time (no merge).** Moderate rozum work
    (LoRA-aware forward, or a load-time merge for AFQ-quantized weights).
    Marginal value for our single-resident-model gateway.
  - **(C) Train during serving / "self-improvement".** Hard — and the hard part
    is mostly **not** engineering. It's the data/signal/eval/forgetting problem,
    plus serving-vs-training concurrency.
- **Can we improve models? Yes** — via (A), on a *small* model for a *narrow*
  domain (your codebase, your tool-call format, your style), always gated by a
  held-out eval. You will **not** make a 4B model "generally smarter"; that is not
  what local small models are for.

---

## 1. Vocabulary (so the rest is precise)

- **Fine-tuning (full FT).** Update *all* weights by backprop on new data.
  Maximal capacity to change the model; maximal cost (gradients + optimizer state
  for every parameter).
- **LoRA (Low-Rank Adaptation).** Freeze the base weights `W`. For chosen linear
  layers, learn a small low-rank update `ΔW = B·A` (with `A ∈ r×k`, `B ∈ d×r`,
  rank `r` ~ 4–64). At inference, the layer computes `W·x + B·(A·x)`. Only `A,B`
  are trained — typically **<1%** of parameters. Memory for gradients/optimizer
  collapses accordingly.
- **QLoRA.** LoRA where the frozen base is kept **quantized** (4-bit NF4 / our
  AFQ), and only the small fp16 adapters are trained. This is what makes tuning a
  7B feasible on a laptop: the 14 GB fp16 base becomes ~4 GB.
- **SFT (supervised fine-tuning).** Train on `(prompt, desired_completion)` pairs
  — the bread-and-butter way to teach format/style/domain.
- **Preference tuning (DPO / RLHF).** Train on `(prompt, chosen, rejected)` to
  push the model toward preferred outputs. Needs preference data and is where
  "learn from usage" ideas live (and usually die).
- **Merge.** Fold `ΔW = B·A` back into `W` so the result is an ordinary
  checkpoint with no adapter — what you serve.

---

## 2. What our stack already supports

Checked in the vendored fork (`.vendor/mlx-lm`):

- **Autodiff:** `mlx-rs/src/transforms/…value_and_grad…` — reverse-mode grad over
  MLX graphs. The same engine that does our forward can do the backward.
- **Optimizers:** `mlx-rs/src/optimizers/{sgd,adamax,adafactor,lion,rmsprop,…}`.
- **Missing:** there is **no LoRA layer** in either crate, and no training loop /
  data pipeline. So the *primitives* exist; the *training program* (LoRA-wrapped
  Linear, loss, batching, checkpointing, eval) would have to be written.
- **Python reference:** `mlx_lm.lora` / `mlx_lm.fuse` already do LoRA SFT + merge
  on Apple Silicon, end to end. For path (A) we don't need to write any of the
  Rust training code — we use the Python tool and serve the result.

**Implication.** The cheapest credible path uses the existing Python `mlx_lm`
tooling for the *training* step and rozum only for *serving*. Writing training in
our Rust runtime is possible but is effort we'd take on only for path (B)/(C).

---

## 3. The three paths in depth

### (A) Offline tune → merge → serve — **works today, the useful one**

Pipeline, none of which lives inside the rozum host:

1. **Data.** Build `(prompt, completion)` pairs for the target skill. For a coder:
   fill-in-the-middle from your repo, `signature+docstring → body`, or
   `diff → commit message`. Quality and consistency matter far more than volume;
   1–5k clean examples beat 100k noisy ones.
2. **Train (QLoRA).** `mlx_lm.lora` on a small base (e.g. `Qwen2.5-Coder-1.5B/7B-4bit`),
   rank 8–16, target the attention + MLP projections, LR ~1e-4, 1–3 epochs.
   Minutes-to-an-hour on an M-series; fits comfortably in 16–32 GB.
3. **Eval — non-negotiable.** A *held-out* domain set (exact-match / edit-distance
   on completions) **and** a small general probe to detect regression. Without
   this you cannot tell "improved" from "quietly degraded".
4. **Merge.** `mlx_lm.fuse` → an ordinary MLX checkpoint (optionally re-quantized
   to 4-bit AFQ).
5. **Serve.** `rozum launch --model <merged-dir>` — already supported; the native
   runtime loads it like any other. Zero new rozum code.

**Value.** Genuine and bounded: a small local model that is *better at your
specific thing* (your conventions, your stack, your tool-call format) and runs
fast on-device. This is the sweet spot for local fine-tuning.

**Cost.** An afternoon of data + training + eval, repeatable. The risk is entirely
in data quality and remembering to eval.

### (B) Load an adapter at serve time (no merge) — **moderate, marginal for us**

Instead of merging, keep `W` (quantized) + small `A,B` and apply `W·x + B·(A·x)`
in the forward, or merge once at load.

- **What rozum would need.** Either (i) a LoRA-aware `Linear` in the native
  runtime that adds the low-rank term in `forward` (small, but touches the hot
  path and must stay correct under quantization), or (ii) a **load-time merge for
  AFQ weights**: dequantize the affected layers, add `B·A`, re-quantize. (ii) is
  simpler operationally and keeps the forward untouched, at the cost of a one-time
  load step.
- **Why you'd want it.** Hot-swappable adapters: one base in memory, many
  fine-tunes selected per request; or shipping the adapter separately from the
  base.
- **Why it's marginal for us.** Our gateway holds **one resident model** and
  serves an agent. We don't multiplex many adapters per request. (A)'s merged
  checkpoint gives the same served behavior with no runtime complexity. So (B) is
  "nice if we ever serve a family of tunes", not now.

### (C) Train during serving / continual "self-improvement" — **hard; mostly not an engineering problem**

The seductive idea: the model learns from the agent's sessions and gets better
over time. Reality has two layers of difficulty.

**Engineering difficulties (real but tractable):**

1. **Memory.** Backward needs activations retained across layers (scales with
   `batch × seq_len × layers × hidden`), plus gradients + optimizer state for the
   trainable params. QLoRA shrinks the param side to ~tens of MB; activations
   become the driver and want gradient-checkpointing. On a machine already near
   its RAM ceiling for *inference*, training competes hard.
2. **The `!Send` single worker.** The model is owned by one thread serving
   requests. Training either blocks serving or needs a second copy (double
   memory). Mutating weights mid-serve raises consistency questions.
3. **Determinism / trust.** A model that silently changes while it serves is a
   debugging and trust nightmare for a tool whose value is *predictable* behavior.
   You'd want versioned snapshots + the ability to roll back.

**Methodology difficulties (the actual killers):**

4. **Where is the training signal?** To improve, you need labels or a reward. Agent
   sessions give you… implicit, noisy, sparse signals (did the user keep the
   edit? re-prompt? rage-quit?). Turning that into supervised pairs or preferences
   is a whole **reward/labeling pipeline** (instrument accept/reject, or use a
   judge model) — not a runtime toggle.
5. **Catastrophic forgetting.** Updating on a thin stream of in-domain data easily
   degrades general capability. Mitigations exist (low LR, few steps, LoRA over
   full FT, replay/mixing general data, KL-to-base regularization à la DPO) but
   they require care and, again, **continuous eval** to even notice damage.
6. **Distribution & feedback loops.** Training on your own outputs risks
   self-reinforcing errors (model confidently wrong → reinforced). Online RL on a
   small local model with a weak reward is a known way to make things worse.

**Verdict on (C).** Possible in principle; the engine can do the math. But it is a
**research project with an evaluation and data-pipeline core**, not a feature you
bolt onto a gateway. The failure mode isn't "it won't run" — it's "it runs and
quietly makes the model worse, and you don't notice." That is exactly the wrong
property for an inference host.

---

## 4. The memory math (why size decides everything)

Rough per-parameter bytes during training (the reason model size, not cleverness,
sets the ceiling on Apple Silicon):

| Approach | base weights | grads | optimizer (Adam) | trainable params |
|---|---|---|---|---|
| Full FT (fp16) | 2 B/param | 2 B/param | ~8 B/param (fp32 moments) | **all** |
| LoRA (fp16 base) | 2 B/param (frozen) | only adapters | only adapters | <1% |
| **QLoRA (4-bit base)** | ~0.5 B/param (frozen) | only adapters | only adapters | <1% |

- **Full FT of 7B** ≈ 14 GB weights + 14 GB grads + ~56 GB optimizer ≈ **80+ GB**
  before activations → off the table on a laptop. Full FT is realistic only up to
  ~1–2B on big-RAM Apple Silicon.
- **QLoRA of 7B** ≈ ~4 GB frozen 4-bit base + tens of MB adapters/optimizer +
  activations (the real variable, tamed by short `seq_len`, batch 1, gradient
  checkpointing) → **fits on a 32–64 GB Mac** (this is what `mlx_lm.lora` does).
- **QLoRA of 0.5–4B** → comfortable even on 16 GB.

So the practical training menu on consumer Apple Silicon is: **QLoRA on small
models.** Everything else is a remote/cluster job.

---

## 5. Where this could actually be useful *for us*

Narrow, measurable, small-model wins — yes:

- **Domain coder.** QLoRA `Qwen2.5-Coder-1.5B/7B` on your repo's conventions →
  better in-domain completions, fast and local. Clear win.
- **Tool-call reliability.** Small models sometimes botch the `<tool_call>` JSON
  format. A tiny SFT on correct tool-call traces could measurably raise format
  adherence — narrow, low-risk, easy to eval. Possibly the highest value/effort
  ratio for the agent use case.
- **Meeting-room persona/style.** Light style adaptation for the room agents.

Not useful — don't:

- **Making a small model "generally smarter."** You won't out-train the frontier;
  local small models earn their keep on *latency, privacy, cost, and narrow
  fit* — not raw capability.
- **Online self-improvement on a laptop** (path C) without a real eval + reward
  pipeline — high chance of net-negative.

---

## 6. A concrete minimal experiment (if we ever want to try)

Entirely offline (path A), no rozum changes:

1. Base: `mlx-community/Qwen2.5-Coder-1.5B-Instruct-4bit`.
2. Data: ~1–5k `(prompt, completion)` pairs from the repo (FIM, or
   signature+docstring→body, or diff→commit-message). Hold out ~10%.
3. `mlx_lm.lora`: rank 16, target `q/k/v/o + gate/up/down`, LR 1e-4, 2 epochs,
   `seq_len` 2048, batch 1 + grad checkpointing.
4. Eval: held-out exact-match / edit-distance **+** a small general probe (e.g. a
   handful of unrelated prompts) to confirm no regression.
5. `mlx_lm.fuse` → merged 4-bit dir.
6. `rozum launch --model <dir> claude` and compare against the base on real tasks.

Expected: an afternoon, fits in 16–32 GB, and a clear yes/no on "did it help my
domain without breaking general use".

---

## 7. Verdict

- **Host stays inference-only.** Training is a separate workflow (data, eval,
  experiments) that does not belong inside the gateway.
- **Path (A) is "supported" already** (serve the merged checkpoint) and is the
  recommended way to *actually improve a model* for a domain — gated by a held-out
  eval, every time.
- **Path (B)** is a small, optional future nicety (adapter-at-serve / AFQ
  load-time merge) only if we ever serve a family of tunes.
- **Path (C)** — online/continual self-improvement — is a research project whose
  difficulty is data/signal/eval/forgetting, not the autodiff engine. Interesting
  to think about; wrong thing to bolt onto a serving host today.

The one-line takeaway: **the framework can train; the limiting reagents are RAM,
a real evaluation, and an honest training signal — not the code.**
