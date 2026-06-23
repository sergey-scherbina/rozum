# Perf-baseline — current t/s + the open micro-perf levers

Status: prep done (sunny-civet, 2026-06-23); the measurement RUN is slot-gated (needs the host
model slot, held by the matrix). This is the analysis half of the sprint's `#3 Micro-perf →
perf-baseline` item: catalog what is already realized, what the existing tooling measures, and file
one `perf-<lever>` task per genuine opportunity — grounded in code so the run is just "invoke these."

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

### perf-batch-default-on — biggest near-term win, low risk *(needs slot to measure)*
Continuous concurrent-request batching is **built and wired** (`worker_main` gathers ≤`cap` admitted
jobs in a `ROZUM_BATCH_WINDOW_MS` window → one `run_batch`/`run_batch_hybrid` forward, pulling new
jobs into freed slots mid-decode) but ships **off by default**: `batch_cap()` = `ROZUM_BATCH` default
**1 = serial** (:713). The throughput benches prove ~1.98× at B=2. Task: run the baseline A/B
(`ROZUM_BATCH=1` vs `2`/`4`) through the gateway under real concurrent agent load, confirm byte-exact
+ no latency regression on the single-stream case, then either flip the default or document the exact
reason it stays opt-in (Metal-stream contention under mixed load). The proven primitives mean this is
validate-and-flip, not build.

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

### perf-compiled-decode — the structural ~92%-build lever *(higher risk/effort; needs slot)*
Decode is **~92% CPU graph-build** (~400 op-build FFI calls/token; comment :5790). async_eval + retain
are done; the remaining structural fix is a **compiled fixed-shape decode graph** so only the token arg
crosses FFI. `compile_with_state` is net-negative (re-marshals ~400 params/call, :5531); **plain
`compile`** is the candidate but lives only in the probe `mlx_compile_probe_plain` (:5542), not in
`run_job`/`Generate`. Task: use the probe as go/no-go; if positive, wire a fixed-shape KV + compiled
step into the decode loop (Stages 1–3 of the old `mlx-native-decode-gap-remainder` track). Biggest
single-stream ceiling; also the lever that most helps small/dense models where batching doesn't apply.

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
1. `perf-batch-default-on` (proven ~1.98×, low risk, validate-and-flip).
2. `perf-compiled-decode` go/no-go probe (structural; helps where batching can't).
3. `perf-batch-arch-coverage` (GLM-4 + gpt-oss are hot agentic models).
4. `perf-prefix-reuse-fastpaths`, then `perf-batch-nonbatchable-rows`, then `perf-kv-ctxsweep-verify`.
