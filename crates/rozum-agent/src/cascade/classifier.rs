//! Difficulty classification → the `ClassifyThenStart` routing strategy (Phase 5).
//!
//! `AlwaysCheapest` (Phase 1) always starts at tier 0 and climbs only on escalation. That is
//! optimal when the cheap model usually suffices, but it wastes a round-trip on every obviously
//! hard request (long code, multi-step reasoning) that the cheap tier will predictably punt.
//!
//! `ClassifyThenStart` scores the request's *difficulty* once, up front, and starts the cascade
//! at a proportional tier — a trivial "hi" still begins at the cheapest model, while a 2k-token
//! refactor begins partway up. Escalation still works from there; classification only moves the
//! *entry point*, never the ceiling. See `docs/specs/cascade-router.md`.

use crate::backend::{ChatRequest, ContentBlock, Role};

/// Estimates how hard a request is, `0.0` (trivial) .. `1.0` (hard). Pure and cheap: a heuristic
/// over the prompt, or (future) a tiny classifier model. The cascade maps the score to a start
/// tier.
pub trait Classifier: Send + Sync {
    fn difficulty(&self, req: &ChatRequest) -> f32;
}

/// How the cascade picks its *entry* model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RoutingStrategy {
    /// Always start at the cheapest tier; climb only when the answer is rejected (Phase 1).
    #[default]
    AlwaysCheapest,
    /// Score difficulty up front and start at a proportional tier (Phase 5).
    ClassifyThenStart,
    /// Start at the cheapest tier that the **learned stats** show has been good enough for this
    /// task-class; fall back to `ClassifyThenStart` when there's not enough evidence (Phase 7).
    Learned,
    /// **Pipeline** (not escalation): every request flows through ALL tiers in cost order — each
    /// non-final tier is a *planner/advisor* (produces guidance, no tools), and its output is
    /// forwarded into the next tier's input; the final tier is the *executor* (gets the real tools,
    /// streams the answer back). A chain of models behind one endpoint, where the first tells the
    /// next what to do. See `docs/specs/pipeline-cascade.md`.
    Pipeline,
}

/// A free, deterministic difficulty heuristic over the prompt's surface features: length, code
/// markers, math/reasoning cues, multi-step asks, the number of offered tools, and conversation
/// depth. Tuned so a bare greeting scores ~0 and a long multi-signal prompt saturates near 1.
pub struct HeuristicClassifier;

/// Substrings that signal the model is being asked to read or write code.
const CODE_MARKERS: &[&str] =
    &["```", "def ", "function ", "class ", "impl ", "fn ", "import ", "#include", "() =>"];

/// Substrings that signal math / formal reasoning.
const MATH_MARKERS: &[&str] = &[
    "solve", "calculate", "equation", "integral", "derivative", "prove", "theorem", "factor",
];

/// Substrings that signal a multi-step / analytical ask (the model must plan, not just recall).
const MULTISTEP_MARKERS: &[&str] = &[
    "step by step",
    "explain why",
    "analyze",
    "compare",
    "design",
    "reason",
    "trade-off",
    "tradeoff",
    "refactor",
    "debug",
];

/// Collect the user/assistant turn text (the actual ask) lowercased — system prompts and tool
/// results are excluded so boilerplate doesn't inflate the score.
fn prompt_text(req: &ChatRequest) -> String {
    let mut s = String::new();
    for m in &req.messages {
        if !matches!(m.role, Role::User | Role::Assistant) {
            continue;
        }
        for b in &m.content {
            if let ContentBlock::Text { text } = b {
                s.push_str(text);
                s.push('\n');
            }
        }
    }
    s.to_lowercase()
}

/// True if `text` contains any of `needles`.
fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

impl Classifier for HeuristicClassifier {
    fn difficulty(&self, req: &ChatRequest) -> f32 {
        let text = prompt_text(req);
        let mut score = 0.0_f32;

        // Length: longer prompts carry more to attend to. Saturates at ~2k chars → 0.3.
        score += (text.len() as f32 / 2000.0).min(1.0) * 0.3;
        // Code in the prompt → likely a code task.
        if contains_any(&text, CODE_MARKERS) {
            score += 0.3;
        }
        // Math / formal reasoning.
        if contains_any(&text, MATH_MARKERS) {
            score += 0.25;
        }
        // Multi-step / analytical asks.
        if contains_any(&text, MULTISTEP_MARKERS) {
            score += 0.2;
        }
        // Many tools → a richer decision space to navigate. Saturates at 10 tools → 0.2.
        score += (req.tools.len() as f32 / 10.0).min(1.0) * 0.2;
        // A deep conversation has more context to track.
        if req.messages.len() > 6 {
            score += 0.1;
        }

        score.min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{Message, ToolDef};
    use serde_json::json;

    fn req_with(messages: Vec<Message>, tools: Vec<ToolDef>) -> ChatRequest {
        let mut r = ChatRequest::simple("");
        r.messages = messages;
        r.tools = tools;
        r
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef { name: name.into(), description: String::new(), input_schema: json!({}) }
    }

    #[test]
    fn trivial_greeting_scores_near_zero() {
        let d = HeuristicClassifier.difficulty(&ChatRequest::simple("hi"));
        assert!(d < 0.05, "a bare greeting is trivial, got {d}");
    }

    #[test]
    fn code_block_lifts_difficulty() {
        let d = HeuristicClassifier
            .difficulty(&ChatRequest::simple("fix this:\n```\nfn main() {}\n```"));
        assert!(d >= 0.3, "a code prompt should clear the cheap tier, got {d}");
    }

    #[test]
    fn multi_step_reasoning_lifts_difficulty() {
        let d = HeuristicClassifier
            .difficulty(&ChatRequest::simple("analyze and compare these two designs step by step"));
        assert!(d >= 0.2, "a multi-step ask should lift difficulty, got {d}");
    }

    #[test]
    fn long_multi_signal_prompt_saturates_high() {
        let body = "refactor ".repeat(300); // ~2.7k chars, multi-step marker present
        let d = HeuristicClassifier.difficulty(&ChatRequest::simple(format!(
            "```\n{body}\n```\nprove the theorem step by step"
        )));
        assert!(d >= 0.8, "long + code + math + multi-step should saturate, got {d}");
    }

    #[test]
    fn many_tools_lift_difficulty() {
        let tools: Vec<_> = (0..10).map(|i| tool(&format!("t{i}"))).collect();
        let d = HeuristicClassifier.difficulty(&req_with(vec![Message::user("pick one")], tools));
        assert!(d >= 0.2, "ten tools add to difficulty, got {d}");
    }

    #[test]
    fn system_prompt_does_not_inflate_score() {
        // A big system prompt + a trivial user turn should still read as easy.
        let messages = vec![
            Message::system("you are a careful assistant. ".repeat(200)),
            Message::user("hi"),
        ];
        let d = HeuristicClassifier.difficulty(&req_with(messages, vec![]));
        assert!(d < 0.05, "system boilerplate must not inflate difficulty, got {d}");
    }
}
