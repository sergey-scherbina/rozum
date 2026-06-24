# Pipeline cascade — a live, transparent chain of models behind one agent

## Goal (operator vision, 2026-06-23)

> "I just launch an agent with a chain of models and they form a pipeline where the first model tells
> the next what to do — automatically. The models load together if there's enough RAM, or one at a time
> if not. And one-at-a-time goes **round-robin**: every iteration (one prompt) uses both (or more) models,
> then it returns to the first model again for the next prompt."

Concretely:

```
rozum launch --model GLM-4-32B,gpt-oss-20b codex exec "<task>"
```

The agent (codex/claude/opencode) sees **one** OpenAI-compatible endpoint and is unaware there are two
models behind it. On **every** request the agent makes, the gateway runs the request through all tiers in
order — tier 0 (planner) produces guidance, tier 1 (executor) consumes `[request + guidance]` and emits
the actual assistant turn (tool calls / final answer) that returns to the agent. The next request starts
again at tier 0. That per-prompt round-robin is the operator's "по кругу".

This is the **live, in-process** counterpart to the batch [`solve.sh` / planner→executor](planner-executor.md)
workflow: same decomposition (think → do), but transparent inside a single agent invocation instead of two
separate `rozum` runs.

## Relationship to the existing cascade

The [cascade router](cascade-router.md) today is **escalation only** — `RoutingStrategy::{AlwaysCheapest,
ClassifyThenStart, Learned}` pick a START tier, then move up **only on a `Verdict::Escalate`** from the
acceptance checks. Not every prompt touches every tier; a strong-enough cheap answer stops at tier 0.

The pipeline is the **other** routing semantic from the unification table in
[planner-executor.md](planner-executor.md):

| | transition (when to advance) | handoff (what the next stage gets) |
|---|---|---|
| **escalation cascade** (exists) | escalate **on a verdict** | the **same input** flows on |
| **pipeline cascade** (this spec) | **always advance**, every tier, every prompt | **stage-1 output → stage-2 input** |

So this is a new `RoutingStrategy::Pipeline` on the existing `CascadeBackend`, not a new backend type. It
reuses the tier list, the `--model A,B` parse (`from_model_list`), and the admission footprint logic.

## Design

### 1. The pipeline pass (per request)

`CascadeBackend::chat()` under `RoutingStrategy::Pipeline`:

```
input := request.messages
for i, tier in tiers:                       # 0..N, in order, ALWAYS all of them
    backend := residency.acquire(tier)      # eager: instant; lazy: swap (see §2)
    if i < last:                            # planner stage(s)
        guidance := backend.chat(input, planner_framing).text
        input := input + assistant_block(guidance)   # forward-output handoff
    else:                                   # final executor stage
        return backend.chat(input, request.tools)     # real tool-calls go back to the agent
```

- **Only the last tier** is given the agent's real `tools` / response schema and its output is what the
  agent receives. Earlier tiers are "advisors": they get a planner framing ("think about what should be
  done for the next step; do NOT call tools, just lay out the plan") and their text is appended as an
  assistant/system scratchpad the next tier reads.
- **Forward-output handoff**: tier i's output augments the context for tier i+1 (not a replacement of the
  user prompt — the executor still sees the original request, plus the plan).
- `temperature` for planner tiers can default low (0.2) for stable plans; the executor keeps the request's.

### 2. Adaptive residency (`adaptive-cascade-residency`)

Decided once, at gateway admission, from the per-tier footprints:

| condition | mode | behaviour |
|---|---|---|
| `SUM(local tier footprints) + keep_free ≤ available` (via `share::admits`) | **EAGER** | all tiers resident; `residency.acquire` returns the live `Arc<backend>` — **no swap**, fast |
| SUM doesn't fit, but `MAX(single tier) + keep_free ≤ available` | **LAZY** | hold tier **specs**; `residency.acquire(tier)` loads it (unloading the previous) via the gateway **Switchboard**, returns the fresh backend |
| even MAX doesn't fit | **refuse** | admission rejects (existing no-reboot behaviour) — no partial load |

The swap engine already exists: gateway `Switchboard` ("never two resident — next chat lazily rebuilds
from spec", `gateway.rs:111`). Lazy residency = the pipeline driving that load/unload between tiers,
exactly what `solve.sh` does by hand with two sequential gateway processes — but in-process and automatic.

> **HARD CONSTRAINT (validated 2026-06-23, reproducible): two MLX models must NOT be co-resident in one
> process.** Live testing GLM-9B+Qwen3-4B eager (both loaded, ~7 GiB total, RAM not the issue) showed that
> when one model runs a generation while the other's weights also sit in the Metal heap, the GPU command
> buffer exceeds the watchdog → `[METAL] Command buffer execution failed: GPU Timeout Error
> (kIOGPUCommandBufferCallbackErrorTimeout)` → uncaught C++ exception → the gateway process crashes (no
> kernel panic / no reboot, but the whole gateway dies). Reproduced on a clean system with settled GPU.
> Therefore the **eager** branch below is for **remote / non-MLX** tiers only; **any MLX×MLX local pair is
> forced LAZY** (one resident at a time, the other fully torn down first — `solve.sh`'s proven model).
> This makes lazy residency mandatory, not an optimization, for the common local-model case.

> **LAZY STATUS (built + isolated 2026-06-24).** `LazyPipelineBackend` is implemented: per request it
> resolves tier 0 (planner) → plans → tears it down → resolves tier N (executor) → answers → tears down,
> serialized, never co-resident. Admission reserves MAX(local tier) not SUM. **Validated:** no
> co-residency crash (gateway survives round-robin), and a **same-model** lazy pipeline
> (`Qwen3-4B,Qwen3-4B`) works end-to-end — so the load→teardown→load mechanism is sound. **Remaining
> bug (precisely isolated):** a `GLM-9B → Qwen3-4B` lazy pipeline fails the *executor's* first eval
> (`mlx: eval failed`). It is GLM-specific cross-model contamination: GLM-9B's generation leaves pending
> MLX-stream state (its kernels pre-build/async-eval the next token) that a *different* next model's eval
> inherits. Proof it's fixable: the gateway `Switchboard` swaps GLM-9B→Qwen3-4B in-process cleanly (its
> drained swap flushes the stream).
>
> **UPDATE (fix #5 attempted + exhaustively diagnosed 2026-06-24): a teardown stream flush does NOT fix
> it.** `mlx_synchronize` is now exposed (mlx-sys already binds it; `Stream::as_ptr` is public) and called
> at `MlxNativeBackend` teardown — confirmed running (rc=0) — but GLM-9B→Qwen3-4B still fails. Ruled out,
> each tested live: stream-flush, MLX cache-evict, peak-reset, settle-before-build, settle-after-build,
> inline-vs-spawn_blocking drop. The build path is IDENTICAL (the gateway builder calls the same
> `build_from_config`). Controls: the gateway `Switchboard` swaps GLM-9B→Qwen3-4B in-process cleanly even
> after a long GLM generation; and Qwen3-4B→Qwen3-4B lazy works. **⇒ The root cause is STRUCTURAL, not MLX
> state: the Switchboard runs each model as a SEPARATE top-level gateway request; the lazy pipeline runs
> both NESTED in one request — failure is specific to GLM as the first tier.** The real fix is to route the
> lazy pipeline's per-tier load/generate through the gateway's separate-request swap path (architectural).
> Kept wins: the `mlx_synchronize` flush (teardown hygiene) + `reset_peak_memory` at teardown (fixes the
> lazy footprint-cache poisoning). Robust path today: `solve.sh` (separate processes) for GLM chains;
> same-model and non-GLM small pairs work in-process.

**Cost (honest):** in lazy mode every prompt pays `N-1` swaps in + the swap back to tier 0 for the next
prompt — i.e. ~2 model loads per agent turn (load = seconds to tens of seconds for 20–32B). Eager pays
nothing. The two reference models (GLM-4-32B ≈18 GB + gpt-oss-20b ≈11 GB ≈ 29 GB + MLX cache peaks) sit
right at the 36 GB co-fit edge → often lazy. **Eager is the target; lazy is the safe, slower fallback.**
This is measured, not assumed (validation §5).

### 3. Residency abstraction

```rust
enum Residency {
    Eager(Vec<Arc<dyn ChatBackend>>),     // index by tier
    Lazy { specs: Vec<TierSpec>, swap: SwapHandle },  // SwapHandle → gateway load/unload
}
impl Residency { async fn acquire(&self, tier: usize) -> Arc<dyn ChatBackend>; }
```

`SwapHandle` is the integration seam to the gateway Switchboard. For unit tests it's a stub that returns
pre-built Echo backends; in the gateway it triggers a real `switch`.

## Invocation / opt-in syntax

`--model A,B` today builds an **escalation** cascade (`from_model_list`). To avoid silently changing that:

- **Primary (this spec):** `--model A,B` defaults to **pipeline** when launched under an agent — this is
  the operator's stated mental model ("chain of models = pipeline"). Escalation stays available via the
  named `cascade:<name>` specs and `[cascade]` config `strategy = "escalation"`.
- Alternatively a `[pipeline]` config block / `--pipeline` flag selects it explicitly.

Decision recorded in SPRINT (`pipeline-cascade`); default is pipeline-for-comma, with escalation as the
named/explicit opt-in, so the common case is the operator's one-liner.

## Validation plan (isolate discipline — prove the mechanism, then the value)

1. **Mechanism, eager, deterministic:** two SMALL co-fitting models (e.g. two ≤4B) as `A,B`; a real agent
   (codex) does a task; assert each turn ran A→B (logs show two completions per request) and the agent
   received B's tool-calls. Eager → no swap → deterministic. Proves transparency end-to-end.
2. **Mechanism, lazy:** force lazy (cap RAM / use the big pair); assert one model resident at a time
   (slot probe), the swap fires per turn, 0 reboot. Measure per-turn swap wall-clock — report it honestly.
3. **Value, A/B:** on a create-from-scratch task GLM+gpt-oss pipeline vs gpt-oss alone (the
   planner→executor RPN result — pipeline 3/3 vs gpt-oss-alone 2/4 — is the batch baseline; reproduce the
   win live through the agent). Control: gpt-oss→gpt-oss pipeline to confirm the win is the *planner*, not
   just "two passes".
4. Done-when: live pipeline runs transparently through codex; eager + lazy both work; lazy is 0-reboot
   and slot-gated; the per-turn swap cost is measured and documented; value A/B ≥ batch result.

## Risks / open

- **Lazy swap latency dominates tight agent loops.** Mitigation already specced: prefer eager; the operator
  chose "every turn" knowingly. A later `plan-once` cadence (planner only on turn 1) is a cheap variant if
  the per-turn cost proves too high — out of scope for the first cut.
- **Planner over-steering a weak executor**, or the executor ignoring the plan. The executor's own
  tool-loop + the agent's verify step is the safety net (same as batch).
- **Context growth**: appending each planner's text grows the prompt every turn. Cap/trim planner guidance
  to a budget; don't accumulate across turns (each turn re-plans from the current agent state).
- **Streaming**: the final (executor) tier streams to the agent as normal; planner tiers are awaited fully
  (their text is internal). No change to the agent-facing streaming contract.

## The full frame — a verification-gated escalation chain (operator, 2026-06-24)

The chain combines several models into ONE composite, smarter "model" by **correct sequential use +
adaptive resource management**. The unifying mechanism is **verification-gated escalation with feedback**
— broader than fixed planner→executor→verifier roles (those are one configuration of it):

1. **Try → verify → escalate.** Load a model, use it, then **verify the result**. If it is not good
   enough, hand the result **together with the original task** to the NEXT model in the chain, and repeat
   — until a result we accept, or the chain is exhausted. Each model is an *attempt*; verification is the
   gate; the imperfect result is the *feedback* carried forward (not thrown away).
2. **Verification = the soul (already built).** The deterministic gate (`cargo` etc., run by rozum on the
   workdir — `rozum launch` verify-gate) is ground truth; a model-verifier (the backend's verifier role)
   is an optional pre-flight review. A model never decides "done" — the check does.
3. **Adaptive residency.** If RAM allows, **cache** (keep models resident) for speed; if not, **swap**
   (unload one, load the next). Same admission/footprint/swap machinery already in place
   ([[safe-multi-model-residency]], MemAvailable, the in-process swap fix). Parallelism where it fits.
4. **Role-aware quality stats + exclusion.** Track each model's quality *per role* over runs; a model that
   is consistently bad gets **dropped from the chain** when there is an alternative. (Extends the existing
   `Learned` routing / `Verdict`/acceptance signal.)
5. **Cloud tiers last.** The final links may be **remote cloud** models — used only when local links
   haven't produced an acceptable result, AND the cloud is reachable + within limits — so cloud usage is
   *saved*. If cloud is down/over-limit, fall back to local links used optimally (per the above).

**Status.** Done: sequential one-at-a-time chain + swap (no-reboot); the verification + feedback +
orchestration core — `rozum launch` deterministic verify-gate (re-invokes the same model with the real
error) + backend verifier role + repair; adaptive load/swap admission. **To build:** (a) escalation
ACROSS the chain on persistent verify-failure — switch to the NEXT model with (task + best-result-so-far),
not just re-invoke the same one; (b) cache-when-fits vs swap-when-not as an explicit residency policy for
the chain; (c) role-aware quality stats + auto-exclude; (d) cloud-last ordering with availability/limit
checks + local fallback. These layer onto the existing cascade/escalation + residency infrastructure.
