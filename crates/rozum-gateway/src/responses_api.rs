//! OpenAI Responses API dialect (`POST /v1/responses`, the Codex protocol): wire DTOs, input→internal
//! mapping, and the typed Responses SSE/collect serialization. Extracted from `gateway.rs`
//! (gw-per-dialect-split). The HANDLER (`responses_handler`) stays in `gateway.rs` (composition root
//! owning `GatewayState`). This is the stateless dialect layer the handler delegates to.

use std::convert::Infallible;

use axum::http::StatusCode;
use axum::response::{sse::Event, IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use crate::backend::{ChatEvent, ChatStream, ContentBlock, Message, Role, StopReason, ToolDef};
use crate::codex_lean::codex_lean_keep;
use crate::codex_patch::{normalize_codex_tool_args, rewrite_apply_patch_function_args};
use crate::gateway::{
    error_json, new_id, now_secs, CancelOnDrop, ChatLease,
};

// ─── OpenAI Responses API (POST /v1/responses) ───────────────────────────────
//
// The wire protocol Codex CLI (>= 0.137) speaks: a different request shape
// (`input` items + `instructions` + flat `tools`) and a typed SSE event stream
// (`response.created` → `response.output_item.added` → `response.output_text.delta`
// / `response.function_call_arguments.delta` → `response.output_item.done` →
// `response.completed`). We translate to/from the internal `ChatBackend` and reuse
// the same backend stream as `/v1/chat/completions`. Stateless: Codex sends the
// full conversation in `input` each turn (`store:false`), so no server storage.

#[derive(Deserialize)]
pub(crate) struct RespReq {
    #[serde(default)]
    pub(crate) model: Option<String>,
    /// System / developer prompt (prepended as a system message).
    #[serde(default)]
    pub(crate) instructions: Option<String>,
    /// A bare string, or an array of typed input items (messages, function_call,
    /// function_call_output, reasoning, …).
    #[serde(default)]
    pub(crate) input: Value,
    #[serde(default)]
    pub(crate) tools: Vec<RespTool>,
    #[serde(default)]
    pub(crate) tool_choice: Value,
    #[serde(default)]
    pub(crate) stream: Option<bool>,
    pub(crate) temperature: Option<f32>,
    pub(crate) top_p: Option<f32>,
    pub(crate) max_output_tokens: Option<u32>,
    pub(crate) top_k: Option<u32>,
    /// OpenAI Responses reasoning control: `{ "effort": "low"|"medium"|"high" }`. Honoured for
    /// gpt-oss (overrides the `ROZUM_GPTOSS_REASONING` default); ignored by other models.
    #[serde(default)]
    pub(crate) reasoning: Option<Value>,
    /// Responses' structured-output request: `text: { format: { type, schema, … } }`. Unread until
    /// BUG-034, so a constrained-decode request was answered with unconstrained output and a 200 —
    /// while the same capability worked on `/v1/chat/completions` via `response_format`.
    #[serde(default)]
    pub(crate) text: Value,
    /// The same vLLM/TGI extension the Chat dialect accepts, and for the same reason `top_k` is
    /// already here (BUG-036). Costs batching — see `OaiChatReq::repetition_penalty`.
    pub(crate) repetition_penalty: Option<f32>,
    /// See `OaiChatReq::unknown` (BUG-038). This dialect is the one with open questions attached —
    /// whether codex sends `previous_response_id` was unanswerable by reading, and this answers it
    /// from real traffic.
    #[serde(flatten)]
    pub(crate) unknown: serde_json::Map<String, Value>,
}

/// The `effort` out of an OpenAI Responses `reasoning` object, lower-cased + validated.
pub(crate) fn reasoning_effort_of(reasoning: &Option<Value>) -> Option<String> {
    let e = reasoning.as_ref()?.get("effort")?.as_str()?.trim().to_ascii_lowercase();
    matches!(e.as_str(), "low" | "medium" | "high").then_some(e)
}

/// Responses-API tools are FLAT (`{type:"function", name, description, parameters}`),
/// unlike chat-completions (`{type, function:{…}}`).
#[derive(Deserialize)]
pub(crate) struct RespTool {
    #[serde(default, rename = "type")]
    pub(crate) kind: Option<String>,
    #[serde(default)]
    pub(crate) name: Option<String>,
    #[serde(default)]
    pub(crate) description: Option<String>,
    #[serde(default)]
    pub(crate) parameters: Option<Value>,
}

pub(crate) fn responses_content_to_blocks(content: &Value) -> Vec<ContentBlock> {
    match content {
        Value::String(s) if !s.is_empty() => vec![ContentBlock::Text { text: s.clone() }],
        Value::Array(arr) => arr
            .iter()
            .filter_map(|c| match c.get("type").and_then(Value::as_str) {
                // input_text (user), output_text (prior assistant), or plain text.
                Some("input_text") | Some("output_text") | Some("text") => c
                    .get("text")
                    .and_then(Value::as_str)
                    .map(|t| ContentBlock::Text { text: t.to_owned() }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub(crate) fn responses_input_to_internal(instructions: Option<&str>, input: &Value) -> Vec<Message> {
    // Many chat templates (incl. Qwen3.6) require a SINGLE system message that is
    // the very first message, else they `raise_exception('System message must be at
    // the beginning.')`. Codex sends both a top-level `instructions` AND a
    // `developer` message — two system turns — so fold all system/developer text
    // into one leading system message and keep the rest in order.
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(instr) = instructions {
        if !instr.is_empty() {
            system_parts.push(instr.to_owned());
        }
    }
    let mut rest: Vec<Message> = Vec::new();
    let text_of = |blocks: &[ContentBlock]| -> String {
        blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    };
    match input {
        Value::String(s) if !s.is_empty() => rest.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Text { text: s.clone() }],
        }),
        Value::Array(items) => {
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    // A normal message turn (type may be omitted → treat as message).
                    Some("message") | None => {
                        let content = responses_content_to_blocks(&item["content"]);
                        if content.is_empty() {
                            continue;
                        }
                        match item.get("role").and_then(Value::as_str) {
                            // System/developer fold into the single leading system msg.
                            Some("system") | Some("developer") => {
                                system_parts.push(text_of(&content));
                            }
                            Some("assistant") => rest.push(Message {
                                role: Role::Assistant,
                                content,
                            }),
                            _ => rest.push(Message {
                                role: Role::User,
                                content,
                            }),
                        }
                    }
                    // A prior assistant tool call.
                    Some("function_call") => {
                        let id = item
                            .get("call_id")
                            .or_else(|| item.get("id"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let name = item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let args = item.get("arguments").and_then(Value::as_str).unwrap_or("");
                        let input_val: Value = serde_json::from_str(args).unwrap_or(Value::Null);
                        rest.push(Message {
                            role: Role::Assistant,
                            content: vec![ContentBlock::ToolUse {
                                id,
                                name,
                                input: input_val,
                            }],
                        });
                    }
                    // The result of a prior tool call.
                    Some("function_call_output") => {
                        let id = item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned();
                        let out = match item.get("output") {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => String::new(),
                        };
                        rest.push(Message {
                            role: Role::Tool,
                            content: vec![ContentBlock::ToolResult {
                                tool_use_id: id,
                                content: out,
                                is_error: false,
                            }],
                        });
                    }
                    // reasoning / item_reference / etc. — not needed for inference.
                    _ => {}
                }
            }
        }
        _ => {}
    }
    // One leading system message (if any), then the rest in order.
    let mut msgs = Vec::with_capacity(rest.len() + 1);
    if !system_parts.is_empty() {
        msgs.push(Message {
            role: Role::System,
            content: vec![ContentBlock::Text {
                text: system_parts.join("\n\n"),
            }],
        });
    }
    msgs.extend(rest);
    msgs
}

pub(crate) fn responses_tools_to_internal(tools: &[RespTool]) -> Vec<ToolDef> {
    // Default ON: a local model drowns in codex's 18-tool / 21 KB surface (validated: lifts the
    // codex `fix` reds 0→5/5 with Method B). Disable with `ROZUM_CODEX_LEAN=0`.
    let lean = std::env::var("ROZUM_CODEX_LEAN").map(|v| v != "0").unwrap_or(true);
    tools
        .iter()
        .filter(|t| t.kind.as_deref().unwrap_or("function") == "function" && t.name.is_some())
        .filter(|t| !lean || codex_lean_keep(t.name.as_deref().unwrap_or("")))
        .map(|t| ToolDef {
            name: t.name.clone().unwrap_or_default(),
            description: t.description.clone().unwrap_or_default(),
            input_schema: t
                .parameters
                .clone()
                .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
        })
        .collect()
}

pub(crate) fn codex_tool_capture_enabled() -> bool {
    std::env::var("ROZUM_CODEX_TOOL_CAPTURE")
        .map(|v| v != "0")
        .unwrap_or(false)
}

pub(crate) fn codex_tool_capture_max_bytes() -> usize {
    std::env::var("ROZUM_CODEX_TOOL_CAPTURE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(65_536)
}

pub(crate) fn capture_text_json(text: &str, max_bytes: usize) -> Value {
    let bytes = text.len();
    let truncated = max_bytes > 0 && bytes > max_bytes;
    let text = if truncated {
        let mut end = max_bytes;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    } else {
        text
    };
    json!({ "text": text, "bytes": bytes, "truncated": truncated })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn codex_tool_call_capture_json(
    source: &str,
    response_id: &str,
    call_id: &str,
    raw_name: &str,
    emitted_name: &str,
    raw_args: &str,
    final_args: &str,
    reroute_apply_patch: bool,
    apply_patch_is_tool: bool,
) -> Value {
    let cap = codex_tool_capture_max_bytes();
    json!({
        "event": "codex_tool_call",
        "endpoint": "/v1/responses",
        "source": source,
        "response_id": response_id,
        "call_id": call_id,
        "raw_name": raw_name,
        "emitted_name": emitted_name,
        "reroute_apply_patch": reroute_apply_patch,
        "apply_patch_is_tool": apply_patch_is_tool,
        "args_changed": raw_args != final_args,
        "raw_args": capture_text_json(raw_args, cap),
        "final_args": capture_text_json(final_args, cap),
    })
}

pub(crate) fn log_codex_tool_inventory(
    model: Option<&str>,
    stream: bool,
    raw_tools: &[RespTool],
    backend_tools: &[ToolDef],
    apply_patch_is_tool: bool,
    inject_apply_patch: bool,
) {
    if !codex_tool_capture_enabled() {
        return;
    }
    let raw_tool_names: Vec<_> = raw_tools.iter().filter_map(|t| t.name.as_deref()).collect();
    let backend_tool_names: Vec<_> = backend_tools.iter().map(|t| t.name.as_str()).collect();
    crate::obs::log_event(json!({
        "event": "codex_tool_inventory",
        "endpoint": "/v1/responses",
        "model": model.unwrap_or("?"),
        "stream": stream,
        "raw_tools": raw_tool_names,
        "backend_tools": backend_tool_names,
        "apply_patch_is_tool": apply_patch_is_tool,
        "inject_apply_patch": inject_apply_patch,
    }));
}

pub(crate) fn log_codex_tool_call_capture(event: Value) {
    if codex_tool_capture_enabled() {
        crate::obs::log_event(event);
    }
}

/// Build one typed Responses SSE event: stamps `type` + a monotonic
/// `sequence_number` into the payload and sets the SSE `event:` name.
pub(crate) fn resp_event(seq: &mut u64, typ: &str, mut data: Value) -> Event {
    if let Value::Object(ref mut m) = data {
        m.insert("type".into(), json!(typ));
        m.insert("sequence_number".into(), json!(*seq));
    }
    *seq += 1;
    Event::default().event(typ).data(data.to_string())
}

/// The three events that close an assistant `message` output item (text done →
/// content part done → item done). Returned as a Vec so the caller can `yield`
/// each lexically inside the `async_stream` body (a `yield` hidden in a
/// `macro_rules!` would not be seen by the `stream!` proc-macro).
pub(crate) fn close_message_events(seq: &mut u64, mid: &str, output_index: usize, text: &str) -> Vec<Event> {
    vec![
        resp_event(
            seq,
            "response.output_text.done",
            json!({
                "item_id": mid, "output_index": output_index, "content_index": 0, "text": text,
            }),
        ),
        resp_event(
            seq,
            "response.content_part.done",
            json!({
                "item_id": mid, "output_index": output_index, "content_index": 0,
                "part": {"type": "output_text", "text": text, "annotations": []},
            }),
        ),
        resp_event(
            seq,
            "response.output_item.done",
            json!({
                "output_index": output_index,
                "item": {"type": "message", "id": mid, "status": "completed", "role": "assistant",
                         "content": [{"type": "output_text", "text": text, "annotations": []}]},
            }),
        ),
    ]
}

/// The final `response` object (shared by the streaming `response.completed` event
/// and the non-streaming body).
pub(crate) fn responses_object(
    id: &str,
    created: u64,
    model: &str,
    status: &str,
    output: Value,
    input_tokens: u32,
    output_tokens: u32,
) -> Value {
    json!({
        "id": id,
        "object": "response",
        "created_at": created,
        "status": status,
        "model": model,
        "output": output,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "total_tokens": input_tokens + output_tokens,
        },
        "error": null,
        "incomplete_details": null,
        "metadata": {},
        "parallel_tool_calls": true,
    })
}

/// Stream the internal `ChatEvent`s as the typed Responses SSE protocol. Our
/// backend emits text deltas first, then (at finalization) whole tool calls, then
/// `Done` — which maps cleanly onto a `message` output item followed by
/// `function_call` items.
pub(crate) fn responses_sse_stream(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: String,
    lease: Option<ChatLease>,
    apply_patch_is_tool: bool,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let response_id = new_id("resp");
    let created = now_secs();
    async_stream::stream! {
        let mut events = CancelOnDrop { stream: chat_stream, cancel, _lease: lease };
        let mut seq = 0u64;
        let mut next_index = 0usize;

        // Message (assistant text) item state.
        let mut msg_id: Option<String> = None;
        let mut msg_index = 0usize;
        let mut msg_closed = false;
        let mut text = String::new();

        // Tool-call items, completed (for the final output[]).
        let mut tool_items: Vec<Value> = Vec::new();
        // The currently-open function_call:
        // (fc_id, call_id, raw_name, emitted_name, output_index, raw_args, reroute_ap).
        // reroute_ap = this is an apply_patch function call we re-route to exec_command at End.
        let mut cur_tool: Option<(String, String, String, String, usize, String, bool)> = None;

        yield Ok(resp_event(&mut seq, "response.created", json!({
            "response": responses_object(&response_id, created, &model, "in_progress", json!([]), 0, 0)
        })));

        while let Some(ev) = events.next().await {
            match ev {
                Ok(ChatEvent::TextDelta { text: t }) => {
                    if msg_id.is_none() {
                        let mid = new_id("msg");
                        msg_index = next_index; next_index += 1;
                        yield Ok(resp_event(&mut seq, "response.output_item.added", json!({
                            "output_index": msg_index,
                            "item": {"type": "message", "id": mid, "status": "in_progress",
                                     "role": "assistant", "content": []},
                        })));
                        yield Ok(resp_event(&mut seq, "response.content_part.added", json!({
                            "item_id": mid, "output_index": msg_index, "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        })));
                        msg_id = Some(mid);
                    }
                    text.push_str(&t);
                    let mid = msg_id.clone().unwrap();
                    yield Ok(resp_event(&mut seq, "response.output_text.delta", json!({
                        "item_id": mid, "output_index": msg_index, "content_index": 0, "delta": t,
                    })));
                }

                Ok(ChatEvent::ToolUseStart { id, name }) => {
                    if let Some(mid) = msg_id.clone() {
                        if !msg_closed {
                            msg_closed = true;
                            for e in close_message_events(&mut seq, &mid, msg_index, &text) {
                                yield Ok(e);
                            }
                        }
                    }
                    let fc_id = new_id("fc");
                    let oi = next_index; next_index += 1;
                    // gpt-oss calls `apply_patch` as a function, which codex rejects unless it
                    // offered apply_patch as a tool — re-route to exec_command (rewrite at End).
                    let raw_name = name;
                    let reroute_ap = !apply_patch_is_tool && raw_name == "apply_patch";
                    let emit_name = if reroute_ap { "exec_command".to_string() } else { raw_name.clone() };
                    yield Ok(resp_event(&mut seq, "response.output_item.added", json!({
                        "output_index": oi,
                        "item": {"type": "function_call", "id": fc_id, "call_id": id,
                                 "name": emit_name, "arguments": "", "status": "in_progress"},
                    })));
                    cur_tool = Some((fc_id, id, raw_name, emit_name, oi, String::new(), reroute_ap));
                }

                Ok(ChatEvent::ToolUseDelta { input_json_delta, .. }) => {
                    // Buffer tool-call args (don't stream incrementally) so the apply_patch bridge
                    // at ToolUseEnd can rewrite a malformed unified diff consistently (Finding 4).
                    if let Some((_, _, _, _, _, ref mut args, _)) = cur_tool {
                        args.push_str(&input_json_delta);
                    }
                }

                Ok(ChatEvent::ToolUseEnd { .. }) => {
                    if let Some((fc_id, call_id, raw_name, name, oi, raw_args, reroute_ap)) = cur_tool.take() {
                        // Re-route an apply_patch function call to exec_command (gpt-oss), else
                        // bridge a malformed apply_patch shell command (unified diff → patch), Finding 4.
                        let args = if reroute_ap {
                            rewrite_apply_patch_function_args(&raw_args).unwrap_or_else(|| raw_args.clone())
                        } else {
                            normalize_codex_tool_args(&raw_args)
                        };
                        log_codex_tool_call_capture(codex_tool_call_capture_json(
                            "stream",
                            &response_id,
                            &call_id,
                            &raw_name,
                            &name,
                            &raw_args,
                            &args,
                            reroute_ap,
                            apply_patch_is_tool,
                        ));
                        // Args were buffered above; emit them once (post-bridge) as a single delta.
                        yield Ok(resp_event(&mut seq, "response.function_call_arguments.delta", json!({
                            "item_id": fc_id, "output_index": oi, "delta": args,
                        })));
                        yield Ok(resp_event(&mut seq, "response.function_call_arguments.done", json!({
                            "item_id": fc_id, "output_index": oi, "arguments": args,
                        })));
                        let item = json!({"type": "function_call", "id": fc_id, "call_id": call_id,
                                          "name": name, "arguments": args, "status": "completed"});
                        yield Ok(resp_event(&mut seq, "response.output_item.done", json!({
                            "output_index": oi, "item": item.clone(),
                        })));
                        tool_items.push(item);
                    }
                }

                Ok(ChatEvent::Done { stop_reason, input_tokens, output_tokens }) => {
                    if let Some(mid) = msg_id.clone() {
                        if !msg_closed {
                            // (no need to set msg_closed; we break right after)
                            for e in close_message_events(&mut seq, &mid, msg_index, &text) {
                                yield Ok(e);
                            }
                        }
                    }
                    let status = match stop_reason {
                        StopReason::Cancelled => "incomplete",
                        _ => "completed",
                    };
                    let mut output = Vec::new();
                    if let Some(ref mid) = msg_id {
                        output.push(json!({"type": "message", "id": mid, "status": "completed",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text, "annotations": []}]}));
                    }
                    output.extend(tool_items.clone());
                    yield Ok(resp_event(&mut seq, "response.completed", json!({
                        "response": responses_object(&response_id, created, &model, status,
                            json!(output), input_tokens, output_tokens)
                    })));
                    break;
                }

                Err(e) => {
                    yield Ok(resp_event(&mut seq, "response.failed", json!({
                        "response": responses_object(&response_id, created, &model, "failed",
                            json!([]), 0, 0),
                        "error": {"message": e.to_string()},
                    })));
                    break;
                }
            }
        }
    }
}

/// Non-streaming `/v1/responses`: drain the backend and return the final
/// `response` object with `output[]` + `usage`.
pub(crate) async fn responses_collect(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: &str,
    lease: Option<ChatLease>,
    apply_patch_is_tool: bool,
) -> Response {
    let response_id = new_id("resp");
    let created = now_secs();
    let mut events = CancelOnDrop {
        stream: chat_stream,
        cancel,
        _lease: lease,
    };
    let mut text = String::new();
    let mut output: Vec<Value> = Vec::new();
    // (call_id, raw_name, emitted_name, raw_args, reroute_ap).
    // reroute_ap re-routes apply_patch fn → exec_command.
    let mut cur_tool: Option<(String, String, String, String, bool)> = None;
    let mut status = "completed";
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;

    while let Some(ev) = events.next().await {
        match ev {
            Ok(ChatEvent::TextDelta { text: t }) => text.push_str(&t),
            Ok(ChatEvent::ToolUseStart { id, name }) => {
                let raw_name = name;
                let reroute_ap = !apply_patch_is_tool && raw_name == "apply_patch";
                let emitted_name = if reroute_ap { "exec_command".to_string() } else { raw_name.clone() };
                cur_tool = Some((id, raw_name, emitted_name, String::new(), reroute_ap));
            }
            Ok(ChatEvent::ToolUseDelta {
                input_json_delta, ..
            }) => {
                if let Some((_, _, _, ref mut args, _)) = cur_tool {
                    args.push_str(&input_json_delta);
                }
            }
            Ok(ChatEvent::ToolUseEnd { .. }) => {
                if let Some((call_id, raw_name, name, raw_args, reroute_ap)) = cur_tool.take() {
                    let args = if reroute_ap {
                        rewrite_apply_patch_function_args(&raw_args).unwrap_or_else(|| raw_args.clone())
                    } else {
                        normalize_codex_tool_args(&raw_args)
                    };
                    log_codex_tool_call_capture(codex_tool_call_capture_json(
                        "collect",
                        &response_id,
                        &call_id,
                        &raw_name,
                        &name,
                        &raw_args,
                        &args,
                        reroute_ap,
                        apply_patch_is_tool,
                    ));
                    output.push(json!({"type": "function_call", "id": new_id("fc"),
                        "call_id": call_id, "name": name, "arguments": args, "status": "completed"}));
                }
            }
            Ok(ChatEvent::Done {
                stop_reason,
                input_tokens: i,
                output_tokens: o,
            }) => {
                if matches!(stop_reason, StopReason::Cancelled) {
                    status = "incomplete";
                }
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

    // Assistant message item goes first (Responses order), then tool calls.
    let mut full = Vec::new();
    if !text.is_empty() {
        full.push(
            json!({"type": "message", "id": new_id("msg"), "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}]}),
        );
    }
    full.extend(output);
    let body = responses_object(
        &response_id,
        created,
        model,
        status,
        json!(full),
        input_tokens,
        output_tokens,
    );
    axum::Json(body).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_fields_this_dialect_does_not_handle_are_captured_by_name() {
        // BUG-038. The two open questions from the wire sweep — does codex send
        // `previous_response_id`, does anyone send `store`/`metadata` — were unanswerable by
        // reading the code, because an undeclared field left no trace. Now it leaves one, and the
        // known fields must still parse exactly as before.
        let req: RespReq = serde_json::from_value(serde_json::json!({
            "model": "m",
            "instructions": "be brief",
            "input": "hi",
            "temperature": 0.5,
            "previous_response_id": "resp-abc",
            "store": false,
            "metadata": {"run": "7"},
        }))
        .unwrap();
        assert_eq!(req.model.as_deref(), Some("m"), "known fields are unaffected by the capture");
        assert_eq!(req.temperature, Some(0.5));
        let mut names: Vec<&str> = req.unknown.keys().map(String::as_str).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["metadata", "previous_response_id", "store"]);
    }

    #[test]
    fn a_request_this_dialect_fully_understands_reports_nothing() {
        let req: RespReq = serde_json::from_value(serde_json::json!({
            "model": "m", "input": "hi", "stream": true,
        }))
        .unwrap();
        assert!(req.unknown.is_empty(), "no news is the common case and must stay silent");
    }
}
