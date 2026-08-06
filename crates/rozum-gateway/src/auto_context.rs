//! Making a conversation fit the context window without the caller noticing.
//!
//! Extracted from `gateway.rs` (`gw-monolith-decompose`). Six items: estimate what a request will
//! cost in tokens, drop the oldest turns while keeping the system prompt and the recent ones, and
//! optionally summarise what was dropped so the model is told rather than silently amnesiac.
//!
//! The decompose entry listed this as blocked on `error_json` and the streaming types. The error
//! module came out in the first slice, and measured today the family calls NOTHING outside itself
//! once `estimate_prompt_tokens` and `message_text` come along — which is why it moved as a leaf
//! rather than as the architectural pass the entry predicted.

/// Auto-context on by default; `ROZUM_AUTO_CONTEXT=0` restores the legacy "error on overflow".
/// Rough token estimate of the *whole* prompt the model will see — used for the
/// context-overflow preflight and reported as `est_prompt_tokens`. Unlike a Text-only
/// count it includes the parts that actually dominate an agentic request: prior tool-call
/// args, **tool results** (file dumps / command output, often the largest blocks), and the
/// **tool schemas** (which the chat template renders into the prompt — easily ~5K tokens
/// of Claude Code's ~33 tools). Counting only `Text` blocks under-counts a real coding
/// turn several-fold and can let an over-long prompt slip past the overflow guard.
/// gateway-auto-context (spec/backlog `gateway-auto-context`): token headroom reserved for the model's
/// reply so a fitted prompt doesn't fill 100% of the window leaving 0 for output.
pub(crate) const AUTO_CONTEXT_REPLY_RESERVE: u32 = 1024;

use std::sync::Arc;

use futures::StreamExt as _;
use tokio_util::sync::CancellationToken;

use axum::http::StatusCode;
use axum::response::Response;

use crate::backend::{ChatBackend, ChatEvent, ChatRequest, ContentBlock, Message, Role, SamplingParams, ToolDef};
use crate::errors::error_json;

pub(crate) fn auto_context_enabled() -> bool {
    !matches!(std::env::var("ROZUM_AUTO_CONTEXT").ok().as_deref(), Some("0" | "false" | "off"))
}

/// Make a request fit `ctx_win` so it NEVER returns `context_length_exceeded`, in two graceful steps,
/// returning the DROPPED turns (so the caller can attach an elision note — see [`with_elision_note`]):
///   1. **sliding-window trim** — drop the OLDEST non-system turns until it fits (keep all system msgs +
///      the most recent turns + reply headroom). The conversation-overflow case (the common one).
///   2. **tool-schema compression** (lazy-tools) — if turns alone can't fit (few turns but a fat
///      system + many tool schemas, e.g. codex's ~18 tools > a small window), STRIP tool `description`s
///      — keeps every tool (no capability loss), just terser docs.
/// Lossy-but-graceful: a transformer cannot attend beyond `n_ctx`, so we choose what to shrink instead of
/// erroring. With auto-context OFF, preserves the legacy error (`Err(resp)` only in the OFF case).
pub(crate) fn fit_to_context(
    mut messages: Vec<Message>,
    mut tools: Vec<ToolDef>,
    ctx_win: u32,
) -> Result<(Vec<Message>, Vec<ToolDef>, Vec<Message>), Response> {
    if ctx_win == 0 {
        return Ok((messages, tools, vec![])); // unknown window → don't touch it
    }
    let budget = ctx_win.saturating_sub(AUTO_CONTEXT_REPLY_RESERVE);
    if estimate_prompt_tokens(&messages, &tools) <= budget {
        return Ok((messages, tools, vec![])); // already fits — no-op
    }
    if !auto_context_enabled() {
        return Err(error_json(
            StatusCode::BAD_REQUEST,
            &format!("prompt exceeds model context window of {ctx_win} tokens"),
            "context_length_exceeded",
        ));
    }
    // Step 1 — drop oldest non-system turns (keep ≥1 non-system turn + all system msgs), keeping the
    // dropped turns so the caller can summarize them into an elision note.
    let mut dropped: Vec<Message> = Vec::new();
    while estimate_prompt_tokens(&messages, &tools) > budget {
        if messages.iter().filter(|m| !matches!(m.role, Role::System)).count() <= 1 {
            break;
        }
        let Some(i) = messages.iter().position(|m| !matches!(m.role, Role::System)) else {
            break;
        };
        dropped.push(messages.remove(i));
    }
    // Step 2 — if it still doesn't fit, compress tool schemas: strip descriptions (the bulk of a fat
    // tool surface) while keeping every tool callable. Last resort before serving an over-long prompt.
    let mut compressed = 0usize;
    if estimate_prompt_tokens(&messages, &tools) > budget {
        for t in &mut tools {
            if !t.description.is_empty() {
                t.description = String::new();
                compressed += 1;
            }
        }
    }
    if !dropped.is_empty() || compressed > 0 {
        tracing::info!(
            dropped = dropped.len(), compressed, ctx_win,
            "gateway-auto-context: fitted prompt to the window (trimmed turns / compressed tool schemas)"
        );
        crate::obs::log_event(serde_json::json!({
            "event": "auto_context_trim", "dropped": dropped.len(), "tools_compressed": compressed, "ctx_win": ctx_win,
        }));
    }
    Ok((messages, tools, dropped))
}

/// Abstractive LLM rolling-summary (`ROZUM_AUTO_CONTEXT_SUMMARIZE=1`, default OFF): instead of an
/// extractive topic breadcrumb, generate a real summary of the dropped turns via the resident model.
/// Default OFF — it adds a summarizer generation per overflowing request (latency); the deterministic
/// extractive note is the safe default. The summary gen completes before the real generation (the worker
/// serializes them) so it can't deadlock; on any failure it falls back to the extractive note.
pub(crate) fn auto_context_summarize_enabled() -> bool {
    matches!(std::env::var("ROZUM_AUTO_CONTEXT_SUMMARIZE").ok().as_deref(), Some("1" | "true" | "on"))
}

/// Flatten a message's content to plain text (Text + tool-result bodies) for summarization/snippets.
pub(crate) fn message_text(m: &Message) -> String {
    let mut s = String::new();
    for b in &m.content {
        let piece = match b {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::ToolResult { content, .. } => content.as_str(),
            _ => continue,
        };
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(piece);
    }
    s
}

/// Abstractive summary of the dropped turns via the resident model. `None` on any failure (caller falls
/// back to the extractive note).
pub(crate) async fn summarize_dropped(
    dropped: &[Message],
    backend: &Arc<dyn ChatBackend>,
) -> Option<Message> {
    let mut convo = String::new();
    for m in dropped {
        let role = match m.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::System => "System",
            Role::Tool => "Tool",
        };
        let t = message_text(m);
        let t = t.trim();
        if !t.is_empty() {
            convo.push_str(role);
            convo.push_str(": ");
            convo.push_str(t);
            convo.push('\n');
        }
    }
    if convo.trim().is_empty() {
        return None;
    }
    let req = ChatRequest {
        messages: vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "Compress conversation history. Reply with a terse 1-3 sentence summary of the \
                           key facts, decisions, and context in the turns below — no preamble, no markdown."
                        .into(),
                }],
            },
            Message { role: Role::User, content: vec![ContentBlock::Text { text: convo }] },
        ],
        tools: vec![],
        sampling: SamplingParams { temperature: Some(0.2), max_tokens: Some(256), ..Default::default() },
        cancel: CancellationToken::new(),
        session_id: None,
    };
    let mut stream = backend.chat(req).await.ok()?;
    let mut out = String::new();
    while let Some(ev) = stream.next().await {
        if let Ok(ChatEvent::TextDelta { text }) = ev {
            out.push_str(&text);
        }
    }
    let s = out.trim();
    if s.is_empty() {
        return None;
    }
    Some(Message {
        role: Role::System,
        content: vec![ContentBlock::Text {
            text: format!("[Summary of {} earlier omitted turn(s): {s}]", dropped.len()),
        }],
    })
}

pub(crate) fn estimate_prompt_tokens(messages: &[Message], tools: &[ToolDef]) -> u32 {
    let mut chars = 0usize;
    for m in messages {
        for b in &m.content {
            chars += match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
                ContentBlock::Image { data } => data.len() / 8, // rough token proxy
            };
        }
    }
    for t in tools {
        chars += t.name.len() + t.description.len() + t.input_schema.to_string().len();
    }
    (chars as f32 / 3.5) as u32 + 1
}



/// The deterministic EXTRACTIVE elision note: the topic of each dropped turn (first ~80 chars, cap 6).
pub(crate) fn extractive_note(dropped: &[Message], ctx_win: u32) -> Message {
    let topics: Vec<String> = dropped
        .iter()
        .filter_map(|m| {
            let s: String = message_text(m).chars().take(80).collect::<String>().replace('\n', " ");
            let s = s.trim().to_string();
            (!s.is_empty()).then_some(s)
        })
        .take(6)
        .collect();
    let n = dropped.len();
    let text = if topics.is_empty() {
        format!(
            "[Note: {n} earlier turn(s) were omitted to fit the model's {ctx_win}-token context window — \
             you may not have the full prior history.]"
        )
    } else {
        let joined = topics.iter().map(|t| format!("\u{201c}{t}\u{2026}\u{201d}")).collect::<Vec<_>>().join("; ");
        format!(
            "[Note: {n} earlier turn(s) were omitted to fit the {ctx_win}-token window — you may not have \
             the full prior history. They covered (truncated): {joined}]"
        )
    };
    Message { role: Role::System, content: vec![ContentBlock::Text { text }] }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    use crate::oai_api::{OaiMsg, oai_messages_to_internal};

    #[test]
    fn auto_context_trims_oldest_keeps_system_and_recent() {
        let txt = |s: &str| Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: s.into() }],
        };
        let sys = Message {
            role: Role::System,
            content: vec![ContentBlock::Text { text: "SYSTEM PROMPT".into() }],
        };
        // No-op when the prompt already fits a generous window (nothing dropped).
        let small = vec![sys.clone(), txt("hi")];
        let (m, _, d) = fit_to_context(small, vec![], 8192).unwrap();
        assert_eq!(m.len(), 2);
        assert!(d.is_empty(), "nothing dropped when it fits");
        // Overflow: many turns + a tiny window → trims to system + the single most-recent turn; the
        // dropped turns are returned (oldest first) for the elision note.
        let mut many = vec![sys];
        for i in 0..30 {
            many.push(txt(&format!("turn {i} word word word word word word word word word")));
        }
        let (fitted, _, dropped) = fit_to_context(many, vec![], 64).unwrap(); // budget 64−1024 → 0
        assert!(fitted.iter().any(|m| matches!(m.role, Role::System)), "system message kept");
        assert_eq!(
            fitted.iter().filter(|m| !matches!(m.role, Role::System)).count(),
            1,
            "exactly one (most-recent) turn kept"
        );
        let kept = fitted.iter().rev().find(|m| !matches!(m.role, Role::System)).unwrap();
        if let ContentBlock::Text { text } = &kept.content[0] {
            assert!(text.contains("turn 29"), "kept the MOST RECENT turn, got: {text}");
        }
        assert!(!dropped.is_empty(), "dropped turns returned for the elision note");
        // The EXTRACTIVE note tells the model history was trimmed AND what the oldest dropped turn
        // covered (so it has the gist, not just a count). (Abstractive LLM summary is the opt-in path.)
        let note = extractive_note(&dropped, 64);
        if let ContentBlock::Text { text } = &note.content[0] {
            assert!(
                text.contains("omitted") && text.contains("covered") && text.contains("turn 0"),
                "extractive note carries the oldest dropped turn's topic, got: {text}"
            );
        }
        // lazy-tools: ONE turn (turn-dropping can't help) + fat tool descriptions → descriptions get
        // STRIPPED but every tool is KEPT (no capability loss). The codex fat-system case.
        let fat_tools: Vec<ToolDef> = (0..10)
            .map(|i| ToolDef {
                name: format!("tool{i}"),
                description: "a long tool description ".repeat(40),
                input_schema: json!({"type":"object","properties":{"x":{"type":"string"}}}),
            })
            .collect();
        let (msgs2, tools2, _) = fit_to_context(vec![txt("do it")], fat_tools, 64).unwrap();
        assert_eq!(msgs2.len(), 1, "the single turn is kept");
        assert_eq!(tools2.len(), 10, "all tools KEPT (compressed, not dropped)");
        assert!(tools2.iter().all(|t| t.description.is_empty()), "tool descriptions stripped to fit");
        // OFF → legacy error (Err) on overflow.
        // SAFETY: test-local env toggle.
        unsafe { std::env::set_var("ROZUM_AUTO_CONTEXT", "0") };
        let over = vec![txt(&"word ".repeat(1000))];
        assert!(fit_to_context(over, vec![], 64).is_err(), "auto-context OFF → legacy error");
        unsafe { std::env::remove_var("ROZUM_AUTO_CONTEXT") };
    }

    #[test]
    fn estimate_prompt_tokens_counts_text_results_and_tools() {
        // Large user text → large estimate (baseline behaviour).
        let long_text: String = "word ".repeat(200_000);
        let messages = oai_messages_to_internal(&[OaiMsg {
            role: "user".into(),
            content: Value::String(long_text),
            tool_calls: vec![],
            tool_call_id: None,
        }]);
        let base = estimate_prompt_tokens(&messages, &[]);
        assert!(base > 100_000, "expected large token estimate, got {base}");

        // A big tool RESULT (e.g. a file dump) must be counted — the old Text-only
        // count ignored it entirely, under-counting an agentic turn several-fold.
        let dump = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "x".repeat(40_000),
                is_error: false,
            }],
        }];
        assert!(estimate_prompt_tokens(&dump, &[]) > 8_000, "tool results must be counted");

        // Tool schemas render into the prompt → they must add to the estimate.
        let tools = vec![ToolDef {
            name: "Bash".into(),
            description: "run a shell command ".repeat(100),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"}}}),
        }];
        assert!(
            estimate_prompt_tokens(&dump, &tools) > estimate_prompt_tokens(&dump, &[]),
            "tool schemas must increase the estimate"
        );
    }
}

