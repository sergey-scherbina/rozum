//! The bytes each wire dialect puts on the socket, frozen.
//!
//! `plugin-wireprotocol` moves the orchestration spine out of three near-identical handlers, on the
//! path every agent and the whole matrix runs through. The refactor's only acceptable outcome is
//! that nothing changes — so this records what "nothing" is, on the code BEFORE the move, and the
//! move is judged by whether `src/testdata/wire-golden.txt` is byte-identical afterwards.
//!
//! Two halves per case, because a refactor can break either:
//! - **REQUEST**: what actually reached the backend — roles and text, tool names, every sampling
//!   knob. This is where a mis-moved field (a dropped `response_schema`, a `max_tokens` read from
//!   the wrong key) would show up, and it is invisible in the response.
//! - **RESPONSE**: the exact body, SSE frames included.
//!
//! Regenerate deliberately: `ROZUM_WIRE_GOLDEN_UPDATE=1 cargo test -p rozum-gateway --lib wire_golden`.
//! A diff in that file during a behaviour-preserving change is the change failing, not the file
//! being stale.

use super::*;
use crate::backend::{ChatStream, ContentBlock, StopReason};
use std::sync::Mutex as StdMutex;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata/wire-golden.txt");

/// A backend that answers a fixed two-event stream and keeps what it was asked.
///
/// Deliberately NOT `HelloBackend`: the point is the request, and `HelloBackend` throws it away.
struct RecordingBackend {
    seen: Arc<StdMutex<Option<String>>>,
}

#[async_trait::async_trait]
impl ChatBackend for RecordingBackend {
    async fn chat(&self, req: ChatRequest) -> rozum_core::backend::ModelResult<ChatStream> {
        *self.seen.lock().unwrap() = Some(summarize(&req));
        Ok(Box::pin(async_stream::stream! {
            yield Ok(ChatEvent::TextDelta { text: "ok".into() });
            yield Ok(ChatEvent::Done {
                stop_reason: StopReason::EndTurn,
                input_tokens: 7,
                output_tokens: 2,
            });
        }))
    }

    fn context_window(&self) -> u32 {
        4096
    }
}

/// Everything about the internal request a dialect is responsible for producing.
fn summarize(req: &ChatRequest) -> String {
    let mut out = String::new();
    for m in &req.messages {
        let role = format!("{:?}", m.role).to_lowercase();
        let mut text = String::new();
        for b in &m.content {
            match b {
                ContentBlock::Text { text: t } => text.push_str(t),
                ContentBlock::ToolResult { content, .. } => {
                    text.push_str(&format!("[tool_result {content}]"))
                }
                ContentBlock::ToolUse { name, input, .. } => {
                    text.push_str(&format!("[tool_use {name} {input}]"))
                }
                ContentBlock::Image { .. } => text.push_str("[image]"),
            }
        }
        out.push_str(&format!("  msg {role}: {text}\n"));
    }
    let names: Vec<&str> = req.tools.iter().map(|t| t.name.as_str()).collect();
    out.push_str(&format!("  tools: [{}]\n", names.join(", ")));
    let s = &req.sampling;
    // `seed` is here because it was NOT, and that omission mirrored the one in the code: this gate
    // printed every sampling knob except the single field no dialect was filling (BUG-032). A
    // golden can only catch what it prints.
    out.push_str(&format!(
        "  sampling: temperature={:?} top_p={:?} top_k={:?} max_tokens={:?} seed={:?} repeat={:?} freq={:?} presence={:?} stop={:?} schema={} reasoning={:?}\n",
        s.temperature,
        s.top_p,
        s.top_k,
        s.max_tokens,
        s.seed,
        s.repeat_penalty,
        s.frequency_penalty,
        s.presence_penalty,
        s.stop,
        s.response_schema.is_some(),
        s.reasoning_effort,
    ));
    out
}

fn test_state(seen: Arc<StdMutex<Option<String>>>) -> GatewayState {
    let backend = Arc::new(RecordingBackend { seen }) as Arc<dyn ChatBackend>;
    let sb = Arc::new(Switchboard {
        backend: std::sync::RwLock::new(Some(backend)),
        builder: None,
        spec: std::sync::Mutex::new(ModelSpec {
            model_id: "golden-model".into(),
            n_ctx: 4096,
            backend: None,
        }),
        generation: AtomicU64::new(1),
        started_at: 0,
        draining: AtomicBool::new(false),
        resume: tokio::sync::Notify::new(),
        generating: AtomicU64::new(0),
        reload_lock: tokio::sync::Mutex::new(()),
        register: None,
        shutting_down: AtomicBool::new(false),
        warm: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        usage: crate::resident::UsageStats::in_memory(),
        warm_cfg: WarmConfig::default(),
    });
    GatewayState {
        sb,
        auth_token: None,
        observer: crate::obs::Observer::new(),
        activity: Arc::new(Activity::default()),
    }
}

/// Replace the fields that are *supposed* to differ per run — ids and clocks — so the rest is a
/// real byte comparison rather than a diff that is always red.
fn normalize(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let bytes: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        // `"created":1755168000` / `"created_at":1755168000` → …:0
        if body[i..].starts_with("\"created") {
            let rest = &body[i..];
            if let Some(colon) = rest.find(':') {
                let after = &rest[colon + 1..];
                let digits = after.chars().take_while(|c| c.is_ascii_digit()).count();
                if digits > 0 {
                    out.push_str(&rest[..colon + 1]);
                    out.push('0');
                    i += colon + 1 + digits;
                    continue;
                }
            }
        }
        // An id with a random tail: `chatcmpl-8f3c…`, `msg_01AbC…`, `resp_…`.
        // Both spellings, because the three dialects disagree: `resp-<uuid>` / `msg-<uuid>` here,
        // `msg_…` in Anthropic's own docs. A prefix missed here makes this test flap, not fail.
        // Only prefixes that introduce an ID VALUE. `item_`/`call_` are deliberately absent: they
        // are also the start of the KEYS `item_id` and `call_id`, and rewriting those would print
        // a wire format this gateway does not speak — a golden nobody can read against the API docs.
        for prefix in ["chatcmpl-", "msg_", "msg-", "resp_", "resp-", "fc_", "fc-", "rs_", "rs-"] {
            if body[i..].starts_with(prefix) {
                let tail = &body[i + prefix.len()..];
                let n = tail
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                    .count();
                if n > 0 {
                    out.push_str(prefix);
                    out.push('X');
                    i += prefix.len() + n;
                }
            }
        }
        if i >= bytes.len() {
            break;
        }
        let c = body[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

async fn body_text(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body");
    normalize(&String::from_utf8_lossy(&bytes))
}

/// Every case: one realistic request per dialect, streaming and not.
async fn render_all() -> String {
    let mut out = String::new();

    for stream in [false, true] {
        // ── OpenAI Chat ──────────────────────────────────────────────────────
        let seen = Arc::new(StdMutex::new(None));
        let state = test_state(seen.clone());
        let req: crate::oai_api::OaiChatReq = serde_json::from_value(json!({
            "model": "asked-for-model",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "hi"}
            ],
            "tools": [{"type": "function", "function": {
                "name": "read_file",
                "description": "read it",
                "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
            }}],
            "response_format": {"type": "json_object"},
            // `seed` for the same reason `top_p`/`top_k` are on the Anthropic case: the golden's
            // job is to show what a client asking for a knob actually gets.
            "temperature": 0.3, "top_p": 0.9, "top_k": 40, "max_tokens": 64, "seed": 4242,
            "repetition_penalty": 1.1,
            "frequency_penalty": 0.7, "presence_penalty": -0.3,
            // A bare string, which is one of the two shapes OpenAI accepts.
            "stop": "\nHuman:",
            "stream": stream,
        }))
        .unwrap();
        let resp = oai_chat_handler(axum::extract::State(state), axum::http::HeaderMap::new(), axum::Json(req)).await;
        out.push_str(&format!(
            "=== /v1/chat/completions stream={stream}\n--- request\n{}--- response\n{}\n",
            seen.lock().unwrap().clone().unwrap_or_default(),
            body_text(resp).await
        ));

        // ── OpenAI Responses ─────────────────────────────────────────────────
        let seen = Arc::new(StdMutex::new(None));
        let state = test_state(seen.clone());
        let req: crate::responses_api::RespReq = serde_json::from_value(json!({
            "model": "asked-for-model",
            "instructions": "be brief",
            "input": [{"type": "message", "role": "user",
                       "content": [{"type": "input_text", "text": "hi"}]}],
            "tools": [{"type": "function", "name": "read_file", "description": "read it",
                       "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}],
            "temperature": 0.3, "top_p": 0.9, "top_k": 40, "max_output_tokens": 64,
            "reasoning": {"effort": "low"},
            // Structured output, the Responses spelling. Before BUG-034 this line changed nothing
            // and the golden read `schema=false` while the Chat case beside it read `true`.
            "text": {"format": {"type": "json_schema", "name": "answer",
                                "schema": {"type": "object"}, "strict": true}},
            "stream": stream,
        }))
        .unwrap();
        let resp = responses_handler(axum::extract::State(state), axum::http::HeaderMap::new(), axum::Json(req)).await;
        out.push_str(&format!(
            "=== /v1/responses stream={stream}\n--- request\n{}--- response\n{}\n",
            seen.lock().unwrap().clone().unwrap_or_default(),
            body_text(resp).await
        ));

        // ── Anthropic Messages ───────────────────────────────────────────────
        let seen = Arc::new(StdMutex::new(None));
        let state = test_state(seen.clone());
        let req: crate::anthropic_api::AnthropicReq = serde_json::from_value(json!({
            "model": "asked-for-model",
            "system": "be brief",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"name": "read_file", "description": "read it",
                       "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}],
            // `top_p`/`top_k` are here because the golden's job is to show what a client asking
            // for them actually gets: before BUG-031 this line read `top_p=None top_k=None`.
            "temperature": 0.3, "top_p": 0.9, "top_k": 40, "max_tokens": 64,
            // Anthropic's spelling, always an array.
            "stop_sequences": ["\nHuman:", "END"],
            "stream": stream,
        }))
        .unwrap();
        let resp = anthropic_handler(axum::extract::State(state), axum::http::HeaderMap::new(), axum::Json(req)).await;
        out.push_str(&format!(
            "=== /v1/messages stream={stream}\n--- request\n{}--- response\n{}\n",
            seen.lock().unwrap().clone().unwrap_or_default(),
            body_text(resp).await
        ));
    }

    // ── The seventh case: OpenAI Chat streaming WITH `stream_options.include_usage` ──────────
    //
    // Its own case rather than a variant of the loop, because the whole point is the CONTRAST with
    // the plain streaming case above: same request otherwise, and the difference is a `usage` key
    // on every chunk plus one extra chunk at the end. A client that does not ask must keep getting
    // the frames it always got, and the two blocks sitting side by side is how that stays true
    // (BUG-033).
    let seen = Arc::new(StdMutex::new(None));
    let state = test_state(seen.clone());
    let req: crate::oai_api::OaiChatReq = serde_json::from_value(json!({
        "model": "asked-for-model",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
        "stream_options": {"include_usage": true},
    }))
    .unwrap();
    let resp = oai_chat_handler(axum::extract::State(state), axum::http::HeaderMap::new(), axum::Json(req)).await;
    out.push_str(&format!(
        "=== /v1/chat/completions stream=true include_usage=true\n--- request\n{}--- response\n{}\n",
        seen.lock().unwrap().clone().unwrap_or_default(),
        body_text(resp).await
    ));

    // ── The eighth case: a request that PINS ITS OWN DECODE ─────────────────────────────────
    //
    // Same Anthropic body as the case above, plus the two headers a `rozum launch` stamps when it
    // was started with `ROZUM_FORCE_GREEDY=1` / `ROZUM_SAMPLING_SEED=…`. Here rather than in a unit
    // test because the claim being made is end-to-end: what reaches the BACKEND is argmax with a
    // pinned seed, not merely a parsed header. The dialect with no `seed` field of its own is the
    // sharpest case — it is why the policy is a header and not a body key.
    //
    // The contrast with the `/v1/messages` block above is the evidence: identical request, and the
    // only lines that differ are the sampling ones.
    let seen = Arc::new(StdMutex::new(None));
    let state = test_state(seen.clone());
    let req: crate::anthropic_api::AnthropicReq = serde_json::from_value(json!({
        "model": "asked-for-model",
        "system": "be brief",
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.3, "top_p": 0.9, "top_k": 40, "max_tokens": 64,
    }))
    .unwrap();
    let mut pinned = axum::http::HeaderMap::new();
    pinned.insert("x-rozum-decode", "greedy".parse().unwrap());
    pinned.insert("x-rozum-seed", "1234".parse().unwrap());
    let resp = anthropic_handler(axum::extract::State(state), pinned, axum::Json(req)).await;
    out.push_str(&format!(
        "=== /v1/messages stream=false PINNED (x-rozum-decode: greedy, x-rozum-seed: 1234)\n--- request\n{}--- response\n{}\n",
        seen.lock().unwrap().clone().unwrap_or_default(),
        body_text(resp).await
    ));

    out
}

#[tokio::test]
async fn the_three_dialects_put_the_same_bytes_on_the_socket() {
    // Determinism: these knobs rewrite sampling globally (matrix greedy), and a developer with
    // them exported would otherwise "fail" this test with a correct binary.
    let rendered = render_all().await;
    if std::env::var_os("ROZUM_WIRE_GOLDEN_UPDATE").is_some() {
        std::fs::create_dir_all(std::path::Path::new(GOLDEN).parent().unwrap()).unwrap();
        std::fs::write(GOLDEN, &rendered).unwrap();
        eprintln!("wire golden UPDATED: {GOLDEN}");
        return;
    }
    let expected = std::fs::read_to_string(GOLDEN).unwrap_or_else(|e| {
        panic!("{GOLDEN}: {e} — regenerate with ROZUM_WIRE_GOLDEN_UPDATE=1")
    });
    if rendered != expected {
        // Show the first differing line rather than two 200-line blobs.
        let (mut a, mut b) = (rendered.lines(), expected.lines());
        let mut n = 0;
        loop {
            n += 1;
            match (a.next(), b.next()) {
                (Some(x), Some(y)) if x == y => continue,
                (x, y) => panic!(
                    "wire bytes changed at line {n}\n  now:      {:?}\n  expected: {:?}",
                    x.unwrap_or("<eof>"),
                    y.unwrap_or("<eof>")
                ),
            }
        }
    }
}
