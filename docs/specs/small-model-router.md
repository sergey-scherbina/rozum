# Small model as router / classifier / RAG worker

## Overview

A small model (4B / Coder-7B) is the right tool for the **narrow, single-shot,
latency-sensitive** steps that don't need a frontier model: classify a query's
intent, pick a route (which model or tool to invoke), rerank/summarize retrieved
chunks, extract a structured field. Run those on the cheap tier and reserve the
big model for the actual hard work — the opposite of paying frontier latency for a
yes/no decision.

This is the *router/pre-filter* counterpart to the already-shipped **cascade**
(which answers small-first then *escalates on doubt*): the router decides **up
front** where to send the work; the cascade decides **after** a cheap attempt. They
compose — a router can pick the cascade's entry tier.

It builds on what's already here, not a new subsystem:
- `cascade::Classifier` (`difficulty(req) -> f32`) already has a documented hook
  *"or (future) a tiny classifier model"* — this fills it.
- `rag_lite::{Retriever, LexicalIndex, Hit}` already does BM25 retrieval — the RAG
  worker reranks/summarizes its hits with a small model.
- `cascade::ModelJudge` already shows the pattern (small `ChatBackend`, tight
  prompt, `temp 0`, small `max_tokens`, parse-with-fallback) — the router mirrors it.

## Interface

A small-model **classification** primitive, engine-agnostic (any `ChatBackend`):

```
ModelRouter { backend: Arc<dyn ChatBackend>, labels: Vec<Label> }
  async classify(&self, query: &str) -> Classification
Label { name: String, hint: String }     // caller-supplied label set + one-line guidance
Classification { label: String, confidence: f32, fallback_used: bool }
```

- The label set is **caller-supplied** (the cascade passes difficulty buckets; a
  gateway pre-filter passes routes like `chitchat|code|math|retrieval`; a RAG step
  passes `relevant|irrelevant`). No hard-coded taxonomy.
- The prompt is tight and instructs the model to reply with **only** the label;
  output is held to the label set (constrained decode when available — reuse
  `constrain` / `response_schema` enum — else parse-and-snap to the nearest label).
- **Always returns** — an unparseable / off-set reply falls back to the first
  label (or a caller default) with `fallback_used = true`, never an error. A small
  model that occasionally fumbles must not break the caller.
- Off by default / opt-in: nothing routes through a model classifier unless a
  caller constructs one.

## Behavior

- [ ] `classify` returns one of the supplied labels (constrained or snapped), with
      a confidence and a `fallback_used` flag; it never errors out.
- [ ] An empty label set is rejected at construction (`new` returns `Result`).
- [ ] Greedy (`temp 0`), tiny `max_tokens` — a label is a few tokens; latency is
      the point.
- [ ] Parsing tolerates the small model's noise: surrounding text, casing,
      punctuation, a leading "Label:" — snap to the unique label it names; ambiguous
      / none → fallback.
- [ ] A tiny **eval** on a real small model (M4, ignored test) shows it classifies
      a handful of clearly-labeled queries accurately enough to gate the big model
      (e.g. ≥ 80% on the eval set) — the acceptance bar from SPRINT.
- [ ] Composes: an adapter exposes a `Classifier`-shaped score for the cascade
      (map label → difficulty), and the gateway/launch can call `classify` as a
      pre-filter. (Wiring is P2; P1 ships the primitive + eval.)

## Out of scope (P1)

- RAG **rerank / summarize** worker over `rag_lite::Hit`s — same `ModelRouter`
  shape (`relevant|irrelevant` + a score), lands in **P2** once the primitive is
  proven (avoid building both speculatively).
- Wiring the router into the cascade's `RoutingStrategy` / the gateway request
  path — **P2** (the primitive is useful and testable standalone first).
- Embedding-based retrieval (separate `Retriever` impl; orthogonal).
- A learned/fine-tuned classifier — v1 is a prompted off-the-shelf small model.

## Design

- **`src/router.rs`** — `ModelRouter` + `Label` + `Classification`. `classify`
  mirrors `ModelJudge::score`: build a tight prompt enumerating the labels + hints,
  `temp 0`, small `max_tokens`, collect the text, then `snap_to_label(out, labels)`.
- **`snap_to_label`** (pure, unit-tested hardware-free): lowercase + trim, exact
  match → that label; else the unique label whose name appears as a token in the
  reply; else `None` (→ fallback). This is the noise-tolerance the small model needs
  and the part most worth testing without a GPU.
- **Constrained decode (best-effort):** when the backend honors a
  `response_schema` enum (the MLX/constrain path does), pass the label set as an
  enum so the output is forced on-set; `snap_to_label` is then the safety net for
  backends that don't. Keep it optional — the primitive must work over any
  `ChatBackend`.
- **Cascade adapter (P2):** a thin `Classifier` impl is impossible directly
  (`difficulty` is sync, `classify` is async); instead expose
  `ModelRouter::difficulty_of(label) -> f32` and let the async cascade entry call
  `classify` then map — no change to the sync `Classifier` trait.

## Decisions

- **Caller-supplied labels, not a fixed taxonomy** — the same primitive serves
  routing, RAG relevance, and difficulty bucketing; the caller owns the meaning.
- **Never errors; always falls back to a label** — a cheap pre-filter must degrade
  gracefully (a fumbled classification just routes conservatively), mirroring
  `ModelJudge`'s neutral-on-failure rule.
- **`snap_to_label` is the tested core; the model call is the eval** — the noise
  tolerance is pure and engine-free (fast unit tests); model accuracy is proven by
  an ignored M4 eval, like the spec-decode Metal test.
- **Ship the classifier first, RAG-rerank second** — one proven entrypoint
  (SPRINT acceptance) beats two speculative ones (the premature-abstraction trap the
  portability spec warns of); rerank reuses the exact same shape in P2.

## Verification

- Fast unit tests (no model): `snap_to_label` cases (exact, cased, punctuated,
  "Label: X", ambiguous→none, off-set→none), empty-label-set rejected, fallback
  path sets `fallback_used`.
- M4 eval (`#[ignore]`, real 4B): a small labeled query set (intent/route) →
  assert accuracy ≥ the bar. Run: `cargo test --features mlx-native -- --ignored
  --nocapture model_router_eval`.

## Results

**P1 DONE** (`src/router.rs`). `ModelRouter` + `Label` + `Classification` +
`snap_to_label`; 8 hardware-free unit tests (snap exact/cased/prefix/substring-
false-positive/ambiguous/off-set, empty-set rejected, fallback path).

**M4 eval — PASSED, 100%.** `model_router_eval` (Qwen3-4B-4bit, 6 labeled queries
across code / math / chitchat): **6/6 correct**, every one an *exact* match
(confidence 1.0, no fallback). The plain prompt + `snap_to_label` was enough — the
model emitted exactly the label name each time, so constrained decode wasn't needed
for this label set (kept as a P2 option for noisier/larger sets). Well above the
0.80 gate-the-big-model bar.

**Next (P2):** the RAG rerank/summarize worker (same `ModelRouter` shape over
`rag_lite::Hit`s) and wiring `classify` into the cascade entry / gateway pre-filter
(async-classify → `difficulty_of(label)` → entry tier; no change to the sync
`Classifier` trait).
