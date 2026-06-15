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

pub use acceptance::{AcceptanceCheck, StructuralCheck, TurnOutcome, Verdict};

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ModelError, ModelResult, StopReason,
};
use acceptance::pipeline_verdict;

/// A candidate model + its position in the cost order (lowest `tier` = cheapest/fastest).
pub struct ModelCard {
    pub id: String,
    pub backend: Arc<dyn ChatBackend>,
    pub tier: u32,
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
}

impl CascadeConfig {
    /// The default Phase-1 config over `models`: L0 (structural) acceptance, unbounded budget.
    pub fn new(models: Vec<ModelCard>) -> Self {
        Self {
            models,
            acceptance: vec![Box::new(StructuralCheck)],
            budget: CascadeBudget::default(),
        }
    }
}

/// A [`ChatBackend`] that cascades over `config.models` cheapest-first.
pub struct CascadeBackend {
    config: CascadeConfig,
}

impl CascadeBackend {
    pub fn new(mut config: CascadeConfig) -> Self {
        config.models.sort_by_key(|m| m.tier);
        Self { config }
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
/// cascade has to see the whole answer to judge it, so the winner is emitted post-hoc).
fn outcome_events(o: &TurnOutcome) -> Vec<ChatEvent> {
    let mut evs = Vec::new();
    if !o.text.is_empty() {
        evs.push(ChatEvent::TextDelta { text: o.text.clone() });
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

        let max_attempts = self
            .config
            .budget
            .max_escalations
            .saturating_add(1)
            .min(models.len());
        let start = Instant::now();
        let mut best_ok: Option<TurnOutcome> = None;
        let mut last_err: Option<String> = None;

        for card in models.iter().take(max_attempts) {
            if self.config.budget.wall_time.is_some_and(|wt| start.elapsed() >= wt) {
                break;
            }
            let outcome = run_attempt(&card.backend, req.clone()).await;
            if let Some(e) = &outcome.error {
                // Phase 1: an errored attempt is skipped; Phase 2 makes this health-aware.
                last_err = Some(e.clone());
                continue;
            }
            match pipeline_verdict(&self.config.acceptance, &req, &outcome) {
                Verdict::Accept => {
                    tracing::debug!(model = %card.id, "cascade: accepted");
                    return Ok(buffered(outcome_events(&outcome)));
                }
                _ => {
                    tracing::debug!(model = %card.id, "cascade: escalating");
                    best_ok = Some(outcome); // keep the most-capable usable answer so far
                }
            }
        }

        // Exhausted budget/list without an `Accept`: return the best usable answer, or error
        // only if no candidate produced one.
        match best_ok {
            Some(o) => Ok(buffered(outcome_events(&o))),
            None => Err(ModelError::BackendUnavailable(
                last_err.unwrap_or_else(|| "cascade: no model produced a usable answer".into()),
            )),
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
        let calls = Arc::new(AtomicUsize::new(0));
        let mc = ModelCard {
            id: id.to_string(),
            tier,
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
}
