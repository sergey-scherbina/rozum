# Sprint

(Formerly `WORK_QUEUE.md`; renamed to `SPRINT.md` per `AGENTS.md` / the
multi-agent skill.)

Current sprint focus: (1) make Rozum a reliable local meeting room for live agents and a human operator; (2) make Rozum a local LLM provider for Claude Code and Codex via an outward OpenAI/Anthropic-compatible gateway backed by an in-process MLX / GGUF engine on Apple Silicon Metal.

## Sprint

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

> **MLX native runtime is DONE (correctness + perf), 2026-06-13.** Decode root-caused
> & fixed (bf16 stream leak in GatedDeltaNet q/k scaling → ~1000 casts/token): MoE
> decode 33→~88 t/s (2.7×), prefill →1215 (=Python), dense 16→~19.6; byte-exact; merged
> to master `74c7a96`, fork `0d4b3729`. The P0/P1 below are that work (kept for record).
> Chosen next (2026-06-13, user): the three tasks here; hand-fused Metal kernels → BACKLOG.

#### P0 (NEXT): gateway-cc-codex — reliable local-LLM provider for Claude Code & Codex

**Sprint goal #2.** The outward gateway exists (`src/gateway.rs`: `/v1/chat/completions`
OpenAI + `/v1/messages` Anthropic + `/v1/models` + `/control/*`). Make it a *reliable*
drop-in for Claude Code and Codex against the in-process MLX/GGUF engine.
- [ ] gateway-prod-perf-verify — confirm the MLX win flows end-to-end: serve Qwen3.6-MoE
  via `/v1/chat/completions` and measure decode t/s (expect ~88); confirm `ROZUM_MLX_RETAIN`
  is set on the gateway→`LoadedModel::load` path. Add a cheap regression guard.
- [ ] gateway-cc-codex-audit — drive real Claude Code + Codex against the gateway; list
  concrete gaps (tool-use round-trips, streaming SSE shape/stop, `/v1/models` contents,
  model switching via `/control/switch`, cancel/error semantics, auth header).
- [ ] gateway-cc-codex-fixes — close the gaps found, with tests.

#### P1 (NEXT): meeting-room-reliability — solid "agents + human operator" room

**Sprint goal #1.** `meeting/` + the rozum MCP server exist. Harden the live flow.
- [ ] meeting-reliability-audit — map failure modes: turn integrity under concurrent
  submits, wakeup-channel delivery vs long-poll fallback, agent reconnect/leave, operator
  moderation (round-robin + manual), idle model-unload interactions.
- [ ] meeting-reliability-fixes — fix the concrete gaps, with tests.

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
- [~] dense-decode-dispatch - the 1.4× → now ~1.2× (16.2 → ~19 t/s) from the SAME shared GDN
  fix. Remaining 19 vs 23 is the leaner-graph / fused-op tail (smaller payoff than MoE was).

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
- [ ] **Stage 0 — probe (de-risk first).** Write a *plain*-`compile` probe (not
  `compile_with_state`): one decode step on the SMALL dense Qwen3 (0.6B/4B), fixed
  shapes + fixed-size cache, weights captured. A/B compiled vs uncompiled. Decides
  go/no-go on the cache redesign BEFORE building it.
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

- [ ] mlx-native-mistral - **CHEAP — likely a one-line alias.** Mistral / Mistral-Nemo
  (`model_type: "mistral"`) is architecturally Llama (GQA, no qkv-bias, SwiGLU, RoPE),
  and upstream `mlx_lm` serves it with the *llama* model class. So try routing
  `"mistral" => llama::load_llama_model` (+ `supported_model_type` += "mistral") with NO
  new fork file, then validate greedy byte-exact vs Python `mlx_lm` on a small Mistral
  MLX 4-bit. Caveat: Mistral uses sliding-window attention (window 4096); the llama path
  does full attention, so it diverges from the reference only for contexts **>window** —
  fine for the agent use case and bounded by the KV preflight. If a real divergence shows
  up at short context, fall back to a thin `mistral.rs` copy. Opens Mistral/Nemo (and the
  Mixtral base) at near-zero cost.

- [ ] mlx-native-llama-aliases - **TRIVIAL — verify, don't port.** SmolLM / TinyLlama /
  Llama-3.x variants already ship `model_type: "llama"`, so they should run on the
  existing Llama path as-is. Just download one small one and confirm greedy vs oracle;
  add to `models::RECOMMENDED` if it's a useful default. No code beyond a possible
  RECOMMENDED entry.

- [ ] mlx-native-fp16-verify - **TRIVIAL — verify only.** The AFQ loader already has a
  non-quantized branch (`quantization = None`), so bf16/fp16 MLX checkpoints should load
  (just more RAM). Confirm with one small fp16 MLX model; document it. No code expected.

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
- [~] mlx-native-perf - Phase 5: throughput. Spec section: `docs/specs/mlx-native-runtime.md`
  "Performance".

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

- [ ] channel-wakeup-capability - Declare `experimental:{"claude/channel":{}}` in the
  proxy `InitializeResult` + extend `instructions` to teach the agent to read
  `<channel source="rozum" …>` events as a wakeup (authoritative delta via `wait_my_turn`).
- [ ] channel-wakeup-pusher - Per-joined-room background task (modeled on `heartbeat_task`)
  that runs its own room long-poll and emits `notifications/claude/channel`
  (`content` = rendered delta, `meta` = `{room,from,seq,your_turn}`) on `upstream_peer`.
  Fire-and-forget; never crash the proxy/room conn on send failure.
- [ ] channel-wakeup-lifecycle - Abort the task on leave / room-switch / teardown
  (same points as `heartbeat_task`/`RoomConn`); de-dup own-authored turns; advance
  `since_seq` past delivered entries so reconnect doesn't replay a notification storm.
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
