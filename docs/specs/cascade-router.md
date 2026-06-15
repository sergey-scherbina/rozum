# Cascade Router (frugal / escalation routing)

## Goal

Use models **efficiently and cheaply** by *cascading* instead of ensembling: try the
cheapest/fastest model first (smallest local → bigger local → cheap remote → … →
frontier), and **escalate to a stronger model only when the cheap answer isn't good
enough**. Stop at the first acceptable answer. The opposite of a parallel ensemble
(OpenRouter Fusion runs all models + a judge → ~4–5× the cost); a cascade is cheaper than a
single frontier call on average, because most requests are satisfied by a cheap tier.

The caller supplies the **candidate model list** (inline or via a named config); arbitration
runs over *that* list. One model → no arbitration (passthrough). Everything — which escalation
triggers fire and in what order, the routing strategy, the thresholds, the cost weights — is
**configurable**, and the router **adapts** from accumulated statistics over time. A busy
fleet is **scheduled in parallel**: each concurrent request is routed to the tier its
difficulty warrants, and the cheap lane and the heavy lane run concurrently so a simple
request never waits behind a complex one.

## Scope

New, mostly self-contained, composing what already exists:

- `src/cascade/` — the router: model registry, the acceptance pipeline, the routing strategy,
  the per-request scheduler, the stats store, and a `CascadeBackend: ChatBackend`.
- Reuses: `BackendOrchestrator` (the existing `Fallback`/`FanOut` strategies — the cascade is a
  smarter `Fallback`); the OpenAI/Anthropic **HTTP client backends** (`openai_http`,
  `anthropic_http`) as the remote tiers; `constrain` (structural acceptance); `memory_store`
  (the JSONL pattern for learned stats); `models::{RECOMMENDED, scan_all_installed}` (registry
  seed); `concurrency::admit_wrap` (per-backend admission under the lanes); the gateway
  (request surface).
- Subsumes the concurrency items `concurrency-multi-instance` (size-class routing across
  several models) and overlaps `shared-gateway-multislot` (more than one resident model).

## Concepts

### Model registry (cost-sorted, local → remote)

A `ModelCard` per candidate:

```rust
struct ModelCard {
    id: String,            // stable id (the spec, or a config alias)
    backend: BackendRef,   // mlx-native | gguf | openai-http{base,model} | anthropic-http{…} | …
    tier: u32,             // rank in the cost order (0 = cheapest)
    cost: Cost,            // $/Mtok in+out (≈0 for local) — money
    speed: Speed,          // measured tok/s + time-to-first-token — latency
    capability: f32,       // a coarse capability score (seeds the difficulty→tier map)
    context_window: u32,
    modality: Modality,    // text | +vision …
}
```

Local and remote are one cost-ordered list; remote is itself a sorted sub-cascade
(`gpt-mini → claude-haiku → … → claude-opus`). The static fields seed from config +
`models::RECOMMENDED`; the **learned** fields (`speed`, acceptance rates) refine over time.

### Cascade config (caller-supplied, selectable)

```rust
struct CascadeConfig {
    models: Vec<ModelCard>,        // the candidate list, cost-ordered (the caller's choice)
    strategy: RoutingStrategy,     // AlwaysCheapest | ClassifyThenStart | Learned
    acceptance: Vec<AcceptanceCheck>, // ordered, cheapest-first (L0 structural → L1 self → L2 judge)
    judge: Option<JudgeRef>,       // model/heuristic for L2 (default: next-cheapest-local + heuristic)
    classifier: Option<ClassifierRef>, // for ClassifyThenStart
    cost_weights: CostWeights,     // money vs latency tradeoff (so "cheap but slow" ≠ always best)
    budget: CascadeBudget,         // max escalations, max wall-time, max $ per request
}
```

**Selection.** A request picks a config by name or supplies one inline:
- `model: "cascade"` → the default config; `model: "cascade:<name>"` → a named config.
- inline override in the body: `{"cascade": {"models": [...], "config": "<name>", …}}`.
- Named configs live in a config file (`runtime-config.md` pattern); resolved at request time.
- **A single-model list is a passthrough** — no classifier, no judge, no escalation.

### Acceptance pipeline (the escalation decision, cheapest-check-first)

After each model answers, decide accept-vs-escalate by running the enabled checks **in order,
bailing to escalate on the first failure**, cheapest checks first so we rarely pay for L2:

- **L0 — structural (free, deterministic).** Did it produce output? For a request with a
  schema / tool requirement: does the output pass JSON-Schema conformance (`constrain`),
  tool-arg validity, or parse/compile? **Fail → escalate immediately** (don't judge garbage).
  Pass → strong accept for structured tasks.
- **L1 — self-signal (free, same response).** Did the model call an `escalate` tool, refuse, or
  emit a low-confidence marker? → escalate.
- **L2 — cheap judge (one extra cheap call, only when L0/L1 are inconclusive).** A small local
  model or heuristic scores the answer; below `threshold` → escalate. Last, because it costs.

```rust
trait AcceptanceCheck {            // each is cheap-before-expensive ordered in the config
    fn decide(&self, req: &ChatRequest, answer: &Turn) -> Verdict; // Accept | Escalate | Inconclusive
}
```

`Accept` short-circuits (return). `Escalate` goes to the next tier. `Inconclusive` falls to the
next check; if all are `Inconclusive`, the default is `Accept` (don't escalate without a
reason).

### Routing strategy (start-tier selection)

- **AlwaysCheapest** — start at tier 0, escalate on failure. Cheapest average; extra latency on
  hard tasks (several attempts).
- **ClassifyThenStart** — a cheap difficulty classifier (heuristic features: length, code/math
  markers, multi-step cues, tool count/complexity; or a tiny model) picks the **start tier**, so
  obviously-hard requests skip wasted cheap attempts. Then cascade from there.
- **Learned** — a data-driven `(task-class → start tier)` map (a bandit/threshold over the stats)
  layered on the classifier; adapts the start tier as evidence accumulates.

### Parallel scheduling (lanes)

Concurrent requests are dispatched by difficulty so the cheap lane and the heavy lane run
**concurrently and non-blocking**:

- Each request is classified → a target **lane** (a tier/model). Lanes execute independently.
- **Remote lanes** are trivially parallel (HTTP). **Local lanes** are memory-gated: models that
  can co-reside (`shared-gateway-multislot`, the `ConcurrencyBudget` says both fit) run in
  parallel; mutually-exclusive big locals are serialized within their lane (per-lane queue +
  `admit_wrap`) but never block a *different* lane. So a simple request on the small fast model
  is not stuck behind a complex request on the big model.
- The scheduler sits **above** per-backend admission; it owns lane assignment + the residency
  policy, and delegates within-lane concurrency to the existing `concurrency::admit_wrap`.

### Stats store (static + learned)

Per `(task-class, model)`: accepted / escalated counts, latency, realized cost, judge-score.
Persisted as JSONL (`memory_store` pattern) and replayed on start. Feeds: judge thresholds, the
`Learned` start-tier map, the registry's measured `speed`, and the cost-vs-latency weighting.
`task-class` v1 buckets: `{freeform, structured, tool-use}` × a coarse difficulty label.

### Escalation as a tool (optional, agent mode)

A `consult_stronger(question)` / `escalate(reason)` tool (a `ToolSource`) the model invokes when
it judges the task beyond it — the model self-routes one step up the cascade. Composes with the
agent runtime (`run_agent` + `MultiToolSource`). This is L1 made explicit + model-driven.

## Behavior

1. Resolve the `CascadeConfig` for the request (named / inline / default). 1 model → passthrough.
2. **Start tier** = `strategy`'s choice (0 for `AlwaysCheapest`; classifier/learned otherwise).
3. **Dispatch** the request to the start tier's **lane** (concurrent with other lanes).
4. Run the model; collect the `Turn` (text / tool calls / structured output).
5. **Acceptance pipeline** (L0→L1→L2, enabled+ordered): `Accept` → return; `Escalate` → tier+1.
6. Repeat from 3 at the next tier until `Accept`, the list is exhausted, or `budget` is hit
   (max escalations / wall-time / $). On exhaustion return the **best answer so far** (highest
   judge-score / last tier) with a `cascade` trailer in the response metadata.
7. **Record stats** for each attempt `(task-class, model, accepted?, latency, cost, score)`.
8. The response carries which model finally answered + the escalation path (observability).

Determinism: with `temperature: 0` and `AlwaysCheapest` + only L0 (structural), the whole thing
is deterministic and model-free testable end-to-end.

## Interface (surface)

- `CascadeBackend: ChatBackend` — drop-in; the gateway routes `model: "cascade[:name]"` to it.
  Its `chat()` runs the algorithm and streams the *accepted* model's output (the escalation
  attempts before it are not streamed to the client; only the winner is).
- `/stats` gains a `cascade` block: per-tier attempt/accept/escalate counts, realized savings
  (vs always-frontier), avg escalations/request.
- Config: a `cascade` section in the runtime config + named configs; env overrides for the
  global knobs (e.g. `ROZUM_CASCADE_DEFAULT=<name>`).

## Design notes

- The cascade is a **smarter `BackendOrchestrator::Fallback`**: Fallback escalates on *error*,
  the cascade escalates on *the acceptance verdict* (which includes errors). Build it as a new
  orchestration mode reusing the backend registry.
- The remote tiers are plain `ChatBackend`s (`openai_http`/`anthropic_http` with `with_api_key`),
  so they need no special-casing — they're just high-`tier` `ModelCard`s.
- L0 structural reuses `constrain::Schema`/`Constraint` (already validates schema + tool form);
  for `response_format` requests the conformance check is free and exact.
- The scheduler's lane/residency policy is where this meets `shared-gateway-multislot` /
  `concurrency-multi-instance`; keep the lane abstraction so single-resident (today) and
  multi-resident (later) are the same code with a different residency policy.

## Phased delivery

Each phase ships value and is testable; early phases are deterministic/model-free.

1. **Registry + pure cascade + L0 structural acceptance.** Caller-supplied list, `AlwaysCheapest`,
   escalate on error/structural-fail, single-model passthrough. Local→remote tiers via existing
   backends. Model-free e2e with mock backends (a cheap one that fails L0, a strong one that
   passes). The deterministic core.
2. **L1 self-signal + the `escalate`/`consult_stronger` tool.**
3. **L2 cheap judge** (next-cheapest-local + heuristic; pluggable).
4. **Difficulty classifier → `ClassifyThenStart`.**
5. **Parallel scheduler / lanes** (per-request difficulty routing, non-blocking lanes; residency
   policy — single-resident first, then multi-resident with `ConcurrencyBudget`).
6. **Learned stats + adaptive thresholds/start-tier** (the `Learned` strategy; persisted JSONL).

## Decisions

- Acceptance order is **fixed cheap→expensive** (L0 free → L1 free → L2 paid); the *set* and
  thresholds are configurable, the ordering principle is not.
- `Inconclusive` defaults to **Accept** — never escalate without a positive reason (keeps it cheap).
- Local cost is `$0` but the **latency** is weighted (`cost_weights`) so a "cheap but slow" local
  doesn't always beat a "slightly paid but instant" remote.
- The client sees only the **winning** model's stream, plus metadata of the path taken.
- Judge default = the **next-cheapest local model** + a heuristic, not a dedicated expensive judge
  (keep the verifier cheaper than the escalation target).

## Out of scope (for now)

- Parallel ensembling / consensus (the Fusion model) — explicitly the opposite of this feature.
- Training a dedicated router/classifier model (the v1 classifier is heuristic + small-model).
- Cross-process fleet coordination (`concurrency-cross-process`) — single-process lanes first.
