# Prompt-lookup decoding — draft-free speculative decode for code/agentic work

Status: SPEC + P0 proposer LANDED (2026-06-22, `sunny-civet`). The draft-free proposer
(`PromptLookupDraft`, `impl Draft`) + unit tests are in; the decode-loop wiring + model
A/B are slot-gated and touch the MLX decode path (coordinate with the engine owner).
Builds directly on the existing spec-decode infra (`crates/rozum-mlx/src/specdecode.rs`,
`docs/specs/speculative-decoding.md`).

## One-line

Speed up single-stream decode by proposing the next *k* tokens from an **n-gram lookup
of the context** (no draft model) and verifying them in one target forward — a pure
latency win on the agentic/code workload, where the model re-emits large **verbatim**
chunks of the file it just read.

## Why now — three dead-ends and one workload property

Single-stream decode is the dominant cost of an agent loop, and the obvious levers are
spent or off:
- **`mx.compile`d decode — NO-GO** (`SPRINT` decode-compile Stage 0): plain compile is
  *0.58–0.69×* (slower) on 0.6B; decode is FFI/op-launch bound, compile adds overhead.
- **Draft-model spec-decode — net-negative on MoE** (`[[project-spec-decode-economics]]`):
  on the recommended MoE target (Qwen3-30B-A3B) it was **2.8× slower** — a 4B draft is no
  cheaper than the 3B-active MoE, so the draft *is* the cost.
- **Batching — shipped**, but only helps *concurrent* requests; a single agent is serial.

What nothing here exploits: **agentic edit/fix/refactor output is highly self-similar to
its prompt.** The model reads a file, then re-emits it with a few lines changed — long
runs of tokens that already appear verbatim in the context. Prompt-lookup turns that
redundancy into speed at ~zero draft cost.

## Mechanism — a new `Draft`, the verify/decode loop unchanged

The spec-decode module already factors the exact seam we need
(`crates/rozum-mlx/src/specdecode.rs`):

```rust
trait Draft  { fn propose(&mut self, ctx: &[TokenId], k: usize) -> Vec<TokenId>; }
trait Target { fn verify(&mut self, ctx: &[TokenId], draft: &[TokenId]) -> Verify; } // 1 forward, accept-longest-greedy-prefix
fn decode_streaming(prompt, draft: &mut dyn Draft, target: &mut dyn Target, k, …)
```

Prompt-lookup is **one new `impl Draft`** — no draft model, no second resident, no extra
GPU memory:

```text
PromptLookupDraft::propose(ctx, k):
  let ngram = ctx[ctx.len()-n ..];            // last n tokens (n≈2–3, tunable)
  find the MOST RECENT earlier occurrence of `ngram` in ctx (scan right→left)
  if found at index j:  return ctx[j+n .. j+n+k]   // the k tokens that followed it
  else:                 return []                  // no proposal → falls back to plain decode
```

`Target::verify` (already implemented) scores the proposal in **one** forward against the
target's own greedy continuation and emits the longest matching prefix + the first
divergent greedy token. So a high-overlap region (re-emitting a read file) accepts *k*
tokens per forward; a novel region proposes nothing and costs exactly one forward — never
worse than plain decode. **The entire loop, KV handling, and verify are reused as-is.**

Context for the lookup = prompt tokens **+** tokens generated so far (so the model can also
copy from its own recent output, e.g. a repeated signature). Cap the scan window
(`ROZUM_PLOOKUP_WINDOW`, e.g. last 8k tokens) so the search stays O(window).

## Correctness — greedy-parity, and the one MoE caveat

- **Byte-exact on DENSE targets.** Verify only accepts tokens equal to the target's own
  greedy pick, so the output is identical to plain greedy decode — a latency win, not a
  quality change. Same guarantee the draft path already gives.
- **The MoE seq-length caveat (inherited, must be documented).**
  `[[project-spec-decode-moe-numerics]]`: on quantized MoE, `gather_qmm` is **not
  bit-invariant to sequence length** — a verify forward at `L=k+1` can produce logits that
  differ from the `L=1` decode at **near-ties**, so byte-exactness can break by a token at
  a coin-flip boundary (within sampling noise, but not bit-identical). This is exactly why
  draft spec-decode stays off for MoE. Prompt-lookup shares it. Options (P2):
  1. **Restrict byte-exact mode to dense targets**; offer MoE behind an explicit
     "accept near-tie divergence" flag (the divergence is statistically negligible).
  2. Verify at the **same `L`** the model would use (re-decode accepted tokens at `L=1`)
     — removes the win, defeats the point; rejected.
  3. Accept it: for *greedy* agentic decode the divergence is a rare single-token flip,
     and the agent's correctness is judged by final file state, not bit-identity.
  Decision deferred to the P0/P2 data.

## Economics — why it flips the MoE net-negative

Draft spec-decode lost on MoE because the draft forward ≈ the target forward. Prompt-lookup's
"draft" is a **string search (microseconds, CPU)** — effectively free. So:
- Per accepted token: pure win (k tokens for ~1 forward).
- Per miss: one verify forward at `L=k+1` instead of `L=1`. On MoE, forward cost grows with
  L, so a *wasted* speculation has a small penalty (the `k` extra positions). Net win
  requires acceptance high enough to amortize that — which the code-editing workload
  delivers (long verbatim runs). Keep `k` modest (3–5) so a miss is cheap.
- **No memory cost** (no draft resident) — so it composes with the residency gate and never
  pushes toward the BUG-003 overcommit, unlike a second model.

## Where it sits

- `rozum-mlx::specdecode`: add `PromptLookupDraft` (`impl Draft`) + its unit tests. Pure
  token-slice logic — **hardware-free, unit-testable without a model**.
- Wire into the decode entry the same way the draft path is selected, opt-in:
  `ROZUM_PLOOKUP=1` (and `ROZUM_PLOOKUP_K`, `_NGRAM`, `_WINDOW`). Off by default until P3
  matrix-gated, like spec-decode.
- Gateway auto-detect: enable for code/agentic requests; leave plain decode otherwise.

## Phased plan (each independently shippable + benchmarked)

- **P0 — proposer + offline A/B (mostly slot-free).** ✅ **Proposer DONE** —
  `PromptLookupDraft` (`crates/rozum-mlx/src/specdecode_plookup.rs`, `impl Draft`) + 6 unit
  tests on token slices, hardware-free (most-recent-match, periodic continuation, window
  bound, k/max_k clamp, no-match, short-ctx). REMAINING: a small probe on a DENSE model
  (e.g. Qwen3-4B) over a **real edit transcript** (read-file → re-emit-with-change) to
  measure accept-rate + tokens/forward vs plain decode (slot-gated). **Gate:** acceptance
  high + net speedup on the copy-heavy region, byte-exact.
- **P1 — wire into the decode loop** behind `ROZUM_PLOOKUP`, dense only. Byte-exact vs
  plain greedy on fixed prompts.
- **P2 — MoE decision.** Measure the near-tie divergence rate on Qwen3.6-35B; pick the
  dense-only-byte-exact vs accept-divergence policy from data.
- **P3 — matrix gate.** Run the agentic matrix with `ROZUM_PLOOKUP=1`; confirm pass-matrix
  unchanged + measure end-to-end speedup on edit/fix/debug tasks. Ship on-by-default for
  code requests only if the matrix is identical and the speedup is real.

## Risks & open questions

- **Acceptance on real agent transcripts** is the whole bet — P0 must measure it on actual
  read→edit flows, not synthetic repetition. If acceptance is low outside pure rewrites,
  keep it opt-in/code-only.
- **MoE byte-exactness** (above) — the one correctness nuance; P2 decides.
- **Interaction with hybrid prefix-reuse / the KV cache.** The verify forward must use the
  same KV path as plain decode; confirm it composes with the hybrid cache (the same care
  the draft path needed).
- **`n`/`k` tuning** — too-greedy `k` wastes forwards on misses; P0 sweeps it.

## Non-goals

- Not a replacement for draft spec-decode where a genuinely cheaper draft exists (slow
  DENSE target + tiny draft still wins there, `[[project-spec-decode-economics]]`).
- No quality change: greedy-parity is the contract (modulo the documented MoE near-tie).
- Not on by default for non-code requests in v1.
