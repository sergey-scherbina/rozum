/// HTTP gateway exposing rozum's ChatBackend over two dialects:
///
/// - OpenAI Chat Completions  (`POST /v1/chat/completions`, `GET /v1/models`)
/// - Anthropic Messages       (`POST /v1/messages`)
///
/// Bind address is always `127.0.0.1`. Auth is optional bearer token via
/// `ROZUM_GATEWAY_TOKEN`. Cancel propagates from client disconnect.
use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    /// The resident model behind a swap cell so `switch` / `unload` can replace
    /// it in place (drain → swap → resume) without restarting the process.
    sb: Arc<Switchboard>,
    /// Optional bearer token required on every request.
    auth_token: Option<String>,
    /// Per-request metrics + JSONL event log.
    observer: Arc<crate::obs::Observer>,
    /// Liveness for the shared-daemon idle watchdog.
    activity: Arc<Activity>,
}

/// Tracks request activity so the shared gateway can idle-exit (free the model)
/// only when nothing is in flight and nothing has arrived for a while.
#[derive(Default)]
struct Activity {
    last_active: AtomicU64,
    in_flight: AtomicU64,
}

/// Builds a backend for a model spec / context window / optional backend hint.
/// Async because a model load can take seconds. `None` = nothing could be built
/// (bad spec or load failure). Injected by the binary so the daemon can rebuild
/// in place on `switch` / lazy `unload` reload without the library depending on
/// the binary's backend-selection chain.
pub type BackendBuilder = Arc<
    dyn Fn(
            String,
            u32,
            Option<String>,
        ) -> Pin<Box<dyn Future<Output = Option<Arc<dyn ChatBackend>>> + Send>>
        + Send
        + Sync,
>;

/// Which model/backend the daemon currently serves; mutated on `switch`.
#[derive(Clone)]
struct ModelSpec {
    model_id: String,
    n_ctx: u32,
    backend: Option<String>,
}

/// Holds the resident backend behind a swap cell. `rozum gateway switch` /
/// `unload` drain in-flight work, drop the old model (never two resident —
/// memory), build the new one, bump `generation`, and resume. `reload` (binary
/// upgrade) re-execs instead; this covers in-place model/backend changes.
struct Switchboard {
    /// `None` = unloaded (model freed; the next chat lazily rebuilds from `spec`).
    backend: std::sync::RwLock<Option<Arc<dyn ChatBackend>>>,
    /// Backend factory; `None` on a `--dedicated` gateway (switching disabled).
    builder: Option<BackendBuilder>,
    spec: std::sync::Mutex<ModelSpec>,
    generation: AtomicU64,
    started_at: u64,
    /// Set while a switch/unload finishes in-flight work; chat requests park on
    /// `resume` until it clears so none is served mid-swap.
    draining: AtomicBool,
    resume: tokio::sync::Notify,
    /// Active generations — NOT the idle-watchdog `in_flight`. A drain waits for
    /// this to reach 0; parked/queued requests don't count, so it can't deadlock
    /// on requests that are themselves waiting for the drain to finish.
    generating: AtomicU64,
    /// Serializes lazy reload so racing requests rebuild the model only once.
    reload_lock: tokio::sync::Mutex<()>,
    /// `(pid, port)` when registered, so a switch can republish `active.json`.
    register: Option<(u32, u16)>,
}

/// Held by a chat handler for the whole request (prefill + stream). Keeps the
/// chosen backend alive and counts against `generating` so a `switch` waits for
/// real work to finish before swapping the model.
struct ChatLease {
    backend: Arc<dyn ChatBackend>,
    model_id: String,
    sb: Arc<Switchboard>,
}

impl Drop for ChatLease {
    fn drop(&mut self) {
        self.sb.generating.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Max seconds a switch/unload/reload waits for in-flight generations to finish
/// before giving up (`ROZUM_GATEWAY_DRAIN_SECS`, default 120).
fn drain_secs() -> u64 {
    std::env::var("ROZUM_GATEWAY_DRAIN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(120)
}

/// Idle seconds before the watchdog auto-unloads the resident model (keeping the
/// daemon alive; the next chat lazily reloads). `ROZUM_GATEWAY_UNLOAD_IDLE_SECS`,
/// default 900 (15 min); `0` disables. Spec: `docs/specs/model-unload-on-idle.md`.
fn unload_idle_secs() -> u64 {
    std::env::var("ROZUM_GATEWAY_UNLOAD_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(900)
}

impl Switchboard {
    fn current(&self) -> Option<Arc<dyn ChatBackend>> {
        self.backend.read().unwrap().clone()
    }

    fn model_id(&self) -> String {
        self.spec.lock().unwrap().model_id.clone()
    }

    fn n_ctx(&self) -> u32 {
        self.spec.lock().unwrap().n_ctx
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Bump `generation` and republish the registry so proxies see "the daemon
    /// was reconfigured". Returns the new generation.
    fn bump_and_republish(&self, spec: &ModelSpec) -> u64 {
        let g = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some((pid, port)) = self.register {
            let _ = crate::share::write_active(&crate::share::ActiveGateway {
                model: spec.model_id.clone(),
                port,
                pid,
                n_ctx: spec.n_ctx,
                started_at: self.started_at,
                generation: g,
            });
        }
        g
    }

    /// Return the live backend, lazily rebuilding it once if unloaded.
    async fn ensure_loaded(&self) -> Result<Arc<dyn ChatBackend>, &'static str> {
        if let Some(b) = self.current() {
            return Ok(b);
        }
        let _g = self.reload_lock.lock().await;
        if let Some(b) = self.current() {
            return Ok(b); // a racing request already rebuilt it
        }
        let Some(builder) = self.builder.clone() else {
            return Err("model is unloaded and this gateway cannot reload it");
        };
        let spec = self.spec.lock().unwrap().clone();
        crate::obs::log_event(json!({
            "event": "gateway_lazy_reload", "model": spec.model_id, "n_ctx": spec.n_ctx,
        }));
        match builder(spec.model_id.clone(), spec.n_ctx, spec.backend.clone()).await {
            Some(b) => {
                *self.backend.write().unwrap() = Some(b.clone());
                self.bump_and_republish(&spec);
                Ok(b)
            }
            None => Err("model failed to reload"),
        }
    }

    /// Chat-handler entry: park while a swap drains, lazily reload if unloaded,
    /// then take a `generating` token. Returns a guard the handler holds for the
    /// whole request so the model can't be swapped out from under it.
    async fn enter(self: &Arc<Self>) -> Result<ChatLease, Response> {
        loop {
            // Take a token first, then check `draining`: paired with begin_drain
            // (set draining, then wait for generating==0) this never races a swap
            // into running mid-flight.
            self.generating.fetch_add(1, Ordering::SeqCst);
            if !self.draining.load(Ordering::SeqCst) {
                break;
            }
            self.generating.fetch_sub(1, Ordering::SeqCst);
            tokio::select! {
                _ = self.resume.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        }
        // Token held; load (or lazily rebuild) the backend.
        match self.ensure_loaded().await {
            Ok(backend) => Ok(ChatLease {
                backend,
                model_id: self.model_id(),
                sb: Arc::clone(self),
            }),
            Err(msg) => {
                self.generating.fetch_sub(1, Ordering::SeqCst);
                Err(error_json(
                    StatusCode::SERVICE_UNAVAILABLE,
                    msg,
                    "model_unloaded",
                ))
            }
        }
    }

    /// Stop admitting new chats and wait for in-flight generations to finish.
    /// On timeout, resume and return an error (don't swap under live work).
    async fn begin_drain(&self) -> Result<(), String> {
        self.draining.store(true, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_secs(drain_secs());
        while self.generating.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= deadline {
                self.draining.store(false, Ordering::SeqCst);
                self.resume.notify_waiters();
                return Err("timed out draining in-flight requests".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Ok(())
    }

    fn end_drain(&self) {
        self.draining.store(false, Ordering::SeqCst);
        self.resume.notify_waiters();
    }

    /// In-place model/backend swap: drain → drop old (frees RAM) → build new →
    /// bump generation → resume. Sequential, never two models resident. On build
    /// failure the spec reverts so the next request lazily reloads the old model.
    async fn switch(
        &self,
        model: String,
        n_ctx: Option<u32>,
        backend: Option<String>,
    ) -> Result<u64, String> {
        let Some(builder) = self.builder.clone() else {
            return Err("this gateway does not support switching (dedicated)".into());
        };
        self.begin_drain().await?;
        let old = self.spec.lock().unwrap().clone();
        let new = ModelSpec {
            model_id: model.clone(),
            n_ctx: n_ctx.unwrap_or(old.n_ctx),
            backend,
        };
        // Drop the current model first so the new one never coexists with it.
        *self.backend.write().unwrap() = None;
        *self.spec.lock().unwrap() = new.clone();
        crate::obs::log_event(json!({
            "event": "gateway_switch_start", "from": old.model_id, "to": new.model_id, "n_ctx": new.n_ctx,
        }));
        let out = match builder(new.model_id.clone(), new.n_ctx, new.backend.clone()).await {
            Some(b) => {
                *self.backend.write().unwrap() = Some(b);
                let g = self.bump_and_republish(&new);
                crate::obs::log_event(json!({
                    "event": "gateway_switch_done", "model": new.model_id, "generation": g,
                }));
                Ok(g)
            }
            None => {
                // Revert so a lazy reload restores the previous model on demand.
                *self.spec.lock().unwrap() = old;
                crate::obs::log_event(json!({
                    "event": "gateway_switch_failed", "model": new.model_id,
                }));
                Err(format!("failed to load model '{model}'"))
            }
        };
        self.end_drain();
        out
    }

    /// True while a model is resident (vs freed by `unload`/idle-unload).
    fn is_loaded(&self) -> bool {
        self.backend.read().unwrap().is_some()
    }

    /// True when the model can be rebuilt in process (has a builder). A
    /// `--dedicated` gateway returns false — it must never auto-unload.
    fn can_reload(&self) -> bool {
        self.builder.is_some()
    }

    /// Free the resident model but keep the daemon listening; the next chat
    /// lazily reloads it. Frees RAM while idle.
    async fn unload(&self) -> Result<u64, String> {
        if self.builder.is_none() {
            return Err("this gateway cannot reload after unload (dedicated)".into());
        }
        self.begin_drain().await?;
        *self.backend.write().unwrap() = None;
        let spec = self.spec.lock().unwrap().clone();
        let g = self.bump_and_republish(&spec);
        crate::obs::log_event(json!({
            "event": "gateway_unloaded", "model": spec.model_id, "generation": g,
        }));
        self.end_drain();
        Ok(g)
    }
}

/// Options for [`serve_on`]. Defaults (no register, no idle, no builder) preserve
/// the plain in-process gateway behaviour used by `rozum launch --dedicated`.
#[derive(Default)]
pub struct ServeConfig {
    /// Idle-exit after this many seconds with zero in-flight requests and no new
    /// arrivals (the shared daemon). `None`/`0` = never idle-exit.
    pub idle_secs: Option<u64>,
    /// When set, publish/remove the shared-gateway registry (`share::active.json`).
    pub register_n_ctx: Option<u32>,
    /// Backend factory enabling in-place `switch` / lazy `unload` reload. `None`
    /// disables those (the model can't be rebuilt in process).
    pub builder: Option<BackendBuilder>,
    /// Optional backend hint recorded with the spec (so a lazy reload rebuilds
    /// the same way). Mirrors `switch --backend`.
    pub backend_hint: Option<String>,
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
    /// Kept alive for the whole stream so a `switch` waits for streaming to
    /// finish (the lease counts against `generating`) before swapping the model.
    _lease: Option<ChatLease>,
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
    lease: Option<ChatLease>,
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

async fn oai_collect(
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
struct RespReq {
    #[serde(default)]
    model: Option<String>,
    /// System / developer prompt (prepended as a system message).
    #[serde(default)]
    instructions: Option<String>,
    /// A bare string, or an array of typed input items (messages, function_call,
    /// function_call_output, reasoning, …).
    #[serde(default)]
    input: Value,
    #[serde(default)]
    tools: Vec<RespTool>,
    #[serde(default)]
    stream: Option<bool>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    max_output_tokens: Option<u32>,
    top_k: Option<u32>,
}

/// Responses-API tools are FLAT (`{type:"function", name, description, parameters}`),
/// unlike chat-completions (`{type, function:{…}}`).
#[derive(Deserialize)]
struct RespTool {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<Value>,
}

fn responses_content_to_blocks(content: &Value) -> Vec<ContentBlock> {
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

fn responses_input_to_internal(instructions: Option<&str>, input: &Value) -> Vec<Message> {
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

fn responses_tools_to_internal(tools: &[RespTool]) -> Vec<ToolDef> {
    tools
        .iter()
        .filter(|t| t.kind.as_deref().unwrap_or("function") == "function" && t.name.is_some())
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

/// Build one typed Responses SSE event: stamps `type` + a monotonic
/// `sequence_number` into the payload and sets the SSE `event:` name.
fn resp_event(seq: &mut u64, typ: &str, mut data: Value) -> Event {
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
fn close_message_events(seq: &mut u64, mid: &str, output_index: usize, text: &str) -> Vec<Event> {
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
fn responses_object(
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

async fn anthropic_collect(
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

// ─── Route handlers ───────────────────────────────────────────────────────────

async fn stats_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let mut snap = state.observer.snapshot();
    if let Value::Object(ref mut m) = snap {
        m.insert("model".into(), json!(state.sb.model_id()));
        m.insert("generation".into(), json!(state.sb.generation()));
        let ctx = state
            .sb
            .current()
            .map(|b| b.context_window())
            .unwrap_or(state.sb.n_ctx());
        m.insert("context_window".into(), json!(ctx));
        m.insert("loaded".into(), json!(state.sb.current().is_some()));
    }
    axum::Json(snap)
}

/// Cheap admission probe for the two-tier backpressure: a launch-local proxy
/// reads this to learn the daemon's free window and decide whether to forward a
/// queued request now or hold it at the edge. Ungated backends report a generous
/// always-free window so the proxy fails open. (`shared-gateway.md`.)
async fn admit_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    // While a switch/unload is in progress (draining or model dropped), advertise
    // no free window so proxies hold their queued requests at the edge — that's
    // how the swap stays transparent (the gap looks like backpressure).
    if state.sb.draining.load(Ordering::SeqCst) || state.sb.current().is_none() {
        return axum::Json(json!({ "limit": 1, "in_use": 1, "waiting": 0, "free": 0 }));
    }
    match state.sb.current().and_then(|b| b.admission_stats()) {
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
    let model_id = state.sb.model_id();
    let claude_alias = claude_model_alias(&model_id);
    let entry = json!({
        "id": claude_alias,
        "object": "model",
        "created": now_secs(),
        "owned_by": "rozum",
        "display_name": model_id,
    });
    // `data` is the OpenAI shape (the real model). `models` is the key Codex's
    // model-list refresh requires — but each Codex `Model` entry has many required
    // fields (`slug`, `supported_reasoning_levels`, …) we'd have to track. We force
    // `-m local` via the launch, so the list is unused; return an EMPTY `models` so
    // Codex finds the key and validates zero entries (no "missing field" warning),
    // while OpenAI clients keep using `data`.
    axum::Json(json!({
        "object": "list",
        "data": [entry],
        "models": [],
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
        stream = req.stream.unwrap_or(false),
        "POST /v1/chat/completions"
    );
    // Hold a lease for the whole request so a `switch` can't swap the model
    // mid-flight; parks here if a swap is draining, lazily reloads if unloaded.
    let lease = match state.sb.enter().await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let messages = oai_messages_to_internal(&req.messages);
    let tools = oai_tools_to_internal(&req.tools);

    // Approximate context overflow check
    let prompt_text = total_message_text(&messages);
    let est = estimate_tokens(&prompt_text);
    let ctx_win = lease.backend.context_window();
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

    let model = req.model.unwrap_or_else(|| lease.model_id.clone());
    // OpenAI/Anthropic spec default for an absent `stream` is non-streaming JSON.
    // (Streaming clients — CC, Codex — always send `stream:true` explicitly.)
    let stream_mode = req.stream.unwrap_or(false);

    match lease.backend.chat(chat_req).await {
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
                Sse::new(oai_sse_stream(chat_stream, cancel, model, Some(lease))).into_response()
            } else {
                oai_collect(chat_stream, cancel, &model, Some(lease)).await
            }
        }
    }
}

async fn responses_handler(
    State(state): State<GatewayState>,
    axum::Json(req): axum::Json<RespReq>,
) -> Response {
    tracing::debug!(
        model = req.model.as_deref().unwrap_or("?"),
        tools = req.tools.len(),
        stream = req.stream.unwrap_or(false),
        "POST /v1/responses"
    );
    let lease = match state.sb.enter().await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let messages = responses_input_to_internal(req.instructions.as_deref(), &req.input);
    let tools = responses_tools_to_internal(&req.tools);

    let prompt_text = total_message_text(&messages);
    let est = estimate_tokens(&prompt_text);
    let ctx_win = lease.backend.context_window();
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
            max_tokens: req.max_output_tokens,
            top_k: req.top_k,
            ..Default::default()
        },
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = req.model.unwrap_or_else(|| lease.model_id.clone());
    let stream_mode = req.stream.unwrap_or(false);

    match lease.backend.chat(chat_req).await {
        Err(e) => {
            crate::obs::log_event(json!({
                "event": "request_error", "endpoint": "/v1/responses", "error": e.to_string(),
            }));
            chat_error_response(&e, "backend_error")
        }
        Ok(chat_stream) => {
            let meta = crate::obs::ReqMeta {
                endpoint: "/v1/responses",
                model: model.clone(),
                n_messages,
                n_tools,
                est_prompt_tokens: est,
            };
            let chat_stream = crate::obs::meter(chat_stream, state.observer.clone(), meta);
            if stream_mode {
                Sse::new(responses_sse_stream(
                    chat_stream,
                    cancel,
                    model,
                    Some(lease),
                ))
                .into_response()
            } else {
                responses_collect(chat_stream, cancel, &model, Some(lease)).await
            }
        }
    }
}

/// Stream the internal `ChatEvent`s as the typed Responses SSE protocol. Our
/// backend emits text deltas first, then (at finalization) whole tool calls, then
/// `Done` — which maps cleanly onto a `message` output item followed by
/// `function_call` items.
fn responses_sse_stream(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: String,
    lease: Option<ChatLease>,
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
        // The currently-open function_call: (fc_id, call_id, name, output_index, args).
        let mut cur_tool: Option<(String, String, String, usize, String)> = None;

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
                    yield Ok(resp_event(&mut seq, "response.output_item.added", json!({
                        "output_index": oi,
                        "item": {"type": "function_call", "id": fc_id, "call_id": id,
                                 "name": name, "arguments": "", "status": "in_progress"},
                    })));
                    cur_tool = Some((fc_id, id, name, oi, String::new()));
                }

                Ok(ChatEvent::ToolUseDelta { input_json_delta, .. }) => {
                    if let Some((ref fc_id, _, _, oi, ref mut args)) = cur_tool {
                        args.push_str(&input_json_delta);
                        yield Ok(resp_event(&mut seq, "response.function_call_arguments.delta", json!({
                            "item_id": fc_id, "output_index": oi, "delta": input_json_delta,
                        })));
                    }
                }

                Ok(ChatEvent::ToolUseEnd { .. }) => {
                    if let Some((fc_id, call_id, name, oi, args)) = cur_tool.take() {
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
async fn responses_collect(
    chat_stream: ChatStream,
    cancel: CancellationToken,
    model: &str,
    lease: Option<ChatLease>,
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
    let mut cur_tool: Option<(String, String, String)> = None; // (call_id, name, args)
    let mut status = "completed";
    let mut input_tokens = 0u32;
    let mut output_tokens = 0u32;

    while let Some(ev) = events.next().await {
        match ev {
            Ok(ChatEvent::TextDelta { text: t }) => text.push_str(&t),
            Ok(ChatEvent::ToolUseStart { id, name }) => {
                cur_tool = Some((id, name, String::new()));
            }
            Ok(ChatEvent::ToolUseDelta {
                input_json_delta, ..
            }) => {
                if let Some((_, _, ref mut args)) = cur_tool {
                    args.push_str(&input_json_delta);
                }
            }
            Ok(ChatEvent::ToolUseEnd { .. }) => {
                if let Some((call_id, name, args)) = cur_tool.take() {
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

async fn anthropic_handler(
    State(state): State<GatewayState>,
    axum::Json(req): axum::Json<AnthropicReq>,
) -> Response {
    tracing::debug!(
        model = req.model.as_deref().unwrap_or("?"),
        msgs = req.messages.len(),
        tools = req.tools.len(),
        stream = req.stream.unwrap_or(false),
        "POST /v1/messages"
    );
    let lease = match state.sb.enter().await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let messages = anthropic_messages_to_internal(req.system.as_ref(), &req.messages);
    let tools = anthropic_tools_to_internal(&req.tools);

    // Approximate context overflow check
    let prompt_text = total_message_text(&messages);
    let est = estimate_tokens(&prompt_text);
    let ctx_win = lease.backend.context_window();
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

    let model = req.model.unwrap_or_else(|| lease.model_id.clone());
    // OpenAI/Anthropic spec default for an absent `stream` is non-streaming JSON.
    // (Streaming clients — CC, Codex — always send `stream:true` explicitly.)
    let stream_mode = req.stream.unwrap_or(false);

    match lease.backend.chat(chat_req).await {
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
                Sse::new(anthropic_sse_stream(
                    chat_stream,
                    cancel,
                    model,
                    Some(lease),
                ))
                .into_response()
            } else {
                anthropic_collect(chat_stream, cancel, &model, Some(lease)).await
            }
        }
    }
}

// ─── Control plane (switch / unload / reload) ────────────────────────────────

#[derive(Deserialize)]
struct SwitchReq {
    model: String,
    #[serde(default)]
    n_ctx: Option<u32>,
    #[serde(default)]
    backend: Option<String>,
}

/// `POST /control/switch` — in-place model/backend swap. Drains, drops the old
/// model, loads the new one, bumps `generation`, resumes. Proxies hold their
/// queued requests across the gap (see `admit_handler`), so it's transparent.
async fn control_switch(
    State(state): State<GatewayState>,
    axum::Json(req): axum::Json<SwitchReq>,
) -> Response {
    match state
        .sb
        .switch(req.model.clone(), req.n_ctx, req.backend)
        .await
    {
        Ok(generation) => axum::Json(json!({
            "status": "switched", "model": req.model, "generation": generation,
        }))
        .into_response(),
        Err(e) => error_json(StatusCode::INTERNAL_SERVER_ERROR, &e, "switch_failed"),
    }
}

/// `POST /control/unload` — free the resident model but keep the daemon; the
/// next chat lazily reloads it.
async fn control_unload(State(state): State<GatewayState>) -> Response {
    match state.sb.unload().await {
        Ok(generation) => axum::Json(json!({
            "status": "unloaded", "generation": generation,
        }))
        .into_response(),
        Err(e) => error_json(StatusCode::INTERNAL_SERVER_ERROR, &e, "unload_failed"),
    }
}

/// `POST /control/reload` — graceful restart from the current binary (picks up an
/// upgraded `rozum`). Drains, then re-execs with the current model/port. The
/// brief port gap is covered by the proxies' replay path, like a failover.
async fn control_reload(State(state): State<GatewayState>) -> Response {
    if state.sb.builder.is_none() {
        return error_json(
            StatusCode::BAD_REQUEST,
            "this gateway does not support reload (dedicated)",
            "reload_failed",
        );
    }
    if let Err(e) = state.sb.begin_drain().await {
        return error_json(StatusCode::INTERNAL_SERVER_ERROR, &e, "reload_failed");
    }
    let spec = state.sb.spec.lock().unwrap().clone();
    let Some((_, port)) = state.sb.register else {
        state.sb.end_drain();
        return error_json(
            StatusCode::BAD_REQUEST,
            "reload requires a registered shared gateway",
            "reload_failed",
        );
    };
    // Re-exec after the 200 response has had a moment to flush. `exec` replaces
    // this process image (same pid); the new daemon rebinds the same port and
    // republishes with a bumped generation (read from the prior record at start).
    let model_id = spec.model_id.clone();
    crate::obs::log_event(json!({
        "event": "gateway_reload", "model": model_id, "port": port,
    }));
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        reexec_gateway(&spec, port);
    });
    axum::Json(json!({ "status": "reloading", "model": model_id })).into_response()
}

/// Replace this process with a fresh `rozum gateway` for the same model/port.
/// Falls back to `process::exit(0)` if exec fails (the failover watchdog or a
/// fresh launch will respawn it).
fn reexec_gateway(spec: &ModelSpec, port: u16) -> ! {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| "rozum".into());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("gateway")
        .arg("--model")
        .arg(&spec.model_id)
        .arg("--n-ctx")
        .arg(spec.n_ctx.to_string())
        .arg("--port")
        .arg(port.to_string());
    let err = cmd.exec(); // returns only on failure
    crate::obs::log_event(json!({
        "event": "gateway_reload_exec_failed", "error": err.to_string(),
    }));
    std::process::exit(0);
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

// ─── Poison fast-refuse ─────────────────────────────────────────────────────

fn poison_ttl_secs() -> u64 {
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
async fn poison_layer(req: axum::extract::Request, next: Next) -> Response {
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

    let started_at = crate::share::now_unix();
    let n_ctx = cfg
        .register_n_ctx
        .unwrap_or_else(|| backend.context_window());

    // Shared-daemon registry: publish so `rozum launch` clients discover & reuse
    // us; remove on exit (only if it's still our record). Spec: shared-gateway.
    // `generation` continues from any prior record on this port (a respawn / exec
    // reload reusing the port) so it changes monotonically across restarts.
    let (register, generation0) = match (cfg.register_n_ctx, port) {
        (Some(n_ctx), Some(port)) => {
            let pid = std::process::id();
            let generation = crate::share::read_active()
                .map(|a| a.generation)
                .unwrap_or(0)
                + 1;
            let _ = crate::share::write_active(&crate::share::ActiveGateway {
                model: model_id.clone(),
                port,
                pid,
                n_ctx,
                started_at,
                generation,
            });
            (Some((pid, port)), generation)
        }
        _ => (None, 0),
    };
    let registered_pid = register.map(|(pid, _)| pid);

    let sb = Arc::new(Switchboard {
        backend: std::sync::RwLock::new(Some(backend)),
        builder: cfg.builder,
        spec: std::sync::Mutex::new(ModelSpec {
            model_id,
            n_ctx,
            backend: cfg.backend_hint,
        }),
        generation: AtomicU64::new(generation0),
        started_at,
        draining: AtomicBool::new(false),
        resume: tokio::sync::Notify::new(),
        generating: AtomicU64::new(0),
        reload_lock: tokio::sync::Mutex::new(()),
        register,
    });

    let state = GatewayState {
        sb,
        auth_token: std::env::var("ROZUM_GATEWAY_TOKEN").ok(),
        observer,
        activity: Arc::new(Activity::default()),
    };

    // Lifecycle watchdog (shared daemon). Per 2 s tick, in order:
    //   1. exit — no client lease and nothing in flight. A launch-managed daemon
    //      exits the moment the last agent's lease drops (free everything); a
    //      manual `rozum gateway` exits after `idle_secs` of no HTTP traffic.
    //   2. idle-unload — model resident but not generating and quiet for
    //      `unload_secs`: drop just the model's RAM, keep the daemon for lazy
    //      reload. A `--dedicated` gateway has no builder, so unload is guarded
    //      off. Spec: `docs/specs/model-unload-on-idle.md`.
    let idle_exit = cfg.idle_secs.filter(|&s| s > 0);
    let unload_after = unload_idle_secs();
    let unload_on_idle = unload_after > 0 && state.sb.can_reload();
    // Daemons spawned by `rozum launch` are launch-managed: shut down once the
    // last client lease drops, even if a lease was never observed (a short
    // startup grace lets the launch register its first lease).
    let launch_managed = std::env::var("ROZUM_GATEWAY_LAUNCH_MANAGED").is_ok();
    if idle_exit.is_some() || unload_on_idle || launch_managed {
        use std::sync::atomic::Ordering;
        const STARTUP_GRACE_SECS: u64 = 15;
        state
            .activity
            .last_active
            .store(crate::share::now_unix(), Ordering::Relaxed);
        let activity = Arc::clone(&state.activity);
        let sb = Arc::clone(&state.sb);
        let started = crate::share::now_unix();
        tokio::spawn(async move {
            let mut seen_lease = false;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let live_leases = crate::share::live_lease_count(crate::share::LEASE_FRESH_SECS);
                if live_leases > 0 {
                    seen_lease = true;
                }
                let idle_for = crate::share::now_unix()
                    .saturating_sub(activity.last_active.load(Ordering::Relaxed));

                // 1. Lifecycle exit: no client lease and nothing in flight.
                if activity.in_flight.load(Ordering::Relaxed) == 0 && live_leases == 0 {
                    let up_for = crate::share::now_unix().saturating_sub(started);
                    let exit_reason = if seen_lease {
                        // A client lease was observed and is now gone -> agent exited.
                        Some("clients_gone")
                    } else if launch_managed && up_for >= STARTUP_GRACE_SECS {
                        // Launch-spawned but no lease ever appeared (agent never
                        // attached or exited between polls) -> don't linger.
                        Some("clients_gone")
                    } else if idle_exit.is_some_and(|s| idle_for >= s) {
                        Some("idle")
                    } else {
                        None
                    };
                    if let Some(reason) = exit_reason {
                        crate::obs::log_event(serde_json::json!({
                            "event": "gateway_exit", "reason": reason, "idle_secs": idle_for,
                        }));
                        if let Some(pid) = registered_pid {
                            crate::share::remove_active_if_mine(pid);
                        }
                        std::process::exit(0);
                    }
                }

                // 2. idle-unload: model resident, nothing generating, quiet for
                // `unload_secs`. `is_loaded()` makes this fire once, not every tick.
                if unload_on_idle
                    && idle_for >= unload_after
                    && sb.generating.load(Ordering::SeqCst) == 0
                    && sb.is_loaded()
                {
                    crate::obs::log_event(serde_json::json!({
                        "event": "gateway_idle_unload", "idle_secs": idle_for,
                    }));
                    if let Err(e) = sb.unload().await {
                        tracing::warn!(error = %e, "idle-unload failed");
                    }
                }
            }
        });
    }

    let app = Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/admit", get(admit_handler))
        .route("/stats", get(stats_handler))
        .route("/v1/chat/completions", post(oai_chat_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/messages", post(anthropic_handler))
        .route("/control/switch", post(control_switch))
        .route("/control/unload", post(control_unload))
        .route("/control/reload", post(control_reload))
        .layer(middleware::from_fn(poison_layer))
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
        let sse = oai_sse_stream(stream, cancel, "hello".to_owned(), None);
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
        let sse = anthropic_sse_stream(stream, cancel, "hello-model".to_owned(), None);
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

    // ── OpenAI Responses API (/v1/responses, for Codex) ─────────────────────

    #[test]
    fn responses_input_parsed() {
        // instructions + a user message + a prior tool call + its result.
        let input = json!([
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hi"}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather",
             "arguments": "{\"city\":\"Paris\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "sunny"},
        ]);
        let msgs = responses_input_to_internal(Some("Be terse."), &input);
        assert_eq!(msgs.len(), 4); // system + user + assistant(tool_use) + tool(result)
        assert!(matches!(msgs[0].role, Role::System));
        assert!(matches!(msgs[1].role, Role::User));
        assert!(matches!(msgs[2].role, Role::Assistant));
        assert!(matches!(msgs[3].role, Role::Tool));
        match &msgs[2].content[0] {
            ContentBlock::ToolUse { name, id, .. } => {
                assert_eq!(name, "get_weather");
                assert_eq!(id, "call_1");
            }
            _ => panic!("expected ToolUse in assistant turn"),
        }
        match &msgs[3].content[0] {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => {
                assert_eq!(tool_use_id, "call_1");
                assert_eq!(content, "sunny");
            }
            _ => panic!("expected ToolResult in tool turn"),
        }
    }

    #[test]
    fn responses_string_input() {
        let msgs = responses_input_to_internal(None, &json!("just a string"));
        assert_eq!(msgs.len(), 1);
        assert!(matches!(msgs[0].role, Role::User));
    }

    #[test]
    fn responses_folds_multiple_system_messages() {
        // Codex sends a top-level `instructions` AND a `developer` message — two
        // system turns. Templates (Qwen3.6) require a single system message that is
        // first, so they must fold into ONE leading system message.
        let input = json!([
            {"type": "message", "role": "developer",
             "content": [{"type": "input_text", "text": "Dev rules."}]},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "hi"}]},
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "do it"}]},
        ]);
        let msgs = responses_input_to_internal(Some("Top instructions."), &input);
        assert_eq!(msgs.len(), 3, "one system + two users");
        assert!(matches!(msgs[0].role, Role::System));
        assert!(matches!(msgs[1].role, Role::User));
        assert!(matches!(msgs[2].role, Role::User));
        assert_eq!(
            msgs.iter().filter(|m| matches!(m.role, Role::System)).count(),
            1,
            "exactly one system message"
        );
        match &msgs[0].content[0] {
            ContentBlock::Text { text } => {
                assert!(text.contains("Top instructions."), "instructions folded in");
                assert!(text.contains("Dev rules."), "developer folded in");
            }
            _ => panic!("system message should be text"),
        }
    }

    #[test]
    fn responses_tool_def_mapped() {
        // Responses tools are FLAT (no nested `function`).
        let tools = vec![RespTool {
            kind: Some("function".into()),
            name: Some("get_weather".into()),
            description: Some("Get weather".into()),
            parameters: Some(json!({ "type": "object" })),
        }];
        let defs = responses_tools_to_internal(&tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "get_weather");
    }

    #[test]
    fn responses_object_shape() {
        let obj = responses_object("resp_1", 123, "m", "completed", json!([]), 3, 4);
        assert_eq!(obj["object"], "response");
        assert_eq!(obj["status"], "completed");
        assert_eq!(obj["usage"]["total_tokens"], 7);
    }

    #[tokio::test]
    async fn responses_sse_emits_events() {
        let backend = Arc::new(HelloBackend::new()) as Arc<dyn ChatBackend>;
        let req = ChatRequest::simple("hi");
        let stream = backend.chat(req).await.unwrap();
        let cancel = CancellationToken::new();
        let sse = responses_sse_stream(stream, cancel, "m".to_owned(), None);
        futures::pin_mut!(sse);
        let events: Vec<_> = sse.collect().await;
        // created + output_item.added + content_part.added + >=1 delta +
        // output_text.done + content_part.done + output_item.done + completed.
        assert!(
            events.len() >= 5,
            "expected Responses SSE events, got {}",
            events.len()
        );
    }

    // ── Switchboard (gateway switch / unload / reload) ──────────────────────

    fn test_sb(builder: Option<BackendBuilder>, loaded: bool) -> Arc<Switchboard> {
        let backend = loaded.then(|| Arc::new(HelloBackend::new()) as Arc<dyn ChatBackend>);
        Arc::new(Switchboard {
            backend: std::sync::RwLock::new(backend),
            builder,
            spec: std::sync::Mutex::new(ModelSpec {
                model_id: "model-old".into(),
                n_ctx: 100,
                backend: None,
            }),
            generation: AtomicU64::new(1),
            started_at: 0,
            draining: AtomicBool::new(false),
            resume: tokio::sync::Notify::new(),
            generating: AtomicU64::new(0),
            reload_lock: tokio::sync::Mutex::new(()),
            register: None, // don't touch active.json in tests
        })
    }

    fn ok_builder() -> BackendBuilder {
        Arc::new(|_m, _n, _b| {
            Box::pin(async { Some(Arc::new(HelloBackend::new()) as Arc<dyn ChatBackend>) })
        })
    }

    #[tokio::test]
    async fn switch_swaps_model_and_bumps_generation() {
        let sb = test_sb(Some(ok_builder()), true);
        let g0 = sb.generation();
        let g1 = sb
            .switch("model-new".into(), Some(200), None)
            .await
            .unwrap();
        assert_eq!(g1, g0 + 1, "generation bumps on switch");
        assert_eq!(sb.model_id(), "model-new");
        assert_eq!(sb.n_ctx(), 200, "new n_ctx applied");
        assert!(sb.current().is_some(), "new model is resident");
        assert!(
            !sb.draining.load(Ordering::SeqCst),
            "drain cleared after switch"
        );
    }

    #[tokio::test]
    async fn switch_keeps_n_ctx_when_unspecified() {
        let sb = test_sb(Some(ok_builder()), true);
        sb.switch("model-new".into(), None, None).await.unwrap();
        assert_eq!(sb.n_ctx(), 100, "n_ctx preserved across switch");
    }

    #[tokio::test]
    async fn unload_frees_model_then_enter_lazily_reloads() {
        let sb = test_sb(Some(ok_builder()), true);
        let g = sb.unload().await.unwrap();
        assert_eq!(g, 2, "generation bumps on unload");
        assert!(sb.current().is_none(), "model freed after unload");
        // A chat entering finds it unloaded and rebuilds it once.
        let lease = sb.enter().await.expect("lazy reload should succeed");
        assert!(sb.current().is_some(), "model reloaded on demand");
        assert_eq!(
            sb.generating.load(Ordering::SeqCst),
            1,
            "lease holds a token"
        );
        drop(lease);
        assert_eq!(
            sb.generating.load(Ordering::SeqCst),
            0,
            "token released on drop"
        );
    }

    #[test]
    fn is_loaded_and_can_reload_reflect_state() {
        assert!(test_sb(Some(ok_builder()), true).is_loaded());
        assert!(!test_sb(Some(ok_builder()), false).is_loaded());
        assert!(test_sb(Some(ok_builder()), true).can_reload());
        // A --dedicated gateway has no builder: it must never auto-unload.
        assert!(!test_sb(None, true).can_reload());
    }

    #[tokio::test]
    async fn idle_unload_guard_is_idempotent() {
        // The watchdog fires unload only while `is_loaded()`. After the first
        // unload the model is gone, so the next tick is a no-op — no repeated
        // drains / generation bumps / log spam every 30 s.
        let sb = test_sb(Some(ok_builder()), true);
        assert!(sb.is_loaded() && sb.generating.load(Ordering::SeqCst) == 0);
        let g = sb.unload().await.unwrap();
        assert!(!sb.is_loaded(), "model freed; watchdog guard now skips it");
        // A second tick would re-check `is_loaded()` first and not unload again.
        assert!(!sb.is_loaded());
        assert_eq!(
            sb.generation(),
            g,
            "no further generation bump while unloaded"
        );
    }

    #[tokio::test]
    async fn switch_failure_reverts_spec_and_resumes() {
        let bad: BackendBuilder = Arc::new(|_m, _n, _b| Box::pin(async { None }));
        let sb = test_sb(Some(bad), true);
        let err = sb.switch("model-new".into(), None, None).await.unwrap_err();
        assert!(err.contains("failed to load"), "got: {err}");
        assert_eq!(sb.model_id(), "model-old", "spec reverts on build failure");
        assert!(
            !sb.draining.load(Ordering::SeqCst),
            "drain cleared after failure"
        );
    }

    #[tokio::test]
    async fn dedicated_gateway_refuses_switch_and_unload() {
        let sb = test_sb(None, true);
        assert!(sb.switch("x".into(), None, None).await.is_err());
        assert!(sb.unload().await.is_err());
        assert!(
            sb.current().is_some(),
            "model untouched when switch is refused"
        );
    }

    #[tokio::test]
    async fn enter_parks_while_draining_then_resumes() {
        let sb = test_sb(Some(ok_builder()), true);
        sb.draining.store(true, Ordering::SeqCst);
        let sb2 = Arc::clone(&sb);
        let task = tokio::spawn(async move { sb2.enter().await.map(|_| ()) });
        // While draining, enter must not complete.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!task.is_finished(), "enter parked during drain");
        sb.end_drain();
        let res = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(res.is_ok(), "enter resumed after end_drain");
    }
}
