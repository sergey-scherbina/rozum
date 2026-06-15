//! The acceptance pipeline — decide, after a model answers, whether the answer is good enough
//! (`Accept`), needs a stronger model (`Escalate`), or this check has no opinion
//! (`Inconclusive`). Checks are ordered cheapest-first; the first `Accept`/`Escalate` wins, and
//! all-`Inconclusive` defaults to `Accept` (never escalate without a positive reason). See
//! `docs/specs/cascade-router.md`. Phase 1 ships L0 (structural).

use crate::backend::{ChatRequest, StopReason};
use crate::constrain::Schema;

/// The collected result of one model attempt (the stream drained to a value).
#[derive(Debug, Default)]
pub struct TurnOutcome {
    pub text: String,
    /// `(id, name, arguments-json)` per tool call.
    pub tool_calls: Vec<(String, String, String)>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub stop_reason: Option<StopReason>,
    /// A backend/stream error, if the attempt failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Accept,
    Escalate,
    Inconclusive,
}

/// One acceptance check over `(request, answer)`. Cheap checks come first in the config.
pub trait AcceptanceCheck: Send + Sync {
    fn decide(&self, req: &ChatRequest, answer: &TurnOutcome) -> Verdict;
}

/// **L0 — structural validation** (free, deterministic). A backend error escalates. If the
/// request demands structure — a `response_format` schema, or tool calls against the offered
/// tools — and the output fails it, escalate; if it conforms, accept. Otherwise no opinion
/// (free-form text → `Inconclusive`). Reuses the `constrain` engine.
pub struct StructuralCheck;

impl AcceptanceCheck for StructuralCheck {
    fn decide(&self, req: &ChatRequest, ans: &TurnOutcome) -> Verdict {
        if ans.error.is_some() {
            return Verdict::Escalate;
        }
        // Structured output: the whole reply must conform to the response schema.
        if let Some(schema) = &req.sampling.response_schema {
            let s = Schema::parse(schema);
            return if s.is_complete(ans.text.trim()) {
                Verdict::Accept
            } else {
                Verdict::Escalate
            };
        }
        // Tool use: any tool call must name an offered tool and carry schema-valid args.
        if !req.tools.is_empty() && !ans.tool_calls.is_empty() {
            for (_, name, args) in &ans.tool_calls {
                let Some(tool) = req.tools.iter().find(|t| &t.name == name) else {
                    return Verdict::Escalate; // hallucinated tool
                };
                let s = Schema::parse(&tool.input_schema);
                if !s.is_complete(args.trim()) {
                    return Verdict::Escalate; // malformed / non-conformant args
                }
            }
            return Verdict::Accept;
        }
        Verdict::Inconclusive
    }
}

/// Run the ordered checks: the first decisive `Accept`/`Escalate` wins; `None` if every check is
/// `Inconclusive` (the caller then consults the L2 judge, if configured).
pub fn pipeline_verdict(
    checks: &[Box<dyn AcceptanceCheck>],
    req: &ChatRequest,
    ans: &TurnOutcome,
) -> Option<Verdict> {
    for c in checks {
        match c.decide(req, ans) {
            Verdict::Inconclusive => continue,
            v => return Some(v),
        }
    }
    None
}
