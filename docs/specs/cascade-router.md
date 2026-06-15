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

It is also **resilient**: model availability is transient — a remote can hit its token quota
(hourly/weekly/monthly), get rate-limited under load, go down, or the network drops; a big local
can OOM. The router tracks this as live, adaptive health, routes around a model that's failing
*right now* to the best **available** alternative (a remote outage → a local; a big-local OOM → a
smaller one), and recovers automatically when the model comes back — "do what we can with what's
available," never a hard failure while any model can serve.

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
- **L2 — cheap judge (one extra cheap call, only when L0/L1 are inconclusive).** A pluggable
  `Judge` scores the answer `0..1`; below `threshold` → escalate. Last, because it costs.
  Implementations: a free `HeuristicJudge` (empty / explicit-non-answer → low) and a `ModelJudge`
  (a small model rates the answer 0–10). Default judge = the next-cheapest local model + a
  heuristic.

```rust
trait AcceptanceCheck {            // L0/L1 — cheap, synchronous, cheapest-first in the config
    fn decide(&self, req: &ChatRequest, answer: &Turn) -> Verdict; // Accept | Escalate | Inconclusive
}
trait Judge { async fn score(&self, req, answer) -> f32; }  // L2 — consulted only on Inconclusive
```

L0/L1 run first (sync); the first `Accept`/`Escalate` decides. If every check is `Inconclusive`,
the L2 judge (if configured) decides (`score >= threshold` → `Accept`); with no judge, `Inconclusive`
defaults to `Accept` (don't escalate without a reason).

**Execution-feedback signal (agent context).** The most reliable quality signal isn't a judge's
opinion — it's whether the answer *worked*. In an agent loop a model's tool calls either succeed or
return a `ToolError`; the agent runtime already records this (`AgentOutcome.operations[].output:
Result<Value, String>`) and feeds each error back to the model for self-correction. This can't drive
the bare per-response cascade (the response is returned before the tools run), so it lives at the
**agent** level: (a) `run_agent` over a cascade escalates the backend when tool errors persist (a
model that keeps producing failing calls → a stronger tier for the next step), and (b) the per-model
tool-error rate per task-class feeds the learned stats (§ adaptive routing). Tracked as a follow-up
phase.

### Routing strategy (start-tier selection)

- **AlwaysCheapest** — start at tier 0, escalate on failure. Cheapest average; extra latency on
  hard tasks (several attempts).
- **ClassifyThenStart** — a cheap difficulty classifier (heuristic features: length, code/math
  markers, multi-step cues, tool count/complexity; or a tiny model) picks the **start tier**, so
  obviously-hard requests skip wasted cheap attempts. Then cascade from there.
- **Learned** — a data-driven `(task-class → start tier)` map (a bandit/threshold over the stats)
  layered on the classifier; adapts the start tier as evidence accumulates.

### Availability & health (transient, adaptive)

Quality-escalation goes *up*; **availability routing goes to the best model usable right now**,
which may be sideways or *down* (a remote that's failing → a local; a big local that OOM'd → a
smaller one). A model's availability is **transient** — it fails and recovers — so it's tracked
as runtime health, not a static property, with backoff and persisted history.

```rust
struct Health { state: HealthState, reason: FailReason, cooldown_until: Instant, fails: u32 }
enum HealthState { Healthy, Degraded, Unavailable }   // Degraded = recently flaky / half-open
enum FailReason { None, RateLimited, QuotaExhausted, Down, Network, OutOfMemory, Unknown }
```

- **Error classification.** Each backend error maps to a `FailReason` + a backoff: HTTP 429 →
  `RateLimited` (short backoff); 401/403 or a quota message → `QuotaExhausted` (long backoff —
  to the hour/day boundary if known); 5xx / timeout → `Down`; connection/DNS error → `Network`
  (applies to *all* remote tiers at once — the internet is gone); a local OOM → `OutOfMemory`
  for *that* model only (a smaller local still fits).
- **Selection respects health.** At every step the candidate set is the configured list **minus
  models in cooldown**, ordered by cost; pick the start tier (per strategy) among the *available*
  ones. So a configured order `[small-local, big-local, cheap-remote, frontier]` with the two
  remotes in cooldown degrades to the locals automatically; with all locals OOM-capped it uses
  the smallest that fits.
- **Recovery is automatic** (transient): `cooldown_until` expires → the model goes `Degraded`
  (half-open) and is retried on the next eligible request; a success → `Healthy`, another failure
  → re-`Unavailable` with a longer backoff (exponential + jitter). No manual reset.
- **Graceful degradation, never hard-fail.** If the *ideal* tier is unavailable, answer with the
  best available model rather than erroring — "do what we can with what's available." Only when
  *no* candidate is available does the request error.
- **Persisted + adaptive.** Health *events* (when/why a model failed and recovered) go in the
  stats store, so patterns carry forward across restarts (e.g. a model that reliably exhausts
  its monthly quota near month-end can be deprioritized proactively, and typical recovery times
  tune the backoff). Health is also a routing signal the `Learned` strategy can weight.

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

### Adaptive per-model concurrency (measured, not assumed)

Residency (above) is **cross-model** — which models can be co-resident. Orthogonal to it is each
model's own **throughput concurrency**: how many requests *one* model serves well before it
saturates, rate-limits, OOMs, or its answers degrade. That number **differs per model and can't be
assumed** — a small local may take 4–8 concurrent prefills, a big local 1, a generous remote dozens,
a metered remote 2 before 429s. So **measure it at runtime** and adapt, per model.

- **The actuators already exist.** Per-model throughput is the resizable admission limit
  (`AdmissionScheduler::set_limit` / `bump_limit` on the `AdmittingBackend` wrapping each backend);
  cross-model residency is the Phase-6 lane slots. The effective live concurrency of a model is
  `min(its adaptive limit, its lane's residency share)`. This phase adds only the **controller** that
  moves `set_limit`; Phase 6 is unchanged.
- **Signals, per location class:**
  - *Local* — the model's resident/peak memory + CPU/GPU utilization vs **free system** memory and
    compute headroom (so we raise concurrency only while resources allow, and back off before an
    OOM, not after).
  - *Remote* — the provider's reaction: 429 / `RateLimited` incidence, latency inflation under load,
    `QuotaExhausted` boundaries (don't probe up into a known-metered ceiling).
  - *All* — failure rate **and answer quality** (judge / execution-feedback score) **as a function of
    the concurrency level**: if quality or success drops when N rises, that N is too high for this
    model regardless of raw resources.
- **Controller: AIMD-style probe + back-off.** Per model, additively raise the limit while
  throughput improves and every signal stays green; on any red signal (429, OOM/near-OOM, latency
  cliff, quality/failure regression) multiplicatively back off and remember the safe ceiling. State
  is per-model and **persisted** in the stats store (the measured concurrency–throughput–quality
  curve carries across restarts; we don't re-learn from scratch each boot).
- **Composes with everything:** the lane gate still enforces residency; health/cooldown still parks
  failing models; the controller only tunes the *width* of each healthy lane to what that specific
  model has demonstrably sustained.

### Stats store (static + learned)

Per `(task-class, model)`: accepted / escalated counts, latency, realized cost, judge-score,
**plus health events** (failure `FailReason` + timestamp, and recovery) **plus the concurrency level
and a resource snapshot** at the time of each attempt (local mem/CPU headroom; remote
rate-limit/latency reaction) — so quality, latency, and failures are attributable to the concurrency
at which they happened, which is what the adaptive controller (above) consumes. Persisted as JSONL
(`memory_store` pattern) and replayed on start. Feeds: judge thresholds, the `Learned` start-tier
map, the registry's measured `speed`, the cost-vs-latency weighting, the availability backoff +
proactive deprioritization, and the **per-model concurrency curve**. `task-class` v1 buckets:
`{freeform, structured, tool-use}` × a coarse difficulty label.

### Escalation as a tool (optional, agent mode)

A `consult_stronger(question)` / `escalate(reason)` tool (a `ToolSource`) the model invokes when
it judges the task beyond it — the model self-routes one step up the cascade. Composes with the
agent runtime (`run_agent` + `MultiToolSource`). This is L1 made explicit + model-driven.

## Behavior

1. Resolve the `CascadeConfig` for the request (named / inline / default). 1 model → passthrough.
2. **Available set** = the configured list minus models in health cooldown (§ Availability).
   **Start tier** = `strategy`'s choice over the available set (0 for `AlwaysCheapest`).
3. **Dispatch** the request to the chosen model's **lane** (concurrent with other lanes).
4. Run the model; collect the `Turn` (text / tool calls / structured output).
   - On a backend **error**: classify it → update the model's `Health` (cooldown + backoff) →
     re-select the best **available** candidate (which may be a cheaper local — sideways/down,
     not just up) and go to 3. A `Network` error parks all remote tiers at once.
5. **Acceptance pipeline** (L0→L1→L2, enabled+ordered): `Accept` → return; `Escalate` → the next
   *available* tier up; `Inconclusive` → `Accept`.
6. Repeat from 3 until `Accept`, the available list is exhausted, or `budget` is hit (max
   escalations / wall-time / $). On exhaustion return the **best answer so far** (highest
   judge-score / last tier) with a `cascade` trailer in the response metadata. Only if *no*
   candidate was ever available does the request error.
7. **Record stats** for each attempt `(task-class, model, accepted?, latency, cost, score)` and
   any health transition.
8. The response carries which model finally answered + the path taken (escalations + any
   availability fallbacks), for observability.

Determinism: with `temperature: 0`, `AlwaysCheapest`, only L0 (structural), and all models
healthy, the whole thing is deterministic and model-free testable end-to-end — including the
availability fallback (a mock backend that errors → the next available is chosen deterministically).

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

**Status (2026-06-15): phases 1–7 shipped** (`src/cascade/`, 48 tests; P7 = stats store + `Learned`
start-tier, with adaptive thresholds / health-pattern persistence as a follow-up on the same store).
Remaining: 8 (execution-feedback escalation), 9 (adaptive per-model concurrency).

1. **Registry + pure cascade + L0 structural acceptance.** Caller-supplied list, `AlwaysCheapest`,
   escalate on error/structural-fail, single-model passthrough. Local→remote tiers via existing
   backends. Model-free e2e with mock backends (a cheap one that fails L0, a strong one that
   passes). The deterministic core.
2. **Availability & health-aware routing.** Error classification → `Health` (cooldown + exp
   backoff + jitter), skip models in cooldown, best-available selection (sideways/down fallback),
   graceful degradation, automatic half-open recovery. Model-free e2e: a "remote" mock that errors
   (429 / network) → falls to a healthy "local" mock; OOM on the big mock → the small mock.
3. **L1 self-signal + the `escalate`/`consult_stronger` tool.**
4. **L2 cheap judge** (next-cheapest-local + heuristic; pluggable).
5. **Difficulty classifier → `ClassifyThenStart`.**
6. **Parallel scheduler / lanes** (per-request difficulty routing, non-blocking lanes; residency
   policy — single-resident first, then multi-resident with `ConcurrencyBudget`).
7. **Learned stats + adaptive thresholds/start-tier + persisted health patterns** (the `Learned`
   strategy; JSONL carried across restarts).
8. **Execution-feedback escalation (agent context).** `run_agent` over a cascade escalates the
   backend when tool calls keep failing (`AgentOutcome.operations` errors), and the tool-error rate
   feeds the learned stats. The grounded "did it actually work" quality signal.
9. **Adaptive per-model concurrency** (§ Adaptive per-model concurrency). An AIMD controller per
   model that measures throughput / resources / rate-limits / quality vs the concurrency level and
   moves the resizable admission limit (`set_limit`) to each model's demonstrated sweet spot. Reuses
   the existing actuators (Phase 6 lanes + `AdmittingBackend`); consumes the Phase 7 stats
   (concurrency curve persisted across restarts). No change to Phase 6.

## Decisions

- Acceptance order is **fixed cheap→expensive** (L0 free → L1 free → L2 paid); the *set* and
  thresholds are configurable, the ordering principle is not.
- `Inconclusive` defaults to **Accept** — never escalate without a positive reason (keeps it cheap).
- Local cost is `$0` but the **latency** is weighted (`cost_weights`) so a "cheap but slow" local
  doesn't always beat a "slightly paid but instant" remote.
- The client sees only the **winning** model's stream, plus metadata of the path taken.
- Judge default = the **next-cheapest local model** + a heuristic, not a dedicated expensive judge
  (keep the verifier cheaper than the escalation target).
- **Availability is transient, not static**: a failing model is put in timed cooldown and probed
  again (half-open), never permanently blacklisted. Backoff length scales with the `FailReason`
  (rate-limit short; quota long, to a known reset boundary; network applies to all remotes at once).
- **Availability trumps the cost order** for *unavailable* models: routing always falls to the best
  *usable* model, even sideways/down (remote down → local; big-local OOM → smaller local) — "do
  what we can with what's available," and only hard-fail when nothing is available.

## Out of scope (for now)

- Parallel ensembling / consensus (the Fusion model) — explicitly the opposite of this feature.
- Training a dedicated router/classifier model (the v1 classifier is heuristic + small-model).
- Cross-process fleet coordination (`concurrency-cross-process`) — single-process lanes first.
