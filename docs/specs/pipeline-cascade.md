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
