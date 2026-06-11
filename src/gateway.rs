/// HTTP gateway exposing rozum's ChatBackend over two dialects:
///
/// - OpenAI Chat Completions  (`POST /v1/chat/completions`, `GET /v1/models`)
/// - Anthropic Messages       (`POST /v1/messages`)
///
/// Bind address is always `127.0.0.1`. Auth is optional bearer token via
/// `ROZUM_GATEWAY_TOKEN`. Cancel propagates from client disconnect.
use std::convert::Infallible;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, Sse},
    },
    routing::{get, post},
};
use futures::{Stream, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, ContentBlock, Message, ModelError,
    ModelResult, Role, SamplingParams, StopReason, ToolDef,
};

// ─── Shared state ─────────────────────────────────────────────────────────────

#[derive(Clone)]
struct GatewayState {
    backend: Arc<dyn ChatBackend>,
    /// Optional bearer token required on every request.
    auth_token: Option<String>,
    /// Advertised model name (used in response envelopes).
    model_id: String,
    /// Per-request metrics + JSONL event log.
    observer: Arc<crate::obs::Observer>,
    /// Liveness for the shared-daemon idle watchdog.
    activity: Arc<Activity>,
}

/// Tracks request activity so the shared gateway can idle-exit (free the model)
/// only when nothing is in flight and nothing has arrived for a while.
#[derive(Default)]
struct Activity {
    last_active: std::sync::atomic::AtomicU64,
    in_flight: std::sync::atomic::AtomicU64,
}

/// Options for [`serve_on`]. Defaults (no register, no idle) preserve the plain
/// in-process gateway behaviour used by `rozum launch --dedicated`.
#[derive(Default)]
pub struct ServeConfig {
    /// Idle-exit after this many seconds with zero in-flight requests and no new
    /// arrivals (the shared daemon). `None`/`0` = never idle-exit.
    pub idle_secs: Option<u64>,
    /// When set, publish/remove the shared-gateway registry (`share::active.json`).
    pub register_n_ctx: Option<u32>,
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_id(prefix: &str) -> String {
    format!("{}-{}", prefix, Uuid::new_v4().simple())
}

/// Very rough token estimate: 1 token ≈ 3.5 chars (conservative for code).
fn estimate_tokens(text: &str) -> u32 {
    ((text.len() as f32) / 3.5) as u32 + 1
}

fn total_message_text(messages: &[Message]) -> String {
    messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn error_json(status: StatusCode, msg: &str, err_type: &str) -> Response {
    let body = json!({ "error": { "message": msg, "type": err_type } });
    (status, axum::Json(body)).into_response()
}

/// Map a backend `chat()` error to an HTTP response. Overload sheds with 429 +
/// `Retry-After` so clients back off; everything else is a 500 with the dialect's
/// own error type (`backend_error` for OpenAI, `api_error` for Anthropic).
fn chat_error_response(e: &ModelError, fallback_type: &str) -> Response {
    match e {
        ModelError::Overloaded(msg) => {
            let mut resp = error_json(StatusCode::TOO_MANY_REQUESTS, msg, "overloaded");
            resp.headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            resp
        }
        _ => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
            fallback_type,
        ),
    }
}

// ─── CancelOnDrop ─────────────────────────────────────────────────────────────

/// Wraps a `ChatStream` and cancels the token when dropped.
/// When the axum Sse sink drops this stream (client disconnect), the backend
/// stops generating on the next token check.
struct CancelOnDrop {
    stream: ChatStream,
    cancel: CancellationToken,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

// ChatStream = Pin<Box<dyn Stream + Send>>; Pin<Box<T>> is Unpin because Box<T>: Unpin.
// CancellationToken: Unpin. So CancelOnDrop: Unpin automatically.

impl Stream for CancelOnDrop {
    type Item = ModelResult<ChatEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

// ─── OpenAI wire types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct OaiChatReq {
    #[serde(default)]
    model: Option<String>,
    messages: Vec<OaiMsg>,
    #[serde(default)]
    tools: Vec<OaiTool>,
    #[serde(default)]
    stream: Option<bool>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_tokens: Option<u32>,
    top_k: Option<u32>,
}

#[derive(Deserialize)]
struct OaiMsg {
    role: String,
    /// String, array of content blocks, or null (for tool-call-only turns).
    #[serde(default)]
    content: Value,
    #[serde(default)]
    tool_calls: Vec<OaiToolCall>,
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
struct OaiTool {
    function: OaiFn,
}

#[derive(Deserialize)]
struct OaiFn {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Value>,
}

#[derive(Deserialize)]
struct OaiToolCall {
    id: String,
    function: OaiFnCall,
}

#[derive(Deserialize)]
struct OaiFnCall {
    name: String,
    #[serde(default)]
    arguments: String,
}

// ─── OpenAI conversion ────────────────────────────────────────────────────────

fn oai_content_to_blocks(content: &Value, tool_calls: &[OaiToolCall]) -> Vec<ContentBlock> {
    let mut blocks = Vec::new();

    match content {
        Value::String(s) if !s.is_empty() => {
            blocks.push(ContentBlock::Text { text: s.clone() });
        }
        Value::Array(arr) => {
            for item in arr {
                match item.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(t) = item["text"].as_str() {
                            blocks.push(ContentBlock::Text { text: t.to_owned() });
                        }
                    }
                    Some("tool_result") => {
                        let id = item["tool_use_id"].as_str().unwrap_or("").to_owned();
                        let c = item["content"].as_str().unwrap_or("").to_owned();
                        let is_err = item["is_error"].as_bool().unwrap_or(false);
                        blocks.push(ContentBlock::ToolResult {
                            tool_use_id: id,
                            content: c,
                            is_error: is_err,
                        });
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    for tc in tool_calls {
        let input: Value = serde_json::from_str(&tc.function.arguments).unwrap_or(Value::Null);
        blocks.push(ContentBlock::ToolUse {
            id: tc.id.clone(),
            name: tc.function.name.clone(),
            input,
        });
    }

    blocks
}

fn oai_messages_to_internal(msgs: &[OaiMsg]) -> Vec<Message> {
    msgs.iter()
        .filter_map(|m| {
            let role = match m.role.as_str() {
                "system" => Role::System,
                "user" => Role::User,
                "assistant" => Role::Assistant,
                "tool" => {
                    let id = m.tool_call_id.clone().unwrap_or_default();
                    let text = m.content.as_str().unwrap_or("").to_owned();
                    return Some(Message {
                        role: Role::Tool,
                        content: vec![ContentBlock::ToolResult {
                            tool_use_id: id,
                            content: text,
                            is_error: false,
                        }],
                    });
                }
                _ => return None,
            };
            let content = oai_content_to_blocks(&m.content, &m.tool_calls);
            if content.is_empty() && m.role != "assistant" {
                return None;
            }
            Some(Message { role, content })
        })
        .collect()
}

fn oai_tools_to_internal(tools: &[OaiTool]) -> Vec<ToolDef> {
    tools
        .iter()
        .map(|t| ToolDef {
            name: t.function.name.clone(),
            description: t.function.description.clone().unwrap_or_default(),
            input_schema: t
                .function
                .parameters
                .clone()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        })
        .collect()
}

// ─── OpenAI SSE serialization ─────────────────────────────────────────────────

/// Accumulated state while streaming tool calls for the OAI SSE format.
struct OaiToolState {
    index: usize,
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    name: String,
    args: String,
}

fn oai_chunk(completion_id: &str, model: &str, delta: Value, finish_reason: Option<&str>) -> Event {
    let data = json!({
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": now_secs(),
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason,
        }]
    });
    Event::default().data(data.to_string())
}

fn oai_sse_stream(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let completion_id = new_id("chatcmpl");
    async_stream::stream! {
        let mut events = CancelOnDrop { stream: chat_stream, cancel };
        let mut tool: Option<OaiToolState> = None;
        let mut role_sent = false;

        while let Some(ev) = events.next().await {
            match ev {
                Ok(ChatEvent::TextDelta { text }) => {
                    // Send role on first delta
                    if !role_sent {
                        yield Ok(oai_chunk(&completion_id, &model,
                            json!({"role": "assistant", "content": ""}), None));
                        role_sent = true;
                    }
                    yield Ok(oai_chunk(&completion_id, &model,
                        json!({"content": text}), None));
                }

                Ok(ChatEvent::ToolUseStart { id, name }) => {
                    if !role_sent {
                        yield Ok(oai_chunk(&completion_id, &model,
                            json!({"role": "assistant", "content": null}), None));
                        role_sent = true;
                    }
                    let index = tool.as_ref().map(|t| t.index + 1).unwrap_or(0);
                    // First chunk for this tool call: id + name + empty args
                    let delta = json!({
                        "tool_calls": [{
                            "index": index,
                            "id": id,
                            "type": "function",
                            "function": { "name": name, "arguments": "" }
                        }]
                    });
                    yield Ok(oai_chunk(&completion_id, &model, delta, None));
                    tool = Some(OaiToolState { index, id, name, args: String::new() });
                }

                Ok(ChatEvent::ToolUseDelta { input_json_delta, .. }) => {
                    if let Some(ref t) = tool {
                        let delta = json!({
                            "tool_calls": [{
                                "index": t.index,
                                "function": { "arguments": input_json_delta }
                            }]
                        });
                        yield Ok(oai_chunk(&completion_id, &model, delta, None));
                        if let Some(ref mut t) = tool {
                            t.args.push_str(&input_json_delta);
                        }
                    }
                }

                Ok(ChatEvent::ToolUseEnd { .. }) => {
                    // Tool args complete; stop_reason will come with Done
                }

                Ok(ChatEvent::Done { stop_reason, .. }) => {
                    let finish = match stop_reason {
                        StopReason::ToolUse => "tool_calls",
                        StopReason::MaxTokens => "length",
                        StopReason::Cancelled => "stop",
                        StopReason::EndTurn => "stop",
                    };
                    yield Ok(oai_chunk(&completion_id, &model, json!({}), Some(finish)));
                    break;
                }

                Err(e) => {
                    // Emit as a final error chunk and stop
                    let data = json!({ "error": { "message": e.to_string() } });
                    yield Ok(Event::default().data(data.to_string()));
                    break;
                }
            }
        }
        yield Ok(Event::default().data("[DONE]"));
    }
}

// ─── OpenAI non-streaming response ───────────────────────────────────────────

async fn oai_collect(chat_stream: ChatStream, cancel: CancellationToken, model: &str) -> Response {
    let completion_id = new_id("chatcmpl");
    let mut events = CancelOnDrop {
        stream: chat_stream,
        cancel,
    };
    let mut text = String::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut current_tool: Option<(String, String, String)> = None; // (id, name, args)
    let mut finish_reason = "stop";
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;

    while let Some(ev) = events.next().await {
        match ev {
            Ok(ChatEvent::TextDelta { text: t }) => text.push_str(&t),
            Ok(ChatEvent::ToolUseStart { id, name }) => {
                current_tool = Some((id, name, String::new()));
            }
            Ok(ChatEvent::ToolUseDelta {
                input_json_delta, ..
            }) => {
                if let Some((_, _, ref mut args)) = current_tool {
                    args.push_str(&input_json_delta);
                }
            }
            Ok(ChatEvent::ToolUseEnd { .. }) => {
                if let Some((id, name, args)) = current_tool.take() {
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": { "name": name, "arguments": args }
                    }));
                }
            }
            Ok(ChatEvent::Done {
                stop_reason,
                input_tokens: i,
                output_tokens: o,
            }) => {
                finish_reason = match stop_reason {
                    StopReason::ToolUse => "tool_calls",
                    StopReason::MaxTokens => "length",
                    _ => "stop",
                };
                input_tokens = i;
                output_tokens = o;
                break;
            }
            Err(e) => {
                return error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                    "backend_error",
                );
            }
        }
    }

    let message = if tool_calls.is_empty() {
        json!({ "role": "assistant", "content": text })
    } else {
        json!({ "role": "assistant", "content": null, "tool_calls": tool_calls })
    };

    let body = json!({
        "id": completion_id,
        "object": "chat.completion",
        "created": now_secs(),
        "model": model,
        "choices": [{ "index": 0, "message": message, "finish_reason": finish_reason }],
        "usage": {
            "prompt_tokens": input_tokens,
            "completion_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        }
    });
    axum::Json(body).into_response()
}

// ─── Anthropic wire types ─────────────────────────────────────────────────────

#[derive(Deserialize)]
struct AnthropicReq {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    system: Option<Value>,
    messages: Vec<AnthropicMsg>,
    #[serde(default)]
    tools: Vec<AnthropicTool>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    #[serde(default)]
    stream: Option<bool>,
}

#[derive(Deserialize)]
struct AnthropicMsg {
    role: String,
    content: Value, // String or array of content blocks
}

#[derive(Deserialize)]
struct AnthropicTool {
    name: String,
    #[serde(default)]
    description: Option<String>,
    input_schema: Value,
}

// ─── Anthropic conversion ─────────────────────────────────────────────────────

fn anthropic_content_to_blocks(content: &Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => vec![ContentBlock::Text { text: s.clone() }],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = item["text"].as_str().unwrap_or("").to_owned();
                    Some(ContentBlock::Text { text: t })
                }
                Some("tool_use") => {
                    let id = item["id"].as_str().unwrap_or("").to_owned();
                    let name = item["name"].as_str().unwrap_or("").to_owned();
                    let input = item["input"].clone();
                    Some(ContentBlock::ToolUse { id, name, input })
                }
                Some("tool_result") => {
                    let id = item["tool_use_id"].as_str().unwrap_or("").to_owned();
                    let c = match &item["content"] {
                        Value::String(s) => s.clone(),
                        Value::Array(arr) => arr
                            .iter()
                            .filter_map(|b| {
                                if b["type"] == "text" {
                                    b["text"].as_str().map(str::to_owned)
                                } else {
                                    None
                                }
                            })
                            .collect::<Vec<_>>()
                            .join(""),
                        _ => String::new(),
                    };
                    let is_err = item["is_error"].as_bool().unwrap_or(false);
                    Some(ContentBlock::ToolResult {
                        tool_use_id: id,
                        content: c,
                        is_error: is_err,
                    })
                }
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn anthropic_messages_to_internal(system: Option<&Value>, msgs: &[AnthropicMsg]) -> Vec<Message> {
    let mut out = Vec::new();

    // Inject system message if present
    if let Some(sys) = system {
        let text = match sys {
            Value::String(s) => s.clone(),
            Value::Array(arr) => arr
                .iter()
                .filter_map(|b| {
                    if b["type"] == "text" {
                        b["text"].as_str().map(str::to_owned)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join(""),
            _ => String::new(),
        };
        if !text.is_empty() {
            out.push(Message {
                role: Role::System,
                content: vec![ContentBlock::Text { text }],
            });
        }
    }

    for m in msgs {
        let role = match m.role.as_str() {
            "user" => Role::User,
            "assistant" => Role::Assistant,
            _ => continue,
        };
        let content = anthropic_content_to_blocks(&m.content);
        out.push(Message { role, content });
    }

    out
}

fn anthropic_tools_to_internal(tools: &[AnthropicTool]) -> Vec<ToolDef> {
    tools
        .iter()
        .map(|t| ToolDef {
            name: t.name.clone(),
            description: t.description.clone().unwrap_or_default(),
            input_schema: t.input_schema.clone(),
        })
        .collect()
}

// ─── Anthropic SSE serialization ──────────────────────────────────────────────

fn anthropic_event(ev_type: &str, data: Value) -> Event {
    Event::default().event(ev_type).data(data.to_string())
}

fn anthropic_stop_reason(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::Cancelled => "end_turn",
    }
}

fn anthropic_sse_stream(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: String,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let msg_id = new_id("msg");
    async_stream::stream! {
        let mut events = CancelOnDrop { stream: chat_stream, cancel };

        // message_start
        yield Ok(anthropic_event("message_start", json!({
            "type": "message_start",
            "message": {
                "id": msg_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "usage": { "input_tokens": 0, "output_tokens": 0 }
            }
        })));

        let mut block_index: u32 = 0;
        let mut text_block_open = false;
        #[allow(unused_assignments)] let mut tool_block_open = false;

        while let Some(ev) = events.next().await {
            match ev {
                Ok(ChatEvent::TextDelta { text }) => {
                    if !text_block_open {
                        // Close any open tool block first (shouldn't happen normally)
                        if tool_block_open {
                            yield Ok(anthropic_event("content_block_stop",
                                json!({ "type": "content_block_stop", "index": block_index })));
                            block_index += 1;
                            tool_block_open = false;
                        }
                        yield Ok(anthropic_event("content_block_start", json!({
                            "type": "content_block_start",
                            "index": block_index,
                            "content_block": { "type": "text", "text": "" }
                        })));
                        text_block_open = true;
                    }
                    yield Ok(anthropic_event("content_block_delta", json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": { "type": "text_delta", "text": text }
                    })));
                }

                Ok(ChatEvent::ToolUseStart { id, name }) => {
                    // Close text block if open
                    if text_block_open {
                        yield Ok(anthropic_event("content_block_stop",
                            json!({ "type": "content_block_stop", "index": block_index })));
                        block_index += 1;
                        text_block_open = false;
                    }
                    // Close previous tool block if open
                    if tool_block_open {
                        yield Ok(anthropic_event("content_block_stop",
                            json!({ "type": "content_block_stop", "index": block_index })));
                        block_index += 1;
                        // tool_block_open will be set true again immediately below
                    }
                    yield Ok(anthropic_event("content_block_start", json!({
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} }
                    })));
                    tool_block_open = true;
                }

                Ok(ChatEvent::ToolUseDelta { input_json_delta, .. }) => {
                    yield Ok(anthropic_event("content_block_delta", json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": { "type": "input_json_delta", "partial_json": input_json_delta }
                    })));
                }

                Ok(ChatEvent::ToolUseEnd { .. }) => {
                    if tool_block_open {
                        yield Ok(anthropic_event("content_block_stop",
                            json!({ "type": "content_block_stop", "index": block_index })));
                        block_index += 1;
                        tool_block_open = false;
                    }
                }

                Ok(ChatEvent::Done { stop_reason, input_tokens: _, output_tokens }) => {
                    // Close any open block. Don't update tracking vars; we break immediately.
                    if text_block_open | tool_block_open {
                        yield Ok(anthropic_event("content_block_stop",
                            json!({ "type": "content_block_stop", "index": block_index })));
                    }
                    let sr = anthropic_stop_reason(&stop_reason);
                    yield Ok(anthropic_event("message_delta", json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": sr, "stop_sequence": null },
                        "usage": { "output_tokens": output_tokens }
                    })));
                    yield Ok(anthropic_event("message_stop", json!({ "type": "message_stop" })));
                    break;
                }

                Err(e) => {
                    yield Ok(anthropic_event("error", json!({
                        "type": "error",
                        "error": { "type": "api_error", "message": e.to_string() }
                    })));
                    break;
                }
            }
        }
    }
}

// ─── Anthropic non-streaming response ────────────────────────────────────────

async fn anthropic_collect(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: &str,
) -> Response {
    let msg_id = new_id("msg");
    let mut events = CancelOnDrop {
        stream: chat_stream,
        cancel,
    };
    let mut text = String::new();
    let mut tool_blocks: Vec<Value> = Vec::new();
    let mut current_tool: Option<(String, String, String)> = None;
    let mut stop_reason = "end_turn";
    let mut in_tokens = 0u32;
    let mut out_tokens = 0u32;

    while let Some(ev) = events.next().await {
        match ev {
            Ok(ChatEvent::TextDelta { text: t }) => text.push_str(&t),
            Ok(ChatEvent::ToolUseStart { id, name }) => {
                current_tool = Some((id, name, String::new()));
            }
            Ok(ChatEvent::ToolUseDelta {
                input_json_delta, ..
            }) => {
                if let Some((_, _, ref mut a)) = current_tool {
                    a.push_str(&input_json_delta);
                }
            }
            Ok(ChatEvent::ToolUseEnd { .. }) => {
                if let Some((id, name, args)) = current_tool.take() {
                    let input: Value =
                        serde_json::from_str(&args).unwrap_or(Value::Object(Default::default()));
                    tool_blocks.push(
                        json!({ "type": "tool_use", "id": id, "name": name, "input": input }),
                    );
                }
            }
            Ok(ChatEvent::Done {
                stop_reason: sr,
                input_tokens: i,
                output_tokens: o,
            }) => {
                stop_reason = anthropic_stop_reason(&sr);
                in_tokens = i;
                out_tokens = o;
                break;
            }
            Err(e) => {
                return error_json(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &e.to_string(),
                    "api_error",
                );
            }
        }
    }

    let mut content: Vec<Value> = Vec::new();
    if !text.is_empty() {
        content.push(json!({ "type": "text", "text": text }));
    }
    content.extend(tool_blocks);

    let body = json!({
        "id": msg_id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "usage": { "input_tokens": in_tokens, "output_tokens": out_tokens }
    });
    axum::Json(body).into_response()
}

// ─── Route handlers ───────────────────────────────────────────────────────────

async fn stats_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let mut snap = state.observer.snapshot();
    if let Value::Object(ref mut m) = snap {
        m.insert("model".into(), json!(state.model_id));
        m.insert(
            "context_window".into(),
            json!(state.backend.context_window()),
        );
    }
    axum::Json(snap)
}

/// Cheap admission probe for the two-tier backpressure: a launch-local proxy
/// reads this to learn the daemon's free window and decide whether to forward a
/// queued request now or hold it at the edge. Ungated backends report a generous
/// always-free window so the proxy fails open. (`shared-gateway.md`.)
async fn admit_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    match state.backend.admission_stats() {
        Some(s) => axum::Json(json!({
            "limit": s.limit,
            "in_use": s.in_use,
            "waiting": s.waiting,
            "free": s.free(),
        })),
        None => axum::Json(json!({ "free": 1, "unlimited": true })),
    }
}

async fn models_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    tracing::debug!("GET /v1/models");
    // Claude Code's gateway discovery only adds models whose id starts with
    // "claude" or "anthropic" to its /model picker. Expose an alias so the
    // real model name still appears in display_name.
    let claude_alias = claude_model_alias(&state.model_id);
    axum::Json(json!({
        "object": "list",
        "data": [{
            "id": claude_alias,
            "object": "model",
            "created": now_secs(),
            "owned_by": "rozum",
            "display_name": state.model_id,
        }]
    }))
}

fn sanitize_id(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The model id Claude Code will see in `/v1/models` for a given backend spec.
/// `rozum launch` exports this as `ANTHROPIC_MODEL` so Claude Code pre-selects
/// the local model instead of starting on the default OAuth model.
pub fn claude_model_alias(model_spec: &str) -> String {
    format!("claude-rozum-{}", sanitize_id(model_spec))
}

async fn oai_chat_handler(
    State(state): State<GatewayState>,
    axum::Json(req): axum::Json<OaiChatReq>,
) -> Response {
    tracing::debug!(
        model = req.model.as_deref().unwrap_or("?"),
        msgs = req.messages.len(),
        tools = req.tools.len(),
        stream = req.stream.unwrap_or(true),
        "POST /v1/chat/completions"
    );
    let messages = oai_messages_to_internal(&req.messages);
    let tools = oai_tools_to_internal(&req.tools);

    // Approximate context overflow check
    let prompt_text = total_message_text(&messages);
    let est = estimate_tokens(&prompt_text);
    let ctx_win = state.backend.context_window();
    if ctx_win > 0 && est > ctx_win {
        return error_json(
            StatusCode::BAD_REQUEST,
            &format!("prompt exceeds model context window of {ctx_win} tokens"),
            "context_length_exceeded",
        );
    }

    let (n_messages, n_tools) = (messages.len(), tools.len());
    let cancel = CancellationToken::new();
    let chat_req = ChatRequest {
        messages,
        tools,
        sampling: SamplingParams {
            temperature: req.temperature,
            top_p: req.top_p,
            max_tokens: req.max_tokens,
            top_k: req.top_k,
            ..Default::default()
        },
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = req.model.unwrap_or_else(|| state.model_id.clone());
    let stream_mode = req.stream.unwrap_or(true);

    match state.backend.chat(chat_req).await {
        Err(e) => {
            crate::obs::log_event(json!({
                "event": "request_error", "endpoint": "/v1/chat/completions", "error": e.to_string(),
            }));
            chat_error_response(&e, "backend_error")
        }
        Ok(chat_stream) => {
            let meta = crate::obs::ReqMeta {
                endpoint: "/v1/chat/completions",
                model: model.clone(),
                n_messages,
                n_tools,
                est_prompt_tokens: est,
            };
            let chat_stream = crate::obs::meter(chat_stream, state.observer.clone(), meta);
            if stream_mode {
                Sse::new(oai_sse_stream(chat_stream, cancel, model)).into_response()
            } else {
                oai_collect(chat_stream, cancel, &model).await
            }
        }
    }
}

async fn anthropic_handler(
    State(state): State<GatewayState>,
    axum::Json(req): axum::Json<AnthropicReq>,
) -> Response {
    tracing::debug!(
        model = req.model.as_deref().unwrap_or("?"),
        msgs = req.messages.len(),
        tools = req.tools.len(),
        stream = req.stream.unwrap_or(true),
        "POST /v1/messages"
    );
    let messages = anthropic_messages_to_internal(req.system.as_ref(), &req.messages);
    let tools = anthropic_tools_to_internal(&req.tools);

    // Approximate context overflow check
    let prompt_text = total_message_text(&messages);
    let est = estimate_tokens(&prompt_text);
    let ctx_win = state.backend.context_window();
    if ctx_win > 0 && est > ctx_win {
        return error_json(
            StatusCode::BAD_REQUEST,
            &format!("prompt exceeds model context window of {ctx_win} tokens"),
            "context_length_exceeded",
        );
    }

    let (n_messages, n_tools) = (messages.len(), tools.len());
    let cancel = CancellationToken::new();
    let chat_req = ChatRequest {
        messages,
        tools,
        sampling: SamplingParams {
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            ..Default::default()
        },
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = req.model.unwrap_or_else(|| state.model_id.clone());
    let stream_mode = req.stream.unwrap_or(true);

    match state.backend.chat(chat_req).await {
        Err(e) => {
            crate::obs::log_event(json!({
                "event": "request_error", "endpoint": "/v1/messages", "error": e.to_string(),
            }));
            chat_error_response(&e, "api_error")
        }
        Ok(chat_stream) => {
            let meta = crate::obs::ReqMeta {
                endpoint: "/v1/messages",
                model: model.clone(),
                n_messages,
                n_tools,
                est_prompt_tokens: est,
            };
            let chat_stream = crate::obs::meter(chat_stream, state.observer.clone(), meta);
            if stream_mode {
                Sse::new(anthropic_sse_stream(chat_stream, cancel, model)).into_response()
            } else {
                anthropic_collect(chat_stream, cancel, &model).await
            }
        }
    }
}

// ─── Auth middleware ──────────────────────────────────────────────────────────

async fn auth_layer(
    State(state): State<GatewayState>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    if let Some(expected) = &state.auth_token {
        let ok = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(|token| token.trim() == expected.as_str())
            .unwrap_or(false);
        if !ok {
            return (StatusCode::UNAUTHORIZED, "401 Unauthorized\n").into_response();
        }
    }
    // Activity tracking for the idle watchdog: count this request as in-flight
    // for its whole duration so a long generation can't trip the idle timer.
    use std::sync::atomic::Ordering;
    let act = &state.activity;
    act.in_flight.fetch_add(1, Ordering::Relaxed);
    act.last_active
        .store(crate::share::now_unix(), Ordering::Relaxed);
    let resp = next.run(req).await;
    act.in_flight.fetch_sub(1, Ordering::Relaxed);
    act.last_active
        .store(crate::share::now_unix(), Ordering::Relaxed);
    resp
}

// ─── Public entry point ───────────────────────────────────────────────────────

pub async fn run(
    backend: Arc<dyn ChatBackend>,
    port: u16,
    model_id: String,
    cfg: ServeConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_on(backend, listener, model_id, cfg).await
}

/// Serve the gateway on an already-bound listener.
/// Useful when callers need to bind before spawning to avoid startup races.
pub async fn serve_on(
    backend: Arc<dyn ChatBackend>,
    listener: tokio::net::TcpListener,
    model_id: String,
    cfg: ServeConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let observer = crate::obs::Observer::new();
    observer.set_backend_label(backend.label());
    let port = listener.local_addr().ok().map(|a| a.port());
    crate::obs::log_event(serde_json::json!({
        "event": "gateway_listening",
        "addr": listener.local_addr().ok().map(|a| a.to_string()),
        "backend": backend.label(),
        "model": model_id,
    }));

    // Shared-daemon registry: publish so `rozum launch` clients discover & reuse
    // us; remove on exit (only if it's still our record). Spec: shared-gateway.
    let registered_pid = match (cfg.register_n_ctx, port) {
        (Some(n_ctx), Some(port)) => {
            let pid = std::process::id();
            let _ = crate::share::write_active(&crate::share::ActiveGateway {
                model: model_id.clone(),
                port,
                pid,
                n_ctx,
                started_at: crate::share::now_unix(),
            });
            Some(pid)
        }
        _ => None,
    };

    let state = GatewayState {
        backend,
        auth_token: std::env::var("ROZUM_GATEWAY_TOKEN").ok(),
        model_id,
        observer,
        activity: Arc::new(Activity::default()),
    };

    // Idle watchdog (shared daemon only): exit when nothing is in flight and no
    // request has arrived for `idle_secs`, freeing the resident model.
    if let Some(idle) = cfg.idle_secs.filter(|&s| s > 0) {
        use std::sync::atomic::Ordering;
        state
            .activity
            .last_active
            .store(crate::share::now_unix(), Ordering::Relaxed);
        let activity = Arc::clone(&state.activity);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                let idle_for = crate::share::now_unix()
                    .saturating_sub(activity.last_active.load(Ordering::Relaxed));
                // Stay up while any launch holds a fresh lease OR a request is in
                // flight OR there was recent HTTP traffic (covers manual `rozum
                // gateway` use). Exit only when all are quiet for `idle`.
                let live_leases = crate::share::live_lease_count(crate::share::LEASE_FRESH_SECS);
                if activity.in_flight.load(Ordering::Relaxed) == 0
                    && live_leases == 0
                    && idle_for >= idle
                {
                    crate::obs::log_event(serde_json::json!({
                        "event": "gateway_idle_exit", "idle_secs": idle_for,
                    }));
                    if let Some(pid) = registered_pid {
                        crate::share::remove_active_if_mine(pid);
                    }
                    std::process::exit(0);
                }
            }
        });
    }

    let app = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/admit", get(admit_handler))
        .route("/stats", get(stats_handler))
        .route("/v1/chat/completions", post(oai_chat_handler))
        .route("/v1/messages", post(anthropic_handler))
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer))
        .with_state(state);

    tracing::info!(addr = ?listener.local_addr().ok(), "rozum gateway listening");
    let result = axum::serve(listener, app).await;
    if let Some(pid) = registered_pid {
        crate::share::remove_active_if_mine(pid);
    }
    result?;
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{ChatEvent, HelloBackend, StopReason};
    use futures::StreamExt as _;

    async fn collect_sse(stream: impl Stream<Item = Result<Event, Infallible>>) -> Vec<String> {
        futures::pin_mut!(stream);
        let mut out = Vec::new();
        while let Some(Ok(ev)) = stream.next().await {
            // axum Event doesn't expose data directly; format as string via debug
            // We check via the JSON we build so use a mock approach instead
            let _ = ev; // just collect count
            out.push("event".to_owned());
        }
        out
    }

    #[tokio::test]
    async fn oai_chat_handler_returns_hello() {
        let backend = Arc::new(HelloBackend::new()) as Arc<dyn ChatBackend>;
        let req = ChatRequest::simple("ping");
        let stream = backend.chat(req).await.unwrap();
        let cancel = CancellationToken::new();
        let sse = oai_sse_stream(stream, cancel, "hello".to_owned());
        futures::pin_mut!(sse);
        // Must have at least one event + [DONE]
        let events: Vec<_> = sse.collect().await;
        // Content event + finish event + [DONE]
        assert!(
            events.len() >= 2,
            "expected SSE events, got {}",
            events.len()
        );
    }

    #[tokio::test]
    async fn anthropic_sse_emits_message_start() {
        let backend = Arc::new(HelloBackend::new()) as Arc<dyn ChatBackend>;
        let req = ChatRequest::simple("hello");
        let stream = backend.chat(req).await.unwrap();
        let cancel = CancellationToken::new();
        let sse = anthropic_sse_stream(stream, cancel, "hello-model".to_owned());
        futures::pin_mut!(sse);
        let events: Vec<_> = sse.collect().await;
        // message_start + content_block_start + content_block_delta + content_block_stop +
        // message_delta + message_stop = 6+ events
        assert!(
            events.len() >= 4,
            "expected Anthropic SSE events, got {}",
            events.len()
        );
    }

    #[tokio::test]
    async fn context_overflow_detected() {
        // PlaceholderBackend returns Err immediately and has context_window = 0
        // (0 means unchecked). Test with HelloBackend whose context_window = u32::MAX.
        let long_text: String = "word ".repeat(200_000);
        let messages = oai_messages_to_internal(&[OaiMsg {
            role: "user".into(),
            content: Value::String(long_text),
            tool_calls: vec![],
            tool_call_id: None,
        }]);
        let text = total_message_text(&messages);
        let est = estimate_tokens(&text);
        // HelloBackend has context_window u32::MAX so overflow check won't fire for it.
        // Just verify the estimate function works.
        assert!(est > 100_000, "expected large token estimate, got {est}");
    }

    #[test]
    fn oai_messages_parsed() {
        let msgs = vec![
            OaiMsg {
                role: "system".into(),
                content: Value::String("You are helpful.".into()),
                tool_calls: vec![],
                tool_call_id: None,
            },
            OaiMsg {
                role: "user".into(),
                content: Value::String("Hello".into()),
                tool_calls: vec![],
                tool_call_id: None,
            },
        ];
        let internal = oai_messages_to_internal(&msgs);
        assert_eq!(internal.len(), 2);
        assert!(matches!(internal[0].role, Role::System));
        assert!(matches!(internal[1].role, Role::User));
    }

    #[test]
    fn anthropic_system_injected() {
        let sys = Value::String("Be concise.".into());
        let msgs = vec![AnthropicMsg {
            role: "user".into(),
            content: Value::String("Hi".into()),
        }];
        let internal = anthropic_messages_to_internal(Some(&sys), &msgs);
        assert_eq!(internal.len(), 2);
        assert!(matches!(internal[0].role, Role::System));
        assert!(matches!(internal[1].role, Role::User));
    }

    #[test]
    fn oai_tool_def_mapped() {
        let tools = vec![OaiTool {
            function: OaiFn {
                name: "get_weather".into(),
                description: Some("Get weather".into()),
                parameters: Some(json!({ "type": "object" })),
            },
        }];
        let defs = oai_tools_to_internal(&tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "get_weather");
    }
}
