# Memory × correctness frontier

## Overview

Rozum must choose the smallest-memory execution path that still has sufficient evidence of
correctness. Memory and quality are already measured independently; this feature joins them into
one decision surface, bounds retained prefix KV by bytes rather than slot count alone, and prevents
an unavailable semantic judge from being recorded as a successful verification.

## Interface

### Benchmark evidence

`scripts/bench/agentic.sh` appends these backward-compatible columns to `per-run.csv`:

- `verifier_kind` — `benchmark-deterministic`, `explicit`, `derived`, `structural`, or
  `model-judge`;
- `verdict` — `pass`, `fail`, or `unknown`;
- `verdict_confidence` — evidence strength in `[0,1]` (`1` for a deterministic check that ran);
- `gateway_generation`, `context_window`;
- `mlx_active_mb`, `mlx_peak_mb`, `mlx_cache_mb` sampled from `/stats` after the run.

When a new MLX resident starts and no other MLX resident is live, its registration resets the
process-global allocator peak. This makes the next `/stats` peak and footprint-cache observation
generation-scoped. The peak is never reset while models are co-resident.

`scripts/bench/memory_correctness_frontier.py <per-run.csv>...` emits a model × driver frontier.
For each candidate it reports pass rate, a 95% Wilson lower bound, peak memory, mean time, and
GiB-seconds per solved task (failed attempts count toward the cost). `--json` emits the same data as
JSON. A candidate is Pareto-optimal when no other candidate has at least its correctness lower bound,
no greater peak memory, and no greater GiB-seconds per solve, with at least one strict improvement.

### Prefix KV budget

- `ROZUM_PREFIX_CACHE_MB=<MiB>` bounds *additional retained conversations* by estimated KV bytes;
  default `1024` MiB.
- `ROZUM_PREFIX_CACHE_SLOTS` remains a hard entry-count ceiling for compatibility.
- The most-recent conversation may exceed the byte budget by itself: keeping the one active prefix
  avoids a full re-prefill; extra conversations are evicted LRU until both limits hold.
- At host `warn` or `critical` memory pressure, the effective retention policy collapses to the one
  most-recent conversation regardless of the configured extra-session budget.
- `ROZUM_PREFIX_CACHE=0` still disables prefix persistence entirely.

The estimate is `resident_positions × kv_bytes_per_position`. When architecture metadata is
unavailable the store uses a conservative fallback per-token cost. The budget is a retention policy,
not the host-safety boundary; the residency ledger and request KV preflight remain authoritative.

### Verification verdict

Semantic verification returns one of:

```text
Pass
Fail(reason)
Unknown(reason)
```

Malformed JSON, a missing `pass` field, timeout, and unreachable judge are `Unknown`, never `Pass`.
For a multi-model chain, a distinct model is used as judge when available and models remain
sequentially resident. Deterministic checks retain precedence and do not pay for a model judge.

Persistent model quality keys are scoped as:

```text
model × driver × executor-role × task-class × verifier-kind
```

The legacy model × role records remain readable data but are not used to skip a task-conditioned
model. The first chain link and last resort are never auto-skipped.

## Behavior

- [ ] Existing matrix CSV readers continue to accept the appended evidence columns.
- [ ] Sequential model generations do not inherit the previous model's allocator high-water peak.
- [ ] The frontier includes failed attempts in memory-time cost and identifies dominated candidates.
- [ ] A single run cannot be presented as high-confidence merely because it passed; the Wilson lower
      bound exposes the evidence count.
- [ ] PrefixStore evicts LRU entries until the slot ceiling and byte budget hold, while retaining one
      oversized MRU entry.
- [ ] Host memory pressure evicts all but the most-recent prefix without interrupting the active
      request.
- [ ] Prefix reuse remains byte-identical to a fresh prefill; the policy changes retention only.
- [ ] A missing or malformed model-judge response produces `Unknown` and cannot end a verified launch
      successfully.
- [ ] A multi-model fuzzy-task judge uses a model distinct from the executor when one exists, without
      co-residency.
- [ ] Quality history for one task class cannot exclude the same model from another task class.
- [ ] Multi-model Solve launches default to lazy residency unless the operator explicitly overrides
      `ROZUM_PIPELINE_EAGER`.

## Out of scope

- Changing model weights, quantization formats, or sampler numerics.
- Claiming formal correctness for open-ended tasks; `Pass` means the configured verifier supplied the
  required evidence.
- A hard allocator ceiling: MLX's memory limit is soft, so host safety remains admission + pressure
  shedding.
- Automatically launching the expensive real-model matrix while another gateway/client holds the
  model slot.

## Design

The benchmark layer owns the cross-run Pareto calculation because it already has driver, task,
duration, pass/fail, and model-footprint evidence. The gateway `/stats` snapshot adds allocator
active/peak/cache context without introducing a new telemetry service.

`PrefixStore` keeps an estimated byte cost on every dense/hybrid entry. Insertions are MRU-first;
trimming removes from the back. The one-entry exception is deliberate: a byte limit smaller than the
current conversation should prevent *additional* retained sessions, not force an expensive full
prefill on every turn.

The verifier uses a three-state enum internally. Only `Pass` records a successful model outcome.
`Fail` carries an actionable reason into repair. `Unknown` carries an infrastructure/evidence reason
and follows the same bounded escalation path, but is recorded separately in benchmark evidence.

## Decisions

- **Optimize evidence, not self-reported confidence** — pass probability comes from verifier outcomes;
  model prose is never counted as success by itself.
- **Use Wilson lower bound** — it penalizes tiny samples and avoids promoting a one-shot 1/1 model as
  equivalent to a stable multi-rep model.
- **Budget extra KV while retaining one MRU** — bounds multi-session accumulation without destroying
  the latency and activation-memory benefit of single-session prefix reuse.
- **Unknown is non-success** — fail-open prevented false rejection but allowed false completion; a
  bounded escalation/explicit unverified result is the honest contract.
- **Task-conditioned history** — model capability is relational and task-dependent; global role stats
  are too coarse for routing.

## Results

To be filled after implementation and verification.
