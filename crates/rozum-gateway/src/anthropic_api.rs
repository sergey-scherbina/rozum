//! Anthropic Messages dialect (`POST /v1/messages`): wire DTOs, request→internal mapping, and
//! SSE/collect response serialization. Extracted from `gateway.rs` (gw-per-dialect-split). The
//! HANDLER (`anthropic_handler`) stays in `gateway.rs` (composition root owning `GatewayState`);
//! `parse_anthropic_tool_choice` stays there too (used by the handler). This is the stateless layer.
use std::convert::Infallible;

use axum::http::StatusCode;
use axum::response::{sse::Event, IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::backend::{ChatEvent, ChatStream, ContentBlock, Message, Role, StopReason, ToolDef};
use crate::gateway::{
    decode_data_uri_image, error_json, new_id, CancelOnDrop, ChatLease,
};

// ─── Anthropic wire types ─────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct AnthropicReq {
    #[serde(default)]
    pub(crate) model: Option<String>,
    #[serde(default)]
    pub(crate) system: Option<Value>,
    pub(crate) messages: Vec<AnthropicMsg>,
    #[serde(default)]
    pub(crate) tools: Vec<AnthropicTool>,
    #[serde(default)]
    pub(crate) tool_choice: Value,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) temperature: Option<f32>,
    #[serde(default)]
    pub(crate) stream: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct AnthropicMsg {
    pub(crate) role: String,
    pub(crate) content: Value, // String or array of content blocks
}

#[derive(Deserialize)]
pub(crate) struct AnthropicTool {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    pub(crate) input_schema: Value,
}

// ─── Anthropic conversion ─────────────────────────────────────────────────────

pub(crate) fn anthropic_content_to_blocks(content: &Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) => vec![ContentBlock::Text { text: s.clone() }],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|item| match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    let t = item["text"].as_str().unwrap_or("").to_owned();
                    Some(ContentBlock::Text { text: t })
                }
                // Anthropic image: {"type":"image","source":{"type":"base64","data":"..."}}
                // or {"type":"url","url":"data:...;base64,..."}.
                Some("image") => {
                    let src = &item["source"];
                    let data = match src.get("type").and_then(Value::as_str) {
                        Some("base64") => {
                            use base64::Engine;
                            src["data"].as_str().and_then(|d| {
                                base64::engine::general_purpose::STANDARD
                                    .decode(d.as_bytes())
                                    .ok()
                            })
                        }
                        Some("url") => src["url"].as_str().and_then(decode_data_uri_image),
                        _ => None,
                    };
                    data.map(|data| ContentBlock::Image { data })
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

pub(crate) fn anthropic_messages_to_internal(system: Option<&Value>, msgs: &[AnthropicMsg]) -> Vec<Message> {
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
        match m.role.as_str() {
            "assistant" => {
                out.push(Message {
                    role: Role::Assistant,
                    content: anthropic_content_to_blocks(&m.content),
                });
            }
            // Anthropic has no `tool` role: tool results ride inside a `user` message as
            // `tool_result` blocks. The Qwen3 chat template only renders the trained
            // tool-response format under the `tool` role — left under `user`, the model
            // can't tie the output to its call and re-issues it (a file read-loop that
            // never reaches the edit). Split each tool_result into its own Role::Tool
            // message (mirrors the OpenAI/Responses paths), preserving block order.
            "user" => {
                let mut pending = Vec::new();
                for b in anthropic_content_to_blocks(&m.content) {
                    if matches!(b, ContentBlock::ToolResult { .. }) {
                        if !pending.is_empty() {
                            out.push(Message {
                                role: Role::User,
                                content: std::mem::take(&mut pending),
                            });
                        }
                        out.push(Message { role: Role::Tool, content: vec![b] });
                    } else {
                        pending.push(b);
                    }
                }
                if !pending.is_empty() {
                    out.push(Message { role: Role::User, content: pending });
                }
            }
            _ => continue,
        }
    }

    out
}

pub(crate) fn anthropic_tools_to_internal(tools: &[AnthropicTool]) -> Vec<ToolDef> {
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

pub(crate) fn anthropic_event(ev_type: &str, data: Value) -> Event {
    Event::default().event(ev_type).data(data.to_string())
}

pub(crate) fn anthropic_stop_reason(stop: &StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::ToolUse => "tool_use",
        StopReason::Cancelled => "end_turn",
    }
}

pub(crate) fn anthropic_sse_stream(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: String,
    lease: Option<ChatLease>,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let msg_id = new_id("msg");
    async_stream::stream! {
        let mut events = CancelOnDrop { stream: chat_stream, cancel, _lease: lease };

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

pub(crate) async fn anthropic_collect(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: &str,
    lease: Option<ChatLease>,
) -> Response {
    let msg_id = new_id("msg");
    let mut events = CancelOnDrop {
        stream: chat_stream,
        cancel,
        _lease: lease,
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
