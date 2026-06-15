//! Cascade Router (`cascade-router`, Phase 1) — frugal/escalation model routing. Try the
//! cheapest model first; escalate to a stronger one only when the cheap answer isn't good
//! enough (the acceptance pipeline); stop at the first acceptable. The opposite of a parallel
//! ensemble — cheaper than a single frontier call on average. See
//! `docs/specs/cascade-router.md`.
//!
//! Phase 1: the deterministic core — a cost-ordered candidate list, the `AlwaysCheapest`
//! strategy, L0 (structural) acceptance, single-model passthrough, and budget-bounded
//! best-so-far. Availability/health (Phase 2), self-signal (P3), judge (P4), classifier (P5),
//! parallel lanes (P6), and learned stats (P7) layer on top.

mod acceptance;
mod classifier;
mod health;
mod judge;
mod self_signal;

pub use acceptance::{AcceptanceCheck, StructuralCheck, TurnOutcome, Verdict};
pub use classifier::{Classifier, HeuristicClassifier, RoutingStrategy};
pub use health::{classify, FailReason, HealthRegistry, HealthState};
pub use judge::{HeuristicJudge, Judge, JudgeConfig, ModelJudge};
pub use self_signal::{escalation_tools, EscalationAffordance, SelfSignalCheck};

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ModelError, ModelResult, StopReason,
};
use acceptance::pipeline_verdict;

/// Where a model runs — local (its own resource limits, e.g. memory) vs remote (the network +
/// the provider's quota/rate limits). A `Network` failure parks *all* remote models at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Location {
    Local,
    Remote,
}

/// A candidate model + its position in the cost order (lowest `tier` = cheapest/fastest).
pub struct ModelCard {
    pub id: String,
    pub backend: Arc<dyn ChatBackend>,
    pub tier: u32,
    pub location: Location,
}

/// Bounds on how far a request may escalate.
pub struct CascadeBudget {
    /// Max tier hops (so `max_escalations + 1` attempts).
    pub max_escalations: usize,
    /// Wall-clock ceiling across the whole cascade.
    pub wall_time: Option<Duration>,
}

impl Default for CascadeBudget {
    fn default() -> Self {
        Self { max_escalations: usize::MAX, wall_time: None }
    }
}

/// The candidate list + the acceptance pipeline + the budget. The caller supplies the
/// (cost-ordered) `models`; `CascadeBackend::new` re-sorts by `tier` to be safe.
pub struct CascadeConfig {
    pub models: Vec<ModelCard>,
    pub acceptance: Vec<Box<dyn AcceptanceCheck>>,
    pub budget: CascadeBudget,
    /// The escalation affordance injected into each non-top model's prompt (`None` = off).
    pub affordance: Option<EscalationAffordance>,
    /// The L2 judge, consulted only when L0/L1 are inconclusive (`None` = accept inconclusive).
    pub judge: Option<JudgeConfig>,
    /// How the cascade chooses its *entry* model (Phase 5).
    pub strategy: RoutingStrategy,
    /// The difficulty classifier for `ClassifyThenStart` (`None` → the built-in heuristic).
    pub classifier: Option<Box<dyn Classifier>>,
}

impl CascadeConfig {
    /// The default config over `models`: L0 (structural) + L1 (self-signal) acceptance, the
    /// escalation affordance on, no L2 judge (opt-in), unbounded budget.
    pub fn new(models: Vec<ModelCard>) -> Self {
        Self {
            models,
            acceptance: vec![Box::new(StructuralCheck), Box::new(SelfSignalCheck::default())],
            budget: CascadeBudget::default(),
            affordance: Some(EscalationAffordance::default()),
            judge: None,
            strategy: RoutingStrategy::AlwaysCheapest,
            classifier: None,
        }
    }
}

/// A [`ChatBackend`] that cascades over `config.models` cheapest-first, skipping models in
/// transient health cooldown and recording outcomes back into [`HealthRegistry`].
pub struct CascadeBackend {
    config: CascadeConfig,
    health: HealthRegistry,
}

impl CascadeBackend {
    pub fn new(mut config: CascadeConfig) -> Self {
        config.models.sort_by_key(|m| m.tier);
        Self { config, health: HealthRegistry::new() }
    }

    /// The shared health registry (observability / tests).
    pub fn health(&self) -> &HealthRegistry {
        &self.health
    }

    /// The cost-ordered index of the *entry* model for this request, per the routing strategy.
    /// `ClassifyThenStart` maps difficulty `0..1` onto `0..n-1` (round to nearest tier); the
    /// configured classifier wins, else the built-in heuristic. Never exceeds the top tier.
    fn start_index(&self, req: &ChatRequest, n: usize) -> usize {
        match self.config.strategy {
            RoutingStrategy::AlwaysCheapest => 0,
            RoutingStrategy::ClassifyThenStart if n > 1 => {
                let d = match &self.config.classifier {
                    Some(c) => c.difficulty(req),
                    None => HeuristicClassifier.difficulty(req),
                };
                ((d.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize).min(n - 1)
            }
            RoutingStrategy::ClassifyThenStart => 0,
        }
    }
}

/// Drive one model to completion, draining its stream into a [`TurnOutcome`].
async fn run_attempt(backend: &Arc<dyn ChatBackend>, req: ChatRequest) -> TurnOutcome {
    let mut o = TurnOutcome::default();
    let mut stream = match backend.chat(req).await {
        Ok(s) => s,
        Err(e) => {
            o.error = Some(e.to_string());
            return o;
        }
    };
    let mut cur: Option<(String, String, String)> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            Ok(ChatEvent::TextDelta { text }) => o.text.push_str(&text),
            Ok(ChatEvent::ToolUseStart { id, name }) => cur = Some((id, name, String::new())),
            Ok(ChatEvent::ToolUseDelta { input_json_delta, .. }) => {
                if let Some((_, _, a)) = &mut cur {
                    a.push_str(&input_json_delta);
                }
            }
            Ok(ChatEvent::ToolUseEnd { .. }) => {
                if let Some(c) = cur.take() {
                    o.tool_calls.push(c);
                }
            }
            Ok(ChatEvent::Done { input_tokens, output_tokens, stop_reason }) => {
                o.input_tokens = input_tokens;
                o.output_tokens = output_tokens;
                o.stop_reason = Some(stop_reason);
                break;
            }
            Err(e) => {
                o.error = Some(e.to_string());
                break;
            }
        }
    }
    o
}

/// Reconstruct the `ChatEvent`s to replay a collected outcome to the client (buffered — the
/// cascade has to see the whole answer to judge it, so the winner is emitted post-hoc). Any
/// escalation `marker` is stripped from the emitted text (a fallback answer mustn't leak it).
fn outcome_events(o: &TurnOutcome, marker: Option<&str>) -> Vec<ChatEvent> {
    let mut evs = Vec::new();
    let text = match marker {
        Some(m) => self_signal::strip_marker(&o.text, m),
        None => o.text.clone(),
    };
    if !text.is_empty() {
        evs.push(ChatEvent::TextDelta { text });
    }
    for (id, name, args) in &o.tool_calls {
        evs.push(ChatEvent::ToolUseStart { id: id.clone(), name: name.clone() });
        evs.push(ChatEvent::ToolUseDelta { id: id.clone(), input_json_delta: args.clone() });
        evs.push(ChatEvent::ToolUseEnd { id: id.clone() });
    }
    evs.push(ChatEvent::Done {
        input_tokens: o.input_tokens,
        output_tokens: o.output_tokens,
        stop_reason: o.stop_reason.unwrap_or(StopReason::EndTurn),
    });
    evs
}

fn buffered(events: Vec<ChatEvent>) -> ChatStream {
    Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
}

#[async_trait]
impl ChatBackend for CascadeBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        let models = &self.config.models;
        // Single model → passthrough (keeps live streaming; no arbitration).
        if models.len() == 1 {
            return models[0].backend.chat(req).await;
        }

        let cap = self.config.budget.max_escalations.saturating_add(1);
        let marker = self.config.affordance.as_ref().map(|a| a.marker.as_str());
        let start = Instant::now();
        let mut best_ok: Option<TurnOutcome> = None;
        let mut last_err: Option<String> = None;
        let mut attempts = 0usize;
        let last_idx = models.len() - 1;

        // Choose the entry tier. `AlwaysCheapest` starts at 0; `ClassifyThenStart` scores the
        // request once and maps difficulty → a proportional start tier. The candidate order is
        // then start-and-up (the natural escalation path), then the cheaper tiers below start as
        // availability fallbacks — so a parked entry tier still degrades to *something*.
        let start_idx = self.start_index(&req, models.len());
        let order: Vec<usize> = (start_idx..models.len()).chain((0..start_idx).rev()).collect();

        for &idx in &order {
            let card = &models[idx];
            if attempts >= cap || self.config.budget.wall_time.is_some_and(|wt| start.elapsed() >= wt)
            {
                break;
            }
            // Skip a model that's in health cooldown — route to the best *available* one.
            if !self.health.is_available(&card.id) {
                continue;
            }
            attempts += 1;
            // Give every non-top model the escalation affordance (the "skill") so it knows it can
            // defer instead of guessing. The top tier has nothing above it → no affordance.
            let attempt_req = match &self.config.affordance {
                Some(aff) if idx != last_idx => self_signal::inject_affordance(req.clone(), aff),
                _ => req.clone(),
            };
            let outcome = run_attempt(&card.backend, attempt_req).await;
            if let Some(e) = &outcome.error {
                let reason = classify(e);
                self.health.record_failure(&card.id, reason);
                // A network failure means the internet is gone — park every remote at once.
                if reason == FailReason::Network {
                    for c in models.iter().filter(|c| c.location == Location::Remote) {
                        self.health.record_failure(&c.id, FailReason::Network);
                    }
                }
                tracing::debug!(model = %card.id, ?reason, "cascade: model failed, routing around");
                last_err = Some(e.clone());
                continue;
            }
            self.health.record_success(&card.id);
            // L0/L1 (sync) decide first; if inconclusive, consult the L2 judge (async) if set.
            let verdict = match pipeline_verdict(&self.config.acceptance, &req, &outcome) {
                Some(v) => v,
                None => match &self.config.judge {
                    Some(jc) => {
                        if jc.judge.score(&req, &outcome).await >= jc.threshold {
                            Verdict::Accept
                        } else {
                            Verdict::Escalate
                        }
                    }
                    None => Verdict::Accept,
                },
            };
            match verdict {
                Verdict::Accept => {
                    tracing::debug!(model = %card.id, "cascade: accepted");
                    return Ok(buffered(outcome_events(&outcome, marker)));
                }
                _ => {
                    tracing::debug!(model = %card.id, "cascade: escalating");
                    best_ok = Some(outcome); // keep the most-capable usable answer so far
                }
            }
        }

        // Exhausted budget/list without an `Accept`: return the best usable answer (graceful
        // degradation), or error only if nothing was available or usable.
        match best_ok {
            Some(o) => Ok(buffered(outcome_events(&o, marker))),
            None => Err(ModelError::BackendUnavailable(last_err.unwrap_or_else(|| {
                if attempts == 0 {
                    "cascade: all candidate models are temporarily unavailable".into()
                } else {
                    "cascade: no model produced a usable answer".into()
                }
            }))),
        }
    }

    fn context_window(&self) -> u32 {
        self.config.models.iter().map(|m| m.backend.context_window()).max().unwrap_or(0)
    }

    fn label(&self) -> &'static str {
        "cascade"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{collect_to_string, ModelResult, SamplingParams};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    enum Script {
        Text(&'static str),
        Err(&'static str),
    }

    struct Mock {
        script: Script,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ChatBackend for Mock {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.script {
                Script::Err(e) => Err(ModelError::BackendUnavailable((*e).into())),
                Script::Text(t) => {
                    let evs: Vec<ModelResult<ChatEvent>> = vec![
                        Ok(ChatEvent::TextDelta { text: (*t).into() }),
                        Ok(ChatEvent::Done {
                            input_tokens: 1,
                            output_tokens: 1,
                            stop_reason: StopReason::EndTurn,
                        }),
                    ];
                    Ok(Box::pin(futures::stream::iter(evs)))
                }
            }
        }
        fn context_window(&self) -> u32 {
            u32::MAX
        }
    }

    fn card(id: &str, tier: u32, script: Script) -> (ModelCard, Arc<AtomicUsize>) {
        card_at(id, tier, Location::Local, script)
    }

    fn card_at(
        id: &str,
        tier: u32,
        location: Location,
        script: Script,
    ) -> (ModelCard, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let mc = ModelCard {
            id: id.to_string(),
            tier,
            location,
            backend: Arc::new(Mock { script, calls: calls.clone() }),
        };
        (mc, calls)
    }

    fn schema_req() -> ChatRequest {
        let mut r = ChatRequest::simple("give json");
        r.sampling = SamplingParams {
            response_schema: Some(json!({
                "type": "object",
                "properties": {"x": {"type": "integer"}},
                "required": ["x"]
            })),
            ..Default::default()
        };
        r
    }

    async fn collect(stream: ChatStream) -> String {
        collect_to_string(stream).await.unwrap()
    }

    #[tokio::test]
    async fn escalates_when_cheap_fails_structural() {
        let (c, cc) = card("cheap", 0, Script::Text("not json"));
        let (s, sc) = card("strong", 1, Script::Text("{\"x\": 1}"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(schema_req()).await.unwrap()).await;
        assert_eq!(out, "{\"x\": 1}", "the conformant strong answer wins");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn accepts_cheap_and_skips_strong() {
        let (c, _cc) = card("cheap", 0, Script::Text("{\"x\": 7}"));
        let (s, sc) = card("strong", 1, Script::Text("{\"x\": 1}"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(schema_req()).await.unwrap()).await;
        assert_eq!(out, "{\"x\": 7}");
        assert_eq!(sc.load(Ordering::SeqCst), 0, "strong is never called when cheap conforms");
    }

    #[tokio::test]
    async fn single_model_is_passthrough() {
        let (c, cc) = card("only", 0, Script::Text("hi"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c]));
        let out = collect(be.chat(ChatRequest::simple("x")).await.unwrap()).await;
        assert_eq!(out, "hi");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn errored_cheap_escalates() {
        let (c, _cc) = card("cheap", 0, Script::Err("rate limited"));
        let (s, sc) = card("strong", 1, Script::Text("{\"x\": 1}"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(schema_req()).await.unwrap()).await;
        assert_eq!(out, "{\"x\": 1}");
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn budget_caps_attempts_and_returns_best_so_far() {
        let (c, cc) = card("cheap", 0, Script::Text("bad1"));
        let (m, mc) = card("mid", 1, Script::Text("bad2"));
        let (s, sc) = card("strong", 2, Script::Text("{\"x\": 1}"));
        let mut cfg = CascadeConfig::new(vec![c, m, s]);
        cfg.budget.max_escalations = 1; // → 2 attempts
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(schema_req()).await.unwrap()).await;
        assert_eq!(out, "bad2", "best usable answer within budget (mid), not the unreached strong");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(mc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 0, "strong is beyond the budget");
    }

    #[tokio::test]
    async fn freeform_accepts_cheapest() {
        // No schema/tools → L0 is Inconclusive → accept the cheapest.
        let (c, _cc) = card("cheap", 0, Script::Text("hello"));
        let (s, sc) = card("strong", 1, Script::Text("world"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "hello");
        assert_eq!(sc.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn all_errored_yields_error() {
        let (c, _cc) = card("cheap", 0, Script::Err("down"));
        let (s, _sc) = card("strong", 1, Script::Err("down"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        assert!(be.chat(schema_req()).await.is_err(), "no usable model → error");
    }

    // ── Phase 2: availability / health ──────────────────────────────────────

    #[tokio::test]
    async fn parked_model_skipped_on_next_request() {
        // Request 1: cheap OOMs (a long cooldown) → strong wins. Request 2: cheap is in cooldown,
        // so it's skipped and strong serves directly.
        let (c, cc) = card("cheap", 0, Script::Err("out of memory"));
        let (s, sc) = card("strong", 1, Script::Text("{\"x\": 1}"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        assert_eq!(collect(be.chat(schema_req()).await.unwrap()).await, "{\"x\": 1}");
        assert_eq!(collect(be.chat(schema_req()).await.unwrap()).await, "{\"x\": 1}");
        assert_eq!(cc.load(Ordering::SeqCst), 1, "parked cheap was tried only once");
        assert_eq!(sc.load(Ordering::SeqCst), 2);
        assert_eq!(be.health().state("cheap"), HealthState::Unavailable);
    }

    #[tokio::test]
    async fn network_error_parks_all_remotes() {
        // local (escalates) → remote1 network-fails → that parks BOTH remotes, so remote2 is
        // never tried; we degrade to local's best-so-far.
        let (l, lc) = card_at("local", 0, Location::Local, Script::Text("nope"));
        let (r1, r1c) = card_at("remote1", 1, Location::Remote, Script::Err("connection refused"));
        let (r2, r2c) = card_at("remote2", 2, Location::Remote, Script::Text("{\"x\": 1}"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![l, r1, r2]));
        let out = collect(be.chat(schema_req()).await.unwrap()).await;
        assert_eq!(out, "nope", "degrades to the local best-so-far");
        assert_eq!(lc.load(Ordering::SeqCst), 1);
        assert_eq!(r1c.load(Ordering::SeqCst), 1);
        assert_eq!(r2c.load(Ordering::SeqCst), 0, "remote2 parked by remote1's network failure");
        assert_eq!(be.health().state("remote2"), HealthState::Unavailable);
    }

    #[tokio::test]
    async fn oom_on_big_local_falls_back_to_smaller() {
        // small escalates (best-so-far); big OOMs → parked; we fall back to small's answer.
        let (small, _sc) = card("small", 0, Script::Text("partial"));
        let (big, bc) = card("big", 1, Script::Err("Metal out of memory"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![small, big]));
        let out = collect(be.chat(schema_req()).await.unwrap()).await;
        assert_eq!(out, "partial", "falls back to the smaller model's answer");
        assert_eq!(bc.load(Ordering::SeqCst), 1);
        assert_eq!(be.health().state("big"), HealthState::Unavailable);
    }

    // ── Phase 3: self-signal ────────────────────────────────────────────────

    #[tokio::test]
    async fn self_signal_marker_escalates() {
        // The cheap model honestly defers via the marker → the strong model answers.
        let (c, cc) = card("cheap", 0, Script::Text("[[ESCALATE: not sure]]"));
        let (s, sc) = card("strong", 1, Script::Text("the answer"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(ChatRequest::simple("hard q")).await.unwrap()).await;
        assert_eq!(out, "the answer");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn marker_is_stripped_from_a_fallback_answer() {
        // Both defer; the top tier's answer is the fallback, with its marker stripped.
        let (c, _cc) = card("cheap", 0, Script::Text("[[ESCALATE: a]]"));
        let (s, _sc) = card("strong", 1, Script::Text("partial answer [[ESCALATE: b]]"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(ChatRequest::simple("q")).await.unwrap()).await;
        assert_eq!(out, "partial answer", "the marker is removed from the fallback");
    }

    // ── Phase 4: the L2 judge ───────────────────────────────────────────────

    #[tokio::test]
    async fn judge_escalates_low_quality_freeform() {
        // Free-form (L0/L1 inconclusive): the heuristic judge scores the cheap non-answer low →
        // escalate to the strong model.
        let (c, cc) = card("cheap", 0, Script::Text("I don't know"));
        let (s, sc) = card("strong", 1, Script::Text("Paris is the capital of France."));
        let mut cfg = CascadeConfig::new(vec![c, s]);
        cfg.judge = Some(JudgeConfig { judge: Box::new(HeuristicJudge), threshold: 0.5 });
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(ChatRequest::simple("capital of France?")).await.unwrap()).await;
        assert_eq!(out, "Paris is the capital of France.");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn no_judge_accepts_inconclusive_cheap() {
        // Without a judge, an inconclusive free-form answer is accepted (judge is opt-in).
        let (c, _cc) = card("cheap", 0, Script::Text("I don't know"));
        let (s, sc) = card("strong", 1, Script::Text("better"));
        let be = CascadeBackend::new(CascadeConfig::new(vec![c, s]));
        let out = collect(be.chat(ChatRequest::simple("q")).await.unwrap()).await;
        assert_eq!(out, "I don't know");
        assert_eq!(sc.load(Ordering::SeqCst), 0);
    }

    // ── Phase 5: difficulty classifier → ClassifyThenStart ──────────────────

    fn classify_cfg(models: Vec<ModelCard>) -> CascadeConfig {
        let mut cfg = CascadeConfig::new(models);
        cfg.strategy = RoutingStrategy::ClassifyThenStart;
        cfg
    }

    #[tokio::test]
    async fn classify_starts_trivial_prompt_at_cheapest() {
        // A bare greeting is trivial → start at tier 0; the stronger tiers are never touched.
        let (c, cc) = card("cheap", 0, Script::Text("hello"));
        let (m, mc) = card("mid", 1, Script::Text("MID"));
        let (s, sc) = card("strong", 2, Script::Text("STRONG"));
        let be = CascadeBackend::new(classify_cfg(vec![c, m, s]));
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "hello");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(mc.load(Ordering::SeqCst), 0);
        assert_eq!(sc.load(Ordering::SeqCst), 0, "trivial prompt never reaches the strong tier");
    }

    #[tokio::test]
    async fn classify_skips_cheap_on_a_hard_prompt() {
        // Code + math + multi-step → difficulty ≳ 0.75 → start above tier 0, so the cheap model
        // is bypassed entirely and the strong model answers directly.
        let (c, cc) = card("cheap", 0, Script::Text("weak"));
        let (s, sc) = card("strong", 1, Script::Text("strong answer"));
        let be = CascadeBackend::new(classify_cfg(vec![c, s]));
        let hard = "```\nfn solve() {}\n```\nprove the theorem step by step and analyze";
        let out = collect(be.chat(ChatRequest::simple(hard)).await.unwrap()).await;
        assert_eq!(out, "strong answer");
        assert_eq!(cc.load(Ordering::SeqCst), 0, "the cheap tier is skipped for a hard prompt");
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn classify_falls_back_below_start_when_entry_unavailable() {
        // Hard prompt routes to the strong tier, but it's down → fall back to the cheaper tier
        // below the entry point rather than failing.
        let (c, cc) = card("cheap", 0, Script::Text("cheap saves the day"));
        let (s, sc) = card("strong", 1, Script::Err("down"));
        let be = CascadeBackend::new(classify_cfg(vec![c, s]));
        let hard = "```\nfn solve() {}\n```\nprove the theorem step by step and analyze";
        let out = collect(be.chat(ChatRequest::simple(hard)).await.unwrap()).await;
        assert_eq!(out, "cheap saves the day", "degrades to the cheaper tier below the entry");
        assert_eq!(sc.load(Ordering::SeqCst), 1);
        assert_eq!(cc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn model_judge_scores_from_backend() {
        let calls = Arc::new(AtomicUsize::new(0));
        let jb: Arc<dyn ChatBackend> = Arc::new(Mock { script: Script::Text("8"), calls });
        let j = ModelJudge { backend: jb };
        let ans = TurnOutcome { text: "an answer".into(), ..Default::default() };
        let score = j.score(&ChatRequest::simple("q"), &ans).await;
        assert!((score - 0.8).abs() < 1e-3, "8/10 → 0.8, got {score}");
    }
}
