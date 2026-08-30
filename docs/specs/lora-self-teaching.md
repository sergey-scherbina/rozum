# LoRA self-teaching (teach mode)

## Overview

The model learns from its operator, at the operator's request: a distinct **teach mode**
in which rozum collects (prompt, answer, rating, correction) pairs — including literally
asking the operator "how do you rate this?" / "how should this have been done?" — and
periodically trains a **LoRA adapter** on that dataset, entirely locally. The base model
stays frozen (SPRINT scope: `mlx-community:Qwen3.5-4B-MLX-4bit`); personalization lives in
a small, versioned, instantly-revertible adapter file. The collected dataset doubles as a
held-out eval set for the existing bench matrix, so every adapter is gated before it
serves.

Operator's binding constraints (2026-08-30, in-session):
- **Training is pure Rust/MLX** — no Python anywhere in the loop. This makes phase 1 a
  real porting effort (backward pass + optimizer in the mlx-rs stack) and is priced in
  below; data collection does not wait for it.
- **Opt-in only** — the model learns "по желанию пользователя": teach mode is an explicit
  toggle; nothing is collected while it is off.
- **Collection surfaces, all three**: Telegram bot first (inline 👍/👎/correct buttons +
  `/teach` mode command), UCC phone chat (rating buttons), CLI/TUI sessions.

## Interface

- **Dataset** — `~/.rozum/teach/dataset.jsonl` (0600, local-only, append-only): one JSON
  object per feedback event: `{ts, surface, model, chat_ref, prompt, answer, rating
  (up|down|score), correction?, tags?}`. `rozum teach export` renders it to the SFT chat
  format (Qwen3.5 chat template, loss masked on the prompt half); `rozum teach stats`
  reports counts/quality (rated, corrected, pairs suitable for DPO).
- **Teach mode toggle** — per surface: Telegram `/teach on|off` (per chat), UCC toggle,
  `rozum teach on|off` for CLI/TUI (per project). While ON: answers carry rating
  affordances, and the agent MAY ask a short evaluation question after non-trivial
  answers (rate-limited — never more than one ask per N answers, configurable).
- **Trainer (pure Rust)** — `rozum teach train [--epochs N --rank R --lr X]`: QLoRA-style
  — frozen 4-bit base, trainable fp16 LoRA on the attention + MLP projections of the
  qwen3_5 family (configurable target set); produces
  `~/.rozum/teach/adapters/<version>/` = safetensors + manifest (base-model SHA, dataset
  SHA, hyperparams, eval scores). Prerequisite (own backlog item): autodiff + AdamW in
  the vendored mlx-rs stack — mlx-c already exposes `value_and_grad`; the work is
  bindings + optimizer + LoRA-injected forward for training.
- **Serving** — `rozum teach apply <version> | rollback | list`: the native runtime loads
  base + adapter (fold the low-rank delta into the affected weights at load time — no
  per-token runtime cost, no permanent model mutation; the fused-in-memory result is
  admission-accounted like any resident). Adapter choice is carried on the model spec
  (e.g. `…4bit+teach@v3`) so panels/registry show what is actually serving.
- **Gate** — `rozum teach eval <version>`: before/after on (a) a fixed bench-matrix
  subset (regression guard) and (b) a held-out slice of the operator's own pairs
  (personalization signal). `apply` refuses a version whose regression exceeds the
  threshold unless `--force`.

## Behavior

- [ ] Teach mode OFF ⇒ zero collection: no dataset writes, no rating affordances, no
      evaluation questions.
- [ ] Telegram: 👍/👎 on an answer appends a rated event; replying to an answer in teach
      mode with a correction appends a corrected pair; `/teach on|off` per chat.
- [ ] UCC chat and CLI/TUI: same events through their own affordances; all three
      surfaces write the SAME dataset schema.
- [ ] The evaluation-question ask respects the rate limit and never blocks the answer
      itself (it trails, fire-and-forget).
- [ ] `teach export` produces valid Qwen3.5 chat-template SFT samples with prompt-side
      loss masking; corrected pairs export correction-as-target.
- [ ] `teach train` runs entirely in-process (Rust/MLX): no Python, no network; peak RAM
      respects the residency-admission ledger (training co-exists with or preempts a
      resident per the existing queue rules, never OOMs the host).
- [ ] Adapter versions are immutable; `apply`/`rollback` switch between them and plain
      base without re-downloading or mutating the base snapshot.
- [ ] `teach eval` reports both metrics; `apply` refuses on regression past threshold
      (override only with `--force`).
- [ ] Deleting `~/.rozum/teach/` removes every trace (dataset + adapters) — the whole
      feature is one directory.

## Out of scope

- Multi-user/federated learning, any cloud or telemetry — this is one operator, one
  machine.
- RLHF beyond DPO; reward models.
- Full fine-tuning of base weights; training any model other than the frozen 4B family.
- Continuous/online learning — training is an explicit operator action.

## Design

Phased; each phase is independently shippable and separately backlogged:

- **Phase 0 — collect** (`teach-collect`): dataset + toggles + all three surfaces +
  export/stats. Ships value on day one (the dataset is also an eval asset) and gates
  nothing on the trainer. Cold-start reality: SFT below ~100 quality pairs overfits, so
  collection must run ahead of training anyway.
- **Phase 1 — trainer in Rust** (`teach-train-rust`): autodiff/optimizer/LoRA-forward in
  the mlx-rs stack, then the training CLI. The hard prerequisite is explicit: mlx-c
  `value_and_grad` bindings into our fork, AdamW, gradient flow through the quantized
  forward's LoRA branches only.
- **Phase 2 — serve + gate** (`teach-serve-adapters`): load-time weight folding,
  versioning, apply/rollback, eval gate wired to the bench matrix.
- **Phase 3 — DPO** (`teach-dpo`): when corrected pairs accumulate, prefer
  (correction ≻ original) preference training over plain SFT; same trainer plumbing,
  different loss.

Load-time folding over runtime low-rank paths: zero inference-cost, zero new code on the
hot path, and rollback is just reloading; a runtime adapter path can come later if
hot-swapping without reload ever matters.

## Decisions

- **Pure Rust/MLX trainer** — operator decision, binding: no Python even offline. Costs
  a real porting phase before first training; bought: one toolchain, one memory ledger,
  the trainer obeys the same admission rules as inference.
- **LoRA adapter over full fine-tune** — revertible, versionable, tiny, base snapshot
  stays pristine; catastrophic-forgetting risk is contained and gated.
- **Explicit teach mode over ambient collection** — the operator asked for exactly this
  ("модель учится у пользователя по его желанию"); it is also the privacy story.
- **SFT first, DPO second** — ratings/corrections naturally produce preference pairs,
  but SFT on curated positives is the simplest correct start; DPO needs volume.
- **Gate before serve** — a tiny personal dataset can silently degrade general ability;
  the bench matrix already exists, so using it is nearly free.

## Results

(to fill per phase at verify: dataset event counts by surface; first adapter's eval
before/after table; RAM peak during training vs admission grant; rollback drill.)
