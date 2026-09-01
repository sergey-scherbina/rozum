# Prefix-KV reuse for the constrained decode path

## Overview

Every tool-bearing request — which means every turn of every agent — routes out of `run_job`
into `run_constrained_{dense,hybrid}` before the prefix-reuse block, and their prefills build a
fresh cache over the full prompt. The measured cost on the live service (Qwen3.5-4B, agent turns
of 6–8k prompt tokens): ttft 5–10 s per turn at ~1.2 ms/token, growing with history — while the
same machinery on the unconstrained path reuses 6008/6030 tokens with ttft 357 ms. This feature
gives the constrained prefills the same take → truncate/restore → suffix-prefill → put cycle
`run_job` already has. Full diagnostic trail: BACKLOG `constrained-path-prefix-reuse`.

## Interface

No public surface changes. Internal:

- `run_constrained_dense/hybrid` gain a `store: &mut PrefixStore` parameter (threaded from
  `run_job`, which owns the borrow).
- `prefill_job_dense/hybrid` gain `store` and return, additionally, the data the caller needs to
  persist afterwards: `prompt_ids` and `conv_len` (and, for hybrid, the conversation-boundary
  `Vec<LinearSnap>`).
- `constrained_decode_loop` returns `Option<Vec<C>>` — the advanced cache on clean completion
  (including EOS/max-tokens), `None` when generation aborted on a send error, so an
  inconsistent cache is never persisted.
- `ROZUM_PREFIX_CACHE=0` disables reuse here exactly as on the unconstrained path.

## Behavior

- [ ] A dense constrained request whose prompt extends a stored conversation truncates the
      stored KV to the shared prefix and prefills only the suffix.
- [ ] A hybrid constrained request restores the Linear (GatedDeltaNet) state from the stored
      conversation-boundary snapshot, truncates the Full layers, and prefills only the suffix.
- [ ] After generation the advanced cache is re-inserted keyed by the conversation boundary
      (`conv_len`, the render WITHOUT the generation prompt), so the next turn matches.
- [ ] The hybrid snapshot is taken exactly at `conv_len` — two-phase prefill
      (`[reuse..conv_len)` → snapshot → `[conv_len..)`) — so restore-on-reuse is byte-exact.
- [ ] Byte-exactness: a constrained generation with reuse produces the same tool call as the
      same request against a fresh cache (asserted by test on the dense path; the hybrid
      truncate/restore primitives are covered by the existing `mlx_prefix_reuse_byte_exact_hybrid`).
- [ ] A prompt with no stored prefix behaves exactly as before (fresh full prefill).
- [ ] A cancelled/failed generation does not poison the store (no put without a clean finish).

## Out of scope

- Reuse inside `run_batch`/`run_batch_hybrid` (multi-row KV sharing is a different design).
- The VL/multimodal constrained path (image splice forbids token-prefix reuse, as in `run_job`).
- Store sizing/eviction policy changes — the existing LRU + byte budget applies unchanged.

## Design

`run_job` already owns `&mut PrefixStore` at the routing point, so threading is borrow-trivial.
The dense suffix-prefill needs no mask plumbing: `qwen3::Model::forward` with `mask: None`
builds its causal mask from the cache offset (`create_attention_mask(&h, cache, …)`), so a
prefill over `prompt[reuse..]` against a truncated cache is byte-exact by construction — the
same property the unconstrained path relies on.

The hybrid path is the delicate half: Linear layers carry recurrent state that cannot be
truncated positionally, only restored from a snapshot taken at the right offset. The
unconstrained path gets that snapshot from `Generate.prefill_snapshot`; the constrained path
has no `Generate`, so `prefill_job_hybrid` prefills in two phases around `conv_len` and
snapshots between them. Phase splitting is safe because prefill is causal and the fork's
`prefill` is already chunked internally.

## Decisions

- **Return the cache out of `constrained_decode_loop`** rather than persist inside it — chosen
  because the loop is generic over `C` and knows nothing about stores or conv boundaries;
  its two callers do. Rejected: passing store+ids into the loop (leaks path-specific policy
  into a generic decode loop).
- **Two-phase prefill for the hybrid snapshot** — chosen because it reuses the existing
  `prefill`/`snapshot` primitives byte-exactly. Rejected: adding a snapshot-at-offset API to
  the fork (more fork surface for the same result).

## Results

_To be filled at verify time — measured, not predicted. Planned measurements: the 3-turn agent
probe (`rozum launch --lean claude -p`, `ROZUM_PREFIX_DEBUG=1`) before/after on the live
service, and the byte-exact reuse-vs-fresh gate on the dense path._
