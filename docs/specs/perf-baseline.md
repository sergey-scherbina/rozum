# Perf-baseline — current t/s + the open micro-perf levers

Status: **DONE (2026-07-03, nimble-raven).** Run: `scripts/bench/results/20260703-124650/`.

Prep: sunny-civet, 2026-06-23 — lever audit done, tooling documented. RUN: slot freed after
GLM-4.7-Flash bench. This is the analysis half of the sprint's `#3 Micro-perf → perf-baseline`
item: catalog what is already realized, what the existing tooling measures, and file one
`perf-<lever>` task per genuine opportunity — grounded in code so the run is just "invoke these."

### Actual baseline numbers (Qwen3.6-35B-A3B-4bit-DWQ, n_ctx=8192, 2026-07-03)

| Metric          | Value               |
|-----------------|---------------------|
| Load time       | 5 s                 |
| Peak footprint  | 21,161 MB (20.7 GB) |
| TTFT            | 0.13–0.23 s         |
| Decode t/s (1 tok out) | 72.5 t/s   |
| Decode t/s (19–768 tok) | **81–83 t/s flat** |

**KV flatness confirmed:** t6(356 out)=81.4, t7(512)=81.4, t8(768)=81.0 — no O(context) regression.
Closes `perf-kv-ctxsweep-verify` (the ROZUM_CTXSWEEP cargo test is redundant; non-DWQ not cached).

All line refs are `crates/rozum-mlx/src/mlx_native_backend.rs` unless noted; the per-token `forward`,
the `Generate` iterator and `ConcatKeyValueCache` live in the pinned `mlx-rs` fork
(`crates/rozum-mlx/Cargo.toml` rev `12fac5c0`, `mlx-lm/src/…`).

## Don't rebuild — the measurement tooling already exists

**Single-stream, per-model, end-to-end (the t/s baseline):** `scripts/bench/run.sh`. Starts a
private offline gateway under `/usr/bin/time -l`, streams `POST /v1/chat/completions` for every task
in `tasks.jsonl`, records per task: TTFT (first content chunk via `Time::HiRes`), end-to-end time,
generated tokens, **pure decode tok/s excluding prefill**, a keyword PASS/FAIL, resident memory; plus
a per-model peak physical footprint. One model resident at a time → clean memory numbers. Knobs:
`BENCH_NCTX` (8192), `BENCH_LOAD_TIMEOUT` (600), `BENCH_KILL_JAVA`.

**Decode/prefill/batch micro-benches (in-code, `#[ignore]`):** run with
`cargo test -p rozum-mlx --features mlx-native -- --ignored --nocapture <name>`:
- `mlx_dense_backend_chat_tps` (:4819) — end-to-end dense chat, asserts ~19–20 t/s.
- `mlx_moe_backend_chat_tps` (:4800) — end-to-end MoE chat, asserts ~96 t/s (hybrid pipelining).
- `mlx_qwen35_prefill_bench` (:5161) — 27B prefill tok/s + serial-vs-pipelined decode (serial==pipe byte-exact).
- `mlx_qwen35_moe_decode_bench` (:5258) — MoE decode t/s; hosts `ROZUM_CTXSWEEP` KV-flatness sweep (:5295) and the per-token **build-vs-eval % split** (:5335, the Lever-3 quantifier).
- `mlx_hybrid_batched_decode_throughput` (:5798) — hybrid 27B B=2 batched vs 2×serial t/s + speedup (~1.98×).
- `mlx_batched_decode_probe` (:6038) — dense Qwen3 B>1 batched vs serial (byte-exact + throughput).
- `mlx_decode_pipeline_probe` (:6140) — serial vs `async_eval`-pipelined A/B.
- `mlx_compile_probe_plain` (:5542) — uncompiled vs plain `compile` decode-step ms (go/no-go for Lever 3).
- Scheduler integration tests assert "one `run_batch` call" for N concurrent requests
  (:6259/:6326/:6459 continuous-admit/…), i.e. that serving actually batches.

### Baseline RUN plan (when the slot frees)
1. `scripts/bench/run.sh` over the prod catalog (Qwen3.6-35B-A3B MoE, Qwen3.6-27B dense, gpt-oss-20b,
   GLM-4-32B, a small Qwen3-4B) → single-stream decode t/s + TTFT + peak RAM table.
2. `ROZUM_CTXSWEEP=1 … mlx_qwen35_moe_decode_bench` → confirm decode t/s stays flat across context
   (proves the pre-allocated KV layout; a downward slope would mean a regression).
3. The build-vs-eval split (:5335) → record the current `% CPU build` per model (the Lever-3 ceiling).
4. `mlx_hybrid_batched_decode_throughput` + `mlx_batched_decode_probe` → confirm the ~1.98× B=2 win
   the batching lever would unlock.
Record into `scripts/bench/results/perf-baseline-<date>/`. Follow the 🛑 REBOOT-SAFETY PROTOCOL
(claim the slot in-room; one model resident at a time).

## Already realized (baseline confirms; do NOT re-open)

- **Prefix-cache reuse (cross-turn KV) — DONE for the mainline serving path.** LRU `PrefixStore`
  (:826, 4 slots `ROZUM_PREFIX_CACHE_SLOTS`), longest-prefix `best_match` (:844), `run_job` truncates
  kept KV + prefills only the new suffix (:1130–1185), persists back (:1289). Default-ON
  (`ROZUM_PREFIX_CACHE`, only `=0` disables). Covers dense, gpt-oss (:1146), hybrid (:1160). Byte-exact
  tests `mlx_prefix_reuse_byte_exact{,_hybrid}` (:4638/:4696). *Open only on the opt-in fast-paths —
  see `perf-prefix-reuse-fastpaths`.*
- **KV cache layout — DONE.** `ConcatKeyValueCache` (`mlx-lm/src/cache.rs:80`) pre-allocates in
  `KV_STEP=256` blocks and writes **in place** (`update_and_fetch` :160) — the per-step O(context)
  copy is gone (one growth concat every 256 steps). *Verification-only:* `perf-kv-ctxsweep-verify`.
- **async_eval decode pipelining + retained command buffers — DONE.** Build step n+1 + `async_eval`
  before blocking on n (`rozum-core/src/engine.rs` `consume_tokens`; probe `mlx_decode_pipeline_probe`
  :6140). `ROZUM_MLX_RETAIN` drops ~48 syncs/token (~12→16–17 t/s on hybrid, :123).

## Open levers → tasks

### perf-batch-default-on — biggest near-term win, but NOT a free flip *(needs slot to measure)*
Continuous concurrent-request batching is **built and wired** (`worker_main` gathers ≤`cap` admitted
jobs in a `ROZUM_BATCH_WINDOW_MS` window → one `run_batch`/`run_batch_hybrid` forward, pulling new
jobs into freed slots mid-decode) but ships **off by default**: `batch_cap()` = `ROZUM_BATCH` default
**1 = serial** (:713). The throughput benches prove ~1.98× at B=2, and batched==serial **correctness is
already well-covered** (byte-exact ragged `mlx_batched_ragged_byte_exact` :5665 / `_hybrid` :5895; per-arch
`mlx_{,llama_,qwen2_,gemma3_}batched_two_concurrent`; `mlx_continuous_admit_three` :6459; sampling :6519).

**The blocker is latency, not correctness — and it's a real code gap, not a config flip.** With
`ROZUM_BATCH>1` on a batchable arch, the gather loop (`worker_main` :676-687) waits the **full**
`batch_window_ms()` (default 10) for a 2nd job; a **lone** request therefore eats up to 10 ms of TTFT
before the partition discovers it's alone and falls back to serial. For the common single-agent case
(one request in flight) that's a pure TTFT regression with zero benefit. So the task is **two-step**:
1. **`perf-batch-gather-shortcircuit` (prereq, code):** skip the window when no concurrency is
   present — e.g. one non-blocking `try_recv` after `first`; if Empty AND the admission in-flight count
   is ≤1, go straight to serial (no wait). Must still wait/batch when a 2nd job is admitted/queued, so
   re-run the `*_two_concurrent` + `continuous_admit_three` scheduler tests to prove batching still
   triggers and the lone path no longer waits. **Run those tests before flipping anything.**
2. **flip + A/B (slot):** with the lone-request tax gone, run `ROZUM_BATCH=1` vs `2`/`4` through the
   gateway under real concurrent agent load; confirm single-stream TTFT unchanged + the ~1.98×
   concurrent win + byte-exact, then flip the default (or keep opt-in if Metal-stream contention under
   mixed load erodes it). The primitives are proven; the gather short-circuit is the only new code.

### perf-batch-arch-coverage — extend batching to GLM-4 + gpt-oss *(needs slot)*
`is_batchable_arch` (:736) admits Qwen3/Qwen3Moe/Qwen35/Qwen35Moe/Llama/Qwen2/Gemma3 only — **Glm4 and
GptOss serialize** even when `ROZUM_BATCH>1`. Both are heavily used agentic models. Task: add their
per-row-rope/cache batch paths (GLM-4 is qwen2-shaped; gpt-oss needs sink/sliding-window batch
handling) with a byte-exact ragged test mirroring `mlx_batched_ragged_byte_exact` (:5665).

### perf-batch-nonbatchable-rows — stop serializing penalty/seed/constrained rows *(needs slot)*
`is_batchable` (:760) drops rep-penalty (logits need per-row history), explicit-seed (per-row RNG
keys) and constrained tool jobs (B=1 masked loop) to the serial path. Under agentic load these are
common (tool jobs especially). Task: per-row RNG keys + per-row penalty application in `run_batch`
(the `mlx_rope_per_row_probe` :5620 already shows per-row offsets are feasible), or quantify the loss
and document. Lower priority than default-on.

### perf-compiled-decode — mostly CLOSED (Stage-0 probe was NO-GO) *(deprioritized)*
Decode is **~92% CPU graph-build** (~400 op-build FFI calls/token; comment :5790), so a compiled
fixed-shape decode graph (token arg the only FFI crossing) was the obvious structural lever. **But it
was already probed and rejected:** commit `f6b20a3` (2026-06-22) ran `mlx_compile_probe_plain` on
Qwen3-0.6B-4bit → compiled step is **SLOWER** (T=1 0.69×, 26.6→38.4 ms; T=16 0.58×, 19.7→33.9 ms),
matching the earlier `compile_with_state` net-negative (:5531). Decision recorded there: **don't build
Stages 1/2.** Both the `compile` candidates are net-negative at decode scale; **batching is the lever
that actually paid off** (the hybrid-decode-gap finding). What remains is only the explicit *caveats*
from that commit — untested at 27B (vs 0.6B) and with a fixed-shape (non-growing) cache — which are
low-confidence and speculative. Treat as **deprioritized / on-ice**: re-open only if a 27B + fixed-shape
re-probe shows a different sign. Do NOT spend a slot-session re-running the 0.6B probe; it's answered.

### perf-prefix-reuse-fastpaths — bring cross-turn reuse to plookup + spec-decode *(needs slot)*
`run_plookup_job` and `run_spec_job` use **fresh KV** (forgone reuse — comments :655, :1735, :1831),
so on multi-turn agentic work they re-prefill the whole growing conversation each turn, eating the
plookup/spec-decode win. Task: thread the existing `PrefixStore` truncate/restore into both fast-paths
(they already reuse `MlxDenseTarget`, so the KV shape matches). Unlocks plookup for multi-turn agents,
not just single-shot copy-heavy generation.

### perf-kv-ctxsweep-verify — verification-only *(needs slot)*
Run `ROZUM_CTXSWEEP=1 mlx_qwen35_moe_decode_bench` and assert decode t/s is flat across context
(proves the pre-allocated KV layout has no O(context)/token regression). No code change expected.

## Priority (once the slot frees)
1. `perf-batch-gather-shortcircuit` (code, no slot to write; scheduler tests need the slot) → then
   `perf-batch-default-on` flip + A/B. Proven ~1.98× concurrent win; the short-circuit removes the
   lone-request 10 ms TTFT tax that otherwise blocks default-on. Biggest near-term win.
2. `perf-batch-arch-coverage` (GLM-4 + gpt-oss are hot agentic models that currently serialize).
3. `perf-prefix-reuse-fastpaths`, then `perf-batch-nonbatchable-rows`, then `perf-kv-ctxsweep-verify`.
4. `perf-compiled-decode` — **on-ice** (Stage-0 probe NO-GO, `f6b20a3`); re-open only for a 27B +
   fixed-shape re-probe, not the 0.6B path that's already answered.
