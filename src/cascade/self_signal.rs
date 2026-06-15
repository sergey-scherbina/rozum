//! L1 — the self-signal acceptance check + the escalation *affordance* (`cascade-p3-self-signal`).
//!
//! Rather than guessing whether a model "sounds unsure", we give it the skill: a system-prompt
//! instruction (injected for every non-top tier) that tells the model the escalation option
//! exists and that **admitting uncertainty beats guessing** — then a clear way to signal it (a
//! marker, or an `escalate`/`consult_stronger` tool call). [`SelfSignalCheck`] detects that signal
//! and escalates. See `docs/specs/cascade-router.md`.

use serde_json::json;

use crate::agent::CallbackToolSource;
use crate::backend::{ChatRequest, ContentBlock, Message, Role, ToolDef};

use super::acceptance::{AcceptanceCheck, TurnOutcome, Verdict};

/// The escalation affordance — the "skill" injected into a non-top model's system prompt so it
/// knows the option exists and how to use it.
#[derive(Clone)]
pub struct EscalationAffordance {
    /// Appended to (or set as) the system message.
    pub instruction: String,
    /// The marker the model emits to request escalation (also what `SelfSignalCheck` matches and
    /// what's stripped from a fallback answer).
    pub marker: String,
}

impl Default for EscalationAffordance {
    fn default() -> Self {
        let marker = "[[ESCALATE".to_string();
        Self {
            marker,
            instruction: "If you are not confident you can answer this correctly and completely, \
                do NOT guess — admitting it is better than being confidently wrong. In that case \
                reply with exactly `[[ESCALATE: <one-line reason>]]` and nothing else, and a more \
                capable model will take over. Answer directly only when you are confident."
                .to_string(),
        }
    }
}

/// Inject the affordance into `req`'s system prompt (append to an existing system message, or add
/// one at the front).
pub fn inject_affordance(mut req: ChatRequest, aff: &EscalationAffordance) -> ChatRequest {
    let add = format!("\n\n{}", aff.instruction);
    match req.messages.iter_mut().find(|m| matches!(m.role, Role::System)) {
        Some(sys) => sys.content.push(ContentBlock::Text { text: add }),
        None => req.messages.insert(0, Message::system(aff.instruction.clone())),
    }
    req
}

/// Drop an escalation marker (and anything after it) from a fallback answer. No-op for an empty
/// marker or text without it.
pub fn strip_marker(text: &str, marker: &str) -> String {
    if marker.is_empty() {
        return text.to_string();
    }
    match text.find(marker) {
        Some(i) => text[..i].trim_end().to_string(),
        None => text.to_string(),
    }
}

/// **L1 — self-signal.** Escalate when the model explicitly asks to (an `escalate`/`consult_stronger`
/// tool call, or the affordance marker in the text), or matches a configured refusal pattern.
/// Refusal-pattern matching is OFF by default — we rely on the *taught* signal, not guessing.
pub struct SelfSignalCheck {
    pub marker: String,
    pub escalate_tools: Vec<String>,
    pub refusal_patterns: Vec<String>,
}

impl Default for SelfSignalCheck {
    fn default() -> Self {
        Self {
            marker: "[[ESCALATE".to_string(),
            escalate_tools: vec!["escalate".to_string(), "consult_stronger".to_string()],
            refusal_patterns: Vec::new(),
        }
    }
}

impl AcceptanceCheck for SelfSignalCheck {
    fn decide(&self, _req: &ChatRequest, ans: &TurnOutcome) -> Verdict {
        if ans
            .tool_calls
            .iter()
            .any(|(_, name, _)| self.escalate_tools.iter().any(|t| t == name))
        {
            return Verdict::Escalate;
        }
        if !self.marker.is_empty() && ans.text.contains(&self.marker) {
            return Verdict::Escalate;
        }
        if !self.refusal_patterns.is_empty() {
            let lower = ans.text.to_lowercase();
            if self.refusal_patterns.iter().any(|p| lower.contains(p)) {
                return Verdict::Escalate;
            }
        }
        Verdict::Inconclusive
    }
}

/// The escalation tool as a [`ToolSource`](crate::agent::ToolSource) for agent mode — a model in
/// `run_agent` can call `consult_stronger` to defer. (In the cascade flow the *call itself* is the
/// signal; this handler just acks.)
pub fn escalation_tools() -> CallbackToolSource {
    CallbackToolSource::new().with_tool(
        ToolDef {
            name: "consult_stronger".to_string(),
            description: "Defer to a more capable model when you are not confident you can answer \
                correctly. Prefer this over guessing."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {"reason": {"type": "string"}},
                "required": ["reason"]
            }),
        },
        |args| {
            Ok(json!({
                "escalated": true,
                "reason": args.get("reason").cloned().unwrap_or(serde_json::Value::Null)
            }))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolSource;

    fn outcome(text: &str, tools: Vec<(&str, &str, &str)>) -> TurnOutcome {
        TurnOutcome {
            text: text.to_string(),
            tool_calls: tools
                .into_iter()
                .map(|(i, n, a)| (i.to_string(), n.to_string(), a.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn detects_marker_tool_and_default_inconclusive() {
        let c = SelfSignalCheck::default();
        let req = ChatRequest::simple("q");
        assert_eq!(c.decide(&req, &outcome("[[ESCALATE: unsure]]", vec![])), Verdict::Escalate);
        assert_eq!(
            c.decide(&req, &outcome("", vec![("id", "consult_stronger", "{}")])),
            Verdict::Escalate
        );
        assert_eq!(c.decide(&req, &outcome("the answer is 42", vec![])), Verdict::Inconclusive);
    }

    #[test]
    fn refusal_patterns_opt_in() {
        let mut c = SelfSignalCheck::default();
        let req = ChatRequest::simple("q");
        assert_eq!(c.decide(&req, &outcome("I don't know", vec![])), Verdict::Inconclusive);
        c.refusal_patterns = vec!["i don't know".to_string()];
        assert_eq!(c.decide(&req, &outcome("I don't know", vec![])), Verdict::Escalate);
    }

    #[test]
    fn affordance_injects_into_system_prompt() {
        let aff = EscalationAffordance::default();
        // No system message → one is prepended.
        let req = inject_affordance(ChatRequest::simple("hi"), &aff);
        assert!(matches!(req.messages[0].role, Role::System));
        // Existing system message → appended.
        let mut r2 = ChatRequest::simple("hi");
        r2.messages.insert(0, Message::system("be terse"));
        let r2 = inject_affordance(r2, &aff);
        let sys_text: String = r2.messages[0]
            .content
            .iter()
            .filter_map(|c| match c {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(sys_text.contains("be terse") && sys_text.contains("ESCALATE"));
    }

    #[test]
    fn strip_marker_removes_the_tail() {
        assert_eq!(strip_marker("answer [[ESCALATE: x]]", "[[ESCALATE"), "answer");
        assert_eq!(strip_marker("clean answer", "[[ESCALATE"), "clean answer");
        assert_eq!(strip_marker("anything", ""), "anything");
    }

    #[tokio::test]
    async fn escalation_tool_acks() {
        let t = escalation_tools();
        let out = t.dispatch("consult_stronger", json!({"reason": "too hard"})).await.unwrap();
        assert_eq!(out["escalated"], true);
        assert_eq!(out["reason"], "too hard");
    }
}
