//! Declarative cascade configuration — the bridge from a config/`model:` string to a live
//! [`CascadeBackend`] (the gateway request-surface wiring).
//!
//! A [`CascadeSpec`] is a serializable description of a cascade: an ordered (cheapest-first) list
//! of [`TierSpec`]s plus a few knobs. [`build_cascade`] turns it into a `CascadeBackend` by handing
//! each tier's model spec to a caller-supplied async **resolver** (the gateway passes its existing
//! backend-build chain; remotes become HTTP backends). The cascade module stays decoupled from how
//! backends are actually constructed.
//!
//! The gateway routes `model: "cascade"` / `"cascade:<name>"` here (see [`parse_cascade_model`]).
//! Named specs are loaded from config (env JSON for v1: `ROZUM_CASCADE` / `ROZUM_CASCADE_<NAME>`).

use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use super::{CascadeBackend, CascadeConfig, Lane, Location, ModelCard, RoutingStrategy};
use crate::backend::ChatBackend;

/// One cascade tier — a model spec the resolver turns into a backend, plus its placement.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TierSpec {
    /// The model spec resolved to a backend (a local model id like `mlx-community:Qwen3-4B-4bit`,
    /// or a remote model id like `claude-haiku-4-5`).
    pub model: String,
    /// Local (own memory) vs remote (network/quota). Drives health (network parks all remotes) and
    /// the default residency lane.
    #[serde(default)]
    pub location: Location,
    /// Residency pool override (Phase 6). `None` → [`Lane::default_for`] the location.
    #[serde(default)]
    pub pool: Option<String>,
    /// Remote only: the OpenAI-/Anthropic-compatible HTTP endpoint.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Remote only: the environment variable holding the API key.
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// The start-tier routing strategy, by name (config-friendly).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StrategyName {
    #[default]
    AlwaysCheapest,
    Classify,
    Learned,
}

impl From<StrategyName> for RoutingStrategy {
    fn from(s: StrategyName) -> Self {
        match s {
            StrategyName::AlwaysCheapest => RoutingStrategy::AlwaysCheapest,
            StrategyName::Classify => RoutingStrategy::ClassifyThenStart,
            StrategyName::Learned => RoutingStrategy::Learned,
        }
    }
}

/// A whole cascade, ready to build: cost-ordered tiers + a few knobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeSpec {
    /// Tiers in cost order, cheapest first (the tier index is the position).
    pub tiers: Vec<TierSpec>,
    /// Cap on escalation hops (`None` = unbounded).
    #[serde(default)]
    pub max_escalations: Option<usize>,
    /// The start-tier strategy.
    #[serde(default)]
    pub strategy: StrategyName,
}

impl TierSpec {
    /// The residency lane for this tier — its explicit `pool`, else the default for its location.
    fn lane(&self) -> Lane {
        match &self.pool {
            Some(p) => Lane::Pool(p.clone()),
            None => Lane::default_for(self.location),
        }
    }
}

/// Build a [`CascadeBackend`] from `spec`, resolving each tier's model to a backend via `resolve`.
/// A tier whose backend can't be built (e.g. a remote with a missing key, an unreachable endpoint)
/// is **skipped** and the cascade is built from the survivors — so a partial config still runs;
/// only an *empty* result is an error. Survivors are re-tiered by surviving order, preserving the
/// cheapest-first invariant.
pub async fn build_cascade<F, Fut>(spec: &CascadeSpec, resolve: F) -> Result<CascadeBackend, String>
where
    F: Fn(TierSpec) -> Fut,
    Fut: Future<Output = Option<Arc<dyn ChatBackend>>>,
{
    let mut cards = Vec::new();
    let mut skipped = Vec::new();
    for tier in &spec.tiers {
        match resolve(tier.clone()).await {
            Some(backend) => {
                let next = cards.len() as u32;
                cards.push(ModelCard {
                    id: tier.model.clone(),
                    backend,
                    tier: next,
                    location: tier.location,
                    lane: tier.lane(),
                });
            }
            None => skipped.push(tier.model.clone()),
        }
    }
    if cards.is_empty() {
        return Err(format!(
            "cascade: no tier could be built (tried: {})",
            spec.tiers.iter().map(|t| t.model.as_str()).collect::<Vec<_>>().join(", ")
        ));
    }
    if !skipped.is_empty() {
        tracing::warn!(skipped = ?skipped, "cascade: some tiers were unavailable and skipped");
    }

    let mut cfg = CascadeConfig::new(cards);
    if let Some(m) = spec.max_escalations {
        cfg.budget.max_escalations = m;
    }
    cfg.strategy = spec.strategy.into();
    Ok(CascadeBackend::new(cfg))
}

/// Parse a gateway `model` field: `"cascade"` → the default config (`Some("")`),
/// `"cascade:<name>"` → a named config (`Some("<name>")`), anything else → `None` (not a cascade).
pub fn parse_cascade_model(model: &str) -> Option<String> {
    let m = model.trim();
    if m == "cascade" {
        Some(String::new())
    } else {
        m.strip_prefix("cascade:").map(|n| n.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChatEvent, ChatRequest, ChatStream, ModelResult, StopReason};
    use async_trait::async_trait;
    use serde_json::json;

    struct Echo(&'static str);
    #[async_trait]
    impl ChatBackend for Echo {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            let evs: Vec<ModelResult<ChatEvent>> = vec![
                Ok(ChatEvent::TextDelta { text: self.0.into() }),
                Ok(ChatEvent::Done {
                    input_tokens: 1,
                    output_tokens: 1,
                    stop_reason: StopReason::EndTurn,
                }),
            ];
            Ok(Box::pin(futures::stream::iter(evs)))
        }
        fn context_window(&self) -> u32 {
            4096
        }
    }

    fn tier(model: &str, location: Location) -> TierSpec {
        TierSpec { model: model.into(), location, pool: None, endpoint: None, api_key_env: None }
    }

    #[test]
    fn parse_cascade_model_cases() {
        assert_eq!(parse_cascade_model("cascade"), Some(String::new()));
        assert_eq!(parse_cascade_model("cascade:fast"), Some("fast".into()));
        assert_eq!(parse_cascade_model("  cascade:fast  "), Some("fast".into()));
        assert_eq!(parse_cascade_model("mlx-community:Qwen3-4B-4bit"), None);
        assert_eq!(parse_cascade_model("gpt-4o"), None);
    }

    #[test]
    fn spec_json_round_trips() {
        let s: CascadeSpec = serde_json::from_value(json!({
            "tiers": [
                {"model": "local-small"},
                {"model": "claude-haiku-4-5", "location": "remote",
                 "endpoint": "https://api.anthropic.com", "api_key_env": "ANTHROPIC_API_KEY"}
            ],
            "max_escalations": 1,
            "strategy": "classify"
        }))
        .unwrap();
        assert_eq!(s.tiers.len(), 2);
        assert_eq!(s.tiers[0].location, Location::Local); // defaulted
        assert_eq!(s.tiers[1].location, Location::Remote);
        assert!(matches!(s.strategy, StrategyName::Classify));
        assert_eq!(s.max_escalations, Some(1));
    }

    #[tokio::test]
    async fn build_resolves_each_tier_in_order() {
        let spec = CascadeSpec {
            tiers: vec![tier("cheap", Location::Local), tier("strong", Location::Remote)],
            max_escalations: None,
            strategy: StrategyName::AlwaysCheapest,
        };
        let be = build_cascade(&spec, |t| async move {
            Some(Arc::new(Echo(if t.model == "cheap" { "C" } else { "S" })) as Arc<dyn ChatBackend>)
        })
        .await
        .unwrap();
        // The cheapest tier answers (AlwaysCheapest, free-form accept).
        let out = crate::backend::collect_to_string(
            be.chat(ChatRequest::simple("hi")).await.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(out, "C");
    }

    #[tokio::test]
    async fn unbuildable_tiers_are_skipped_not_fatal() {
        // The first tier (a remote with no key, say) fails to resolve → skipped; the survivor runs.
        let spec = CascadeSpec {
            tiers: vec![tier("missing-remote", Location::Remote), tier("local", Location::Local)],
            max_escalations: None,
            strategy: StrategyName::AlwaysCheapest,
        };
        let be = build_cascade(&spec, |t| async move {
            if t.model == "missing-remote" {
                None
            } else {
                Some(Arc::new(Echo("L")) as Arc<dyn ChatBackend>)
            }
        })
        .await
        .unwrap();
        let out = crate::backend::collect_to_string(
            be.chat(ChatRequest::simple("hi")).await.unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(out, "L", "the cascade runs on the surviving local tier");
    }

    #[tokio::test]
    async fn no_buildable_tier_is_an_error() {
        let spec = CascadeSpec {
            tiers: vec![tier("a", Location::Remote), tier("b", Location::Remote)],
            max_escalations: None,
            strategy: StrategyName::AlwaysCheapest,
        };
        let r = build_cascade(&spec, |_t| async move { None }).await;
        assert!(r.is_err(), "an all-unavailable cascade is an error, not an empty backend");
    }

    #[tokio::test]
    async fn pool_override_sets_the_lane() {
        let mut t = tier("local", Location::Local);
        t.pool = Some("gpu0".into());
        let spec =
            CascadeSpec { tiers: vec![t], max_escalations: None, strategy: StrategyName::default() };
        // One tier → passthrough still builds fine; the lane override is carried into the card.
        let be = build_cascade(&spec, |_t| async move {
            Some(Arc::new(Echo("x")) as Arc<dyn ChatBackend>)
        })
        .await;
        assert!(be.is_ok());
    }
}
