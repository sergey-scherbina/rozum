# Planner → Executor (decompose a task across two local models)

## Goal

Run a coding task as **two sequential stages on two different local models**, each playing to its
strength: a **planner** that reasons out the whole solution in one shot, then an **executor** that
delivers that solution through the agentic tool-loop (write files, run, fix). One model resident at a
time — safe on a 36 GB no-reboot host, no co-residency overcommit.

This is **decomposition** (split the task by ROLE: think vs do), distinct from the
[cascade router](cascade-router.md) which is **escalation** (same task, retry on a stronger model when
the cheap one fails). They are complementary; this spec is the planner→executor pipeline.

## Motivation (grounded in the matrix data, 2026-06-23)

The full 3-model agentic matrix + isolation probes proved an asymmetry between the two local models:

- **GLM-4-32B** writes **correct code and reasons well in one shot** (raw tokens showed it emit
  `args[1].chars().rev().collect::<String>()`), but it **fails to DELIVER agentically** — high run-to-run
  variance, `cargo new` into a subdir, broken `echo '…\n' > file`, stops at the verify step. Agentic
  score 4/15. Its create-from-scratch *delivery* could not be fixed at the gateway (4 hypotheses → A/B →
  refutation; the failures are the model's tool-use variance, not one shape). See SPRINT
  `glm-shell-delivery-fix`.
- **gpt-oss-20b** **delivers reliably** through the agent loop (edit/fix/run, 12/15), but its
  *create-from-scratch* code has a correctness tail (sometimes incomplete `main`).

So neither is strong end-to-end at create-from-scratch alone. **Composed**, they cover each other:
**GLM produces the correct solution; gpt-oss reliably lands and verifies it.** GLM's delivery weakness
and gpt-oss's from-scratch-correctness weakness both fall away.

## Design

A two-stage pipeline, **sequential** (one model resident at a time):

```
Stage 1 — PLANNER (GLM-4-32B):
  input:  the full task ("create a Rust CLI that reverses its arg …")
  prompt: "Produce the COMPLETE solution: every file's full final contents + a one-line build/run
           command. Do NOT execute anything — just output the solution."
  output: the solution as text (fenced files + commands)

  → unload the planner (free its RAM)

Stage 2 — EXECUTOR (gpt-oss-20b, via the agent loop):
  input:  the task + the planner's solution as context
  prompt: "Here is a vetted solution. IMPLEMENT it exactly: write each file, then build/run/test and
           fix any error until it works."
  output: files on disk, verified (cargo run / cargo test green)
```

The executor still runs the normal agent loop (claude/codex/opencode or the built-in agent), so its
reliable tool-use is preserved; it just starts from a correct solution instead of inventing one.

### No-reboot fit

The two stages are **never co-resident**. Stage 1 loads the planner, generates, then the planner is
unloaded (gateway `switch`/`unload`) before the executor loads. Peak residency = max(planner, executor),
not the sum — so a 19 GB GLM and a 17 GB gpt-oss run on a 36 GB host one after the other, with the
existing admission gate + adaptive load + keep-free all applying per stage. This sidesteps the
cascade's co-residency limit on this host.

### Handoff

The planner's raw text (the solution) is injected into the executor's first user message as a
`<solution>…</solution>` (or fenced) block. No structured parsing required — the executor reads it as
context. If the planner emits multiple files, they are passed verbatim; the executor decides how to
write them (its strength).

### Invocation (proposed)

- CLI: `rozum solve --planner <model> --executor <model> -- <agent> "<task>"`, e.g.
  `rozum solve --planner GLM-4-32B --executor gpt-oss-20b -- codex "create a reverse-cli …"`.
- Or a config block `[solve]` with `planner`/`executor` defaults, and `rozum solve "<task>"`.
- Internally: stage 1 = a one-shot `/v1/chat/completions` to the planner gateway; stage 2 = `rozum
  launch` the executor with the task+solution prompt. The model swap reuses the existing gateway
  `switch`/sequential-load machinery.

## When it pays off (and when not)

- **Pays off:** non-trivial tasks where *planning/reasoning* is the hard part — multi-file scaffolds,
  algorithmic problems, "design then implement" — where GLM's one-shot quality lifts a weaker-from-
  scratch executor. The handoff cost (one planner generation + one model swap, ~30–40 s) is amortized.
- **Not worth it:** trivial tasks (reverse a string) where the executor alone succeeds — the planner
  adds latency without value; and pure EDIT/fix/debug tasks where gpt-oss is already strong (12/15) and
  there's nothing to "plan". Use a single model there.

## Validation plan

A/B on a set of create-from-scratch tasks that a single model fails:
1. **Baseline:** gpt-oss alone (agent loop) — measure pass-rate.
2. **Planner→executor:** GLM solves → gpt-oss implements — measure pass-rate + wall-clock (incl. swap).
3. **Confounds:** confirm the win is the *plan* (try gpt-oss-plans→gpt-oss-executes as a control), and
   that the handoff doesn't leak GLM's broken delivery (the executor must re-implement, not copy GLM's
   shell). Slot-gated (no-reboot): one model at a time, graceful teardown between stages.
   Done-when: planner→executor build pass-rate > max(GLM-alone, gpt-oss-alone) on the from-scratch set,
   0 reboot.

## Risks / open

- **Swap cost** (~30–40 s/task) — acceptable for hard tasks, not for a tight inner loop. Mitigate: keep
  the executor resident across many tasks once chosen; only re-invoke the planner when a task is "hard".
- **When to trigger** — a classifier ("is this task hard enough to plan first?") could gate it; reuse the
  cascade's `classifier`. Out of scope for the first cut (explicit `solve` invocation).
- **Handoff fidelity** — the executor might over-trust a wrong plan; the executor's own build/verify loop
  is the safety net (it must make `cargo run`/`cargo test` actually pass, not just copy the plan).
- **Generalization** — start with GLM→gpt-oss; the roles are configurable (any strong-reasoner →
  any reliable-executor, incl. a cloud planner + local executor).
