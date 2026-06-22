//! L2 — the cheap judge (`cascade-p4-judge`). When L0/L1 are inconclusive (a free-form answer
//! with no structural requirement and no self-signal), a [`Judge`] scores the answer; below the
//! configured threshold → escalate. Last in the pipeline because it can cost an extra call, so
//! most requests are settled by the free L0/L1 checks first. Pluggable: a cheap heuristic
//! ([`HeuristicJudge`]) or a small model ([`ModelJudge`]).
//!
//! (A future *execution-feedback* judge — grounded in the agent's tool-call error outcomes,
//! `AgentOutcome.operations`, the most reliable quality signal — lives in the agent loop, not the
//! per-response cascade; see `docs/specs/cascade-router.md`.)

use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;

use crate::backend::{ChatBackend, ChatEvent, ChatRequest, ContentBlock, Role};

use super::acceptance::TurnOutcome;

/// Score an answer in `0.0..=1.0` (higher = better). The cascade accepts at/above the threshold.
#[async_trait]
pub trait Judge: Send + Sync {
    async fn score(&self, req: &ChatRequest, ans: &TurnOutcome) -> f32;
}

/// The judge plus its accept threshold (`score >= threshold` → accept, else escalate).
pub struct JudgeConfig {
    pub judge: Box<dyn Judge>,
    pub threshold: f32,
}

/// A free, deterministic judge from cheap surface signals: an empty answer scores 0, an explicit
/// non-answer ("I don't know", "I cannot…") scores low, everything else passes. No model call.
pub struct HeuristicJudge;

const NON_ANSWERS: &[&str] =
    &["i don't know", "i do not know", "i cannot", "i can't", "i'm not sure", "unable to"];

#[async_trait]
impl Judge for HeuristicJudge {
    async fn score(&self, _req: &ChatRequest, ans: &TurnOutcome) -> f32 {
        let t = ans.text.trim();
        if t.is_empty() {
            return 0.0;
        }
        let lower = t.to_lowercase();
        if NON_ANSWERS.iter().any(|p| lower.contains(p)) {
            return 0.2;
        }
        1.0
    }
}

/// A judge backed by a (cheap) model: ask it to rate the answer 0–10 and parse the number. On any
/// judge error the score is neutral (0.5) so a flaky judge never blocks the cascade.
pub struct ModelJudge {
    pub backend: Arc<dyn ChatBackend>,
}

#[async_trait]
impl Judge for ModelJudge {
    async fn score(&self, req: &ChatRequest, ans: &TurnOutcome) -> f32 {
        let prompt = format!(
            "You are grading an answer. Question:\n{}\n\nAnswer:\n{}\n\nRate the answer's quality \
             from 0 (useless/wrong) to 10 (excellent). Reply with ONLY the number.",
            first_user_text(req),
            ans.text
        );
        let mut jr = ChatRequest::simple(prompt);
        jr.sampling.temperature = Some(0.0);
        jr.sampling.max_tokens = Some(8);
        let mut stream = match self.backend.chat(jr).await {
            Ok(s) => s,
            Err(_) => return 0.5,
        };
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(ChatEvent::TextDelta { text }) => out.push_str(&text),
                Ok(ChatEvent::Done { .. }) => break,
                Err(_) => return 0.5,
                _ => {}
            }
        }
        parse_score(&out).map(|n| (n / 10.0).clamp(0.0, 1.0)).unwrap_or(0.5)
    }
}

pub(crate) fn first_user_text(req: &ChatRequest) -> String {
    req.messages
        .iter()
        .rev()
        .find(|m| matches!(m.role, Role::User))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|c| match c {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// The first integer/decimal in `s` (e.g. `"Rating: 7/10"` → `7`).
pub(crate) fn parse_score(s: &str) -> Option<f32> {
    let num: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    num.parse::<f32>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome_text(t: &str) -> TurnOutcome {
        TurnOutcome { text: t.to_string(), ..Default::default() }
    }

    #[test]
    fn parse_score_extracts_first_number() {
        assert_eq!(parse_score("8"), Some(8.0));
        assert_eq!(parse_score("Rating: 7/10"), Some(7.0));
        assert_eq!(parse_score("9.5 out of 10"), Some(9.5));
        assert_eq!(parse_score("no number here"), None);
    }

    #[tokio::test]
    async fn heuristic_judge_scores_surface_signals() {
        let j = HeuristicJudge;
        let r = ChatRequest::simple("q");
        assert_eq!(j.score(&r, &outcome_text("")).await, 0.0);
        assert!(j.score(&r, &outcome_text("I don't know the answer")).await < 0.5);
        assert_eq!(j.score(&r, &outcome_text("Paris is the capital of France.")).await, 1.0);
    }
}
