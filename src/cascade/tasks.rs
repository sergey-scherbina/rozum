//! Task-typed **small-first** cascades — the narrow, bounded jobs a 4B/Coder-7B can do
//! (commit messages, PR descriptions, renames, docstrings, one-line fixes), run small-first
//! behind a cheap, task-specific **validator gate** that `ACCEPT`s a good cheap answer or
//! `ESCALATE`s to the big model. Most requests resolve on the cheap tier; only the hard
//! residue pays for the big model.
//!
//! This is a thin **preset over the cascade core** ([`super::CascadeBackend`]), not a new
//! engine: the only new pieces are one [`AcceptanceCheck`] gate per task + a two-tier config
//! builder + a prompt helper. Health/backoff, budget, learned stats, and residency lanes all
//! come from the cascade. It's the after-the-fact counterpart to the up-front
//! `small-model-router` (`docs/specs/small-model-router.md`). See
//! `docs/specs/small-model-cascade.md`.

use std::sync::Arc;

use crate::backend::{ChatBackend, ChatRequest};

use super::acceptance::{AcceptanceCheck, TurnOutcome, Verdict};
use super::scheduler::Lane;
use super::{CascadeConfig, Location, ModelCard, StructuralCheck};

/// A bounded, single-shot task served small-first with a cheap validator gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmallTask {
    /// Generate a git commit message from a diff.
    CommitMessage,
}

impl SmallTask {
    /// The task's decisive acceptance gate (runs after L0 structural).
    fn gate(self) -> Box<dyn AcceptanceCheck> {
        match self {
            SmallTask::CommitMessage => Box::new(CommitMessageGate::default()),
        }
    }
}

/// Build a two-tier small-first [`CascadeConfig`] for `task`: `small` (tier 0) is tried
/// first, escalating to `big` (tier 1) only when the task gate rejects the cheap answer.
/// `AlwaysCheapest` (the whole point is to *try cheap first*); acceptance = L0 structural
/// then the task gate; the self-signal L1 + escalation affordance are **off** (these tasks
/// have a concrete validator, not a self-reported confidence).
///
/// Both backends are treated as **local** (the dominant case — escalate to a bigger *local*
/// model). For a **remote** big tier, build the `CascadeConfig` directly so its `ModelCard`
/// gets `Location::Remote` (correct residency laning under concurrency).
pub fn small_task_config(
    task: SmallTask,
    small: Arc<dyn ChatBackend>,
    big: Arc<dyn ChatBackend>,
) -> CascadeConfig {
    let models = vec![local_card("small", 0, small), local_card("big", 1, big)];
    let mut cfg = CascadeConfig::new(models);
    cfg.acceptance = vec![Box::new(StructuralCheck), task.gate()];
    cfg.affordance = None;
    cfg
}

fn local_card(id: &str, tier: u32, backend: Arc<dyn ChatBackend>) -> ModelCard {
    ModelCard {
        id: id.into(),
        backend,
        tier,
        location: Location::Local,
        lane: Lane::default_for(Location::Local),
    }
}

/// Build a tight commit-message request for `diff` so the small model's output is
/// gate-shaped: an imperative subject ≤ 72 chars, no chatter/fences. Greedy, modest length.
pub fn commit_message_request(diff: &str) -> ChatRequest {
    let prompt = format!(
        "Write a git commit message for the following diff.\n\n\
         Rules:\n\
         - First line: a concise imperative subject, at most 72 characters \
           (e.g. \"Fix …\", \"Add …\", \"Remove …\").\n\
         - Optionally a blank line then a short body explaining WHY.\n\
         - Output ONLY the commit message — no preamble, no backticks, no quotes.\n\n\
         Diff:\n{diff}\n"
    );
    let mut req = ChatRequest::simple(prompt);
    req.sampling.temperature = Some(0.0);
    req.sampling.max_tokens = Some(200);
    req
}

/// Validate an LLM-generated git commit message **structurally** — free, deterministic, no
/// I/O. `ESCALATE`s on a missing / over-length / refusal / placeholder subject; otherwise
/// `ACCEPT`s. Never `Inconclusive` (it's the decisive task gate; commit messages are
/// free-form text, so the L0 [`StructuralCheck`] defers to it).
pub struct CommitMessageGate {
    /// Max subject-line length in chars. Default 72 (the git convention).
    pub max_subject_chars: usize,
}

impl Default for CommitMessageGate {
    fn default() -> Self {
        Self { max_subject_chars: 72 }
    }
}

impl AcceptanceCheck for CommitMessageGate {
    fn decide(&self, _req: &ChatRequest, ans: &TurnOutcome) -> Verdict {
        if ans.error.is_some() {
            return Verdict::Escalate;
        }
        // The subject is the first non-empty line, with any wrapping fence/quote/heading
        // markers stripped (small models sometimes wrap the message).
        let subject = ans
            .text
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("")
            .trim_matches(|c| matches!(c, '`' | '#' | '*' | '"' | '\'' | ' '))
            .trim();
        if subject.is_empty()
            || subject.chars().count() > self.max_subject_chars
            || looks_unusable(subject)
        {
            return Verdict::Escalate;
        }
        Verdict::Accept
    }
}

/// Does this subject look like a refusal, a chatter preamble, or an unfilled placeholder
/// rather than a real commit subject?
fn looks_unusable(subject: &str) -> bool {
    let s = subject.to_lowercase();
    const REFUSAL: &[&str] = &[
        "as an ai",
        "i can't",
        "i cannot",
        "i'm unable",
        "i am unable",
        "i'm sorry",
        "i am sorry",
    ];
    if REFUSAL.iter().any(|b| s.contains(b)) {
        return true;
    }
    // A chatter preamble instead of the subject itself.
    if ["here is", "here's", "here are", "sure,", "sure!", "okay", "ok,"]
        .iter()
        .any(|p| s.starts_with(p))
    {
        return true;
    }
    // An unfilled angle-bracket placeholder (`<subject>`), or the literal prompt echoed back.
    if subject.contains('<') && subject.contains('>') {
        return true;
    }
    matches!(s.as_str(), "commit message" | "your commit message" | "subject")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{
        collect_to_string, ChatEvent, ChatStream, ContentBlock, ModelResult, StopReason,
    };
    use crate::cascade::CascadeBackend;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const GOOD: &str = "Fix off-by-one in the ring buffer index\n\nThe head wrapped one slot early.";

    fn outcome(text: &str) -> TurnOutcome {
        TurnOutcome { text: text.into(), ..Default::default() }
    }

    // ─── Gate unit tests (no model) ─────────────────────────────────────────────

    #[test]
    fn gate_accepts_a_good_message() {
        assert_eq!(CommitMessageGate::default().decide(&ChatRequest::simple(""), &outcome(GOOD)), Verdict::Accept);
    }

    #[test]
    fn gate_escalates_empty() {
        let g = CommitMessageGate::default();
        assert_eq!(g.decide(&ChatRequest::simple(""), &outcome("   \n  ")), Verdict::Escalate);
    }

    #[test]
    fn gate_escalates_over_length_subject() {
        let long = "Refactor ".to_string() + &"x".repeat(80);
        assert_eq!(
            CommitMessageGate::default().decide(&ChatRequest::simple(""), &outcome(&long)),
            Verdict::Escalate
        );
    }

    #[test]
    fn gate_escalates_refusal_and_chatter_and_placeholder() {
        let g = CommitMessageGate::default();
        for bad in [
            "As an AI I cannot write commit messages",
            "Here is your commit message:",
            "<commit subject here>",
            "commit message",
        ] {
            assert_eq!(g.decide(&ChatRequest::simple(""), &outcome(bad)), Verdict::Escalate, "{bad:?}");
        }
    }

    #[test]
    fn gate_strips_a_wrapping_fence() {
        // The model wrapped the subject in backticks/quotes — strip and accept.
        assert_eq!(
            CommitMessageGate::default()
                .decide(&ChatRequest::simple(""), &outcome("`Add retry to the uploader`")),
            Verdict::Accept
        );
    }

    #[test]
    fn gate_does_not_false_positive_on_legit_subject() {
        // "commit message" appears but it's a real subject, not the placeholder.
        assert_eq!(
            CommitMessageGate::default()
                .decide(&ChatRequest::simple(""), &outcome("Fix commit message parser crash")),
            Verdict::Accept
        );
    }

    #[test]
    fn commit_message_request_embeds_the_diff() {
        let req = commit_message_request("@@ -1 +1 @@\n-old\n+new");
        let text = match &req.messages[0].content[0] {
            ContentBlock::Text { text } => text.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.contains("+new"), "diff embedded");
        assert!(text.contains("72 characters"), "length rule present");
        assert_eq!(req.sampling.temperature, Some(0.0));
    }

    // ─── e2e over the cascade (mock backends) ───────────────────────────────────

    /// A backend whose reply is chosen by call index; counts its calls.
    struct Seq {
        replies: Vec<&'static str>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ChatBackend for Seq {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            let i = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = self.replies.get(i).copied().unwrap_or_else(|| *self.replies.last().unwrap());
            let evs: Vec<ModelResult<ChatEvent>> = vec![
                Ok(ChatEvent::TextDelta { text: text.into() }),
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

    fn seq(replies: Vec<&'static str>) -> (Arc<dyn ChatBackend>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (Arc::new(Seq { replies, calls: calls.clone() }), calls)
    }

    async fn run(cascade: &CascadeBackend, diff: &str) -> String {
        let stream = cascade.chat(commit_message_request(diff)).await.unwrap();
        collect_to_string(stream).await.unwrap()
    }

    #[tokio::test]
    async fn good_cheap_answer_accepts_at_tier0_big_not_called() {
        let (small, small_calls) = seq(vec![GOOD]);
        let (big, big_calls) = seq(vec!["SHOULD NOT BE CALLED"]);
        let cascade = CascadeBackend::new(small_task_config(SmallTask::CommitMessage, small, big));
        let out = run(&cascade, "diff").await;
        assert!(out.starts_with("Fix off-by-one"));
        assert_eq!(small_calls.load(Ordering::SeqCst), 1);
        assert_eq!(big_calls.load(Ordering::SeqCst), 0, "big tier must not be reached");
    }

    #[tokio::test]
    async fn junk_cheap_answer_escalates_to_big() {
        let (small, _) = seq(vec!["As an AI I cannot help with that"]);
        let (big, big_calls) = seq(vec![GOOD]);
        let cascade = CascadeBackend::new(small_task_config(SmallTask::CommitMessage, small, big));
        let out = run(&cascade, "diff").await;
        assert!(out.starts_with("Fix off-by-one"), "big tier's answer is returned");
        assert_eq!(big_calls.load(Ordering::SeqCst), 1, "escalated to big exactly once");
    }

    #[tokio::test]
    async fn small_tier_hit_rate_is_measurable() {
        // The small tier passes 3 of 4 requests; the 1 junk reply escalates → big called once.
        let (small, _) = seq(vec![GOOD, GOOD, "sorry, I can't", GOOD]);
        let (big, big_calls) = seq(vec![GOOD]);
        let cascade = CascadeBackend::new(small_task_config(SmallTask::CommitMessage, small, big));
        for _ in 0..4 {
            run(&cascade, "diff").await;
        }
        assert_eq!(big_calls.load(Ordering::SeqCst), 1, "only the 1 junk answer escalated (3/4 cheap hit-rate)");
    }
}
