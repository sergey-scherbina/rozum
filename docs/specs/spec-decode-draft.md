> **Status (2026-06-18): concluded — parked, opt-in only.** P0 (byte-identical greedy on a
> DENSE target) and P1 (dual-model MLX worker + gateway auto-detect) are built and proven
> correct. Matrix verdict: **speed net-negative on the recommended MoE targets** (Qwen3-30B-A3B
> 34.8 vs 98.1 t/s, ~2.8× slower) — a MoE verify-forward cost scales with tokens and a 4B draft
> is no cheaper than 3B-active MoE decode. So spec-decode **stays off-by-default**; it wins only
> for a slow DENSE target + a tiny draft. Productionized code: `src/specdecode.rs` /
> `src/specdecode_backend.rs`. Design reference ported from the removed `feature/spec-decode-draft`
> branch (still on origin). See memory `project-spec-decode-economics`, `project-spec-decode-moe-numerics`.

# Speculative decoding — a small draft model accelerates a big target

## One-line

Run two resident models on one machine: a small **draft** (e.g. `Qwen3-4B-4bit`)
proposes `k` tokens cheaply; the big **target** (e.g. `Qwen3.6-35B-A3B-4bit`)
verifies all `k` in **one forward** and accepts the longest correct prefix. Net:
fewer big-model forwards → **lower latency with byte-identical greedy output**. It
is the single-machine realization of the North Star's "co-use multiple model
tiers" (SPEC.md § North Star).

## Why it's free (greedy)

This is **not** a quality tradeoff — the emitted sequence is *exactly* what the
target would produce greedily. Every token we emit is the **target's own greedy
argmax** at that position; the draft only changes *how fast* we discover it (a
correct guess saves a target forward; a wrong guess costs nothing — we fall back
to the target's argmax we just computed). The draft can be any model; correctness
depends only on the target.

## The algorithm (greedy)

State: the target's KV cache covers the accepted prefix `[x_0 … x_t]`; the next
target greedy token is `argmax p_t` (the target's logits at position `t`, kept
from the previous step).

1. **Draft proposes.** From `x_t`, the draft autoregressively generates `k` tokens
   `d_1 … d_k` (the draft greedily continues; its own KV advances `k` steps). Cheap
   — the draft is small.
2. **Target verifies in ONE forward.** Feed `[d_1 … d_k]` to the target as `k` new
   positions over its cached prefix; the forward returns the target's logits at
   each, i.e. its greedy tokens `t_1 … t_k` where `t_i = argmax p(·| x_0…x_t, d_1…d_i)`
   (plus the bonus `t_{k}` = the target's next token *after* `d_k`).
3. **Accept the longest correct prefix.** Walk `i = 1..k`: the target's greedy
   token at position `t+i-1` is `argmax p_{t+i-1}`. Accept `d_i` iff it equals that
   argmax. On the **first mismatch** `j`, emit the target's argmax there (the
   correction) and **reject** `d_{j+1..k}`. If **all `k` match**, also take the
   **bonus** token `argmax p_{t+k}` for free → `k+1` tokens this round.
4. **Roll back + repeat.** The target forward advanced its KV by `k` positions;
   truncate it back to the accepted length and loop. Each round emits **1…k+1**
   target tokens for **one** target forward + `k` cheap draft forwards.

Speedup ≈ the **mean accepted length** per target forward (high when the draft
agrees with the target — same family, easy spans like boilerplate/code). `k`
(draft lookahead) is tunable; too large wastes draft work on low-acceptance tails.

## KV-cache management — the crux

The target processes `k` speculative positions, so its KV must be **rolled back**
to the accepted length when some are rejected.

- **Dense targets** (Qwen3, Qwen3-Coder, gpt-oss, Llama — external
  `ConcatKeyValueCache`): rollback is `cache.truncate(accepted_len)` — already
  used by prefix reuse, O(1). **This is the first target tier.**
- **Hybrid targets** (Qwen3.6 GatedDeltaNet): the recurrent conv/linear state is
  **not freely truncatable** (see `HybridPrefix`) — a rejected speculation has
  already advanced the recurrent state. Rollback needs a **snapshot before** the
  speculative forward and a **restore on rejection**, exactly the
  truncate-`Full` + restore-`Linear`-from-snapshot machinery prefix reuse uses.
  Deferred to a later phase; **dense-target first**.

The **draft's** KV also advances `k` steps per round and rolls back to the
accepted length the same way (the draft is dense — Qwen3-4B — so `truncate`).

**Validated (P0 harness, 2026-06-17).** `run_spec_decode` in `mlx_native_backend`
(gated test `spec_decode_matches_greedy`) implements exactly this: prefill both →
each round draft proposes `k`, target verifies `[cur, d1..dk]` in one
`dense_forward`, `accept_greedy` takes the matching prefix, both caches
`truncate(kv + 1 + accepted)`. With a **dense target** (Qwen3-4B as target+draft)
the output is **byte-identical** to plain greedy — including under forced
partial-accept rounds — confirming the truncate-based rollback + the overlapping
in-place cache rewrite are correct. See the quantized-MoE caveat below.

## Two-model residency

Both models live on the **single MLX worker thread** (all MLX work is
single-threaded; §`mlx_native_backend`). Draft `Qwen3-4B-4bit` (~2.5 GB) + target
`Qwen3.6-35B-A3B-4bit` (~20 GB) fit in 36 GB alongside activations. Constraints:

- **Shared tokenizer / vocab.** Draft and target must agree on token ids (the
  draft's `d_i` are compared to the target's argmax over the same vocab). The
  **Qwen3 family shares a tokenizer** (Qwen3-4B ↔ Qwen3.6-35B-A3B ↔ Qwen3-Coder),
  so draft+target within the family is the supported pairing. Cross-family is out
  of scope (different vocab → meaningless comparison).
- Loaded once, resident across requests (like the main model). A `--draft-model`
  (or `ROZUM_DRAFT_MODEL`) selects it; absent ⇒ the plain decode path, unchanged.

## Where it plugs in

The MLX leaf's decode loop (`run_job` → the per-model `Generate` / the dense
`dense_forward`). Add a **spec-decode loop** taken when a draft is configured:

- Reuse the target's multi-token **`dense_forward(tokens, cache) → logits[B,T,V]`**
  (the prefill path already does an N-token forward returning per-position logits)
  for the verify step.
- Reuse a small draft `Generate` for the `k`-token propose step.
- Feed the accepted tokens through the **shared `engine::consume_tokens`** so
  streaming / tool-call parse / EOS are unchanged (the spec-decode loop only
  changes *how token ids are produced*, not how they're consumed — clean fit with
  the `native-engine-spi` seam).

Greedy only at first (`temperature == 0`); a request that asks for sampling
(`temp > 0`, top-p, penalties) falls back to plain decode until the
sampling-spec-decode extension lands.

## Quantized-MoE numerics — the byte-identical guarantee has a floor

Spec-decode's "free" property assumes the target's forward is **invariant to
sequence length**: the verify scores `k+1` tokens in ONE forward (`L = k+1`),
while plain greedy decodes one token at a time (`L = 1`). For that to yield
byte-identical output, `argmax(L=k+1 forward)` must equal `argmax(L=1 forward)`
at every position.

- **Dense targets:** length-invariant — verified byte-identical (NUMCHECK control
  `first diff = None`; spec `first_div = None`).
- **Quantized MoE targets** (Qwen3-30B-A3B, Qwen3-Coder-30B-A3B): the quantized
  expert kernel **`gather_qmm` is NOT bit-invariant to `L`**. An `L=5` forward's
  logits differ from `L=1` by ~one quantization step (e.g. `37.25` vs `37.00`).
  These ~0.25-logit differences accumulate in the KV cache over a decode and
  occasionally **flip a near-tie** token (observed: `"so, for example, the 0th
  term"` vs `"so, the 0th term"` at index 72 — both valid greedy). This is a
  **model-numerics property, not a spec-decode bug**, proven three ways:
  1. dense target is byte-identical even with forced partial rounds;
  2. a **block-greedy control** (decode via `L=k+1` forwards with FIXED filler,
     take position-0 argmax — no drafts) diverges from `L=1` plain at the **same
     index** the spec-decode does, independent of the draft;
  3. on a clean cache, `L=1` and `L=5` agree (gap `0.50`); the flip only appears
     after the cache accumulates the per-step `L`-non-invariance.

  **Guarantee for MoE:** spec-decode is byte-identical to plain greedy **up to the
  point the target's own `L=k+1` forward stops being** (the numeric floor) and a
  **valid greedy decode** thereafter. The test asserts `spec.first_div >=
  block_greedy.first_div` under `ROZUM_SPEC_ALLOW_NUMERIC=1` — spec-decode adds
  **zero** divergence beyond the model's intrinsic floor. A true logic bug would
  diverge from round 0 and cascade.

  *Future (P4):* a length-invariant MoE verify (e.g. force the decode path to use
  the same `L`-block kernel, or a `gather_qmm` tiling that is `L`-stable) would
  restore strict byte-identity on quantized MoE. Out of scope for P0.

## Phasing

- **P0 — dense-target greedy + parity gate.** ✅ **Core validated** (gated harness
  `spec_decode_matches_greedy`): `run_spec_decode` (two-model, KV truncate
  rollback, `accept_greedy`) is **byte-identical** to plain greedy on a dense
  target and a **valid greedy decode** (+1.15–1.35× with a 4B draft) on quantized
  MoE — see the numerics section above. **Remaining:** wire behind `--draft-model`
  / `ROZUM_DRAFT_MODEL` in the serving path (strictly gated; absent ⇒ plain decode
  unchanged). Single-stream, B=1.
- **P1 — measure + tune.** tok/s vs non-draft on the dense target; sweep `k`;
  report mean accepted length. Bound the draft cost (don't let a low-acceptance
  tail erase the win).
- **P2 — hybrid target (Qwen3.6-35B-A3B).** Snapshot/restore the GatedDeltaNet
  state around the speculative forward (reuse `HybridPrefix`). **Gate:** identical
  output + speedup on the 35B-A3B.
- **P3 — sampling.** Temperature/top-p spec-decode (the accept/reject ratio test,
  Leviathan et al.) so non-greedy requests also benefit, still distribution-exact.

## Acceptance

- `--draft-model <spec>` (or `ROZUM_DRAFT_MODEL`) produces greedy output
  **identical** to the same request without it (a diff test on ≥2 prompts).
- A **measured tok/s speedup** on the target (P1: dense; P2: 35B-A3B), with the
  mean-accepted-length reported.
- No regression to the plain (no-draft) path; tool-call / streaming / EOS via
  `consume_tokens` unchanged.

## Non-goals

- **Not** cross-family draft/target (vocab mismatch).
- **Not** multi-device (the draft on an iGPU, target on a dGPU) — that's the x86
  heterogeneous track (`portability-heterogeneous-devices`); here both are on the
  one Apple-Silicon worker.
- **Not** tree/Medusa-style multi-branch speculation in v1 (linear `k`-token draft
  first; trees are a later optimization).

## Risks / open questions

- **Acceptance rate variance.** Speedup is workload-dependent (high on
  code/boilerplate, lower on hard reasoning). P1 must measure on real agentic
  prompts, not toy text.
- **Draft cost vs win.** `k` cheap draft forwards per round; if acceptance is low
  the draft is pure overhead. Mitigate with an adaptive `k` (shrink on a run of
  rejections) — design in P1.
- **Hybrid snapshot cost.** The GatedDeltaNet snapshot/restore per round (P2) may
  eat into the win; measure before committing to hybrid targets.
- **Two-model RAM.** 4B + 35B + activations on 36 GB is tight; the KV/RAM preflight
  must account for both resident models.

## Decisions (locked 2026-06-17)

- **Greedy, dense-target, Qwen3-family draft, single worker** first (P0). Hybrid
  target + sampling are later phases. Spec-first per spec-dev.
