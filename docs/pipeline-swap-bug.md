# In-process GLM→Qwen swap bug — fresh-system handoff

## ✅ RESOLVED (2026-06-24, merry-tapir, fresh boot) — REAL MLX bug, fixed

Verdict (fresh boot, ample RAM, `REPS=3`): **real in-process-swap bug, NOT RAM degradation.**
`qwen_short 3/3, qwen_long 3/3` (Qwen handles long ALONE = RAM control passes) while
`swap_short 0/3, swap_long 0/3, lazy_pipeline 0/3`. After the fix: **all 5 cells 3/3.** It was
*not* prompt-length / prefill-cache (swap+short also failed) and **not** GLM-specific — **all four
`/control/switch` permutations failed (incl. Qwen→Qwen)**: any model loaded on a *new worker thread*
after another failed its first decode.

**Root cause (in MLX core, `mlx/backend/metal/device.cpp`):** the metal command-encoder map is
`static thread_local`, and a stream's encoder is registered (`gpu::new_stream` → `try_emplace`) **only
on the thread that created the stream**. rozum runs each model on its own worker thread and tears it
down on swap; the next model's fresh worker thread evaluates arrays still bound to the *previous*
thread's stream → `get_command_encoder` misses → throws **"There is no Stream(gpu, N) in current
thread."** (surfaced only after we replaced the swallowed bare `mlx: eval failed` with the real
exception — that opacity had cost a prior session).

**Fix:** an MLX-core **self-heal** patch — if `get_command_encoder` misses, *register* the encoder on
the current thread instead of throwing. Shipped as a 2nd chained CMake `PATCH_COMMAND` in the
`mlx-c` submodule of the vendored fork (`patches/mlx-stream-encoder-selfheal.patch`). General,
culprit-agnostic, fires at most once per stream per thread. (NOT the handoff's per-load-cap
hypothesis; NOT a teardown flush — that one was harmful and stays reverted.) Forks committed:
`sergey-scherbina/mlx-c @ cd329a6`, `sergey-scherbina/mlx-rs @ 7922c10a` (was `12fac5c0`).

The rest of this doc is the original pre-fix handoff, kept for context.

---

**One question to settle on a fresh boot:** is the in-process `GLM-9B → Qwen3-4B` swap failure a
**real MLX bug** or **session/RAM degradation**? Run `scripts/bench/pipeline-swap-repro.sh` right after
a reboot; its output answers it. This doc is the context so a fresh session nails it in one go.

## The exact symptom
After GLM-9B and Qwen3-4B are swapped **in one process**, a generation on the second model fails:
`HTTP 500` / the backend returns `mlx: eval failed` (the eval at `mlx_native_backend.rs` decode loop
returns `Err`). It is **not** a crash — the gateway stays up; only the request fails.

## What is and isn't true (measured, this session)
- **It's the swap path, not the pipeline orchestration.** The failure reproduces via the gateway's own
  `POST /control/switch` (load GLM → switch to Qwen → generate), **not just** the `LazyPipelineBackend`.
  ⇒ Routing the pipeline "through the Switchboard" (the old task #6 plan) would **not** fix it.
- **It's correlated with a LONG executor prompt.** `swap` + a **short** Qwen prompt works; `swap` + a
  **long** prompt fails. The planner→executor pipeline inherently feeds the executor a long
  (plan-appended) prompt, so this *is* the real-world pipeline case.
- **Qwen handles long prompts ALONE** — a 1449-word prompt to a single Qwen3-4B worked **when RAM was
  plentiful (~22 GiB free)**. The swap-long failures showed up only after the session had eaten RAM down
  to ~6 GiB free. **This is the whole reason for a fresh-boot re-test.**
- **Build/drop paths are identical** between the lazy pipeline and the Switchboard (both
  `build_from_config` + `MlxNativeBackend::Drop`).

## Ruled out as fixes (do not re-try blind)
Each tested live; none fixed it:
- **Teardown `mlx_synchronize` stream-flush — HARMFUL. Reverted in `1482dd7`. DO NOT RE-ADD.** It ran at
  every teardown and broke model-switching for *all* models (short prompts too). This was a self-inflicted
  regression that polluted several mid-session readings.
- MLX cache-evict (`set_cache_limit(0)`/restore), `reset_peak_memory`, settle-before-build (1.5 s),
  settle-after-build (2 s), inline-drop vs `spawn_blocking`, a separate tokio task per tier.

## The two live hypotheses → decision tree
Run the script on a fresh boot (ample free RAM). Read the result matrix:

| `swap_long` / `lazy_pipeline` on fresh system | conclusion | next step |
|---|---|---|
| **PASS** (≈N/N) | It was **session/RAM degradation**; the in-process pipeline works on a healthy host | No code fix. Optionally re-confirm by eating RAM and watching it start to fail. Ship the pipeline. |
| **FAIL** while `qwen_long` **PASS** | **Real in-process-swap bug** (Qwen handles long alone, but not after a swap) | Investigate a **per-load MLX memory/cache-limit RESET** in the worker load path (`build_from_config` → `worker_main`/`LoadedModel::load`) — the new model may be inheriting GLM's adaptive `set_memory_limit`/`set_cache_limit`, starving its larger prefill. **NOT a teardown flush.** Meanwhile `solve.sh` (separate processes) avoids it. |
| free RAM was **< 8 GiB** at the top | not a clean run | reboot, re-run |

## If it's a real bug — where to look
- `crates/rozum-mlx/src/mlx_native_backend.rs`: `worker_main` / `LoadedModel::load` — does each model load
  RESET the MLX memory/cache limits, or inherit the previous model's? See
  `[[reference-mlx-memory-cap-semantics]]`: `set_memory_limit` is soft, `set_cache_limit` bounds the cache.
  A model loaded after a larger one may keep a too-small cap for its prefill.
- The gateway's adaptive load (`adapt_n_ctx_to_fit` / cache-limit setting in `main.rs`) — confirm it's
  re-applied per load, not once at startup.

## Robust path regardless
`solve.sh` (planner→executor across **separate processes**) sidesteps all of this — each model gets a fresh
process + Metal context. Proven (RPN 3/3). Use it for GLM chains until/unless the above is resolved.
