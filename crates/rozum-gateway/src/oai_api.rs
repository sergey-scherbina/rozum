//! OpenAI Chat Completions dialect (`POST /v1/chat/completions`): wire DTOs, request→internal
//! mapping, and SSE/collect response serialization. Extracted from `gateway.rs`
//! (gw-per-dialect-split). The HANDLER (`oai_chat_handler`) stays in `gateway.rs` as the composition
//! root — it owns `GatewayState`; moving it would leak the gateway's internal state API here.
use std::convert::Infallible;

use axum::http::StatusCode;
use axum::response::{sse::Event, IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::backend::{ChatEvent, ChatStream, ContentBlock, Message, Role, StopReason, ToolDef};
use crate::gateway::{
    decode_data_uri_image, error_json, new_id, now_secs, CancelOnDrop, ChatLease,
};
// NOTE: `ToolChoice` (a dialect-agnostic enum) physically landed in this module with the OAI
// mapping range; `gateway.rs` + the other dialects reach it via `use crate::oai_api::*`. It should
// migrate to a shared `api_common` module in a later pass — tracked in gw-per-dialect-split.

// ─── OpenAI wire types ────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub(crate) struct OaiChatReq {
    #[serde(default)]
    pub(crate) model: Option<String>,
    pub(crate) messages: Vec<OaiMsg>,
    #[serde(default)]
    pub(crate) tools: Vec<OaiTool>,
    #[serde(default)]
    pub(crate) tool_choice: Value,
    #[serde(default)]
    pub(crate) response_format: Value,
    #[serde(default)]
    pub(crate) stream: Option<bool>,
    pub(crate) temperature: Option<f32>,
    pub(crate) top_p: Option<f32>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) top_k: Option<u32>,
    /// OpenAI's reproducibility knob, and the one field `SamplingParams` had a home for while no
    /// dialect ever filled it (BUG-032). Absent from `/v1/responses` on purpose: the Responses API
    /// does not define `seed`, so adding it there would invent a parameter rather than honour one.
    pub(crate) seed: Option<u64>,
    /// `{"include_usage": true}` — the client asking for token counts in the STREAM. Unread until
    /// BUG-033, when this endpoint was the only one of the three that reported no usage while
    /// streaming at all.
    #[serde(default)]
    pub(crate) stream_options: Option<StreamOptions>,
    /// Not an OpenAI parameter — the vLLM/TGI extension, accepted for the same reason `top_k`
    /// already is: it is what OpenAI-compatible local servers speak. Until BUG-036 `SamplingParams`
    /// had the field and the MLX sampler honoured it, with no way for any client to ask.
    ///
    /// **It costs batching.** A job with a penalty is decoded on the serial path (the penalty is
    /// per-sequence over recent tokens), so this is a real throughput trade the client is making,
    /// not a free knob.
    pub(crate) repetition_penalty: Option<f32>,
    /// `stop`: a bare string or an array of them, up to four per OpenAI. Until BUG-037 there was no
    /// field for this ANYWHERE — not on the wire and not in `SamplingParams` — so a client asking
    /// to stop at a marker generated straight past it.
    #[serde(default)]
    pub(crate) stop: Value,
}

#[derive(Deserialize, Default)]
pub(crate) struct StreamOptions {
    #[serde(default)]
    pub(crate) include_usage: bool,
}

#[derive(Deserialize)]
pub(crate) struct OaiMsg {
    pub(crate) role: String,
    /// String, array of content blocks, or null (for tool-call-only turns).
    #[serde(default)]
    pub(crate) content: Value,
    #[serde(default)]
    pub(crate) tool_calls: Vec<OaiToolCall>,
    pub(crate) tool_call_id: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct OaiTool {
    pub(crate) function: OaiFn,
}

#[derive(Deserialize)]
pub(crate) struct OaiFn {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) parameters: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct OaiToolCall {
    pub(crate) id: String,
    pub(crate) function: OaiFnCall,
}

#[derive(Deserialize)]
pub(crate) struct OaiFnCall {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) arguments: String,
}

pub(crate) fn oai_content_to_blocks(content: &Value, tool_calls: &[OaiToolCall]) -> Vec<ContentBlock> {
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
                    // OpenAI vision: {"type":"image_url","image_url":{"url":"data:...;base64,..."}}
                    Some("image_url") => {
                        if let Some(data) =
                            item["image_url"]["url"].as_str().and_then(decode_data_uri_image)
                        {
                            blocks.push(ContentBlock::Image { data });
                        }
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

pub(crate) fn oai_messages_to_internal(msgs: &[OaiMsg]) -> Vec<Message> {
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

pub(crate) fn oai_tools_to_internal(tools: &[OaiTool]) -> Vec<ToolDef> {
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

// ─── tool_choice (Contract-1) ─────────────────────────────────────────────────

/// Normalized tool-choice across the OpenAI / Anthropic wire formats. We honor it by
/// transforming the tool set the backend sees (no SPI change): `None` removes all tools,
/// `Named` restricts to that one tool. `Auto` (the default) and `Required` leave the set
/// intact — `Required` is accepted but enforcement is best-effort (the model is not forced
/// to start a call), so it is documented as such, not silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) enum ToolChoice {
    #[default]
    Auto,
    None,
    Required,
    Named(String),
}

/// Parse the OpenAI / Responses `tool_choice` value (string `auto`/`none`/`required`, or
/// `{"type":"function","function":{"name":…}}` / flat `{"type":"function","name":…}`).
pub(crate) fn parse_oai_tool_choice(v: &Value) -> ToolChoice {
    match v {
        Value::String(s) => match s.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Value::Object(_) => {
            // name may be nested under `function` (chat) or flat (responses).
            let name = v
                .get("function")
                .and_then(|f| f.get("name"))
                .or_else(|| v.get("name"))
                .and_then(Value::as_str);
            match name {
                Some(n) => ToolChoice::Named(n.to_string()),
                None => ToolChoice::Auto,
            }
        }
        _ => ToolChoice::Auto,
    }
}

// ─── OpenAI SSE serialization ─────────────────────────────────────────────────

/// Accumulated state while streaming tool calls for the OAI SSE format.
pub(crate) struct OaiToolState {
    index: usize,
    #[allow(dead_code)]
    id: String,
    #[allow(dead_code)]
    name: String,
    args: String,
}

pub(crate) fn oai_chunk(completion_id: &str, model: &str, delta: Value, finish_reason: Option<&str>) -> Event {
    oai_chunk_with_usage(completion_id, model, delta, finish_reason, false)
}

/// A chunk, plus the `usage` key when the client asked for usage in the stream.
///
/// OpenAI's shape is specific: with `stream_options.include_usage`, EVERY ordinary chunk carries
/// `"usage": null` and one extra chunk at the end carries the real numbers with an empty `choices`.
/// The null is not decoration — it is how a client tells "this build does not report usage" from
/// "this chunk is not the one that does" (BUG-033).
pub(crate) fn oai_chunk_with_usage(
    completion_id: &str,
    model: &str,
    delta: Value,
    finish_reason: Option<&str>,
    include_usage: bool,
) -> Event {
    let mut data = json!({
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
    if include_usage {
        data["usage"] = Value::Null;
    }
    Event::default().data(data.to_string())
}

/// The extra final chunk: no choices, the whole request's token counts.
pub(crate) fn oai_usage_chunk(completion_id: &str, model: &str, input: u32, output: u32) -> Event {
    let data = json!({
        "id": completion_id,
        "object": "chat.completion.chunk",
        "created": now_secs(),
        "model": model,
        "choices": [],
        "usage": {
            "prompt_tokens": input,
            "completion_tokens": output,
            "total_tokens": input + output,
        }
    });
    Event::default().data(data.to_string())
}

pub(crate) fn oai_sse_stream(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: String,
    lease: Option<ChatLease>,
    include_usage: bool,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let completion_id = new_id("chatcmpl");
    async_stream::stream! {
        let mut events = CancelOnDrop { stream: chat_stream, cancel, _lease: lease };
        let mut tool: Option<OaiToolState> = None;
        let mut role_sent = false;

        while let Some(ev) = events.next().await {
            match ev {
                Ok(ChatEvent::TextDelta { text }) => {
                    // Send role on first delta
                    if !role_sent {
                        yield Ok(oai_chunk_with_usage(&completion_id, &model,
                            json!({"role": "assistant", "content": ""}), None, include_usage));
                        role_sent = true;
                    }
                    yield Ok(oai_chunk_with_usage(&completion_id, &model,
                        json!({"content": text}), None, include_usage));
                }

                Ok(ChatEvent::ToolUseStart { id, name }) => {
                    if !role_sent {
                        yield Ok(oai_chunk_with_usage(&completion_id, &model,
                            json!({"role": "assistant", "content": null}), None, include_usage));
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
                    yield Ok(oai_chunk_with_usage(&completion_id, &model, delta, None, include_usage));
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
                        yield Ok(oai_chunk_with_usage(&completion_id, &model, delta, None, include_usage));
                        if let Some(ref mut t) = tool {
                            t.args.push_str(&input_json_delta);
                        }
                    }
                }

                Ok(ChatEvent::ToolUseEnd { .. }) => {
                    // Tool args complete; stop_reason will come with Done
                }

                Ok(ChatEvent::Done { stop_reason, input_tokens, output_tokens }) => {
                    let finish = match stop_reason {
                        StopReason::ToolUse => "tool_calls",
                        StopReason::MaxTokens => "length",
                        StopReason::Cancelled => "stop",
                        StopReason::EndTurn => "stop",
                    };
                    yield Ok(oai_chunk_with_usage(&completion_id, &model, json!({}), Some(finish), include_usage));
                    // The extra chunk, and ONLY when asked for: a client that never sent
                    // `stream_options` must keep receiving exactly the frames it always has.
                    if include_usage {
                        yield Ok(oai_usage_chunk(&completion_id, &model, input_tokens, output_tokens));
                    }
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

pub(crate) async fn oai_collect(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: &str,
    lease: Option<ChatLease>,
) -> Response {
    let completion_id = new_id("chatcmpl");
    let mut events = CancelOnDrop {
        stream: chat_stream,
        cancel,
        _lease: lease,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_in_the_stream_is_opt_in_and_absence_means_no() {
        // BUG-033. The default is the load-bearing half: a client that never sent `stream_options`
        // must keep receiving exactly the frames it always received, so "absent" and
        // "include_usage: false" both have to mean no.
        let parse = |v: serde_json::Value| -> OaiChatReq { serde_json::from_value(v).unwrap() };
        let base = serde_json::json!({"model": "m", "messages": [], "stream": true});

        let mut asked = base.clone();
        asked["stream_options"] = serde_json::json!({"include_usage": true});
        assert!(parse(asked).stream_options.is_some_and(|o| o.include_usage));

        let mut declined = base.clone();
        declined["stream_options"] = serde_json::json!({"include_usage": false});
        assert!(!parse(declined).stream_options.is_some_and(|o| o.include_usage));

        // Absent entirely, and present-but-empty (a client that sent the object and nothing in it).
        assert!(parse(base.clone()).stream_options.is_none());
        let mut empty = base;
        empty["stream_options"] = serde_json::json!({});
        assert!(!parse(empty).stream_options.is_some_and(|o| o.include_usage));
    }

    #[test]
    fn a_client_seed_survives_parsing_and_an_absent_one_stays_absent() {
        // BUG-032. `SamplingParams` has carried a `seed` field all along and no dialect ever filled
        // it, so `apply_determinism`'s "a caller that genuinely sent its own seed keeps it"
        // described a branch no HTTP client could reach. The distinction below is the whole
        // mechanism: `None` is what lets `ROZUM_SAMPLING_SEED` fill in, and `Some` is what
        // overrides it — inventing a default here would silently take the env's turn away.
        let req: OaiChatReq = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "seed": 4242,
        }))
        .expect("a documented OpenAI body must parse");
        assert_eq!(req.seed, Some(4242));

        let bare: OaiChatReq = serde_json::from_value(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .unwrap();
        assert_eq!(bare.seed, None);
    }
}
