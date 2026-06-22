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
        }
    }

    /// Attach an `Authorization: Bearer <key>` to every request (builder style).
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        let k = key.into();
        self.api_key = (!k.is_empty()).then_some(k);
        self
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

// ─── ChatBackend impl ─────────────────────────────────────────────────────────

#[async_trait]
impl ChatBackend for OpenAiHttpBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        let oai_messages = messages_to_oai(&req.messages);

        let mut body = json!({
            "model": self.model,
            "messages": oai_messages,
            "stream": true,
        });

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

        let url = format!("{}/chat/completions", self.endpoint);
        let mut request = self.client.post(&url).json(&body);
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key);
        }
        let response = request
            .send()
            .await
            .map_err(|e| ModelError::BackendUnavailable(format!("http request: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ModelError::BackendUnavailable(format!(
                "server returned {status}: {text}"
            )));
        }

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
