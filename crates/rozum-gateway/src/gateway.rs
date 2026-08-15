//! HTTP gateway exposing rozum's `ChatBackend` over three agent **wire protocols**.
//!
//! # Wire protocols (the agent-dialect seam)
//!
//! This is the `WireProtocol` axis of `docs/specs/architecture-spi.md`. Each dialect is one
//! *thin* handler that (1) parses its request shape into the internal `ChatRequest`, (2) calls
//! the backend via `chat_or_loopbreak`, (3) serializes the internal `ChatEvent` stream back
//! into its SSE shape. The three handlers are deliberately parallel — to add an agent dialect,
//! copy the triple. All three converge on `ChatRequest` / `ChatEvent`, so the backend, the
//! model, and every robustness policy (loop-breaker, read-repair) are written once,
//! dialect-agnostic.
//!
//! | Dialect | Route | Request | Parse (wire→internal) | Serialize (internal→wire) | Handler |
//! |---|---|---|---|---|---|
//! | OpenAI Chat | `POST /v1/chat/completions` | `OaiChatReq` | `oai_messages_to_internal` / `oai_tools_to_internal` | `oai_sse_stream` / `oai_chunk` | `OaiWire` |
//! | OpenAI Responses (Codex) | `POST /v1/responses` | `RespReq` | `responses_input_to_internal` / `responses_tools_to_internal` (+ `codex_lean_keep` tool policy) | `responses_sse_stream` / `responses_collect` | `RespWire` |
//! | Anthropic Messages | `POST /v1/messages` | `AnthropicReq` | `anthropic_messages_to_internal` / `anthropic_tools_to_internal` | `anthropic_sse_stream` | `AnthropicWire` |
//!
//! Cross-cutting, owned by neither dialect: `chat_or_loopbreak` / `detect_stuck_loop`
//! (loop-breaker), `synthetic_stop_stream`, `parse_response_format` (structured output),
//! `parse_*_tool_choice` (tool-choice normalization). `GET /v1/models`, `/health`, `/ready`,
//! `/stats`, `/control/*` are non-chat endpoints.
//!
//! **A map AND a seam, and where the line between them is** (`plugin-wireprotocol`, operator
//! override 2026-08-14 of the Stage-3 "map, not trait" call). The two objections that rejected a
//! trait still hold and are still honoured: each dialect keeps its OWN typed extractor, so request
//! validation is untouched, and each keeps its own SSE sequence, so no dialect is bent into another
//! one's shape. What the earlier investigation did not weigh is what sits BETWEEN parse and
//! serialize — lease, auto-context fit, elision note, token estimate, `ChatRequest`, loop-breaker,
//! metering, generation timeout, stream/collect branch. That spine was written three times, and it
//! had already drifted: `/v1/messages` accepts no `top_p` or `top_k`, which its own API defines.
//! `trait WireDialect` + `serve_wire` hold it once; the dialects hold what genuinely differs.
//! Cost, measured: +44 lines of code and one indirection. Gate: `src/testdata/wire-golden.txt`,
//! frozen before the move and byte-identical after.
//!
//! Bind address is always `127.0.0.1`. Auth is optional bearer token via
//! `ROZUM_GATEWAY_TOKEN`. Cancel propagates from client disconnect.
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::Sse,
    },
    routing::{get, post},
};
use futures::{Stream, StreamExt as _};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// The codex `apply_patch` / tool-arg rewriters were extracted to their own module
// (gw-monolith-decompose); glob-import them so the request-handling call sites read unchanged.
// Likewise the stuck-loop detector (loop-breaker).
use crate::auto_context::*;
pub(crate) use crate::serving::*;
pub(crate) use crate::switchboard::*;
// `BackendBuilder` is part of this crate's PUBLIC surface — the binary injects the backend-selection
// chain through it. Re-exported by name so `rozum_gateway::gateway::BackendBuilder` keeps resolving:
// moving a type must not move its path out from under its callers.
pub use crate::switchboard::BackendBuilder;
// ...and the codex-lean tool/prompt-trimming policy helpers.
use crate::codex_lean::*;
use crate::oai_api::*;
use crate::anthropic_api::*;
use crate::responses_api::*;

use crate::backend::{
    ChatBackend, ChatEvent, ChatRequest, ChatStream, Message,
    ModelResult, SamplingParams, ToolDef,
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

// The resident-model manager and its `BackendBuilder` live in `switchboard.rs`
// (gw-monolith-decompose); glob-imported so every call site and test reads unchanged.


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

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(crate) fn new_id(prefix: &str) -> String {
    format!("{}-{}", prefix, Uuid::new_v4().simple())
}










// Error responses live in `errors.rs` (gw-monolith-decompose). Re-exported here because four
// modules import them from `crate::gateway`, and an extraction that rewrites its callers is
// harder to review than the code it moved.
pub(crate) use crate::errors::{chat_error_response, error_json};

// ─── CancelOnDrop ─────────────────────────────────────────────────────────────


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

// ─── Generation inactivity timeout ────────────────────────────────────────────





// ─── OpenAI conversion ────────────────────────────────────────────────────────

/// Decode a `data:<mime>;base64,<b64>` image URI into raw bytes. Returns `None`
/// for non-data URIs (remote URLs are not fetched — SSRF/complexity) or bad base64.
pub(crate) fn decode_data_uri_image(url: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let rest = url.strip_prefix("data:")?;
    let comma = rest.find(',')?;
    let (meta, tail) = rest.split_at(comma);
    if !meta.contains("base64") {
        return None;
    }
    base64::engine::general_purpose::STANDARD
        .decode(tail[1..].as_bytes())
        .ok()
}










// ─── Route handlers ───────────────────────────────────────────────────────────

/// `GET /health` — liveness. 200 as long as the process serves HTTP. Cheap, never
/// touches the model; an orchestrator uses it to decide whether to restart the pod.
async fn health_handler() -> impl IntoResponse {
    (StatusCode::OK, axum::Json(json!({ "status": "ok" })))
}

/// `GET /ready` — readiness for a load balancer. 200 when this instance can serve a
/// new request now; 503 while draining for shutdown (so the LB stops routing here and
/// in-flight work finishes) or when the model is gone and can't be rebuilt.
async fn ready_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let ready = state.sb.is_ready();
    let code = if ready { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    let body = json!({
        "ready": ready,
        "loaded": state.sb.is_loaded(),
        "shutting_down": state.sb.is_shutting_down(),
        "model": state.sb.model_id(),
    });
    (code, axum::Json(body))
}

/// Seconds to keep serving after a shutdown signal so a load balancer notices
/// `/ready` = 503 before connections are cut. `ROZUM_SHUTDOWN_GRACE_SECS` (default 3).
fn shutdown_grace_secs() -> u64 {
    std::env::var("ROZUM_SHUTDOWN_GRACE_SECS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(3)
}

/// How long a drain may take before the process stops waiting for connections that will not close.
///
/// Longer than any handshake and shorter than an operator's patience. It is a ceiling on the
/// SHUTDOWN, not on a request: a generation in flight suspends it entirely.
const DRAIN_DEADLINE_SECS: u64 = 20;

/// Completes when the drain has gone on too long AND nothing is generating.
///
/// Both halves matter. Without the deadline the process waits forever on an idle stream; without
/// the generating check it could exit into a live Metal eval, which is how this machine was once
/// rebooted (BUGS.md BUG-001). So it waits for the signal, gives the drain its deadline, and then
/// waits as long as it must for the GPU to be quiet — announcing itself so a five-minute wait is
/// legible in the log rather than a mystery.
async fn drain_deadline(sb: Arc<Switchboard>) {
    // Only starts counting once shutdown has actually begun.
    while !sb.is_shutting_down() {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    tokio::time::sleep(std::time::Duration::from_secs(DRAIN_DEADLINE_SECS)).await;
    let mut waited = 0u64;
    while sb.generating.load(Ordering::SeqCst) != 0 {
        if waited % 30 == 0 {
            eprintln!(
                "rozum gateway: shutdown is waiting on a live generation ({}s) — not forcing an \
                 exit mid-Metal-eval (BUGS.md BUG-001)",
                DRAIN_DEADLINE_SECS + waited
            );
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        waited += 1;
    }
}

/// Completes on SIGTERM/SIGINT, after flipping the gateway to "not ready"/// Completes on SIGTERM/SIGINT, after flipping the gateway to "not ready" and waiting a
/// short grace so the load balancer deregisters this instance before axum stops
/// accepting connections and drains the in-flight streams. Wired into
/// `axum::serve(...).with_graceful_shutdown(...)`.
async fn shutdown_signal(sb: Arc<Switchboard>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    // WHICH signal, and who was around to send it. On 2026-08-13 the resident gateway took a
    // shutdown signal mid-request during a bench that was explicitly SHARING it, and the record
    // said only "gateway_shutdown_signal" — enough to know it was signalled, not enough to name
    // the sender. Three candidates were checked and cleared by reading code (the bench skips its
    // stop path when sharing; `rozum launch` was on its reuse path, not takeover; launchd's job
    // reported a clean exit), which is exactly the position a one-word event leaves you in.
    let signal = tokio::select! {
        _ = ctrl_c => "SIGINT",
        _ = term => "SIGTERM",
    };
    crate::obs::log_event(json!({
        "event": "gateway_shutdown_signal",
        "signal": signal,
        "ppid": crate::procctl::parent_pid(),
        "pid": std::process::id(),
    }));
    sb.mark_shutting_down();
    tokio::time::sleep(Duration::from_secs(shutdown_grace_secs())).await;
}

async fn stats_handler(State(state): State<GatewayState>) -> impl IntoResponse {
    let mut snap = state.observer.snapshot();
    if let Value::Object(ref mut m) = snap {
        m.insert("model".into(), json!(state.sb.model_id()));
        // Resident model set (residency-unify): the primary plus any co-resident warm
        // secondaries — what this gateway serves right now. Informational; `/stats` (unlike
        // `/v1/models`) is not parsed by agent clients, so it's safe to surface here.
        let mut residents = vec![state.sb.model_id()];
        residents.extend(state.sb.warm.lock().await.keys().cloned());
        m.insert("resident_models".into(), json!(residents));
        m.insert("generation".into(), json!(state.sb.generation()));
        let ctx = state
            .sb
            .current()
            .map(|b| b.context_window())
            .unwrap_or(state.sb.n_ctx());
        m.insert("context_window".into(), json!(ctx));
        m.insert("loaded".into(), json!(state.sb.current().is_some()));
        // Admission window — instantaneous (limit / in-flight / queued / free) plus the
        // cumulative scheduler counters (admitted, fast-lane hits, shed/429, queued) so the
        // admission policy is tunable from data.
        if let Some(a) = state.sb.current().and_then(|b| b.admission_stats()) {
            m.insert(
                "admission".into(),
                json!({
                    "limit": a.limit,
                    "in_use": a.in_use,
                    "waiting": a.waiting,
                    "free": a.free(),
                    "admitted": a.admitted,
                    "fast_lane": a.fast_lane,
                    "shed": a.shed,
                    "queued": a.queued,
                }),
            );
        }
        // MLX Metal memory (native runtime): the resident model's unified-memory footprint
        // (active / peak / cache MB), which process RSS doesn't capture. `active` drops to ~0
        // after an idle-unload, so this is how you watch the model free its RAM.
        if let Some((active, peak, cache)) = crate::obs::mlx_memory_mb() {
            m.insert(
                "mlx_memory_mb".into(),
                json!({ "active": active, "peak": peak, "cache": cache }),
            );
        }
        // Host memory-pressure level (the OS jetsam ladder the shed watchdog acts on):
        // normal / warn / critical — the real-time host safety margin, observable without
        // re-sampling sysctl. See `rozum-core::shed` + docs/specs/safe-multi-model-program.md.
        m.insert(
            "memory_pressure".into(),
            json!(crate::shed::read_host_pressure().as_str()),
        );
        // Batched-decode occupancy (native MLX): how many concurrent requests actually
        // share a forward. `avg_occupancy = rows/runs`; `max` is the high-water batch size;
        // `admits` counts continuous mid-decode admissions. Omitted until something batches.
        if let Some(b) = crate::obs::batch_stats() {
            let avg = if b.runs > 0 { b.rows as f64 / b.runs as f64 } else { 0.0 };
            m.insert(
                "batch".into(),
                json!({
                    "runs": b.runs,
                    "rows": b.rows,
                    "admits": b.admits,
                    "max": b.max,
                    "avg_occupancy": (avg * 100.0).round() / 100.0,
                    "serial_seed": b.serial_seed,
                    "serial_penalty": b.serial_penalty,
                    "serial_constrained": b.serial_constrained,
                }),
            );
        }
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



// ─── The wire seam ────────────────────────────────────────────────────────────

/// What a dialect produces once its OWN extractor has parsed the body: the internal request, minus
/// everything the gateway does to it afterwards.
pub(crate) struct WireRequest {
    pub(crate) messages: Vec<Message>,
    pub(crate) tools: Vec<ToolDef>,
    pub(crate) sampling: SamplingParams,
}

/// One agent-facing wire dialect — OpenAI Chat, OpenAI Responses, Anthropic Messages.
///
/// **What this is NOT.** It is not an abstraction over the request bodies: each dialect keeps its
/// own typed extractor (`OaiChatReq` / `RespReq` / `AnthropicReq`), so axum's validation is
/// unchanged, and it is not an abstraction over the SSE sequences — `respond` hands back a finished
/// `Response` and each impl calls its own serializer, whose bytes differ per dialect by design.
/// Both of those were the reason `architecture-spi.md` rejected a `WireProtocol` trait in Stage 3,
/// and both objections still hold; this trait deliberately does not cross either line.
///
/// **What it removes** is the third copy of the orchestration between parse and serialize: acquire
/// the lease, fit the prompt to the context window, attach the elision note, estimate tokens, build
/// the `ChatRequest`, run the loop-breaker, meter, apply the generation timeout, branch on `stream`.
/// That spine was written three times and had already drifted (`/v1/messages` accepts no `top_p` or
/// `top_k`), and every cross-cutting change to it — auto-context, metering, the generation timeout —
/// had to be made three times or be wrong in one place. Adding a dialect is now: one extractor, one
/// impl, one route.
trait WireDialect: Sized {
    /// The route, as it appears in the error events and the request metrics.
    const ENDPOINT: &'static str;
    /// The `type` this dialect gives an error body. OpenAI says `backend_error`; Anthropic's own
    /// error envelope says `api_error`, and a client that switches on it would notice the
    /// difference — so it stays a property of the dialect.
    const ERROR_KIND: &'static str;

    /// The model this request asked for, if it named one.
    fn model_hint(&self) -> Option<&str>;

    /// Did the client ask for SSE? (Absent `stream` is non-streaming JSON in both specs.)
    fn stream_mode(&self) -> bool;

    /// Parse into the internal request. Takes the lease because two dialects need the RESOLVED
    /// model id to decide what to send (codex-lean's instruction trim, its reasoning floor), and
    /// takes `&mut self` because a dialect may derive state here that its serializer needs later
    /// (the Responses `apply_patch` re-route).
    fn into_internal(&mut self, lease: &ChatLease) -> WireRequest;

    /// Serialize the answer — SSE or a single JSON body, this dialect's own shape.
    ///
    /// Returns a future rather than being `async fn` so the `+ Send` bound can be stated: axum
    /// requires a `Send` handler future, and an `async fn` in a trait does not promise one.
    fn respond(
        self,
        chat_stream: ChatStream,
        cancel: CancellationToken,
        model: String,
        lease: ChatLease,
    ) -> impl std::future::Future<Output = Response> + Send;
}

/// Everything that happens to a chat request between "parsed" and "serialized", once.
async fn serve_wire<D: WireDialect>(state: GatewayState, mut dialect: D) -> Response {
    // Hold a lease for the whole request so a `switch` can't swap the model
    // mid-flight; parks here if a swap is draining, lazily reloads if unloaded.
    let lease = match state.sb.enter(dialect.model_hint()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };

    let WireRequest {
        messages,
        tools,
        sampling,
    } = dialect.into_internal(&lease);

    // gateway-auto-context: fit the prompt to the window (drop oldest turns, then compress tool
    // schemas) instead of erroring, then attach an elision note for the dropped turns.
    let ctx_win = lease.backend.context_window();
    let (messages, tools, dropped) = match fit_to_context(messages, tools, ctx_win) {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let messages = with_elision_note(messages, dropped, ctx_win, &lease.backend).await;
    let est = estimate_prompt_tokens(&messages, &tools); // post-fit token estimate (for obs)

    let (n_messages, n_tools) = (messages.len(), tools.len());
    let cancel = CancellationToken::new();
    let chat_req = ChatRequest {
        messages,
        tools,
        sampling: apply_determinism_env(sampling),
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = dialect
        .model_hint()
        .map(str::to_owned)
        .unwrap_or_else(|| lease.model_id.clone());

    match chat_or_loopbreak(&lease.backend, chat_req).await {
        Err(e) => {
            crate::obs::log_event(json!({
                "event": "request_error", "endpoint": D::ENDPOINT, "error": e.to_string(),
            }));
            chat_error_response(&e, D::ERROR_KIND)
        }
        Ok(chat_stream) => {
            let meta = crate::obs::ReqMeta {
                endpoint: D::ENDPOINT,
                model: model.clone(),
                n_messages,
                n_tools,
                est_prompt_tokens: est,
            };
            let chat_stream = crate::obs::meter(chat_stream, state.observer.clone(), meta);
            let chat_stream = with_gen_timeout(chat_stream, cancel.clone(), gen_inactivity_timeout());
            dialect.respond(chat_stream, cancel, model, lease).await
        }
    }
}

// ─── OpenAI Chat ──────────────────────────────────────────────────────────────

struct OaiWire {
    req: OaiChatReq,
}

impl OaiWire {
    /// Did the client ask for token counts inside the stream (`stream_options.include_usage`)?
    /// Absent means no, and no must mean the frames are exactly what they always were (BUG-033).
    fn include_usage(&self) -> bool {
        self.req
            .stream_options
            .as_ref()
            .is_some_and(|o| o.include_usage)
    }
}

impl WireDialect for OaiWire {
    const ENDPOINT: &'static str = "/v1/chat/completions";
    const ERROR_KIND: &'static str = "backend_error";

    fn model_hint(&self) -> Option<&str> {
        self.req.model.as_deref()
    }

    fn stream_mode(&self) -> bool {
        // OpenAI/Anthropic spec default for an absent `stream` is non-streaming JSON.
        // (Streaming clients — CC, Codex — always send `stream:true` explicitly.)
        self.req.stream.unwrap_or(false)
    }

    fn into_internal(&mut self, _lease: &ChatLease) -> WireRequest {
        log_unhandled_fields(Self::ENDPOINT, &self.req.unknown);
        WireRequest {
            messages: oai_messages_to_internal(&self.req.messages),
            tools: apply_tool_choice(
                oai_tools_to_internal(&self.req.tools),
                &parse_oai_tool_choice(&self.req.tool_choice),
            ),
            sampling: SamplingParams {
                temperature: self.req.temperature,
                top_p: self.req.top_p,
                max_tokens: self.req.max_tokens,
                top_k: self.req.top_k,
                // A client seed now reaches `apply_determinism`, which fills the seed from
                // `ROZUM_SAMPLING_SEED` only when it is unset — so the client wins, exactly as that
                // function's comment has always claimed. Until BUG-032 no endpoint parsed one, so
                // the branch was unreachable and the comment described nothing.
                seed: self.req.seed,
                repeat_penalty: self.req.repetition_penalty,
                frequency_penalty: self.req.frequency_penalty,
                presence_penalty: self.req.presence_penalty,
                stop: parse_stop(&self.req.stop),
                response_schema: parse_response_format(&self.req.response_format),
                ..Default::default()
            },
        }
    }

    async fn respond(
        self,
        chat_stream: ChatStream,
        cancel: CancellationToken,
        model: String,
        lease: ChatLease,
    ) -> Response {
        if self.stream_mode() {
            Sse::new(oai_sse_stream(
                chat_stream,
                cancel,
                model,
                Some(lease),
                self.include_usage(),
            ))
            .into_response()
        } else {
            oai_collect(chat_stream, cancel, &model, Some(lease)).await
        }
    }
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
    serve_wire(state, OaiWire { req }).await
}

// ─── OpenAI Responses (Codex) ─────────────────────────────────────────────────

struct RespWire {
    req: RespReq,
    /// Did codex offer `apply_patch` as a function tool for THIS request? Derived while parsing and
    /// needed again while serializing — the reason `into_internal` takes `&mut self`.
    apply_patch_is_tool: bool,
}

impl WireDialect for RespWire {
    const ENDPOINT: &'static str = "/v1/responses";
    const ERROR_KIND: &'static str = "backend_error";

    fn model_hint(&self) -> Option<&str> {
        self.req.model.as_deref()
    }

    fn stream_mode(&self) -> bool {
        self.req.stream.unwrap_or(false)
    }

    fn into_internal(&mut self, lease: &ChatLease) -> WireRequest {
        log_unhandled_fields(Self::ENDPOINT, &self.req.unknown);
        // Trim codex's ~21 KB instructions to a short focused prompt for load-sensitive models
        // (gpt-oss) — the bisection-proven dominant breaker of tool delivery. Verbatim for 35B et al.
        let effective_instructions =
            codex_effective_instructions(&lease.model_id, self.req.instructions.as_deref());
        if effective_instructions.as_deref() != self.req.instructions.as_deref() {
            tracing::debug!(
                model = %lease.model_id,
                from_bytes = self.req.instructions.as_deref().map(str::len).unwrap_or(0),
                to_bytes = effective_instructions.as_deref().map(str::len).unwrap_or(0),
                "codex-lean: replaced instructions with the focused coding prompt"
            );
        }
        let messages =
            responses_input_to_internal(effective_instructions.as_deref(), &self.req.input);
        // Did codex offer `apply_patch` as a function tool for this request? If not, a model that
        // calls it as a function (gpt-oss) would hit "unsupported call: apply_patch" — so we
        // re-route those to exec_command. When codex DID offer it as a tool, the call is legit and
        // we leave it alone.
        self.apply_patch_is_tool = self
            .req
            .tools
            .iter()
            .any(|t| t.name.as_deref() == Some("apply_patch"));
        let mut tools = apply_tool_choice(
            responses_tools_to_internal(&self.req.tools),
            &parse_oai_tool_choice(&self.req.tool_choice),
        );
        // EXPERIMENT (ROZUM_CODEX_INJECT_APPLY_PATCH): gpt-oss is trained to call `apply_patch` as a
        // function, but codex offers it only as a shell command for our config — so the model GUESSES
        // the schema (keys begin_patch / cmd / update …) and we drop the guesses. Give it the tool it
        // expects, with a CLEAR schema, so it stops guessing; its clean {patch:…} call is re-routed to
        // exec_command by the Responses serializer (apply_patch_is_tool stays false → reroute fires).
        let inject_ap = std::env::var("ROZUM_CODEX_INJECT_APPLY_PATCH")
            .map(|v| v != "0")
            .unwrap_or(false);
        if inject_ap && !self.apply_patch_is_tool {
            tools.push(ToolDef {
                name: "apply_patch".into(),
                description: "Apply a patch to a file in the working directory — the preferred way to \
                    EDIT files (use this instead of shell `sed`/`cat` heredocs). The `patch` argument \
                    is the full V4A patch: a `*** Begin Patch` line, then `*** Update File: <relative \
                    path>`, then a hunk with context lines, `-` (remove) and `+` (add) lines, then a \
                    `*** End Patch` line."
                    .into(),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "patch": {
                            "type": "string",
                            "description": "The full patch, from `*** Begin Patch` to `*** End Patch`."
                        }
                    },
                    "required": ["patch"]
                }),
            });
        }
        log_codex_tool_inventory(
            self.req.model.as_deref(),
            self.req.stream.unwrap_or(false),
            &self.req.tools,
            &tools,
            self.apply_patch_is_tool,
            inject_ap,
        );

        WireRequest {
            messages,
            tools,
            sampling: SamplingParams {
                temperature: self.req.temperature,
                top_p: self.req.top_p,
                max_tokens: self.req.max_output_tokens,
                top_k: self.req.top_k,
                // codex's `reasoning.effort` → gpt-oss harmony reasoning level — but a load-sensitive
                // model on the lean path is forced to `low` (codex's `medium` otherwise times it out on
                // multi-turn agentic tasks); ignored by other models.
                reasoning_effort: codex_effective_reasoning(
                    &lease.model_id,
                    reasoning_effort_of(&self.req.reasoning),
                ),
                // `text.format`, the Responses spelling of what Chat calls `response_format`
                // (BUG-034). `{"type":"text"}` — which is what a client says when it wants plain
                // prose — parses to `None`, so the default stays UNCONSTRAINED.
                repeat_penalty: self.req.repetition_penalty,
                response_schema: parse_text_format(&self.req.text),
                ..Default::default()
            },
        }
    }

    async fn respond(
        self,
        chat_stream: ChatStream,
        cancel: CancellationToken,
        model: String,
        lease: ChatLease,
    ) -> Response {
        if self.stream_mode() {
            Sse::new(responses_sse_stream(
                chat_stream,
                cancel,
                model,
                Some(lease),
                self.apply_patch_is_tool,
            ))
            .into_response()
        } else {
            responses_collect(
                chat_stream,
                cancel,
                &model,
                Some(lease),
                self.apply_patch_is_tool,
            )
            .await
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
    if std::env::var_os("ROZUM_RESP_DUMP").is_some() {
        let ins_len = req.instructions.as_deref().map(|s| s.len()).unwrap_or(0);
        let input_len = serde_json::to_string(&req.input).map(|s| s.len()).unwrap_or(0);
        eprintln!(
            "─── RESP_DUMP: instructions={ins_len}B input={input_len}B tools={} ───",
            req.tools.len()
        );
        for t in &req.tools {
            eprintln!(
                "  tool: {} (desc {}B)",
                t.name.as_deref().unwrap_or("?"),
                t.description.as_deref().map(|s| s.len()).unwrap_or(0)
            );
        }
        if let Some(ins) = req.instructions.as_deref() {
            let head: String = ins.chars().take(2000).collect();
            eprintln!("─── instructions head ───\n{head}\n─── /instructions ───");
        }
    }
    serve_wire(
        state,
        RespWire {
            req,
            apply_patch_is_tool: false,
        },
    )
    .await
}


// ─── Anthropic Messages ───────────────────────────────────────────────────────

struct AnthropicWire {
    req: AnthropicReq,
}

/// `output_config.effort` → the internal reasoning level, in the client's own vocabulary.
///
/// TWO THINGS ARE MEASURED HERE, not assumed. Claude Code sends `high` and `xhigh`; the shared
/// `reasoning_effort_of` accepts only `low|medium|high`, so `xhigh` — a client asking for MORE
/// thinking — would have parsed to `None` and the chat template would then have defaulted to
/// `medium`, giving it LESS. Clamping to `high` is the closest thing the downstream template can
/// express, and it is a clamp rather than a rename: if the template ever learns a higher level this
/// is the one line to change.
///
/// `thinking` is READ and deliberately not mapped. `{"type":"disabled"}` asks for no reasoning at
/// all, and `reasoning_effort` is a level with no "off" — `None` means unset, which the template
/// turns into `medium`. So a request that says "do not think" is honoured by refusing to raise the
/// level, and nothing more; inventing a level for it would answer a question we were not asked.
fn anthropic_effort(output_config: &Option<Value>, thinking: &Option<Value>) -> Option<String> {
    let disabled = thinking
        .as_ref()
        .and_then(|t| t.get("type"))
        .and_then(Value::as_str)
        == Some("disabled");
    if disabled {
        return None;
    }
    let e = output_config
        .as_ref()?
        .get("effort")?
        .as_str()?
        .trim()
        .to_ascii_lowercase();
    match e.as_str() {
        "low" | "medium" | "high" => Some(e),
        "xhigh" => Some("high".into()),
        _ => None,
    }
}

impl WireDialect for AnthropicWire {
    const ENDPOINT: &'static str = "/v1/messages";
    /// Anthropic's error envelope, not OpenAI's `backend_error` — a client switching on the
    /// field would see the difference.
    const ERROR_KIND: &'static str = "api_error";

    fn model_hint(&self) -> Option<&str> {
        self.req.model.as_deref()
    }

    fn stream_mode(&self) -> bool {
        self.req.stream.unwrap_or(false)
    }

    fn into_internal(&mut self, _lease: &ChatLease) -> WireRequest {
        log_unhandled_fields(Self::ENDPOINT, &self.req.unknown);
        WireRequest {
            messages: anthropic_messages_to_internal(self.req.system.as_ref(), &self.req.messages),
            tools: apply_tool_choice(
                anthropic_tools_to_internal(&self.req.tools),
                &parse_anthropic_tool_choice(&self.req.tool_choice),
            ),
            // `top_p`/`top_k` are Messages API parameters and were silently dropped until
            // BUG-031 — the gap this dialect's own golden line exposed. No `seed`: that one the
            // Messages API genuinely does not define, so its absence here is the dialect.
            //
            // `response_schema` USED to be in that sentence too, and the sentence was wrong.
            // Measured 2026-08-15 by capturing a real Claude Code request: it sends
            // `output_config.format = {"type":"json_schema","schema":{…}}` on every call to
            // `/v1/messages?beta=true`. That is the same nesting `parse_text_format` already reads
            // for the Responses dialect, so structured output existed on a third dialect and was
            // dropped — the same shape as BUG-034, one dialect further on.
            sampling: SamplingParams {
                temperature: self.req.temperature,
                top_p: self.req.top_p,
                top_k: self.req.top_k,
                max_tokens: self.req.max_tokens,
                stop: parse_stop(&self.req.stop_sequences),
                response_schema: parse_text_format(
                    self.req.output_config.as_ref().unwrap_or(&Value::Null),
                ),
                reasoning_effort: anthropic_effort(&self.req.output_config, &self.req.thinking),
                ..Default::default()
            },
        }
    }

    async fn respond(
        self,
        chat_stream: ChatStream,
        cancel: CancellationToken,
        model: String,
        lease: ChatLease,
    ) -> Response {
        if self.stream_mode() {
            Sse::new(anthropic_sse_stream(chat_stream, cancel, model, Some(lease))).into_response()
        } else {
            anthropic_collect(chat_stream, cancel, &model, Some(lease)).await
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
        stream = req.stream.unwrap_or(false),
        "POST /v1/messages"
    );
    serve_wire(state, AnthropicWire { req }).await
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
/// `GET /control/status` — the models/gateway control snapshot (active gateway + host residency +
/// installed catalog) as JSON, for a dashboard / the UCC web target. Read-only; no gateway state.
async fn control_status_handler() -> Response {
    axum::Json(crate::status::status().await).into_response()
}

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
/// Falls back to `process::exit(0)` if the replacement fails (the failover watchdog or a
/// fresh launch will respawn it).
///
/// "Replace" means `exec` on unix — same pid, same open files, same supervision. Where there is no
/// exec the successor is a separate process and this one exits after starting it; that difference
/// (and what it costs) is documented in `crate::procctl::replace_self`.
fn reexec_gateway(spec: &ModelSpec, port: u16) -> ! {
    let exe = std::env::current_exe().unwrap_or_else(|_| "rozum".into());
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("gateway")
        .arg("--model")
        .arg(&spec.model_id)
        .arg("--n-ctx")
        .arg(spec.n_ctx.to_string())
        .arg("--port")
        .arg(port.to_string());
    if let Err(err) = crate::procctl::replace_self(&mut cmd) {
        crate::obs::log_event(json!({
            "event": "gateway_reload_exec_failed", "error": err.to_string(),
        }));
    }
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
        shutting_down: AtomicBool::new(false),
        warm: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        // Persist per-model usefulness so the warm set adapts across restarts.
        usage: {
            let _ = crate::share::ensure_dir();
            crate::resident::UsageStats::open(crate::share::gateway_dir().join("warm-usage.jsonl"))
        },
        warm_cfg: WarmConfig::new(n_ctx),
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
    // Memory-pressure shedding (runtime-drift half of BUG-003): a reloadable gateway
    // runs the watchdog so it can unload its idle model under host pressure even when
    // no other lifecycle trigger is active.
    let shed_policy = crate::shed::ShedPolicy::from_env();
    let shed_active = shed_policy.enabled && state.sb.can_reload();
    if idle_exit.is_some() || unload_on_idle || launch_managed || shed_active {
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
                    let exit_reason = if launch_managed && seen_lease {
                        // A client lease was observed and is now gone -> agent exited.
                        // Launch-managed only: a manual `rozum gateway` must outlive
                        // transient lease gaps between `rozum launch` invocations
                        // (it only exits via `idle_secs`); otherwise the shared
                        // "load once, reuse across tasks" gateway self-exits the
                        // moment one agent detaches.
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

                // 1b. Cooperative preemption (P4): a higher-priority load asked us to yield our RAM.
                // The instant we're idle (no in-flight request, nothing generating), exit — the OS
                // frees our reservation + model RAM for it. NEVER mid-generation (the idle guard).
                // Auto-requeue: our driver / managed failover reloads when RAM frees again.
                {
                    let mypid = std::process::id();
                    if activity.in_flight.load(Ordering::Relaxed) == 0
                        && sb.generating.load(Ordering::SeqCst) == 0
                        && crate::share::preempt_requested(mypid)
                    {
                        crate::share::clear_preemption(mypid);
                        crate::obs::log_event(serde_json::json!({
                            "event": "gateway_exit", "reason": "preempted",
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

                // 2-pre. Cache-squeeze (smmr-D light lever): under host pressure, free the reclaimable
                // MLX buffer cache (often several GB) BEFORE evicting any warm/primary model — the
                // cheapest graceful reclaim, keeps every model serving. Idle-only (generating==0) so it
                // can't race an in-flight allocation. After the first squeeze the cache is ~0 so repeats
                // free nothing (the freed>0 log fires once); it rebuilds on the next generation.
                if shed_active
                    && sb.generating.load(Ordering::SeqCst) == 0
                    && crate::shed::read_host_pressure() != crate::shed::PressureLevel::Normal
                {
                    let freed = crate::obs::mlx_squeeze_cache();
                    if freed > 0 {
                        crate::obs::log_event(serde_json::json!({
                            "event": "gateway_cache_squeeze", "freed_mb": freed / (1024 * 1024),
                        }));
                    }
                }

                // 2a. Pressure-evict WARM secondaries first (residency-unify U2): under
                // genuine host pressure, free the supplementary co-resident models before
                // touching the primary — graceful degradation that keeps the primary serving.
                // Probe sysctl only when there IS warm to shed (no per-tick cost otherwise).
                if shed_active && !sb.warm.lock().await.is_empty() {
                    if crate::shed::read_host_pressure() != crate::shed::PressureLevel::Normal {
                        crate::obs::log_event(serde_json::json!({
                            "event": "gateway_pressure_evict_warm",
                        }));
                        sb.sweep_idle_warm(0).await; // evict ALL currently-idle warm now
                    }
                }

                // 2b. Memory-pressure shedding: under genuine host pressure, unload an
                // idle resident model EARLIER than the idle timeout so the host degrades
                // gracefully instead of rebooting (runtime-drift half of BUG-003). The
                // primary is the LAST resort (warm shed first, 2a). The `sysctl` pressure
                // probe is skipped unless the model is already idle + not generating, so
                // there is no hot-path cost.
                if shed_active
                    && sb.is_loaded()
                    && sb.generating.load(Ordering::SeqCst) == 0
                    && idle_for >= shed_policy.min_idle_secs
                {
                    let inputs = crate::shed::ShedInputs {
                        pressure: crate::shed::read_host_pressure(),
                        inflight: 0,
                        idle_secs: idle_for,
                    };
                    if crate::shed::should_shed(&inputs, &shed_policy) {
                        crate::obs::log_event(serde_json::json!({
                            "event": "gateway_pressure_unload",
                            "pressure": format!("{:?}", inputs.pressure),
                            "idle_secs": idle_for,
                        }));
                        if let Err(e) = sb.unload().await {
                            tracing::warn!(error = %e, "pressure-unload failed");
                        }
                    }
                }

                // 3. Evict warm secondary residents (multislot) idle past the unload timeout,
                // freeing their RAM. The primary has its own idle-unload above.
                if unload_after > 0 {
                    sb.sweep_idle_warm(unload_after).await;
                }
            }
        });
    }

    // Declarative co-residency (residency-unify U3): warm a named model set at startup so
    // "run these N models at once" is declarative, not lazy-on-first-request — each declared
    // model is warmed up front (no per-model cold-start latency). Background, so the primary
    // serves immediately; each warm-load is admission-gated (won't overcommit — overflow is
    // refused gracefully) and the governor backstops. Opt-in: `ROZUM_WARM_MODELS=spec1,spec2`.
    // (They follow the normal adaptive warm lifecycle — idle/pressure-evictable.)
    if let Ok(list) = std::env::var("ROZUM_WARM_MODELS") {
        let specs: Vec<String> = list
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !specs.is_empty() {
            let sb = Arc::clone(&state.sb);
            tokio::spawn(async move {
                for spec in specs {
                    let event = match sb.ensure_warm(&spec).await {
                        Some(_) => "warm_preloaded",
                        None => "warm_preload_skipped", // not warmable / didn't fit / no builder
                    };
                    crate::obs::log_event(json!({ "event": event, "model": spec }));
                }
            });
        }
    }

    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/ready", get(ready_handler))
        .route("/v1/models", get(models_handler))
        .route("/v1/admit", get(admit_handler))
        .route("/stats", get(stats_handler))
        .route("/v1/chat/completions", post(oai_chat_handler))
        .route("/v1/responses", post(responses_handler))
        .route("/v1/messages", post(anthropic_handler))
        .route("/control/status", get(control_status_handler))
        .route("/control/switch", post(control_switch))
        .route("/control/unload", post(control_unload))
        .route("/control/reload", post(control_reload))
        .layer(middleware::from_fn(poison_layer))
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer))
        .with_state(state.clone());

    tracing::info!(addr = ?listener.local_addr().ok(), "rozum gateway listening");
    // BOUND THE DRAIN. `with_graceful_shutdown` waits for every in-flight connection, and a
    // long-lived stream — the phone chat's SSE, an agent holding keep-alive — never ends on its
    // own, so the wait is unbounded. Measured 2026-08-13: the resident gateway took a shutdown
    // signal at 20:54:56 and the next one started at 20:59:56 — the port was dead for five minutes
    // while launchd (KeepAlive) could do nothing, because the old process had not exited yet. Four
    // bench tasks failed against that hole in zero seconds each.
    //
    // The bound NEVER fires mid-generation. A hard exit during a live Metal eval is what rebooted
    // this host through an IOGPU double-free (BUGS.md BUG-001), so a generation that is still
    // running keeps the process alive and says so every 30s. What the deadline collects is the
    // opposite case, and the common one: nothing computing, one socket someone forgot to close.
    let sb_drain = Arc::clone(&state.sb);
    let server = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal(state.sb.clone()));
    let result = tokio::select! {
        r = server => r,
        _ = drain_deadline(sb_drain) => {
            eprintln!(
                "rozum gateway: drain deadline ({DRAIN_DEADLINE_SECS}s) reached with nothing \
                 generating — closing the connections that outlived the shutdown and exiting"
            );
            Ok(())
        }
    };
    if let Some(pid) = registered_pid {
        crate::share::remove_active_if_mine(pid);
    }
    result?;
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

/// The frozen bytes of all three dialects — the gate `plugin-wireprotocol` is judged by.
/// Its own file because it carries a data file beside it, not because it is a separate subject.
#[cfg(test)]
#[path = "wire_golden_tests.rs"]
mod wire_golden;

#[cfg(test)]
mod tests {
    use super::*;
    // Used only by these tests — at file scope the LIB build warns, because the production code
    // that needed them moved to `serving.rs` and `auto_context.rs`.
    use crate::backend::{ChatStream, Message, ModelError, Role};
    use crate::loopbreak::{detect_stuck_loop, synthetic_stop_stream};
    // Used only by these tests — keeping it at file scope makes the LIB build warn.
    use crate::backend::ContentBlock;
    use crate::backend::{ChatEvent, HelloBackend, StopReason};
    // Imported explicitly, NOT via `use super::*`: the monolith split moved the apply_patch /
    // codex-tool-arg rewriters to `codex_patch` and the SSE types to the dialect modules, which
    // removed gateway.rs's own imports of both — and a glob of `super` can only re-export what
    // `super` still imports. The regression corpus below is the only remaining caller here.
    use crate::codex_patch::*;
    use axum::response::sse::Event;
    use std::convert::Infallible;

    #[test]
    fn anthropic_effort_maps_what_claude_code_actually_sends() {
        // The two shapes captured from a real run against a fake `/v1/messages?beta=true`.
        let oc = |v: serde_json::Value| Some(v);
        assert_eq!(
            anthropic_effort(&oc(json!({"effort": "high", "format": {"type": "json_schema"}})), &None)
                .as_deref(),
            Some("high")
        );
        // `xhigh` is a client asking for MORE. The shared validator rejects it, which would have
        // produced `None` → the template's own default `medium` → LESS. Clamp, do not drop.
        assert_eq!(anthropic_effort(&oc(json!({"effort": "xhigh"})), &None).as_deref(), Some("high"));
        assert_eq!(anthropic_effort(&oc(json!({"effort": "MEDIUM"})), &None).as_deref(), Some("medium"));
        // Nothing invented for a level the downstream template does not know.
        assert_eq!(anthropic_effort(&oc(json!({"effort": "ultra"})), &None), None);
        assert_eq!(anthropic_effort(&None, &None), None);
    }

    #[test]
    fn thinking_disabled_never_raises_the_level() {
        // `{"type":"disabled"}` asks for no reasoning; the only lever is a LEVEL with no "off", so
        // the honest answer is to refuse to raise it rather than to invent one.
        assert_eq!(
            anthropic_effort(&Some(json!({"effort": "xhigh"})), &Some(json!({"type": "disabled"}))),
            None
        );
        // `adaptive` is not "off" — it is the model choosing, so the requested level still applies.
        assert_eq!(
            anthropic_effort(
                &Some(json!({"effort": "high"})),
                &Some(json!({"type": "adaptive", "display": "omitted"}))
            )
            .as_deref(),
            Some("high")
        );
    }

    #[test]
    fn anthropic_structured_output_reads_the_same_nesting_as_responses() {
        // `output_config.format` is `{"type":"json_schema","schema":{…}}` — the Responses shape,
        // which `parse_text_format` already handles. This asserts the reuse rather than a new parser.
        let oc = json!({"format": {"type": "json_schema",
                                   "schema": {"type": "object", "properties": {"title": {"type": "string"}}}}});
        assert_eq!(
            parse_text_format(&oc),
            Some(json!({"type": "object", "properties": {"title": {"type": "string"}}}))
        );
        // A request without it stays unconstrained, which is what every client that sends no schema
        // has always got.
        assert_eq!(parse_text_format(&Value::Null), None);
    }

    #[test]
    fn reasoning_effort_of_parses_and_validates() {
        let eff = |v: serde_json::Value| reasoning_effort_of(&Some(v));
        assert_eq!(eff(json!({"effort": "high"})).as_deref(), Some("high"));
        assert_eq!(eff(json!({"effort": "LOW"})).as_deref(), Some("low")); // lower-cased
        assert_eq!(eff(json!({"effort": "  medium  "})).as_deref(), Some("medium")); // trimmed
        assert_eq!(eff(json!({"effort": "ultra"})), None); // invalid level rejected
        assert_eq!(eff(json!({"summary": "auto"})), None); // no effort key
        assert_eq!(reasoning_effort_of(&None), None); // no reasoning object
    }

    #[test]
    fn codex_lean_prompt_trims_only_load_sensitive_models() {
        // gpt-oss is load-sensitive; the capable tier (35B) is not.
        assert!(model_is_load_sensitive("mlx-community:gpt-oss-20b-MXFP4-Q4"));
        assert!(!model_is_load_sensitive("mlx-community:Qwen3.6-35B-A3B-4bit"));
        // Default (no override): trim ON for gpt-oss when tool-lean is on, OFF for 35B.
        assert!(lean_prompt_on("mlx-community:gpt-oss-20b-MXFP4-Q4", None, true));
        assert!(!lean_prompt_on("mlx-community:Qwen3.6-35B-A3B-4bit", None, true));
        // Tool-lean off → no prompt trim either (whole codex-lean disabled).
        assert!(!lean_prompt_on("mlx-community:gpt-oss-20b-MXFP4-Q4", None, false));
        // Explicit override wins both ways, regardless of model.
        assert!(!lean_prompt_on("mlx-community:gpt-oss-20b-MXFP4-Q4", Some("0"), true));
        assert!(lean_prompt_on("mlx-community:Qwen3.6-35B-A3B-4bit", Some("1"), false));
        // The effective-instructions wrapper: 35B keeps codex's verbatim (no regression).
        let big = "x".repeat(21_000);
        assert_eq!(
            codex_effective_instructions("mlx-community:Qwen3.6-35B-A3B-4bit", Some(&big))
                .as_deref(),
            Some(big.as_str())
        );
    }

    #[test]
    fn published_reservation_counts_shared_reserve_once() {
        const GB: u64 = 1 << 30;
        let reserve = rozum_models::model_source::process_reserve_bytes(0);
        // Footprints are realistic: a full runtime_footprint_bytes is ALWAYS ≥ one reserve
        // (weights + KV + reserve), so build each as reserve + an "active" (weights+KV) part.
        let (a_p, a_w1, a_w2) = (4 * GB, 3 * GB, 2 * GB); // per-model active parts
        let primary = reserve + a_p;
        let warm1 = reserve + a_w1;
        let warm2 = reserve + a_w2;
        // Single model (no warm): unchanged — exactly the primary footprint.
        assert_eq!(published_reservation(primary, &[]), primary);
        // Primary + 1 warm: two footprints each carry one reserve, but the cache+prefill pool is
        // shared → publish Σ fp − ONE redundant reserve = Σ active + ONE reserve.
        assert_eq!(published_reservation(primary, &[warm1]), a_p + a_w1 + reserve);
        // Primary + 2 warm: subtract the (N-1)=2 redundant reserves.
        let pub2 = published_reservation(primary, &[warm1, warm2]);
        assert_eq!(pub2, a_p + a_w1 + a_w2 + reserve, "= Σ active + ONE reserve");
        // SAFETY: never below the real co-resident peak — at least the primary's own footprint,
        // and at least Σ(weights+KV)+one reserve (here equal, since default-cache reserves match).
        assert!(pub2 >= primary, "never under-reserves below the primary footprint");
        assert!(pub2 >= a_p + a_w1 + a_w2, "covers every model's active bytes + a reserve");
    }

    #[test]
    fn determinism_off_is_byte_for_byte_passthrough() {
        // Both knobs off → the client's params are untouched (behaviour-preserving default).
        let s = SamplingParams {
            temperature: Some(1.0),
            top_p: Some(0.95),
            top_k: Some(40),
            ..Default::default()
        };
        let out = apply_determinism(s.clone(), false, None);
        assert_eq!(out.temperature, Some(1.0));
        assert_eq!(out.top_p, Some(0.95));
        assert_eq!(out.top_k, Some(40));
        assert_eq!(out.seed, None, "no seed forced when ROZUM_SAMPLING_SEED unset");
    }

    #[test]
    fn determinism_seed_fills_only_when_unset() {
        // Pins an entropy-seeded request (the matrix case) ...
        let pinned = apply_determinism(
            SamplingParams { temperature: Some(1.0), ..Default::default() },
            false,
            Some(7),
        );
        assert_eq!(pinned.seed, Some(7));
        assert_eq!(pinned.temperature, Some(1.0), "seed pin does not touch temperature");
        // ... but never clobbers a client that genuinely sent its own seed.
        let kept = apply_determinism(
            SamplingParams { seed: Some(123), ..Default::default() },
            false,
            Some(7),
        );
        assert_eq!(kept.seed, Some(123));
    }

    #[test]
    fn determinism_force_greedy_overrides_sampling() {
        // force_greedy → argmax: temperature 0, nucleus/top-k cleared (no RNG at all).
        let out = apply_determinism(
            SamplingParams {
                temperature: Some(1.0),
                top_p: Some(0.9),
                top_k: Some(50),
                ..Default::default()
            },
            true,
            Some(7),
        );
        assert_eq!(out.temperature, Some(0.0));
        assert_eq!(out.top_p, None);
        assert_eq!(out.top_k, None);
        assert_eq!(out.seed, Some(7), "seed still pinned alongside greedy (harmless for argmax)");
    }

    #[tokio::test]
    async fn gen_timeout_aborts_stalled_stream() {
        // A backend that produces nothing for far longer than the inactivity window.
        let inner: ChatStream = Box::pin(async_stream::stream! {
            tokio::time::sleep(Duration::from_secs(30)).await;
            yield Ok(ChatEvent::TextDelta { text: "late".into() });
        });
        let cancel = CancellationToken::new();
        let mut s = with_gen_timeout(inner, cancel.clone(), Duration::from_millis(50));
        match s.next().await {
            Some(Err(ModelError::Timeout(_))) => {}
            other => panic!("expected Timeout, got {other:?}"),
        }
        assert!(cancel.is_cancelled(), "job must be cancelled on timeout");
        assert!(s.next().await.is_none(), "stream ends after the timeout error");
    }

    // ── stuck-loop detector ──────────────────────────────────────────────────
    fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ToolUse { id: id.into(), name: name.into(), input }
    }
    fn tool_err(id: &str, err: bool) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: "x".into(),
                is_error: err,
            }],
        }
    }
    fn asst(block: ContentBlock) -> Message {
        Message { role: Role::Assistant, content: vec![block] }
    }
    /// A successful tool result with specific output — signature 4 reads the output, not
    /// just the error flag, so a test about it cannot use the fixed-content helper.
    fn tool_out(id: &str, content: &str) -> Message {
        Message {
            role: Role::Tool,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn a_verify_loop_whose_output_keeps_changing_is_not_a_loop() {
        // Measured 2026-07-31 on the Qwen3.5-4B matrix: matching on (name, input) alone cut
        // 11 of nadia's 16 cells and 6 of codex's, because an agent told to VERIFY re-runs
        // the same `cargo test` on purpose. Identical command, different output — the files
        // changed underneath it. That is fix -> test -> fix, not a spin.
        let args = json!({ "command": "cargo test" });
        let mut msgs = vec![Message::user("fix both bugs")];
        for i in 0..6 {
            let id = format!("v{i}");
            msgs.push(asst(tool_use(&id, "Bash", args.clone())));
            msgs.push(tool_out(&id, &format!("{} failed", 6 - i)));
        }
        assert!(
            detect_stuck_loop(&msgs).is_none(),
            "re-running one command while its result changes is progress, not a loop"
        );
    }

    #[test]
    fn an_identical_call_with_an_identical_result_still_trips() {
        // The true positive the signature exists for: nothing moves, at all.
        let args = json!({ "command": "cargo test" });
        let mut msgs = vec![Message::user("fix it")];
        for i in 0..5 {
            let id = format!("s{i}");
            msgs.push(asst(tool_use(&id, "Bash", args.clone())));
            msgs.push(tool_out(&id, "2 failed"));
        }
        let reason = detect_stuck_loop(&msgs).expect("identical call AND identical result is a spin");
        assert!(reason.contains("same result"), "{reason}");
    }

    #[test]
    fn stuck_loop_fires_on_three_identical_errored_calls() {
        let args = json!({ "old_string": "s.to_string()", "new_string": "s.chars().rev().collect()" });
        let mut msgs = vec![Message::user("fix the bug")];
        for i in 0..3 {
            let id = format!("call_{i}");
            msgs.push(asst(tool_use(&id, "Edit", args.clone())));
            msgs.push(tool_err(&id, true)); // "String to replace not found"
        }
        assert!(detect_stuck_loop(&msgs).is_some(), "3 identical failing Edits must trip");
    }

    #[test]
    fn stuck_loop_ignores_successes_variation_and_short_runs() {
        let args = json!({ "old_string": "a", "new_string": "b" });
        // (a) identical but SUCCEEDING calls — not stuck (no error).
        let mut ok = vec![Message::user("go")];
        for i in 0..3 {
            let id = format!("c{i}");
            ok.push(asst(tool_use(&id, "Edit", args.clone())));
            ok.push(tool_err(&id, false));
        }
        assert!(detect_stuck_loop(&ok).is_none(), "successful repeats are not a loop");

        // (b) failing but DIFFERENT args at the tail — not stuck.
        let mut varied = vec![Message::user("go")];
        for i in 0..3 {
            let id = format!("v{i}");
            varied.push(asst(tool_use(&id, "Edit", json!({ "old_string": format!("x{i}") }))));
            varied.push(tool_err(&id, true));
        }
        assert!(detect_stuck_loop(&varied).is_none(), "distinct args are not a loop");

        // (c) only two identical failing calls — below threshold.
        let mut two = vec![Message::user("go")];
        for i in 0..2 {
            let id = format!("t{i}");
            two.push(asst(tool_use(&id, "Edit", args.clone())));
            two.push(tool_err(&id, true));
        }
        assert!(detect_stuck_loop(&two).is_none(), "two repeats is below threshold");
    }

    fn asst_text(t: &str) -> Message {
        asst(ContentBlock::Text { text: t.into() })
    }

    #[test]
    fn stuck_loop_fires_on_repeated_assistant_text() {
        // Claude Code's interrupted-tool loop: the same placeholder assistant turn
        // ("[Tool use interrupted]") repeated, with no structured tool blocks at all.
        let mut msgs = vec![Message::user("fix the bug"), asst_text("I'll fix it.")];
        for _ in 0..3 {
            msgs.push(Message::user("(no content)"));
            msgs.push(asst_text("[Tool use interrupted]"));
        }
        assert!(detect_stuck_loop(&msgs).is_some(), "3 identical assistant turns must trip");
    }

    #[test]
    fn stuck_loop_fires_on_alternating_no_progress() {
        // The real CC signature: ping-pong between a re-diagnosis and an interruption —
        // not consecutive, but one text recurs >= threshold within the recent window.
        let msgs = vec![
            asst_text("I'll fix it."),
            asst_text("The bug is in reverse."),
            asst_text("[Tool use interrupted]"),
            asst_text("The bug is in reverse."),
            asst_text("[Tool use interrupted]"),
            asst_text("[Tool use interrupted]"),
        ];
        assert!(detect_stuck_loop(&msgs).is_some(), "3x interrupted in window must trip");
    }

    #[test]
    fn stuck_loop_ignores_distinct_progress() {
        // Normal progress — every assistant turn distinct — must NOT trip.
        let ok = vec![
            asst_text("Reading the file."),
            asst_text("Found the bug in reverse()."),
            asst_text("Applied the fix."),
            asst_text("Ran cargo, it prints olleh."),
            asst_text("Verified, done."),
        ];
        assert!(detect_stuck_loop(&ok).is_none(), "distinct assistant turns are not a loop");
    }

    // ── signature 3: edit-churn / ping-pong ──
    /// One apply_patch-style tool call: `old` → `new` on `file`, as gpt-oss emits it.
    fn patch_call(id: &str, file: &str, old: &str, new: &str) -> Message {
        let body = format!("*** Begin Patch\n*** Update File: {file}\n@@\n-    {old}\n+    {new}\n*** End Patch");
        asst(tool_use(id, "apply_patch", json!({ "command": ["apply_patch", body] })))
    }

    #[test]
    fn stuck_loop_fires_on_pingpong_edit_churn() {
        // gpt-oss toggling collect() <-> collect::<String>() on one file: different,
        // SUCCEEDING patches that sigs 1/2 miss. The 3rd edit re-adds what the 2nd removed.
        let msgs = vec![
            Message::user("fix the bug"),
            patch_call("c0", "src/main.rs", "s.to_string()", "s.chars().rev().collect::<String>()"),
            patch_call("c1", "src/main.rs", "s.chars().rev().collect::<String>()", "s.chars().rev().collect()"),
            patch_call("c2", "src/main.rs", "s.chars().rev().collect()", "s.chars().rev().collect::<String>()"),
        ];
        assert!(detect_stuck_loop(&msgs).is_some(), "ping-pong edit churn must trip");
    }

    #[test]
    fn stuck_loop_fires_on_six_edits_backstop() {
        // Six edits to one file (no strict ping-pong) — the >=6 backstop trips.
        let mut msgs = vec![Message::user("go")];
        for i in 0..6 {
            msgs.push(patch_call(&format!("c{i}"), "src/main.rs", &format!("old{i}"), &format!("new{i}")));
        }
        assert!(detect_stuck_loop(&msgs).is_some(), "six edits to one file must trip backstop");
    }

    #[test]
    fn stuck_loop_ignores_healthy_linear_edits() {
        // A healthy fix: two forward edits to one file, never re-adding a removed line.
        let two = vec![
            Message::user("fix it"),
            patch_call("c0", "src/main.rs", "s.to_string()", "s.chars().rev().collect()"),
            patch_call("c1", "src/lib.rs", "let x = 1;", "let x = 2;"),
        ];
        assert!(detect_stuck_loop(&two).is_none(), "two forward edits to distinct files are not churn");

        // Three forward edits to ONE file but no ping-pong (each removes a fresh line) — below
        // backstop and not circular, so it must NOT trip.
        let fwd = vec![
            Message::user("refactor"),
            patch_call("d0", "src/main.rs", "line_a", "line_a2"),
            patch_call("d1", "src/main.rs", "line_b", "line_b2"),
            patch_call("d2", "src/main.rs", "line_c", "line_c2"),
        ];
        assert!(detect_stuck_loop(&fwd).is_none(), "linear forward edits are not churn");
    }

    /// Claude Code's structured Edit tool: `file_path` + `old_string`/`new_string` (no diff body),
    /// the form that ran to a 67-turn timeout because the codex-format scan couldn't see it.
    fn cc_edit(id: &str, file: &str, old: &str, new: &str) -> Message {
        asst(tool_use(id, "Edit", json!({ "file_path": file, "old_string": old, "new_string": new })))
    }

    #[test]
    fn stuck_loop_fires_on_claude_edit_churn() {
        // Ping-pong: edit #3 re-adds the line edit #2 removed (different, mostly-succeeding edits).
        let pp = vec![
            Message::user("fix the reverse bug"),
            cc_edit("c0", "src/main.rs", "s.to_string()", "s.chars().rev().collect::<String>()"),
            cc_edit("c1", "src/main.rs", "s.chars().rev().collect::<String>()", "s.chars().rev().collect()"),
            cc_edit("c2", "src/main.rs", "s.chars().rev().collect()", "s.chars().rev().collect::<String>()"),
        ];
        assert!(detect_stuck_loop(&pp).is_some(), "claude Edit ping-pong churn must trip");

        // Six Edits to one file (no strict ping-pong) → the >=6 backstop trips.
        let mut six = vec![Message::user("fix")];
        for i in 0..6 {
            six.push(cc_edit(&format!("e{i}"), "src/main.rs", &format!("old{i}"), &format!("new{i}")));
        }
        assert!(detect_stuck_loop(&six).is_some(), "six claude Edits to one file must trip backstop");

        // A Write-churn loop (re-overwriting one file) also trips the backstop.
        let mut writes = vec![Message::user("create it")];
        for i in 0..6 {
            writes.push(asst(tool_use(
                &format!("w{i}"),
                "Write",
                json!({ "file_path": "src/main.rs", "content": format!("fn main() {{ /* v{i} */ }}") }),
            )));
        }
        assert!(detect_stuck_loop(&writes).is_some(), "six Writes to one file must trip backstop");

        // Healthy: two forward Edits to DISTINCT files → not churn.
        let ok = vec![
            Message::user("fix"),
            cc_edit("a", "src/main.rs", "x", "y"),
            cc_edit("b", "src/lib.rs", "p", "q"),
        ];
        assert!(detect_stuck_loop(&ok).is_none(), "two forward CC edits to distinct files are not churn");
    }

    /// Signature 4 — the no-stop-after-success loop: the model re-runs the SAME verification
    /// command / re-reads the same file turn after turn, with varying prose and no errors, so
    /// signatures 1–3 (errored-consecutive / identical-text / edit-churn) all miss it. Modeled on
    /// a real Qwen3-Coder-30B transcript (`cargo run -- hello 2>&1` issued 8×, interleaved Reads),
    /// the shape that burned 482 tool calls to timeout on a task it had already passed.
    #[test]
    fn stuck_loop_fires_on_repeated_bash_verification() {
        let run = json!({ "command": "cargo run -- hello 2>&1", "description": "run it" });
        let mut msgs = vec![Message::user("debug the failing program")];
        // 4 byte-identical Bash runs, each its own turn with DIFFERENT prose and a non-error
        // result, interleaved with distinct Reads — only the windowed-recurrence signature sees it.
        for i in 0..4 {
            msgs.push(Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text { text: format!("Let me try approach {i} and run it again.") },
                    tool_use(&format!("b{i}"), "Bash", run.clone()),
                ],
            });
            msgs.push(tool_err(&format!("b{i}"), false)); // succeeds (produces output, not an error)
            msgs.push(asst(tool_use(&format!("r{i}"), "Read", json!({ "file_path": "src/main.rs", "offset": i }))));
            msgs.push(tool_err(&format!("r{i}"), false));
        }
        assert!(detect_stuck_loop(&msgs).is_some(), "4 identical Bash verifications must trip sig-4");
    }

    #[test]
    fn stuck_loop_ignores_few_identical_verifications() {
        // Healthy convergence: build a few times while applying DIFFERENT fixes between runs.
        // 3 identical builds (below the 4-threshold) interleaved with real forward edits — must
        // NOT trip, preserving the "3 identical calls are not a loop" contract for non-edits too.
        let build = json!({ "command": "cargo build", "description": "build" });
        let mut msgs = vec![Message::user("make it compile")];
        for i in 0..3 {
            msgs.push(asst(tool_use(&format!("b{i}"), "Bash", build.clone())));
            msgs.push(tool_err(&format!("b{i}"), false));
            msgs.push(cc_edit(&format!("e{i}"), "src/main.rs", &format!("bad{i}"), &format!("good{i}")));
        }
        assert!(detect_stuck_loop(&msgs).is_none(), "3 identical builds amid real edits are not a loop");
    }

    #[tokio::test]
    async fn synthetic_stop_stream_ends_with_endturn() {
        let mut s = synthetic_stop_stream("stopping".into());
        assert!(matches!(s.next().await, Some(Ok(ChatEvent::TextDelta { .. }))));
        assert!(matches!(
            s.next().await,
            Some(Ok(ChatEvent::Done { stop_reason: StopReason::EndTurn, .. }))
        ));
        assert!(s.next().await.is_none());
    }

    #[tokio::test]
    async fn gen_timeout_passes_normal_stream_through() {
        let inner: ChatStream = Box::pin(async_stream::stream! {
            yield Ok(ChatEvent::TextDelta { text: "hi".into() });
            yield Ok(ChatEvent::Done {
                stop_reason: StopReason::EndTurn,
                input_tokens: 1,
                output_tokens: 1,
            });
        });
        let cancel = CancellationToken::new();
        let mut s = with_gen_timeout(inner, cancel.clone(), Duration::from_secs(5));
        let mut n = 0;
        while let Some(ev) = s.next().await {
            assert!(ev.is_ok());
            n += 1;
        }
        assert_eq!(n, 2);
        assert!(!cancel.is_cancelled(), "no timeout => no cancel");
    }

    #[tokio::test]
    async fn gen_timeout_zero_disables() {
        let inner: ChatStream = Box::pin(async_stream::stream! {
            yield Ok(ChatEvent::TextDelta { text: "x".into() });
        });
        let cancel = CancellationToken::new();
        let mut s = with_gen_timeout(inner, cancel.clone(), Duration::from_secs(0));
        assert!(matches!(s.next().await, Some(Ok(ChatEvent::TextDelta { .. }))));
        assert!(!cancel.is_cancelled());
    }
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
        let sse = oai_sse_stream(stream, cancel, "hello".to_owned(), None, false);
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

    // ── Contract-1: tool_choice ─────────────────────────────────────────────

    #[test]
    fn tool_choice_parse_openai() {
        assert_eq!(parse_oai_tool_choice(&Value::Null), ToolChoice::Auto);
        assert_eq!(parse_oai_tool_choice(&json!("auto")), ToolChoice::Auto);
        assert_eq!(parse_oai_tool_choice(&json!("none")), ToolChoice::None);
        assert_eq!(parse_oai_tool_choice(&json!("required")), ToolChoice::Required);
        // Chat form (nested under `function`).
        assert_eq!(
            parse_oai_tool_choice(&json!({"type": "function", "function": {"name": "f"}})),
            ToolChoice::Named("f".into())
        );
        // Responses form (flat `name`).
        assert_eq!(
            parse_oai_tool_choice(&json!({"type": "function", "name": "g"})),
            ToolChoice::Named("g".into())
        );
    }

    #[test]
    fn tool_choice_parse_anthropic() {
        assert_eq!(parse_anthropic_tool_choice(&Value::Null), ToolChoice::Auto);
        assert_eq!(parse_anthropic_tool_choice(&json!({"type": "auto"})), ToolChoice::Auto);
        assert_eq!(parse_anthropic_tool_choice(&json!({"type": "any"})), ToolChoice::Required);
        assert_eq!(parse_anthropic_tool_choice(&json!({"type": "none"})), ToolChoice::None);
        assert_eq!(
            parse_anthropic_tool_choice(&json!({"type": "tool", "name": "f"})),
            ToolChoice::Named("f".into())
        );
    }

    #[test]
    fn response_format_parsing() {
        assert_eq!(parse_response_format(&Value::Null), None);
        assert_eq!(parse_response_format(&json!({"type": "text"})), None);
        assert_eq!(
            parse_response_format(&json!({"type": "json_object"})),
            Some(json!({"type": "object"}))
        );
        let schema = json!({"type": "object", "properties": {"x": {"type": "integer"}}});
        assert_eq!(
            parse_response_format(&json!({
                "type": "json_schema",
                "json_schema": {"name": "r", "schema": schema}
            })),
            Some(schema)
        );
        // json_schema without an explicit schema falls back to "any object".
        assert_eq!(
            parse_response_format(&json!({"type": "json_schema", "json_schema": {}})),
            Some(json!({"type": "object"}))
        );
    }

    #[test]
    fn tool_choice_apply_semantics() {
        let mk = |n: &str| ToolDef {
            name: n.into(),
            description: String::new(),
            input_schema: json!({"type": "object"}),
        };
        let tools = || vec![mk("a"), mk("b")];
        assert_eq!(apply_tool_choice(tools(), &ToolChoice::Auto).len(), 2);
        assert_eq!(apply_tool_choice(tools(), &ToolChoice::Required).len(), 2);
        assert_eq!(apply_tool_choice(tools(), &ToolChoice::None).len(), 0);
        let named = apply_tool_choice(tools(), &ToolChoice::Named("b".into()));
        assert_eq!(named.len(), 1);
        assert_eq!(named[0].name, "b");
        // Naming a tool the client never defined yields an empty set (predictable).
        assert_eq!(apply_tool_choice(tools(), &ToolChoice::Named("z".into())).len(), 0);
    }

    // ── Contract-1: tool-call response shapes ───────────────────────────────

    /// A backend stream that emits one tool call (`start`/`delta`/`end`) then `Done(ToolUse)`.
    fn tool_call_stream(name: &str, args: &str) -> ChatStream {
        let evs: Vec<crate::backend::ModelResult<ChatEvent>> = vec![
            Ok(ChatEvent::ToolUseStart { id: "call_0".into(), name: name.into() }),
            Ok(ChatEvent::ToolUseDelta {
                id: "call_0".into(),
                input_json_delta: args.into(),
            }),
            Ok(ChatEvent::ToolUseEnd { id: "call_0".into() }),
            Ok(ChatEvent::Done {
                input_tokens: 5,
                output_tokens: 7,
                stop_reason: StopReason::ToolUse,
            }),
        ];
        Box::pin(futures::stream::iter(evs))
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn oai_collect_tool_call_shape() {
        let stream = tool_call_stream("get_weather", "{\"city\":\"Paris\"}");
        let resp = oai_collect(stream, CancellationToken::new(), "m", None).await;
        let v = body_json(resp).await;
        assert_eq!(v["object"], "chat.completion");
        let choice = &v["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert!(choice["message"]["content"].is_null());
        let tc = &choice["message"]["tool_calls"][0];
        assert_eq!(tc["id"], "call_0");
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "get_weather");
        assert_eq!(tc["function"]["arguments"], "{\"city\":\"Paris\"}");
    }

    #[tokio::test]
    async fn anthropic_collect_tool_use_shape() {
        let stream = tool_call_stream("get_weather", "{\"city\":\"Paris\"}");
        let resp = anthropic_collect(stream, CancellationToken::new(), "m", None).await;
        let v = body_json(resp).await;
        assert_eq!(v["type"], "message");
        assert_eq!(v["stop_reason"], "tool_use");
        let block = &v["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["id"], "call_0");
        assert_eq!(block["name"], "get_weather");
        // `input` is parsed back to a JSON object, not a string.
        assert_eq!(block["input"]["city"], "Paris");
    }

    #[test]
    fn anthropic_tool_result_maps_to_tool_role() {
        // Anthropic carries a tool result inside a `user` message; it must become a
        // Role::Tool message so the Qwen3 template renders it as a tool response (not a
        // user turn). Mirrors the OpenAI/Responses convention.
        let msgs = vec![
            AnthropicMsg { role: "user".into(), content: json!("fix the bug") },
            AnthropicMsg {
                role: "assistant".into(),
                content: json!([{"type": "tool_use", "id": "t1", "name": "Read",
                                 "input": {"file_path": "lib.rs"}}]),
            },
            AnthropicMsg {
                role: "user".into(),
                content: json!([{"type": "tool_result", "tool_use_id": "t1",
                                 "content": "a - b"}]),
            },
        ];
        let out = anthropic_messages_to_internal(None, &msgs);
        assert_eq!(out.len(), 3);
        assert!(matches!(out[0].role, Role::User));
        assert!(matches!(out[1].role, Role::Assistant));
        assert!(matches!(out[2].role, Role::Tool), "tool_result must map to Role::Tool");
        match &out[2].content[0] {
            ContentBlock::ToolResult { tool_use_id, content, .. } => {
                assert_eq!(tool_use_id, "t1");
                assert_eq!(content, "a - b");
            }
            _ => panic!("expected ToolResult in tool turn"),
        }
    }

    #[test]
    fn anthropic_tool_result_then_text_splits_in_order() {
        // A user message with a tool_result followed by text splits into Tool then User,
        // preserving order.
        let msgs = vec![AnthropicMsg {
            role: "user".into(),
            content: json!([
                {"type": "tool_result", "tool_use_id": "t1", "content": "done"},
                {"type": "text", "text": "now also add a test"},
            ]),
        }];
        let out = anthropic_messages_to_internal(None, &msgs);
        assert_eq!(out.len(), 2);
        assert!(matches!(out[0].role, Role::Tool));
        assert!(matches!(out[1].role, Role::User));
        match &out[1].content[0] {
            ContentBlock::Text { text } => assert_eq!(text, "now also add a test"),
            _ => panic!("expected trailing user text"),
        }
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
        // Responses tools are FLAT (no nested `function`). Use a tool the default codex-lean
        // filter keeps (`shell`), so this exercises the flat→ToolDef mapping regardless of
        // `ROZUM_CODEX_LEAN` (which defaults ON and drops non-coding tools like `get_weather`).
        let tools = vec![RespTool {
            kind: Some("function".into()),
            name: Some("shell".into()),
            description: Some("Run a shell command".into()),
            parameters: Some(json!({ "type": "object" })),
        }];
        let defs = responses_tools_to_internal(&tools);
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "shell");
    }

    #[test]
    fn codex_tool_capture_truncates_on_utf8_boundary() {
        let truncated = capture_text_json("aébc", 3);
        assert_eq!(truncated["text"], "aé");
        assert_eq!(truncated["bytes"], 5);
        assert_eq!(truncated["truncated"], true);

        let uncapped = capture_text_json("aébc", 0);
        assert_eq!(uncapped["text"], "aébc");
        assert_eq!(uncapped["bytes"], 5);
        assert_eq!(uncapped["truncated"], false);
    }

    #[test]
    fn codex_tool_capture_records_raw_and_final_call_shape() {
        let event = codex_tool_call_capture_json(
            "collect",
            "resp_1",
            "call_1",
            "apply_patch",
            "exec_command",
            "{\"patch\":\"*** Begin Patch\"}",
            "{\"cmd\":\"patch -p0 --fuzz=3\"}",
            true,
            false,
        );

        assert_eq!(event["event"], "codex_tool_call");
        assert_eq!(event["endpoint"], "/v1/responses");
        assert_eq!(event["source"], "collect");
        assert_eq!(event["response_id"], "resp_1");
        assert_eq!(event["call_id"], "call_1");
        assert_eq!(event["raw_name"], "apply_patch");
        assert_eq!(event["emitted_name"], "exec_command");
        assert_eq!(event["reroute_apply_patch"], true);
        assert_eq!(event["apply_patch_is_tool"], false);
        assert_eq!(event["args_changed"], true);
        assert_eq!(event["raw_args"]["text"], "{\"patch\":\"*** Begin Patch\"}");
        assert_eq!(event["final_args"]["text"], "{\"cmd\":\"patch -p0 --fuzz=3\"}");
    }

    #[test]
    fn apply_patch_bridge_rewrites_unified_diff() {
        // The exact malformed patch a local model emitted (matrix-failure-analysis Finding 4):
        // codex `*** Begin Patch` envelope but unified-diff headers inside.
        let patch = "*** Begin Patch\n--- src/main.rs\n+++ src/main.rs\n@@ -4,7 +4,7 @@\n \
            /// Reverse a string by characters.\n fn reverse(s: &str) -> String {\n\
            -    // BUG: returns the input unchanged.\n-    s.to_string()\n\
            +    s.chars().rev().collect()\n }\n\n fn main() {\n*** End Patch";
        let out = rewrite_unified_diff_to_apply_patch(patch);
        assert!(out.contains("*** Update File: src/main.rs"), "got: {out}");
        assert!(!out.contains("--- src/main.rs") && !out.contains("+++ "), "headers not removed: {out}");
        assert!(!out.contains("@@"), "unified hunk header not dropped: {out}");
        // The (correct) change + context lines survive verbatim (codex locates via context).
        assert!(out.contains("-    s.to_string()"));
        assert!(out.contains("+    s.chars().rev().collect()"));
        assert!(out.contains(" fn reverse(s: &str) -> String {"));
        assert!(out.contains("*** Begin Patch") && out.contains("*** End Patch"));

        // Hybrid the model also emits: correct `*** Update File:` header but a unified `@@ -n,m @@`
        // hunk line (the NEW-binary repro: codex "Failed to find context '-3,7 +3,7 @@'").
        let hybrid = "*** Begin Patch\n*** Update File: src/main.rs\n@@ -3,7 +3,7 @@\n \
            fn reverse(s: &str) -> String {\n-    s.to_string()\n+    s.chars().rev().collect()\n*** End Patch";
        let h = rewrite_unified_diff_to_apply_patch(hybrid);
        assert!(h.contains("*** Update File: src/main.rs") && !h.contains("@@"), "hybrid not fixed: {h}");
        assert!(h.contains("+    s.chars().rev().collect()"));

        // Well-formed codex patches and non-patch args are untouched by the V4A fallback.
        let ok = "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** End Patch";
        assert_eq!(rewrite_unified_diff_to_apply_patch(ok), ok);
        assert_eq!(normalize_codex_tool_args("{\"command\":\"ls -l\"}"), "{\"command\":\"ls -l\"}");
    }

    #[test]
    fn apply_patch_method_b_rewrites_to_patch_fuzz() {
        // The model's REAL apply_patch command (the run codex's V4A rejected with "Failed to find
        // context"). Method B reconstructs a minimal unified diff + `patch --fuzz`.
        let cmd = "apply_patch \"*** Begin Patch\n*** Update File: src/main.rs\n@@ -3,7 +3,7 @@\n \
            /// Reverse a string by characters.\n fn reverse(s: &str) -> String {\n\
            -    // BUG: returns the input unchanged.\n-    s.to_string()\n\
            +    s.chars().rev().collect()\n }\n\n fn main() {\n*** End Patch\"";
        let rw = rewrite_apply_patch_command(cmd).expect("reconstructable");
        assert!(rw.starts_with("patch -p0 --fuzz=3 -N --forward <<'ROZUM_PATCH_EOF'"), "got: {rw}");
        assert!(rw.contains("--- src/main.rs") && rw.contains("+++ src/main.rs"));
        assert!(rw.contains("@@ -1,"), "no reconstructed hunk header: {rw}");
        assert!(rw.contains("-    s.to_string()") && rw.contains("+    s.chars().rev().collect()"));
        assert!(rw.contains("ROZUM_PATCH_EOF\n"), "patch heredoc not closed: {rw}");
        // whitespace-tolerant fallback appended after the patch heredoc
        assert!(rw.contains("$f.rej") && rw.contains("ROZUM_PY_EOF"), "ws fallback missing: {rw}");
        assert!(rewrite_apply_patch_command("cargo run -- hello").is_none());

        // End-to-end through the args JSON: apply_patch command → patch --fuzz, no apply_patch left.
        let args = json!({ "command": ["zsh", "-lc", cmd] }).to_string();
        let fixed = normalize_codex_tool_args(&args);
        assert!(fixed.contains("patch -p0 --fuzz"), "Method B not applied: {fixed}");
        assert!(!fixed.contains("apply_patch"), "apply_patch should be gone: {fixed}");
    }

    #[test]
    fn apply_patch_json_wrapped_patches_flag_creates_files() {
        // gpt-oss (OpenAI tool surface) emits the create as a JSON payload under `-patches`, with the
        // V4A body JSON-escaped (`\n`, `\"`). Before the fix this fell through the shell-unescape path
        // (literal `\n` → no `*** Add File:` lines parsed → apply_patch ran raw → "accepts exactly one
        // argument" → nothing written, matrix rc11). Now each JSON `content` is decoded → file writes.
        let cmd = r#"apply_patch -patches '[{"content":"*** Begin Patch\n*** Add File: Cargo.toml\n+[package]\n+name = \"reverse-cli\"\n*** Add File: src/main.rs\n+fn main() { println!(\"hi\"); }\n*** End Patch"}]'"#;
        let rw = rewrite_apply_patch_command(cmd).expect("JSON-wrapped apply_patch should rewrite");
        // Both files created via the shared synth_create_command heredoc.
        assert!(rw.contains("cat > 'Cargo.toml'"), "Cargo.toml not created: {rw}");
        assert!(rw.contains("cat > 'src/main.rs'"), "src/main.rs not created: {rw}");
        // Content JSON-DECODED: real quotes, no literal `\n`, no V4A directive leaking through.
        assert!(rw.contains("name = \"reverse-cli\""), "quotes not decoded: {rw}");
        assert!(rw.contains("[package]"), "manifest body missing: {rw}");
        assert!(!rw.contains("\\n"), "literal backslash-n leaked (not decoded): {rw}");
        assert!(!rw.contains("*** Add File:"), "V4A directive leaked into output: {rw}");
        // A raw (non-JSON) apply_patch shell string has no JSON bracket → falls back to shell path.
        assert!(
            rewrite_json_wrapped_apply_patch("apply_patch \"*** Begin Patch\n*** End Patch\"").is_none()
        );
    }

    #[test]
    fn apply_patch_block_decodes_unicode_escaped_operators() {
        // gpt-oss over-escapes `>`/`&`/`<` in the patch body (observed in debug edits): a context
        // line `pub fn add(a,b) -> i32 {` arrives as `... -> i32 {`, which would never match
        // the file's `->`. apply_patch_block_to_fuzz must decode it so `patch` finds the context.
        let block = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n\
            -pub fn add(a: i32, b: i32) -\\u003e i32 {\n-    a - b\n\
            +pub fn add(a: i32, b: i32) -> i32 {\n+    a + b\n*** End Patch";
        let cmd = apply_patch_block_to_fuzz(block).expect("reconstructable");
        assert!(!cmd.contains("\\u003e"), "literal \\u003e survived into the patch: {cmd}");
        assert!(cmd.contains("-pub fn add(a: i32, b: i32) -> i32 {"), "decoded context missing: {cmd}");
    }

    #[test]
    fn ws_fallback_lands_a_patch_whose_removed_line_lost_its_indent() {
        // Integration: gpt-oss drops the leading indent on the changed line. `patch` rejects it;
        // the python fallback must still land the fix, preserving the file's own indentation.
        // (Skips cleanly if python3 isn't on the box.)
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("rozum-wsfb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/main.rs"),
            "fn reverse(s: &str) -> String {\n    // BUG\n    s.to_string()\n}\n",
        )
        .unwrap();
        // The model's sloppy block: removed line has NO indentation, wrong @@ line.
        let block = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-s.to_string()\n\
                     +    s.chars().rev().collect()\n*** End Patch";
        let cmd = apply_patch_block_to_fuzz(block).expect("reconstructable");
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&cmd)
            .current_dir(&dir)
            .output()
            .unwrap();
        let got = std::fs::read_to_string(dir.join("src/main.rs")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            got.contains("    s.chars().rev().collect()"),
            "fallback didn't land with preserved indent.\nstderr: {}\nfile:\n{got}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!got.contains("s.to_string()"), "bug line still present:\n{got}");
        assert!(!got.contains(".rej"), "reject file leaked into source");
    }

    #[test]
    fn apply_patch_function_reroutes_to_exec_command() {
        // gpt-oss emits apply_patch as a FUNCTION (`{"command":["apply_patch","<patch>"]}`); codex
        // rejects it ("unsupported call: apply_patch"). We re-route to an exec_command payload.
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n \
            /// Reverse a string by characters.\n fn reverse(s: &str) -> String {\n\
            -    // BUG: returns the input unchanged.\n-    s.to_string()\n\
            +    s.chars().rev().collect()\n }\n*** End Patch";
        let args = json!({ "command": ["apply_patch", patch] }).to_string();
        let out = rewrite_apply_patch_function_args(&args).expect("reroutable");
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["login"], true, "exec_command needs login flag");
        let cmd = v["cmd"].as_str().unwrap();
        assert!(cmd.starts_with("patch -p0 --fuzz=3 -N --forward <<'ROZUM_PATCH_EOF'"), "got: {cmd}");
        assert!(cmd.contains("-    s.to_string()") && cmd.contains("+    s.chars().rev().collect()"));
        assert!(!cmd.contains("apply_patch"), "Method B should leave no apply_patch: {cmd}");

        // The `{"input": "<patch>"}` shape is also accepted.
        assert!(rewrite_apply_patch_function_args(&json!({ "input": patch }).to_string()).is_some());
        // A non-patch exec call is left untouched (None → caller keeps the original args).
        let plain = json!({ "cmd": "cargo run", "login": true }).to_string();
        assert!(rewrite_apply_patch_function_args(&plain).is_none());

        // B1: a multi-file CREATE arriving as a function-call ARRAY (no patch string) — Devstral's
        // captured `{patches:[{op:Add,path,content}]}` form — must synthesize writes, not be lost.
        let fnarr = json!({
            "patches": [
                { "op": "Add", "path": "Cargo.toml", "content": "[package]\nname = \"rpn-calc\"" },
                { "op": "Add", "path": "src/main.rs", "content": "fn main() {}" }
            ]
        })
        .to_string();
        let fo: Value = serde_json::from_str(&rewrite_apply_patch_function_args(&fnarr).expect("fn-array reroutable")).unwrap();
        assert_eq!(fo["login"], true);
        let fc = fo["cmd"].as_str().unwrap();
        assert!(fc.contains("cat > 'Cargo.toml'") && fc.contains("cat > 'src/main.rs'"), "fn-array not synthesized: {fc}");
        assert!(!fc.contains("apply_patch"), "no bare apply_patch: {fc}");
    }

    #[test]
    fn apply_patch_function_decodes_unicode_escapes() {
        // gpt-oss (trained on the OpenAI function surface) emits apply_patch as a FUNCTION call and
        // JSON-double-escapes operators in the body: `&`→&, `<`→<, `>`→>. A Rust fix
        // is full of these (`&str`, `&arg`, `collect::<String>()`, `->`). The function-call reroute
        // must decode them or they land LITERALLY in the file and break compilation. (The shell-cmd
        // path already decodes; this path did not — the codex×gpt-oss corruption.)
        let patch = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n \
            fn reverse(s: \\u0026str) -\\u003e String {\n\
            -    s.to_string()\n+    s.chars().rev().collect::\\u003cString\\u003e()\n }\n*** End Patch";
        let args = json!({ "command": ["apply_patch", patch] }).to_string();
        let out = rewrite_apply_patch_function_args(&args).expect("reroutable");
        let cmd: String = serde_json::from_str::<Value>(&out).unwrap()["cmd"].as_str().unwrap().into();
        assert!(!cmd.contains("\\u00"), "literal \\uXXXX escape survived into the patch: {cmd}");
        assert!(cmd.contains("collect::<String>()"), "generic not decoded: {cmd}");
        assert!(cmd.contains("&str") && cmd.contains("-> String"), "&/-> not decoded: {cmd}");
    }

    #[test]
    fn heredoc_redirect_repairs_missing_gt_and_spares_valid_forms() {
        // The exact build-red shape (codex×gpt-oss run OzUnnR): the model's CORRECT final main.rs
        // was sent as `cat src/main.rs <<'EOF' … EOF` WITHOUT `>`, so the write was a no-op read and
        // the file never landed. Repair must insert the `>`.
        let botched = "cat src/main.rs <<'EOF'\nfn main() {\n    let x = 1;\n}\nEOF";
        let fixed = repair_heredoc_write(botched).expect("missing `>` should be repaired");
        assert!(
            fixed.starts_with("cat > src/main.rs <<'EOF'"),
            "redirect not inserted: {fixed}"
        );
        // the body and delimiter are untouched
        assert!(fixed.contains("fn main() {") && fixed.ends_with("EOF"));

        // Negatives — must NOT fire:
        // (a) well-formed write already has `>`
        assert!(repair_heredoc_write("cat > src/main.rs <<'EOF'\nx\nEOF").is_none());
        assert!(repair_heredoc_write("cat >> log.txt <<'EOF'\nx\nEOF").is_none());
        // (b) a plain read (no heredoc) is a real read, leave it
        assert!(repair_heredoc_write("cat src/main.rs").is_none());
        // (c) a stdout heredoc (no file arg) is legitimate
        assert!(repair_heredoc_write("cat <<'EOF'\nhello\nEOF").is_none());
        // (d) a body line that itself starts with `cat … <<` must NOT be rewritten (heredoc-aware)
        let nested = "cat > note.txt <<'EOF'\ncat data.bin <<X is just prose here\nEOF";
        assert!(
            repair_heredoc_write(nested).is_none(),
            "rewrote inside a heredoc body"
        );
        // end-to-end through normalize_codex_tool_args (the real call path)
        let args = json!({ "cmd": botched }).to_string();
        let out = normalize_codex_tool_args(&args);
        let cmd = serde_json::from_str::<Value>(&out).unwrap()["cmd"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(cmd.starts_with("cat > src/main.rs"), "normalize didn't repair: {cmd}");
    }

    #[test]
    fn exec_command_decodes_unicode_escaped_operators() {
        // gptoss-exec-decode-loopbreak (a): the model emits `>` as the JSON escape `>`, so a
        // redirect never happens (`cat > f` lands as a literal token). Decode it back.
        let args = "{\"cmd\":\"cat \\u003e src/main.rs\"}";
        let out = normalize_codex_tool_args(args);
        let cmd = serde_json::from_str::<Value>(&out).unwrap()["cmd"].as_str().unwrap().to_string();
        assert_eq!(cmd, "cat > src/main.rs", "operator not decoded: {cmd}");

        // The argv-array shape too: {"command":["bash","-lc","echo hi > f"]}.
        let args = "{\"command\":[\"bash\",\"-lc\",\"echo hi \\u003e f\"]}";
        let out = normalize_codex_tool_args(args);
        let arr = serde_json::from_str::<Value>(&out).unwrap();
        assert_eq!(arr["command"][2].as_str().unwrap(), "echo hi > f");

        // A normal command with no escapes is untouched (no false rewrite).
        assert_eq!(
            normalize_codex_tool_args("{\"cmd\":\"cargo build\"}"),
            "{\"cmd\":\"cargo build\"}"
        );
    }

    #[test]
    fn empty_exec_args_become_no_op_echo() {
        // gptoss-exec-decode-loopbreak (b): when the model emits empty function-call arguments,
        // codex's router fails with "expected value at line 1 col 1" and retries → runaway loop.
        // We substitute a no-op echo so codex can continue cleanly.
        for empty in &["", "   ", "\t\n"] {
            let out = normalize_codex_tool_args(empty);
            let v: Value = serde_json::from_str(&out)
                .unwrap_or_else(|e| panic!("empty args {empty:?} → not valid JSON: {e}\nout={out}"));
            let cmd = v["cmd"].as_str().expect("cmd field missing");
            assert!(cmd.starts_with("echo"), "expected echo no-op for empty {empty:?}, got: {cmd}");
        }
        // Non-empty non-JSON prose is returned unchanged (characterize live before fixing).
        let prose = "Let me check the file first";
        assert_eq!(normalize_codex_tool_args(prose), prose);
    }

    #[test]
    fn folds_cmd_apply_patch_sibling_and_decodes_unicode() {
        // gpt-oss's DOMINANT edit shape: a bare `apply_patch` command with the patch stranded in a
        // sibling field, and `&`/`>` double-escaped as & / > in the body.
        let args = json!({
            "cmd": "apply_patch",
            "patch": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n \
                fn reverse(s: \\u0026str) -\\u003e String {\n\
                -    s.to_string()\n+    s.chars().rev().collect()\n }\n*** End Patch"
        })
        .to_string();
        let out = normalize_codex_tool_args(&args);
        let v: Value = serde_json::from_str(&out).unwrap();
        let cmd = v["cmd"].as_str().unwrap();
        assert!(cmd.starts_with("patch -p0 --fuzz=3"), "sibling not folded: {cmd}");
        assert!(v.get("patch").is_none(), "the consumed sibling should be gone");
        assert!(cmd.contains("&str") && cmd.contains("-> String"), "unicode not decoded: {cmd}");
        assert!(!cmd.contains("\\u0026"), "literal escape remained: {cmd}");
        assert!(cmd.contains("-    s.to_string()") && cmd.contains("+    s.chars().rev().collect()"));
    }

    #[test]
    fn synthesizes_file_write_from_path_and_content() {
        // Create-from-scratch (Finding 5): gpt-oss routes a write-intent through the codex shell tool
        // as {cmd:"apply_patch", path, content} where `content` is a WHOLE file (not a patch). The
        // apply_patch fold finds nothing → bare apply_patch → the file never lands. We must synthesize
        // a real write so build/test create tasks actually produce the file.
        let body = "[package]\nname = \"hello\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";
        let args = json!({ "cmd": "apply_patch", "path": "Cargo.toml", "content": body }).to_string();
        let out = normalize_codex_tool_args(&args);
        let v: Value = serde_json::from_str(&out).unwrap();
        let cmd = v["cmd"].as_str().unwrap();
        // A real, verbatim write — not a bare apply_patch.
        assert!(!cmd.contains("apply_patch"), "apply_patch should be gone: {cmd}");
        assert!(cmd.contains("cat > 'Cargo.toml'"), "no cat-write synthesized: {cmd}");
        assert!(cmd.contains("mkdir -p"), "parent dir not ensured: {cmd}");
        assert!(cmd.contains("<<'ROZUM_WRITE_EOF'"), "not a quoted heredoc (would expand $/`): {cmd}");
        assert!(cmd.contains("name = \"hello\""), "file body not in the write: {cmd}");
        // The consumed shape keys are removed so codex sees a clean exec_command.
        assert!(v.get("path").is_none() && v.get("content").is_none(), "path/content not consumed");

        // A nested target ensures its directory.
        let nested = json!({ "cmd": "apply_patch", "path": "src/main.rs", "content": "fn main() {}\n" })
            .to_string();
        let n = serde_json::from_str::<Value>(&normalize_codex_tool_args(&nested)).unwrap();
        assert!(n["cmd"].as_str().unwrap().contains("dirname 'src/main.rs'"), "no mkdir for nested path");

        // A real patch in `content` still goes through the patch fold, NOT the raw-write path.
        let patchy = json!({
            "cmd": "apply_patch",
            "content": "*** Begin Patch\n*** Update File: a.rs\n@@\n-x\n+y\n*** End Patch"
        })
        .to_string();
        let p = serde_json::from_str::<Value>(&normalize_codex_tool_args(&patchy)).unwrap();
        let pc = p["cmd"].as_str().unwrap();
        assert!(pc.contains("patch -p0 --fuzz"), "patch content should fold to patch, not cat: {pc}");
        assert!(!pc.contains("ROZUM_WRITE_EOF"), "patch wrongly treated as a raw write: {pc}");

        // No `content` → nothing synthesized (we never invent empty files from a bare path).
        let pathonly = json!({ "cmd": "apply_patch", "path": "Cargo.toml" }).to_string();
        let po = serde_json::from_str::<Value>(&normalize_codex_tool_args(&pathonly)).unwrap();
        assert_eq!(po["cmd"].as_str().unwrap(), "apply_patch", "should be left untouched: {po}");
    }

    #[test]
    fn structured_apply_patch_patches_array_synthesizes_multi_file_writes() {
        // codex's STRUCTURED multi-file create form (the gpt-oss rpn residual, R2.3):
        // {cmd:apply_patch, patches:[{path,content},…]} — each entry is a whole file, no V4A markers.
        // Neither the V4A fold nor the single-file synth fires; without this the bare shim gets JSON it
        // can't parse → nothing lands (rc11). Every entry must become a real file write.
        let args = json!({
            "cmd": "apply_patch",
            "patches": [
                { "path": "Cargo.toml", "content": "[package]\nname = \"rpn-calc\"\nedition = \"2021\"\n[dependencies]" },
                { "path": "src/main.rs", "content": "fn main() {\n    println!(\"{}\", 42);\n}" }
            ]
        })
        .to_string();
        let out = normalize_codex_tool_args(&args);
        let o = serde_json::from_str::<Value>(&out).unwrap();
        let c = o["cmd"].as_str().unwrap();
        assert!(c.contains("cat > 'Cargo.toml'"), "Cargo.toml write missing: {c}");
        assert!(c.contains("cat > 'src/main.rs'"), "src/main.rs write missing: {c}");
        assert!(c.contains("name = \"rpn-calc\""), "content not written verbatim: {c}");
        assert!(o.get("patches").is_none(), "patches key should be consumed: {o}");
        assert!(!c.contains("apply_patch"), "bare apply_patch should be gone: {c}");

        // Devstral uses the SAME structure under a different key: `file_changes` (r3-cumulative capture).
        let dev = json!({
            "cmd": "apply_patch",
            "file_changes": [{ "path": "src/main.rs", "content": "fn main() { println!(\"hi\"); }" }]
        })
        .to_string();
        let od = serde_json::from_str::<Value>(&normalize_codex_tool_args(&dev)).unwrap();
        let cd = od["cmd"].as_str().unwrap();
        assert!(cd.contains("cat > 'src/main.rs'"), "file_changes not synthesized: {cd}");
        assert!(od.get("file_changes").is_none(), "file_changes key should be consumed: {od}");
        assert!(!cd.contains("apply_patch"), "bare apply_patch should be gone: {cd}");

        // Devstral's DOMINANT form (38× in the r3 capture): `patches` with a per-entry `file` key
        // (not `path`) — the whole reason codex×Devstral create still scored rc11 after the file_changes
        // alias. Both the array key AND the path key vary; accept `file`/`filename` too.
        let devfile = json!({
            "cmd": "apply_patch",
            "patches": [{ "file": "Cargo.toml", "content": "[package]\nname = \"reverse-cli\"" }]
        })
        .to_string();
        let of = serde_json::from_str::<Value>(&normalize_codex_tool_args(&devfile)).unwrap();
        let cf = of["cmd"].as_str().unwrap();
        assert!(cf.contains("cat > 'Cargo.toml'"), "patches[{{file}}] not synthesized: {cf}");
        assert!(!cf.contains("apply_patch"), "bare apply_patch should be gone: {cf}");
    }

    #[test]
    fn create_patch_against_absent_file_writes_instead_of_patching() {
        // Create-from-scratch via a PATCH (the dominant coherent gpt-oss shape under top_p clip):
        // the model labels the new file an `*** Update File:` with a bogus `---` "old" side. patch
        // can't update an absent file → .rej, nothing lands. We must create it from the `+` lines.
        let block = "*** Begin Patch\n*** Update File: Cargo.toml\n@@\n---\n+[package]\n\
                     +name = \"reverse-cli\"\n+version = \"0.1.0\"\n+edition = \"2021\"\n*** End Patch";
        let cmd = apply_patch_block_to_fuzz(block).expect("reconstructable");
        // It writes (create-when-absent), it does NOT try to `patch` an absent file.
        assert!(cmd.contains("cat > 'Cargo.toml'"), "no create-write: {cmd}");
        assert!(cmd.contains("[ -e 'Cargo.toml' ] ||"), "create not guarded by absence: {cmd}");
        assert!(cmd.contains("mkdir -p"), "parent dir not ensured: {cmd}");
        assert!(!cmd.contains("patch -p0"), "should not patch an absent file: {cmd}");
        // The `+` content lands verbatim, the bogus `---` old-side is dropped.
        assert!(cmd.contains("name = \"reverse-cli\""), "content missing: {cmd}");
        assert!(!cmd.contains("\n---\n"), "bogus context leaked into the file: {cmd}");

        // A GENUINE edit (real removed line) is byte-identical to before — patch path, no wrapper.
        let edit = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n \
                    fn reverse(s: &str) -> String {\n-    s.to_string()\n+    s.chars().rev().collect()\n }\n*** End Patch";
        let ec = apply_patch_block_to_fuzz(edit).expect("reconstructable");
        assert!(ec.starts_with("patch -p0 --fuzz="), "edit must stay on the patch path: {ec}");
        assert!(!ec.contains("ROZUM_CREATE_EOF"), "edit wrongly treated as create: {ec}");
    }

    #[test]
    fn create_file_directive_writes_new_file() {
        // The DOMINANT gpt-oss create-from-scratch shape: an explicit `*** Create File:` (its variant
        // of the standard `*** Add File:`) with bare content lines. codex can't run bare apply_patch
        // in the jail → file never lands. Turn it into a real write.
        for kw in ["Create File", "Add File"] {
            let block = format!(
                "*** Begin Patch\n*** {kw}: Cargo.toml\n[package]\nname = \"reverse-cli\"\n\
                 version = \"0.1.0\"\nedition = \"2021\"\n*** End Patch"
            );
            let cmd = apply_patch_block_to_fuzz(&block).unwrap_or_else(|| panic!("{kw} not handled"));
            assert!(cmd.contains("cat > 'Cargo.toml'"), "{kw}: no write: {cmd}");
            assert!(cmd.contains("[ -e 'Cargo.toml' ] ||"), "{kw}: not absence-guarded: {cmd}");
            assert!(cmd.contains("name = \"reverse-cli\""), "{kw}: content missing: {cmd}");
            assert!(!cmd.contains("patch -p0"), "{kw}: must not patch: {cmd}");
            assert!(!cmd.contains("*** "), "{kw}: directive leaked into file: {cmd}");
        }

        // `+`-prefixed (strict V4A) content has the prefix stripped.
        let v4a = "*** Begin Patch\n*** Add File: src/main.rs\n+fn main() {}\n*** End Patch";
        let c = apply_patch_block_to_fuzz(v4a).expect("v4a add");
        assert!(c.contains("cat > 'src/main.rs'") && c.contains("fn main() {}"), "v4a add: {c}");
        assert!(!c.contains("+fn main"), "v4a `+` prefix not stripped: {c}");

        // Multi-file in one patch → a create command per file.
        let multi = "*** Begin Patch\n*** Create File: Cargo.toml\n[package]\nname=\"x\"\n\
                     *** Create File: src/main.rs\nfn main() {}\n*** End Patch";
        let m = apply_patch_block_to_fuzz(multi).expect("multi");
        assert!(m.contains("cat > 'Cargo.toml'") && m.contains("cat > 'src/main.rs'"), "multi: {m}");
    }

    #[test]
    fn bare_update_file_block_creates_from_verbatim_body() {
        // gpt-oss dumps a whole NEW file as a fake `*** Update File:` with NO diff markers (seen for
        // src/main.rs, inside a broken `apply_patch <<'…'` heredoc that runs bare → nothing lands).
        // The body verbatim IS the file → create it; indentation must be preserved (not stripped).
        let block = "*** Begin Patch\n*** Update File: src/main.rs\n@@\nfn main() {\n    \
                     let a: Vec<String> = std::env::args().collect();\n    \
                     println!(\"{}\", a[1].chars().rev().collect::<String>());\n}\n*** End Patch";
        let cmd = apply_patch_block_to_fuzz(block).expect("bare body handled");
        assert!(cmd.contains("cat > 'src/main.rs'"), "no create-write: {cmd}");
        assert!(cmd.contains("[ -e 'src/main.rs' ] ||"), "not absence-guarded: {cmd}");
        assert!(!cmd.contains("patch -p0"), "must not patch an absent file: {cmd}");
        assert!(cmd.contains("    let a: Vec<String>"), "indentation not preserved: {cmd}");
        assert!(!cmd.contains("*** "), "patch directive leaked into file: {cmd}");

        // A GENUINE diff (real +/- markers) must NOT be hijacked by the bare-body path.
        let edit = "*** Begin Patch\n*** Update File: src/main.rs\n@@\n \
                    fn reverse(s: &str) -> String {\n-    s.to_string()\n+    s.chars().rev().collect()\n }\n*** End Patch";
        assert!(parse_bare_file_block(edit).is_none(), "real diff wrongly taken as bare body");
        let ec = apply_patch_block_to_fuzz(edit).expect("edit");
        assert!(ec.starts_with("patch -p0 --fuzz="), "edit must stay on the patch path: {ec}");
    }

    #[test]
    fn repair_broken_read_translates_to_cat() {
        // broken reads → cat (the model's intent is the file path)
        assert_eq!(repair_broken_read("sed -n 'src/main.rs'").as_deref(), Some("cat src/main.rs"));
        assert_eq!(
            repair_broken_read("sed -n '1' '1' src/main.rs").as_deref(),
            Some("cat src/main.rs")
        );
        assert_eq!(
            repair_broken_read("sed -n '1,200' src/main.rs").as_deref(),
            Some("cat src/main.rs")
        );
        // intentional edits / transforms / redirects / non-read tools are left untouched
        assert!(repair_broken_read("sed -i 's/a/b/' f.rs").is_none());
        assert!(repair_broken_read("sed -n 's/x/y/' f.rs").is_none());
        assert!(repair_broken_read("echo hi > f.rs").is_none());
        assert!(repair_broken_read("cat src/main.rs").is_none());
        assert!(repair_broken_read("cargo run -- hello").is_none());
        // WELL-FORMED reads are left intact (refine-before-default-on): a valid sed print-script
        // and any head/tail with a file work as written → don't collapse them to a full cat.
        assert!(repair_broken_read("sed -n '1,200p' src/main.rs").is_none(), "valid ranged read must be left");
        assert!(repair_broken_read("sed -n '5p' src/main.rs").is_none());
        assert!(repair_broken_read("head -20 src/main.rs").is_none(), "head with a file works");
        assert!(repair_broken_read("tail -5 src/main.rs").is_none());
    }

    #[test]
    fn decode_unicode_escapes_only_bare_4hex() {
        assert_eq!(decode_unicode_escapes("a \\u0026 b"), "a & b");
        assert_eq!(decode_unicode_escapes("plain text"), "plain text");
        // Rust's brace form `\u{..}` is valid source — must be left intact.
        assert_eq!(decode_unicode_escapes("x\\u{26}y"), "x\\u{26}y");
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
        let sse = responses_sse_stream(stream, cancel, "m".to_owned(), None, false);
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
        test_sb_cfg(builder, loaded, WarmConfig::default())
    }

    fn test_sb_cfg(
        builder: Option<BackendBuilder>,
        loaded: bool,
        warm_cfg: WarmConfig,
    ) -> Arc<Switchboard> {
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
            shutting_down: AtomicBool::new(false),
            warm: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            usage: crate::resident::UsageStats::in_memory(),
            warm_cfg,
        })
    }

    // ── Multi-resident warm cache (multislot Phase 2) ───────────────────────

    const GB: u64 = 1024 * 1024 * 1024;

    /// A deterministic `WarmConfig`: `weights` maps a known model spec → GB, `budget_gb` is the
    /// usable budget. Anything not in the map is "not a cached local" (not warmable).
    fn warm_cfg(budget_gb: u64, weights: &[(&'static str, u64)]) -> WarmConfig {
        let map: std::collections::HashMap<String, u64> =
            weights.iter().map(|(m, gb)| ((*m).to_string(), gb * GB)).collect();
        WarmConfig {
            // Matched with `same_model`, as `WarmConfig::new` does in production — otherwise a
            // stub that is stricter than the real thing hides exactly the spelling bugs these
            // tests exist to catch. Ids with neither a slash nor a colon (`model-old`, `big`)
            // are not HF specs, so this stays an exact match for every other test here.
            weight: Arc::new(move |spec: &str| {
                map.iter()
                    .find(|(k, _)| rozum_models::model_source::same_model(k, spec))
                    .map(|(_, w)| *w)
            }),
            budget: Arc::new(move || budget_gb * GB),
            // These stubs model raw weights (no reserve baked in) ⇒ reserve-less world. The
            // shared-reserve accounting itself is unit-tested in `resident::tests`.
            reserve: Arc::new(|| 0),
        }
    }

    /// Like [`warm_cfg`] but with a non-zero shared reserve baked into every `weight` footprint —
    /// the production shape (footprints from `runtime_footprint_bytes`, reserve from
    /// `process_reserve_bytes`). The planner charges `reserve_gb` once across all co-residents.
    fn warm_cfg_reserve(budget_gb: u64, reserve_gb: u64, weights: &[(&'static str, u64)]) -> WarmConfig {
        let mut cfg = warm_cfg(budget_gb, weights);
        cfg.reserve = Arc::new(move || reserve_gb * GB);
        cfg
    }

    #[tokio::test]
    async fn warm_admits_a_co_resident_by_counting_reserve_once() {
        // The production accounting, end-to-end through `ensure_warm`. Budget 18, reserve 5; each
        // footprint bundles one reserve: model-old 10 (= 5 active + 5), warm-b 9 (= 4 active + 5).
        //   Per-model-reserve sum 10 + 9 = 19 > 18 → would refuse warm-b (cf. the `_doesnt_fit` test).
        //   Shared reserve once: active 5 + 4 + ONE 5 = 14 ≤ 18 → warm-b co-resides.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg_reserve(18, 5, &[("model-old", 10), ("warm-b", 9)]));
        let lease = sb.enter(Some("warm-b")).await.expect("admitted, not refused");
        assert_eq!(lease.model_id, "warm-b", "warm-b co-resides because the shared reserve is charged once");
        assert!(sb.warm.lock().await.contains_key("warm-b"));
        assert!(sb.current().is_some(), "the primary is untouched");
    }

    #[tokio::test]
    async fn warm_serves_a_second_model_without_disturbing_primary() {
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[("model-old", 2), ("warm-b", 2)]));
        let lease = sb.enter(Some("warm-b")).await.expect("the warmable second model is served");
        assert_eq!(lease.model_id, "warm-b");
        assert_eq!(sb.generating.load(Ordering::SeqCst), 0, "a warm lease never touches primary generating");
        assert_eq!(sb.warm.lock().await.get("warm-b").unwrap().handle.inflight.load(Ordering::SeqCst), 1);
        drop(lease);
        assert_eq!(sb.warm.lock().await.get("warm-b").unwrap().handle.inflight.load(Ordering::SeqCst), 0, "released on drop");
        assert!(sb.current().is_some(), "the primary is untouched");
    }

    #[tokio::test]
    async fn warm_falls_back_to_primary_when_it_doesnt_fit() {
        // budget 4; primary 3 + requested 3 = 6 > 4 → oversubscribed → serve the primary.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(4, &[("model-old", 3), ("big", 3)]));
        let lease = sb.enter(Some("big")).await.expect("served");
        assert_eq!(lease.model_id, "model-old", "a too-big model falls back to the primary");
        assert_eq!(sb.generating.load(Ordering::SeqCst), 1, "the primary lease holds a token");
        assert!(sb.warm.lock().await.is_empty());
    }

    /// Point a test switchboard's primary at a real-shaped HF spec.
    fn with_primary(sb: &Arc<Switchboard>, spec: &str) {
        sb.spec.lock().unwrap().model_id = spec.to_string();
    }

    #[tokio::test]
    async fn an_equivalent_spelling_of_the_primary_is_the_primary() {
        // One model, several valid specs. `mlx-community/Qwen3.5-4B-MLX-4bit` is what anyone
        // copying the id off the Hub sends; `mlx-community:Qwen3.5-4B-MLX-4bit` is what rozum
        // launched with. Comparing them as strings made the gateway warm a SECOND resident
        // copy of the weights it already had — observed live as a `warm_built` naming the
        // primary's own model.
        let colon = "mlx-community:Qwen3.5-4B-MLX-4bit";
        let slash = "mlx-community/Qwen3.5-4B-MLX-4bit";
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[(
            "mlx-community:Qwen3.5-4B-MLX-4bit",
            2,
        )]));
        with_primary(&sb, colon);

        let lease = sb.enter(Some(slash)).await.expect("served");
        assert_eq!(lease.model_id, colon, "the other spelling must resolve to the resident model");
        assert!(
            sb.warm.lock().await.is_empty(),
            "no second copy of the primary's own weights"
        );
        assert_eq!(sb.generating.load(Ordering::SeqCst), 1, "it took the primary path");
    }

    #[tokio::test]
    async fn a_warm_secondary_is_found_under_either_spelling() {
        // Same defect one level down: the warm map is keyed by whatever string the first
        // requester used, so an exact-key lookup misses its own entry and builds a duplicate.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[
            ("model-old", 2),
            ("mlx-community:Qwen3-4B-4bit", 2),
        ]));
        let first = sb.enter(Some("mlx-community:Qwen3-4B-4bit")).await.expect("warmed");
        assert_eq!(first.model_id, "mlx-community:Qwen3-4B-4bit");
        drop(first);

        let second = sb.enter(Some("mlx-community/Qwen3-4B-4bit")).await.expect("served");
        assert_eq!(second.model_id, "mlx-community/Qwen3-4B-4bit", "the lease echoes what was asked");
        assert_eq!(
            sb.warm.lock().await.len(),
            1,
            "the same weights must not be warmed twice under two spellings"
        );
    }

    #[tokio::test]
    async fn warm_skips_unknown_or_remote_models() {
        // "claude-x" isn't a known cached local → not warmable → primary path (req.model informational).
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[("model-old", 2)]));
        let lease = sb.enter(Some("claude-x")).await.expect("served");
        assert_eq!(lease.model_id, "model-old");
        assert!(sb.warm.lock().await.is_empty());
    }

    #[tokio::test]
    async fn warm_sweep_evicts_long_idle_models() {
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[("model-old", 2), ("warm-b", 2)]));
        drop(sb.enter(Some("warm-b")).await.expect("warmed")); // now idle
        // Backdate last activity so it reads as long-idle.
        sb.warm.lock().await.get("warm-b").unwrap().handle.last_used.store(0, Ordering::SeqCst);
        sb.sweep_idle_warm(60).await;
        assert!(sb.warm.lock().await.is_empty(), "a long-idle warm model is swept (RAM freed)");
    }

    #[tokio::test]
    async fn warm_sweep_keeps_a_busy_model() {
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[("model-old", 2), ("warm-b", 2)]));
        let _lease = sb.enter(Some("warm-b")).await.expect("warmed"); // held → inflight 1
        sb.warm.lock().await.get("warm-b").unwrap().handle.last_used.store(0, Ordering::SeqCst);
        sb.sweep_idle_warm(60).await;
        assert!(sb.warm.lock().await.contains_key("warm-b"), "a busy warm model is never swept");
    }

    #[tokio::test]
    async fn warm_evicts_an_idle_model_to_make_room() {
        // budget 4: primary(1) + one warm(2) fits; a second warm needs the first evicted.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(4, &[("model-old", 1), ("A", 2), ("B", 2)]));
        drop(sb.enter(Some("A")).await.expect("A warmed")); // A resident, then idle
        assert!(sb.warm.lock().await.contains_key("A"));
        drop(sb.enter(Some("B")).await.expect("B warmed")); // B needs room → evict idle A
        let warm = sb.warm.lock().await;
        assert!(warm.contains_key("B"), "B is now warm");
        assert!(!warm.contains_key("A"), "idle A was evicted to fit B");
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
        let lease = sb.enter(None).await.expect("lazy reload should succeed");
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

    #[test]
    fn readiness_reflects_servability() {
        // Loaded → ready. Unloaded but rebuildable → still ready (lazy reload).
        assert!(test_sb(Some(ok_builder()), true).is_ready());
        assert!(test_sb(Some(ok_builder()), false).is_ready());
        // Unloaded AND no builder (dedicated, freed) → can't serve → not ready.
        assert!(!test_sb(None, false).is_ready());
        // A loaded dedicated gateway is ready.
        assert!(test_sb(None, true).is_ready());
    }

    #[test]
    fn shutdown_flips_readiness() {
        let sb = test_sb(Some(ok_builder()), true);
        assert!(sb.is_ready(), "ready before shutdown");
        sb.mark_shutting_down();
        assert!(sb.is_shutting_down());
        assert!(!sb.is_ready(), "a draining instance must read NOT ready");
    }

    #[tokio::test]
    async fn enter_rejects_new_chats_while_shutting_down() {
        let sb = test_sb(Some(ok_builder()), true);
        sb.mark_shutting_down();
        let err = sb.enter(None).await.err().expect("enter must reject during shutdown");
        assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            sb.generating.load(Ordering::SeqCst),
            0,
            "a rejected chat must not leak a generating token"
        );
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

    // ── Cache-when-fits switch (multislot Phase 2: promote-from-warm + keep-old-warm) ───────

    /// A builder that counts how many times it actually BUILDS a model — proves a promote reuses the
    /// warm resident (no rebuild) while a co-reside build invokes it exactly once.
    fn counting_builder(calls: Arc<AtomicU64>) -> BackendBuilder {
        Arc::new(move |_m, _n, _b| {
            let c = Arc::clone(&calls);
            Box::pin(async move {
                c.fetch_add(1, Ordering::SeqCst);
                Some(Arc::new(HelloBackend::new()) as Arc<dyn ChatBackend>)
            })
        })
    }

    #[tokio::test]
    async fn switch_keeps_old_primary_warm_when_both_fit() {
        // Budget 16; model-old(2) + warm-b(2) co-reside → switching to warm-b keeps model-old warm
        // (the cache) instead of dropping it, so a switch back is free.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[("model-old", 2), ("warm-b", 2)]));
        let g0 = sb.generation();
        let g1 = sb.switch("warm-b".into(), None, None).await.unwrap();
        assert_eq!(g1, g0 + 1, "generation bumps on a cached switch");
        assert_eq!(sb.model_id(), "warm-b", "target is now primary");
        assert!(sb.current().is_some(), "target is resident");
        assert!(
            sb.warm.lock().await.contains_key("model-old"),
            "the old primary is kept warm (cache-when-fits), not dropped"
        );
    }

    #[tokio::test]
    async fn switch_promotes_a_warm_target_without_rebuilding() {
        // model-old(2) primary; warm-b(2) warmed first (1 build). Switching to warm-b PROMOTES the
        // resident copy — no extra build — and demotes model-old into the warm set.
        let calls = Arc::new(AtomicU64::new(0));
        let sb = test_sb_cfg(
            Some(counting_builder(Arc::clone(&calls))),
            true,
            warm_cfg(16, &[("model-old", 2), ("warm-b", 2)]),
        );
        drop(sb.enter(Some("warm-b")).await.expect("warm-b warmed")); // build #1, then idle
        assert_eq!(calls.load(Ordering::SeqCst), 1, "warming built warm-b once");
        let warm_arc = sb.warm.lock().await.get("warm-b").unwrap().backend.clone();

        let g0 = sb.generation();
        let g1 = sb.switch("warm-b".into(), None, None).await.unwrap();
        assert_eq!(g1, g0 + 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "promote did NOT rebuild the target");
        assert_eq!(sb.model_id(), "warm-b", "target promoted to primary");
        assert!(
            Arc::ptr_eq(&warm_arc, &sb.current().unwrap()),
            "the promoted primary IS the exact warm backend (no rebuild)"
        );
        assert!(
            sb.warm.lock().await.contains_key("model-old"),
            "the old primary is demoted into the warm set"
        );
    }

    #[tokio::test]
    async fn switch_drops_old_when_target_cannot_coreside() {
        // Budget 4; model-old(3) + big(3) = 6 > 4 → can't co-reside → the old model is dropped (the
        // destructive swap), NOT kept warm.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(4, &[("model-old", 3), ("big", 3)]));
        sb.switch("big".into(), None, None).await.unwrap();
        assert_eq!(sb.model_id(), "big");
        assert!(sb.current().is_some(), "the big model is resident as sole primary");
        assert!(
            !sb.warm.lock().await.contains_key("model-old"),
            "old is dropped (couldn't co-reside), not kept warm"
        );
    }

    #[tokio::test]
    async fn switch_clears_warm_on_a_noncacheable_swap() {
        // A different n_ctx is not cacheable (warm residents share one n_ctx) → the warm set is
        // cleared (RAM freed) and the switch is destructive.
        let sb = test_sb_cfg(Some(ok_builder()), true, warm_cfg(16, &[("model-old", 2), ("warm-b", 2)]));
        drop(sb.enter(Some("warm-b")).await.expect("warm-b warmed"));
        assert!(sb.warm.lock().await.contains_key("warm-b"));
        sb.switch("other".into(), Some(200), None).await.unwrap();
        assert_eq!(sb.model_id(), "other");
        assert_eq!(sb.n_ctx(), 200);
        assert!(
            sb.warm.lock().await.is_empty(),
            "the warm set is cleared before a non-cacheable destructive swap"
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
        let task = tokio::spawn(async move { sb2.enter(None).await.map(|_| ()) });
        // While draining, enter must not complete.
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(!task.is_finished(), "enter parked during drain");
        sb.end_drain();
        let res = tokio::time::timeout(Duration::from_secs(2), task).await;
        assert!(res.is_ok(), "enter resumed after end_drain");
    }
}
