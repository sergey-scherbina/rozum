/// OpenAI-compatible HTTP backend.
///
/// Connects to any server that speaks `POST /v1/chat/completions` with SSE streaming
/// (Ollama, llama.cpp server, mlx_lm.server, vLLM, OpenAI, …).
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
    ModelResult, Role, StopReason,
};

pub struct OpenAiHttpBackend {
    /// Base URL including `/v1` if needed (e.g. `http://localhost:11434/v1`).
    endpoint: String,
    model: String,
    /// Optional bearer token for authenticated remotes (OpenAI, OpenRouter, …). Local
    /// servers (Ollama, llama.cpp) need none.
    api_key: Option<String>,
    client: reqwest::Client,
    /// Whether this endpoint accepts `stream_options` — see [`OpenAiHttpBackend::chat`].
    ///
    /// Per-instance and not global: one process talks to several endpoints and they do not agree.
    /// Starts optimistic, and only the FIRST request against a server that refuses pays for finding
    /// out.
    stream_options_ok: std::sync::atomic::AtomicBool,
}

impl OpenAiHttpBackend {
    pub fn new(endpoint: impl Into<String>, model: impl Into<String>) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300)) // long generation
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            endpoint: endpoint.into(),
            model: model.into(),
            api_key: None,
            client,
            stream_options_ok: std::sync::atomic::AtomicBool::new(stream_usage_wanted()),
        }
    }

    /// Attach an `Authorization: Bearer <key>` to every request (builder style).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        let k = key.into();
        self.api_key = (!k.is_empty()).then_some(k);
        self
    }

    /// The request body for one attempt. Pure — no I/O — so a test can read what goes on the wire.
    pub(crate) fn build_body(&self, req: &ChatRequest, include_usage: bool) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages_to_oai(&req.messages),
            "stream": true,
        });
        if include_usage {
            // The shape is OpenAI's: the object carries one flag, and the server answers with an
            // extra final chunk whose `choices` is empty and whose `usage` is the real count.
            body["stream_options"] = json!({ "include_usage": true });
        }
        if let Some(t) = req.sampling.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = req.sampling.top_p {
            body["top_p"] = json!(p);
        }
        if let Some(n) = req.sampling.max_tokens {
            body["max_tokens"] = json!(n);
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema
                        }
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }
        body
    }

    /// One POST. Separates "the server refused this parameter" from every other failure, so the
    /// caller can retry the first and must not retry the second.
    async fn post_chat(&self, req: &ChatRequest, include_usage: bool) -> ModelResult<Attempt> {
        let body = self.build_body(req, include_usage);
        let url = format!("{}/chat/completions", self.endpoint);
        let mut request = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ModelError::BackendUnavailable(format!("http request: {e}")))?;

        if response.status().is_success() {
            return Ok(Attempt::Answered(response));
        }
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if include_usage && refused_stream_options(status, &text) {
            return Ok(Attempt::RefusedStreamOptions);
        }
        Err(ModelError::BackendUnavailable(format!(
            "server returned {status}: {text}"
        )))
    }

    /// Return `true` if the server answers to `GET /v1/models` within 3 s.
    pub async fn probe(&self) -> bool {
        self.client
            .get(format!("{}/models", self.endpoint))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }

    /// Probe an arbitrary path (e.g. Ollama's `/api/version` which is cheaper
    /// than `/v1/models` and present in every Ollama version).
    pub async fn probe_path(&self, path: &str) -> bool {
        // The endpoint here is the *base* (e.g. http://localhost:11434), not /v1.
        self.client
            .get(format!("{}{path}", self.endpoint.trim_end_matches("/v1")))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .map(|r| r.status().is_success())
            .unwrap_or(false)
    }
}

// ─── Conversion helpers ───────────────────────────────────────────────────────

fn messages_to_oai(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .filter_map(|m| {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            // Collect text content
            let mut text_parts: Vec<&str> = Vec::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            let mut tool_result_id: Option<&str> = None;
            let mut tool_result_content = String::new();

            for block in &m.content {
                match block {
                    ContentBlock::Text { text } => text_parts.push(text.as_str()),
                    ContentBlock::ToolUse { id, name, input } => {
                        tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": input.to_string()
                            }
                        }));
                    }
                    ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => {
                        tool_result_id = Some(tool_use_id.as_str());
                        tool_result_content = content.clone();
                    }
                    // Not forwarded to upstream OpenAI-compatible endpoints.
                    ContentBlock::Image { .. } => {}
                }
            }

            if let Some(id) = tool_result_id {
                return Some(json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": tool_result_content
                }));
            }

            let content_str = text_parts.join("");
            let mut obj = json!({
                "role": role,
                "content": if content_str.is_empty() && !tool_calls.is_empty() {
                    Value::Null
                } else {
                    Value::String(content_str)
                }
            });

            if !tool_calls.is_empty() {
                obj["tool_calls"] = Value::Array(tool_calls);
            }

            Some(obj)
        })
        .collect()
}

// ─── SSE parser ───────────────────────────────────────────────────────────────

struct OaiToolInFlight {
    #[allow(dead_code)]
    index: usize,
    id: String,
    name: String,
    args: String,
}

/// Parse a single `data: {...}` line from OAI SSE into zero or more ChatEvents.
/// Returns `None` if the line is not an SSE data line.
/// Returns an empty vec for `data: [DONE]`.
fn parse_sse_data_line(
    line: &str,
    tools_in_flight: &mut Vec<OaiToolInFlight>,
) -> Option<Vec<ChatEvent>> {
    let data = line.strip_prefix("data: ")?;
    if data.trim() == "[DONE]" {
        return Some(vec![]);
    }
    let v: Value = serde_json::from_str(data).ok()?;
    let delta = &v["choices"][0]["delta"];
    let finish = v["choices"][0]["finish_reason"].as_str();

    let mut events = Vec::new();

    // Text delta
    if let Some(text) = delta["content"].as_str() {
        if !text.is_empty() {
            events.push(ChatEvent::TextDelta {
                text: text.to_owned(),
            });
        }
    }

    // Tool call deltas
    if let Some(tcs) = delta["tool_calls"].as_array() {
        for tc in tcs {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            // Ensure slot exists
            while tools_in_flight.len() <= idx {
                tools_in_flight.push(OaiToolInFlight {
                    index: tools_in_flight.len(),
                    id: String::new(),
                    name: String::new(),
                    args: String::new(),
                });
            }
            let slot = &mut tools_in_flight[idx];

            // id and name arrive only in the first chunk for this tool call
            if let Some(id) = tc["id"].as_str() {
                if !id.is_empty() && slot.id.is_empty() {
                    slot.id = id.to_owned();
                }
            }
            if let Some(name) = tc["function"]["name"].as_str() {
                if !name.is_empty() && slot.name.is_empty() {
                    slot.name = name.to_owned();
                    events.push(ChatEvent::ToolUseStart {
                        id: slot.id.clone(),
                        name: slot.name.clone(),
                    });
                }
            }
            // Argument fragment
            if let Some(frag) = tc["function"]["arguments"].as_str() {
                if !frag.is_empty() {
                    slot.args.push_str(frag);
                    events.push(ChatEvent::ToolUseDelta {
                        id: slot.id.clone(),
                        input_json_delta: frag.to_owned(),
                    });
                }
            }
        }
    }

    // Finish
    if let Some(reason) = finish {
        // Close any open tool calls
        for slot in tools_in_flight.drain(..) {
            events.push(ChatEvent::ToolUseEnd { id: slot.id });
        }
        let stop_reason = match reason {
            "tool_calls" => StopReason::ToolUse,
            "length" => StopReason::MaxTokens,
            _ => StopReason::EndTurn,
        };
        // usage if present
        let in_tok = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let out_tok = v["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;
        events.push(ChatEvent::Done {
            input_tokens: in_tok,
            output_tokens: out_tok,
            stop_reason,
        });
    }

    Some(events)
}

/// Whether to ask an upstream for streamed token usage at all.
///
/// `ROZUM_OPENAI_STREAM_USAGE=0` turns it off for every endpoint in the process. It exists because
/// this is the one place rozum changes what it SENDS to a third-party server, and an operator who
/// hits a refusal shape this code does not recognise needs a way to stop it that does not require a
/// new build.
fn stream_usage_wanted() -> bool {
    !matches!(
        std::env::var("ROZUM_OPENAI_STREAM_USAGE").ok().as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no")
    )
}

/// Did the server refuse specifically because of `stream_options`?
///
/// NAME-BASED ON PURPOSE, and the alternative is worse. Treating any 4xx as "must be the new
/// parameter" would retry a request the server rejected for a real reason — a bad model name, a
/// context overflow — and then report the SECOND failure, hiding the first. A server that refuses an
/// unknown field says which one; one that does not gets the operator's escape hatch instead of a
/// guess.
///
/// `include_usage` is checked too: some servers name the inner field rather than the object.
fn refused_stream_options(status: reqwest::StatusCode, body: &str) -> bool {
    if !status.is_client_error() {
        return false;
    }
    let b = body.to_ascii_lowercase();
    b.contains("stream_options") || b.contains("include_usage")
}

/// What one POST produced: an answer, or the one refusal worth retrying without the parameter.
enum Attempt {
    Answered(reqwest::Response),
    RefusedStreamOptions,
}

// ─── ChatBackend impl ─────────────────────────────────────────────────────────

#[async_trait]
impl ChatBackend for OpenAiHttpBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        use std::sync::atomic::Ordering;

        // ASK FOR THE TOKEN COUNTS, which this client has always parsed and never requested.
        //
        // `parse_done_finish` pins the expectation that a streamed chunk carries `usage`, and
        // OpenAI's contract is that it does NOT unless the request opted in with
        // `stream_options: {"include_usage": true}`. So against a spec-compliant upstream every
        // `Done` event reported 0 input and 0 output tokens — no error, no warning, a number that
        // looks like a measurement. This is the client half of BUG-033; the server half landed
        // separately.
        //
        // It was deferred once because it changes what we SEND to a third party: a server that
        // validates unknown parameters strictly answers 400 for the whole request rather than
        // ignoring the field, and there is no way to probe the real providers from here. The answer
        // is not a probe and not a flag the operator has to know about — it is to ask, and to fall
        // back on the ONE refusal that means "I do not know this parameter", remembering the answer
        // per endpoint so only the first request pays.
        let include_usage = self.stream_options_ok.load(Ordering::Relaxed);
        let response = match self.post_chat(&req, include_usage).await? {
            Attempt::Answered(r) => r,
            Attempt::RefusedStreamOptions => {
                // Remember, so every later request against this endpoint goes straight to the
                // supported shape instead of paying for the round trip again.
                self.stream_options_ok.store(false, Ordering::Relaxed);
                match self.post_chat(&req, false).await? {
                    Attempt::Answered(r) => r,
                    // The second attempt does not carry the parameter, so it cannot be refused for
                    // it; `post_chat` returns the error rather than this arm.
                    Attempt::RefusedStreamOptions => {
                        return Err(ModelError::BackendUnavailable(
                            "server refused stream_options on a request that did not send it".into(),
                        ));
                    }
                }
            }
        };

        let cancel = req.cancel.clone();
        let byte_stream = response.bytes_stream();

        let stream: ChatStream = Box::pin(async_stream::stream! {
            use futures::StreamExt as _;
            tokio::pin!(byte_stream);

            let mut buf = String::new();
            let mut tools_in_flight: Vec<OaiToolInFlight> = Vec::new();
            let mut done_sent = false;

            loop {
                if cancel.is_cancelled() {
                    if !done_sent {
                        yield Ok(ChatEvent::Done {
                            input_tokens: 0,
                            output_tokens: 0,
                            stop_reason: StopReason::Cancelled,
                        });
                    }
                    break;
                }

                match byte_stream.next().await {
                    None => {
                        // Stream ended without [DONE]; synthesise Done
                        if !done_sent {
                            // flush remaining tool calls
                            for slot in tools_in_flight.drain(..) {
                                yield Ok(ChatEvent::ToolUseEnd { id: slot.id });
                            }
                            yield Ok(ChatEvent::Done {
                                input_tokens: 0,
                                output_tokens: 0,
                                stop_reason: StopReason::EndTurn,
                            });
                        }
                        break;
                    }
                    Some(Err(e)) => {
                        yield Err(ModelError::BackendUnavailable(format!("stream read: {e}")));
                        break;
                    }
                    Some(Ok(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        // Process complete lines
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_owned();
                            buf = buf[nl + 1..].to_owned();

                            if line.starts_with("data: ") {
                                match parse_sse_data_line(&line, &mut tools_in_flight) {
                                    None => {}
                                    Some(events) => {
                                        for ev in events {
                                            let is_done = matches!(ev, ChatEvent::Done { .. });
                                            yield Ok(ev);
                                            if is_done {
                                                done_sent = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        });

        Ok(stream)
    }

    fn context_window(&self) -> u32 {
        // Let the server enforce its own limit; use a large sentinel.
        u32::MAX
    }

    fn label(&self) -> &'static str {
        "openai-http"
    }
}

// ─── Auto-detect helpers ──────────────────────────────────────────────────────

/// Try the Python `mlx_lm.server` at `http://localhost:8080/v1` (override with
/// `ROZUM_MLX_HTTP`). Superseded by the in-process native MLX runtime, so it is
/// **opt-in only** — never tried in the default auto-chain unless `ROZUM_MLX_HTTP`
/// is set, or forced via `--engine mlx-server`. Kept for anyone who prefers to
/// run their own `python -m mlx_lm.server` (e.g. a model the native runtime does
/// not port yet, or a remote host).
pub async fn try_mlx_server(model_spec: &str) -> Option<Arc<dyn ChatBackend>> {
    let url = std::env::var("ROZUM_MLX_HTTP")
        .unwrap_or_else(|_| "http://localhost:8080/v1".to_owned());
    let b = OpenAiHttpBackend::new(&url, model_spec);
    if b.probe().await {
        eprintln!("backend: mlx_lm.server at {url} (model: {model_spec})");
        Some(Arc::new(b))
    } else {
        None
    }
}

/// Try LM Studio's local server at the default port (`http://localhost:1234/v1`).
/// LM Studio bundles a native MLX runtime; kept as a GUI-app fallback for MLX
/// models neither in-process backend ports yet.
pub async fn try_lmstudio_http(model_spec: &str) -> Option<Arc<dyn ChatBackend>> {
    let url = std::env::var("ROZUM_LMSTUDIO_HTTP")
        .unwrap_or_else(|_| "http://localhost:1234/v1".to_owned());
    let b = OpenAiHttpBackend::new(&url, model_spec);
    if b.probe().await {
        eprintln!("backend: LM Studio at {url} (model: {model_spec})");
        Some(Arc::new(b))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a_request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message {
                role: Role::User,
                content: vec![ContentBlock::Text { text: "hi".into() }],
            }],
            tools: vec![],
            sampling: Default::default(),
            cancel: Default::default(),
            session_id: None,
        }
    }

    // ── asking an upstream for the token counts we were already parsing ──────────────────────────

    #[test]
    fn the_body_asks_for_usage_in_openais_shape() {
        let b = OpenAiHttpBackend::new("http://x/v1", "m").build_body(&a_request(), true);
        assert_eq!(b["stream_options"]["include_usage"], serde_json::json!(true));
        // …and the rest of the request is untouched by the ask.
        assert_eq!(b["stream"], serde_json::json!(true));
        assert_eq!(b["model"], serde_json::json!("m"));
    }

    #[test]
    fn without_it_the_body_is_exactly_what_it_always_was() {
        // The fallback must be byte-for-byte the OLD request, or a server that refused the ask gets
        // a second request that differs in some other way too and the retry proves nothing.
        let b = OpenAiHttpBackend::new("http://x/v1", "m").build_body(&a_request(), false);
        assert!(b.get("stream_options").is_none(), "{b}");
    }

    #[test]
    fn only_a_4xx_that_names_the_parameter_is_treated_as_a_refusal() {
        use reqwest::StatusCode;
        // The shapes a server that does not know the field actually answers with.
        assert!(refused_stream_options(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#
        ));
        assert!(refused_stream_options(
            StatusCode::BAD_REQUEST,
            "unknown field `include_usage`"
        ));
        // A 400 for a REAL reason must not be retried: retrying it would report the second failure
        // and hide the first, which is worse than not retrying at all.
        assert!(!refused_stream_options(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"model `nope` not found"}}"#
        ));
        // Neither is a server error — that is not a statement about the parameter.
        assert!(!refused_stream_options(
            StatusCode::INTERNAL_SERVER_ERROR,
            "stream_options exploded"
        ));
    }

    #[test]
    fn the_escape_hatch_is_off_by_absence_not_by_presence() {
        // Unset must mean ON: the whole point is that a spec-compliant upstream reports usage
        // without anyone configuring anything.
        let saved = std::env::var("ROZUM_OPENAI_STREAM_USAGE").ok();
        unsafe { std::env::remove_var("ROZUM_OPENAI_STREAM_USAGE") };
        assert!(stream_usage_wanted());
        for off in ["0", "false", "off", "no"] {
            unsafe { std::env::set_var("ROZUM_OPENAI_STREAM_USAGE", off) };
            assert!(!stream_usage_wanted(), "{off} should disable it");
        }
        unsafe { std::env::set_var("ROZUM_OPENAI_STREAM_USAGE", "1") };
        assert!(stream_usage_wanted());
        match saved {
            Some(v) => unsafe { std::env::set_var("ROZUM_OPENAI_STREAM_USAGE", v) },
            None => unsafe { std::env::remove_var("ROZUM_OPENAI_STREAM_USAGE") },
        }
    }

    #[test]
    fn parse_text_delta() {
        let line =
            r#"data: {"id":"x","choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#;
        let mut tools: Vec<OaiToolInFlight> = Vec::new();
        let events = parse_sse_data_line(line, &mut tools).unwrap();
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], ChatEvent::TextDelta { text } if text == "hello"));
    }

    #[test]
    fn parse_done_finish() {
        let line = r#"data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":3}}"#;
        let mut tools = Vec::new();
        let events = parse_sse_data_line(line, &mut tools).unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            ChatEvent::Done {
                stop_reason: StopReason::EndTurn,
                ..
            }
        )));
    }

    #[test]
    fn parse_tool_call_sequence() {
        let mut tools = Vec::new();
        // First chunk: id + name
        let l1 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#;
        // Argument chunk
        let l2 = r#"data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#;
        // Done chunk
        let l3 = r#"data: {"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;

        let e1 = parse_sse_data_line(l1, &mut tools).unwrap();
        let e2 = parse_sse_data_line(l2, &mut tools).unwrap();
        let e3 = parse_sse_data_line(l3, &mut tools).unwrap();

        assert!(
            e1.iter().any(
                |e| matches!(e, ChatEvent::ToolUseStart { name, .. } if name == "get_weather")
            )
        );
        assert!(
            e2.iter()
                .any(|e| matches!(e, ChatEvent::ToolUseDelta { .. }))
        );
        assert!(e3.iter().any(|e| matches!(
            e,
            ChatEvent::Done {
                stop_reason: StopReason::ToolUse,
                ..
            }
        )));
    }

    #[test]
    fn parse_done_marker() {
        let line = "data: [DONE]";
        let mut tools = Vec::new();
        let events = parse_sse_data_line(line, &mut tools).unwrap();
        assert_eq!(events.len(), 0, "DONE marker returns empty vec");
    }

    #[test]
    fn messages_text_roundtrip() {
        let msgs = vec![
            Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "You are helpful.".into(),
                }],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::Text {
                    text: "Hello".into(),
                }],
            },
        ];
        let oai = messages_to_oai(&msgs);
        assert_eq!(oai.len(), 2);
        assert_eq!(oai[0]["role"], "system");
        assert_eq!(oai[1]["content"], "Hello");
    }
}
