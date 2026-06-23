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
mod scheduler;
mod self_signal;
mod spec;
mod stats;
mod tasks;

pub use acceptance::{AcceptanceCheck, StructuralCheck, TurnOutcome, Verdict};
pub use classifier::{Classifier, HeuristicClassifier, RoutingStrategy};
pub use health::{classify, FailReason, HealthRegistry, HealthState};
pub use judge::{HeuristicJudge, Judge, JudgeConfig, ModelJudge};
pub use scheduler::{Lane, LaneSet};
pub use self_signal::{escalation_tools, EscalationAffordance, SelfSignalCheck};
pub use spec::{
    build_cascade, classify_model_name, from_model_list, from_model_pipeline, parse_cascade_model,
    CascadeSpec, RemoteApi, StrategyName, TierSpec,
};
pub use stats::{
    AttemptRecord, DiffBucket, ModelTaskStat, ResourceSnapshot, StatsStore, TaskClass, TaskShape,
};
pub use tasks::{commit_message_request, small_task_config, CommitMessageGate, SmallTask};

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
    ModelResult, Role, StopReason,
};
use acceptance::pipeline_verdict;

/// Sampling temperature for an advisor/planner tier — low, for a stable, deterministic-ish plan.
const PLANNER_TEMPERATURE: f32 = 0.2;
/// Token budget for an advisor's plan (the executor gets the full budget; a plan stays concise).
const PLANNER_MAX_TOKENS: u32 = 800;
/// The framing handed to every non-final (advisor) tier in a `Pipeline` cascade.
const PLANNER_FRAMING: &str = "You are the PLANNER in a multi-stage pipeline. Do NOT call tools, write \
files, or run anything. Think the task through and output a concise, concrete plan for the next model to \
execute: the approach, the key steps in order, and any critical code or structure it must get right. Be \
specific and brief — this plan is the only thing the executor receives from you.";
/// How an advisor's plan is framed when forwarded into the next tier's input.
const PLAN_PREFIX: &str = "[Plan from the advisor model — follow it to complete the task]\n";

/// Forward an advisor's plan into the conversation the next tier sees (the forward-output handoff).
/// Appends to the trailing user-text message when there is one (keeps role alternation intact for
/// strict chat templates); otherwise adds a new user message.
fn forward_plan(messages: &mut Vec<Message>, plan: &str) {
    let note = format!("{PLAN_PREFIX}{plan}");
    if let Some(last) = messages.last_mut() {
        if last.role == Role::User {
            if let Some(ContentBlock::Text { text }) = last.content.last_mut() {
                text.push_str("\n\n");
                text.push_str(&note);
                return;
            }
        }
    }
    messages.push(Message::user(note));
}

/// Where a model runs — local (its own resource limits, e.g. memory) vs remote (the network +
/// the provider's quota/rate limits). A `Network` failure parks *all* remote models at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Location {
    #[default]
    Local,
    Remote,
}

/// A candidate model + its position in the cost order (lowest `tier` = cheapest/fastest).
pub struct ModelCard {
    pub id: String,
    pub backend: Arc<dyn ChatBackend>,
    pub tier: u32,
    pub location: Location,
    /// The residency lane this model competes in (Phase 6). Co-residents share a [`Lane::Pool`]
    /// and are serialized; remotes are [`Lane::Free`]. See [`Lane::default_for`].
    pub lane: Lane,
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
    /// An optional **small-model router** as the (async, model-backed) difficulty source for
    /// `ClassifyThenStart`/`Learned`: a 4B classifies the query into a difficulty-ordered label
    /// set, overriding the sync heuristic. `None` (default) → the sync `classifier`/heuristic.
    /// Skipped under `AlwaysCheapest` (difficulty is ignored there, so no model call is made).
    /// See `docs/specs/small-model-router.md`.
    pub router: Option<Arc<crate::router::ModelRouter>>,
    /// Per-pool concurrent residency slots (Phase 6). Unlisted pools default to `1`
    /// (single-resident); raise a pool's count for co-resident models (multi-resident).
    pub residency_slots: HashMap<String, usize>,
    /// The learned stats store (Phase 7). `Some` → every attempt is recorded and the `Learned`
    /// strategy can read it; `None` → no learning/recording.
    pub stats: Option<Arc<StatsStore>>,
    /// `Learned` start-tier thresholds: the cheapest tier whose accept-rate ≥ this, with ≥
    /// `learned_min_attempts` of evidence, becomes the entry point. Also the "trust" bar for the
    /// adaptive judge threshold.
    pub learned_accept_threshold: f64,
    pub learned_min_attempts: u64,
    /// Adaptive judge: how much to *lower* the L2 judge threshold for a `(task-class, model)` the
    /// stats have shown to be trustworthy (accept-rate ≥ `learned_accept_threshold`). `0.0` = off.
    pub judge_trust_discount: f32,
    /// Persist health transitions (cooldowns) to this JSONL and replay on start, so a parked model
    /// stays parked across restarts. `None` = in-memory health only.
    pub health_path: Option<PathBuf>,
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
            router: None,
            residency_slots: HashMap::new(),
            stats: None,
            learned_accept_threshold: 0.6,
            learned_min_attempts: 5,
            judge_trust_discount: 0.1,
            health_path: None,
        }
    }
}

/// A [`ChatBackend`] that cascades over `config.models` cheapest-first, skipping models in
/// transient health cooldown and recording outcomes back into [`HealthRegistry`].
pub struct CascadeBackend {
    config: CascadeConfig,
    health: HealthRegistry,
    /// Residency lanes, shared across all concurrent requests so co-residents serialize while
    /// different lanes (and remotes) run in parallel (Phase 6).
    lanes: LaneSet,
}

impl CascadeBackend {
    pub fn new(mut config: CascadeConfig) -> Self {
        config.models.sort_by_key(|m| m.tier);
        let lanes =
            LaneSet::new(config.models.iter().map(|m| &m.lane), &config.residency_slots);
        let health = match &config.health_path {
            Some(p) => HealthRegistry::open(p),
            None => HealthRegistry::new(),
        };
        Self { config, health, lanes }
    }

    /// The shared health registry (observability / tests).
    pub fn health(&self) -> &HealthRegistry {
        &self.health
    }

    /// The difficulty score for `req` — the configured classifier, else the built-in heuristic.
    fn difficulty(&self, req: &ChatRequest) -> f32 {
        match &self.config.classifier {
            Some(c) => c.difficulty(req),
            None => HeuristicClassifier.difficulty(req),
        }
    }

    /// Map a difficulty score onto a proportional start tier in `0..n` (round to nearest).
    fn classify_start(difficulty: f32, n: usize) -> usize {
        if n <= 1 {
            return 0;
        }
        ((difficulty.clamp(0.0, 1.0) * (n - 1) as f32).round() as usize).min(n - 1)
    }

    /// The cost-ordered index of the *entry* model for this request, per the routing strategy.
    /// `ClassifyThenStart` maps difficulty onto the tiers; `Learned` prefers the cheapest tier the
    /// stats say has been good enough for this task-class, falling back to the classifier when the
    /// evidence is thin. Never exceeds the top tier.
    fn start_index(&self, req: &ChatRequest, difficulty: f32, n: usize) -> usize {
        match self.config.strategy {
            // `Pipeline` runs every tier from 0 and never consults `start_index`; 0 is a safe default.
            RoutingStrategy::AlwaysCheapest | RoutingStrategy::Pipeline => 0,
            RoutingStrategy::ClassifyThenStart => Self::classify_start(difficulty, n),
            RoutingStrategy::Learned => {
                if let Some(stats) = &self.config.stats {
                    let task = TaskClass::of(req, difficulty);
                    let ids: Vec<&str> = self.config.models.iter().map(|m| m.id.as_str()).collect();
                    if let Some(i) = stats.learned_start_tier(
                        task,
                        &ids,
                        self.config.learned_accept_threshold,
                        self.config.learned_min_attempts,
                    ) {
                        return i;
                    }
                }
                Self::classify_start(difficulty, n)
            }
        }
    }

    /// The L2 judge threshold to apply for this `(task, model)` — the configured base, lowered by
    /// `judge_trust_discount` when the stats show the model has earned trust on this task-class
    /// (fewer wasted escalations second-guessing a proven model). Falls back to the base with no
    /// stats / no track record.
    fn effective_judge_threshold(&self, base: f32, task: TaskClass, model: &str) -> f32 {
        if self.config.judge_trust_discount > 0.0 {
            if let Some(stats) = &self.config.stats {
                if stats.is_trusted(
                    task,
                    model,
                    self.config.learned_accept_threshold,
                    self.config.learned_min_attempts,
                ) {
                    return (base - self.config.judge_trust_discount).max(0.0);
                }
            }
        }
        base
    }

    /// Record one attempt into the learned stats store (no-op when stats are off).
    #[allow(clippy::too_many_arguments)]
    fn record_attempt(
        &self,
        task: TaskClass,
        id: &str,
        tier: u32,
        accepted: bool,
        latency_ms: u64,
        input_tokens: u32,
        output_tokens: u32,
        judge_score: Option<f32>,
        fail_reason: Option<FailReason>,
    ) {
        if let Some(stats) = &self.config.stats {
            stats.record(AttemptRecord {
                ts: crate::share::now_unix(),
                task,
                model: id.to_string(),
                tier,
                accepted,
                escalated: !accepted,
                latency_ms,
                input_tokens,
                output_tokens,
                judge_score,
                fail_reason,
                concurrency: 0,
                resource: ResourceSnapshot::default(),
            });
        }
    }

    /// The **pipeline** pass (`RoutingStrategy::Pipeline`): every request flows through all tiers in
    /// cost order. Each non-final tier is an *advisor* — it gets the conversation-so-far plus a
    /// planning framing, **no tools**, and its text is forwarded as guidance into the next tier. The
    /// final tier is the *executor* — it gets the real tools and its answer goes back to the caller.
    /// One prompt → all tiers → back to tier 0 for the next prompt (the operator's round-robin). See
    /// `docs/specs/pipeline-cascade.md`. Eager: tiers are already-resident `Arc`s here; lazy-swap
    /// residency layers on in `adaptive-cascade-residency`.
    async fn run_pipeline(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        let models = &self.config.models;
        let last = models.len() - 1; // ≥1: single-model is handled by the caller
        // The conversation we forward, growing by one advisor note per planner stage.
        let mut messages = req.messages.clone();

        // Advisor tiers (0..last): plan only, then forward the plan to the next tier.
        for card in &models[..last] {
            // An advisor in health cooldown is skipped — degrade to the executor without its plan.
            if !self.health.is_available(&card.id) {
                continue;
            }
            let mut preq = req.clone();
            preq.tools = Vec::new(); // an advisor never calls tools — it plans
            preq.sampling.temperature = Some(PLANNER_TEMPERATURE);
            preq.sampling.response_schema = None; // never constrain a planner to a tool schema
            preq.sampling.max_tokens = Some(PLANNER_MAX_TOKENS);
            let mut pmsgs = messages.clone();
            pmsgs.push(Message::user(PLANNER_FRAMING));
            preq.messages = pmsgs;

            let outcome = {
                let _lane = self.lanes.enter(&card.lane).await;
                run_attempt(&card.backend, preq).await
            };
            if let Some(e) = &outcome.error {
                self.health.record_failure(&card.id, classify(e));
                tracing::debug!(model = %card.id, "pipeline: advisor failed, continuing without its plan");
                continue;
            }
            self.health.record_success(&card.id);
            let plan = outcome.text.trim();
            rozum_core::obs::log_event(serde_json::json!({
                "event": "pipeline_stage", "stage": "advisor",
                "model": card.id, "plan_chars": plan.len(),
            }));
            if !plan.is_empty() {
                forward_plan(&mut messages, plan);
            }
        }

        // Executor tier (last): the real request + tools; buffered back to the caller (consistent
        // with the escalation path; live-stream passthrough is a later optimization).
        let exec = &models[last];
        rozum_core::obs::log_event(serde_json::json!({
            "event": "pipeline_stage", "stage": "executor", "model": exec.id, "tiers": models.len(),
        }));
        let mut ereq = req;
        ereq.messages = messages;
        let outcome = {
            let _lane = self.lanes.enter(&exec.lane).await;
            run_attempt(&exec.backend, ereq).await
        };
        match outcome.error {
            Some(e) => Err(ModelError::BackendUnavailable(e)),
            None => Ok(buffered(outcome_events(&outcome, None))),
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
        // Pipeline (not escalation): every request flows through all tiers, planner→…→executor.
        if matches!(self.config.strategy, RoutingStrategy::Pipeline) {
            return self.run_pipeline(req).await;
        }

        let cap = self.config.budget.max_escalations.saturating_add(1);
        let marker = self.config.affordance.as_ref().map(|a| a.marker.as_str());
        let start = Instant::now();
        let mut best_ok: Option<TurnOutcome> = None;
        let mut last_err: Option<String> = None;
        let mut attempts = 0usize;
        let last_idx = models.len() - 1;

        // Score difficulty once: it drives both the entry tier and the stats task-class.
        // A configured small-model router is the difficulty source for the difficulty-using
        // strategies (one cheap 4B classification picks the entry tier); `AlwaysCheapest`
        // ignores difficulty, so skip the model call there and use the free heuristic.
        let difficulty = match &self.config.router {
            Some(r) if !matches!(self.config.strategy, RoutingStrategy::AlwaysCheapest) => {
                r.difficulty(&judge::first_user_text(&req)).await
            }
            _ => self.difficulty(&req),
        };
        let task = TaskClass::of(&req, difficulty);
        // Choose the entry tier. `AlwaysCheapest` starts at 0; `ClassifyThenStart`/`Learned` start
        // higher for harder/known-hard classes. The candidate order is then start-and-up (the
        // natural escalation path), then the cheaper tiers below start as availability fallbacks —
        // so a parked entry tier still degrades to *something*.
        let start_idx = self.start_index(&req, difficulty, models.len());
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
            // Enter the model's residency lane: co-residents serialize here, but a request in a
            // different lane (or a remote) is never blocked. Held only for this attempt; freed on
            // escalation so the next tier (or a waiting concurrent request) can take the slot.
            let t0 = Instant::now();
            let _lane = self.lanes.enter(&card.lane).await;
            let outcome = run_attempt(&card.backend, attempt_req).await;
            let latency_ms = t0.elapsed().as_millis() as u64;
            if let Some(e) = &outcome.error {
                let reason = classify(e);
                self.health.record_failure(&card.id, reason);
                self.record_attempt(task, &card.id, card.tier, false, latency_ms, 0, 0, None, Some(reason));
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
            let mut judge_score: Option<f32> = None;
            let verdict = match pipeline_verdict(&self.config.acceptance, &req, &outcome) {
                Some(v) => v,
                None => match &self.config.judge {
                    Some(jc) => {
                        let s = jc.judge.score(&req, &outcome).await;
                        judge_score = Some(s);
                        let thr = self.effective_judge_threshold(jc.threshold, task, &card.id);
                        if s >= thr { Verdict::Accept } else { Verdict::Escalate }
                    }
                    None => Verdict::Accept,
                },
            };
            self.record_attempt(
                task,
                &card.id,
                card.tier,
                verdict == Verdict::Accept,
                latency_ms,
                outcome.input_tokens,
                outcome.output_tokens,
                judge_score,
                None,
            );
            // Feed the grounded quality verdict to the model's adaptive controller (no-op unless the
            // backend self-tunes): a rejection while running concurrently backs its admission
            // ceiling off — "quality drops under load" closed into the live concurrency loop.
            card.backend.report_quality(verdict == Verdict::Accept);
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
            lane: Lane::default_for(location),
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

    // ── small-model router as the cascade's difficulty source (small-model-router P2) ──

    fn diff_labels() -> Vec<crate::router::Label> {
        use crate::router::Label;
        vec![
            Label::new("trivial", "a greeting or a one-word answer"),
            Label::new("moderate", "a normal question"),
            Label::new("hard", "long multi-step reasoning or code"),
        ]
    }

    fn router_over(reply: &'static str) -> Arc<crate::router::ModelRouter> {
        let be: Arc<dyn ChatBackend> =
            Arc::new(Mock { script: Script::Text(reply), calls: Arc::new(AtomicUsize::new(0)) });
        Arc::new(crate::router::ModelRouter::new(be, diff_labels()).unwrap())
    }

    #[tokio::test]
    async fn router_routes_hard_prompt_to_top_tier() {
        // The small-model router classifies "hard" → difficulty 1.0 → enter at the top tier;
        // the cheap/mid tiers are never touched. Note the *prompt itself* is trivial-looking —
        // the entry tier comes from the router's verdict, not the heuristic.
        let (c, cc) = card("cheap", 0, Script::Text("CHEAP"));
        let (m, mc) = card("mid", 1, Script::Text("MID"));
        let (s, sc) = card("strong", 2, Script::Text("STRONG"));
        let mut cfg = classify_cfg(vec![c, m, s]);
        cfg.router = Some(router_over("hard"));
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "STRONG");
        assert_eq!(cc.load(Ordering::SeqCst), 0, "router said hard → cheap tier skipped");
        assert_eq!(mc.load(Ordering::SeqCst), 0, "mid tier skipped");
        assert_eq!(sc.load(Ordering::SeqCst), 1, "entered at the strong tier");
    }

    #[tokio::test]
    async fn router_routes_trivial_prompt_to_cheapest() {
        // The router classifies "trivial" → difficulty 0.0 → start at the cheapest tier even
        // for a prompt the heuristic would score as hard (code fences + multi-step markers).
        let (c, cc) = card("cheap", 0, Script::Text("CHEAP"));
        let (s, sc) = card("strong", 1, Script::Text("STRONG"));
        let mut cfg = classify_cfg(vec![c, s]);
        cfg.router = Some(router_over("trivial"));
        let be = CascadeBackend::new(cfg);
        let hard = "```\nfn solve() {}\n```\nprove the theorem step by step and analyze";
        let out = collect(be.chat(ChatRequest::simple(hard)).await.unwrap()).await;
        assert_eq!(out, "CHEAP", "router overrode the heuristic → started cheap");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 0, "strong tier untouched");
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

    // ── Phase 6: parallel residency lanes ───────────────────────────────────

    /// A backend that blocks at a shared barrier before answering. Two of them complete only if
    /// *both* reach the barrier — i.e. only if they ran concurrently. If the cascade serialized
    /// them into one lane, the first would wait at the barrier forever (the second never starts).
    struct BarrierMock {
        barrier: Arc<tokio::sync::Barrier>,
        text: &'static str,
    }

    #[async_trait]
    impl ChatBackend for BarrierMock {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            self.barrier.wait().await;
            let evs: Vec<ModelResult<ChatEvent>> = vec![
                Ok(ChatEvent::TextDelta { text: self.text.into() }),
                Ok(ChatEvent::Done {
                    input_tokens: 1,
                    output_tokens: 1,
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
        fn context_window(&self) -> u32 {
            u32::MAX
        }
    }

    fn barrier_card(
        id: &str,
        tier: u32,
        pool: &str,
        barrier: Arc<tokio::sync::Barrier>,
        text: &'static str,
    ) -> ModelCard {
        ModelCard {
            id: id.into(),
            tier,
            location: Location::Local,
            lane: Lane::Pool(pool.into()),
            backend: Arc::new(BarrierMock { barrier, text }),
        }
    }

    #[tokio::test]
    async fn different_lanes_run_concurrently() {
        // A simple request (→ cheap, lane "small") and a hard request (→ strong, lane "big") are
        // dispatched at once. Distinct lanes ⇒ both acquire their permits and meet at the barrier;
        // a single global lane would deadlock one of them. ClassifyThenStart does the routing.
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let cheap = barrier_card("cheap", 0, "small", barrier.clone(), "easy");
        let strong = barrier_card("strong", 1, "big", barrier.clone(), "hard");
        let mut cfg = CascadeConfig::new(vec![cheap, strong]);
        cfg.strategy = RoutingStrategy::ClassifyThenStart;
        let be = Arc::new(CascadeBackend::new(cfg));

        let be1 = be.clone();
        let simple = tokio::spawn(async move {
            collect(be1.chat(ChatRequest::simple("hi")).await.unwrap()).await
        });
        let be2 = be.clone();
        let hard = tokio::spawn(async move {
            let prompt = "```\nfn solve() {}\n```\nprove the theorem step by step and analyze";
            collect(be2.chat(ChatRequest::simple(prompt)).await.unwrap()).await
        });

        let joined = tokio::time::timeout(Duration::from_secs(2), async {
            (simple.await.unwrap(), hard.await.unwrap())
        })
        .await;
        let (s, h) = joined.expect("distinct lanes must run concurrently (else the barrier hangs)");
        assert_eq!(s, "easy", "the simple request was served by the cheap/small lane");
        assert_eq!(h, "hard", "the hard request was served by the strong/big lane, in parallel");
    }

    // ── Phase 7: learned stats → Learned start-tier ─────────────────────────

    fn seed_stat(
        stats: &StatsStore,
        task: TaskClass,
        model: &str,
        tier: u32,
        accepted: bool,
        n: usize,
    ) {
        for _ in 0..n {
            stats.record(AttemptRecord {
                ts: 0,
                task,
                model: model.into(),
                tier,
                accepted,
                escalated: !accepted,
                latency_ms: 1,
                input_tokens: 0,
                output_tokens: 0,
                judge_score: None,
                fail_reason: None,
                concurrency: 0,
                resource: ResourceSnapshot::default(),
            });
        }
    }

    #[tokio::test]
    async fn learned_skips_cheap_when_stats_say_it_escalates() {
        // History for this task-class: the cheap tier almost always escalated, the strong tier was
        // good. `Learned` therefore enters at the strong tier and the cheap one is never tried.
        let stats = Arc::new(StatsStore::in_memory());
        let task = TaskClass { shape: TaskShape::Freeform, difficulty: DiffBucket::Easy };
        seed_stat(&stats, task, "cheap", 0, false, 10); // accept-rate 0
        seed_stat(&stats, task, "strong", 1, true, 10); // accept-rate 1

        let (c, cc) = card("cheap", 0, Script::Text("weak"));
        let (s, sc) = card("strong", 1, Script::Text("strong answer"));
        let mut cfg = CascadeConfig::new(vec![c, s]);
        cfg.strategy = RoutingStrategy::Learned;
        cfg.stats = Some(stats);
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "strong answer");
        assert_eq!(cc.load(Ordering::SeqCst), 0, "learned to skip the cheap tier on this class");
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn learned_falls_back_to_cheapest_without_evidence() {
        // No history → `Learned` defers to the classifier; a trivial prompt starts at tier 0.
        let stats = Arc::new(StatsStore::in_memory());
        let (c, cc) = card("cheap", 0, Script::Text("hello"));
        let (s, sc) = card("strong", 1, Script::Text("strong"));
        let mut cfg = CascadeConfig::new(vec![c, s]);
        cfg.strategy = RoutingStrategy::Learned;
        cfg.stats = Some(stats);
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "hello");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cascade_records_attempts_into_stats() {
        // The cheap model fails L0 (non-JSON) → escalate; the strong model conforms → accept. Both
        // attempts are recorded under the request's task-class.
        let stats = Arc::new(StatsStore::in_memory());
        let (c, _cc) = card("cheap", 0, Script::Text("not json"));
        let (s, _sc) = card("strong", 1, Script::Text("{\"x\": 1}"));
        let mut cfg = CascadeConfig::new(vec![c, s]);
        cfg.stats = Some(stats.clone());
        let be = CascadeBackend::new(cfg);
        let req = schema_req();
        let task = TaskClass::of(&req, HeuristicClassifier.difficulty(&req));
        let _ = collect(be.chat(req).await.unwrap()).await;

        let cheap = stats.stat(task, "cheap").expect("cheap attempt recorded");
        assert_eq!((cheap.attempts, cheap.accepts, cheap.escalations), (1, 0, 1));
        let strong = stats.stat(task, "strong").expect("strong attempt recorded");
        assert_eq!((strong.attempts, strong.accepts, strong.escalations), (1, 1, 0));
    }

    // ── Phase 7 follow-up: adaptive judge threshold ─────────────────────────

    struct FixedJudge(f32);
    #[async_trait]
    impl Judge for FixedJudge {
        async fn score(&self, _req: &ChatRequest, _ans: &TurnOutcome) -> f32 {
            self.0
        }
    }

    #[tokio::test]
    async fn adaptive_judge_trusts_a_proven_model() {
        // cheap is proven on this task-class (accept-rate 1.0). A judge score of 0.45 is below the
        // base threshold 0.5 but above the trusted threshold (0.5 − 0.1) → cheap is accepted.
        let stats = Arc::new(StatsStore::in_memory());
        let task = TaskClass { shape: TaskShape::Freeform, difficulty: DiffBucket::Easy };
        seed_stat(&stats, task, "cheap", 0, true, 10);

        let (c, cc) = card("cheap", 0, Script::Text("an ok answer"));
        let (s, sc) = card("strong", 1, Script::Text("better"));
        let mut cfg = CascadeConfig::new(vec![c, s]);
        cfg.judge = Some(JudgeConfig { judge: Box::new(FixedJudge(0.45)), threshold: 0.5 });
        cfg.stats = Some(stats);
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "an ok answer", "a trusted model is accepted at the lowered threshold");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn adaptive_judge_holds_the_base_for_an_unproven_model() {
        // Same judge score, but no track record → the base threshold 0.5 applies → 0.45 escalates.
        let stats = Arc::new(StatsStore::in_memory());
        let (c, cc) = card("cheap", 0, Script::Text("an ok answer"));
        let (s, sc) = card("strong", 1, Script::Text("better"));
        let mut cfg = CascadeConfig::new(vec![c, s]);
        cfg.judge = Some(JudgeConfig { judge: Box::new(FixedJudge(0.45)), threshold: 0.5 });
        cfg.stats = Some(stats);
        let be = CascadeBackend::new(cfg);
        let out = collect(be.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        assert_eq!(out, "better", "an unproven model is held to the base threshold and escalates");
        assert_eq!(cc.load(Ordering::SeqCst), 1);
        assert_eq!(sc.load(Ordering::SeqCst), 1);
    }

    // ── Pipeline strategy (planner → executor, every tier every request) ─────────────────────────

    /// A backend that records the text + tool-count of the request it received, then replies
    /// with a fixed string — so a test can assert what each pipeline tier actually saw.
    struct CapMock {
        reply: &'static str,
        seen_text: Arc<std::sync::Mutex<String>>,
        seen_tools: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ChatBackend for CapMock {
        async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.seen_tools.store(req.tools.len(), Ordering::SeqCst);
            let mut buf = String::new();
            for m in &req.messages {
                for c in &m.content {
                    if let ContentBlock::Text { text } = c {
                        buf.push_str(text);
                        buf.push('\n');
                    }
                }
            }
            *self.seen_text.lock().unwrap() = buf;
            let evs: Vec<ModelResult<ChatEvent>> = vec![
                Ok(ChatEvent::TextDelta { text: self.reply.into() }),
                Ok(ChatEvent::Done {
                    input_tokens: 1,
                    output_tokens: 1,
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
        fn context_window(&self) -> u32 {
            u32::MAX
        }
    }

    fn cap_card(
        id: &str,
        tier: u32,
        reply: &'static str,
    ) -> (ModelCard, Arc<std::sync::Mutex<String>>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let seen_text = Arc::new(std::sync::Mutex::new(String::new()));
        let seen_tools = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let mc = ModelCard {
            id: id.to_string(),
            tier,
            location: Location::Local,
            lane: Lane::default_for(Location::Local),
            backend: Arc::new(CapMock {
                reply,
                seen_text: seen_text.clone(),
                seen_tools: seen_tools.clone(),
                calls: calls.clone(),
            }),
        };
        (mc, seen_text, seen_tools, calls)
    }

    #[tokio::test]
    async fn pipeline_runs_all_tiers_and_forwards_plan() {
        let (planner, p_seen, p_tools, p_calls) = cap_card("planner", 0, "STEP-A then STEP-B");
        let (exec, e_seen, e_tools, e_calls) = cap_card("exec", 1, "FINAL");
        let mut cfg = CascadeConfig::new(vec![planner, exec]);
        cfg.strategy = RoutingStrategy::Pipeline;
        let be = CascadeBackend::new(cfg);

        let mut req = ChatRequest::simple("solve the task");
        req.tools = vec![crate::backend::ToolDef {
            name: "write_file".into(),
            description: "write a file".into(),
            input_schema: json!({"type": "object"}),
        }];

        let out = collect(be.chat(req).await.unwrap()).await;

        assert_eq!(out, "FINAL", "the executor's answer is what the caller receives");
        assert_eq!(p_calls.load(Ordering::SeqCst), 1, "the planner ran exactly once");
        assert_eq!(e_calls.load(Ordering::SeqCst), 1, "the executor ran exactly once");
        // Forward-output handoff: the executor saw the planner's plan.
        assert!(
            e_seen.lock().unwrap().contains("STEP-A then STEP-B"),
            "the executor received the planner's plan"
        );
        // The advisor plans without tools; only the executor gets the real tools.
        assert_eq!(p_tools.load(Ordering::SeqCst), 0, "the advisor is given no tools");
        assert_eq!(e_tools.load(Ordering::SeqCst), 1, "the executor is given the real tools");
        // The advisor saw the planning framing; the original task is still present for the executor.
        assert!(p_seen.lock().unwrap().contains("PLANNER"), "the advisor got the planning framing");
        assert!(e_seen.lock().unwrap().contains("solve the task"), "the original task reaches the executor");
    }

    #[tokio::test]
    async fn pipeline_degrades_when_advisor_fails() {
        // The advisor errors; the executor must still run (without a plan) and answer.
        let (bad_planner, _bc) = card("planner", 0, Script::Err("advisor down"));
        let (exec, e_seen, _e_tools, e_calls) = cap_card("exec", 1, "FINAL");
        let mut cfg = CascadeConfig::new(vec![bad_planner, exec]);
        cfg.strategy = RoutingStrategy::Pipeline;
        let be = CascadeBackend::new(cfg);

        let out = collect(be.chat(ChatRequest::simple("do it")).await.unwrap()).await;
        assert_eq!(out, "FINAL", "a failed advisor degrades to the executor, it still answers");
        assert_eq!(e_calls.load(Ordering::SeqCst), 1, "the executor ran");
        assert!(
            !e_seen.lock().unwrap().contains("[Plan from the advisor"),
            "no plan note is forwarded when the advisor failed"
        );
    }
}
