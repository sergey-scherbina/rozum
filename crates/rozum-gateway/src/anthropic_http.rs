//! Anthropic Messages HTTP backend (client side).
//!
//! A [`ChatBackend`] that calls `POST /v1/messages` on the Anthropic API (or any
//! Anthropic-compatible server) and maps its SSE stream back to [`ChatEvent`]s. The
//! mirror of the gateway's *server*-side Anthropic dialect — here we are the client.
//! Useful as a frontier-model escalation / fallback when a local model can't handle a
//! task (see `docs/specs/integration.md`).

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
    ModelResult, Role, StopReason,
};

/// Default `max_tokens` when the request doesn't set one (Anthropic requires it).
const DEFAULT_MAX_TOKENS: u32 = 4096;

pub struct AnthropicHttpBackend {
    /// Base URL (no trailing slash), e.g. `https://api.anthropic.com`.
    endpoint: String,
    model: String,
    api_key: String,
    version: String,
    client: reqwest::Client,
}

impl AnthropicHttpBackend {
    pub fn new(
        endpoint: impl Into<String>,
        model: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .expect("reqwest client");
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            model: model.into(),
            api_key: api_key.into(),
            version: "2023-06-01".to_owned(),
            client,
        }
    }
}

// ─── Conversion: internal messages → Anthropic request ─────────────────────────

/// Split internal messages into Anthropic's `(system, messages)` shape. System turns
/// fold into the top-level `system` string; tool results go in user-role messages.
fn messages_to_anthropic(messages: &[Message]) -> (String, Vec<Value>) {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();

    for m in messages {
        if matches!(m.role, Role::System) {
            for b in &m.content {
                if let ContentBlock::Text { text } = b {
                    if !system.is_empty() {
                        system.push('\n');
                    }
                    system.push_str(text);
                }
            }
            continue;
        }
        // Tool results are carried as user-role messages in the Anthropic wire form.
        let role = match m.role {
            Role::Assistant => "assistant",
            _ => "user",
        };
        let blocks: Vec<Value> = m
            .content
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
                ContentBlock::Image { .. } => json!({ "type": "text", "text": "" }),
                ContentBlock::ToolUse { id, name, input } => {
                    json!({ "type": "tool_use", "id": id, "name": name, "input": input })
                }
                ContentBlock::ToolResult { tool_use_id, content, is_error } => json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                    "content": content,
                    "is_error": is_error,
                }),
            })
            .collect();
        if !blocks.is_empty() {
            out.push(json!({ "role": role, "content": blocks }));
        }
    }
    (system, out)
}

// ─── SSE parser: Anthropic events → ChatEvents ─────────────────────────────────

struct AnthropicSseState {
    /// content-block index → tool-call id (only for `tool_use` blocks).
    tool_blocks: HashMap<usize, String>,
    stop_reason: StopReason,
    in_tokens: u32,
    out_tokens: u32,
}

impl Default for AnthropicSseState {
    fn default() -> Self {
        Self {
            tool_blocks: HashMap::new(),
            stop_reason: StopReason::EndTurn,
            in_tokens: 0,
            out_tokens: 0,
        }
    }
}

fn anthropic_stop_reason(s: &str) -> StopReason {
    match s {
        "tool_use" => StopReason::ToolUse,
        "max_tokens" => StopReason::MaxTokens,
        // Upstream Anthropic distinguishes this and we were flattening it into `end_turn` on the
        // way in — found while adding the variant, not by looking for it.
        "stop_sequence" => StopReason::StopSequence,
        _ => StopReason::EndTurn,
    }
}

/// Map one Anthropic SSE event (the parsed `data:` JSON) to zero or more `ChatEvent`s.
fn parse_anthropic_event(v: &Value, state: &mut AnthropicSseState) -> Vec<ChatEvent> {
    let mut events = Vec::new();
    match v["type"].as_str() {
        Some("message_start") => {
            state.in_tokens = v["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32;
        }
        Some("content_block_start") => {
            let index = v["index"].as_u64().unwrap_or(0) as usize;
            let cb = &v["content_block"];
            if cb["type"] == "tool_use" {
                let id = cb["id"].as_str().unwrap_or("").to_owned();
                let name = cb["name"].as_str().unwrap_or("").to_owned();
                state.tool_blocks.insert(index, id.clone());
                events.push(ChatEvent::ToolUseStart { id, name });
            }
        }
        Some("content_block_delta") => {
            let index = v["index"].as_u64().unwrap_or(0) as usize;
            let delta = &v["delta"];
            match delta["type"].as_str() {
                Some("text_delta") => {
                    if let Some(t) = delta["text"].as_str() {
                        if !t.is_empty() {
                            events.push(ChatEvent::TextDelta { text: t.to_owned() });
                        }
                    }
                }
                Some("input_json_delta") => {
                    if let (Some(id), Some(frag)) =
                        (state.tool_blocks.get(&index), delta["partial_json"].as_str())
                    {
                        events.push(ChatEvent::ToolUseDelta {
                            id: id.clone(),
                            input_json_delta: frag.to_owned(),
                        });
                    }
                }
                _ => {}
            }
        }
        Some("content_block_stop") => {
            let index = v["index"].as_u64().unwrap_or(0) as usize;
            if let Some(id) = state.tool_blocks.remove(&index) {
                events.push(ChatEvent::ToolUseEnd { id });
            }
        }
        Some("message_delta") => {
            if let Some(sr) = v["delta"]["stop_reason"].as_str() {
                state.stop_reason = anthropic_stop_reason(sr);
            }
            if let Some(o) = v["usage"]["output_tokens"].as_u64() {
                state.out_tokens = o as u32;
            }
        }
        Some("message_stop") => {
            events.push(ChatEvent::Done {
                input_tokens: state.in_tokens,
                output_tokens: state.out_tokens,
                stop_reason: state.stop_reason,
            });
        }
        _ => {}
    }
    events
}

// ─── ChatBackend impl ──────────────────────────────────────────────────────────

#[async_trait]
impl ChatBackend for AnthropicHttpBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        let (system, messages) = messages_to_anthropic(&req.messages);

        let mut body = json!({
            "model": self.model,
            "max_tokens": req.sampling.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            "messages": messages,
            "stream": true,
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if let Some(t) = req.sampling.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(p) = req.sampling.top_p {
            body["top_p"] = json!(p);
        }
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect();
            body["tools"] = json!(tools);
        }

        let url = format!("{}/v1/messages", self.endpoint);
        let response = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", &self.version)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| ModelError::BackendUnavailable(format!("http request: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(ModelError::BackendUnavailable(format!(
                "anthropic returned {status}: {text}"
            )));
        }

        let cancel = req.cancel.clone();
        let byte_stream = response.bytes_stream();

        let stream: ChatStream = Box::pin(async_stream::stream! {
            use futures::StreamExt as _;
            tokio::pin!(byte_stream);

            let mut buf = String::new();
            let mut state = AnthropicSseState::default();
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
                        if !done_sent {
                            yield Ok(ChatEvent::Done {
                                input_tokens: state.in_tokens,
                                output_tokens: state.out_tokens,
                                stop_reason: state.stop_reason,
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
                        while let Some(nl) = buf.find('\n') {
                            let line = buf[..nl].trim_end_matches('\r').to_owned();
                            buf = buf[nl + 1..].to_owned();
                            // Dispatch off the data JSON's own `type`; ignore `event:` lines.
                            let Some(data) = line.strip_prefix("data: ") else { continue };
                            let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                            for ev in parse_anthropic_event(&v, &mut state) {
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
        });

        Ok(stream)
    }

    fn context_window(&self) -> u32 {
        u32::MAX // the server enforces its own limit
    }

    fn label(&self) -> &'static str {
        "anthropic-http"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_one(data: &str, state: &mut AnthropicSseState) -> Vec<ChatEvent> {
        parse_anthropic_event(&serde_json::from_str(data).unwrap(), state)
    }

    #[test]
    fn parses_text_stream_to_done() {
        let mut s = AnthropicSseState::default();
        assert!(parse_one(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":7}}}"#,
            &mut s
        )
        .is_empty());
        parse_one(r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#, &mut s);
        let e = parse_one(
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            &mut s,
        );
        assert!(matches!(&e[0], ChatEvent::TextDelta { text } if text == "Hi"));
        parse_one(r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#, &mut s);
        let done = parse_one(r#"{"type":"message_stop"}"#, &mut s);
        match &done[0] {
            ChatEvent::Done { input_tokens, output_tokens, stop_reason } => {
                assert_eq!(*input_tokens, 7);
                assert_eq!(*output_tokens, 3);
                assert_eq!(*stop_reason, StopReason::EndTurn);
            }
            _ => panic!("expected Done"),
        }
    }

    #[test]
    fn parses_tool_use_block() {
        let mut s = AnthropicSseState::default();
        let start = parse_one(
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
            &mut s,
        );
        assert!(matches!(&start[0], ChatEvent::ToolUseStart { id, name } if id == "toolu_1" && name == "get_weather"));
        let d = parse_one(
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            &mut s,
        );
        assert!(matches!(&d[0], ChatEvent::ToolUseDelta { id, input_json_delta } if id == "toolu_1" && input_json_delta == "{\"city\":"));
        let stop = parse_one(r#"{"type":"content_block_stop","index":1}"#, &mut s);
        assert!(matches!(&stop[0], ChatEvent::ToolUseEnd { id } if id == "toolu_1"));
        // stop_reason tool_use propagates to Done.
        parse_one(r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#, &mut s);
        let done = parse_one(r#"{"type":"message_stop"}"#, &mut s);
        assert!(matches!(&done[0], ChatEvent::Done { stop_reason: StopReason::ToolUse, .. }));
    }

    #[test]
    fn message_conversion_splits_system_and_tools() {
        use crate::backend::ContentBlock;
        let msgs = vec![
            Message::system("be terse"),
            Message::user("hi"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "f".into(),
                    input: json!({"a": 1}),
                }],
            },
            Message {
                role: Role::Tool,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
            },
        ];
        let (system, out) = messages_to_anthropic(&msgs);
        assert_eq!(system, "be terse");
        assert_eq!(out.len(), 3); // user, assistant(tool_use), user(tool_result)
        assert_eq!(out[0]["role"], "user");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0]["type"], "tool_use");
        assert_eq!(out[2]["role"], "user");
        assert_eq!(out[2]["content"][0]["type"], "tool_result");
        assert_eq!(out[2]["content"][0]["tool_use_id"], "t1");
    }
}

#[cfg(test)]
mod stop_reason_tests {
    use super::anthropic_stop_reason;
    use crate::backend::StopReason;

    #[test]
    fn an_upstream_stop_sequence_is_no_longer_flattened_into_end_turn() {
        // The other direction, found while adding the variant rather than by looking for it:
        // upstream Anthropic distinguishes these and this shim was collapsing the distinction on
        // the way IN, so a proxied answer lost the same information the serializer now preserves.
        assert_eq!(anthropic_stop_reason("stop_sequence"), StopReason::StopSequence);
        assert_eq!(anthropic_stop_reason("tool_use"), StopReason::ToolUse);
        assert_eq!(anthropic_stop_reason("max_tokens"), StopReason::MaxTokens);
        assert_eq!(anthropic_stop_reason("end_turn"), StopReason::EndTurn);
        // Anything unknown stays the safe default rather than becoming a new guess.
        assert_eq!(anthropic_stop_reason("something_new"), StopReason::EndTurn);
    }
}
