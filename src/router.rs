//! Small model as router / classifier — the up-front *pre-filter* counterpart to
//! the cascade's after-the-fact escalation. A small model (4B / Coder-7B) reads a
//! query and picks one of a **caller-supplied** label set: intent, a route (which
//! model/tool), RAG relevance, a difficulty bucket. Cheap and single-shot — reserve
//! the big model for the actual work. See `docs/specs/small-model-router.md`.
//!
//! Engine-agnostic: it drives any [`ChatBackend`] (mirrors `cascade::ModelJudge` —
//! tight prompt, `temp 0`, tiny `max_tokens`, parse-with-fallback). It **never
//! errors**: an unparseable / off-set reply snaps to the nearest label, and total
//! failure falls back to the first label with `fallback_used = true`. A cheap
//! pre-filter must degrade gracefully, not break the caller.

use std::sync::Arc;

use crate::{ChatBackend, ChatRequest, collect_to_string};

/// One classification target: a short `name` the model should emit and a one-line
/// `hint` describing it (shown in the prompt to steer the model).
#[derive(Debug, Clone)]
pub struct Label {
    pub name: String,
    pub hint: String,
}

impl Label {
    pub fn new(name: impl Into<String>, hint: impl Into<String>) -> Self {
        Self { name: name.into(), hint: hint.into() }
    }
}

/// The standard difficulty-ordered label set (cheapest → hardest) for using a
/// [`ModelRouter`] as the cascade's [`ModelRouter::difficulty`] source — `trivial`
/// → `0.0`, `moderate` → `0.5`, `hard` → `1.0`.
pub fn difficulty_labels() -> Vec<Label> {
    vec![
        Label::new("trivial", "a greeting, a one-word answer, or a simple lookup"),
        Label::new("moderate", "a normal question needing a short focused answer"),
        Label::new("hard", "long multi-step reasoning, math proofs, or substantial code"),
    ]
}

/// The outcome of a classification: the chosen label, a coarse confidence, and
/// whether the router fell back (the model's reply didn't name a label, or the call
/// failed). `confidence` is coarse in v1 (no logprobs): `1.0` exact, `0.6` snapped
/// from noisy text, `0.0` fallback.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    pub label: String,
    pub confidence: f32,
    pub fallback_used: bool,
}

/// A small-model classifier over a fixed label set.
pub struct ModelRouter {
    backend: Arc<dyn ChatBackend>,
    labels: Vec<Label>,
    max_tokens: u32,
}

impl ModelRouter {
    /// Build a router. Rejects an empty label set (there'd be nothing to choose).
    pub fn new(backend: Arc<dyn ChatBackend>, labels: Vec<Label>) -> Result<Self, String> {
        if labels.is_empty() {
            return Err("ModelRouter: at least one label is required".into());
        }
        Ok(Self { backend, labels, max_tokens: 16 })
    }

    /// The label set this router chooses among.
    pub fn labels(&self) -> &[Label] {
        &self.labels
    }

    /// Classify `query` into one of the labels. Greedy, single-shot, never errors —
    /// an off-set / empty reply falls back to the first label.
    pub async fn classify(&self, query: &str) -> Classification {
        let mut req = ChatRequest::simple(self.prompt(query));
        req.sampling.temperature = Some(0.0);
        req.sampling.max_tokens = Some(self.max_tokens);
        let reply = match self.backend.chat(req).await {
            Ok(stream) => collect_to_string(stream).await.unwrap_or_default(),
            Err(_) => String::new(),
        };
        match snap_to_label(&reply, &self.labels) {
            Some(m) => Classification {
                label: self.labels[m.index].name.clone(),
                confidence: if m.exact { 1.0 } else { 0.6 },
                fallback_used: false,
            },
            None => Classification {
                label: self.labels[0].name.clone(),
                confidence: 0.0,
                fallback_used: true,
            },
        }
    }

    /// A difficulty score in `0.0..=1.0` for `query`, for use as the cascade's
    /// (async, model-backed) `Classifier` signal. The label set is treated as
    /// ordered **cheapest → hardest**, and the chosen label's position maps to
    /// `index / (n-1)` — so construct the router with difficulty-ordered labels
    /// (e.g. `["trivial","moderate","hard"]`) to use this. A single label, or a
    /// fallback (the model didn't name a label / the call failed), → `0.0` (the
    /// conservative cheap-first default; the cascade still escalates from there).
    pub async fn difficulty(&self, query: &str) -> f32 {
        if self.labels.len() <= 1 {
            return 0.0;
        }
        let c = self.classify(query).await;
        let idx = self.labels.iter().position(|l| l.name == c.label).unwrap_or(0);
        idx as f32 / (self.labels.len() - 1) as f32
    }

    /// The classification prompt: enumerate labels + hints, ask for ONLY the name.
    fn prompt(&self, query: &str) -> String {
        let mut p = String::from(
            "You are a fast text classifier. Classify the input below into exactly one \
             of these categories:\n\n",
        );
        for l in &self.labels {
            p.push_str("- ");
            p.push_str(&l.name);
            if !l.hint.is_empty() {
                p.push_str(": ");
                p.push_str(&l.hint);
            }
            p.push('\n');
        }
        p.push_str("\nInput:\n");
        p.push_str(query);
        p.push_str("\n\nReply with ONLY the category name, nothing else.");
        p
    }
}

/// A matched label and whether it was an exact (whole-reply) match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LabelMatch {
    pub index: usize,
    pub exact: bool,
}

/// Lowercase alphanumeric tokens of `s` (the same split the lexical index uses).
fn tokens(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect()
}

/// Map a small model's noisy reply to one of `labels`. Tolerates surrounding text,
/// casing, punctuation, and a leading `"Label:"`:
///
/// 1. exact — the trimmed/lowercased reply equals a label name → that label, `exact`.
/// 2. snap — the label's name (as a contiguous token run) appears in the reply; if
///    **exactly one** label matches → that label (not exact). Word-boundary matching
///    on tokens, so `code` does not match inside `decode`.
/// 3. otherwise (no label, or two+ ambiguously) → `None` (the caller falls back).
pub fn snap_to_label(reply: &str, labels: &[Label]) -> Option<LabelMatch> {
    let reply_tokens = tokens(reply);
    // 1. exact — the whole reply is just the label's tokens (tolerates stray case /
    //    punctuation / whitespace, e.g. "  Code.  " → exact "code").
    for (i, l) in labels.iter().enumerate() {
        let lt = tokens(&l.name);
        if !lt.is_empty() && reply_tokens == lt {
            return Some(LabelMatch { index: i, exact: true });
        }
    }
    // 2. unique token-run match.
    let mut found: Option<usize> = None;
    for (i, l) in labels.iter().enumerate() {
        let lt = tokens(&l.name);
        if lt.is_empty() {
            continue;
        }
        let matches = reply_tokens.windows(lt.len()).any(|w| w == lt.as_slice());
        if matches {
            if found.is_some() {
                return None; // ambiguous: two+ labels named in the reply
            }
            found = Some(i);
        }
    }
    found.map(|index| LabelMatch { index, exact: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels() -> Vec<Label> {
        vec![
            Label::new("code", "writing or editing code"),
            Label::new("math", "calculation or proofs"),
            Label::new("chitchat", "casual conversation"),
        ]
    }

    #[test]
    fn snap_exact_match() {
        let m = snap_to_label("code", &labels()).unwrap();
        assert_eq!(m, LabelMatch { index: 0, exact: true });
    }

    #[test]
    fn snap_cased_and_punctuated() {
        let m = snap_to_label("  Code.  ", &labels()).unwrap();
        assert_eq!(m, LabelMatch { index: 0, exact: true });
    }

    #[test]
    fn snap_label_prefix_and_surrounding_text() {
        // "Label: math" / a sentence naming the label → snapped (not exact).
        let m = snap_to_label("Label: math", &labels()).unwrap();
        assert_eq!(m, LabelMatch { index: 1, exact: false });
        let m2 = snap_to_label("I think this is chitchat, honestly", &labels()).unwrap();
        assert_eq!(m2, LabelMatch { index: 2, exact: false });
    }

    #[test]
    fn snap_no_substring_false_positive() {
        // "decode" must NOT match the label "code" (word-boundary tokens).
        assert!(snap_to_label("please decode this", &labels()).is_none());
    }

    #[test]
    fn snap_ambiguous_is_none() {
        // Two labels named → ambiguous → fall back.
        assert!(snap_to_label("is this code or math?", &labels()).is_none());
    }

    #[test]
    fn snap_off_set_is_none() {
        assert!(snap_to_label("banana", &labels()).is_none());
        assert!(snap_to_label("", &labels()).is_none());
    }

    #[tokio::test]
    async fn empty_label_set_rejected() {
        let backend: Arc<dyn ChatBackend> = Arc::new(crate::HelloBackend::new());
        assert!(ModelRouter::new(backend, vec![]).is_err());
    }

    #[tokio::test]
    async fn fallback_when_model_reply_off_set() {
        // HelloBackend always replies "hello!" — never a label → fallback to labels[0].
        let backend: Arc<dyn ChatBackend> = Arc::new(crate::HelloBackend::new());
        let router = ModelRouter::new(backend, labels()).unwrap();
        let c = router.classify("solve x^2 = 4").await;
        assert_eq!(c.label, "code");
        assert!(c.fallback_used);
        assert_eq!(c.confidence, 0.0);
    }

    // Real-model accuracy eval (M4, ignored). A 4B classifies a small labeled set;
    // accuracy must clear the gate-the-big-model bar. Run:
    //   cargo test --features mlx-native -- --ignored --nocapture model_router_eval
    #[cfg(feature = "mlx-native")]
    #[tokio::test]
    #[ignore = "heavy: loads mlx-community/Qwen3-4B-4bit"]
    async fn model_router_eval() {
        use crate::mlx_native_backend::{MlxNativeBackend, ensure_model_dir};
        let spec = "mlx-community:Qwen3-4B-4bit";
        let dir = ensure_model_dir(spec).await.expect("resolve qwen3-4b");
        let backend: Arc<dyn ChatBackend> =
            Arc::new(MlxNativeBackend::new(dir, spec.replace(':', "/"), None).await.expect("load"));
        let router = ModelRouter::new(
            backend,
            vec![
                Label::new("code", "writing, editing, or explaining source code"),
                Label::new("math", "arithmetic, algebra, calculus, or proofs"),
                Label::new("chitchat", "greetings and casual small talk"),
            ],
        )
        .unwrap();

        let cases: &[(&str, &str)] = &[
            ("Write a Rust function that reverses a string", "code"),
            ("Fix the borrow checker error in this impl block", "code"),
            ("What is the derivative of x^3 + 2x?", "math"),
            ("Solve the equation 3x + 7 = 22", "math"),
            ("hey there, how's your day going?", "chitchat"),
            ("good morning! nice weather today", "chitchat"),
        ];
        let mut correct = 0usize;
        for (q, want) in cases {
            let c = router.classify(q).await;
            let ok = c.label == *want;
            correct += ok as usize;
            eprintln!(
                "ROUTER  want={want:8}  got={:8}  conf={:.1}  fallback={}  {}  | {q}",
                c.label,
                c.confidence,
                c.fallback_used,
                if ok { "OK" } else { "MISS" }
            );
        }
        let acc = correct as f32 / cases.len() as f32;
        eprintln!("ROUTER accuracy: {correct}/{} = {:.0}%", cases.len(), acc * 100.0);
        assert!(acc >= 0.80, "router accuracy {acc:.2} below the gate-the-big-model bar (0.80)");
    }
}
