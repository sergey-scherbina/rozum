# Small-model cascade — task-typed, small-first, escalate-on-doubt

## Overview

The narrow, **bounded, single-shot** jobs a 4B/Coder-7B can actually do — commit
messages, PR descriptions, code explanations, renames, docstrings, simple one-line
fixes — run **small-first** behind a cheap, **task-specific validator gate**: the small
model answers, a fast deterministic check decides `ACCEPT` (good enough — ship the cheap
answer) or `ESCALATE` (hand off to the big model). Most requests resolve on the cheap
tier; only the hard residue pays for the big model.

This is the **after-the-fact** counterpart to `[[small-model-router-rag]]` (which decides
**up front** where to send the work). It is **not a new engine** — it's a thin preset on
the already-shipped cascade (`docs/specs/cascade-router.md`): a task-typed
`AcceptanceCheck` gate + a two-tier (`small`, `big`) `CascadeConfig` + a prompt builder.
Everything the cascade already provides — health/backoff, budget, learned stats, lanes —
comes for free.

## Interface

```
SmallTask { CommitMessage, … }                 // the bounded task types (start: CommitMessage)
small_task_config(task, small: ModelCard, big: ModelCard) -> CascadeConfig
commit_message_request(diff: &str) -> ChatRequest   // tight task prompt for the small tier

CommitMessageGate { max_subject_chars }        // an AcceptanceCheck: validate an LLM commit msg
```

- The gate is a plain `cascade::AcceptanceCheck` — `decide(req, answer) -> Verdict` —
  so it slots into the existing acceptance pipeline (L0 structural → the task gate).
- `small_task_config` returns a `CascadeConfig` the caller turns into a
  `CascadeBackend`; the request is built by the per-task prompt helper (or by the
  caller). One model in / one model out is the degenerate passthrough (cascade core).
- Only worth it where a **cheap, reliable accept/reject signal exists**. For
  open-ended generation with no validator, lead with the big model instead (don't
  build a gate that just always-accepts — that's a slow passthrough).

## Behavior

- [ ] A good cheap answer is `ACCEPT`ed at tier 0; the big model is **never called**.
- [ ] A bad/empty/refusal/oversized cheap answer `ESCALATE`s to the big model, which
      inherits the request and answers.
- [ ] The gate is deterministic and free (no model, no I/O for the structural task
      types); a backend error escalates (inherited from `StructuralCheck`, run first).
- [ ] `commit_message_request(diff)` builds a tight prompt (imperative subject, ≤ N
      chars, no chatter) that steers the small model toward a gate-passable answer.
- [ ] Over a batch, the **small-tier hit-rate** is measurable: count tier-0 accepts vs
      escalations (the cascade already records per-attempt outcomes; a test asserts the
      cheap tier handled the passing requests without escalation).

## Out of scope (v1)

- Task types whose gate needs an **external process** (e.g. `OneLineFix` gated by
  `cargo check`, `Rename` by a build/lint) — the gate trait supports them (it can run a
  command), but a hermetic fast test can't, so v1 ships the **structural** task type
  (commit-message) and leaves process-gated tasks as a documented follow-up.
- CLI/gateway wiring (`rozum commit-msg` from `git diff --cached`) — a thin follow-up on
  top of the library entrypoint; v1 is the library + gate + prompt builder + tests.
- Learned per-task acceptance thresholds — the cascade's `StatsStore` already records
  the outcomes; tuning is the existing learned track, not this preset.

## Design

- **`src/cascade/tasks.rs`** — `SmallTask`, `CommitMessageGate`, `small_task_config`,
  `commit_message_request`. Re-exported from `cascade`.
- **The gate as an `AcceptanceCheck`** — `CommitMessageGate::decide` ignores structured
  output (commit messages are free-form text, so `StructuralCheck` returns `Inconclusive`
  and defers to us). It `ESCALATE`s on: empty subject, a subject over `max_subject_chars`
  (default 72), or a subject that looks like a refusal/placeholder ("i can't", "as an AI",
  "TODO", "<commit message>", a bare fence). Otherwise `ACCEPT`. Never `Inconclusive` —
  it's the decisive task gate.
- **Acceptance pipeline** = `[StructuralCheck, <task gate>]` (drop the self-signal L1:
  these tasks have a concrete validator, not a self-reported confidence) and the
  escalation **affordance off** (no `[[ESCALATE]]` marker prompt for a bounded one-shot).
- **Two tiers** — `small` at `tier 0`, `big` at `tier 1`; `AlwaysCheapest` strategy (the
  whole point is to *try cheap first*, not classify up front). Budget default (one
  escalation hop is enough for two tiers).
- **Prompt builder** — `commit_message_request` embeds the diff in a tight instruction so
  the small model's output is gate-shaped; `temp 0`, modest `max_tokens`.

## Decisions

- **Preset, not a parallel system** — reuse `CascadeBackend`/`AcceptanceCheck`; the only
  new code is one gate + one config preset + one prompt helper. (Avoids the
  premature-abstraction trap the portability spec warns of.)
- **Structural task first (commit-message)** — it has a free, deterministic,
  hermetically-testable validator, so it proves the pattern end-to-end in the fast suite;
  process-gated tasks (one-line-fix) reuse the exact same shape once a non-hermetic gate
  is acceptable.
- **Gate escalates conservatively, never accepts junk** — the failure mode to avoid is
  shipping a bad cheap answer; an over-eager escalate just costs the big model (the
  baseline you'd have paid anyway).

## Verification

- Fast unit tests (no model): `CommitMessageGate` accepts a good message; escalates on
  empty / over-length / refusal / placeholder. `commit_message_request` embeds the diff
  and asks for a subject line.
- Fast e2e (mock backends, like the cascade tests): a small backend returning a good
  message → tier-0 accept, big never called; a small backend returning junk → escalates
  to big. A small-tier **hit-rate** test: a batch where the small tier passes K/N →
  assert the big backend was called exactly N−K times.

## Results

**DONE 2026-06-18** (`src/cascade/tasks.rs`, re-exported from `cascade`). A thin preset over
the cascade core — no new engine:
- `SmallTask::CommitMessage` + `small_task_config(task, small, big)` builds a two-tier
  `CascadeConfig` (`small`=tier 0, `big`=tier 1, `AlwaysCheapest`, acceptance =
  `[StructuralCheck, <task gate>]`, self-signal + affordance off). Both tiers local; a
  remote big tier builds the config directly (documented).
- `CommitMessageGate` (`AcceptanceCheck`): deterministic, free. Extracts the subject (first
  non-empty line, wrapping fence/quote/heading stripped) and `ESCALATE`s on empty /
  over-length (`max_subject_chars`, default 72) / refusal / chatter-preamble / `<placeholder>`;
  otherwise `ACCEPT`. Never `Inconclusive`. Tuned to avoid false positives on legit subjects
  (e.g. "Fix commit message parser crash" accepts).
- `commit_message_request(diff)` builds the tight, gate-shaped prompt (imperative subject ≤ 72,
  no chatter/fences; `temp 0`).

**10 fast tests, all green** (no model): gate accepts-good / escalates empty / over-length /
refusal+chatter+placeholder / strips-fence / no-false-positive; `commit_message_request` embeds
the diff; e2e over a real `CascadeBackend` with mock backends — good cheap answer accepts at
tier 0 (**big never called**), junk escalates to big once, and a **small-tier hit-rate** batch
(small passes 3/4 → big called exactly once). 383 fast tests green.

**CLI wiring DONE 2026-06-18** — `rozum commit-msg [--model <spec[,spec2]>]` (`src/main.rs`):
reads `git diff --cached`, builds the gate-shaped `commit_message_request`, and prints the
message. A single `--model` generates directly; a `small,big` comma-list builds the
`small_task_config(CommitMessage, …)` cascade (the small model answers, the
`CommitMessageGate` escalates to the big model only when the cheap answer is unusable). Model
defaults to `[runtime].model` from `rozum.toml`. `staged_diff_in` split out + unit-tested in a
temp git repo (stages a file, asserts the diff; empty index → empty, not an error); the
model-call path is manual (needs a real model).

**Follow-ups (deferred):** process-gated task types (`OneLineFix` via `cargo check`, `Rename`
via build/lint — the gate trait supports running a command, but not hermetically testable);
a remote big tier for `commit-msg` (v1 treats both tiers as local — `small_task_config`'s
documented limitation).
</content>
</invoke>
