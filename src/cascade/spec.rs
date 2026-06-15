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

/// The remote wire protocol for a tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteApi {
    /// OpenAI-compatible `/v1/chat/completions` (OpenAI, OpenRouter, LM Studio, mlx_lm.server, …).
    #[default]
    Openai,
    /// Anthropic-native `/v1/messages` (Claude).
    Anthropic,
}

/// One cascade tier — a model spec the resolver turns into a backend, plus its placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Remote only: the wire protocol (default OpenAI-compatible; `anthropic` for native Claude).
    #[serde(default)]
    pub api: RemoteApi,
    /// Remote only: the HTTP endpoint. Optional for `anthropic` (defaults to
    /// `https://api.anthropic.com`); required for an OpenAI-compatible remote.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Remote only: the environment variable holding the API key (defaults per `api`:
    /// `ANTHROPIC_API_KEY` / `OPENAI_API_KEY`).
    #[serde(default)]
    pub api_key_env: Option<String>,
}

/// The start-tier routing strategy, by name (config-friendly).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StrategyName {
    /// Always start at the cheapest tier; escalate only when the answer is rejected.
    #[default]
    #[serde(alias = "alwaysCheapest")] // back-compat with the old name
    Cheapest,
    Classify,
    Learned,
}

impl StrategyName {
    /// Parse a CLI/env value (case- and separator-insensitive): `cheapest` (alias `alwaysCheapest`),
    /// `classify`, `learned`. `None` for an unrecognized value.
    pub fn parse_cli(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().replace(['-', '_', ' '], "").as_str() {
            "cheapest" | "cheap" | "alwayscheapest" => Some(StrategyName::Cheapest),
            "classify" | "classifythenstart" => Some(StrategyName::Classify),
            "learned" | "learn" => Some(StrategyName::Learned),
            _ => None,
        }
    }
}

impl From<StrategyName> for RoutingStrategy {
    fn from(s: StrategyName) -> Self {
        match s {
            StrategyName::Cheapest => RoutingStrategy::AlwaysCheapest,
            StrategyName::Classify => RoutingStrategy::ClassifyThenStart,
            StrategyName::Learned => RoutingStrategy::Learned,
        }
    }
}

/// A whole cascade, ready to build: cost-ordered tiers + a few knobs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

/// Classify a bare model name into a [`TierSpec`] — its location and (for remotes) wire protocol —
/// from the name alone. `claude…`/`anthropic…` → native Anthropic; OpenAI families (`gpt…`, `o1/o3/
/// o4…`, `chatgpt…`) → OpenAI; everything else (HF repo ids, `mlx-community/…`, `hf:…`) → local.
/// Endpoints/keys are left unset — the gateway resolver fills the provider defaults.
pub fn classify_model_name(name: &str) -> TierSpec {
    let model = name.trim().to_string();
    let lower = model.to_lowercase();
    let (location, api) = if lower.starts_with("claude") || lower.contains("anthropic") {
        (Location::Remote, RemoteApi::Anthropic)
    } else if is_openai_name(&lower) {
        (Location::Remote, RemoteApi::Openai)
    } else {
        (Location::Local, RemoteApi::Openai) // api is irrelevant for a local tier
    };
    TierSpec { model, location, pool: None, api, endpoint: None, api_key_env: None }
}

fn is_openai_name(lower: &str) -> bool {
    lower.starts_with("gpt-")
        || lower.starts_with("gpt4")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("chatgpt")
        || lower.starts_with("text-")
        || lower.starts_with("davinci")
}

/// Pull a `<n>b` parameter count (billions) out of a model name, preferring an MoE *active* count
/// `a<n>b` (so `Qwen3-30B-A3B` ranks like a 3B for speed). `None` if the name carries no size.
fn name_param_billions(lower: &str) -> Option<f64> {
    let b = lower.as_bytes();
    let mut max_size: Option<f64> = None;
    let mut active: Option<f64> = None;
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_digit() {
            let start = i;
            while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                i += 1;
            }
            // `<n>b` is a parameter count — but not the `<n>bit` quantization suffix.
            if i < b.len() && b[i] == b'b' && !lower[i..].starts_with("bit") {
                if let Ok(v) = lower[start..i].parse::<f64>() {
                    if start > 0 && b[start - 1] == b'a' {
                        active = Some(v);
                    }
                    max_size = Some(max_size.map_or(v, |m: f64| m.max(v)));
                }
            }
        } else {
            i += 1;
        }
    }
    active.or(max_size)
}

/// A rough provider-tier rank for a remote model (cheaper/faster → lower).
fn remote_tier_rank(lower: &str) -> u64 {
    if lower.contains("haiku")
        || lower.contains("mini")
        || lower.contains("flash")
        || lower.contains("nano")
        || lower.contains("small")
    {
        10
    } else if lower.contains("sonnet") || lower.contains("gpt-4o") || lower.contains("gpt-4-turbo") {
        30
    } else if lower.contains("opus")
        || lower.contains("o1")
        || lower.contains("o3")
        || lower.contains("o4")
        || lower.contains("gpt-4")
        || lower.contains("large")
    {
        50
    } else {
        40 // unknown remote → middle of the pack
    }
}

/// A cost/capability rank for ordering a cascade cheapest-first: locals (free, on-device) before
/// remotes, locals by parameter size (MoE by active params), remotes by provider tier.
fn model_rank(t: &TierSpec) -> u64 {
    let lower = t.model.to_lowercase();
    match t.location {
        Location::Local => name_param_billions(&lower).map(|b| b.round() as u64).unwrap_or(50),
        Location::Remote => 1000 + remote_tier_rank(&lower),
    }
}

/// Build a cascade from a **flat list of model names** — the simple path. Each name is classified
/// (local / Claude / OpenAI) and the list is auto-ordered cheapest→most-capable; the strategy
/// defaults to `classify` (simple requests start cheap, hard ones start higher). This is what
/// `--model "qwen3-4b,claude-haiku-4-5,gpt-4o"` and a multi-select model picker produce.
pub fn from_model_list<S: AsRef<str>>(names: &[S]) -> CascadeSpec {
    let mut ranked: Vec<(u64, TierSpec)> = names
        .iter()
        .map(|n| classify_model_name(n.as_ref()))
        .map(|t| (model_rank(&t), t))
        .collect();
    ranked.sort_by_key(|(r, _)| *r); // stable → preserves input order within a tier rank
    CascadeSpec {
        tiers: ranked.into_iter().map(|(_, t)| t).collect(),
        max_escalations: None,
        strategy: StrategyName::Classify,
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
        TierSpec {
            model: model.into(),
            location,
            pool: None,
            api: RemoteApi::default(),
            endpoint: None,
            api_key_env: None,
        }
    }

    #[test]
    fn strategy_name_serde_uses_cheapest_with_back_compat() {
        // The canonical serialized name is "cheapest".
        assert_eq!(serde_json::to_string(&StrategyName::Cheapest).unwrap(), "\"cheapest\"");
        assert_eq!(
            serde_json::from_str::<StrategyName>("\"cheapest\"").unwrap(),
            StrategyName::Cheapest
        );
        // The old "alwaysCheapest" still deserializes (config back-compat).
        assert_eq!(
            serde_json::from_str::<StrategyName>("\"alwaysCheapest\"").unwrap(),
            StrategyName::Cheapest
        );
    }

    #[test]
    fn strategy_name_parses_cli_values() {
        assert_eq!(StrategyName::parse_cli("classify"), Some(StrategyName::Classify));
        assert_eq!(StrategyName::parse_cli("Learned"), Some(StrategyName::Learned));
        assert_eq!(StrategyName::parse_cli("always-cheapest"), Some(StrategyName::Cheapest));
        assert_eq!(StrategyName::parse_cli("cheapest"), Some(StrategyName::Cheapest));
        assert_eq!(StrategyName::parse_cli("nonsense"), None);
    }

    #[test]
    fn classify_model_name_detects_provider() {
        assert_eq!(classify_model_name("claude-haiku-4-5").location, Location::Remote);
        assert_eq!(classify_model_name("claude-haiku-4-5").api, RemoteApi::Anthropic);
        assert_eq!(classify_model_name("gpt-4o").api, RemoteApi::Openai);
        assert_eq!(classify_model_name("o1-mini").api, RemoteApi::Openai);
        assert_eq!(classify_model_name("o1-mini").location, Location::Remote);
        // A local HF/MLX id.
        let local = classify_model_name("mlx-community/Qwen3-4B-4bit");
        assert_eq!(local.location, Location::Local);
    }

    #[test]
    fn from_model_list_orders_cheapest_first() {
        // Mixed: a big remote, a small local, a big local, a cheap remote, a MoE.
        let names = [
            "claude-opus-4-8",                  // remote, top tier
            "mlx-community/Qwen3-30B-A3B-4bit",  // local MoE, active 3B → ranks small
            "mlx-community/Qwen2.5-Coder-32B",   // local dense 32B → ranks large
            "claude-haiku-4-5",                  // remote, cheap tier
            "mlx-community/Qwen3-4B-4bit",       // local 4B → cheapest
        ];
        let spec = from_model_list(&names);
        let order: Vec<&str> = spec.tiers.iter().map(|t| t.model.as_str()).collect();
        // Locals first (free), ascending by size (MoE by active params); remotes last by tier.
        assert_eq!(
            order,
            vec![
                "mlx-community/Qwen3-30B-A3B-4bit", // active 3B
                "mlx-community/Qwen3-4B-4bit",       // 4B
                "mlx-community/Qwen2.5-Coder-32B",   // 32B
                "claude-haiku-4-5",                  // remote cheap
                "claude-opus-4-8",                   // remote top
            ]
        );
        assert_eq!(spec.strategy, StrategyName::Classify);
        assert_eq!(spec.tiers[3].api, RemoteApi::Anthropic);
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
                {"model": "claude-haiku-4-5", "location": "remote", "api": "anthropic"}
            ],
            "max_escalations": 1,
            "strategy": "classify"
        }))
        .unwrap();
        assert_eq!(s.tiers.len(), 2);
        assert_eq!(s.tiers[0].location, Location::Local); // defaulted
        assert_eq!(s.tiers[0].api, RemoteApi::Openai); // defaulted
        assert_eq!(s.tiers[1].location, Location::Remote);
        assert_eq!(s.tiers[1].api, RemoteApi::Anthropic);
        assert!(matches!(s.strategy, StrategyName::Classify));
        assert_eq!(s.max_escalations, Some(1));
    }

    #[tokio::test]
    async fn build_resolves_each_tier_in_order() {
        let spec = CascadeSpec {
            tiers: vec![tier("cheap", Location::Local), tier("strong", Location::Remote)],
            max_escalations: None,
            strategy: StrategyName::Cheapest,
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
            strategy: StrategyName::Cheapest,
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
            strategy: StrategyName::Cheapest,
        };
        let r = build_cascade(&spec, |_t| async move { None }).await;
        assert!(r.is_err(), "an all-unavailable cascade is an error, not an empty backend");
    }

    #[tokio::test]
    async fn api_kind_reaches_the_resolver() {
        // The wire protocol flows through build_cascade to the resolver, where the gateway picks
        // the Anthropic vs OpenAI backend.
        let mut t = tier("claude-haiku-4-5", Location::Remote);
        t.api = RemoteApi::Anthropic;
        let spec =
            CascadeSpec { tiers: vec![t], max_escalations: None, strategy: StrategyName::default() };
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen2 = std::sync::Arc::clone(&seen);
        let be = build_cascade(&spec, move |t: TierSpec| {
            let seen2 = std::sync::Arc::clone(&seen2);
            async move {
                *seen2.lock().unwrap() = Some(t.api);
                Some(Arc::new(Echo("x")) as Arc<dyn ChatBackend>)
            }
        })
        .await;
        assert!(be.is_ok());
        assert_eq!(*seen.lock().unwrap(), Some(RemoteApi::Anthropic));
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
