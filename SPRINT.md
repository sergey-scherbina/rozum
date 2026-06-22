# Sprint

(Formerly `WORK_QUEUE.md`; renamed to `SPRINT.md` per `AGENTS.md` / the
multi-agent skill.)

Current sprint focus: (1) make Rozum a reliable local meeting room for live agents and a human operator; (2) make Rozum a local LLM provider for Claude Code and Codex via an outward OpenAI/Anthropic-compatible gateway backed by an in-process MLX / GGUF engine on Apple Silicon Metal.

## Sprint

### ★ Active program (user priority — do strictly in this order)

Do these in sequence: **1) green matrix → 2) plugin-ize everything → 3) micro-perf.**
Process: per the **multi-agent** skill — claim before work, worktree off `origin/master`,
push `feature/<slug>:master` (never edit master), then delete the done entry + prepend
`CHANGELOG.md`. Coordinate with the sibling on the workspace-split + codex/gpt-oss.

#### 0. Host safety — never let a 2nd model-load OOM-reboot the Mac (precondition for ALL matrix work)

- [x] **reboot-gateway-singleflight** (BUG-003) — **DONE.** Code on master `3bcee03`
  (`sunny-civet`); spec + board + independent verify (`nimble-raven`, room n=25–34). Spec:
  `docs/specs/gateway-residency-singleflight.md`; diagnosis: memory `project-reboot-watchdog-oom`.
  - **What/why:** 2026-06-22 13:41 the 36 GiB Mac **rebooted via kernel watchdog panic** (panic +
    3× JetsamEvent logs): **3 concurrent model-loaded `rozum` gateways ≈61.6 GB** (two matrix runs
    at once) → `vm-compressor-space-shortage` jetsam cascade → `watchdogd` starved 92 s → panic.
    NOT the GPU double-free of `project-matrix-kernel-panic`. A dedicated `rozum gateway --port N`
    (what agentic.sh starts) bypassed the shared-gateway port singleton, so nothing stopped the 2nd.
  - **Fix:** host-wide `flock` gate `share::acquire_residency()` wired into BOTH load sites —
    `run_gateway` (matrix path) + `run_launch_dedicated` — each binds `_residency` (held for the
    process lifetime) BEFORE the model loads. 2nd concurrent loader waits
    `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` (240) then `exit(1)` naming the holder (reboot → recoverable
    error). flock auto-releases on process death (no stale-lock). Escape hatch
    `ROZUM_ALLOW_CONCURRENT_RESIDENT=1`; fail-open on lockfile error.
  - **Verified (nimble-raven, no model loaded):** 2 unit tests green on clean rebuild — admit-one +
    *deny-second-while-held* (flock contends across opens) + release-on-drop + escape-hatch; code
    read confirms guard held (`_residency`, not `_`) before load on both paths; matrix
    `rozum gateway --port N`→run_gateway covered, child gateways transitively, `ensure_shared_gateway`
    reuse correctly NOT gated. Plus sunny-civet's real-binary smoke (held→refuse+exit1 before load).
  - **Operational rule still holds:** don't deliberately run 2 model-gateways without checking budget.
    **v2 RAM-ledger DONE** (`feature/gateway-residency-ram-ledger`, sunny-civet): the gate now admits a
    genuinely-fitting small 2nd model (reserve footprint up-front under an admit lock; per-pid flock
    liveness — answers the v1 racy/PID-reap objection) and refuses only a true overcommit. Remaining
    v3 (BACKLOG): sibling-aware `cap_mlx_memory` backstop.

> ### 🛑 REBOOT-SAFETY PROTOCOL (operator directive 2026-06-22 — load-bearing, read before any model load)
> The 36 GiB Mac reboots if >1 model is resident (BUG-003). The gate now refuses a 2nd load, but
> **do not rely on it as a license to be careless** — treat "one model-loaded gateway on this host
> at a time" as the hard rule. Before running ANY command that loads a model (`rozum launch`,
> `rozum gateway`, `agentic.sh`, any matrix/probe):
> 1. **Claim the model slot in the rozum room first** ("working: taking model slot for <what>") and
>    check nobody else holds it. Release it ("done: slot free") when finished.
> 2. **Check the host is clear:** `ps -axo pid,rss,command | grep '[r]ozum gateway'` shows no live
>    model gateway, and `lsof <gateway_dir>/residency.lock` is unheld. (`mcp-proxy` + `meetings` are
>    fine — they load no model.)
> 3. **Prefer the smallest model** that proves the point; for fast gate checks use
>    `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS=0`.
> 4. **Never start a 2nd matrix/launch while one is running.** If unsure, ask in the room.
> Everything you do goes on this board *before* you run it, so a reboot costs minutes, not work.

- [ ] **validate-gate-live** (P0, owner `nimble-raven`) — capstone the BUG-003 fix: prove the box
  *survives* a concurrent-load attempt under the REAL binary on master, not just unit tests.
  - **How (single model load only — safe by construction):** build `rozum` from master (≥`3bcee03`),
    start gateway A on a TINY model (smallest in catalog), then attempt gateway B with
    `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS=0` → assert B prints the holder-named refusal + `exit 1`
    **before** any RSS spike (B never loads). Then stop A → B succeeds. Only ONE model ever resident.
  - **Done-when:** B refuses+exits before load (capture stderr + exit code + `ps` RSS showing no 2nd
    model); A unaffected; A-stop frees the gate. Record result here + CHANGELOG.
  - **Note:** sunny-civet already did a real-binary smoke (held→refuse+exit1); this is the
    end-to-end confirmation through an actual model-loaded gateway A. Coordinate the slot (above).

> #### ▶ nimble-raven active tracks (2026-06-22, operator: "do all three, set priorities, don't reboot, write it all down")
> My priority order across the operator's three asks, scheduled around the **single model slot**:
> 1. **validate-gate-live** (P0, above) — cheap safety-capstone; needs the slot briefly. Coordinate
>    with `plucky-finch` (queued for the slot for the do-first nondeterminism probe) — ideally
>    piggyback: while their model gateway is up, my gateway-B attempt IS the live concurrency proof.
> 2. **matrix-baseline** (P1 — green matrix #1 below) — the big authoritative run. **PREP DONE
>    (2026-06-22, see the item below): push-button + verified reboot-safe.** Just needs the slot,
>    AFTER the nondeterminism probe.
> 3. **plugin track** (P2 — plugin-ize #2 below) — **PARKED after triage (2026-06-22): no clean,
>    safe, no-model increment exists right now** (verdict recorded under "#### 2" below). So my
>    no-model default work is now **matrix-baseline prep** (done) + supporting `plucky-finch`'s probe.
> Rule: slot-tasks (1,2) serialize behind the room claim; no-slot work runs in parallel.

##### ▶▶ safe-multi-model-residency (operator vision 2026-06-22: "несколько моделей одновременно ИЛИ очень быстрый swap — но ГЛАВНОЕ всё безопасно")
Spec: `docs/specs/safe-multi-model-residency.md`. The North-Star residency goal, safety-first.
Owners (room n=25–43): admission *mechanism* = `sunny-civet` (v2 RAM-ledger, on master `644e8e8`);
admission *numbers* + per-process cap + validation = `nimble-raven`.

> **⚠️ LIVE SAFETY FINDING (nimble-raven, data-backed): v2 as shipped can still reboot.** v2 admits a
> 2nd model by `estimate = size_bytes × 1.25 + 1 GB`, but measured peak resident is **cache-dominated,
> not weight-proportional**: Qwen3-4B (weights ~2.5 GB) peaked at **26.9 GB**; Qwen2.5-0.5B at 14.9 GB
> (MLX caches to the per-process cap `total−8 ≈ 28 GB`). So the estimate under-counts small models ~6×;
> two 4B models pass admission (4+4 ≤ 0.65×36=23 GB) but really need ~54 GB → **reboot**. Fix is NOT a
> bigger inflate (balloon is additive/cache-driven). The fix = the share-bounded cap (A) — it turns the
> reservation into an *enforced* ceiling, the only thing that makes safe co-residency possible on 36 GiB.

- [x] **smmr-A-soft-hint** (`nimble-raven`) — **DONE `95b98d6` + CORRECTED.** Each gateway process sets its
  own MLX `set_memory_limit` = its reservation before the worker loads. **CORRECTION** (source audit, re-
  verified vs pinned fork `12fac5c` memory.rs:64): `set_memory_limit` is **SOFT** (evicts cache/waits, then
  allocates anyway) — NOT a hard cap. So A is **defense-in-depth**, not enforcement; the structural lever is
  conservative ADMISSION (v2 ledger) + `set_cache_limit` (the real cache bound). `set_memory_cap_bytes` +
  pure `select_mlx_mem_limit_bytes` (2 tests); docs relabeled "soft hint". MLX path only (gguf/mistralrs:
  admission-only). cap default+--no-default green.
- [x] **smmr-B-footprint** (`nimble-raven`) — **DONE `d7fd456` + CORRECTED.** `runtime_footprint_bytes` =
  weights + `kv_bytes_per_position(config)·n_ctx` + activation reserve. Since admission (not a hard cap) is
  the lever, the footprint MUST be ≥ real resident peak = active + bounded cache; the reserve is now
  `max(6 GiB, weights/4)` ≈ ~4 GiB cache (`set_cache_limit`) + ~2 GiB prefill (was a 3 GiB catch-all <
  the cache limit alone — the audit's flagged bug). 4 unit tests updated. Interim 14 GB floor dropped.
  **Open (smmr-D):** truly-≥-peak hinges on the active-vs-cache split — measure live.
- [~] **smmr-C-fast-swap** (`sunny-civet`) — **FOUNDATION DONE `c3fa8ef`.** `rozum-core::prefetch::
  warm_dir_page_cache` (page-cache prewarm, reclaimable + not GPU-residency → budget-free, cancellable,
  3 tests) + spec § C fleshed out (swap invariant: never both GPU-resident; drain→free→settle→load;
  prewarm-during-drain). Argues C > A/B in value (27–35B can't co-reside on 36 GiB by need → common case
  is swap). REMAINING (slot-gated, touches `gateway.rs`, after D): wire prewarm + ordered swap into
  `gateway switch`; measure swap latency with/without prewarm.
- [x] **smmr-D-probe-harness** (`sunny-civet`) — **DONE `4c2329b`.** `crates/rozum-mlx/examples/mlx_mem_probe.rs`.
  Raw-alloc mode (slot-free) EMPIRICALLY confirmed `set_memory_limit` is SOFT (limit 512 MB, allocated
  1024 MB live → active 1024 MB) and `set_cache_limit` bounds the cache (256 MB retained after drop).
  Model mode (header-documented) is nimble-raven's D measurement: split a real model's peak into
  active vs cache under the slot. Spec § Findings updated with the measurement.
- [x] **smmr-D-validate** (`nimble-raven`) — **DONE 2026-06-22 (crux settled).** ✅ raw-alloc probe:
  `set_memory_limit` SOFT (active 2048MB > 512MB), `set_cache_limit` = real cache bound. ✅ admission gate
  validated live (sibling 35B resident → 4B load REFUSED, no reboot). ✅ **model-mode (Qwen3-4B, exclusive
  slot):** real resident ~2.3 GB (active 2267MB, peak 2310MB, cache 255MB ≪ 4 GB limit, RSS 2.32 GB) —
  the 26.9 GB does NOT reproduce under `set_cache_limit` (it was uncapped-cache, old runs). **VERDICT:
  resident = active(weights+KV, bounded by n_ctx) + cache(bounded by set_cache_limit) → co-residency SAFE
  by construction.** Caveat: big-model full-prefill peak not directly measured (structural bound + chunked
  prefill cover it). Spec § D Progress.
- [x] **footprint-overconservative** (`nimble-raven`) — **DONE `f430c1d`.** Two data-justified fixes so
  small models co-reside while keeping a big REAL margin: (1) `activation_reserve` tied to real bounds =
  `set_cache_limit + 1.5 GiB prefill` (≈5.5 GiB) not arbitrary `max(6 GiB, weights/4)`; (2)
  `RAM_BUDGET_FRAC` 0.65→0.75. Now two ~4B pass admission (25.2 ≤ 27) and really use ~12–20 GiB (≥16 GiB
  free). Big models still sole. rozum-models 7/7 + core 96/96 green. Live two-model validation = smmr-D when slot free.

##### ▶▶ PROGRAM: max safety × capability × performance × flexibility (operator 2026-06-22)
Spec `docs/specs/safe-multi-model-program.md` — obstacle register + the path. Core insight: today's
safety is estimate-based (open-loop admission); the GUARANTEE needs measured closed-loop control.
- [~] **govern-core** (`nimble-raven`) — **CORE DONE `2b510cb`.** `rozum-core::govern`: pure
  `classify(free,total,thresholds)→Pressure{Green/Yellow/Red}` (on FREE RAM — catches over-spend from
  ANY source admission can't see; unknown total→Red) + `action_for→{Admit,HoldAdmissions,EvictOne}`;
  env thresholds (`ROZUM_GOV_YELLOW_FRAC` 0.20 / `_RED_FRAC` 0.10). 4 tests. **NEXT (govern-loop):** a
  gateway task that samples `vm_stat` free + `mlx get_active/get_cache` on an interval, logs the band,
  and on Red evicts the lowest-utility idle model via the Switchboard — the measured backstop that makes
  pushing capability/perf safe. Coordinate with `sunny-civet` (eviction = Switchboard/ledger domain).
- [ ] **govern-os-containment** — set gateway jetsam priority / RLIMIT so a breach is a recoverable
  process-kill, not a kernel panic. Cheap, large safety upside.
- [ ] **residency-unify-in-process** — fold co-residency into one in-process Switchboard (N models, exact
  accounting, instant swap) instead of N flock-coordinated processes; flock ledger stays the cross-process
  backstop. (Big; design-gated; coordinate.)
- [ ] **footprint-before-download** (`nimble-raven`, found via smmr-D 2026-06-22) — `estimate_model_footprint_bytes`
  runs BEFORE `ensure_model_dir`, so an **un-cached** model scans empty → unknown-sentinel (`u64::MAX/4`) →
  reserves ~4.4e12 MB for its whole life, **blocking every other model load** (a tiny uncached 4B blocked a
  sibling's gpt-oss matrix on :8301). Safe (over-reserves) but badly over-conservative. **Fix:** compute the
  footprint AFTER the model dir is resolved/downloaded (size known) — i.e. move/repeat the
  reserve+cap after `ensure_model_dir` in `run_gateway`/`run_launch_dedicated`. Touches the admission call
  site (sunny-civet's ledger + my estimate) → **coordinate before editing.** Done-when: an uncached small
  model reserves its real size, not the sentinel; doesn't block siblings; cargo check + tests green.
- [x] **residency-ledger-hardening** (`sunny-civet`) — **DONE `f5cbec2`.** Ledger was already flock-robust;
  added `reap_orphan_residents()` (reusable reaper for a doctor/maintenance path) + edge tests (PID-reuse
  overwrite, dead-vs-live reap, non-pid/held files untouched). Additive to `share.rs`, no API change → did
  not disturb smmr-A/B. Core 93/93. Deferred (riskier, touches `gateway.rs`): release-on-idle-unload.

> #### ▶ sunny-civet active tracks (2026-06-22, operator: "запиши всё в спринт и сделай автономно")
> Discipline: slot-free + non-colliding by default (room MCP down → coordinate via `rozum meetings post`;
> nimble-raven owns rozum-mlx cap / rozum-models footprint / the model slot — I add NEW files, don't edit
> their code, don't grab the slot). Order:
> 1. ✅ **smmr-D-probe-harness** DONE (`4c2329b`) — set_memory_limit empirically SOFT; harness for D.
> 2. ✅ **residency-ledger-hardening** DONE (`f5cbec2`).
> 3. [~] **smmr-C-fast-swap** foundation DONE (`c3fa8ef`); orchestration slot-gated (after D).
> 4. ⏳ NEXT (larger, fresh tracks): **decode-compile-stage0** (perf P3, slot-free probe), **plugin-x86-engine**
>    (P2 North-Star). Not rushed at the tail of this session — kept queued so they get a proper start.
> Outcome: my source-audit finding (`9726971`) was accepted — nimble-raven reconciled A/B (`e7c9762`,
> "admission is the lever"). Slot-needing validation (probe model-mode, swap e2e) handed to the slot owner.

- **INTERIM SAFETY:** the interim floor is dropped; smmr-A caps each process. CAVEAT (`sunny-civet`,
  `9726971`): the cap is via `set_memory_limit` which is SOFT — see spec § Findings; co-residency safety
  is not yet *proven* (hinges on smmr-D's active-vs-cache measurement), so treat default co-residency as
  provisional until D lands.

#### 1. Green matrix — NO fails allowed, ever

The matrix must be **all green**. Every fail — and every non-deterministic flip — is a
concrete bug **in our stack**, to isolate **fully and from all sides (the `isolate` skill)
BEFORE any conclusion**. Never close a cell as "model too weak / model-side" — that has been
premature and wrong before (codex×gpt-oss was *our* gateway bug twice). One task per fail.

- [x] **matrix-baseline** — DONE (plucky-finch 2026-06-22, seed-pinned, release@master,
  reboot-safe single-box, **0 panic/0 reboot/0 rc2**). claude+codex × {Qwen3.6-35B-A3B,
  gpt-oss-20b} × 5 tasks = **16/20 in this single run** (claude 10/10, codex 6/10). NOTE: a
  single run is one sample per cell, NOT a verdict — agent-layer non-determinism (codex injects a
  per-run session-id) means cells are pass-RATES (see `matrix-nondeterminism-flip`). Indeed one
  red, `matrix-35b-codex-test`, was an ~80%-pass FLAKE (4/4 on re-isolation) — already resolved.
  Remaining reds filed as `matrix-*` tasks below; confirm each over N runs before treating as a bug.
  Results: `scripts/bench/results/baseline-postfix/per-run.csv`.
  (Not yet run: opencode column; the other models — file more tasks when run.)
  - **PREP DONE (nimble-raven 2026-06-22):** runner is `scripts/bench/agentic.sh`; override via
    `AGENTIC_MODELS="spec1 spec2" AGENTS="claude codex" TASKS="greet build fix test debug"`
    (defaults: `Qwen3.6-35B-A3B-4bit` + `gpt-oss-20b-MXFP4-Q4`; gold standard Qwen3.6-35B = 15/15).
    It starts **one shared gateway per model, serially** (`gateway --port PORT_BASE+idx`), runs all
    agents×tasks, then SIGINT-drains + waits ≤`TEARDOWN_GRACE`(180s) for full process exit +
    `GPU_SETTLE`(8s) before the next model.
  - **REBOOT-SAFE — verified against the BUG-003 gate:** a *single* matrix never has >1 model
    resident (serial loop), and the old gateway's process fully exits (flock released on fd-close)
    before the next starts, so the gate's 240s wait is never approached → **no false-refuse within a
    run**. A *concurrent 2nd* matrix degrades gracefully: its gateway waits 240s then `exit(1)` →
    agentic.sh's "gateway not ready" path skips that model — it can't reboot the box. `agentic.sh`
    also `gateway stop --force`s any stale shared gateway first (frees a leftover flock).
  - **NOW SEED-PINNED (plucky-finch 2026-06-22):** `agentic.sh` exports
    `ROZUM_SAMPLING_SEED=${ROZUM_SAMPLING_SEED-1234}` → the baseline is reproducible by default
    (see `matrix-nondeterminism-flip`), so a red is reproducible+debuggable, not sampling noise.
  - **Before running:** follow the 🛑 REBOOT-SAFETY PROTOCOL (claim the slot in-room; `ps`/lockfile
    check that no OTHER model gateway — e.g. a dedicated one from another worktree — is live).
- [x] **matrix-nondeterminism-flip** — DONE (E1 fixed) + HONEST two-layer finding. Root cause
  has TWO layers: **E1 (our bug — FIXED):** the gateway never threads `SamplingParams.seed`, so
  the sampler + MLX RNG seed from entropy — any `temperature>0` request (Claude = 1.0) yields a
  different token stream per run. Proven on GLM-4-9B: temp1 unseeded **5/5 distinct** → temp1 +
  `ROZUM_SAMPLING_SEED=42` **1/5 (fixed)**. Shipped (default-OFF): `apply_determinism_env`
  (`ROZUM_SAMPLING_SEED`+`ROZUM_FORCE_GREEDY`, 3 tests) + `agentic.sh` seed-pin + probe.
  **CORRECTION (was "validated end-to-end" — that was premature):** the seed does NOT make the
  matrix fully deterministic, because of **Layer-A (agent's own, irreducible by us):** the agent
  CLIs inject a fresh per-run `session-id`+timestamp into every request (proven from codex's log),
  so the request is non-identical across runs → trajectory varies even at a fixed seed. Demonstrated:
  `codex×35B×test` flipped FAIL(baseline)→PASS(repro) at the SAME seed=1234 (see
  `matrix-35b-codex-test`). **Conclusion: the seed removes OUR sampling noise (E1); agent-layer
  noise remains, so the agentic matrix must be read as an N-run PASS-RATE, not a single binary
  cell** (echoes the GLM "5-task matrix too noisy" meta-finding). Spec
  `docs/specs/matrix-nondeterminism.md` (to update). Optional: OpenAI `seed` request-field parse.
- [ ] **matrix-glm32-agentic** — GLM-4-32B emits file *content* (```toml/```rust) or raw
  ```shell instead of *naming* Write/shell. Isolate WHERE in **our** stack it's lost
  (prompt render, tool-schema presentation, chat-template). Do NOT close as a model
  decision-gap until a clean-prompt model-only probe proves the model itself can't.
  Filed from the `matrix-baseline` fail map (one task per red, all codex). Repro any cell with
  (under the 🛑 REBOOT-SAFETY PROTOCOL — claim the slot, one model at a time):
  `BENCH_BIN=<release> AGENTS=codex AGENTIC_MODELS="<spec>" TASKS=<task> KEEP=1 RUN_TIMEOUT=300 scripts/bench/agentic.sh`
  (seed is pinned to 1234 by the harness ⇒ reproducible). `verify_task` scores by **final file
  state**, not rc. Do NOT close any as "model too weak / model-side" until isolated from all sides.

- [x] **matrix-35b-codex-test** — RESOLVED, **not a structural fail — a Layer-A flake** (isolated
  plucky-finch 2026-06-22). The baseline `codex×35B×test` FAIL (24.7s, rc=0, anomalously fast) did
  NOT reproduce: re-run at the same seed=1234 PASSed (67.6s), and a 3× characterization on a fresh
  gateway was **3/3 PASS** (47-49s) — so **4/4 PASS in fresh isolation, 1 FAIL only in the baseline**
  (where the cell ran on a gateway warmed by 8 prior cells). 35B is fully capable here. The fail is
  **agent-layer non-determinism**: codex emits a fresh `session id` per run (019ef0df…/019ef0e0…/…
  observed) → non-identical request → occasional bad trajectory the seed can't pin (≈80% pass).
  Lesson (feeds `matrix-nondeterminism-flip`): a single-run matrix red is NOT a bug until confirmed
  over N runs; this one dissolved under isolation. NOT a codex-delivery bug, NOT a model-weakness.
- [ ] **matrix-gptoss-codex-build** — `codex × gpt-oss-20b × build` = **fail** (196.7s, rc=0).
  Create-from-scratch via codex's V4A `apply_patch`. Known family ([[project-gateway-patch-revert]],
  Finding 5/6): gpt-oss wrestles codex's 21KB/18-tool V4A load → malformed/duplicate writes →
  files don't land. Verify against the latest gateway create-write-synth + codex-lean; isolate any
  residual delivery loss vs. a true model ceiling. Coordinate — a sibling holds the codex/gpt-oss branches.
- [ ] **matrix-gptoss-codex-test** — `codex × gpt-oss-20b × test` = **fail** (300.1s, rc=143,
  **RUN_TIMEOUT**). Same create-from-scratch family as `-build` but it TIMED OUT (the model
  flailed past 300s). Isolate: is it the same V4A delivery loop, or does gpt-oss never converge on
  the test scaffold within budget? Consider whether a higher `RUN_TIMEOUT` or the create-write-synth
  changes it. Coordinate with the sibling on codex/gpt-oss.
- [ ] **matrix-gptoss-codex-debug** — `codex × gpt-oss-20b × debug` = **fail** (55.9s, rc=0).
  Edit-existing-file (fix the bug so `cargo test` passes). Failed fast (didn't time out) → the edit
  never landed or was wrong. Likely the codex apply_patch edit-delivery path on gpt-oss; cross-check
  against the five shipped gateway delivery fixes ([[project-gateway-patch-revert]]). Coordinate with the sibling.

#### 2. Plugin-ize everything

> **TRIAGE (nimble-raven 2026-06-22): none of the three is a clean, safe, no-model increment to
> start *now* — each needs an operator decision first. Recorded so the next agent doesn't re-derive.**
> - **wireprotocol** — HIGHEST risk: it rewrites the matrix-critical request/SSE path in
>   `crates/rozum-gateway/src/gateway.rs`, and arch-spi Stage 3 already found the trait **net-negative**
>   (three genuinely different typed extractors `Json<OaiChatReq>`/`Json<RespReq>`/`Json<AnthropicReq>`
>   → three SSE sequences; a trait either erases axum's compile-time validation = behaviour change, or
>   is a fat indirection that deletes no complexity). Nothing in the code changed since to flip that.
>   Can't be fully de-risked without loading a model. → **re-scope** (unify only internal
>   `ChatRequest`/`ChatEvent` + cross-cutting policy, leave extractors per-route) and **re-decide** first.
> - **services** — explicitly **"out of scope (decided)"** in arch-spi ("subcommands stay"); it also
>   touches the reboot-sensitive `run_gateway` dispatch (`src/main.rs:~923`). → needs an operator override.
> - **x86-engine** — **already structurally a plugin**: `X86NativeBackend impl ChatBackend` + `X86Engine
>   impl LocalEngine`, selectable via the forced build chain (`main.rs:~4213`, symmetric with MLX's
>   direct `try_build_mlx_native_backend` — NOT the GGUF registry-IoC path, which exists only for the
>   workspace-split core→gguf dep break). The only real remaining work is the **Vulkan kernels**, which
>   can't be built/tested on Apple Silicon. Mirroring GGUF's `BackendEngine`/`OnceLock` registry IoC
>   would add an asymmetric abstraction (MLX/mistralrs don't use it either), not plugin-ize. → defer to
>   real x86 hardware.

- [ ] **plugin-wireprotocol** — make the agent wire layer a real `WireProtocol` trait
  (Chat / Messages / Responses impls). Supersedes the arch-spi "map, not trait" call —
  full plugin-ization is the goal. **(See TRIAGE above — re-scope + re-decide before starting.)**
- [ ] **plugin-services** — services (gateway / web / meetings / bridges) behind a plugin
  registry instead of `Command` match arms. **(See TRIAGE — decided out-of-scope; needs override.)**
- [ ] **plugin-x86-engine** — the reserved `rozum-x86` engine slot → a real engine plugin
  behind `LocalEngine` / `ChatBackend` (the North-Star multi-device frontier).
  (Already plugin-ized: `ChatBackend`, `ToolSource` + MCP client, `ToolDialect`.)
  **(See TRIAGE — already structurally a plugin; remaining work is Vulkan kernels → needs x86 HW.)**

#### 3. Micro-perf

- [ ] **perf-baseline** — measure current single-stream + batched t/s per model on
  `origin/master`; file one `perf-<lever>` task per opportunity (prefix-cache reuse,
  further batching, FFI/CPU-build-cost per the hybrid-decode findings, KV layout).

### Workspace split — monolith → layered Cargo workspace (spec-gated, `docs/specs/workspace-split.md`)

Goal: decompose the single ~47K-LOC `rozum` crate into a **Cargo workspace of ~7–8
crates** along the dependency layers the code already has (core SPI → models/hardware/
engines → agent/gateway → meeting → bin), so each concern is its own crate with a
**compiler-enforced** boundary. One repo, one workspace (NOT separate repos). The
crate-level successor to the architecture-spi work below. **Behaviour-preserving and
green at every phase** — same binary, same features, same matrix. Decisions + the
dependency-graph evidence (prod graph is already a clean DAG; the wrong-way edges are
test-only) live in the spec.

- [x] **Workspace scaffold up** — root is now `[workspace] members = ["crates/*"]` (landed
  with Phase 1, since `rozum-meeting` has 0 internal deps and didn't need `rozum-core` first).
- [x] **Phase 0 — `rozum-core`** ✅. Moved the SPI cluster (`backend`/`concurrency`/`obs`/`engine`/
  `serving`/`sampler`/`constrain`/`harmony`) into `crates/rozum-core`; module names preserved +
  re-exported from `src/lib.rs` (`pub use rozum_core::{…}` + `pub(crate) use rozum_core::backend`)
  → consumers unchanged. Knot 1 handled (the `backend↔concurrency↔obs` trio stays co-located in
  core). **Knot — `backend→gguf` broken via inversion of control** (`25ea416`): the SPI registry no
  longer constructs the gguf engine; `backend::register_gguf_engine()` is a hook the binary fills at
  startup (`gguf::register_engine()` in `main()`), so core depends on **no** engine. `local-models`
  (the candle `CandleBackend`) now lives in `rozum-core` and the bin forwards its feature. **Deferred:**
  `config` stays in the bin (its `config→cascade` knot 2 is resolved when `cascade` moves in Phase 3).
  Verified: `cargo check` default **and** `--no-default-features` green; **82/82** core tests + **300/0**
  root lib tests pass (incl. orchestrator/gguf-registration path).
- [x] **Phase 1 — Extract `rozum-meeting`** ✅ (proof-of-concept; daemon has **0 internal deps**).
  meeting + clients (tui/web/discord/telegram) → `crates/rozum-meeting`; module names preserved
  so internal `crate::…` paths resolve unchanged + the `rozum` lib re-exports them under their
  original paths (near-zero churn in consumers). `cargo check` default **and**
  `--no-default-features` green; **71/71** crate tests pass; **no engine crate in its tree**
  (an engine edit no longer recompiles the daemon — the isolation win). `proxy`/`service` stayed
  in the bin (they need `concurrency`/`share` from `rozum-core`); they fold into `rozum-meeting`
  once Phase 0 lands.
- [x] **Phase 2 — `rozum-models` + engines** ✅ (`rozum-mlx`/`-gguf`/`-mistralrs`/`-x86`). `rozum-models`
  (catalog/sourcing/residency) extracted as a pure leaf (`c2f0bfa`). Each engine is its own crate with
  the **heavy dep gated behind the crate's own feature, forwarded from the bin** (`mlx-native =
  ["rozum-mlx/mlx-native"]`, etc. — "Approach B": engine crate always linked, heavy backend optional):
  `rozum-mistralrs` (`55c9347`), `rozum-x86` (`4aed2b5`), `rozum-gguf` (`cb4722b`, self-registers via the
  IoC hook), `rozum-mlx` (`96daf50`, + specdecode, + a `backend::*` glob since mlx uses SPI types at the
  crate root). Module names preserved + re-exported from `src/lib.rs` → consumers unchanged (the
  gateway→mlx telemetry refs resolve via mlx's always-compiled `mlx_memory_mb`/`batch_stats` fallbacks).
  Verified: `cargo check` default (mlx+gguf on, MLX C++ built) **and** `--no-default-features` green, plus
  `--features mistralrs`/`x86-native`; crate tests pass. **The isolation win is now real for engines:
  editing one engine recompiles only that crate, not the others or the daemon.**
- [x] **Phase 3 — `rozum-agent` + `rozum-gateway`** ✅. `share` moved to `rozum-core` (leaf util used
  by both + the bin). `rozum-agent` (agent/cascade/router/rag_lite/builtin_tools/memory_store/
  mcp_tool_source) — the intelligence layer, depends on core+models but **no engine** (118/118 tests).
  `rozum-gateway` (gateway/openai_http/anthropic_http) — the serving layer, **no engine** (71/71 tests).
  **Knot 3 fixed:** the gateway's MLX-telemetry reads are now a `rozum-core::obs` registration hook
  (`register_mlx_memory`/`register_mlx_batch_stats` + `BatchStats`); `rozum-mlx::register_telemetry()`
  fills it in `main()`, the gateway reads via `crate::obs` → it no longer references the engine.
  **Knot 4 fixed:** the 3 real-MLX `#[ignore]` evals (agent/router/rag) relocated to the bin's
  `tests/mlx_evals.rs` (the only place agent-loop legitimately meets a concrete engine), so rozum-agent
  has zero mlx reference. **Knot 2 dissolved:** `config` stays in the bin (top), so `config→cascade`
  is bin→agent — no inversion, no type move needed. (`POISON_ENV_LOCK` made `pub`+non-cfg(test) so the
  bin's proxy tests reach it cross-crate.) Verified: `cargo check` default (mlx on) + `--no-default-features`
  green; all test targets compile under default features. **The bin is now just the launch/CLI layer**
  (main/config/doctor/proxy/sandbox/service).
- [ ] **Phase 4 — `rozum-hardware`** (device detect + placement; North Star). Separate spec —
  reserved as a crate slot here, designed later (it is new work, not a move).

### Architecture legibility — SPI boundaries (spec-gated, `docs/specs/architecture-spi.md`)

Goal: make rozum's extension points legible — a reader finds the seam for any
concern (model / tool / agent / service) in one hop. NOT "plugin-ize everything":
two axes are already SPIs, one tangle is worth extracting, services stay as-is.
Each step is **behaviour-preserving** and **matrix-gated**. **Stages 1–3 DONE +
merged** (the legibility goal is met). Spec `2edbf00`; outcome in
`docs/specs/architecture-spi.md` Results.

- [x] **Stage 1 — Document** (merged `2b0a135`). `SPEC.md` "Extension points" names
  the four axes (`ChatBackend`/`ToolSource` SPIs ✓, the two seams, services).
- [x] **Stage 2 — Extract `ToolDialect`** (merged `023c121`). `dialect_for(template)`
  → `Qwen`/`Harmony`/`Glm`; render + constraint-envelope flag flow through it.
  Behaviour-preserving: 448/0 lib tests + Qwen3.6-35B claude×fix smoke pass=1.
  **Scope correction:** parse stayed a generic union (`parse_tool_calls`), not
  per-family — the dialect owns only what varies (render + envelope).
- [x] **Stage 3 — `WireProtocol`: MAPPED, trait rejected** (merged `2fc0ed9`).
  Investigated; the gateway wire layer is *already factored* (named per-dialect
  parse + serialize fns + thin handlers converging on `ChatRequest`/`ChatEvent`).
  A trait would force uniformity over different typed extractors + SSE sequences →
  net-negative on matrix-critical code. Fixed the stale module doc ("two dialects" →
  three) into an accurate wire-protocol **map**. Docs-only.
- [x] **Stage 4 — MCP `ToolSource` adapter** (merged). `McpToolSource`
  (`src/mcp_tool_source.rs`): rmcp 1.7 client over a stdio child process; tools cached
  at connect, `dispatch`→`call_tool` with `is_error`/transport failures → recoverable
  `ToolError`; composes with in-process tools via `MultiToolSource`. **5/0 tests** via an
  in-memory duplex against a minimal in-process rmcp server (`echo`+`boom`) — no external
  binary. Spec `docs/specs/mcp-toolsource.md`. (Surfacing it in an embedded-agent command
  that *configures* MCP servers is a separate later step — out of scope here.)
- [ ] **Out of scope (decided):** services-as-plugins (subcommands stay); dynamic/
  loadable plugins (dylib/WASM/out-of-process) — in-tree trait impls only;
  re-abstracting `ChatBackend`/`ToolSource` (already correct).

### Top priority (P0): mistralrs Qwen3.6 finish-the-forward — RESOLVED (day 6)

**Root cause found and fixed.** The residual divergence was NOT the
weight-row-ordering hypothesis from days 1-5. It was the **RMSNorm `+1`
convention**: `GemmaRmsNorm::new` bakes `weight = on_disk_weight + 1.0`, but
the sanitized `mlx-community/Qwen3.6-35B-A3B-4bit` checkpoint uses raw
RMSNorm weights (MLX's `should_shift_norm_weights` is False for sanitized
checkpoints: conv1d `(8192,4,1)`, no MTP). The `+1` over-scaled every norm
(~2.1x) and the silu MoE compounded it to a ~14x experts blow-up + an
over-peaked router. Fix: `GemmaRmsNorm::new_unshifted` for the five norm
sites in `vision_models/qwen3_5_moe/text.rs`.

Full writeup + the one-pass diagnostic methodology that localized it:
`docs/specs/mlx-weight-layout-and-afq.md` section 13. Oracle:
`scripts/mlx_ref.py` (--layers/--attn/--mlp/--router) +
`scripts/mlx_ref.qwen36-hello.txt`. Patch:
`patches/mistralrs-qwen36-afq-wip.patch`.

- [x] qwen36-fullattention-split - VERIFIED ALREADY CORRECT (red herring).
  - `qwen3_5_moe/text.rs` FullAttention already loads separate
    `q_proj/k_proj/v_proj` AFQ layers and its gate-interleave split matches
    `mlx_lm/models/qwen3_next.py::Qwen3NextAttention` exactly. Confirmed
    byte-for-byte: layer-3 (first FullAttention) `||x||` 1.4432 vs Python
    1.4460 once the RMSNorm fix is in. No layout change was needed.

- [x] qwen36-moe-switchmlp-layout - VERIFIED ALREADY CORRECT (red herring).
  - The MLX checkpoint ships experts **pre-fused** as
    `switch_mlp.{gate,up,down}_proj` `(num_experts, out, in)` (no per-expert
    `experts.<i>`). On Metal the `MoEExperts` Fast path -> quant
    `FusedExperts::new` loads them via `AfqLayer::afq_packed_linear_b` and
    the forward applies router weights correctly. The 14x magnitude was the
    RMSNorm bug feeding a 10x input, not a layout/dequant error. Confirmed:
    layer-0 per-expert outputs now match Python's `[0.78,0.25,0.57,...]`.

- [x] qwen36-numerical-parity-gate - PASSED.
  - `"Hello"` (11 tokens) renders identically in both runs; embedding
    byte-for-byte; all 40 layer last-position `||x||` match within bf16
    rounding; top-1 logit `id=8160 'Here' logit=22.0` identical to `mlx_lm`.
    Greedy generation begins identically.
  - Remaining (follow-up, not blocking): thread the fix into the upstream PR
    (`docs/specs/mistralrs-qwen36-pr.md`) and decide whether rozum's
    crates.io `mistralrs` 0.8.1 dep gets a `[patch]` to `.vendor/mistral-rs`
    or waits for an upstream release. NOTE: rozum currently has NO `[patch]`,
    so the fix lives only in `.vendor` + `patches/` until that is wired —
    `rozum`'s own binary still loads unpatched 0.8.1 and Qwen3.6 should keep
    routing through the LM Studio HTTP backend until then.

### Active

#### codex×gpt-oss gateway reliability (2026-06-21) — agentic matrix 22/30 → 27/30

Five OUR-bugs found+fixed in `src/gateway.rs` via the `isolate` skill (agent-bisection:
same gpt-oss, codex fails / opencode+claude pass → look at the codex path, not the model).
Full writeup [[project-gateway-patch-revert]]; specs `docs/specs/apply-patch-*`,
`loopbreak-edit-churn`, `patch-fuzz-tunable`.

- [x] apply-patch-idempotent — `patch -p0 --fuzz=3 -N --forward`: re-sending an already-applied
  patch was REVERTING the fix (no-tty "Assume -R?"). Coin-flip pass → deterministic. `f63d583`.
- [x] loopbreak-edit-churn — `detect_stuck_loop` signature 3: ping-pong edit-churn (≥3 edits to one
  file + a re-added removed line, or ≥6). Killed all 300s timeouts; opencode×gpt-oss 1/5→5/5. `c134334`.
- [x] apply-patch-fn-decode — the apply_patch FUNCTION-call reroute (gpt-oss's dominant shape) didn't
  decode `\uXXXX` → `collect::<String>()` landed as `collect::<String>()`. `14fe6c8`.
- [x] read-repair-default-on — gpt-oss emits broken `sed -n "src/main.rs"` → never reads → never fixes;
  the existing repair was gated off. Default-on + refined to only fire on broken reads. `14fe6c8`.
- [x] apply-patch-ws-fallback — gpt-oss drops the leading indent on changed lines; BSD `patch` (even
  `--ignore-whitespace`) can't match → `.rej`, fix lost (looked like a "revert"). Static python reads
  the `.rej`, matches by trimmed content, re-applies preserving indent. codex×gpt-oss×fix ~1-2/5→5/6. `6f2bed9`.
- [x] codex×gpt-oss build/test (create-from-scratch) — **DONE 2026-06-22: gateway-fixable, NOT a model
  limit** (the "model limit" verdict was reached without capturing the tool calls). `ROZUM_CODEX_TOOL_
  CAPTURE=1` on a real codex×gpt-oss build run showed **10/11 calls are one malformed shape**:
  `exec_command` args = `{cmd:"apply_patch", path, content}` (dup `cmd` key) — a **write-intent shoved
  into exec_command**. The `content` is a VALID Cargo.toml; exec_command only runs `{cmd:"<shell>"}`, so
  codex executed `apply_patch` as a bare command and dropped `path`/`content` → file never lands → build
  loops to timeout (rc=143), test fails. **FIX (landed):** `normalize_codex_tool_args` now detects a bare
  `apply_patch` carrying a non-patch `{path, content}` and synthesizes the real write — `mkdir -p
  "$(dirname '<path>')"; cat > '<path>' <<'ROZUM_WRITE_EOF'…ROZUM_WRITE_EOF` (single-quoted heredoc →
  verbatim body). Patch-content still folds to `patch --fuzz`; path-only untouched. Unit + shell-e2e
  validated; full model matrix cell deferred behind a concurrent GLM-4-32B run holding RAM (expected
  3/5 → 5/5). Spec `docs/specs/codex-create-write-synth.md`; Finding 5 in `docs/matrix-failure-analysis.md`.
- [x] **gptoss-v4a-isolate** — DONE 2026-06-22 (Finding 6). gpt-oss IS V4A-competent (5/5 clean, text+toolcall, multi-file); adherence COLLAPSES under codex's 21KB+18-tool load (5/5→2/5 with filler) — invents JSON / drops `*** Begin Patch`. 35B robust (minimal reasoning). Fix: codex-lean (shipped, explains why) + trim codex prompt + steer to simple primitive + prefer 35B for codex. — WHY does gpt-oss specifically struggle with codex's V4A `apply_patch`
  protocol, when 35B clears it and gpt-oss creates files fine under claude/opencode? Use the `isolate`
  skill (model-only probe, no agent): send a CLEAN minimal V4A create request straight to the gateway
  `/v1/chat/completions` for gpt-oss AND 35B (control); count shapes; prove model-V4A-competence vs
  codex-interface-degradation. Questions: (a) can gpt-oss produce a syntactically valid V4A patch at
  all? (b) is V4A out-of-distribution for gpt-oss's harmony training? (c) does the failure scale with
  prompt size / tool count (codex's 21KB/18-tool overload)? Writeup → `docs/matrix-failure-analysis.md`;
  feeds the Finding-5 solution (system-prompt steering to one simple create primitive).

#### Agentic-matrix model problems (catalog, 2026-06-21) — per-model failures + their cause

Consolidated from the agentic matrix (`scripts/bench/agentic.sh`, sandbox on). Reminder:
`verify_task` scores by **final file state**, not rc. Gold standard: **Qwen3.6-35B-A3B = 15/15**
(claude/codex/opencode), the agentic pick; **Qwen3-30B-A3B** close behind. Per-model issues:

- [~] **gpt-oss-20b** — claude **5/5**; codex/opencode the residual.
  - `fix`/`debug` (edit-existing): RESOLVED — five gateway delivery bugs (revert, churn, `\uXXXX`
    decode, read-repair-off, whitespace-`.rej` fallback), see the `codex×gpt-oss gateway reliability`
    entry above. codex×gpt-oss×fix ~1-2/5 → 5/6.
  - `build`/`test` (create-from-scratch): the dominant residual — gpt-oss can't scaffold via codex's
    `apply_patch` (`patch` can't create → `Oops.rej`; model flails `cat`/`tee`/`apply_patch`, stacks
    duplicate `[package]`, never reaches `src/main.rs`). Being addressed by `codex-create-write-synth`
    (apply_patch `{path,content}` → synthesize a `cat >` write). REASONING model → must sample (temp~1.0),
    greedy loops its CoT.
- [ ] **GLM-4-32B-0414** (new MLX-native port, byte-parity) — agentic matrix **4/15**. Symptom: in
  agent runs it **narrates tool use in markdown** (` ```zsh\ncat src/main.rs\n``` ` / ` ```bash\nRead\n{json}\n``` `,
  + ` ```prose\n…\n``` `, repeating blocks) instead of emitting a structured call → not parsed → not
  executed. **ROOT CAUSE ISOLATED (`isolate` skill — my first read "GLM is a weak driver / model
  characteristic, unfixable" was PREMATURE and WRONG):**
  - **Model-only probes (direct gateway, clean prompts): GLM emits PERFECTLY STRUCTURED calls** — single
    tool (`read_file {"path":…}`), claude's 4-tool set (`Read {"file_path":…}`), codex shell-framing
    (`shell {"command":["cat",…]}`), AND the Anthropic `/v1/messages` endpoint. So GLM is fully capable.
  - **Trigger reproduced deterministically:** add a big system prompt that instructs "explain in prose +
    use markdown before each tool call" (exactly what the agent CLIs do) → GLM returns
    ` ```prose\nI'm going to read…\n``` ` markdown narration, NO structured call. GLM **faithfully follows
    the narrate-in-markdown instruction**, which conflicts with structured tool-calling.
  - **Control:** Qwen3.6-35B is 15/15 under the SAME agent prompts → far less susceptible. So GLM-4-0414
    over-weights the narration framing vs the structured-call format. A model trait, but **prompt-triggered
    and FIXABLE**, not "inherently can't".
  - claude **2/5** (greet+fix): when GLM happens to wrap a clean `Name\n{json}` in a fence, the new
    `parse_glm_tool_call` fenced parser (serving.rs) catches it. Also a leak to fix: the parsed tool-call
    text is ALSO returned as a content/text block (Anthropic path returns both text + tool_use).
  - **FIX TRIED — (b) prompt-override REGRESSED, discarded.** Injected a strong last-word system
    instruction ("call a tool = ONLY name\\nJSON, no prose/markdown/fences") for GLM+tools. Isolated
    probe passed (big narration prompt → structured). But the real agent run **regressed claude
    fix 1→0**: the override worked partially — GLM *dropped the markdown fence* but kept a lead-in prose
    line → `prose\\nRead\\n{json}` (bare, un-fenced, embedded) → the fenced parser (which had been
    catching the ` ```bash\\nRead\\n{json}\\n``` ` form) now misses it. And codex still emitted ` ```zsh\\n<raw
    shell>\\n``` ` (not `shell\\n{json}`), so the override didn't reach/fix the responses path. A second
    "passed in isolation, regressed in the full multi-turn/multi-endpoint system" case (skill-recorded).
  - **FIX (a) SHIPPED — logit-constrained GLM tool calls (master `99c6081`).** Taught the masked
    decoder GLM's `name\\n{bare_args}` envelope: `find_glm_tool_call` anchors on the last line that is
    exactly a known tool name + `\\n`, then forces the args to that tool's JSON schema; a pure-prose
    answer (no tool-name line) stays free so the final-answer turn survives. Plus an embedded parser
    (`glm_embedded`) + `tool_markup_at` suppression. **Proven on the LIVE matrix** (not an isolated
    probe — the override's lesson): it fires and every call is schema-valid (claude×fix `Read`/`Edit`/
    `Bash` clean → **pass=1, no regression**; claude×debug calls clean). 449/0 lib tests.
  - **But the matrix score did NOT lift — and the reason is a _different_ gap, read from the kept
    transcripts (no premature verdict).** build/test/codex fail because GLM emits the **artifact
    directly** — Cargo.toml/main.rs *content* inside ```toml/```rust fences, or raw `cat`/```bash —
    instead of ever *naming* `Write`/`shell` (claude×test: tools=0; codex: raw shell). No tool-name
    line ⇒ no anchor ⇒ nothing for any output-format constraint to force. So **GLM-4-32B-0414 has a
    tool-use _decision_ gap, not a _format_ gap**: when it names a tool the call is now clean; for
    file-creation/shell it *shows* the artifact rather than *naming* the tool (a tuning property),
    fixable only model-side (a tool-calling-tuned GLM variant) or by intent-forcing that breaks the
    final-answer turn. debug = a 3rd axis (clean calls, driver loops → RUN_TIMEOUT).
  - **DECISION-NUDGE TRIED → DISCARDED (net-negative).** A positive few-shot in the GLM render
    ("you are an agent, call tools, don't print artifacts"; `ROZUM_GLM_TOOLUSE_NUDGE`). Live A/B on
    the same binary: it **proves the decision gap is prompt-MOVABLE** — GLM named `Write` for the first
    file in test where it never had — but it is **not a reliable lever**: it *reliably regressed the
    one stable cell* (nudge-OFF claude×fix = 1, 2/2 runs; nudge-ON = 0 — GLM read, described the fix in
    prose, and stopped without the Edit) and induced a new failure (args object emitted with NO tool
    name → no anchor). Discarded.
  - **META-finding: the 5-task matrix is too NOISY for small agentic deltas.** Control exposed it —
    claude×test flipped **0↔1 across two nudge-OFF runs of the same config**. So ±1-cell single-run
    deltas on these toy tasks are variance, not signal; only a *reliably* reproduced shift (like the
    fix regression, or a multi-run mean) counts. Don't read a single matrix run as a verdict.
  - **Verdict:** GLM-4 ships in `EXTRA` as a parity-exact chat/code model with **hardened tool-calling
    when invoked** (schema-valid args, no drift, default-on). The agentic-driver ceiling is a tool-use
    DECISION gap (GLM *shows* artifacts vs *names* Write/shell) — **prompt-movable but not
    prompt-fixable**; the robust fix is model-side (a tool-calling-tuned GLM variant). Qwen3.6-35B
    (15/15) remains the agentic driver; GLM-4 is the chat/code model. Spec `docs/specs/glm4-bringup.md`.
- [x] **Qwen2.5-Coder-7B / Qwen3-4B** — below the **~27B agentic cliff**: can't reliably drive the
  multi-step tool loop (2/5-ish). Not a bug; kept in `EXTRA` for non-agentic use. Don't surface as
  default agentic picks.

#### model-sandbox (2026-06-19, operator-driven) — structural jail for agentic model runs

**Goal (operator):** the models rozum hosts run agentic loops that touch the FS + shell; they must
be **structurally** unable to do harm **without a stream of per-action approval prompts** — free
inside an allowed path-set, denied outside by the OS, no asking. Spec: `docs/specs/model-sandbox.md`.
Two enforcement backends over one `(path,mode)` policy: macOS Seatbelt (default) and Docker (opt-in,
any OS). `ROZUM_SANDBOX=0` opts out; `--no-sandbox` is the launch-flag sugar.

- [x] **P1 — Seatbelt MVP (DONE 2026-06-19).** `src/sandbox.rs` `SandboxPolicy`/`rust_coding`/
      `to_seatbelt_profile`; `rozum launch` jails every agent (all exec paths via `sandboxed_command`).
      ON by default on macOS, secrets denied read+write, agent-state dirs writable. Validated on M4
      (cargo build in-jail succeeds, escape denied) + real matrix cell.
- [x] **`--no-sandbox` flag (DONE 2026-06-20, `56e4a03`).** CLI sugar over `ROZUM_SANDBOX=0`; hoisted
      by `reorder_launch_args`; left for the child after `--`.
- [x] **P3 — Docker backend (DONE 2026-06-20, `95d2814`).** `ROZUM_SANDBOX_BACKEND=docker` renders the
      same policy to `docker run` (bind mounts + tmpfs-masked secrets + `host.docker.internal` gateway
      + env allowlist). Validated: unit tests + real `docker run` e2e + full `rozum launch` run.
- [x] **rozum-agent image (DONE 2026-06-20, `ba1bb80`).** `docker/rozum-agent.Dockerfile` +
      `scripts/build-agent-image.sh` (rust + git + node + claude/codex/opencode). Validated: a real
      `cargo build` runs inside the container jail via `rozum launch`.
- [x] **sandbox-docker-resource-limits — DONE 2026-06-20.** `--memory`/`--cpus`/`--pids-limit` via
      `ROZUM_SANDBOX_DOCKER_{MEMORY,CPUS,PIDS}` (memory/cpus opt-in; pids default 2048 fork-bomb guard).
      `DockerLimits{none,from_env}` + render; verified on M4: 64 MB cap OOM-kills (rc 137), pids cap
      fails forks past the limit, normal builds unaffected. Also added `wget` to the rozum-agent image.
- [x] **sandbox-network-policy-knob — DONE 2026-06-20.** `ROZUM_SANDBOX_NETWORK`
      (`none`|`gateway-only` (default)|`full`) via `NetPolicy::from_env()`, honored by BOTH backends
      (`sandboxed_command` no longer hard-codes `GatewayOnly`). Verified via `rozum launch`:
      `none`→gateway BLOCKED (true zero-egress), `gateway-only`→REACHED. Unit test on the parse/aliases.
- [x] **sandbox-docker-strict-egress — DONE 2026-06-20.** `ROZUM_SANDBOX_NETWORK=gateway-strict`
      (`NetPolicy::GatewayStrict`) = true gateway-only-no-internet. Docker has no egress-allowlist flag
      (`--internal` kills the host too), so it's enforced IN the container: `--cap-add=NET_ADMIN` +
      `ROZUM_EGRESS=strict` → the `rozum-agent` entrypoint installs an iptables allowlist (lo +
      established + resolved host-gateway, DROP rest incl. IPv6; fails loud exit 70 if unenforceable).
      Seatbelt = `gateway-only` (already loopback-only). **Verified on M4 via `rozum launch`:** gateway
      REACHED, internet BLOCKED; `gateway-only` control reaches both.
- [x] **sandbox-docker-opencode-config — DONE 2026-06-20.** `write_opencode_config` now writes under
      canonical `/tmp` (a toolchain bind mount) instead of `$TMPDIR` (not shared by Docker Desktop), so
      the file is visible in the container at `OPENCODE_CONFIG` (verified in the `rozum-agent` image).
      Regression test `opencode_config_lives_under_tmp_so_docker_mounts_it`.
- [x] **sandbox-no-approval-autonomy — DONE 2026-06-20.** The "No-noise principle": when jailed,
      `rozum launch` injects the agent's approval-bypass flag for HEADLESS launches (`claude -p` →
      `--dangerously-skip-permissions`, `codex exec` → `--dangerously-bypass-approvals-and-sandbox`,
      `opencode run` → `--dangerously-skip-permissions`) — sandboxed models act with no per-action
      prompts (kills the codex reject-escalation loop, Finding 1a). Gated: jailed-only, headless-only,
      never overrides an explicit policy. Pure `autonomy_flag_for` (2 tests) + e2e-verified.
- [x] **sandbox-config-table — DONE 2026-06-20.** A `[sandbox]` table in `rozum.toml` (`SandboxConfig`):
      `workspace` (extra rw), `read_only` (Docker `:ro` mounts), `secret_deny` (extra denies),
      `network`, `backend`. Loaded in `sandboxed_command`, merged via `SandboxPolicy::rust_coding_with`;
      **env overrides config**. Config-parse + read-only + `rust_coding_with` tests + live smoke. **The
      model-sandbox track is now complete** (only the optional P2 Linux Landlock/bubblewrap remains).

#### meeting-web-pwa-ssc (2026-06-19, operator-driven) — phone-installable meeting client, then re-author in .ssc→Rust

**Goal:** a polished meeting web the operator opens + installs on their phone over the mesh,
then re-authored in ScalaScript that **compiles to Rust** and works with the rozum daemon.
Part of demo-conference (`docs/specs/demo-conference.md`); this is the meeting-web-PWA half of
the split (sibling owns the model-participant bridge). Worktree `feature/meeting-web-pwa-ssc`
(off rozum at `../rozum-meeting-pwa`). ScalaScript toolkit-on-Rust (the foundation) lives in
`../scalascript` (branch feature/rust-web-toolkit): vstack/heading/text/lower/serve→SSR, named
args, signals (data-ssc-*), server-push (/__ssc/push|state) — all shipped + green.

**STATUS 2026-06-22: the .ssc rewrite IS the shipped, only meeting web — the hand-written
`web.rs`/`web_index.html`/`meetings web` subcommand were removed. Source: `meeting-ssc/meeting.ssc`
on `feature/meeting-web-pwa-ssc` (worktree `../rozum-meeting-pwa`); launchd `com.rozum.meeting-ssc`
:8405 behind Tailscale `:8443`/`:8446`. (Code is safe in the worktree; this shared-master SPRINT.md
is churned by sibling agents — feature tasks also tracked in `meeting-ssc/TASKS.md`.)**

Done since the rewrite: rich text (inline `code`, `**bold**`, clickable links, done/working/blocked
badges, per-day date dividers, `HH:MM`, per-handle colour), trim-80, chronological `.sorted`,
auto-scroll, active-participants bar; content-integrity fix (quote truncation). **Dynamic rooms**:
`/r/<room>` for ANY room via http **prefix routing** (toolkit runtime) + `<select>` switcher.
**`/manage` panel** (⚙): rooms list/switch/create/delete + **bulk "clean empty rooms"** (19→8 junk
ghosts pruned; project rooms protected), models list/`rm`, agents view. Toolkit additions landed in
`feature/rust-web-toolkit`: `&str` patterns (contains/split/join/replace), Vec
`.take/.drop/.takeRight/.dropRight/.sorted/.distinct`, `String.toList→.chars()`, `.sum`, http
`no-store` + MIME-by-extension + prefix routing.

- [ ] **Gateway control in `/manage`** (2026-06-22, in progress) — `rozum gateway` HAS `status`
      (model/port/pid/uptime/clients), `switch --model <spec>` (clean drain→swap), `stop`/`unload`.
      Show active model + status; switch model (per installed model); stop/unload. Makes "manage
      models/agents" real. (Launching interactive agents via `rozum launch` stays CLI — a TTY program
      isn't a web button.)

- [x] **Installable PWA** (`c368774`) — apple-mobile-web-app meta + web manifest + service worker +
      SVG icon; routes `/manifest.webmanifest`, `/sw.js`, `/icon.svg` in `src/meeting/web.rs`. iOS
      Add-to-Home-Screen runs full-screen.
- [x] **Reachable from the phone over Tailscale** — busi runs tailscaled userspace
      (`~/.busi/tailscale`); `tailscale serve --bg --https=8443 → 127.0.0.1:8400` (HTTPS is REQUIRED
      for the SW's secure context). The operator's iPhone (on the tailnet) opens
      `https://busi.tail1174e2.ts.net:8443/`. busi's own `/`→:8088 untouched.
- [x] **No-auth mode for a network-gated front** (`c368774`) — `ROZUM_WEB_NO_AUTH=1` (empty secret ⇒
      `auth_layer` lets all in). Tailscale already gates the tailnet; the Basic-auth dialog was
      re-prompting on iOS and blocking entry.
- [x] **Persistent service** — `~/Library/LaunchAgents/com.rozum.meeting-web.plist` (RunAtLoad +
      KeepAlive; `--room demo --bind 0.0.0.0 --port 8400`, NO_AUTH). Survives session-end + reboot.
- [x] **Live-update fix** — a reverse proxy (Tailscale serve) buffers the SSE `/api/stream`, so the
      phone never saw new/just-sent messages. Added a `pollHistory` fallback (re-fetch /api/messages
      every 2.5 s + immediately after submit; `add()` dedups by date:n). web_index.html.
- [x] **Room picker** — DONE as `demo`/`rozum` tabs in the .ssc page header (fixed, centered);
      each tab is a `/r/<room>` GET; `/m/<room>` live fragment + `/p/<room>` POST per room.
- [x] **Re-author the meeting web in `.ssc`→Rust (operator directive) — DONE; now the ONLY version.**
      `meeting-ssc/meeting.ssc` compiles to a standalone Rust binary: multi-room (demo + the project
      rozum room, read from `.rozum/room/`), per-handle role colours, live JS polling of `/m/<room>`,
      fetch posting → `rozum meetings post`, full PWA (manifest/sw/icon + iOS standalone), grabber
      pull-to-refresh, flex + `visualViewport` layout (picker pinned, input above keyboard). launchd
      `com.rozum.meeting-ssc` :8405, Tailscale `:8443`+`:8446`. The hand-written
      `web.rs`/`web_index.html`/`meetings web` subcommand were REMOVED (`445c7a8`, cargo check clean).
      Toolkit fixes that unblocked it landed in scalascript `feature/rust-web-toolkit` (`a0aa846`:
      `&str` patterns, borrow intrinsics, `String.toList`→`.chars()`, `.sum`, http `no-store` +
      MIME-by-extension).

**Now — autonomous polish queue (operator: "все задачи в спринт и делай автономно", 2026-06-21):**
- [x] **Trim history** — `msgsView` now `takeRight(80)`s the content lines so polling + render stay
      bounded as rooms grow. Needed new toolkit Vec ops.
- [x] **Message timestamps** — `tsOf` parses the jsonl `"ts":` epoch, +UTC offset, shows a dim
      `HH:MM` per row. Surfaced + fixed a latent **ordering bug**: `readRoom` concatenated dated files
      in `listDir` order (non-chronological) → now `.sorted` so the newest message is last.
- [~] **Dynamic room list** — DEFERRED. The hardcoded `["demo","rozum"]` is the operator's earlier
      declutter ("слишком много комнат"); scanning `roomsDir` re-introduces junk rooms. Revisit only
      with an activity filter if a third real room is wanted.
- [~] **Highlight the operator's own messages** — NOT FEASIBLE as posed: the human web client and the
      agents all post under the same local identity ("Sergiy · <handle>"), so there is no reliable
      "me" marker distinct from agents. Per-handle colour (shipped) already separates sessions.

- [x] **Content integrity** (found in an audit, not the original queue) — messages containing a `"`
      were truncated: `msg()` split on the next bare quote. Now split on the `","ts":` boundary +
      `.replace("\\\"", "\"")` to un-escape. Needed a toolkit `.replace(from,to)`→`&str` patterns fix.
- [x] **Empty-room placeholder** + auto-scroll-to-newest on live update (when already at bottom).
- [x] **End-to-end QA (2026-06-21)** — all 8 routes 200 with correct MIME (html / manifest+json /
      text/javascript / image/svg+xml); post round-trips in both rooms; full content (no truncation);
      per-handle colours; `HH:MM` timestamps. Live on `:8443`+`:8446`.
- [x] **Readability polish (2026-06-21)** — inline `` `code` `` → monospace pills; `working:`/`done:`/
      `blocked:` status badges; per-day date dividers; favicon already served via `/icon.svg`. The rozum
      coordination room is now scannable. `.ssc` index-iteration needs a val-bound seq
      (`.takeRight(80).toList`) to stay indexable; inline split-index doesn't lower.

- [x] **Rich text + presence (2026-06-21)** — `**bold**`, clickable `http(s)` links, and an
      **active-participants bar** (distinct authors of the last 25 messages, coloured chips below the
      tabs). Note: `roster.json` is cumulative (daemon `leave()` does not prune it — `room.rs:194`),
      so "active = recent authors" is more accurate than the roster for a live "who's here".

Toolkit work that landed for the above (scalascript `feature/rust-web-toolkit`): Vec `.take`/`.drop`/
`.takeRight`/`.dropRight`/`.sorted`/`.distinct` lowering (`.drop` had collided with Rust's `Drop`);
`.replace`→`&str`; string char-hash (`String.toList`→`.chars()`, `.sum`).

#### demo-polish-and-resilience (2026-06-20, operator-approved) — make the live demo boringly reliable

**Goal:** after the sandbox/model-participant push, turn the demo path into something a human can
preflight, trust, and hand to sibling agents without rediscovering stale state. This is a
cross-cutting polish queue; detailed web tasks remain under `meeting-web-pwa-ssc`, and long-term
portability stays in BACKLOG.

- [x] **demo-doctor-self-test — first implementation pass (DONE 2026-06-20).** Added
      `rozum doctor [--web-url <url>] [--strict]` (spec:
      `docs/specs/demo-doctor.md`) to report meeting daemon reachability, room list, shared gateway
      health, sandbox backend/network, Docker image availability when Docker is selected, web/PWA
      endpoint reachability when a URL is supplied, Tailscale CLI availability, and the
      `scripts/demo-conference.sh` launcher. Must be non-destructive: no model launch, no service
      mutation, no Docker pulls, no room posts. Verified: `cargo test doctor --lib
      --no-default-features`, `cargo test doctor --lib`, `cargo build --bin rozum
      --no-default-features`, and live `target/debug/rozum doctor`.
- [x] **sandbox-regression-harness — DONE 2026-06-20.** Added
      `tests/sandbox_regression.rs`, an explicit jail-invariant target for
      Seatbelt/Docker: workspace write allowed, secret read/write denied, selected network policy
      enforced (`none`/`gateway-strict`), gateway reachable when expected, simple Rust build works
      inside the jail, no-approval flags are applied only for jailed headless launches, and opencode
      config remains visible in Docker. Fast coverage is safe by default; slow/host-mutating checks
      are `#[ignore]`. Verified: `cargo test --test sandbox_regression --no-default-features`,
      `cargo test sandbox_autonomy --no-default-features`, and macOS Seatbelt e2e via
      `cargo test --test sandbox_regression seatbelt_e2e_allows_workspace_and_denies_secret_and_escape
      --no-default-features -- --ignored`. Docker e2e rerun is deferred to BACKLOG
      `sandbox-docker-e2e-rerun` because Docker Desktop is memory-heavy and may be intentionally off.
- [ ] **PWA room picker.** Finish the active `meeting-web-pwa-ssc` room picker: `/api/rooms`, `?room=`
      on messages/stream/submit, selected-room UI, shareable room links, and mobile-safe unread/active
      state. This is the product/demo win after the doctor lands.
- [ ] **`.ssc` live data binding for meeting web.** Complete the ScalaScript/Rust web rewrite by
      binding the compiled `.ssc` page to live rozum rooms/transcripts/submits, then compare it against
      the current phone-proven PWA baseline.
- [x] **sprint-backlog-hygiene — DONE 2026-06-20.** Removed stale sprint/backlog signals now
      contradicted by master:
      model-participant is done, sandbox P3 is complete except optional Linux native jail, daemon web
      exists, and follow-up bridge wording now separates daemon-backed web from legacy
      telegram/discord bridge ports. The old `codex/queue-serving-hygiene` branch/commit `14c425f`
      remains consciously unmerged for separate review; it is not a current sprint blocker.
- [x] **windows-core-ci — DONE 2026-06-20.** Added a low-risk portability gate in
      `.github/workflows/ci.yml`: `windows-latest` build/test with `--no-default-features`,
      mirroring `linux-core`. This documents/exposes Unix-only assumptions without starting a full
      Windows port.
- [x] **meetings-rest-read — DONE 2026-06-21.** Added the daemon-side read-only HTTP API gated by
      `ROZUM_WEB_SECRET`: `GET /rooms/{name}/days` and
      `GET /rooms/{name}/messages/YYYY-MM-DD?from=N&count=M`. It reads only the room registry,
      `index.json`, and daily transcript files; no room writer/model/web UI path is touched. Listener
      is opt-in via `ROZUM_WEB_SECRET` and binds `ROZUM_MEETINGS_REST_BIND` or `127.0.0.1:8401`.
      Verified with tempdir REST unit tests, daemon tests, `cargo build --no-default-features`, and a
      live temporary-daemon curl smoke that posted into `rest-smoke` and read the message back.
- [x] **model-participant web controls — DONE 2026-06-21.** `rozum meetings web` now has a compact
      model control panel plus authenticated `/api/model/status`, `/api/model/start`, and
      `/api/model/stop`. It supervises one managed `rozum meetings participant` child per web process,
      passes model/handle/gateway/reply-policy/peers/persona options through to the existing CLI,
      rejects a second start with `409`, reports child exit and best-effort gateway state, and stops
      only the managed child. Verified with focused web tests, no-default build, and an isolated
      temporary-web live smoke (status → start manual participant → 409 on second start → stop).

#### matrix-failure-analysis (2026-06-18) — study every matrix red → fix or prove-structural+document

From the full-canonical matrix on master (60 cells, `results/full-canonical-091719`): claude 19/20,
opencode 17/20, codex 10/20 = 46/60, **0 panics / 0 rc=2** (infra clean; BUG-001 holds at full
scale). The **14 reds** are being worked one root-cause at a time. Living doc + evidence + verdicts:
`docs/matrix-failure-analysis.md`. Method: reproduce each cell with `KEEP=1`, classify by control
(other agent passes ⇒ agent mechanism; all fail ⇒ model ceiling), prove our-bug vs model-limit
before concluding.

- [x] **Finding 1a — codex stalls in the approval/meta-tool layer (30B repro).** Model requests
  escalated permissions (rejected under `approval=never`) + calls meta-tools (`request_user_input`),
  falls back to `cargo new <name>` (subdir). Plain shell itself works (`cargo new` in 154 ms).
- [x] **Finding 1b — codex writes CODE via `echo > file`; zsh escaping corrupts it (gpt-oss repro).**
  `println!("{}", rev)` → `println!({},rev)` (quotes lost) → won't compile; codex too slow → timed
  out before recovery. **This is the real "why not plain shell"**: shell is fine for commands,
  fragile for source code. claude/opencode write raw content via structured tools → code lands intact.
  Together 1a/1b **overturn** the (mock-derived) "structured-edit MCP" hypothesis.
- [x] **Finding 2 — opencode appends a DUPLICATE `fn main` (gpt-oss repro)** → E0428; its structured
  write is fine, the model's *edit* (append-vs-replace) isn't; timed out before fixing. Model-quality, not infra.
- [x] **Finding 3 — gpt-oss `build` is NOT a ceiling.** claude PASSES it (62 s). No true model ceiling
  among the 14 — a single matrix run mislabeled a flaky cell. The model emits buggy first drafts;
  whether a run passes is decided by **agent recovery within the time budget** (claude re-Writes the
  whole file and recovers; codex/opencode time out).
- [x] **Finding 4 — codex `fix`/`debug` (edit-existing): unified-diff vs apply_patch mismatch (27B repro).**
  The model correctly diagnoses the bug, then emits a **standard unified diff** (`--- /+++ /@@`) into
  codex `apply_patch`, which wants its bespoke `*** Update File:` format → `Invalid patch hunk` → edit
  never lands → bug stays. The precise, evidence-based version of `project-codex-patch-barrier`,
  specific to **edit-existing**. **Most actionable lever:** bridge unified-diff → apply_patch
  (gateway/wrapper). opencode `fix` **flaky-passed** the repro → its fix reds are speed/variance, not structural.
- **Synthesis:** codex fails *differently by task shape* (create → echo/approval 1a/1b; edit → patch
  mismatch F4). 3 interacting factors, none a clean infra bug — model code-quality × agent file-write
  mechanism (codex shell-echo/approval vs claude raw-Write vs opencode append-error) × speed/time-budget.
- [x] **raw codex tool-call capture — DONE 2026-06-21.** Added opt-in
  `ROZUM_CODEX_TOOL_CAPTURE=1` JSONL events for the Codex `/v1/responses` tool
  inventory and completed tool calls, preserving raw vs post-rewrite names/args
  across streaming and non-streaming paths.
- [ ] **Still to do:** repro the `fix`/`debug`/`test` reds (edit-existing-file,
  different shape); per-fail verdict (fix vs structural); A/B candidate fixes;
  re-run the matrix. Full evidence + verdicts in `docs/matrix-failure-analysis.md`.

> Note (2026-06-18): staged on branch `docs/matrix-analysis` (off master) because a co-agent occupies
> the master worktree; fast-forward / merge into master when it's free.

#### Meeting-room daemon: disk-backed, multi-room, dedicated daemon (spec stage, 2026-06-16)

Spec committed: `docs/specs/agent-meetings-daemon.md` (supersedes the
one-process-one-room topology in `agent-meetings-process.md`; `SPEC.md` updated).
Puts meeting rooms in a **dedicated meeting daemon** (`rozum meetings`) as many
disk-backed rooms (supervised tasks, not threads); the **model gateway is
untouched**. Key decisions locked with the user:
- **Topology**: dedicated meeting daemon `rozum meetings` (control:
  `start|stop|status`, like `rozum gateway`; `install|uninstall` as a launchd/
  systemd user-service like `rozum service`), separate from the gateway (gateway
  is stateless/scalable/idle-exiting, rooms are stateful/single-writer — don't
  co-host); model-as-participant is a localhost HTTP call to the gateway.
- **Storage**: append-only JSONL is canonical, in the project at `.rozum/room/`,
  split into daily files (`YYYY-MM-DD.jsonl`); message address is `(date, n)` with
  a per-day counter `n` reset to 0 each day (no global seq); daemon holds only
  high-water `(date,n,offset)` + budget counters (no turns in RAM). Lazy-created
  on first message; `.rozum/.gitignore`=`*`; `index.json` date→{count,bytes};
  `rooms.json` registry of room locations for discovery/reopen.
- **Single writer + direct-read clients**: `submit` is an RPC to the daemon (it
  owns seq/append/budget); TUI + `mcp-proxy` read `transcript.jsonl` directly,
  write-before-notify, content never transits the daemon. Agent tool contract
  unchanged.
- **Identity**: opaque `ParticipantId` + friendly per-project handle, stable for
  the proxy's session (token in proxy memory, binding persisted in `roster.json`);
  `#N` suffix removed.
- **Rooms by project**: project → one canonical room (idempotent); TUI is a
  daemon client — launched in a project it enters that room, else a picker;
  `[o]rooms` statusline shortcut to select/switch; many TUIs, independent. TUI
  renders/scrolls **by day** (loads current day, lazy older days, day separators).
- **Future (not now)**: remote stateless REST read on the meeting daemon,
  **day-scoped** (`GET /rooms/{name}/days` +
  `GET /rooms/{name}/messages/YYYY-MM-DD?from=N&count=M`).

**Status: P0–P6 ALL DONE + first polish pass (54 meeting tests green + live
CLI/stdio smoke) on branch `feature/meetings-impl`. The meeting daemon, agent
proxy, user-service, and human TUI client are implemented end-to-end.
POLISH DONE: graceful SIGTERM drain (pending waits → `{ended:server-shutdown}`),
idle-evict watchdog (`ROZUM_MEETINGS_IDLE_SECS`), and **content off the daemon**
(`wait` returns coordination only; proxy + `MeetingClient` read content from disk
via `store::read_since`). The only unverified piece is the ratatui *rendering* of
`rozum meetings attach` (needs interactive run); its logic is unit-tested.
`src/gateway.rs` untouched; fully additive. POLISH (2nd pass): per-room
panic isolation — every daemon tool handler runs under `guard(...)`
(`catch_unwind`), test `guard_isolates_panics`. POLISH (3rd pass) — all three
remaining items DONE: (a) **bare-`rozum` cutover** — `rozum` now attaches a TUI
to the daemon by default; `--legacy-room` (or `--web-port`) keeps the legacy
in-process room (bridges + sampling) as the escape hatch (additive, nothing
removed); (b) **room picker** — `Ctrl-O`/`/rooms` lists rooms (name/topic/
participants/last-day from enriched `rooms.list`), ↑↓+Enter switch, `/new`/`[+ new
room]` creates an ad-hoc room (`rooms.new`); (c) **second poll connection** —
`MeetingClient::spawn_poll` long-polls on its own connection + streams via mpsc,
so keypresses never cancel an in-flight `wait_my_turn` (tests
`poll_stream_delivers_new_messages`, `rooms_new_creates_ad_hoc_and_list_enriches`).
57 meeting tests green. ONLY the ratatui *rendering* of `rozum`/`rozum meetings
attach` needs interactive verification. **Update 2026-06-20:** model-as-participant is now done
via `rozum meetings participant` (see `docs/specs/demo-conference.md`); the daemon-side remaining
item here is the deferred REST read.**
Build sequence (each phase compiles + has its own tests; do them in order — P0→P2
are pure library and land behind today's behavior, P3 brings the daemon up, P4/P5
are clients and can go in parallel, P6 is the service):

- [x] **P0 — Storage core** — DONE (`src/meeting/store.rs`, 9 tempdir tests green;
      branch `feature/meetings-impl`). Library, no daemon. New `src/meeting/store.rs`:
      `RoomPaths` (resolve `<project>/.rozum/room/`, ad-hoc fallback
      `$XDG_STATE_HOME/rozum/rooms/<name>/`, `rooms.json` registry, write
      `.rozum/.gitignore`=`*`); `TranscriptWriter` (lazy-create on first append,
      daily file `YYYY-MM-DD.jsonl`, per-day `n` reset at rollover, `index.json`
      date→{count,bytes}, `meta.json` incl. `budget_chars`); `TranscriptReader`
      (open day file, tail `[off,end)`, parse whole lines, roll to next day).
      *Verify (tempdir unit tests):* append→`(date,n)`; rollover resets `n`;
      reader tails new lines; reopen recovers high-water from newest day;
      `index.json` rebuild; `.gitignore` + `rooms.json` add/list.
- [x] **P1 — Identity** — DONE (`src/meeting/identity.rs`, 6 tests green). Additive
      primitives (`Roster`/`RosterEntry`, `resolve_or_mint`, `mint_handle`,
      `display_name`): opaque UUID `ParticipantId` + `handle` (adjective-animal,
      unique-in-room) + `session_token` reconnect key, `roster.json` round-trip;
      no `#N`. Wiring into the room (replacing name/staleness reclaim) lands in
      P2/P3. *Verified:* same token→same id/handle; new token→new handle; roster
      reload rebinds; minted handle avoids taken.
- [x] **P2 — Room model** — DONE (`src/meeting/room.rs` `DaemonRoom`, 6 tests; all
      44 meeting tests green). PLAN ADJUSTMENT: built a **new** disk-backed room
      model additively instead of mutating the live `state.rs::Meeting` — keeps the
      build green; `DaemonRoom` owns a `TranscriptWriter` (no content in RAM) + a
      `Roster`, free-submit, shrunk `RoomEvent` (`{date,n,end_offset}`, no content),
      budget from the writer. The legacy in-process `Meeting`/`run_room`/web path is
      retired when bare `rozum` becomes a client (P5). *Verified:* join mints+
      persists+rebinds-by-token; submit appends to disk + emits shrunk Posted;
      reopen restores high-water+roster; max-chars ends.
- [x] **P3a — RoomRegistry** — DONE (`src/meeting/registry.rs`, 3 tokio tests).
      `RoomHandle = Arc<AsyncMutex<DaemonRoom>>`; lazy `get_or_create` (open from
      disk if `meta.json` exists, else fresh), `get_by_name` via `rooms.json`,
      `evict` (files stay, reopen on demand), `list`, `open_count`. *Verified:*
      idempotent while open; evict→reopen continues from disk (high-water+roster);
      list/get_by_name see registered rooms.
- [x] **P3b — daemon server + CLI** — DONE (`src/meeting/daemon.rs` +
      `src/main.rs` `meetings` cmd; 2 in-process rmcp tests + live CLI smoke).
      `MeetingServer` (rmcp on `meeting.sock`, per-session room) over the
      `RoomRegistry`: `rooms.list/join`, `_join_internal{project,session_token}`,
      `meeting.submit/wait_my_turn/mark_responding/status/leave`; lazy reopen via
      registry; `pub daemon_alive`/`daemon_rooms` client helpers. CLI `rozum
      meetings start [--foreground] | stop | status` (detached spawn, pidfile,
      socket-ping liveness, `kill` stop). *Verified:* join→submit→wait→list
      roundtrip over a unix socket; same-token rebind across reconnect; live
      start→status→stop lifecycle clean (no stray procs). DEFERRED to follow-ups:
      idle-evict watchdog, graceful drain (pending waits → `{ended}`) on SIGTERM,
      per-room `catch_unwind`; `install|uninstall` is P6.
- [x] **P4 — mcp-proxy → daemon** — DONE (`src/meeting/daemon_proxy.rs`; 1
      in-process roundtrip test + binary stdio smoke). New `DaemonProxy` stdio
      server: generates a `session_token` once, detects project (git root/cwd),
      auto-spawns the daemon (`rozum meetings start`), auto-joins the project room
      via `_join_internal{project,session_token}`, forwards `rooms.*`/`meeting.*`,
      and tracks the `(date,n)` cursor so `meeting.wait_my_turn` takes no args.
      `rozum mcp-proxy` now uses it by default (`ROZUM_LEGACY_PROXY=1` for the old
      per-room-socket proxy). PLAN NOTE: built additively beside legacy
      `proxy.rs` (untouched) rather than rewriting it. DEFERRED: move the wait
      content-read from the daemon into the proxy (`TranscriptReader`) — daemon
      currently returns content; the proxy already tracks the cursor. *Verified:*
      auto-join→submit→cursor-tracked wait→rooms.list over a unix socket; `rozum
      mcp-proxy` serves the 7-tool surface over stdio.
- [x] **P5 — TUI as client (model + shell)** — model DONE & tested; ratatui shell
      functional, interactive-verify pending. `src/meeting/tui_client.rs`
      `MeetingClient` (2 tests): connect-as-human, `list_rooms` (incl. `root`),
      `enter_project`/`enter_named` (joins `kind="human"`, identity on
      `rooms.join`), disk-read day-scoped transcript, cursor-tracked `poll`,
      `load_prev_day` scrollback. `src/meeting/attach.rs` + `rozum meetings
      attach [--room]`: ratatui loop (transcript + day separators + input,
      PgUp=older, Esc=quit), auto-spawns daemon, enters cwd project room.
      PLAN NOTE: additive (`rozum meetings attach`) — bare `rozum` / legacy
      `tui/mod.rs` untouched; the bare-`rozum`→client cutover + full picker UX are
      follow-ups. *Verified:* model tests (enter/submit/tail/list, scrollback) +
      build/CLI; **ratatui rendering needs interactive `rozum meetings attach`.**
      FOLLOW-UP: poll-cancel on keypress abandons one daemon long-poll (self-
      corrects); a second connection for the poll loop would avoid it.
- [x] **P6 — user service** — DONE (`src/service.rs` `meetings_launchd_plist`/
      `meetings_systemd_unit` + paths/labels, 3 tests; `rozum meetings
      install|uninstall` in `src/main.rs`, cfg-gated macOS/Linux). launchd
      `com.rozum.meetings` / systemd `rozum-meetings.service`, runs `meetings start
      --foreground` with RunAtLoad+KeepAlive / Restart=on-failure; logs under
      `state/meetings/service.log`. *Verified:* generation unit tests + CLI `--help`
      wires up; the real `launchctl`/`systemctl` call is operator-validated (same
      convention as the gateway service — not run against the dev machine).
- [ ] **Deferred (not now):** Future REST read-by-day on the meeting daemon's HTTP
      (`/rooms/{name}/days`, `/messages/<date>`). Model-as-participant via gateway
      local HTTP is DONE as `rozum meetings participant`.

#### agent-meeting-coordination — meetings as the collaboration system (2026-06-18, user-driven)

**Goal:** all the user's agents coordinate via meetings when they need to; the human sees what's
happening any time + intervenes, from any client. The daemon becomes a collaboration hub with
**equal pluggable clients** (TUI/web/telegram/discord/remote) + a **`Principal` identity layer**
(one human = one identity across clients; agents auto-identified; multi-user/remote = a resolver
swap later). Spec: `docs/specs/agent-meeting-coordination.md`. Phased P1–P4.

- [x] **P1.1 — post transport + author display (DONE 2026-06-18, branch
  `feature/agent-meeting-coordination`).** `meeting::tui_client::post_once` + `rozum meetings post
  <text> [--room <name>] [--as <display>]`: one-shot connect→join (project room by default, or a
  named room)→submit→exit; **auto-spawns the daemon** if down. The transport the SessionStart/Stop
  hooks will call + a handy human/script post. Also **surfaced the author** in the transcript:
  `room.rs::submit` now writes `display_for(id)` = `base_name · handle` (e.g. `claude · spry-wren`)
  instead of the bare handle, so readers see WHO posted (the `--as`/agent name). Verified live
  (auto-spawn → post → on disk → author shows the name) + `post_once` unit test (lands in room,
  unknown-room errors cleanly). 380 fast tests green.
- [~] **P1.2 — shared room (DONE) / true multi-room (deferred).** `ROZUM_MEETING_ROOM=<name>`
  routes an agent into ONE shared room (e.g. `commons`) instead of its per-project room: the
  proxy's auto-join uses `rooms.new` (create-or-open) when set, and `rozum meetings post` honors it
  (precedence `--room` > `ROZUM_MEETING_ROOM` > project) so hook posts land where the agents are.
  Verified live (post created+routed to `commons`; `status` lists it) + a `post_once` Shared
  create-or-open unit test. **Deferred (needs a daemon model change — best shaped by dogfooding):**
  being in the project room AND `commons` *simultaneously* (the daemon session is single-room);
  and a `rozum.toml [meeting]` config (env-only for now).
- [x] **P1.3 — `rozum mcp install/uninstall` (DONE 2026-06-18).** `rozum mcp install [--agent
  claude|codex|opencode|all]` registers `rozum mcp-proxy` via each agent's **own `mcp add`**
  (robust — agent owns its config), `uninstall` via `mcp remove`; idempotent (remove-then-add).
  Verified live + reversibly (claude user-scope ✔ Connected + codex registered, then removed).
  opencode's `mcp add` is interactive → guidance-only (config-write follow-up). 3 unit tests
  (`mcp_add_spec`/`mcp_remove_spec`/`expand_mcp_agents`).
- [x] **Presence via the proxy, not hooks (DONE 2026-06-18 — superseded the CC hooks).** The
  mcp-proxy posts a `joined:` line on its first join and a `left:` line when the agent's session
  ends, **over the agent's own session** → the presence line carries the agent's handle (unified
  with its messages), works for **every** agent (not just CC), and edits no `settings.json`. This
  replaced the earlier CC `SessionStart`/`SessionEnd` hooks (would double-post, CC-only, intrusive)
  — removed the hook-merge code + `--no-hooks` flag; kept serde_json `preserve_order` (harmless).
  Verified (the existing daemon-proxy test now asserts the `joined:` line + the submit).
- [x] **P1.4 — coordination instructions + AGENTS.md convention (DONE 2026-06-18).** Rewrote the
  mcp-proxy `PROXY_INSTRUCTIONS` (every connecting agent sees it) into a coordination contract:
  announce `working:` on start, check the room before clashing on files/`responding`, ask when
  blocked, post `done:`/`blocked:` on finish, human messages are priority — on the agent's own
  judgement. Strengthened AGENTS.md "Meeting-room coordination" (join paths, the etiquette, the
  one-shot `rozum meetings post`).
- [~] **P1.5 — TUI multi-room visibility.** The room picker (Ctrl-O / `/rooms`) already lists
  every room enriched with participants + last-activity day, so the human can see + switch across
  all rooms — the core multi-room visibility. A dedicated always-on overview *dashboard* (all rooms'
  unread at a glance) is **interactive-shaped polish** — best built once the operator has used the
  current TUI and says what the overview should show (can't be render-verified without a TTY).
- [x] **P1.6 — local-default `Principal`: stable local identity (DONE 2026-06-18).**
  `src/meeting/local_identity.rs` persists a stable `{token, display}` in
  `~/.config/rozum/identity.json`; the TUI (`MeetingClient::connect_as`) + `rozum meetings post`
  (human path) use it, so the operator is **one participant (handle) across launches/clients**
  instead of a fresh random one. `rozum identity whoami` / `set-name <name>` manage it. Verified
  live (two posts → same `Sergiy · mellow-marten`; set-name keeps the token) + a path-injected unit
  test (no global-env mutation). The first, zero-config rung of the Principal model. **Remaining
  (P2-P4, dogfooding-shaped):** unify the agent's hook-post + mcp-proxy handles (needs session
  correlation); link one human across web/telegram (P3); auth + multiple/remote humans (P4).
- [x] **P1.7 — daemon-backed web client with shared-secret auth (DONE 2026-06-19/20).**
  A human web UI to the **daemon** meeting rooms (the legacy `rozum web` bridges the in-process
  room — this one reads the daemon's disk transcript + submits via the daemon). `rozum meetings web
  [--port P] [--room name] [--bind addr]`. Auth = a single shared secret (env `ROZUM_WEB_SECRET`,
  else generated + printed on start): a login page takes the code → sets a cookie; API + stream
  require it. Endpoints: serve the chat page; `GET /api/messages?since=` (read transcript from
  disk); `POST /api/submit` (submit as the local human identity); `GET /api/stream` (SSE tailing
  the room → the **wakeup**: new messages appear live). Acceptance (with the user): the operator
  opens the link, logs in with the code, sees the transcript, posts a message that an agent (or
  `rozum meetings post`) sees, and an agent's reply appears live via SSE. **This is the first equal
  non-TUI client (P3 groundwork).** Implemented as `src/meeting/web.rs` + `rozum meetings web`;
  later PWA polish/no-auth/live-poll fallback is tracked under `meeting-web-pwa-ssc`.

> **MLX native runtime is DONE (correctness + perf), 2026-06-13.** Decode root-caused
> & fixed (bf16 stream leak in GatedDeltaNet q/k scaling → ~1000 casts/token): MoE
> decode 33→~88 t/s (2.7×), prefill →1215 (=Python), dense 16→~19.6; byte-exact; merged
> to master `74c7a96`, fork `0d4b3729`. The P0/P1 below are that work (kept for record).
> Chosen next (2026-06-13, user): the three tasks here; hand-fused Metal kernels → BACKLOG.

#### Agentic local-model reliability + memory (2026-06-16, from the agentic benchmark)

**Final 5-model matrix (2026-06-16, all fixes on): claude 18/25, codex 8/25. Infra is clean** — 0 loops,
0 timeouts, 0 crashes in working models. Remaining failures are model/agent, root-caused:
- **35B-A3B `rc=2` (memory, the only infra-ish failure).** The 35B gateway OOMs **during a single-shot
  prefill** of a big agent prompt: the prefill activation spike + ~25 GB resident hits the MLX memory cap
  (`total−8 = 28 GB` on a 36 GB Mac), and a **Metal OOM is process-fatal** → the gateway dies → every
  later task on that model returns `rc=2`. Not "the agent can't get RAM" (it needs <1 GB) — the *model's
  prefill* momentarily wants >28 GB. And the host rarely has spare RAM (≈7 GB free with a Claude Code
  session running). 27B (5/5) is the practical ceiling for 36 GB; 35B lowered + caveated in `models.rs`.
- **codex wrong paths (the big codex cluster).** The model runs `cargo new reverse-cli --lib` → creates a
  **subdirectory** (forbidden) + a lib (not bin), then can't edit its own files (they're in the subdir).
  Model instruction-following gap, amplified by codex's shell-first style; Claude uses `Write`/`Edit` →
  files land in cwd → passes. Hence claude 72% vs codex 32%.
- **Coder-7B text-only** (turns=1, tools=0): writes the solution as prose instead of calling tools.

- [x] matrix-teardown-panic — **DONE 2026-06-18 (P0; harness-side; ported to master).** The
  agentic matrix **rebooted the Mac via a KERNEL PANIC** — not an OOM. `scripts/bench/agentic.sh`
  tore each model's gateway down with `kill -INT` → 60s → **unconditional `kill -KILL`**; a SIGKILL
  landing on a wedged Metal eval double-frees an IOGPU buffer
  (`IOGPUGroupMemory::remove_memory_object() not found`) → panic → reboot. Fix (validated no-panic
  on 27B on the original `feature/matrix-teardown-panic-fix`, which went 70 commits stale → the
  still-needed change was **ported fresh onto master**, not merged): graceful teardown — SIGINT →
  wait `TEARDOWN_GRACE` (180s) for a clean exit → SIGKILL **only as a loudly-flagged last resort** →
  `GPU_SETTLE` (8s) so the kernel finishes async IOGPU reclamation before the next gateway allocates.
  Also added `ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0` to the launch (keeps the shared gateway alive across
  the claude/codex phases — the `clients_gone` self-exit, [[project-agentic-bench-clients-gone]]).
  `bash -n` clean. Tracked in **BUGS.md BUG-001**; root cause [[project-matrix-kernel-panic]].
  **VALIDATED ON MASTER 2026-06-18 (BUG-001 → done):** two matrix runs, neither produced a new
  `.panic`. (1) 35B × claude+codex+opencode × 5 tasks = **15/15 PASS, rc=0**. (2) the decisive one —
  claude × `27B → 30B-A3B → 35B` (`ROZUM_MLX_CACHE_GB=1`) = **15/15 PASS, rc=0, no panic across 2
  inter-model teardown transitions, no SIGKILL fired** (every gateway exited gracefully; footprint
  flushed 17.8→19.6→21.1 GB between models). The inter-model transition is exactly where the panic
  used to hit.
  **Deliberately NOT done (tracked follow-up):** the deeper rozum-side bounded/non-wedging teardown
  (a real Metal-eval timeout; Drop's `join()` can't block forever) — it touches the GPU teardown hot
  path and can't be validated without risking a reboot, so it isn't shipped blind.
- [x] mlx-chunked-prefill — **DONE 2026-06-16 (dense paths + lower default; fork `9fa852f4`).** Found the
  hybrid Qwen3.6 path ALREADY chunked; the gap was the **dense** `qwen3`/`qwen3_moe` Generate (single
  forward over the whole prompt → unbounded activation spike). Both dense Prefill states now chunk at
  `prefill_chunk_size()`, advancing the KV cache and eval'ing only the cache between chunks (MLX lazy-skips
  `lm_head` on discarded chunks). Added `KeyValueCache::collect_eval`. **Byte-exact verified on Qwen3-4B**
  (chunk=64 == single-shot). Lowered `PREFILL_CHUNK_DEFAULT` 2048→1024 for 35B headroom (incremental
  prefix-reuse makes per-turn cost ~nil). Cargo rev bumped. **Open:** the 35B OOM relief is reasoned, not
  re-measured (needs RAM headroom — pair with `ROZUM_MLX_CACHE_GB=1`); qwen2/gemma3/llama dense paths not
  yet chunked (rare/dropped models, no current OOM).

The agentic e2e matrix (`scripts/bench/agentic.sh`, 9 models × claude+codex × 5 tasks) drove a chain
of backend fixes and surfaced the levers below. **DONE this round:** cache-aware attention mask for
qwen2/llama (fork `bddd6feb`, fixes the multi-turn prefix-reuse crash), unify+robustify tool-call
parsing in `serving` (loose markdown/bare JSON), suppress loose tool calls from the text stream (fixes
the agentic re-emit loop), constrain-on-by-default, **JSON-repair** (recovers malformed tool calls on
the constrain-OFF fast path), per-task gateway (robust under MLX memory growth), Mistral-v0.3 dropped.

- [x] mlx-memory-cap — **DONE 2026-06-16.** Fork (`693f89ab`) exposes `set_cache_limit` /
  `set_memory_limit` / `set_wired_limit` (wrapping the mlx-c setters); rozum `cap_mlx_memory()`
  (`MlxNativeBackend::new`) sets them — `ROZUM_MLX_CACHE_GB` (default 4) + `ROZUM_MLX_MEM_GB` (default
  total RAM − 8). The cache cap is the key lever: MLX otherwise hoarded freed buffers to ~28 GB.
  **Validated:** a Qwen3-4B gateway serving 12 requests now peaks at **2.5 GB** (was ~28 GB) — the
  cache no longer accumulates, so the rc=2 cascade is gone and the shared per-model "load once" gateway
  is viable again (the per-task reload was only a workaround).

- [x] constrain-default-reconsider — **DONE 2026-06-16.** `ROZUM_MLX_CONSTRAIN` flipped back to
  **opt-in** (`=1` to enable). The `serving` JSON-repair recovers the common malformations on the fast
  path (Coder-7B agentic 2→4/5 with repair, constrain-off), so the B=1 masked decode is only worth its
  cost for the rare `","`-in-content case repair can't disambiguate.

- [x] agent-termination-nudge — **DONE 2026-06-16.** Lowered `MAX_TURNS` 30→15 and added per-task
  "you are DONE → STOP" nudges to all four coding prompts (build/test/fix/debug). The fix/debug nudges
  also tell the model that an `Edit` failing with "String to replace not found" means the change is
  **already applied** — do not retry, just verify. (See why below.)

- [x] agentic-loop-root-cause — **INVESTIGATED 2026-06-16.** Captured a real `rozum launch claude`
  transcript (Qwen3-4B, `fix` task) to answer "why can't agents stop when done?" Mechanism: the model
  applies the correct fix on its **first** `Edit` (success), then re-issues the **byte-identical** edit
  5+ more times; each now fails with `String to replace not found` (the target text is gone), and the
  weak model reads the error as "retry" rather than "already done" → it loops to `--max-turns`. It also
  **skips the verification step** (`cargo run`), so it never gets the positive "olleh" closure signal.
  Tool-call format is NOT at fault (native `<tool_call>` works). This is a small-model state-tracking
  limit: 0.5–4B loop (Llama-1B `fix` = 442 turns), Qwen3-30B-A3B does not. The run that *did* stop only
  did so by luck — the model emitted a `<tool_call>` missing its `name`, which our parser correctly
  dropped to text → `finish_reason: stop`. Levers: `max-turns` cap + prompt nudges (band-aids, shipped)
  + bigger model (architectural, in RECOMMENDED). One real model-agnostic server lever below.

- [x] agentic-loop-breaker — **DONE 2026-06-16.** Server-side, model-agnostic loop break in the gateway
  (`detect_stuck_loop` + `chat_or_loopbreak` + `synthetic_stop_stream`, all three protocol handlers route
  through it). On a detected stuck loop it returns a synthetic one-shot stream (text + `Done{EndTurn}`)
  instead of generating another doomed turn — every per-protocol serializer renders it as an ordinary
  `finish_reason: stop`, so the agent concludes. **Two signatures** (the e2e repro proved one isn't
  enough): (1) **structured** — the same tool call (name+input) repeated ≥3× with error results (Codex /
  Responses, and CC when tool use completes); (2) **text-repeat** — CC headless *interrupts* its own
  doomed tool use and records the turn as a text placeholder (`[Tool use interrupted]` / `(no content)`),
  so the gateway never sees structured tool blocks — only the same assistant text **recurring** in the
  recent window (it ping-pongs between a re-diagnosis and the interruption, so "N-in-a-row" misses it;
  the fix counts repeats in the last 6 assistant turns). **Verified e2e** (Qwen3-4B `fix`): the gateway
  logged `stuck_loop_broken`, the synthetic stop became CC's final turn, and the run ended
  `subtype=success` at 7 turns instead of looping to `--max-turns`. 5 unit tests; conservative thresholds
  = no false positives on distinct-progress conversations. (`ROZUM_DEBUG_LOOP` probe used during the
  investigation, removed.)

- [x] recommend-agentic-models — **DONE 2026-06-16.** `src/models.rs` RECOMMENDED rewritten with the
  agentic verdict: Qwen3-30B-A3B = best for local agentic coding, the 7B→27B capability cliff,
  Qwen3/Qwen3.6 native `<tool_call>` > Qwen2.5/Llama loose JSON. Dropped Mistral/SmolLM2/Phi-3/gemma×2.

- [x] cc-prompt-lean-mode — **DONE 2026-06-16.** `rozum launch --lean` strips non-coding tools from a
  launched `claude`. **Measured** the actual bloat first (Qwen3-4B, real launch): CC ships **33 tools /
  ~4,878 tool-schema tokens** every request — most non-coding (7 `mcp__rozum__*` meeting-room tools,
  `Cron*`, `Task*`, `Workflow`, `Enter/ExitPlanMode`, `Enter/ExitWorktree`, `Skill`, `Agent`,
  `ScheduleWakeup`, `LSP`, `Web*`, `NotebookEdit`). Key correction from the measurement: **`--allowedTools`
  is a *permission* whitelist, not a request shaper** (it left the count unchanged / *higher* — 35) —
  `--disallowedTools` is what removes schemas from the request. `--lean` injects `--disallowedTools` with
  the non-coding list (+ `mcp__rozum` server-wildcard) → **4 tools / ~761 tokens (−84%)**, leaving the
  Read/Write/Edit/Bash core. `apply_lean_tools` (`src/main.rs`) injects at the end of the program vector
  (variadic flag), no-op for non-`claude`, skipped if the user already manages tools; `--lean` added to
  `KNOWN_BOOL_FLAGS` so it hoists when placed after the program name. `scripts/bench/agentic.sh` now uses
  `--lean` (which covers the old `--disallowedTools AskUserQuestion`). 4 unit tests + verified e2e (claude
  still answers under the lean tool set; `--lean` ≠ regression — `fix` 3/3 with vs 3/3 without).
  **Update 2026-06-16:** `--lean` also adds `--exclude-dynamic-system-prompt-sections` — CC embeds git
  status (which changes on every file edit) in the system prefix, busting the prefix-KV cache and forcing
  a full ~1.4K-token re-prefill each turn; the flag relocates those per-machine sections into the first
  user message so the static prefix stays cached. Safe (relocates, doesn't strip the load-bearing system
  prompt). `apply_lean_tools` → `apply_lean_flags`; `fix` 4/5 with both levers, and successful runs went
  ~10 → 6 turns.

- [x] est-prompt-tokens-accurate — **DONE 2026-06-16.** `estimate_prompt_tokens(messages, tools)` replaces
  the Text-only `total_message_text`+`estimate_tokens` at all 3 handlers. The old estimate ignored prior
  tool-call args, **tool results** (file dumps / command output — often the largest blocks), and the
  **tool schemas** (~5K tokens) — under-counting an agentic turn several-fold, so the overflow preflight
  (`est > ctx_win`) could wave through a prompt that blows the model's window. Found while measuring
  `--lean` (the estimate stayed flat as the tool count swung 27→35). New estimate sums all block types +
  each tool's name+description+schema. Unit test covers tool-result + tool-schema contributions.

- [ ] ~~cc-system-prompt-strip~~ — **INVESTIGATED, WON'T DO (2026-06-16).** Tried to cut the other half
  of the prompt overhead (CC's ~1,400-token system prompt). CLI levers measured: `--bare` → sys ~27 tok
  (est −71%), `--system-prompt <minimal>` → sys ~49 tok (est −48%). **Both break the agent on local
  models:** `--bare` 0/3 on `fix` (model runs but can't complete the tool loop) + flaky `build`;
  `--system-prompt` minimal 0/3 on `build`. Unlike the tool schemas (pure overhead → `--lean`), the
  system prompt is **load-bearing** — operating instructions the weak model depends on. Left intact.
  `--exclude-dynamic-system-prompt-sections` only *relocates* per-machine bits (cache reuse, not size).


(mistral-system-fold moved to BACKLOG as WON'T DO — only Mistral-v0.3 needed it, and all kept models
support the `system` role.)

#### P0 (CURRENT, 2026-06-15): cascade-router — frugal/escalation model routing

**User-driven feature.** Spec: `docs/specs/cascade-router.md` (design agreed + availability/health
folded in). Try the cheapest/fastest model first (small local → big local → cheap remote →
frontier), escalate only when the cheap answer isn't good enough, stop at the first acceptable —
cheaper than a single frontier call on average (the opposite of Fusion's parallel ensemble). Caller
supplies the candidate list (inline or a parameter-selectable named config); one model = passthrough.
Configurable + adaptive (acceptance L0 structural → L1 self-signal/escalate-tool → L2 cheap judge;
strategy AlwaysCheapest / ClassifyThenStart / Learned; learned stats). Resilient: transient model
health (quota / rate-limit / down / network / OOM) → route to the best AVAILABLE model, auto-recover,
never hard-fail. Parallel scheduler (difficulty-routed non-blocking lanes; subsumes
`concurrency-multi-instance`). Built on `BackendOrchestrator`; remote tiers = `openai_http`/
`anthropic_http`. 7 phases, each its own branch + tests + merge; early phases deterministic/model-free.

- [x] cascade-p1-core - **Phase 1. DONE 2026-06-15** (`src/cascade/`). `ModelCard {id,backend,tier}`,
  `CascadeConfig` (`models` cost-ordered, `acceptance: [StructuralCheck]`, `CascadeBudget
  {max_escalations, wall_time}`), `CascadeBackend: ChatBackend` (1 model → live passthrough; else
  cost-ordered loop: drain each attempt → L0 verdict → Accept short-circuits, Escalate→next, errored
  skipped; budget/exhaustion → best usable so far, all-failed → error). L0 `StructuralCheck`:
  error→escalate; `response_schema`/tool-args validated via `constrain`→fail→escalate; free-form→
  inconclusive (accept). `AcceptanceCheck` trait + `Verdict` + `pipeline_verdict` (first decisive
  wins; all-inconclusive→Accept). 7 model-free e2e tests (escalate-on-structural-fail, accept-cheap-
  skip-strong, passthrough, error-escalate, budget→best-so-far, free-form-cheapest, all-error→error).
  185/0. **Gateway request-surface DONE** (branch `feature/cascade-startup-wiring`): `model:
  "cascade[:name]"` + comma-lists resolve to a `CascadeBackend` from `[cascade.<name>]` in
  `rozum.toml` (or `ROZUM_CASCADE[_NAME]` JSON). Built on lazy reload all along; the last gap —
  the gateway's **cold-start** build bypassed the detection — is fixed: `try_cascade_backend` is
  the shared chokepoint for both the startup build and the reload builder, so `rozum gateway
  --model cascade:fast` boots serving the cascade (smoke-verified: startup banner, not "no
  backend") + a startup-routing integration test.
- [x] cascade-p2-health - **Phase 2. DONE 2026-06-15** (`src/cascade/health.rs`). `HealthRegistry`:
  per-model `HealthState {Healthy, Degraded(half-open), Unavailable}` + `FailReason {RateLimited,
  QuotaExhausted, Down, Network, OutOfMemory, Unknown}`; `classify(err)` maps backend error strings;
  `record_failure` sets exp-backoff (base/reason × 2^fails, capped) + jitter cooldown; `is_available`
  goes half-open when the cooldown elapses; `record_success` → Healthy. Cascade loop now skips models
  in cooldown (best-AVAILABLE routing — sideways/down), classifies an attempt error → parks the model,
  a `Network` failure parks ALL `Location::Remote` cards at once, graceful degradation (best-so-far,
  hard-fail only if nothing available/usable). `ModelCard` gained `location: Local|Remote`. 6 new
  tests (classify, park→half-open→recover, backoff; e2e: parked-skipped-next-request, network-parks-
  all-remotes→degrade-to-local, OOM-big→fall-to-smaller). 191/0.
- [x] cascade-p3-self-signal - **Phase 3. DONE 2026-06-15** (`src/cascade/self_signal.rs`). L1 plus the
  **escalation affordance** (the user's point — teach the model the skill, don't guess at refusals):
  `EscalationAffordance` injects a system-prompt instruction into every NON-top tier ("if not
  confident, don't guess — reply `[[ESCALATE: reason]]`; admitting it beats being confidently wrong"),
  and `SelfSignalCheck` (L1) escalates on the marker, an `escalate`/`consult_stronger` tool call, or an
  opt-in refusal pattern (off by default — rely on the taught signal). The marker is stripped from any
  fallback answer. `escalation_tools()` exposes `consult_stronger` as a `ToolSource` for agent mode
  (composes with `run_agent`/`MultiToolSource`). Default cascade pipeline is now `[L0 structural, L1
  self-signal]` + affordance on. 7 new tests (marker/tool/refusal detection, affordance injection,
  marker strip, tool ack; e2e marker→escalate, marker-stripped-fallback). 198/0.
- [x] cascade-p4-judge - **Phase 4. DONE 2026-06-15** (`src/cascade/judge.rs`). L2 — a pluggable
  `Judge` trait (`async score(req,ans)->0..1`) consulted ONLY when L0/L1 are inconclusive; below the
  config `threshold` → escalate. `HeuristicJudge` (free — empty/explicit-non-answer → low) and
  `ModelJudge` (a small model rates 0–10, parsed; neutral 0.5 on judge error so a flaky judge never
  blocks). `pipeline_verdict` now returns `Option<Verdict>` (None = all-inconclusive → the cascade
  runs the async judge, or accepts if no judge). Opt-in (default `judge: None`). 5 new tests
  (parse_score, heuristic surface signals; e2e judge-escalates-low-quality, no-judge-accepts,
  model-judge-from-backend). 203/0.
- [x] cascade-p5-classifier - **Phase 5. DONE 2026-06-15** (`src/cascade/classifier.rs`). A
  `Classifier` trait (`difficulty(req)->0..1`) + `HeuristicClassifier` (length, code/math/multi-step
  markers, tool count, conversation depth — user/assistant text only, so system boilerplate doesn't
  inflate). `RoutingStrategy{AlwaysCheapest (default), ClassifyThenStart}` on `CascadeConfig`;
  `start_index` maps difficulty → a proportional *entry* tier (round to nearest of `0..n-1`); the
  candidate order is start-and-up then the cheaper tiers below as availability fallbacks
  (`(start..n).chain((0..start).rev())`), so a parked entry tier still degrades. Classification moves
  only the entry point, never the ceiling — escalation works unchanged from there. `AlwaysCheapest`
  is byte-for-byte the old order (all Phase 1–4 tests stay green). 9 new tests (6 heuristic scoring,
  3 e2e: trivial→cheapest, hard→skip-cheap, hard+entry-down→fall-back-below). 212/0.
- [x] cascade-p6-scheduler - **Phase 6. DONE 2026-06-15** (`src/cascade/scheduler.rs`). Residency
  lanes: a `Lane{Pool(name), Free}` per `ModelCard` (`Lane::default_for`: every local → one shared
  `"local"` pool, every remote → `Free`) + a `LaneSet` of one semaphore per pool. The cascade
  `enter`s a model's lane before each attempt and holds the permit only for that attempt (freed on
  escalation), so co-residents serialize (single-resident = 1 slot) while a request in a *different*
  lane — or any remote — never blocks. Multi-resident is the same code with `residency_slots[pool] >
  1`. Sits above per-backend `concurrency::admit_wrap`. All existing single-request tests are
  unaffected (locals share one 1-slot pool, but they run sequentially within a request anyway). 6
  new tests (5 LaneSet unit: distinct-pools-parallel, same-pool-serialize-then-free,
  Free-ungated, multi-resident-slots, default-lane-map; 1 e2e: a simple + a hard request on distinct
  lanes meet at a shared barrier ⇒ proven concurrent). 218/0.
- [x] cascade-p7-learned - **Phase 7. DONE 2026-06-15** (`src/cascade/stats.rs`). The learned-stats
  data layer: `TaskClass` (`{Freeform,Structured,ToolUse} × {Easy,Medium,Hard}`), an `AttemptRecord`
  per model attempt (accepted/escalated, latency, tokens, judge-score, `FailReason`, **+ concurrency
  level + a `ResourceSnapshot`** for the Phase-9 curve), a `StatsStore` (JSONL append-only +
  replay-on-open like `memory_store`; in-memory aggregates per `(task-class, model)` with EWMA
  latency/score + accept-rate). New `RoutingStrategy::Learned`: `start_index` enters at the cheapest
  tier whose historical accept-rate ≥ `learned_accept_threshold` (0.6) with ≥ `learned_min_attempts`
  (5) of evidence, else falls back to `ClassifyThenStart`. The cascade now records every attempt
  (opt-in `config.stats`). 8 new tests (5 unit: task-class buckets, accept-rate fold, learned-skip,
  needs-evidence, JSONL persist/replay; 3 e2e: learned-skips-cheap, learned-falls-back, records-into-
  stats). 226/0. **Deferred within the learned track** (not blocking): adaptive judge thresholds and
  health-pattern persistence feeding the backoff/proactive-deprioritization — small follow-ups on top
  of this store.
- [x] cascade-p8-exec-feedback - **Phase 8. DONE 2026-06-15 (user idea)** (`src/agent.rs`).
  Execution-feedback escalation in the agent loop: `run_agent_escalating(tiers, …, policy)` drives a
  **cost-ordered list of backends** and, when the current model keeps producing *failing* tool calls,
  hands off to the next tier — which inherits the full transcript (errors included) and corrects it.
  `ExecFeedbackPolicy{escalate_after_error_steps}` (default 2): N **consecutive** all-errored steps →
  escalate; any progress resets. The grounded "did it actually work" signal (a `ToolError` is ground
  truth the answer was wrong) — which the bare per-response cascade can't see (it returns before
  tools run). `ToolInvocation` gained `tier`; `AgentOutcome` gained `final_tier` +
  `tool_error_rate_by_tier()` (the per-tier `(errors, total)` bridge a caller maps tier→model to feed
  the P7 learned stats). `run_agent` is now a one-line wrapper (single tier, never escalates → all
  prior behavior unchanged). 3 new tests (escalate-on-persistent-errors, no-escalation-on-recovery,
  single-backend-stays-tier-0). 229/0.
- [x] cascade-p9-adaptive-concurrency - **Phase 9. DONE 2026-06-15 (user idea)**
  (`src/concurrency.rs`). `AdaptiveConcurrency` — a per-model **AIMD** controller (TCP-style): probe
  the admission limit up by one after `probe_after` clean runs (additive), multiplicatively back off
  (`backoff`, default ×0.5) the moment a model shows load. `ConcurrencySample{overload, headroom,
  latency_ratio, ok}` carries the user's signals — a load failure (429/quota/OOM), thin local
  resource headroom (back off *before* OOM), a latency cliff, and **quality-as-a-function-of-
  concurrency** (a failed answer is red only *above* the floor; serial isn't a concurrency problem).
  `record(model, sample) → new target`; push it onto the already-resizable `AdmissionScheduler::
  set_limit` (the actuator). Starts serial and opens up only as evidence accumulates — measured, not
  assumed. Composes with Phase-6 lanes for free: effective width = `min(adaptive limit, lane
  residency share)` (two independent gates in the pipeline, no extra code). 6 new tests (probe-up,
  back-off-on-overload, floor-holds, headroom+latency reds, quality-red-only-above-floor, drives-a-
  real-scheduler). 235/0. **Live feeding deferred to gateway integration**: classify each request's
  `FailReason`/`ResourceSnapshot`/exec-feedback into a sample and apply `set_limit` per model
  (lands with the cascade request-surface wiring).
- [x] cascade-gateway-wiring - **DONE 2026-06-15** (`src/cascade/spec.rs` + `src/main.rs`). Expose
  the cascade through the gateway: `model: "cascade"` / `"cascade:<name>"` now builds a
  `CascadeBackend`. A serializable `CascadeSpec`/`TierSpec` + `build_cascade(spec, resolver)` (the
  cascade stays decoupled — the resolver builds each tier: locals via `build_from_config`, remotes
  via the OpenAI-compatible HTTP backend with the env-named API key). Unbuildable tiers (missing key
  / endpoint) are **skipped**, not fatal; only an all-empty cascade errors. Named specs load from
  env JSON (`ROZUM_CASCADE` / `ROZUM_CASCADE_<NAME>`). `parse_cascade_model` routes the model string;
  `Location` is now serde. 6 new tests (parse cases, JSON round-trip, build-in-order, skip-on-fail,
  empty-is-error, pool-override). 241/0. **Remaining follow-ups**: Anthropic-native remote tier
  (v1 is OpenAI-compatible only); a `rozum.toml [cascade]` schema (v1 is env JSON).
- [x] cascade-adaptive-live - **DONE 2026-06-15** (`src/concurrency.rs`). The P9 live feed +
  **AIMD ⇄ circuit-breaker reconciliation**. The breaker and the AIMD controller both moved the
  admission `limit` — they'd fight. Fix: the controller now owns the **ceiling** (`set_ceiling` sets
  both `capacity` and the live `limit`), and the breaker (`trip`/`recover_step`) operates as a fast
  inner loop *within* `[1, ceiling]` — an acute OOM still drops `limit` instantly, but recovery
  can't climb above what the controller has learned the model sustains. `AdmittingBackend` gained an
  opt-in `AdaptiveConcurrency`: each completed request feeds a `ConcurrencySample` (clean → probe up;
  overload/error → back off via `set_ceiling`). Adaptive backends **start serial** and open up on
  healthy traffic. Enabled by `ROZUM_ADAPTIVE_CONCURRENCY=1` in `admit_wrap` (default off → static
  budget unchanged). 4 new tests (ceiling-bounds-recovery, ceiling-raises/lowers, probe-up,
  back-off-on-OOM). 245/0.
- [x] cascade-adaptive-signals - **DONE 2026-06-15** (`src/concurrency.rs`, `src/backend.rs`,
  `src/cascade/mod.rs`). Richer P9 signals beyond overload+success: (1) **latency** — the
  `AdmittingBackend` times each request and tracks a low-concurrency `ms/token` baseline (EWMA); a
  loaded request's `per_token/baseline` becomes the `latency_ratio`, so saturation (a latency cliff)
  backs the ceiling off. Cost-normalized (per-token) + a min-token floor so prefill-dominated tiny
  outputs don't add noise. (2) **quality** — a new `ChatBackend::report_quality(ok)` (default no-op;
  the `AdmittingBackend` overrides it) lets a higher layer feed the grounded verdict; the cascade
  calls it after each acceptance verdict, so a model whose answers are rejected *under concurrency*
  backs off ("quality drops under load" closed into the live loop) — no cross-layer registry needed,
  the backend owns its controller. 4 new tests (latency_signal baseline/ratio, latency-cliff
  back-off, report_quality back-off). 248/0.
- [x] cascade-adaptive-headroom - **DONE 2026-06-15** (`src/concurrency.rs`). The last P9 signal:
  **resource headroom**. `system_memory_headroom()` — a std-only, cached (~1s) macOS probe (`vm_stat`
  free+inactive+speculative+purgeable / `sysctl hw.memsize`) → free-RAM fraction `[0,1]`. The
  `AdmittingBackend` feeds it on every completed request; below the controller's `min_headroom`
  (0.15) the ceiling backs off **before** an OOM, not after. Unified memory → one system-wide figure
  covers all local backends; probe lives in the feature-free concurrency layer (no MLX coupling) and
  is **injectable** (`with_headroom_probe`) for a GPU-specific probe later / deterministic tests. 2
  new tests (low-headroom-holds-serial, probe-is-a-sane-fraction). 250/0. **`ConcurrencySample` is
  now fully fed** — overload, success, latency, quality, headroom.
- [x] cascade-anthropic-tier - **DONE 2026-06-15** (`src/cascade/spec.rs` + `src/main.rs`).
  Anthropic-native remote tier so a cascade can use Claude as the strong tier over `/v1/messages`
  (not just an OpenAI-compatible proxy). `TierSpec` gained `api: RemoteApi {Openai (default),
  Anthropic}`. `build_remote_tier` now branches: `anthropic` → `AnthropicHttpBackend` (default
  endpoint `https://api.anthropic.com`, key from `ANTHROPIC_API_KEY` — required, tier skipped if
  absent); else OpenAI-compatible (key from `OPENAI_API_KEY`). `api_key_env`/`endpoint` override the
  defaults. 2 new tests (api in JSON round-trip, api reaches the resolver). 251/0. **Remaining
  follow-up**: a `rozum.toml [cascade]` schema (v1 is env JSON).
- [x] cascade-p7-adaptive - **DONE 2026-06-15** (`src/cascade/{stats,health,mod}.rs`). The last two
  Phase-7 pieces. (1) **Adaptive judge threshold**: `StatsStore::is_trusted(task, model, …)` →
  `CascadeBackend::effective_judge_threshold` lowers the L2 judge threshold by `judge_trust_discount`
  (0.1) for a `(task-class, model)` whose historical accept-rate has earned trust — so we stop
  wasting escalations second-guessing a proven model; an unproven one keeps the base threshold. (2)
  **Health-pattern persistence**: `HealthRegistry::open(path)` replays a JSONL of health transitions
  (`HealthEvent` = failure + wall-clock cooldown deadline / recovery); a still-active cooldown is
  restored on start (the `Instant` rebuilt from the persisted unix deadline) with its `fails` count,
  so a quota-exhausted remote stays parked across a restart instead of being re-probed, and backoff
  keeps escalating. `CascadeConfig` gained `judge_trust_discount` + `health_path` (both opt-in,
  default off). 4 new tests (judge trusts/holds, cooldown-survives-restart, recovered-available).
  255/0.
- [x] cascade-model-list - **DONE 2026-06-15** (`src/cascade/spec.rs`, `src/models.rs`,
  `src/main.rs`). The **simple** cascade path: just list models, rozum auto-orders + auto-policies.
  `from_model_list(names)` classifies each name (`classify_model_name`: `claude…`→Anthropic,
  `gpt…/o1…`→OpenAI, else local) and **auto-orders** cheapest→most-capable (locals by parameter
  size — MoE by *active* params, ignoring the `Nbit` quant suffix — then remotes by provider tier),
  strategy defaults to `classify`. Wired two ways: (1) a comma-separated `model` string
  (`--model "qwen3-4b,claude-haiku-4-5,gpt-4o"` or the request's `model`) → `build_cascade_from_list`
  → auto-cascade; (2) the **launch picker now lists hosted Anthropic + OpenAI models**
  (`models::RECOMMENDED_REMOTE`) alongside locals and supports **multi-select** (e.g. `2 9 4`) →
  joined into a cascade. `build_remote_tier` now defaults the OpenAI endpoint
  (`https://api.openai.com/v1`). 2 new tests (provider detection, cheapest-first ordering). 258/0.
- [x] cascade-cli-ergonomics - **DONE 2026-06-15** (`src/main.rs`, `src/cascade/spec.rs`). `--model`
  is now **repeatable** on `launch`/`gateway` (`Vec<String>`): `--model a --model b` ≡ `--model a,b`
  (each value may itself be a comma list; `join_models` flattens both → the auto-cascade path). New
  **`--strategy`** flag (`classify`/`learned`/`alwaysCheapest`) flows via `ROZUM_CASCADE_STRATEGY`
  (the spawned daemon inherits it) and `build_cascade_from_spec` overrides the spec's start-tier
  strategy with it. `StrategyName::parse_cli` (case/separator-insensitive). 1 new test. 259/0.
- [x] cascade-toml-config - **DONE 2026-06-15** (`src/config.rs` + `src/main.rs`). Named cascade
  configs in `rozum.toml` via `[cascade.<name>]` tables (a `CascadeSpec`: `strategy`,
  `max_escalations`, `[[cascade.<name>.tiers]]`). `model: "cascade"` → `default`, `"cascade:<name>"`
  → `<name>`. `RuntimeConfig.cascades` + `cascade_spec(name)`; `main.rs::load_cascade_spec` prefers
  the TOML table, falling back to the env JSON (`ROZUM_CASCADE[_<NAME>]`) — so config survives a
  restart without exporting env vars. `TierSpec`/`CascadeSpec`/`StrategyName` gained `PartialEq/Eq`
  (RuntimeConfig is `Eq`). 1 new test (parses named cascade tables + default lookup). 256/0. **This
  closes the cascade-router** — all phases, gateway wiring, full adaptive signal set, learned track,
  Anthropic tier, and TOML config shipped.

#### Quick wins (2026-06-15)

- [x] ci-smoke - **DONE** (`.github/workflows/ci.yml`). There was **no CI**. Added a GitHub Actions
  smoke gate on `master` push/PR: `cargo build --lib --bin rozum` + `cargo test --lib` (feature-free,
  no Xcode/Metal) on `macos-latest`, with cargo caching + in-progress cancellation. Protects the
  pure-Rust core (SPI, gateway, agent, cascade, concurrency, config — 260 tests).
- [x] docs-bootstrap - **DONE** (`README.md`). The README was meeting-room only (0 mentions of the
  gateway/`launch`/cascade). Added a "Local LLM gateway & model cascade" quickstart (gateway, launch,
  picker, the cascade model-list + `--strategy`), refreshed the project layout (gateway/cascade/
  concurrency/agent/config modules) and the dev section (feature-free tests = what CI runs).
- [x] concurrency-cost-tokenizer - **DONE 2026-06-15** (`src/concurrency.rs`, `src/backend.rs`).
  Admission cost is tokenizer-pluggable: `RequestCost::estimate(req, count_tokens)` + a
  `ChatBackend::count_tokens(text) -> Option<usize>` hook (default None; `AdmittingBackend` passes
  `self.inner.count_tokens`). Fixed the fallback heuristic from **bytes** (`str::len()/4`) to
  **chars** (`chars().count()/4`) — the old one over-costed Cyrillic/non-ASCII ~2× and skewed SJF
  ordering — and it now sums tool-result + tool-call blocks. 3 tests. 270/0.
- [~] concurrency-multi-instance - **Core (shared GPU gate) DONE 2026-06-15** (`src/concurrency.rs`).
  A process-wide GPU gate (`global_gpu_gate`, semaphore sized to `DEFAULT_SEQS_CEILING`; `ROZUM_GPU_GATE`
  overrides, `0` disables) every local `admit_wrap`-ped backend acquires *after* its per-model slot —
  so concurrent prefills across **distinct resident models** can't oversaturate one GPU. No priority
  inversion (admit-then-gate), composes with cascade lanes + adaptive ceiling, no-op for a single
  resident (gate ≥ cap) → safe default-on. 2 tests. 272/0. Size-class routing = cascade lanes +
  multislot residency; shared memory budget = multislot Phase 2.
- [~] portability-platform-features - **Durable core DONE + CI-enforced 2026-06-15** (`ci.yml`,
  `Cargo.toml`). `cargo build --no-default-features` builds + tests the non-backend durable layer
  (SPI/gateway/agent/cascade/concurrency/config/meeting — 271 tests) with no native toolchain; a CI
  `linux-core` job (`ubuntu-latest`) enforces it every push. Gated one MLX-only test module on the
  feature. Remaining (needs a Linux box): bare `cargo build` on Linux — native backends are
  Metal-bound (mlx-sys + `llama-cpp-2 metal`); → `portability-cuda-gguf`. Also closed C-category
  no-longer-developed items (candle / gguf-tool-use / preemption / superseded stubs).
- [x] shared-gateway-service - **DONE 2026-06-15** (`src/service.rs` + `src/main.rs`). `rozum service
  {install,uninstall,status}` registers the gateway as an always-warm user service (launchd / `systemd
  --user`) instead of lazy spawn + idle-exit. `--model` repeatable/cascade + `--port/--n-ctx/--offline/
  --strategy`; `ROZUM_CASCADE`/`ROZUM_CONFIG` captured. Pure plist/unit generation in the tested
  `service` module (4 tests); binary drives `launchctl`/`systemctl` (operator-validated). Also
  re-closed `streaming-output` (lost backlog doc-edit). 282/0.
- [x] docs-hygiene - **DONE 2026-06-15.** Two doc items. `portability-new-backend-checklist`: the
  add-a-backend recipe written down (`docs/specs/portability-and-the-backend-spi.md`). `prompt-policy`:
  a documented decision (`docs/specs/prompt-policy.md`) — the gateway is a transparent provider, no
  injected per-model prompts (raw is the only mode; per-model persona lives in the caller).
- [~] shared-gateway-multislot - **Phase 1 (decision core) DONE 2026-06-15 (user idea)**
  (`src/resident.rs`). Adaptive memory-gated residency: small requested models that fit and are
  *statistically useful* (frequency × recency) stay co-resident without thrashing; the least-useful
  idle model is evicted to make room; a model too big to co-reside falls back to a swap (unavoidable
  thrash) — "pick the best arrangement possible under the memory budget". `UsageStats` (persisted
  JSONL, `ModelUsage::utility` = count × recency-decay) + the pure `plan_residency` planner
  (keep-highest-utility-that-fits, busy never evicted, `oversubscribed` = swap case). 7 tests. 267/0.
  **Phase 2 IMPLEMENTED 2026-06-15** (mock-tested; `src/gateway.rs`): an **additive warm cache**
  alongside the untouched single-resident core, **on by default** (user's choice; `ROZUM_MULTISLOT=0`
  opts out), a **strict no-op for single-model traffic**. `enter(req.model)` routes a *different*,
  warmable model (known cached local that fits) to a warm secondary resident built via the existing
  builder; admit/evict via `plan_residency`; warm entry has its own inflight (decoupled from the
  primary drain); idle-only eviction with `spawn_blocking` drop; any miss falls back to the primary.
  4 tests (serve-second, fall-back-too-big, skip-unknown, evict-idle). Plus **idle-timeout warm
  eviction** (`sweep_idle_warm` in the watchdog frees a warm model idle past `unload_idle_secs`; each
  entry tracks its own last-activity, busy never swept) and **persisted `UsageStats`**
  (`$XDG_STATE_HOME/rozum/gateway/warm-usage.jsonl`). 6 tests. 278/0. **Real-model validation pending**
  (two real models co-resident, eviction frees RAM — operator runs it).
- [x] cascade-offline - **DONE 2026-06-15 (user idea)** (`src/main.rs`). `--offline` on
  `launch`/`gateway` (→ `ROZUM_OFFLINE`, inherited by the spawned daemon): `build_remote_tier` skips
  every remote tier (dropped like any unbuildable tier — locals survive, an all-remote cascade
  errors), and the launch picker hides the cloud entries (Anthropic + OpenAI). Use only local models.

#### P0 (NEXT): gateway-cc-codex — reliable local-LLM provider for Claude Code & Codex

**Sprint goal #2.** The outward gateway exists (`src/gateway.rs`: `/v1/chat/completions`
OpenAI + `/v1/messages` Anthropic + `/v1/models` + `/control/*`). Make it a *reliable*
drop-in for Claude Code and Codex against the in-process MLX/GGUF engine.
- [x] gateway-prod-perf-verify — DONE. Static: default `build_gateway_backend` routes
  Qwen3.6 → native MLX → `LoadedModel::load` sets `ROZUM_MLX_RETAIN` (hybrid), so the
  +2.7× win is live on `/v1/chat/completions` + `/v1/messages`. Guard: `is_hybrid_model`
  extracted + `hybrid_models_need_retain` unit test (fast suite). Runtime: new perf test
  `mlx_moe_backend_chat_tps` drives the full `MlxNativeBackend.chat`. **FINDING: prod path
  61.8 t/s vs the raw `model.forward` bench ~88 — ~30% lost to per-token detok** (see next).
- [x] **gateway-streaming-detok** — DONE, but MISDIAGNOSED → real fix was pipelining.
  Profiled the prod path (`ROZUM_DETOK_PROFILE`): **detok = 0.03 ms/tok** (negligible),
  the ~30% gap was the **per-token GPU sync** (`eval` + `token.item()` readback) run
  SERIALLY. `Qwen35`/`Qwen35Moe` still passed `pipeline=false` on a stale "kernel
  blocking-evals per call" assumption — the retain fix removed that eval (bench
  `serial==pipe` MATCH). Flipped both to `pipeline=true` (overlaps the next token's
  forward with the current readback): **prod `backend.chat` 62/72 → 96.2 t/s**, sync
  14 ms → 0; byte-exact (hybrid 27B + MoE chat pass). Perf-test floor bumped to 80 t/s
  to guard it; kept a small env-gated per-token profiler. master `b9ef3d5`. (No
  streaming detokenizer needed — left in BACKLOG only if a future tokenizer is slow.)
- [x] gateway-cc-codex-audit — DONE (synthetic pass, 2026-06-13). Launched `rozum gateway`
  + Qwen3-4B and drove the wire protocol. **Core is solid for both dialects:** OpenAI
  `/v1/chat/completions` (non-stream JSON + stream SSE `role→deltas→finish_reason→[DONE]` +
  tool-use `tool_calls`/`finish_reason:"tool_calls"`), Anthropic `/v1/messages` (full SSE
  `message_start→content_block_start→content_block_delta→content_block_stop→message_delta
  {stop_reason}→message_stop` + non-stream JSON + tool-use `tool_use`/`input_json_delta`),
  `/v1/models`, stop-reason mapping (length↔max_tokens, tool_calls), validation→HTTP 422.
  The gateway's own banner targets CC (`ANTHROPIC_BASE_URL`) + Codex (`OPENAI_BASE_URL`).
  Findings → fixes below. (Still worth a LIVE pass with real CC/Codex for client-specific
  quirks; needs the user to point them at the endpoint.)
- [x] gateway-cc-codex-fixes — DONE (3 fixed, 1 not-a-bug):
  - [x] **tool-call-xml-format** — FOUND in the tool-use pass + FIXED (master `8c29870`),
    the most impactful. Qwen3.6 emits tool calls in EITHER `<tool_call>{json}</tool_call>`
    OR the Hermes `<tool_call><function=NAME><parameter=K>V</parameter>…</function></tool_call>`
    form, nondeterministically. `parse_tool_calls` only handled JSON → the XML calls were
    SILENTLY LOST (text suppressed by `<tool_call>`, parse fails → empty response) — a
    showstopper for agentic coding. Now parses both forms + tolerates a missing close tag
    + never swallows tokens (raw-text fallback). Verified read→write_file 5/5 OpenAI + 3/3
    Anthropic. (Underlying greedy MoE nondeterminism between the two formats is a separate,
    model-level quirk; the parser now handles both so it doesn't matter.)
  - [x] **stream-default** — FIXED (master `b4c6501`). `req.stream.unwrap_or(true)` → `false`
    in both handlers + debug logs; an absent `stream` now returns non-streaming JSON (spec).
    Verified live (omit `stream` → `chat.completion` JSON); gateway lib tests 14/0. `rozum
    launch` only exports env (no requests), CC/Codex set `stream:true` explicitly → unaffected.
  - [x] **models-id** — NOT A BUG. The `claude-rozum-<spec>` id is intentional
    (`claude_model_alias`): `rozum launch` exports it as `ANTHROPIC_MODEL` so CC pre-selects
    the local model instead of the default OAuth one. Request `model` is ignored (one loaded
    model). No change.
  - [x] **think-passthrough** — FIXED via option (c), made a launch flag (user's call:
    "disable by default"). The native backend renders the chat template with
    `enable_thinking=false` by DEFAULT, so a reasoning model (Qwen3) prefills a closed
    `<think></think>` in the PROMPT and the generated OUTPUT is clean (no think tags at all,
    not even the empty `/no_think` wrapper). A new `rozum gateway --enable-thinking` flag (or
    `ROZUM_ENABLE_THINKING`) turns reasoning back on. Threaded `enable_thinking: Option<bool>`
    through `ApplyChatTemplateArgs`→minijinja `context!` (fork `4978c2d4`). Verified e2e:
    default → no `<think>`; `--enable-thinking` → `<think>`. master `550f970`.

#### P1 (NEXT): meeting-room-reliability — solid "agents + human operator" room

**Sprint goal #1.** `meeting/` + the rozum MCP server exist. Harden the live flow.
- [x] meeting-reliability-audit — DONE. Mapped the failure modes; found ONE real bug
  (below). Verified-handled: **disconnect/crash cleanup** (`app.rs` spawn_connection:
  `service.waiting()` returns on socket EOF → auto-leave → clears participant + responding/
  polling markers); **stale markers** (responding/polling filtered at read time ~30s + cleared
  on submit/leave, existing tests); **concurrent submits** (serialized by the meeting `Mutex`,
  seq-ordered; "anyone submits any time" by design — no turn lock to corrupt); **wakeup-channel
  vs long-poll** (long-poll is seq-based off the transcript, so a missed broadcast still
  recovers on the next poll). idle-model-unload doesn't touch meetings (rooms are independent
  of the gateway model).
- [x] **meeting-reliability-fixes** — lost-wakeup in `wait_my_turn` FIXED (master `0c85de5`).
  `post_submission` wakes pollers with `notify_waiters()` (stores NO permit), but `wait_my_turn`
  created `notify.notified()` AFTER its transcript check — a submit landing between the check
  and the `.await` was lost, stalling the poller until the 25s deadline (~25s latency on a
  message). Fix: arm the `Notified` (pin + `enable()`) BEFORE the check; kept `notify_waiters`
  (wake-all, since N agents poll concurrently — `notify_one` would wake only one). Regression
  test `wait_my_turn_wakes_on_submit` (poller must return ≪25s on submit). All 23 meeting tests pass.

#### P2: mlx-prealloc-kv — pre-allocated KV cache ✅ DONE (2026-06-13)

`ConcatKeyValueCache` now pre-allocates the key/value buffers in `KV_STEP`(256)-blocks
along the sequence axis and writes each step in place via `slice_update`, returning a
`[:offset]` view — instead of `concatenate`-ing (+ reallocating) the whole history every
decode step (mirrors `mlx_lm`'s `KVCache`). The O(ctx) per-step copy → amortised O(1)
write (one growth concat / 256 steps). **Decode byte-exact** (greedy IDs unchanged, all
chat tests pass, fast lib suite 118/0); decode t/s flat across ctx 64→1024. Chunked-vs-
single prefill stays argmax-exact (~1 bf16 ulp from the strided-slice SDPA when the
single-pass length isn't step-aligned; gate relaxed 1e-2→0.2 with an inline explanation).
Fork `d197d1da`, rozum master `c7ffa5c`. (As expected, no headline-speed change — the
concat was already flat in the ctx sweep; the win is constant-memory KV growth for long
sessions + no realloc churn.) Next: **P0.1 gateway-prod-perf-verify** (runtime).

---
#### (record) P0 RESOLVED: mlx-native-perf-compile — close the decode-speed gap to Python

**✅ RESOLVED (2026-06-12, merged to master).** The hybrid (Qwen3.6) decode gap is
the gated_delta per-call eval, and its ROOT CAUSE is MLX's **unretained
command-buffer references** (`commandBufferWithUnretainedReferences`): a buffer
feeding the custom kernel is freed/reused before the in-flight GPU dispatch reads it
→ garbage from token 2; the per-call eval was a ~48-sync/token workaround. FIX:
env-gated **retained refs** (`ROZUM_MLX_RETAIN`) applied via a `PATCH_COMMAND` on the
MLX FetchContent (mlx-c fork), enabled by the backend for hybrid models only;
gated_delta then drops the per-call eval. **27B decode ~12 → ~16-17 t/s (+30-40%)**,
byte-identical output; dense models unaffected. Forks pushed
(`sergey-scherbina/mlx-c` @ rozum-retain-0.30.6, `sergey-scherbina/mlx-rs` @
rozum-hybrid-decode); rozum pinned to mlx rev `09c5b20d`. Full investigation +
bottom-up Python↔Rust log + patch: `docs/mlx-gd-bug/`. Bumping MLX does NOT help
(master/0.31.2 still unretained); a per-op buffer-lifetime difference vs `mlx_lm`
(which is correct serially on the same MLX) remains a curiosity but is not needed.
The "mx.compile / capture-based" plan below was the wrong lever (probed, dead end);
kept for the record.

#### P1 (follow-up): mlx-native-decode-gap-remainder — re-scoped: gap is REAL; MoE is the big one

**Status: OPEN, re-measured CLEAN 2026-06-12. The "structural FFI overhead" verdict was
WRONG, and the gap is bigger on the MoE than the dense.**

**⚠ METHODOLOGY: kill the Bloop/ScalaCli Java daemon before benchmarking.** It held
**17.9 GB / 38.7**, and unified-memory contention throttles Python HARDER than Rust →
the gap *fakes parity* under pressure (dense Python 15.5 vs Rust 14.4 = 1.08×, false).
`ps -A -o rss,comm | sort -rn | head` → `kill <java pid>` (respawns on demand) → re-bench.
A whole "we're at parity" detour came from benchmarking under this contamination.

**CLEAN numbers (Bloop killed), Qwen3.6-4bit, n=512 prefill + 64 decode:**
| model | Rust (retain) | Py manual | Py `generate` | decode gap | prefill gap |
|---|---|---|---|---|---|
| Dense 27B   | 16.2 t/s | 19.8 | **23.0** | **1.4×** | 170 vs 194 (1.14×) |
| MoE 35B-A3B | 33.4 t/s | 97.3 | **110.9**| **3.3×** | 584 vs 1180 (2.0×) |
Output IDs byte-identical Rust↔Python on both. **The MoE 3.3× is the real prize**, not
the dense 1.4×. (Note: MoE 33 t/s is already faster than dense 16 — usable; this is a
parity-with-Python goal, not a usability blocker.)

**The gap is in `eval` (GPU dispatch), NOT mlx-rs FFI/binding.** Instrumented split,
Rust MoE decode: `build` (FFI node construction in `forward()`) = **2.4 ms/tok (8%)**;
`eval` (graph traverse + Metal dispatch + GPU) = **28.7 ms/tok (92%)**. `eval` is shared
C++ → our op GRAPH dispatches more / less-efficient GPU work than Python's for identical
math. So the fix is leaner/fewer GPU dispatches, not a faster binding.

**Levers:**
- [x] decode-gap-measure - DONE, corrected: dense 16.2 vs 23.0 (1.4×); MoE 33.4 vs 110.9
  (3.3×). Earlier "17 vs 22.8 structural FFI" was cross-session machine-state contamination.
- [x] decode-gap-031 - DONE: version is not a lever.
- [x] decode-gap-compile - TESTED net-NEGATIVE (compute_g via mlx-rs compile: 16.5→15.8).
- [x] decode-gap-pipeline - TESTED ~neutral on both models clean (dense 15.9→16.2, MoE
  32.1→33.4). Not the lever.
- [x] decode-gap-ffi-vs-eval - DONE: build=8%, eval=92% → NOT FFI overhead; it's GPU-side.
- [x] decode-gap-kvcache - RULED OUT for decode: `ConcatKeyValueCache` does an O(ctx) concat
  per step but the context sweep is FLAT (Rust 32.7→30.9 t/s ctx 128→1024). Not this gap.
  (Still worth a pre-alloc cache for very long contexts — separate concern.)
- [x] **moe-prefill-sort** - DONE, SHIPPED. Ported Python `SwitchGLU` expert sort
  (`_gather_sort`/`_scatter_unsort` + `sorted_indices=true` into `gather_qmm` when
  `indices.size>=64`) in `qwen3_moe.rs` `SwitchGlu`/`QSwitchLinear` (shared by qwen3_moe +
  qwen3_5_moe). **Qwen3.6-35B-A3B-4bit prefill 584 → ~1020 tok/s (n=512, ~1.7×)** toward
  Python's 1180; decode unchanged (no sort at T=1); byte-exact (decode IDs identical), both
  MoE chat tests pass. Fork `sergey-scherbina/mlx-rs` @ rozum-hybrid-decode `4ec9bc86`;
  rozum (feature/mlx-hybrid-decode) Cargo pinned to it, reproducible git-rev build verified.
- [x] **moe-decode-dispatch** - ROOT-CAUSED & FIXED (**MoE decode 33 → ~83 t/s, 2.5×**).
  Added `mlx_export_to_dot` (mlx-c) + rust wrapper + a `ROZUM_DUMP_DOT` bench branch, and
  counted graph primitives per T=1 token: **Rust 4366 vs Python 2610**, with **1269 `AsType`
  (dtype casts) in Rust vs ~0 in Python**. Traced them: ~1000 fed QuantizedMatmul/GatherQMM,
  casting the bf16 quantized scales/biases up — because **the activation stream was f32**.
  Root cause: `GatedDeltaNet` (qwen3_5.rs) scaled q/k by `Array::from_f32(inv_scale)` — a
  STRONG f32 0-dim array → promoted the whole stream bf16→f32 at the 1st GDN layer (Python
  multiplies by a python float, stays bf16). Fix: scale by a scalar cast to q/k's dtype.
  Byte-exact (greedy IDs identical, all chat tests pass). Prims 4366→3307 (AsType 1269→210),
  decode 33→83, **prefill 943→~1200 (= Python 1180)**. Shared GDN → dense 27B too. Fork
  `8739cf72` (mlx-c `d71809d`), rozum `5dc7bd0`, reproducible git-rev build verified.
  The earlier "2.4× super-additive eval" was this: the f32 stream's extra casts scaled with
  graph size. FOLLOW-UP DONE: `rms_norm_no_weight` (null-weight fast kernel; mlx-c allowed it,
  mlx-rs wrapper didn't) replaced the per-call ones weight → prims 3307→3127 (Full 60→0),
  **MoE decode ~83→~88 t/s**, dense ~19→~19.6; byte-exact. Fork `0d4b3729`, rozum `4f31ecc`.
  Remaining tail: Rust 3127 vs Python 2610 — the leftover 150 AsType + extra Multiply/Broadcast
  are `compute_g` / gate f32 math that Python fuses via `mx.compile` (mlx-rs compile is
  net-negative). MoE ~88 vs Python 97/110; dense ~19.6 vs 23. Diminishing — needs hand-fused
  Metal kernels or a lower-overhead mlx-rs compile.
- [x] dense-decode-dispatch - RESOLVED end-to-end. The bf16 fix took the bench 16.2→~19, but
  the bench's own argmax/clone/eval loop UNDER-measures ~10-15%: through the real prod
  `backend.chat` (pipelined) **dense Qwen3.6-27B = 22.5 t/s = Python parity (23)**, MoE ~99-100
  (= Python). So effectively NO prod decode gap on either model. The synthetic engine-bench
  tail (88 vs 97) is the BACKLOG fusion item but doesn't show end-to-end. Verified by
  `mlx_dense_backend_chat_tps` / `mlx_moe_backend_chat_tps` (TTFT prefill-bound: dense ~2.5s,
  MoE ~1.2-1.7s/520tok; concurrency serialized via the single MLX worker thread).

Diagnostics on branch `feature/mlx-hybrid-decode`: Rust benches `mlx_qwen35_prefill_bench`
(dense) / `mlx_qwen35_moe_decode_bench` (MoE; `ROZUM_CTXSWEEP=1` env) — run with
`ROZUM_MLX_RETAIN=1`. Python: `docs/mlx-gd-bug/py/decode_bench.py <repo>` (`CTXSWEEP=1` env).
Per-block skip hatches (`ROZUM_SKIP_MOE`/`ROZUM_SKIP_ATTN` in qwen3_5_moe DecoderLayer) were
used for the localization above and reverted; re-add to reproduce.

**Don't block other work on this** — usable speeds today; this is parity-chasing.

**(historical goal) native MLX decode ~12 t/s → ~22 t/s (Python `mlx_lm` parity).**
Full analysis: `docs/specs/mlx-native-runtime.md` → "mx.compile" + the
"Performance — capture-based compile plan" section.

**The problem (root cause, settled).** Decode is **per-call op-launch / FFI-overhead
bound** — ~450 tiny matmul/conv dispatches per token at T=1; each is an FFI call +
lazy-graph build. NOT bandwidth-bound (~27 t/s ceiling), NOT missing fusion (rms_norm
/ rope / sdpa fast kernels already used). Python hits ~22 because `mx.compile` turns
the whole forward into ONE traced graph (weights captured, only token+cache crosses
FFI per step).

**The lever.** A **capture-based plain `compile`** of the decode step (`compile.rs:344`
marshals only `args`, captures weights into the trace — like Python), NOT
`compile_with_state` (re-marshals ~400 params/call → the net-negative my old
`mlx_compile_probe` measured). **Prereq: a fixed-shape KV cache** (preallocate to
max-ctx + in-place slice-update + offset; today's `ConcatKeyValueCache` grows by
concat → shape changes → recompile every token). **Risk:** must stay byte-exact; the
GatedDeltaNet custom kernel must stay OUT of the compiled region (compile + in-place
cache + buffer-donating kernel = the token-2 divergence hazard).

**Stages.**
- [x] **Stage 0 — probe (de-risk first). DONE — NO-GO signal (`sunny-civet`, 2026-06-22).**
  Ran the existing `mlx_compile_probe_plain` (`mlx_native_backend.rs:5076`, plain `compile`,
  weights captured) on Qwen3-0.6B-4bit: **compiled is SLOWER** — T=1 `0.69×` (26.6→38.4 ms),
  T=16 `0.58×` (19.7→33.9 ms). Plain-compile of the decode step does **not** pay off at this
  scale; matches the prior `compile_with_state` net-negative and [[project-mlx-hybrid-decode-gap]]
  ("compile doesn't win, batching was the lever"). **Decision: do NOT build Stage 1/2 on this
  premise.** Caveats (so it's a signal, not absolute): the probe uses the growing
  `ConcatKeyValueCache` (not yet the proposed fixed-shape cache) and 0.6B (not the 27B target);
  the *only* thing that could still flip it is a probe on 27B WITH the fixed-shape cache — but
  given two net-negative compile results, that is a deliberate, slot-heavy bet, not a default.
  Recommend keeping single-stream perf on the shipped batching lever.
- [ ] **Stage 1 — fixed-shape KV cache** (preallocate + in-place slice-update +
  offset). Byte-exact vs the current `ConcatKeyValueCache`.
- [ ] **Stage 2 — compiled decode step** (plain `compile`, weights captured, args =
  token+cache; custom kernel kept out / O(T) ops path at T=1). Byte-exact vs oracle.
- [ ] **Stage 3 — clean A/B on 27B**; target ~22 t/s.

**⚠ RESUME CHECKPOINT — read this first after any reboot, then continue.**
The machine has rebooted from memory pressure mid-experiment before; this block is the
single source of truth to pick up "as if nothing happened". Update it after every
meaningful step and commit (small, frequent commits — never hold uncommitted experiment
state).

**ACTIVE NOW (2026-06-12): MLX 0.31.2 bump for hybrid — NEGATIVE RESULT, settled.
Awaiting the Python-oracle decode number, then revert the bump.**

**⛔ THE BUMP DOES NOT FIX THE HYBRID. The premise was false.** The plan was:
bump MLX 0.30.6→0.31.2 → the GatedDeltaNet buffer-donation bug disappears → drop the
per-call eval → hybrid pipelines → ~22 t/s. Every link broke under test:

1. **0.31.2 does NOT fix the donation hazard.** Clean rebuild from scratch
   (`cargo clean -p mlx-sys --release` → 2m06s full C++ compile, fetched-src
   `git describe`=v0.31.2): `ROZUM_GD_NO_EVAL=1 mlx_qwen35_chat` → **garbage `)`,
   identical to 0.30.6.** eval-ON → `Here's a thinking process:` (byte-exact, as always).
   - ⚠ **STALE-BUILD MIRAGE LESSON:** an *incremental* 21s relink after the GIT_TAG bump
     showed no-eval = coherent `Here\n\n\n</think>` — a LIE. Incremental builds don't
     recompile `libmlx.a`; a 0.30.6/0.31.2 ABI mix gave lucky-looking garbage. **After
     any mlx-c/CMake version bump you MUST `cargo clean -p mlx-sys` before trusting
     runtime behavior.** The clean build is the only truth.
2. **The per-call eval is genuinely mandatory for the metal_kernel path** on both
   versions (the buffer-donation hazard is real and version-independent for our kernel).
3. **The pure-ops path (`ROZUM_GD_OPS=1`, `gated_delta_ops`) is donation-safe with ZERO
   eval** → correct output. So the bug is specific to the custom metal_kernel primitive,
   not the math. (mlx-rs `MetalKernel::new` uses `ensure_row_contiguous=true, atomic=false`
   — faithful to Python defaults, so the wrapper isn't the difference.)
4. **Pipelining does NOT help hybrid — it's compute-bound, not readback-bound.** Bench
   on 27B-4bit (`mlx_qwen35_prefill_bench`):
   ```
                       decode serial   pipelined     prefill
     kernel+eval n=128     12.3          14.3       119 tok/s
     kernel+eval n=1024    13.3          11.1       147 tok/s   (pipeline HURTS)
     ops no-eval n=128     15.0          16.3        92 tok/s
     ops no-eval n=1024    13.1          12.2        85 tok/s   (pipeline HURTS)
   ```
   The dense pipelining win (96.5% of Python) came from filling the per-token readback
   bubble; the hybrid forward (GatedDeltaNet recurrence × 48 layers) has **no idle-GPU
   bubble** → nothing to fill. Dropping the eval (ops path) buys ~20% at short context
   and **vanishes/reverses by n=1024** — exactly the contexts a coding agent runs at.
   And ops wrecks prefill (85 vs 147 tok/s). **Hybrid decode ≈ 13 t/s regardless of
   eval / pipelining / MLX version.**

**CONCLUSION on the bump:** the 0.31.2 bump buys nothing for the hybrid goal and carries
the fft.cpp-disable + ops.cpp `global_scale`-nullopt patches as pure maintenance debt.
**Revert it.** Keep **kernel+eval** for hybrid (correct; best prefill).

**⭐ THE REAL GAP IS FOUND — and it is NOT the bump. Python is ~1.8× faster.** Oracle
measured (2026-06-12, `/tmp/mlxoracle`, mlx 0.31.2 + mlx_lm 0.31.3, same 27B-4bit):
```
  Python mlx_lm  decode = 23.1 tok/s   (prompt 32 t/s, peak 15.4 GB)
  our kernel+eval decode ≈ 12–13 tok/s        → ~1.8× gap, REAL
```
So 13 t/s is NOT the ceiling. Ruled out as the cause: eval (our ops no-eval path = ~15,
still far from 23), pipelining (hurts us), MLX version (both on 0.31.2).

**THE CRUX (next agent: start here).** Python runs the **same `gated_delta` metal_kernel
with NO per-call eval** and is correct; ours needs the 48-syncs/token eval or it's garbage
`)`. That eval is ~the 1.8×. **Why does our identical kernel need the eval when Python's
doesn't?** Python source read (`/private/tmp/mlxoracle/.../mlx_lm/models/`):
- `gated_delta.py:171 gated_delta_kernel` — builds the kernel output, **returns lazy, no
  eval**. Same `GATED_DELTA_SOURCE`, same wrapper (`ensure_row_contiguous=true,atomic=false`).
- `qwen3_5.py:183-197` — `out, state = gated_delta_update(...)`; `cache[1] = state` (stores
  the **lazy** state); `cache.advance(S)`. Uses `ArraysCache` (`cache.py`), NOT our
  `ConcatKeyValueCache`. Conv state: `cache[0] = mx.contiguous(conv_input[:, -n_keep:, :])`
  (note the explicit `mx.contiguous`, qwen3_5.py:166).
**Concrete hypotheses to test (cheapest first, all small-model-probeable):**
  1. **`T` passed as Array vs scalar.** Ours (`gated_delta.rs:226`) passes
     `&t_scalar = Array::from_int(t)` — an MLX **Array** (runtime buffer). Python passes a
     raw **int** `T` → MLX templates it as a constant. mlx-rs `MetalKernel::apply` only
     takes `&[impl AsRef<Array>]` (no scalar path) → forces T to a buffer → possibly
     different/garbage codegen only masked by the eval. **Check if mlx-rs/mlx-c exposes a
     scalar-input path for metal_kernel; if not, add one.** Most promising lead.
  2. **State contiguity / dtype.** Python stores a contiguous state; ours may hand the
     kernel a non-contiguous cache slice that aliases under donation. Try
     `state.contiguous()` (or `+0`) before/after the kernel WITHOUT eval.
  3. **Cache structure.** Port Python's `ArraysCache` (lazy-state store + `advance`) for
     hybrid instead of `ConcatKeyValueCache`; the in-place store may keep the state
     buffer concrete across later layers.
- **If kernel-no-eval is made correct → expect ~23 t/s (kernel speed + no syncs).** That's
  the whole prize. Validate byte-exact greedy vs the oracle each step.
- **Oracle env stays at `/tmp/mlxoracle`** (reuse it; rerun
  `HF_HUB_OFFLINE=1 python -m mlx_lm generate --model mlx-community/Qwen3.6-27B-4bit
  --prompt "..." --max-tokens 64 --temp 0` for fresh numbers). 27B run = memory-heavy,
  ~15.4 GB peak; gate on `memory_pressure` (was 90% free), one run at a time.

**Repo state (NOT merged):** rozum worktree `feature/mlx-031-bump` (Cargo.toml = path-deps
to fork); fork `.vendor/mlx-lm` branch `mlx-0.31.2-bump` (GIT_TAG v0.31.2 @ CMakeLists.txt:38;
fft.cpp disabled @:55; ops.cpp 3 quantize calls patched w/ `std::nullopt` global_scale;
gated_delta.rs:252 `ROZUM_GD_NO_EVAL` gate). 0.30.6 baseline = prior fork branch
`rozum-mlx-native` @ d62049c9. **The lib is currently built clean against 0.31.2.**
**To revert the bump:** point rozum's Cargo.toml/git-pin back at `rozum-mlx-native`
(0.30.6), drop the `feature/mlx-031-bump` + `mlx-0.31.2-bump` branches (or keep them
parked, documented as a closed negative experiment).

- **Memory discipline (this is what caused the reboots):** probe/iterate on the
  SMALLEST model (`mlx-community:Qwen3-0.6B-4bit`, cached), NOT 27B. Check
  `memory_pressure` before any model load; if free < ~25%, stop and free first. Only
  use 27B for a final A/B.
- **DENSE decode SOLVED & merged (pipelining):** `stream_generation` pipelines (build +
  `async_eval` the next token before blocking on the current) for dense arches
  (Qwen3, Qwen3-MoE, Llama, Qwen2 — `pipeline=true`); hybrid (Qwen3.6) keeps the serial
  path (`pipeline=false`) — **and per the bench above, that is correct: pipelining hurts
  hybrid, so do NOT flip the hybrid arms.** Probe `mlx_decode_pipeline_probe`: 4B
  **114→128 t/s = 96.5% of Python's 132.9**.
- **(historical) compile + cache levers — both ruled out.**
- **Results so far (2026-06-12):**
  - `mlx_compile_probe_plain` on Qwen3-0.6B-4bit (thread-local model + plain
    `compile`, weights captured, only token marshaled):
    `T=1 uncompiled 3.137ms vs compiled 4.541ms (0.69×); T=16 1.00×`.
    → **Plain `compile` is ALSO not a win.** So the decode gap is NOT a compile
    problem. (Makes sense: compile fuses elementwise glue, NOT the matmul GEMMs that
    dominate a transformer's ~450 dispatches.) **This rules out the fixed-cache +
    compiled-decode redesign as the lever — big save.**
  - **RE-PROBED 2026-06-14 on a 7× bigger model (Qwen3-4B-4bit) — still net-NEGATIVE:**
    `T=1 uncompiled 9.633ms vs compiled 15.053ms (0.64×); T=16 0.92×`. The hypothesis
    "compile wins once build is expensive" is **refuted**: the slowdown does NOT shrink
    with model size — mlx-rs's `compile` *adds* per-call overhead that exceeds the build
    it saves, independent of build cost. So the Python `mx.compile` win (token+cache only
    crossing FFI, trace reused) does **not** translate to the mlx-rs binding; getting it
    would require fixing mlx-rs `compile` / calling mlx-c compile directly (deep, uncertain
    upstream work). The post-bf16-fix MoE decode is build/FFI-bound (92%), but that build
    is **not reducible via mlx-rs compile** — `mlx-native-perf-compile` is a confirmed
    dead end for now. (Note: the original "eval=92% GPU" framing was pre-bf16-fix; post-fix
    the split flips to build=92% — but compile still can't capture it.)
  - The probe used a FRESH cache, so it didn't measure the cache cost. Confirmed
    `ConcatKeyValueCache::update_and_fetch` does `concatenate_axis` EVERY step
    (`cache.rs:95`) → O(n) realloc+copy per token, vs Python's preallocated in-place
    KVCache. BUT the old bench was ~flat across 128/512/1024 context (~13/12/12 t/s),
    which argues concat is NOT the dominant cost at ≤1024 either.
- **LEVER FOUND = PIPELINING (`async_eval`).** Read `mlx_lm/generate.py:455-470`:
  Python builds step n+1's graph from the *lazy* token n and `mx.async_eval`s it
  BEFORE `y.item()` on token n — GPU never idles waiting for the CPU to build the next
  graph. Our `stream_generation` (line ~570) does `eval`+`item` (blocking) THEN builds
  the next step → a sync bubble every token.
  - `mlx_decode_pipeline_probe` on Qwen3-4B-4bit: **serial 114.2 t/s, pipelined
    128.3 t/s (1.12×)**. Python `mlx_lm` same model = **132.9 t/s**. So pipelined =
    **96.5% of Python** (serial was 86%). **Pipelining closes the gap.** Win scales
    with how much CPU graph-build hides behind GPU compute → bigger on 27B (more
    layers, where the serial gap was 12 vs 22 = 55%).
- **Next concrete step:** implement pipelining in the REAL decode loop
  (`stream_generation`): hold the current lazy token, build+`async_eval` the next
  before reading the current. Then (a) byte-exact check the dense e2e tests
  (`mlx_chat_capital_of_france`, `mlx_qwen2_chat`, `mlx_llama_chat`) — tokens must be
  identical; (b) **carefully** test the HYBRID path (qwen3_5 27B) — the GatedDeltaNet
  custom kernel needs a per-call `eval` (buffer-donation hazard); async_eval deferral
  may re-trigger the token-2 garbage. If hybrid breaks, gate pipelining to non-kernel
  (dense) arches, or force the kernel's state eval inside the loop.
- mx.compile + fixed-cache redesign: **shelved** (probe showed compile is not the
  lever; the cache concat is ~flat across context). Pipelining is simpler and wins.

#### P0 (current): mlx-native-runtime — pure-Rust native MLX runtime

Run MLX-community checkpoints through a **full native MLX forward** (no candle,
no Python, no subprocess) built on the upstream `oxideai/mlx-lm` Rust crate
(scaffolding + Qwen3 dense + Llama already done). Gives MLX's real wins (fusion,
no cross-runtime sync, day-one architectures) in one binary, and **retires
`mlx_lm.server`**. Supersedes both `mistralrs-mlx-direct` (bridge = structural
perf dead end) and the from-scratch `mlx-native-port`.

Spec: `docs/specs/mlx-native-runtime.md`. Branch: `feature/mlx-native`.
Decisions locked: vendor-fork `.vendor/mlx-lm` · broad catalog · top-of-chain
(retire mlx_lm.server) · build on the crate, port only missing models · forward
is 100% MLX, candle only as external oracle.

- [x] mlx-native-p0 - Phase 0 dense: **DONE** (Qwen3-4B-4bit correct + fast).
  - **AFQ load fixed** (3 upstream gaps: config quantization, single-file,
    `.inner.weight` key remap) -> 904/904 params load.
  - **Forward bug #1 FIXED** (`1bbe6e52`): dead KV cache (slots init'd None ->
    decode ran cache-less, repetition). Fix: `Some(C::default())`.
  - **Forward bug #2 FIXED**: mlx-rs `nn::Rope::forward` reshaped to 3D
    `[-1, L, head_dim]`; for decode (L=1) the `[B*n_heads, 1, head_dim]` shape
    trips an MLX fast-rope bug rotating only head 0, leaving later heads
    un-rotated -> garbage. Fix: RoPE on the 4D shape directly (like Python).
    **NOT the cause** (each cost real time): MLX version (0.31.2 reproduces it),
    mask, sinks, layout, device, SDPA. The 0.31.2 bump was done (mlx-c fft/ops
    patched to build) but didn't fix it -> reverted to 0.30.6 (rope fix is
    version-independent, keeps the submodule unpatched).
  - **Result:** Qwen3-4B-4bit byte-identical to mlx_lm ("The capital of France
    is Paris"), **~106 T/s** (> candle ~100, ~10x bridge). Native-MLX thesis
    fully proven (fast AND correct).
- [x] mlx-native-p0b - `MlxNativeBackend` wired into the gateway (`b25497c`).
  - MLX is `!Send` (one Metal stream) -> a dedicated worker thread owns the
    model for life, loads it itself, serves jobs off a channel, streams
    `ChatEvent`s back; the backend is a thin Send+Sync handle. Chat-template
    render (system/user/assistant/tool), EOS/max-tokens/cancel stop, UTF-8-safe
    incremental detokenize (holds a trailing replacement char so mid-Cyrillic
    never leaks). `concurrency_capacity()=1` -> `admit_wrap` gates it.
  - `mlx-native` feature (off by default) + path deps on the vendored fork
    (swap to a git-rev pin at merge, like mistralrs). `build_gateway_backend`
    tries it before mistralrs for MLX checkpoints.
  - E2E test through the real SPI: streams a correct "Paris" answer in ~3.7s
    incl. load. `cargo check --features mlx-native` + fmt + lib suite clean.
  - Merged `origin/master` (the generic `concurrency` admission decorator +
    shared-gateway) into the branch; clean.
  - Gaps still open: hf-hub auto-download; sampler top_p/top_k/rep-penalty
    (Generate only takes temp today); tool-use streaming; EOS list from config.
- [x] mlx-native-p1 - Phase 1: port `qwen3_moe`; gated on `Qwen3-30B-A3B-4bit`.
  - Dense Qwen3 attention reused verbatim; sparse MoE MLP = router gate
    (quantized Linear) -> softmax -> argpartition top-8 -> `take_along_axis`
    scores -> `SwitchGLU` experts via `gather_qmm` -> weighted sum. Experts are
    AFQ 3D `[E,out,in]` raw `Param<Array>`; target-aware load remap adds
    `.inner.weight` only where that param exists (QuantizedLinear leaves) so the
    experts keep `.weight`. Token-sort skipped (gather_qmm identical sorted/not).
  - **Greedy byte-for-byte identical to Python `mlx_lm`** on Qwen3-30B-A3B-4bit:
    `<think>\n\n</think>\n\nThe capital of France is Paris.` Loads 1351 params,
    full load+gen in ~4.6s. Backend dispatches qwen3/qwen3_moe by `model_type`
    via a `LoadedModel` enum + shared generic streaming loop. (Downloaded the
    gate model, ~17GB.) E2E test `mlx_moe_chat_capital`.
  - All 48 layers sparse (mlp_only=[]); dense MoE layers fail loud for now.
- [x] mlx-native-p2 - Phase 2: port `qwen3_5` (27B dense) + `qwen3_5_moe`
  (35B-A3B) hybrid; gate on cached `Qwen3.6-{27B,35B-A3B}-4bit`. Headline: the
  models the user runs, pure-Rust. Qwen3-Next family,
  the hard phase — COMPLETE. Scope mapped from Python `qwen3_5`/`qwen3_next`/`gated_delta`:
  - **DONE (fork `364cebf6`):** the GatedDeltaNet delta-rule recurrence
    (`models/gated_delta.rs`, ops path — mlx-rs has no custom-kernel support, so
    O(T) prefill but byte-exact). Unit-test validated vs Python `gated_delta_ops`
    (<1e-3 on a seed-0 case). compute_g + delta_step + sequential scan.
  - **Phase 2a DONE — `qwen3_5` (Qwen3.6-27B) byte-exact** (fork `9df1dd15`,
    rozum `b39a49c`). `models/qwen3_5.rs`: output-gated full attention (every 4th
    layer; q_proj->queries+gate, `o_proj(out*sigmoid(gate))`, partial RoPE
    rotary_dim=head_dim*0.25 — mRoPE keys ignored for text, confirmed correct) +
    GatedDeltaNet linear layers (depthwise `Conv1d` + causal conv-state cache +
    the f32 delta scan) + heterogeneous `LayerCache::{Full(KV), Linear{conv,state}}`
    + RMSNormGated + weightless q/k rms_norm. Backend `Qwen35` arm; jinja template
    fallback; sharded-no-index load; `language_model.` prefix strip (skip vision
    tower); config under `text_config` (rope from nested `rope_parameters`).
    **Bugs found+fixed during bring-up** (localized via per-layer L2 dumps vs a
    Python oracle): the mlx-community 4bit checkpoint is already sanitized so the
    RMSNorm +1 must NOT be re-applied (was doubling norms -> 6x blowup); the delta
    recurrence must run in f32 not bf16 (greedy drift); and the `A_log` param key
    is capitalized (was loading as ones -> wrong decay). Greedy output identical
    to Python mlx_lm: "Here's a thinking process:" (per-layer L2 to ~0.1%). E2E
    test `mlx_qwen35_chat`.
  - **Phase 2b DONE — `qwen3_5_moe` (Qwen3.6-35B-A3B)** (fork `f27ddc42`, rozum
    `223fd69`). Reuses the qwen3_5 backbone verbatim (attention + GatedDeltaNet +
    LayerCache, made pub) + the qwen3_moe SwitchGLU (made pub); every layer's MLP
    is a sparse MoE block = router gate + top-k SwitchGLU + a shared expert gated
    by `sigmoid(shared_expert_gate(x))`. **Per-module quant**: the router gate and
    shared_expert_gate are 8-bit (rest 4-bit) and nn::quantize is uniform-only, so
    those two are raw `QuantLinear` (quantized_matmul) outside nn::quantize; the
    4-bit experts stay raw SwitchGLU; the rest go through nn::quantize at 4-bit.
    `intermediate_size` optional (pure-MoE omits it). Greedy output matches Python
    mlx_lm: "Thinking Process:" — worked on the first forward run (only two config
    fixes needed). E2E test `mlx_qwen35_moe_chat`. **Phase 2 COMPLETE.**
#### mlx-native catalog + UX (user request 2026-06-12) — broaden models + auto-download

- [x] mlx-native-autodownload - **DONE (`feature/mlx-autodownload`, rozum-only).**
  Native MLX now auto-fetches an MLX snapshot from HuggingFace when not cached. New
  `src/hf_hub.rs`: `ensure_snapshot(repo, config_gate)` — lists files via the HF API
  (`…/api/models/<repo>` → `sha` + `siblings`), downloads into the HF cache layout
  (`~/.cache/huggingface/hub/models--<org>--<name>/snapshots/<sha>/`) via `reqwest`
  streaming (no `hf-hub` crate). **`config.json` fetched FIRST + passed to a gate** so an
  unsupported `model_type` is rejected before the multi-GB weights. **Live progress
  line** per file (`[i/N] <file>  <done>/<total> (NN%)`, throttled ~4 MiB, `\r`+clear).
  Honors `HF_TOKEN`. `mlx_native_backend::ensure_model_dir(spec)` = cached-or-download,
  wired into `try_build_mlx_native_backend` (replaces the cache-only `resolve_model_dir`);
  gate = `supported_model_type` (matches `LoadedModel::load`, checks top-level +
  `text_config.model_type`). Validated: rejection path (config-only) + full ~0.4 GB
  download of `Qwen3-0.6B-4bit` (8 files, progress 0→100%, loadable snapshot). Tests
  `wanted_*` + ignored network `config_first_gate_*` / `full_download_*`.

- [x] mlx-native-hub-cache - **DONE (`feature/mlx-hub-cache`, rozum-only).** Two fixes
  to auto-download so it shares the cache with the native tools (no double-downloads):
  (1) **Proper `huggingface_hub` layout.** Old code wrote real files into
  `snapshots/<sha>/` (so Python re-downloaded the same model into the proper layout →
  wasted disk). Now `hf_hub.rs` writes the exact hf_hub layout — `blobs/<oid|sha256>`
  (etag from the tree API: `oid` for regular, `lfs.oid`=sha256 for LFS), `snapshots/
  <commit>/<path>` → `../../blobs/<oid>` symlinks, `refs/main`; an existing blob (from
  either tool) is reused, not re-fetched. **Proven bidirectional**: `huggingface_hub.
  try_to_load_from_cache` + `mlx_lm.load` work fully OFFLINE against a rozum-written
  cache. (2) **ModelScope source** (`src/modelscope.rs`, spec `modelscope:<owner>/<repo>`)
  — the other hub carrying MLX (Qwen-heavy, CN). Lists via `…/api/v1/models/<repo>/repo/
  files`, downloads flat into `$MODELSCOPE_CACHE/hub/<owner>/<name>/` (ModelScope's own
  layout), same config-first gate + progress (shared `stream_download`/`wanted`).
  `ensure_model_dir`/`resolve_model_dir` dispatch by `modelscope:` prefix. Validated e2e:
  `mlx_modelscope_chat` downloaded `mlx-community/Qwen2.5-0.5B-Instruct-4bit` FROM
  modelscope.cn + loaded + generated. **Disk cleanup done**: deleted 3 old wrong-layout
  dirs (~964 MB; Llama-1B / Qwen2.5-0.5B / a config-only orphan), re-scan confirms only
  correct-layout remains, and a re-download lands in proper layout + loads offline.

- [x] mlx-native-llama - **DONE (`feature/mlx-llama`, fork `b46aee6f`).** Llama family
  for native MLX. Fork `llama.rs`: (1) **AFQ-quant loader** — `load_llama_model` now
  mirrors `load_qwen3_model` (read `quantization` → `nn::quantize` → remap
  `<p>.weight → <p>.inner.weight` where a `.scales` sibling exists); `ModelArgs` gained
  `quantization`. (2) **sampler** — `Generate` reuses qwen3's model-agnostic
  `SamplerOpts`/`sample_with`/`repeat_window` (imported, not duplicated) + history +
  `set_sampler`. rozum: `LoadedModel::Llama` + `load` arm (`model_type` "llama") +
  dispatch like `Qwen3` (external cache + `set_sampler`); `supported_model_type` += llama.
  **Validated greedy byte-exact vs Python `mlx_lm`** on Llama-3.2-1B-Instruct-4bit:
  both emit "The capital of France is Paris." E2E test `mlx_llama_chat` (auto-downloads).

- [x] mlx-native-qwen2 - **DONE (`feature/mlx-llama`, fork `d62049c9`).** Qwen2 / Qwen2.5
  (incl. Qwen2.5-Coder) for native MLX. New fork `models/qwen2.rs` (adapted from
  `llama.rs`): dense transformer with the Qwen2 bias convention — **q/k/v carry a bias,
  o_proj does not** (independent of `attention_bias`, which Qwen2 omits); reuses the
  AFQ-quant loader + qwen3 sampler. Two Qwen2-specific fixes over the llama base: (1)
  `head_dim` optional → filled from `hidden_size / num_attention_heads`; (2) the weight
  remap also maps **`.bias → .inner.bias`** for quantized layers (QuantizedLinear nests
  the linear bias under `inner`; Qwen2 has q/k/v biases, so without this they stay at
  init → garbage; `.biases` quant zero-points left alone). rozum: `LoadedModel::Qwen2` +
  `load` arm + dispatch + `supported_model_type` += qwen2. **Validated greedy byte-exact
  vs Python `mlx_lm`** on Qwen2.5-0.5B-Instruct-4bit. Unlocks `Qwen2.5-Coder-32B-Instruct-4bit`
  (in `RECOMMENDED`). E2E test `mlx_qwen2_chat`.

##### Catalog — quick & cheap wins (do next; each = small port + oracle sweep)

- [x] mlx-native-mistral - **DONE 2026-06-14 — the one-line alias, as predicted.** Mistral /
  Mistral-Nemo (`model_type: "mistral"`) is architecturally Llama (GQA, no qkv-bias, SwiGLU,
  RoPE) and upstream `mlx_lm` serves it with the *llama* model class, so `LoadedModel::load`
  now routes `"llama" | "mistral" => llama::load_llama_model` and `supported_model_type` admits
  `"mistral"` — NO new fork file. Guards: `mistral_is_a_supported_model_type` + `mistral` added
  to the dense/unretained classification test (fast suite, 130/0). Caveat as noted: Mistral's
  sliding-window attention (4096) is approximated by the llama path's full attention, so it
  matches the reference except beyond the window — fine for agents, bounded by the KV preflight;
  if a short-context divergence ever shows, fall back to a thin `mistral.rs`. **VALIDATED
  end-to-end** (`mlx_mistral_chat`, Mistral-7B-Instruct-v0.3-4bit): *"Paris is the capital of
  France."* The run surfaced two general config quirks (NOT in the alias itself), both fixed in the
  fork: (1) Mistral's `config.json` omits `head_dim` → made it `Option` in `llama::ModelArgs`,
  default `hidden_size/num_attention_heads` (fork `1f5475a1`); (2) Mistral ships `chat_template` as
  the older list-of-`{name,template}` form → `load_model_chat_template_from_str` now parses both the
  string and list forms, picking the `"default"` entry (fork `3f230b2a`, + unit test, + fixed
  pre-existing broken `mlx-lm-utils` tests). Added to `models::RECOMMENDED`. Opens Mistral / Nemo
  (and the Mixtral base) — and these two fixes broaden the whole Llama family's config tolerance.

- [x] mlx-native-llama-aliases - **DONE 2026-06-14.** Verified `mlx-community:SmolLM2-1.7B-Instruct`
  (a non-Llama-3 `model_type: "llama"` model) runs on the existing llama path: *"The capital of
  France is Paris."* (`mlx_smollm_chat`). Confirms the alias works for the wider Llama family (and
  the `head_dim` config-tolerance fix held for it). Added to `models::RECOMMENDED`. No code beyond
  the RECOMMENDED entry + test.

- [x] mlx-native-fp16-verify - **DONE 2026-06-14 (same model).** `SmolLM2-1.7B-Instruct` is a
  **non-quantized bf16** checkpoint, so loading it exercises the AFQ loader's `quantization = None`
  branch — confirmed loads + generates correctly (`mlx_smollm_chat`). bf16/fp16 MLX checkpoints
  work as-is (just more RAM). No code.

- [x] mlx-native-p4 - Phase 4: native MLX is the DEFAULT backend; `mlx_lm.server`
  retired (rozum `74b458a`). `default = ["mlx-native"]` (was `["mistralrs"]`);
  mistralrs is now opt-in `--features mistralrs` (broader-catalog candle fallback,
  still tried after native MLX). Removed `try_mlx_server` + its chain step; the
  in-process native runtime supersedes the Python server. Chain is now GGUF ->
  native MLX -> mistralrs (opt-in) -> LM Studio HTTP -> ROZUM_BACKEND_URL. SPEC.md
  resolution chain + no-backend hints + select-failed note updated. Default and
  `--features mistralrs` both build clean. **Open: reproducibility** — mlx-native
  still uses path deps into the gitignored `.vendor/`; merge-to-master must push
  the fork (`sergey-scherbina/mlx-rs` branch `rozum-mlx-native`) and switch to a
  git-rev pin (like the mistralrs `[patch.crates-io]`) so the default builds
  off-tree.
- [x] mlx-native-perf - Phase 5: throughput. Spec section: `docs/specs/mlx-native-runtime.md`
  "Performance". **RESOLVED + SHIPPED + single-stream MAXED (2026-06-13).** Decode root-caused &
  fixed (bf16 stream leak in GatedDeltaNet q/k scaling → ~1000 casts/token): MoE decode 33→~88 t/s
  (2.7×), prefill →1215 (=Python), dense 16→~19.6 (~90% of Python); byte-exact; on master. The
  "capture-based plain-`compile`" lever below was the open hypothesis — since **REFUTED** (MLX
  auto-fuses; bottleneck is 92% CPU build/FFI, not fusion), so **batching is the only further
  lever** and BOTH dense (1.98×) AND hybrid (2.30×, byte-exact) batched decode are now **shipped**
  too. Nothing left on single-stream. Detail: [[project-mlx-hybrid-decode-gap]]; the historical
  investigation is preserved below for the record.

  **STATUS (2026-06-11) — perf territory mapped; prefill wins shipped; one decode
  lever identified (capture-based compile), gated on a fixed-shape cache.** Where
  things stand and what to do next:
  - DONE on master: GatedDeltaNet prefill kernel (~2.9x), chunked prefill +
    last-position projection (bound the large-prompt peak, byte-identical),
    decode-bug resolved (per-call eval is correct + FREE), fused causal SDPA,
    sampling (top_p/top_k/seed/repeat_penalty), cancellation, multi-eos, tool-use +
    multi-turn tool history, KV preflight.
  - DEAD END (measured, don't retry): removing the per-call eval (no-op, decode isn't
    sync-bound); `compile_with_state` (net-negative — re-marshals ~400 params/call).
  - **CORRECTION (2026-06-11):** the "mx.compile is a dead end" finding was scoped to
    `compile_with_state` only. Plain `compile` (`compile.rs:344`) marshals **only the
    args** and captures referenced weights into the trace — exactly how Python
    `mlx_lm` reaches ~22 t/s vs our ~12. This **capture-based plain-`compile`d decode
    step was never probed** and is the one untested lever with ~2× potential.
  - NEXT (recommended order):
    1. `mlx-native-mem-bound` — DONE (KV preflight + "lower --n-ctx" instead of OOM).
    2. SDPA `Causal` mode in prefill — DONE (fork `ef5cbca9`): fused causal SDPA,
       byte-identical (oracle + chunked Δ=0), drops the O(T·ctx) mask allocation.
    3. `mlx-native-perf-compile` (NEW, top remaining perf item) — capture-based
       plain-`compile`d decode step. **Prereq:** fixed-shape KV cache (preallocate +
       in-place slice-update; today's `ConcatKeyValueCache` grows by concat and
       defeats compile). Correctness-critical (must stay byte-exact) and intersects
       the GatedDeltaNet buffer-donation hazard. **Needs a clean machine to A/B** —
       current numbers degraded ~30% by session memory pressure. Do as a dedicated
       session, not a tail-end change.
    4. (FALLBACK, larger) hand-written fused Metal kernels to cut ~450 dispatches/token.

  - **DONE — GatedDeltaNet Metal kernel (~2.9x Qwen3.6 prefill)** (master `a001e90`,
    fork `738a4419`). Bound `mx.fast.metal_kernel` in mlx-rs (`fast::MetalKernel`)
    + ported the Python gated-delta kernel: the whole T-step scan in one GPU
    dispatch. 27B 1024-tok prefill 20.9s->7.1s; greedy still byte-exact on 27B +
    35B-A3B. Caveat: the custom kernel needs a BLOCKING `eval` per call (see the
    bug below), so each call syncs.
  - **DONE — decode bug dig (`mlx-native-decode-bug`): RESOLVED + the eval is FREE.**
    Root cause of the "needs a blocking eval per call" rule: the custom-kernel
    primitive's `state_out` is a lazy buffer that the ~60 intervening layers of the
    forward can donate/reuse before it is materialized, silently corrupting the
    recurrent state (decode diverges at the *second* token: prefill's first token
    is correct, then the carried state is wrong). The per-call `eval` fixes it by
    forcing `state_out` concrete immediately. Confirmed by a 64-deep chained-kernel
    repro: a recurrent kernel chain is correct deferred when **nothing heavy runs
    between the calls**, and only corrupts inside the large model graph — i.e. it is
    a buffer-donation hazard, not a binding bug. (The earlier `async_eval` "garbage"
    was MLX's single default stream racing the next step on a second thread, a
    separate concurrency artifact — the real worker is single-threaded.)
  - **KEY FINDING — the per-call eval is NOT the decode bottleneck.** A/B on the
    27B bench (decode tok/s): per-call eval ON 13.0 / 7.7 / 12.2 vs OFF 16.1 / 11.2
    / 11.2 at n=128/512/1024 — overlapping noise, no gain. Removing the 48 GPU
    syncs/token does nothing measurable. Decode (~12 t/s vs Python ~22) is bound by
    raw op-launch overhead across the 64-layer forward (~450 tiny matmul/conv
    dispatches/token at T=1), the SAME whether eval'd per-call or once. **So the
    per-call eval stays (correct + free); the real lever is op fusion, below.** No
    code change shipped from this dig — the existing per-call eval is already
    optimal.
  - [x] **mx.compile via `compile_with_state`** (`mlx-native-compile`) — net-negative.
    Probe `mlx_compile_probe` (dense Qwen3-4B, fixed shapes): T=1 uncompiled 8.79ms
    vs compiled 17.34ms (**0.51x**), T=16 0.85x. mlx-rs `compile_with_state`
    re-marshals the whole `Updatable` state per call (flattens + SORTS ~400 params)
    + `mlx_detail_compile` per call -> binding overhead > fusion benefit. **NOTE
    (correction):** this rules out the `compile_with_state` API only. Plain `compile`
    marshals just the args (weights captured into the trace) — see
    `mlx-native-perf-compile` above; that variant is the real lever and was NOT
    probed. The fixed-shape-KV-cache redesign (its prereq) is therefore NOT moot.
- [x] mlx-native-chunked-prefill - DONE. `Model::prefill` (qwen3_5 + qwen3_5_moe)
  processes the prompt in chunks of `ROZUM_MLX_PREFILL_CHUNK` (default 2048), so the
  full-attention layers bound their `[chunk, ctx]` causal-mask + SDPA peak instead
  of `[T, T]` (the explicit causal mask `linds.ge(rinds)` is the O(T^2) allocation;
  the fused SDPA tiles but still reads it). Caches advance across chunks and are
  eval'd between them (`LayerCache::collect_eval`) to free each chunk's activations
  and keep the deferred graph from spanning the prompt; GatedDeltaNet is already
  O(1) memory. Returns only the last-position logits. **Byte-identical to single
  pass** (the per-position attention + sequential delta scan are position-local):
  test `mlx_qwen35_chunked_prefill_matches_single_pass` on a 3000-tok prompt gives
  `max|Δlogit|=0.000e0` (chunk 512 vs single-pass). Last-position-only `lm_head`
  (`Model::project`): DONE (fork `932967d6`) — avoids the `[1,chunk,vocab]` ~600MB
  logits transient per chunk + the wasted vocab matmul on discarded positions, still
  Δ=0. SDPA `Causal` mode (fork `ef5cbca9`): DONE — the explicit `[chunk,ctx]` mask
  array is gone too (MLX fused causal SDPA, handles the cache offset), still Δ=0.
- [x] mlx-native-mem-bound - **DONE (preflight).** `run_job` estimates the KV
  footprint of the request — `kv_bytes_per_position * (prompt_len + max_tokens)`,
  where `kv_bytes_per_position = 2 (k+v) * full_attn_layers * n_kv_heads * head_dim *
  2 (bf16)` from `config.json` (`text_config` for the hybrid wrapper; only
  full-attention layers hold KV — `full_attention_interval` selects them — the
  GatedDeltaNet conv/recurrent state is O(1)). If it exceeds `KV_SAFETY_FRAC=0.75`
  of `available_ram_bytes()` (vm_stat) it returns a clear `ModelError` ("context too
  large … lower --n-ctx / max_tokens … fits ~N tokens now") instead of letting Metal
  OOM. Skipped when either term is unknown (no false negatives). Unit test
  `kv_bytes_per_position_estimate` (hybrid + dense + missing-fields). FOLLOW-UP: a
  bounded/rotating KV cache to actually cap resident KV for very long sessions — and
  it now doubles as the prereq for `mlx-native-perf-compile` (a fixed-shape cache is
  what lets plain `compile` reuse the decode graph). Worth doing for either reason.
  (Concurrency/admission is already generic via `admit_wrap`;
  `concurrency_capacity()=1` for native.)

#### Native MLX — backend feature parity (vs mistralrs)

Audit 2026-06-11 (`docs/specs/mlx-native-runtime.md` "Backend feature parity"): the
native backend had correctness + the prefill memory work; the request-handling gaps
vs mistralrs are now mostly closed. Status:

- [x] mlx-native-cancel-prefill - **DONE** (fork `fb263995` + rozum `b022dc4`). The
  hybrid `Generate` gains a `should_cancel` predicate polled between prefill chunks
  (`prefill_cancellable` -> `Ok(None)` ends the run); rozum wires it to `job.cancel`,
  so a cancel/disconnect on a long prompt is honored DURING prefill, not only
  per-token after it. Closes the native-side analog of the mistralrs large-prompt
  stall (an abandoned long request no longer blocks the cap-1 worker). Test
  `mlx_qwen35_prefill_cancels_mid_prefill` (bails at chunk 3 of ~6, deterministic).
- [x] mlx-native-sampling - **DONE — top_p/top_k/seed + repeat_penalty** (fork
  `f36c8c3a`/`e970b23a` + rozum `510c760`/`3597abe`). `sample_with(SamplerOpts)`
  ported from mlx_lm, threaded through all four `Generate`; greedy (temp 0) stays
  argmax (oracle byte-exact). seed -> MLX RNG. `repeat_penalty` (HF convention)
  applies over a `REPEAT_CONTEXT=256` token window via `take_along_axis` /
  `put_along_axis` (O(window)); each `Generate` keeps a token history (only when
  penalty != 1.0, so the greedy path is untouched + skips the per-token id eval).
  Unit test pins top_k=1/tiny-top_p == argmax AND that a hard penalty moves the argmax.
- [x] mlx-native-multi-eos - **DONE** (rozum `b022dc4`). `read_config` collects the
  full `eos_token_id` set; `stream_generation` stops on any.
- [x] mlx-native-tool-use - **DONE** (fork `1fc66029`/`e316dbf7` + rozum `09dfbcc`).
  `mlx-lm-utils` `ApplyChatTemplateArgs` gained a `tools` field threaded into the
  minijinja context (+ enabled minijinja's `json` feature for the `tojson` filter
  the Qwen3 template uses). Rozum: `Job` carries `req.tools`; `render_prompt` builds
  OpenAI-style schemas (`tools_json`) into the template; `stream_generation`
  suppresses `<tool_call>` markup from the text stream and parses the run into
  `ToolUseStart/Delta/End` + `stop_reason=ToolUse` (`parse_tool_calls`). E2E verified:
  `mlx_tool_use_weather` emits a `get_weather` call (`stop=ToolUse`); unit test
  `parse_tool_calls_extracts`.

- [x] mlx-native-tool-history - **DONE** (rozum-only, pin unchanged). `message_text`
  now renders assistant `ContentBlock::ToolUse` blocks back into the prompt as
  Qwen3 `<tool_call>\n{json}\n</tool_call>` markup (the inverse of `parse_tool_calls`),
  instead of dropping them. Multi-turn tool loops — exactly what Claude Code/Codex
  do — now carry the prior call in history. Unit test `tool_use_round_trips_into_history`
  renders then re-parses (round-trip). (`tool` role results already fold in as text
  via `ToolResult`.)

#### SUPERSEDED: mistralrs-mlx-direct — targeted candle->MLX quant-op bridge

Proven dead end (kept as a parity oracle in `feature/mistralrs-mlx-direct`).
Correct (byte-identical on Qwen3-4B) but **slower than candle** (11.76 vs 100.74
T/s) due to a structural per-op cross-runtime GPU-sync floor. MLX speed is
all-or-nothing -> the native runtime above is the right path. p0/p1/p1b records
kept below; p2/p3/p4 cancelled.

Spec: `docs/specs/mistralrs-mlx-direct.md`. Decisions: targeted quant-ops ·
`mlx-rs` · in the `.vendor/mistral-rs` fork, generic.

- [x] mlx-direct-p0 - Phase 0: bridge prototype + single-op parity. **DONE**
  (fork branch `mlx-direct`, commit `14e699a26`).
  - `mlx-direct` feature + `mlx-rs = "0.25.3"` added; copy-baseline bridge
    (`afq/mlx_bridge.rs`) + `afq/mlx_direct.rs` (dequantize + quantized_matmul);
    runtime switch in `afq_dequantize_op`/`afq_mm_op`.
  - Gate PASSED (`--test-threads=1`): MLX dequantize vs candle (diff < 1e-4);
    MLX quantized_matmul vs candle dequant+matmul (diff < 1e-3).
  - Finding: no candle+MLX coexistence deadlock; the deadlock chase was a
    standalone `afq_mm_op` splitk `sum(0)` hang, reproduced with NO MLX linked.
    Metal tests must run single-threaded; `kill -9` of a hung Metal test wedges
    the GPU. Details in spec Results.

- [x] mlx-direct-p1 - Phase 1: dense model correctness. **DONE** (fork commit
  `a7ea747ea`; feature plumbed quant -> core -> cli).
  - `mlx-community/Qwen3-4B-4bit` via `mistralrs run`, `MISTRALRS_MLX_DIRECT`
    0 vs 1, same seed: **generation byte-identical** (623 chars, 136 tokens,
    `A: Paris.`). No deadlock under full-forward candle<->MLX interleaving.
  - **Perf regression: 2.89 vs 100.74 T/s (~35x).** Copy baseline's CPU
    round-trip + per-op sync. Correct but not shippable.
  - Cross-check vs `mlx_lm` not run (not installed); ON==OFF vs the mlx_lm-
    validated candle path stands in.

- [~] mlx-direct-p1b - Phase 1b: bridge perf. **PARTIAL.** (fork `c5986e13d`)
  - Weight-array cache (memoize candle->MLX of constant AFQ weights by Metal
    buffer addr): Qwen3-4B-4bit decode **2.89 -> 11.76 T/s (~4x)**, output still
    byte-identical. Banked.
  - **Remaining ~8.6x gap is structural:** per-op cross-runtime GPU sync (candle
    drain to host for `x` + MLX eval for the result = 2 syncs x ~250 quant
    ops/token). candle alone runs the same ops at 100 T/s via one queue + batched
    commits. Cutting this needs shared-MTLBuffer + shared-queue/event ordering,
    which is NOT reachable via public APIs (candle Private storage; mlx-c adopt
    wants void* not MTLBuffer; no cross-queue event exposed), OR widening the
    MLX region toward a fuller native runtime. Open strategic decision.

- [x] mlx-direct-p2/p3/p4 - CANCELLED (superseded by mlx-native-runtime). The
  bridge cannot beat candle (structural per-op sync floor), so wiring MoE
  gather, generalizing bit-widths, and the zero-copy spike no longer pay off.
  The native MLX runtime gets MLX speed by owning the whole forward instead.

#### mistralrs-concurrency-scheduling — responsive, memory-budgeted concurrency

Replace the blunt `max_num_seqs` 1/2 ladder with a layered model: budgeted
engine capacity (A), a rozum-side admission scheduler decoupled from the static
engine knob (B), priority + a reserved fast lane so small interactive requests
never queue behind big ones (C), and bounded-queue backpressure + an OOM circuit
breaker (D). Memory sets the upper bound; the Metal single-GPU compute sweet spot
sets the ceiling. Deliver synergistically in the order A → B+C → D (A lifts the
floor to 2 so a fast lane is physically possible).

Spec: `docs/specs/mistralrs-concurrency-scheduling.md`. Builds on the constant
per-prefill cost from `mistralrs-chunked-prefill.md` (~465 KB/token × chunk).

- [x] concurrency-budget - Phase A: load-time budgeted engine `max_num_seqs`. **DONE.**
  - `budgeted_max_num_seqs(ConcurrencyBudget)` = `clamp(headroom/per_seq, 1, ceiling)`,
    `headroom = safety_frac*available - weights - kv_pool`, `per_seq = prefill_chunk * ~465KB`.
  - Reuse `main.rs` footprint helpers (weights, kv_cache_bytes, available_ram_bytes).
  - Floor 1; lift to ≥2 only when headroom covers one extra `per_seq` (fast-lane room).
  - `ROZUM_MISTRALRS_MAX_SEQS` forces exact; `ROZUM_MISTRALRS_SEQS_CEILING` caps (default 8).
  - Replaces the 24-36 GB→1 / ≥48 GB→2 ladder. Pure fn unit-tested without Xcode.

- [x] concurrency-admission - Phase B+C: admission scheduler + fast lane. **DONE.**
  - `AdmissionScheduler` semaphore ≤ engine capacity, limit via `ROZUM_MISTRALRS_ADMIT`.
  - `chat()` acquires `AdmitGuard` before the engine; releases on done/cancel/drop.
  - SJF ordering by `RequestCost (prompt+max_tokens)`; reserved fast-lane slot for
    cost < `ROZUM_MISTRALRS_FASTLANE_TOKENS` (default 1024, 0 disables).
  - Finding: fork does NOT yield between prefill chunks (chunk loop is inside
    `pipeline::step`) → admission-order responsiveness only; mid-prefill preempt
    deferred to backlog `concurrency-engine-yield`.
  - Disconnect cancel/reap preserved for queued + admitted requests.

- [x] concurrency-load-shedding - Phase D: backpressure + circuit breaker. **DONE.**
  - Bounded queue `ROZUM_MISTRALRS_QUEUE_MAX` (default 32) → `Overloaded` → gateway 429 + Retry-After.
  - Metal alloc failure → `trip()` drops limit (floor 1), cooldown `recover_step()` raises back. No auto-retry (avoids re-OOM); best-effort substring detection.
  - Per-class `max_tokens` dropped (redundant with cost weighting). Invariants covered by scheduler tests.

**`mistralrs-concurrency-scheduling` complete (A + B+C + D).** Follow-ups in BACKLOG;
the big one is `concurrency-engine-yield` (true mid-prefill interleaving).

- [x] concurrency-backend-abstraction - Lift the admission machinery out of mistralrs into a generic `src/concurrency` module + `AdmittingBackend` decorator. **DONE.**
  - `ChatBackend::concurrency_capacity() -> Option<usize>` (default None); `admit_wrap` gates iff `Some`, passthrough otherwise (safe default for remote backends).
  - mistralrs reports `Some(max_num_seqs)`; its `chat()` is plain inference again. Generic `ROZUM_ADMIT*` env. The new mlx-rs backend gets admission/fast-lane/backpressure/breaker for free by returning a capacity.
  - Spec: `docs/specs/concurrency-backend-abstraction.md`.

#### shared-gateway — one shared model process, many launch clients

Make the model-serving gateway a shared, single-instance detached process that
`rozum launch` clients discover & reuse, so two launches don't load two models
and OOM. Single-owner election (TCP-port bind + advisory flock), transparent
failover on a stable port, idle shutdown via client leases. `--model` becomes
optional (reuse running / interactive picker). Adds `rozum models rm`.
Composes with `concurrency` (sharing = one model; AdmittingBackend = N clients).

Spec: `docs/specs/shared-gateway.md`.

- [x] shared-gateway-mvp - Detached `rozum gateway` daemon (registers `active.json`,
  stable port, idle-timeout exit). `rozum launch` discovers a healthy compatible
  gateway and reuses it, else spawns one (port-bind dedup; flock deferred to
  failover) and waits for health, then execs the agent. `--dedicated` keeps the
  old in-process behaviour. **DONE.**
- [x] shared-gateway-failover - Launch-side watchdog respawns the daemon on death
  (same port), anti-stampede via `share::try_spawn_lock` (O_EXCL stale-steal),
  port-bind backstop. Agent reconnects over the brief gap via its own retry. **DONE.**
- [x] shared-gateway-leases - Lease-refcount lifetime (`leases/<pid>` heartbeat,
  mtime-reap) keeps the daemon up while clients are live; `rozum gateway status`/`stop`. **DONE.**
- [x] launch-model-picker - `--model` optional: omitted+running → reuse (print
  model); omitted+none on a TTY → interactive picker (cached first with
  `(cached, size)` / `(not cached, ~size)` annotations; non-cached → download
  confirm); non-TTY → error. Mismatch policy: takeover-if-idle else reuse-with-warning. **DONE.**
- [x] models-rm - `rozum models rm <spec>`: confirm, refuse if it is the active
  model, delete HF/LMStudio dirs directly (Ollama via `ollama rm`), report freed size. **DONE.**
- [x] shared-gateway-proxy - Launch-local model-free reverse proxy in the request
  path (agent → proxy → daemon), mirroring `mcp-proxy`. Foundation for replay /
  poison / transparent swap. Re-points the agent at the proxy's local port. **DONE.**
- [x] shared-gateway-replay-retry - Buffer + replay a request when the daemon dies
  **before the first streamed token**; mid-stream failures surface. Smart retry:
  backoff + jitter, attempt cap, wait-for-health. **Two-tier admission**: daemon
  advertises room (`GET /v1/admit`); each proxy holds its client's requests in its
  own `concurrency::AdmissionScheduler` (SJF + fast lane) and only forwards within
  the daemon's window — prompts wait at the edge, not bounced. **DONE.**
- [x] shared-gateway-poison - Soft/graduated: per-fingerprint crash count;
  degrade-then-retry (serialize) first; refuse 422 only after `ROZUM_POISON_MAX`
  (default 3); share to TTL'd `poison.json` (default 1 h, decay-on-success) only
  on sole-in-flight high confidence — ambiguous stays local to the proxy.
  Crash-attribution = established-connection death (`!is_connect`), so a failover
  gap isn't blamed on the prompt; degrade = exclusive `lane` write-lock serializes
  the retry prefill; proxy fast-refuses confirmed entries before forwarding and the
  daemon's `poison_layer` re-checks before running the model. **DONE.**
- [x] gateway-switch - `rozum gateway switch --model Y [--backend B] [--n-ctx N]`
  / `reload` / `unload`: in-place drain → drop old model (never two resident) →
  load new → bump `generation` → resume; proxies hold across the gap (`/v1/admit`
  closes its window). Held by a `Switchboard` swap cell + injected `BackendBuilder`
  closure; chat handlers `enter()` (park while draining, lazy-reload if unloaded)
  and hold a `ChatLease` for the whole stream so a switch waits for streaming to
  finish. Drain uses a separate `generating` counter (not the idle `in_flight`,
  which would deadlock). `reload` re-execs the binary; `unload` drops the model and
  lazily rebuilds on the next chat. Control plane: auth-gated localhost
  `POST /control/{switch,unload,reload}`. `--dedicated` (no builder) refuses all
  three. `--backend` forces gguf/mistralrs/lmstudio/mlx/url. **DONE.**

#### channel-wakeup — push room events into idle agent sessions

Turn `rozum mcp-proxy` into a one-way Claude Code **channel** so a joined-but-idle
agent gets woken when a message lands in its room, instead of relying on the agent
to keep long-polling `meeting.wait_my_turn`. The proxy already holds a
`Peer<RoleServer>` to the agent session (`upstream_peer`); it declares the
`claude/channel` capability and a background task pushes `notifications/claude/channel`
with the new transcript delta. `wait_my_turn` stays as the authoritative pull path.

Empirically verified (CC 2.1.172): channels register fine under rozum's
local-gateway env (auth gate is Bedrock/Vertex/Foundry-only, not custom base URL),
but **only in the interactive `claude` CLI** — headless `-p`/Agent-SDK gets no
channel. `rozum launch … claude` is interactive, so it's on the right path.

Spec: `docs/specs/channel-wakeup.md`. rmcp 1.7 confirmed to support both pieces
(`ServerCapabilities.experimental` map + `ServerNotification::CustomNotification`).

> **NOTE 2026-06-18:** channel-wakeup was originally built in the legacy `proxy.rs`, but
> the P4 daemon refactor made `daemon_proxy.rs` the **default** `rozum mcp-proxy` (legacy
> behind `ROZUM_LEGACY_PROXY=1`), stranding the whole feature (Tier-1 capability/pusher AND
> the Tier-3 piggyback writer) on the unused legacy path. All three items below are now
> **DONE by porting the mechanism into `daemon_proxy.rs`**, adapted to its disk-read
> architecture (`feature/channel-wakeup-daemon-proxy`).

- [x] channel-wakeup-capability - **DONE.** `daemon_proxy.rs` `initialize` now captures the
  session `upstream_peer`, advertises `experimental:{"claude/channel":{}}`
  (`channel_capabilities()`), and `PROXY_INSTRUCTIONS` teaches the agent to read
  `<channel source="rozum" …>` events as a wakeup (authoritative delta via `wait_my_turn`).
- [x] channel-wakeup-pusher - **DONE.** A per-session background task (`ensure_wakeup_task`,
  started at `initialize`) **disk-tails** the joined room (`store::read_since` from the
  proxy's `room_root` — read-only, no second daemon connection, no ghost participant, fits
  the daemon's "clients read disk directly" contract) and emits `notifications/claude/channel`
  (`content` = `render_stored_delta`, `meta` = `{room,from,seq,your_turn}`) on the peer.
  Fire-and-forget; a send/read failure never crashes the proxy. Also carries the **Tier-3
  piggyback** append (was likewise stranded on legacy), auto-off when channels are active.
- [x] channel-wakeup-lifecycle - **DONE.** The task idles when `room_root` is `None` (after
  `leave`) and **re-primes on a room switch** (`rooms.join` re-points `room_root`/`self_pid`/
  `room_name`); teardown = process exit (stdio server). De-dups own-authored turns by
  `participant_id` (`self_pid`); primes the cursor to the transcript head (`transcript_head`)
  and advances it past delivered entries (`read_since` is inclusive of `n`, so it tracks
  *next-n*) — no backlog/reconnect notification storm. Bonus: `rooms.join` now also refreshes
  the proxy's disk-read `room_root` (a latent stale-room bug for `wait_my_turn` after a switch).
  4 new unit tests (render skip-own/format/seq, all-own→none, capability+instructions declared,
  transcript_head primes-past-backlog + delivers-fresh). 387 fast tests green.
- [x] channel-wakeup-launch-flag - `rozum launch` injects
  `--dangerously-load-development-channels server:rozum` for Claude Code agents
  (suppressible; CC ≥ 2.1.80; non-`claude` programs untouched). **DONE** —
  `ChannelWakeup::flags_for` probes `claude --help` and appends the flag (else
  degrades silently); `--no-channel-wakeup` suppresses, `--channel-mcp-name`
  sets the `server:<name>`; threaded through `exec_agent` /
  `exec_agent_anthropic` for both the shared and `--dedicated`/`--no-model`
  paths. (The struct + CLI flags pre-existed but were unwired — see runtime-config
  build fix.) The remaining channel-wakeup items (capability/pusher/lifecycle)
  are still open.

- [x] launch-backend-url-flag - **DONE (`feature/mlx-server-backend`).** `rozum launch
  --backend-url <URL>` — CLI equivalent of `ROZUM_BACKEND_URL` for pointing the agent
  at an external OpenAI-compatible server (Ollama `http://localhost:11434/v1`, vLLM,
  any `/v1`). **Forces** that backend (skips the local GGUF/MLX chain) via a dedicated
  in-process path `run_launch_url` — no shared daemon, no model load; `--model` carries
  the upstream model name (e.g. `qwen3:8b`), required (errors if omitted). Conflicts with
  `--no-model`; registered in `reorder_launch_args` (value flag). Builds
  `OpenAiHttpBackend` directly + serves the lightweight gateway, dies with the agent like
  `--dedicated`. Unit test `backend_url_value_flag_hoisted_from_after_program`. SPEC.md
  updated.

- [x] mlx-server-backend-optional - **DONE (`feature/mlx-server-backend`).** Restored
  the Python `mlx_lm.server` HTTP backend (retired in Phase 4) as **opt-in**, no cargo
  feature (HTTP backends are always compiled — just `reqwest`). `try_mlx_server`
  (`openai_http.rs`) probes `ROZUM_MLX_HTTP` (default `http://localhost:8080/v1`).
  Auto-chain step 4b runs it **only when `ROZUM_MLX_HTTP` is set** (so its port isn't
  probed otherwise), between LM Studio and the custom-URL step. Forceable via
  `--backend mlx-server` (aliases `mlx_lm_server`/`mlx-lm-server`; `mlx`/`mlx_lm` still
  force native MLX) — routed through `is_mlx_server_engine` in `build_gateway_backend_forced`
  + `build_choice`. Restored the `print_no_backend_hints` lines + SPEC.md chain. Unit
  test `backend_engine_tests::mlx_server_engine_aliases_are_distinct_from_native_mlx`.

#### rozum-native-channels — Anthropic-independent wakeup ladder

Spec: `docs/specs/rozum-native-channels.md`. Own the meeting wakeup end-to-end so
it doesn't *depend* on Claude Code's research-preview channels (Tier 1). Tier 2 =
the `wait_my_turn` long-poll contract (done, docs-only). Tier 3 = gateway
piggyback for agents that take neither.

- [x] rozum-native-channels-tier3 - **DONE (`feature/piggyback-wakeup`).** Tier-3
  gateway piggyback, keyed by **project + agent name**. New `src/meeting/piggyback.rs`:
  drop file at `$XDG_RUNTIME_DIR/rozum/piggyback/<project>/<agent>.log` (sibling of
  room sockets). **Writer:** the mcp-proxy channel pusher (`src/meeting/proxy.rs`)
  also `append`s each rendered transcript delta when `piggyback::enabled()` — rides
  the long-poll it already holds, no new room read. **Reader:** the launch-local
  HTTP proxy (`src/proxy.rs` `maybe_inject_room_activity`) drains the project's
  drops once per request (after fingerprinting, so injected room text never
  perturbs the poison id) and folds them into an out-of-band system note —
  prepended to Anthropic `/v1/messages` `system` or as an OpenAI
  `/v1/chat/completions` `system` message; tool JSON / SSE framing untouched,
  non-chat paths zero-touch. Drain is rename-then-read (no lost lines on a racing
  append) and only fires once injection is guaranteed (no loss on a parse miss).
  Caps: 4 KiB/injection, 16 KiB drop-file tail. **Piggyback is the fallback rung:
  auto-OFF when Tier-1 channels are active for the agent** (it would otherwise
  double-deliver — channel event *and* injected note), **ON otherwise**; force off
  with `--no-piggyback`, force on with `ROZUM_PIGGYBACK=1` (`resolve_piggyback`
  precedence: flag > env > auto). `WakeupPolicy::resolve` probes the Tier-1 flags
  once (`flags_for` spawns `claude --version` + prints) and drives both ends:
  threads the bool into the launch-local proxy reader (`ProxyState::with_piggyback`
  / `serve(.., piggyback)` — a disabled launch never drains, not even a stale drop
  file) and exports `ROZUM_PIGGYBACK=1|0` to the agent so the mcp-proxy writer
  agrees. 9 unit tests (append/drain round-trip + coalesce + caps + render + both
  inject shapes + zero-touch-when-disabled + `resolve_piggyback` precedence).
  Reaches Codex/aider/opencode/older-Claude at their next inference call (not a true
  idle wake). Build order item 2 in the spec.

- [x] runtime-config - Load backend policy and backend list from `rozum.toml`.
  - `src/config.rs`: `RuntimeConfig` (serde + `toml`) resolved from `$ROZUM_CONFIG`
    → `./rozum.toml` → `$XDG_CONFIG_HOME/rozum/rozum.toml`; malformed / missing-explicit
    is a hard error. `single` / `fallback` / `fanout` policies; every engine name
    accepted (`gguf`/`mistralrs`/`lmstudio`/`mlx`/`url` + the sync `hello`/`candle`/
    `llama-gguf`/`native-rust`/`external-command`).
  - `default()` IS the auto-detect chain in code (`[gguf, mistralrs, lmstudio, mlx, url]`,
    `Fallback`) → zero behaviour change without a `rozum.toml`. The daemon's initial
    load + every `gateway switch` now walk it (`main.rs::build_from_config`); `--backend`
    still force-bypasses. `[runtime].model`/`n_ctx` fill in when `--model`/`--n-ctx`
    omitted; per-backend `url`/`model`/`n_ctx` override.
  - 12 unit tests (Metal-free); lib suite 101 passing. Also fixed a stray
    `channel-wakeup` build break swept into the gateway-switch commit (separate fix
    commit, which also completed the `channel-wakeup-launch-flag` mechanism).
    Spec: `docs/specs/runtime-config.md`. **DONE.**

### Qwen3.6 unblocking track (three escalating upstream fixes)

Ordered cheapest → most strategic. Pick up the first one that lands; downstream
ones still pay off long-term but the user-facing Qwen3.6 problem is solved as
soon as any single track succeeds.

- [ ] llamacpp-qwen36-patch - Upstream PR to llama.cpp accepting `qwen35moe.rope.dimension_sections` length 3.
  - Single hyperparam loader fix (~50 LoC). Concrete error logged with Qwen3.6 GGUF from `unsloth/Qwen3.6-35B-A3B-GGUF`.
  - Patched llama.cpp → patched llama-cpp-2 version bump → `cargo update` in rozum and `--features gguf` works for Qwen3.6.
  - Estimated effort: ~1 week active + upstream review cycle.
  - Spec: `docs/specs/llamacpp-qwen36-patch.md`.

- [ ] mistralrs-qwen36-pr - Upstream PR to mistralrs registering Qwen3.5/3.6 as an alias of the existing `qwen3_next` model.
  - Discovery: mistralrs already has all the hybrid linear-attention layer code in `qwen3_next.rs` (GatedDeltaNet, full-attention, SparseMoeBlock, MoE routing). mlx-lm's `qwen3_5.py` re-uses `qwen3_next.py` classes verbatim — same architecture.
  - The PR is therefore not new layer code; it's: (a) register `model_type: "qwen3_5_moe"` and `architectures: ["Qwen3_5MoeForConditionalGeneration"]` to dispatch to the existing `Qwen3NextLoader`; (b) tolerate the nested `text_config` block + explicit `layer_types` array in the config parser; (c) handle `attn_output_gate` if it changes behaviour.
  - Correctness gate: byte-for-byte token match against `mlx_lm.generate --temp 0`.
  - Highest-leverage: every Rust project that uses mistralrs picks up Qwen3.5/3.6.
  - Estimated effort: ~1 week active (down from 2-3 weeks after the qwen3_next discovery).
  - Spec: `docs/specs/mistralrs-qwen36-pr.md`.

- [ ] mlx-native-port - Native MLX runtime in rozum on top of `mlx-rs`, porting `mlx_lm` Python piece by piece.
  - Phased: Phase 0 (bootstrap) → Phase 1 (Qwen3-4B dense) → Phase 2 (Qwen3 MoE) → Phase 3 (Qwen3.6 hybrid). Each phase has a numerical-match exit criterion.
  - Removes our dependency on mistralrs / llama-cpp-2 release cycles entirely; new model families become ~3-5 day port tasks instead of "wait for upstream".
  - New crate feature `mlx-native` (off by default — heavy compile, big code surface).
  - Estimated effort: ~5-8 calendar weeks for parity with current mistralrs scope.
  - Spec: `docs/specs/mlx-native-port.md`.

### Small-model utility track (P2) — where 4B/7B earn their keep

> Motivation (from the 2026-06-16 agentic matrix): small models (Qwen3-4B,
> Qwen2.5-Coder-7B) are NOT viable autonomous coders — they pass `greet` and the
> occasional one-line edit but fail multi-step `build`/`test`. They ARE fast
> (~8 GB, high tok/s) and fine for narrow single-shot work. Two concrete roles:

- [~] spec-decode-draft - **Speculative decoding: a small draft accelerates a big target. SPEC WRITTEN — NEXT P0, build architecturally.** Spec: `docs/specs/speculative-decoding.md`.
  - A small model (e.g. `Qwen3-4B-4bit`) proposes k tokens; the big target
    (e.g. `Qwen3.6-35B-A3B-4bit`) verifies them in one forward and accepts the
    longest correct prefix. Net: fewer big-model forwards → faster decode with
    **byte-identical greedy output** (the whole point — it's not a quality
    tradeoff, it's a latency win).
  - **Architecture (грамотно, per spec):** NOT a hack in the MLX loop. An
    **engine-agnostic orchestrator above the SPI** (`src/specdecode.rs`,
    accept-longest-prefix loop, byte-identical invariant **unit-tested with a mock
    target** — no real model) + a small opt-in `SpeculativeVerify` capability a
    backend implements (`prefill`/`verify`+KV-rollback). Draft + target are two
    **residents**; device-aware placement (draft cheap device, target fast) —
    the canonical heterogeneous single-stream co-use (`multi-device-residency.md`,
    North Star). MLX implements the capability for dense targets first; GGUF later.
  - Caveats to design around: the hybrid Qwen3.6 GatedDeltaNet recurrent state is
    not freely truncatable (`HybridPrefix`) — rollback on rejected drafts is the
    hard part; **dense-target first**, hybrid degrades to plain greedy. Draft +
    target must share a tokenizer (Qwen3 family ✓; enforced).
  - Acceptance: `--draft-model <spec>` (or env) → greedy output **identical** to
    non-draft + a measured tok/s speedup on a real dense target. Effort: LARGE.
  - **P0 DONE** (`src/specdecode.rs`, 3 tests, branch `feature/spec-decode`): the
    engine-agnostic orchestrator (`Draft`/`Target` traits + `decode()`/`decode_streaming()`,
    accept-longest-greedy-prefix) with the **byte-identical invariant proven with
    a mock target** (oracle/all-wrong/flaky drafts all yield the exact target
    greedy seq; oracle ~len/(k+1) forwards vs all-wrong's len).
  - **P1 DONE — built, proven, matrix-gated; verdict: net-negative on the
    recommended MoE model** (branch `feature/spec-decode-p1`, commits
    `02e2ecd`/`a0ed496`/`eeb8487`). iter-1 = `--draft-model` flag + SPI
    `SpecDecodeBackend` (target-only fallback). iter-2 = MLX **dense** verify
    (multi-pos forward → per-row argmax → accept-longest-greedy-prefix →
    KV-truncate-to-accepted) + propose, on the existing `dense_forward` /
    `ConcatKeyValueCache.truncate` / `argmax`; proven on Metal by
    `mlx_spec_decode_byte_identical`. iter-3 = dual-model worker
    (`MlxNativeBackend::new_spec_decode`, both models one worker, orchestrator runs
    inside) streaming via `BatchSeq` + chunked prefill so a 30B target doesn't OOM;
    gateway auto-detects an MLX-dense pair (`build_spec_decode_backend`).
  - **MATRIX RESULT (M4, claude × Qwen3-30B-A3B + Qwen3-4B draft):** correctness
    gate **PASSED** (pass-matrix identical off-vs-on, no quality regression); speed
    gate **FAILED on this target** — spec-decode 34.8 t/s vs 98.1 baseline (2.8×
    *slower*) despite 2.65× fewer target forwards. **Why (structural, not a bug):**
    (1) the 30B-A3B target is **MoE** — a `k+1`-token verify forward loads more
    distinct experts, so per-forward cost scales with tokens and cancels the
    forward-count win (spec-decode needs a *dense*, bandwidth-bound target whose
    forward cost is flat in token count); (2) the 4B dense draft (124 t/s) is no
    cheaper than the MoE target (98 t/s) — the draft must be ≥5–10× cheaper.
    **Verdict:** correct + valuable durable infra (North Star: dense /
    heterogeneous-device single-stream co-use), stays **off by default** (opt-in
    `--draft-model`); a win only for a slow large *dense* target + tiny draft, which
    isn't among the cached/recommended M4 models. Full numbers + reasoning in the
    spec "Results". Float-tie caveat: byte-identical in exact arithmetic; on Metal,
    identical modulo rare argmax ties (verify batches positions → KV built in a
    different shape than sequential decode) — `mlx_spec_decode_byte_identical`
    asserts the mechanism (lcp), the matrix asserts functional equivalence.

- [x] small-model-router-rag - **Small model as router / classifier / RAG worker.
  DONE — all P1+P2 shipped** (`src/router.rs`, branches `feature/small-model-router`
  + `feature/small-model-rag-worker`).
  - Use a 4B/Coder-7B for the narrow, single-shot, latency-sensitive steps that
    don't need a big model: intent/query classification, model-or-tool routing
    (cheap pre-filter before invoking 27B+), RAG chunk rerank + summarize,
    structured-field extraction. Builds on what's already here — `src/rag_lite.rs`
    and `src/memory_store.rs`.
  - Where: a small routing/classification entrypoint the gateway (or `rozum
    launch`) can call before/around the main model; reuse `rag_lite` for retrieval.
  - **P1 DONE:** `ModelRouter` classifier primitive — caller-supplied `Label` set,
    engine-agnostic over any `ChatBackend` (mirrors `cascade::ModelJudge`), never
    errors (`snap_to_label` snaps a noisy reply to a label; off-set/failure →
    fallback). 8 hardware-free unit tests + **M4 eval `model_router_eval` 6/6 =
    100%** on Qwen3-4B (code/math/chitchat), all exact matches — well above the
    0.80 gate-the-big-model bar. Plain prompt+snap sufficed (no constrained decode
    needed). Spec: `docs/specs/small-model-router.md`.
  - **P2 cascade wiring DONE** (`src/cascade/{mod,spec}.rs`): `ModelRouter` is the
    cascade's optional async model-backed difficulty source.
    `ModelRouter::difficulty` (over `router::difficulty_labels()` trivial/moderate/
    hard) → `CascadeConfig.router` drives the entry tier for `ClassifyThenStart`/
    `Learned` (skipped under `AlwaysCheapest`; sync `Classifier` trait untouched).
    Reachable from config: `CascadeSpec.router_model` resolved in `build_cascade`
    (→ `model:"cascade:<name>"` + `router_model` in `rozum.toml`/`ROZUM_CASCADE`).
    4 new tests (router routes hard→top / trivial→cheapest; router_model resolved+
    threaded; JSON round-trip). 366 fast tests green.
  - **P2 RAG worker DONE 2026-06-18** (`src/router.rs` `RagWorker`): same
    `ModelRouter` shape over `rag_lite::Hit`s. `rerank(query, hits)` judges each hit
    `relevant`/`related`/`irrelevant` (via `snap_to_label`), **drops** irrelevant +
    reorders relevant-first with a **stable** sort (refines BM25 recall, never
    scrambles within a grade); a model fumble keeps the hit as `related` (conservative
    — never silently drop). `summarize(query, hits)` answers grounded **only** in the
    survivors, falls back to the top snippet on a blank/failed generation.
    `rerank_and_summarize` / `grounded_answer(retriever, query, k)` compose the two
    with `rag_lite` recall → `GroundedAnswer { hits, summary }`. 7 hardware-free unit
    tests (drop+grade-order, stable-within-grade, off-set→keep, summary
    empty/fallback/passthrough, `grounded_answer` e2e) + `#[ignore]` M4 eval
    `rag_worker_eval` (4B drops a lexical decoy, ranks the answering doc first, grounds
    the summary). 373 fast tests green. **This closes the small-model-router track**
    (classifier P1, cascade wiring P2, RAG worker P2). Composes with
    [[small-model-cascade]] (the after-the-fact escalation counterpart).

- [x] small-model-cascade - **Single-shot bounded tasks served small-first, escalate on
  doubt. DONE 2026-06-18** (`src/cascade/tasks.rs`, branch `feature/small-model-cascade`).
  Spec: `docs/specs/small-model-cascade.md`. A **thin preset over the cascade core** (one
  `AcceptanceCheck` gate + a two-tier config builder + a prompt helper — no new engine):
  - `SmallTask::CommitMessage` + `small_task_config(task, small, big)` → a two-tier
    `CascadeConfig` (`small`=tier 0, `big`=tier 1, `AlwaysCheapest`, acceptance =
    `[StructuralCheck, <task gate>]`, self-signal + affordance off — the task has a concrete
    validator, not a self-report). Health/backoff/budget/stats/lanes inherited from cascade.
  - `CommitMessageGate` (free, deterministic): extract the subject (first non-empty line,
    fence/quote/heading stripped) → `ESCALATE` on empty / over-72-char / refusal / chatter
    preamble / `<placeholder>`, else `ACCEPT`; never `Inconclusive`. False-positive-safe
    ("Fix commit message parser crash" accepts). `commit_message_request(diff)` builds the
    tight gate-shaped prompt.
  - **10 fast tests, all green** (no model): gate accept/escalate cases + e2e over a real
    `CascadeBackend` with mock backends — good cheap answer accepts at tier 0 (**big never
    called**), junk escalates once, and a **small-tier hit-rate** batch (small passes 3/4 →
    big called exactly once). 383 fast tests green. Generalizes [[small-model-router-rag]]
    (routing decides up-front; the cascade decides after a cheap attempt).
  - **CLI wiring DONE 2026-06-18:** `rozum commit-msg [--model <spec[,spec2]>]` (`src/main.rs`)
    reads `git diff --cached` → `commit_message_request` → prints. Single `--model` generates
    directly; a `small,big` list builds the `small_task_config` cascade (small answers, gate
    escalates). Defaults to `[runtime].model`. `staged_diff_in` unit-tested in a temp git repo.
  - **Follow-ups (deferred):** process-gated task types (`OneLineFix` via `cargo check`,
    `Rename` via build/lint — gate trait supports it, not hermetically testable); a remote big
    tier for `commit-msg` (v1 treats both tiers as local).

### Model bringup track (catalog) — new architectures to get working

> Suggested 2026-06-16. Both need investigation + fiddling before they serve
> cleanly. Bringup workflow per model: pick the MLX checkpoint → check
> `supported_model_type` (`src/mlx_native_backend.rs`) and `config.json`
> `model_type` → if unknown arch, port it (else just register) → numerical-parity
> gate vs `mlx_lm` (`scripts/mlx_ref.py`) → add to the catalog (`src/models.rs`) →
> verify tool-call parse/format (`src/serving.rs` / `src/constrain.rs`).

- [x] gpt-oss-20b-bringup - **DONE 2026-06-17 (native port, merged to master).** Chose
  path (b): added the `gpt_oss` arch to mlx-native — MXFP4 experts (`gather_qmm` mode),
  attention sinks, alternating sliding/full attention, YaRN rope, mixed per-leaf quant
  (8-bit router), clamped SwiGLU. Byte-exact greedy parity vs Python `mlx_lm`. Full
  **harmony** adapter (`src/harmony.rs`): clean chat + single/multi-turn tool calls.
  **claude 5/5** on the agentic matrix (= the 35B) after fixing 3 OUR bugs (greedy-CoT
  loops → temp floor; parser dropped recipient-on-wrong-channel calls; no prefix reuse).
  In `src/models.rs`. Fork pushed + pinned. Memory: [[project-gptoss-native-port]].

- [x] qwen4-coder-bringup - **DONE 2026-06-17 = `Qwen3-Coder-30B-A3B-Instruct-4bit`.**
  `model_type: qwen3_moe` → routes through the EXISTING path; **byte-exact greedy
  parity** vs Python `mlx_lm` (`s.chars().rev().collect::<String>()`). Tool calls
  parse (XML `<function=…>` form). Needed TWO small fixes: (1) the checkpoint
  quantizes the MoE router (`mlp.gate`) at **8-bit** while the rest is 4-bit — added
  per-tensor router-bits handling to the fork's `qwen3_moe` loader (pre-quantize the
  gate at its own bits; backward-compatible). (2) its chat template does
  `tools | length` / `tools is defined` → pass an empty `[]` for no-tools (not null),
  in `tools_json` (matches transformers; truthiness-guarded templates unaffected —
  gpt-oss regression-checked). Added to `src/models.rs`. Fork rev bumped to e5ebe9d2.

- [ ] glm4-bringup — **QUEUED 2026-06-21.** Port GLM-4 (dense) to the MLX-native crate:
  add `.vendor/mlx-lm/mlx-lm/src/models/glm4.rs` + register in `models/mod.rs` + dispatch
  `"glm4" => glm4::load_glm4_model(dir)` in `src/mlx_native_backend.rs`. Same playbook as
  [[project-gptoss-native-port]]: byte-exact greedy parity vs Python `mlx_lm.glm4`
  (`scripts/mlx_ref.py`), then catalog (`src/models.rs`) + tool-call verify. Targets that fit
  36 GB: **GLM-4-9B** (fast bring-up) → **GLM-4-32B-0414** (dense; 4-bit ~18–20 GB, the real
  target, ≈ Qwen3.6-27B footprint). Building blocks ALREADY in the crate to reuse: **partial
  RoPE** (`qwen3_5`/`qwen3_5_moe` — GLM's distinctive `partial_rotary_factor`), **q/k/v bias**
  (`qwen3`), **post-attention / sandwich norm** (`qwen3`/`gemma3`). NEW work = GLM **weight-name
  remap** (the gpt-oss "garbage bug" risk — get q/k/v/o/gate/up/down/norm names exact) + the
  GLM **chat template** (`[gMASK]<sop>` + `<|user|>`/`<|assistant|>`). Quick "does it run at
  all" path meanwhile: the vendored **mistral.rs already has `Glm4ForCausalLM` /
  `Glm4MoeForCausalLM` loaders** → `ROZUM_FORCE_MISTRALRS=1 rozum launch --model <glm-4-9b>`
  (candle/Metal — works but ~5–10× slower per [[project-mistralrs-mlx-direct]]; validation
  only, the real path is the MLX-native port for speed). OUT OF SCOPE for 36 GB (too big — for
  the record): the MoE GLMs — **GLM-4.5-Air** (106B-A12B), **GLM-4.5** (355B), and **GLM-5 /
  GLM-5.1** (744B-A44B MoE, 256 experts/8 active, DeepSeek sparse attention, 200K ctx,
  released 2026-02-11) — GLM-5 ≈ 370 GB at 4-bit; cluster-scale, not local. See BACKLOG
  `glm-model-landscape`.

### Runtime / backend track — new engines below the `ChatBackend` seam

- [ ] native-engine-spi - **ARCHITECTURE FIRST (prerequisite of `x86-native-runtime`).**
  Draw the internal seam every in-process engine shares so a new engine is "implement
  a tiny trait + its kernels", not "re-implement the leaf". Lift the engine-agnostic
  decode/serving logic UP into one shared `drive` loop behind a `LocalEngine` trait;
  push hardware/kernels DOWN into small isolated components. The decode-control loop
  is currently copy-pasted (MLX `stream_generation`, GGUF's own loop) — x86 would be
  a third. Hardware-independent; validated on MLX+GGUF on a Mac. Phases: **A1 [x]**
  define the seam (`src/engine.rs`: `LocalEngine`/`EngineMeta`/`drive`) → **A2a [x]**
  extract the engine-agnostic consumption loop `consume_tokens` (detok→`ChatEvent`,
  harmony + `<tool_call>` parse, EOS/max-tokens/runaway-guard, finalize) +
  `is_runaway_loop`/`next_tool_call_id`, unit-tested hardware-free → **A2b [x]** rewire
  the MLX leaf: `stream_generation` now only PRODUCES token ids (`PipelinedIds`, keeps
  the `async_eval` pipelining; lazy serial fetch so hybrid prefix-reuse stays in sync)
  and delegates to `consume_tokens` (the ~200-line copy deleted). Validated: 314 lib
  tests; gpt-oss chat+tool+~90 tok/s; Qwen3.6-27B hybrid multi-turn prefix-reuse. (A
  formal `impl LocalEngine` wrapping load/meta/generate is the remaining tidy-up.) →
  helpers consolidated to one source. **Core done — the shared layer the x86 leaf
  needs is ready** (`consume_tokens`, `sampler`, `serving`/`harmony`, model-reference).
  **A3 [IN PROGRESS — user-authorized full hardware-independent push, 2026-06-18]**
  (branch `feature/native-engine-spi-a2-a3`). Step 1 DONE: **`portability-shared-
  model-source` extracted** — `spec_to_hf_repo`/`resolve_model_dir`/
  `config_model_type`/`ensure_model_dir` lifted out of the MLX leaf into a new
  engine-agnostic `src/model_source.rs`, with the per-engine "can I load this
  `model_type`?" decision passed in as a **`gate` callback** (so mistralrs / a
  future leaf reuse one fetch/cache/resolve path); the MLX leaf keeps its catalog
  (`supported_model_type`/`model_type_gate`) and re-exports for zero caller churn.
  Verified: feature-free build green, `model_source` unit tests pass, `mlx-native
  --tests` compiles. Step 2 DONE: **`drive` implemented** (was `unimplemented!()`) —
  runs `LocalEngine::generate` over a rendered prompt → `consume_tokens`, render/detok
  stay caller-side (engine tokenizer is borrowed separately from its forward graph);
  unit-tested end-to-end via a minimal in-memory `FakeEngine`. **FINDING (blocks the
  formal MLX `impl LocalEngine`):** the MLX **hybrid** arches (Qwen3.6) reclaim the
  generator's internal KV/conv cache *after* a run (`into_cache_and_snapshot`, for
  prefix reuse), which a `generate()->Box<dyn Iterator>` return ERASES — so routing
  hybrid MLX through `drive` would break shipped prefix reuse. The trait needs a
  cache-reclaim seam, deferred to be shaped against the real x86 engine (dense MLX +
  the x86 leaf have no such reclaim and can adopt `drive` directly). NEXT: A3 GGUF
  adoption (caveat retained: don't downgrade GGUF's *streaming* tool parser;
  render/preflight lift) — also best shaped by the x86 consumer.
  Token-level seam,
  NOT a per-op tensor abstraction (avoids the `mistralrs-mlx-direct` perf dead-end).
  Spec: `docs/specs/native-engine-spi.md`.
  - [x] **engine-spi-a3-gguf — DONE 2026-06-20** (branch `feature/gguf-consume-tokens`). GGUF's
        `generate_blocking` now drives `crate::engine::consume_tokens` via a token iterator
        (`std::iter::from_fn` over llama.cpp sample→advance) + a per-token detok closure; deleted GGUF's
        private ~150-line decode loop + the streaming `ToolUseParser`/`ToolParseEvent`. SPI now proven by
        **two real engines** (MLX + GGUF). `consume_tokens` has no `Send` bound, so the `!Send`
        `LlamaContext` works on the blocking thread (couldn't use `drive()`). The "streaming→finalize"
        tool-call change is cosmetic (clients coalesce). **Surfaced + fixed a pre-existing GGUF bug:**
        `get_logits_ith(n_cur-1)` used the absolute position, but it indexes the last decoded batch
        (1-token decode batch → index 0) → after the first token it read garbage → an end token →
        generation stopped after ~1 token. GGUF was effectively broken in rozum. Fixed (track the right
        index). **Validated e2e** on `ollama:qwen2.5-coder:7b`: before — count→`"1"`, tool→`{"`; after —
        full `"1 2 … 20"` + correct `get_weather` tool_call with a cross-turn-safe id. (Step 1, the
        `next_tool_call_id` fix on `feature/gguf-toolcall-id`, was superseded here — `consume_tokens`
        already uses it.)
  - [x] **engine-spi-dense-mlx-drive — DONE 2026-06-21** (branch `feature/dense-mlx-drive`). Two parts:
        (1) **Send-relaxation** (prereq) — dropped `Send` from `LocalEngine` + `generate()`'s return; a
        feasibility map proved this (not the reclaim seam) was the real blocker, since the MLX engine
        state is irreducibly `!Send`. `drive` runs the engine synchronously on its own thread, so `Send`
        was unneeded. Proven by `drive_accepts_a_not_send_engine` (Rc-holding `!Send` engine — would not
        compile before). (2) **The adoption** — a `DenseMlxEngine` (`impl LocalEngine`) whose `generate`
        dispatches per dense arch (Qwen3/Qwen3Moe/GptOss/Llama/Qwen2/Gemma3), built from the prepared
        prefill + borrowed model+cache (split-borrow); `run_job` now routes the 6 dense arches through
        `engine::drive`, while the 2 hybrid arms stay on `stream_generation` (they reclaim via
        `into_cache_and_snapshot`, which `Box<dyn Iterator>` would erase). `drive` now has its first
        production caller; the SPI is exercised by a real engine. **Validation:** (a) byte-identical by
        construction — same per-arch generator + same `consume_tokens` with identical
        meta/prompt_len/seed/repeat_guard/decode/emit; (b) functional — the branch produced correct
        coherent greedy output on cached gpt-oss-20b (analysis-channel prime list `2, 3, 5, 7, 11, …`);
        (c) engine unit tests green. The empirical master-vs-branch raw A/B was attempted but blocked by
        RAM-starvation from accumulated 11 GB model loads (an environment limit, not the code; and
        risky to force given the GPU-memory history) — the by-construction proof + functional run stand.
        Dense path is byte-identical; no runtime change (the value is the SPI proof / x86 de-risking).
  - [~] **engine-spi-reclaim-seam — DRAFT DONE 2026-06-21** (branch `feature/engine-spi-reclaim-draft`).
        The hybrid cache-reclaim seam is now sketched + compile/FakeHybrid-validated in `src/engine.rs`:
        a `ReclaimStream` trait (`Iterator<Item=Result<u32,String>>` + `type State` +
        `into_state(self: Box<Self>) -> State`, mirroring MLX's `generator.into_cache_and_snapshot()`)
        and `drive_reclaiming(...) -> (StopReason, State)` that drains the stream through the SAME shared
        `consume_tokens` (borrowed so it survives) then reclaims its state. Two tests: `FakeHybridStream`
        round-trips a pretend KV cache through the loop (`drive_reclaiming_returns_post_run_state`) and
        through a `Box<dyn ReclaimStream>` (`..._works_through_a_trait_object`). **Deliberately unwired**
        — not used by MLX; the FINAL shape (fold into `LocalEngine`? exact `State` bounds? engine-side
        production) is to be decided against the real x86 engine. No MLX hybrid rewire until x86 is in
        play. The Send-relaxation half is DONE (above). Spec: `docs/specs/native-engine-spi.md`.

- [x] x86-native-slot - **DONE 2026-06-18 — the empty x86 slot, scaffolded so the real engine
  drops in without rework** (`src/x86/`, branch `feature/x86-native-slot`). Compiles on any host
  (no Vulkan deps yet), so the **default CI keeps the contract honest**. Contents: `X86NativeOptions`,
  `X86NativeBackend` (`impl ChatBackend`, errors with a self-documenting `NOT_IMPLEMENTED`),
  `try_build_x86_backend` (logs + falls through until built), and **`X86Engine` (`impl
  crate::engine::LocalEngine`) — a second `LocalEngine` implementor that proves the token-level seam
  fits a non-MLX engine** (the native-engine-spi validation, no hardware needed). The five compact
  components are pre-shaped stub files with their contract + the test to write
  (`device`/`memory`/`tensor`/`kernels`/`model`). Wired + reachable: `engine="x86-native"` (aliases
  `x86`/`vulkan`) in `config.rs::ACCEPTED_ENGINES` + a `main.rs::build_choice` arm (NOT in the
  default auto-chain). `Cargo.toml` reserves the `x86-native` feature for the future Vulkan binding.
  3 slot tests (default sane, falls-through-until-built, chat errors clearly). 379 fast tests green.
  **To fill:** add the Vulkan dep under the feature + implement the component bodies (P0–P5) + wire
  `chat` through `engine::drive` (native-engine-spi A3, shaped against this real consumer). Spec:
  `docs/specs/x86-native-runtime.md` § "Status: SLOT SCAFFOLDED".
- [ ] x86-native-p0-probe - **P0 of `x86-native-runtime`** (after `native-engine-spi`) (the MLX recipe — iGPU +
  unified memory + zero-copy `mmap` — on commodity x86 via cross-vendor Vulkan).
  Stand up a Vulkan compute device from Rust (`ash`/`vulkano`); on BOTH an Intel
  Xe/Arc and an AMD APU confirm a `HOST_VISIBLE | DEVICE_LOCAL` heap and
  `VK_EXT_external_memory_host`, then `mmap` a safetensors file → import the host
  pointer as device memory → read a tensor back GPU-side (zero-copy). Decide the
  Rust Vulkan binding and whether to lean on a kernel lib for plumbing. Acceptance:
  zero-copy import demonstrated on both vendors + a short decision record appended
  to the spec. **Needs an x86 iGPU box** (can't be validated from macOS). Spec:
  `docs/specs/x86-native-runtime.md`; epic + phases P1–P5 in `BACKLOG.md`
  (`x86-native-runtime`).

### Done

- [x] lmstudio-http-backend - Auto-detect LM Studio's local OpenAI-compatible server at `http://localhost:1234/v1`.
  - Unlocks Qwen3.6 (and any LM Studio MLX model) on Apple Silicon today, ahead of in-process mistralrs AFQ work.
  - Inserts above `mlx_lm.server` in the `build_gateway_backend` priority chain.
  - Reuses the existing `OpenAiHttpBackend` SSE parser; no new dependencies.
  - Env: `ROZUM_LMSTUDIO_HTTP=http://host:port/v1` to override the default endpoint.
  - Spec: `docs/specs/lmstudio-http-backend.md`.

- [x] idle-cpu-reduction - Event-driven TUI / room loops; ~0% CPU when idle.
  - Spec: `docs/specs/idle-cpu-reduction.md`.

- [x] chat-backend-spi - Async streaming `ChatBackend` trait with tool-use, sampling params, cancel; replaces the old sync `InferenceBackend`.
  - Content blocks (`Text` / `ToolUse` / `ToolResult`) in the SPI from day 1.
  - Helper `collect_to_string` for meeting call-sites that still need a final `String`.
  - `BackendOrchestrator` (Single / Fallback / FanOut) rewritten on async streams.
  - Spec: `docs/specs/chat-backend-spi.md`.

- [x] gguf-backend - In-process GGUF inference on Metal via llama-cpp-2.
  - Crate feature `gguf`. Path resolvers for absolute paths, `lmstudio:<repo>`, and Ollama-cached tags (`<name>[:<tag>]`, reading `~/.ollama/models/blobs/` without a running daemon).
  - Streaming, per-token cancel, prompt-cache by `session_id`, Qwen-hermes tool-use parser.
  - Spec: `docs/specs/gguf-backend.md`.

- [x] mistralrs-backend - In-process native-MLX backend via the `mistralrs` crate (on by default).
  - Loads MLX safetensors directly: `mlx-community:<repo>`, `hf:<user>/<repo>`, or local directory. Auto-download via `hf-hub`.
  - Streaming token-by-token; per-token cancel; reuses `crate::gguf::ToolUseParser` for tool calls.
  - Spec: `docs/specs/mistralrs-backend.md`.

- [x] api-gateway - Outward HTTP gateway exposing both OpenAI and Anthropic dialects on `127.0.0.1`.
  - `GET /v1/models`, `POST /v1/chat/completions` (OpenAI SSE with `tool_calls`), `POST /v1/messages` (Anthropic event-stream with `tool_use` blocks).
  - Context-overflow → HTTP 400 with a clear error. Cancel propagates from client disconnect.
  - Optional bearer auth via `ROZUM_GATEWAY_TOKEN`. Bind always `127.0.0.1`.
  - Spec: `docs/specs/api-gateway.md`.

- [x] launch-wrapper - `rozum launch --model X <program>` starts the gateway and execs the agent CLI with `ANTHROPIC_*` / `OPENAI_*` env vars pre-set.
  - Uses `ANTHROPIC_AUTH_TOKEN` (rank-2 in Claude Code auth precedence) so the local model wins without `claude /logout`.
  - Sets `ANTHROPIC_MODEL` + the four `ANTHROPIC_DEFAULT_{OPUS,SONNET,HAIKU}_MODEL` slots so Claude Code starts on the local model without a manual `/model` pick.
  - Enables `CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY=1` so the model shows up in the `/model` picker with `display_name`.
  - Argument reordering pre-parser accepts both `--model X claude` and `claude --model X`; `--` separator forwards remaining args verbatim.
  - Spec: `docs/specs/launch-wrapper.md`.

- [x] launch-no-model - `rozum launch --no-model <program>` runs the agent with no
  local model against upstream Anthropic: no gateway/lease/proxy, no `ANTHROPIC_*`/
  `OPENAI_*` overrides, operator's own auth preserved; only rozum agent-context
  defaults applied. Picker lists "Anthropic (cloud — no local model)" first.
  `LaunchTarget::{Local,Anthropic}`; `--no-model` conflicts with `--model`/
  `--dedicated`/`--n-ctx`/`--port`, reordered like value flags. Unlocks channels
  (real Anthropic auth) for rozum-launched agents. Spec: `docs/specs/launch-wrapper.md`. **DONE.**

- [x] models-cli - `rozum models {list, list --remote, info <spec>}` for discovering and inspecting local LLM models.
  - Scans HuggingFace hub, Ollama (both monolithic GGUF and per-tensor MLX layouts), and LMStudio caches without needing those runtimes running.
  - `list --remote` prints a curated download list optimised for 24-36 GB Apple Silicon unified memory.
  - `info <spec>` fetches HuggingFace metadata for not-installed models (author, downloads, license, total size, tags) and prints the install command.

### Cancelled / Superseded

These were in the queue earlier but either landed as part of larger work or no longer match the current product direction.

- [x] meeting-cli-surface — done as part of the current CLI shape: bare `rozum` launches a meeting, `rozum list` / `rozum mcp-proxy` are present, and the only user-facing model commands are `rozum gateway / launch / models`. No standalone "model diagnostics" CLI was ever shipped. Spec: `docs/specs/optional-local-models.md`.

- [x] agent-meetings — implemented as the default `rozum` runtime + `rozum mcp-proxy`. Claude Code / Codex sessions join via the MCP proxy and a human participates through the TUI. Moderator modes, budget, and hotkeys live in `src/meeting/`. Spec: `docs/specs/agent-meetings*.md`.

- [x] remote-api-backends — superseded by two newer pieces of work: `OpenAiHttpBackend` already speaks the OpenAI Chat Completions dialect against any compatible server (Ollama, mlx_lm.server, vLLM, OpenAI itself) via `ROZUM_BACKEND_URL`, and `api-gateway` exposes both OpenAI and Anthropic dialects locally. A symmetric `AnthropicHttpClient` backend (so rozum can call out to api.anthropic.com) is captured separately under `anthropic-http-client-backend` in `BACKLOG.md`.

- [x] smollm2-chat-template — superseded by per-backend chat templating: `gguf::format_qwen_prompt` for GGUF backends (Qwen / ChatML format with tool defs); mistralrs's own template applier for MLX backends; the gateway forwards chat templates upstream for OpenAI-HTTP backends. No standalone SmolLM2-specific layer is needed.

- [x] eval-harness — no longer in scope while the product focus is "local LLM provider for Claude Code / Codex". Evals matter when we are choosing between local models for accuracy; right now we are choosing for "does it run at all on M-series with the target architecture", which is best answered by trying the model in `rozum launch`. Will reopen as `local-llm-eval-harness` in `BACKLOG.md` if/when we need it.

## Done Criteria

- `cargo fmt --check` passes.
- `cargo test` passes.
- `cargo build --release` passes.
- `cargo build --no-default-features` produces a meeting-room-only binary.
- Bare `rozum` starts a meeting room without model inference.
- User-facing CLI commands are `gateway`, `launch`, `models`, `list`, `mcp-proxy`, `web`, `discord`, `telegram`.
- Specs for completed items have checked behavior boxes and results.
