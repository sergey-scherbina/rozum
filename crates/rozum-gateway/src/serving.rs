//! What every dialect does with a request once it has been parsed.
//!
//! Extracted from `gateway.rs` (`gw-monolith-decompose`). Twelve items the three wire protocols all
//! call in the same order: normalise tool-choice and response-format, apply the determinism
//! environment, run the generation through the loop-breaker, bound it with an inactivity timeout,
//! note any elision, and cancel cleanly when the client disconnects.
//!
//! **This is the module that makes the "three thin handlers" claim in `gateway.rs` true rather than
//! aspirational.** Measured before moving: the OpenAI, Anthropic and Responses handlers call SIX of
//! these each, in the same sequence — the shared middle was always there, spelled out three times
//! by proximity instead of once by name.


use std::sync::Arc;

use std::time::Duration;

use axum::http::StatusCode;
use axum::response::{Response};
use futures::{StreamExt as _};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::backend::{
    ChatBackend, ChatRequest, ChatStream, Message, ModelError, ModelResult,
    Role, SamplingParams, ToolDef,
};
use axum::middleware::Next;

use crate::auto_context::{auto_context_summarize_enabled, extractive_note, summarize_dropped};
use crate::errors::error_json;
use crate::oai_api::ToolChoice;
use crate::switchboard::ChatLease;
use crate::loopbreak::*;

/// Insert an elision note (extractive by default; abstractive LLM summary when opted in) after the real
/// system messages, when turns were dropped. No-op when nothing was dropped (the common/no-overflow case).
pub(crate) async fn with_elision_note(
    mut messages: Vec<Message>,
    dropped: Vec<Message>,
    ctx_win: u32,
    backend: &Arc<dyn ChatBackend>,
) -> Vec<Message> {
    if dropped.is_empty() {
        return messages;
    }
    let note = if auto_context_summarize_enabled() {
        summarize_dropped(&dropped, backend).await.unwrap_or_else(|| extractive_note(&dropped, ctx_win))
    } else {
        extractive_note(&dropped, ctx_win)
    };
    let pos = messages.iter().position(|m| !matches!(m.role, Role::System)).unwrap_or(messages.len());
    messages.insert(pos, note);
    messages
}

/// Wraps a `ChatStream` and cancels the token when dropped.
/// When the axum Sse sink drops this stream (client disconnect), the backend
/// stops generating on the next token check.
pub(crate) struct CancelOnDrop {
    pub(crate) stream: ChatStream,
    pub(crate) cancel: CancellationToken,
    /// Kept alive for the whole stream so a `switch` waits for streaming to
    /// finish (the lease counts against `generating`) before swapping the model.
    pub(crate) _lease: Option<ChatLease>,
}

/// Inactivity ceiling between two backend events. `ROZUM_GEN_TIMEOUT_SECS`
/// (default 300; `0` disables). Must exceed the worst legitimate gap — a cold
/// hybrid/MoE first token (Metal kernel JIT + weight page-in) ran ~33s, and a big
/// quantized model under memory pressure can stall longer, so keep headroom.
pub(crate) fn gen_inactivity_timeout() -> Duration {
    std::env::var("ROZUM_GEN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300))
}

/// Wrap a backend stream so a stalled generation can't hang the client forever.
/// If no event arrives within `gen_inactivity_timeout()`, cancel the job and end
/// the stream with `ModelError::Timeout` (HTTP 504). This is the backstop the
/// per-token cancel check can't provide: a Metal eval wedged under memory
/// pressure blocks inside one FFI call, so the decode loop's `is_cancelled()`
/// check never runs until it returns. Cancelling here lets the worker abandon the
/// job the moment it unblocks; the client gets an error instead of hanging.
pub(crate) fn with_gen_timeout(mut stream: ChatStream, cancel: CancellationToken, dur: Duration) -> ChatStream {
    if dur.is_zero() {
        return stream;
    }
    Box::pin(async_stream::stream! {
        loop {
            match tokio::time::timeout(dur, stream.next()).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    cancel.cancel();
                    crate::obs::log_event(json!({
                        "event": "generation_timeout", "inactivity_secs": dur.as_secs(),
                    }));
                    yield Err(ModelError::Timeout(format!(
                        "no output for {}s; generation aborted",
                        dur.as_secs()
                    )));
                    break;
                }
            }
        }
    })
}

/// `backend.chat`, but first break a detected agentic stuck-loop with a synthetic stop.
pub(crate) async fn chat_or_loopbreak(
    backend: &Arc<dyn ChatBackend>,
    req: ChatRequest,
) -> ModelResult<ChatStream> {
    if let Some(reason) = detect_stuck_loop(&req.messages) {
        crate::obs::log_event(json!({ "event": "stuck_loop_broken", "detail": reason }));
        return Ok(synthetic_stop_stream(reason));
    }
    backend.chat(req).await
}

/// Parse the Anthropic `tool_choice` object (`{"type":"auto"|"any"|"none"|"tool","name":…}`).
pub(crate) fn parse_anthropic_tool_choice(v: &Value) -> ToolChoice {
    match v.get("type").and_then(Value::as_str) {
        Some("none") => ToolChoice::None,
        Some("any") => ToolChoice::Required,
        Some("tool") => match v.get("name").and_then(Value::as_str) {
            Some(n) => ToolChoice::Named(n.to_string()),
            None => ToolChoice::Auto,
        },
        _ => ToolChoice::Auto,
    }
}

/// Parse OpenAI `response_format` into the JSON Schema to constrain the response to (or
/// `None` for free text). `{"type":"json_object"}` → any JSON object; `{"type":"json_schema",
/// "json_schema":{"schema":{…}}}` → that schema; `{"type":"text"}` / absent → `None`.
pub(crate) fn parse_response_format(v: &Value) -> Option<Value> {
    parse_format_object(v)
}

/// The schema out of a FORMAT OBJECT, whichever OpenAI dialect it arrived in.
///
/// The two spell the type names identically and differ only in where the schema sits: Chat wraps it
/// one level deeper (`json_schema.schema`) than Responses (`schema`). That is a nesting difference,
/// not a semantic one, so it is handled once here instead of becoming a second copy of the same
/// rule in the Responses dialect (BUG-034).
///
/// Anything else — including Responses' explicit `{"type": "text"}` — is `None`, i.e. UNCONSTRAINED.
/// That default matters more than the feature: a client saying "plain text please" must not be
/// silently forced to emit JSON.
pub(crate) fn parse_format_object(fmt: &Value) -> Option<Value> {
    match fmt.get("type").and_then(Value::as_str) {
        Some("json_object") => Some(json!({ "type": "object" })),
        Some("json_schema") => fmt
            .get("json_schema")
            .and_then(|js| js.get("schema")) // OpenAI Chat: response_format.json_schema.schema
            .or_else(|| fmt.get("schema")) // Responses: text.format.schema
            .cloned()
            .or_else(|| Some(json!({ "type": "object" }))),
        _ => None,
    }
}

/// The Responses API's structured-output request: `text: { format: { … } }`.
///
/// Until BUG-034 this was not read at all, so the same capability worked on
/// `/v1/chat/completions` and silently did nothing on `/v1/responses` — a constrained-decode
/// request answered with unconstrained output and a 200.
pub(crate) fn parse_text_format(text: &Value) -> Option<Value> {
    text.get("format").and_then(parse_format_object)
}

/// Report the request fields this gateway did not act on — once per novel shape.
///
/// **Why this exists.** Seven defects were closed on 2026-08-14 (BUG-031 … BUG-037) and every one
/// had the same mechanism: `serde` drops a field no struct declares, in silence. The client gets a
/// 200, the parameter does nothing, and nothing anywhere says so — not an error, not a log line.
/// Each was found by eye, one at a time. This makes the class visible instead: whatever a client
/// sends and this gateway ignores is named, with the endpoint, in `gateway.jsonl`.
///
/// **Once per shape, not once per request.** An agent sends the same request shape thousands of
/// times an hour; logging each would bury the signal it exists to raise. The key is the sorted
/// field list, so a NEW unhandled parameter — a client upgrading, a spec growing — is one new line.
///
/// It deliberately does not warn, refuse, or 400. Ignoring an unknown field is correct HTTP
/// behaviour and some clients send bookkeeping fields on purpose (`store`, `metadata`, `user`); the
/// defect was never the ignoring, it was the silence.
pub(crate) fn log_unhandled_fields(endpoint: &'static str, unknown: &serde_json::Map<String, Value>) {
    if unknown.is_empty() {
        return;
    }
    let mut names: Vec<&str> = unknown.keys().map(String::as_str).collect();
    names.sort_unstable();
    let shape = format!("{endpoint} {}", names.join(","));

    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);
    let novel = {
        let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
        let seen = guard.get_or_insert_with(std::collections::HashSet::new);
        is_novel_shape(seen, shape, SHAPE_MEMORY)
    };
    if !novel {
        return;
    }

    crate::obs::log_event(json!({
        "event": "wire_fields_ignored",
        "endpoint": endpoint,
        "fields": names,
    }));
}

/// How many distinct ignored-field shapes are remembered before this stops recording new ones.
///
/// A bound, because the memory is keyed by what a CLIENT sends: a caller with random field names
/// would otherwise grow it forever. Real clients produce a handful of shapes.
pub(crate) const SHAPE_MEMORY: usize = 256;

/// Should this shape be reported? True exactly once per shape, and never once the memory is full.
///
/// Split out from [`log_unhandled_fields`] so the decision is testable without a log file: the
/// "once" is the whole point of the feature, and a dedupe that quietly reports every time would
/// bury the signal it exists to raise.
pub(crate) fn is_novel_shape(
    seen: &mut std::collections::HashSet<String>,
    shape: String,
    cap: usize,
) -> bool {
    if seen.contains(&shape) {
        return false;
    }
    if seen.len() >= cap {
        return false;
    }
    seen.insert(shape);
    true
}

/// How many stop strings one request may carry.
///
/// A bound, not a preference: every generated token is scanned against every stop string, so an
/// unbounded list is an unbounded per-token cost on a local server anyone on the tailnet can reach.
/// OpenAI's own limit is 4 and Anthropic's is small, so no compliant client comes near this —
/// which is why capping is safe here and dropping silently would not have been.
pub(crate) const MAX_STOP_STRINGS: usize = 16;

/// The client's stop strings, from either spelling: OpenAI's `stop` (a bare string OR an array),
/// Anthropic's `stop_sequences` (an array). Empty strings are dropped — one would match at every
/// position and end the turn before the first token (BUG-037).
pub(crate) fn parse_stop(v: &Value) -> Vec<String> {
    let mut out = Vec::new();
    match v {
        Value::String(s) => {
            if !s.is_empty() {
                out.push(s.clone());
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if let Some(s) = item.as_str() {
                    if !s.is_empty() {
                        out.push(s.to_string());
                    }
                }
            }
        }
        _ => {}
    }
    out.truncate(MAX_STOP_STRINGS);
    out
}

/// Apply a [`ToolChoice`] to the resolved tool set: `None` → empty, `Named` → only that tool
/// (empty if the client named a tool it didn't define), `Auto`/`Required` → unchanged.
pub(crate) fn apply_tool_choice(tools: Vec<ToolDef>, choice: &ToolChoice) -> Vec<ToolDef> {
    match choice {
        ToolChoice::Auto | ToolChoice::Required => tools,
        ToolChoice::None => Vec::new(),
        ToolChoice::Named(name) => tools.into_iter().filter(|t| &t.name == name).collect(),
    }
}

/// Reproducibility instrument for the agentic matrix (and any caller wanting a
/// deterministic local model). The gateway passes the client's sampling params through
/// verbatim and leaves `seed` unset, so the sampler + MLX RNG seed from entropy: a
/// `temperature > 0` request (Claude Code's main loop sends 1.0) produces a DIFFERENT
/// token stream every run → a matrix cell flips pass↔fail on a byte-identical config,
/// which undermines every other matrix reading. These env knobs pin a run WITHOUT
/// changing the wire protocol. Both default OFF → behaviour is byte-for-byte unchanged
/// unless explicitly set (so it is purely a benchmark/diagnosis instrument here):
///   `ROZUM_SAMPLING_SEED=<u64>`   pin the RNG seed (only fills it when the client sent none)
///   `ROZUM_FORCE_GREEDY=1|true|on` force temperature 0 (argmax — removes the RNG entirely)
pub(crate) fn apply_determinism_env(s: SamplingParams) -> SamplingParams {
    let force_greedy = matches!(
        std::env::var("ROZUM_FORCE_GREEDY").ok().as_deref(),
        Some("1" | "true" | "on")
    );
    let seed = std::env::var("ROZUM_SAMPLING_SEED")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());
    apply_determinism(s, force_greedy, seed)
}

/// Pure core of [`apply_determinism_env`] (env read split out so it is race-free to test).
/// `force_greedy` wins over the client's temperature; `seed` only fills an unset seed so a
/// caller that genuinely sent its own seed keeps it.
pub(crate) fn apply_determinism(mut s: SamplingParams, force_greedy: bool, seed: Option<u64>) -> SamplingParams {
    if force_greedy {
        s.temperature = Some(0.0);
        s.top_p = None;
        s.top_k = None;
    }
    if s.seed.is_none() {
        if let Some(sd) = seed {
            s.seed = Some(sd);
        }
    }
    s
}

pub(crate) fn poison_ttl_secs() -> u64 {
    std::env::var("ROZUM_POISON_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(crate::share::POISON_TTL_SECS)
}

/// Daemon-side defense-in-depth: a freshly (re)spawned daemon loads the shared
/// poison set and refuses a confirmed crasher *before running the model*, so a
/// poison prompt that survived the crash it caused can't immediately kill the
/// daemon again — even reaching it directly (no proxy). Only POST bodies are
/// fingerprinted (raw bytes, matching what the proxy hashes); the body is
/// re-attached for the downstream handler. Fail-open on any read hiccup.
pub(crate) async fn poison_layer(req: axum::extract::Request, next: Next) -> Response {
    // Only chat POSTs carry prompts worth fingerprinting; control-plane POSTs
    // (switch/unload/reload) pass through untouched.
    if req.method() != axum::http::Method::POST || req.uri().path().starts_with("/control/") {
        return next.run(req).await;
    }
    let (parts, body) = req.into_parts();
    let bytes = match axum::body::to_bytes(body, 64 * 1024 * 1024).await {
        Ok(b) => b,
        Err(_) => {
            // Couldn't buffer — fail open; the handler reports the body error.
            let req = axum::extract::Request::from_parts(parts, axum::body::Body::empty());
            return next.run(req).await;
        }
    };
    let fp = crate::share::fingerprint(&bytes);
    if crate::share::is_poisoned(fp, poison_ttl_secs()) {
        crate::obs::log_event(json!({
            "event": "poison_refused", "fingerprint": format!("{fp:016x}"),
        }));
        return error_json(
            StatusCode::UNPROCESSABLE_ENTITY,
            "request previously crashed this model; refused for now — retry later (advisory, expires)",
            "poison_refused",
        );
    }
    let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
    next.run(req).await
}


#[cfg(test)]
mod format_tests {
    use super::*;

    #[test]
    fn an_ignored_field_shape_is_reported_once_and_the_memory_is_bounded() {
        // "Once" is the feature: an agent sends the same shape thousands of times an hour, and a
        // line per request would bury the one thing this exists to surface — a NEW parameter a
        // client started sending (BUG-038).
        let mut seen = std::collections::HashSet::new();
        assert!(is_novel_shape(&mut seen, "/v1/messages metadata".into(), SHAPE_MEMORY));
        assert!(!is_novel_shape(&mut seen, "/v1/messages metadata".into(), SHAPE_MEMORY));
        // A different shape on the same endpoint is news again.
        assert!(is_novel_shape(&mut seen, "/v1/messages metadata,thinking".into(), SHAPE_MEMORY));
        // …and so is the same shape on a different endpoint: which dialect ignored it matters.
        assert!(is_novel_shape(&mut seen, "/v1/responses metadata".into(), SHAPE_MEMORY));

        // Bounded, because the key comes from what a CLIENT sends: random field names must not
        // grow this without limit.
        let mut small = std::collections::HashSet::new();
        assert!(is_novel_shape(&mut small, "a".into(), 2));
        assert!(is_novel_shape(&mut small, "b".into(), 2));
        assert!(!is_novel_shape(&mut small, "c".into(), 2), "full memory stops recording");
        assert!(!is_novel_shape(&mut small, "a".into(), 2), "and still says no to a known one");
    }

    #[test]
    fn both_openai_dialects_yield_the_same_schema_from_their_own_nesting() {
        // BUG-034: the capability existed on `/v1/chat/completions` and did nothing on
        // `/v1/responses`, because only the nesting differs and only one nesting was read.
        let schema = json!({"type": "object", "properties": {"answer": {"type": "string"}}});

        // Chat: response_format.json_schema.schema
        let chat = json!({"type": "json_schema", "json_schema": {"name": "a", "schema": schema}});
        assert_eq!(parse_response_format(&chat), Some(schema.clone()));

        // Responses: text.format.schema
        let responses = json!({"format": {"type": "json_schema", "name": "a", "schema": schema, "strict": true}});
        assert_eq!(parse_text_format(&responses), Some(schema.clone()));
    }

    #[test]
    fn plain_text_and_absence_both_mean_unconstrained() {
        // The dangerous direction. `{"type":"text"}` is what a Responses client sends when it wants
        // prose; reading it as "constrain to JSON" would break every ordinary request on the
        // endpoint codex drives. Absence must be just as inert.
        assert_eq!(parse_text_format(&json!({"format": {"type": "text"}})), None);
        assert_eq!(parse_text_format(&json!({})), None);
        assert_eq!(parse_text_format(&Value::Null), None);
        assert_eq!(parse_response_format(&Value::Null), None);
        assert_eq!(parse_text_format(&json!({"format": {"type": "who_knows"}})), None);
    }

    #[test]
    fn json_object_asks_for_an_object_and_a_schemaless_json_schema_falls_back_to_one() {
        let obj = json!({"type": "object"});
        assert_eq!(parse_text_format(&json!({"format": {"type": "json_object"}})), Some(obj.clone()));
        assert_eq!(parse_response_format(&json!({"type": "json_object"})), Some(obj.clone()));
        // `json_schema` with no schema in it: constrain to "some object" rather than refuse — the
        // pre-existing Chat behaviour, now shared rather than re-decided.
        assert_eq!(parse_text_format(&json!({"format": {"type": "json_schema"}})), Some(obj));
    }
}
