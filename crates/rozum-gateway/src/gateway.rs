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
//! | OpenAI Chat | `POST /v1/chat/completions` | `OaiChatReq` | `oai_messages_to_internal` / `oai_tools_to_internal` | `oai_sse_stream` / `oai_chunk` | `oai_chat_handler` |
//! | OpenAI Responses (Codex) | `POST /v1/responses` | `Value` | `responses_input_to_internal` / `responses_tools_to_internal` (+ `codex_lean_keep` tool policy) | `responses_sse_stream` / `responses_collect` | `responses_handler` |
//! | Anthropic Messages | `POST /v1/messages` | `AnthropicMsg` | `anthropic_messages_to_internal` / `anthropic_tools_to_internal` | `anthropic_sse_stream` | `anthropic_handler` |
//!
//! Cross-cutting, owned by neither dialect: `chat_or_loopbreak` / `detect_stuck_loop`
//! (loop-breaker), `synthetic_stop_stream`, `parse_response_format` (structured output),
//! `parse_*_tool_choice` (tool-choice normalization). `GET /v1/models`, `/health`, `/ready`,
//! `/stats`, `/control/*` are non-chat endpoints.
//!
//! **Why a map and not a `WireProtocol` trait:** the layer is already at a clean per-route
//! boundary — named per-dialect parse + serialize fns + thin handlers. A unifying trait would
//! fight genuinely different typed extractors (`OaiChatReq` vs `Value` vs `AnthropicMsg`) and
//! different SSE event sequences, forcing either looser request validation (a behaviour change)
//! or a fat trait that adds indirection without removing complexity — net-negative on this
//! matrix-critical path. The legibility win is this accurate map; see the spec's Decisions.
//!
//! Bind address is always `127.0.0.1`. Auth is optional bearer token via
//! `ROZUM_GATEWAY_TOKEN`. Cancel propagates from client disconnect.
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
    /// Set once on SIGTERM/SIGINT: `/ready` flips to 503 (the load balancer stops
    /// routing) and new chats are rejected rather than parked, so in-flight work
    /// drains and the process exits cleanly during a rolling deploy.
    shutting_down: AtomicBool,
    /// Secondary resident models kept warm (multislot Phase 2). Empty when off / single-model.
    warm: tokio::sync::Mutex<std::collections::HashMap<String, WarmEntry>>,
    /// Learned per-model usefulness (frequency × recency) ranking warm eviction.
    usage: crate::resident::UsageStats,
    /// Injectable memory inputs for the warm admission decision.
    warm_cfg: WarmConfig,
}

/// Held by a chat handler for the whole request (prefill + stream). Keeps the
/// chosen backend alive and counts against `generating` so a `switch` waits for
/// real work to finish before swapping the model.
struct ChatLease {
    backend: Arc<dyn ChatBackend>,
    model_id: String,
    sb: Arc<Switchboard>,
    /// `Some` for a **warm** (secondary-resident) lease — drop decrements that model's own counter
    /// (not the primary `generating`) and refreshes its last-activity time.
    warm: Option<WarmHandle>,
}

impl Drop for ChatLease {
    fn drop(&mut self) {
        match &self.warm {
            Some(h) => {
                h.inflight.fetch_sub(1, Ordering::SeqCst);
                h.last_used.store(crate::share::now_unix(), Ordering::SeqCst);
            }
            None => {
                self.sb.generating.fetch_sub(1, Ordering::SeqCst);
            }
        }
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

// ── Multi-resident warm cache (shared-gateway-multislot Phase 2) ─────────────────

/// On by default; `ROZUM_MULTISLOT=0|false|off` disables it (→ exactly the single-resident path).
fn multislot_enabled() -> bool {
    !matches!(std::env::var("ROZUM_MULTISLOT").ok().as_deref(), Some("0" | "false" | "off"))
}

/// A secondary resident model kept warm alongside the primary (avoids thrashing).
struct WarmEntry {
    backend: Arc<dyn ChatBackend>,
    weight_bytes: u64,
    /// Per-model in-flight + last-activity, shared with each lease (see [`WarmHandle`]).
    handle: WarmHandle,
}

/// The shared per-warm-model counters a lease holds: its own in-flight count (NOT the primary
/// `generating`, so warm traffic never holds up a primary swap/unload drain) and last-activity unix
/// time (drives idle eviction).
#[derive(Clone)]
struct WarmHandle {
    inflight: Arc<AtomicU64>,
    last_used: Arc<AtomicU64>,
}

impl WarmHandle {
    fn new(now: u64) -> Self {
        Self { inflight: Arc::new(AtomicU64::new(0)), last_used: Arc::new(AtomicU64::new(now)) }
    }
}

/// Injectable memory inputs for the warm-cache admission decision (real on the daemon; deterministic
/// stubs in tests). `weight(spec)` = a model's resident bytes (`None` ⇒ not a known cached local ⇒
/// not warmable); `budget()` = usable model-memory bytes.
struct WarmConfig {
    weight: Arc<dyn Fn(&str) -> Option<u64> + Send + Sync>,
    budget: Arc<dyn Fn() -> u64 + Send + Sync>,
}

impl WarmConfig {
    /// Production warm-admission config (residency-unify U1). Two corrections vs the old
    /// weights-only/`total*0.8` default:
    /// - **weight = calibrated footprint** (`runtime_footprint_bytes` = weights + KV at
    ///   `n_ctx` + cache reserve, smmr-B), so warm admission sizes a secondary the SAME way
    ///   the residency gate does — not by raw on-disk weights (which under-count the resident
    ///   footprint ~5× and could over-admit → overcommit; multislot is on by default).
    /// - **budget = the unified host budget − other gateways' reservations**
    ///   (`host_ram_budget_bytes − committed_by_others`), so this process's warm set plus any
    ///   external gateway processes sum within ONE host budget (not an isolated 0.8 each).
    fn new(n_ctx: u32) -> Self {
        Self {
            weight: Arc::new(move |spec: &str| {
                crate::models::scan_all_installed()
                    .into_iter()
                    .find(|m| m.spec == spec)
                    .map(|m| rozum_models::model_source::runtime_footprint_bytes(spec, n_ctx, m.size_bytes))
            }),
            budget: Arc::new(|| {
                crate::share::host_ram_budget_bytes()
                    .unwrap_or(0)
                    .saturating_sub(crate::share::committed_by_others_bytes(std::process::id()))
            }),
        }
    }
}

/// Test-only legacy default (weights-only sizing, `total*0.8` budget). The production path
/// uses [`WarmConfig::new`]; warm-admission unit tests inject deterministic stubs.
impl Default for WarmConfig {
    fn default() -> Self {
        Self {
            weight: Arc::new(|spec: &str| {
                crate::models::scan_all_installed().into_iter().find(|m| m.spec == spec).map(|m| m.size_bytes)
            }),
            budget: Arc::new(|| {
                crate::concurrency::total_ram_bytes().map(|t| (t as f64 * 0.8) as u64).unwrap_or(0)
            }),
        }
    }
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

    /// Chat-handler entry. Routes by the request's `model`: a request for a **different**,
    /// warmable model (multislot on, a known cached local spec that fits) is served from the warm
    /// cache without disturbing the primary; everything else (the default / single-model case) takes
    /// the primary path unchanged.
    async fn enter(self: &Arc<Self>, requested: Option<&str>) -> Result<ChatLease, Response> {
        if self.is_shutting_down() {
            return Err(error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway is shutting down",
                "shutting_down",
            ));
        }
        if multislot_enabled() {
            if let Some(model) = requested.map(str::trim).filter(|m| !m.is_empty()) {
                if model != self.model_id() {
                    if let Some((backend, handle)) = self.ensure_warm(model).await {
                        handle.inflight.fetch_add(1, Ordering::SeqCst);
                        handle.last_used.store(crate::share::now_unix(), Ordering::SeqCst);
                        return Ok(ChatLease {
                            backend,
                            model_id: model.to_string(),
                            sb: Arc::clone(self),
                            warm: Some(handle),
                        });
                    }
                }
            }
        }
        self.enter_primary().await
    }

    /// Evict warm secondary residents idle (no in-flight) for ≥ `idle_secs`, freeing their RAM.
    /// Called by the lifecycle watchdog; the primary has its own idle-unload.
    async fn sweep_idle_warm(&self, idle_secs: u64) {
        let now = crate::share::now_unix();
        let mut removed_any = false;
        // Evict under the lock; capture the surviving warm total to republish after release.
        let warm_total: u64 = {
            let mut warm = self.warm.lock().await;
            let victims: Vec<String> = warm
                .iter()
                .filter(|(_, e)| {
                    e.handle.inflight.load(Ordering::SeqCst) == 0
                        && now.saturating_sub(e.handle.last_used.load(Ordering::SeqCst)) >= idle_secs
                })
                .map(|(m, _)| m.clone())
                .collect();
            for m in victims {
                if let Some(removed) = warm.remove(&m) {
                    removed_any = true;
                    let b = removed.backend;
                    tokio::task::spawn_blocking(move || drop(b)); // join the !Send worker off-thread
                    crate::obs::log_event(json!({ "event": "warm_idle_evicted", "model": m }));
                }
            }
            warm.values().map(|e| e.weight_bytes).sum()
        };
        // Republish the reduced total reservation (residency-unify U1 wiring) only when the
        // set changed. `model_id` is read with the warm lock RELEASED → no lock-order risk.
        if removed_any {
            let primary_fp = (self.warm_cfg.weight)(&self.model_id()).unwrap_or(0);
            crate::share::update_my_reservation(&self.model_id(), primary_fp.saturating_add(warm_total));
        }
    }

    /// Get-or-build a **warm** secondary resident for `model`, admitting/evicting via the memory
    /// planner. `None` ⇒ not warmable (unknown model, dedicated gateway, won't fit, or build failed)
    /// → the caller falls back to the primary path. Only known cached local models are warmable.
    async fn ensure_warm(
        self: &Arc<Self>,
        model: &str,
    ) -> Option<(Arc<dyn ChatBackend>, WarmHandle)> {
        let now = crate::share::now_unix();
        // Fast path: already warm.
        {
            let warm = self.warm.lock().await;
            if let Some(e) = warm.get(model) {
                self.usage.record(model, e.weight_bytes, now);
                return Some((e.backend.clone(), e.handle.clone()));
            }
        }
        // Only warm a known cached local model (we know its weight + can build it).
        let weight = (self.warm_cfg.weight)(model)?;
        let builder = self.builder.clone()?; // dedicated gateway → no warming

        // Hold the warm lock for the decision + build (serializes warm builds — rare; fine for v1).
        let mut warm = self.warm.lock().await;
        if let Some(e) = warm.get(model) {
            // A racing request already built it.
            self.usage.record(model, e.weight_bytes, now);
            return Some((e.backend.clone(), e.handle.clone()));
        }
        // Plan: residents = primary (always mandatory) + each warm (busy = inflight>0).
        let primary_id = self.model_id();
        let mut residents = vec![crate::resident::ResidentInfo {
            model: primary_id.clone(),
            weight_bytes: (self.warm_cfg.weight)(&primary_id).unwrap_or(0),
            // The primary is the daemon's launched model — never evicted by the warm logic (its own
            // swap/unload owns its lifecycle), so mark it mandatory (busy) unconditionally.
            busy: true,
        }];
        for (m, e) in warm.iter() {
            residents.push(crate::resident::ResidentInfo {
                model: m.clone(),
                weight_bytes: e.weight_bytes,
                busy: e.handle.inflight.load(Ordering::SeqCst) > 0,
            });
        }
        let req = crate::resident::ResidentRequest {
            requested: model,
            requested_weight: weight,
            residents: &residents,
            budget_bytes: (self.warm_cfg.budget)(),
        };
        let plan = crate::resident::plan_residency(&req, |m| self.usage.utility(m, now));
        if plan.oversubscribed {
            return None; // won't co-reside → fall back to the primary (today's swap/thrash)
        }
        // Evict the plan's idle warm victims (never the primary — it isn't in `warm`).
        for victim in &plan.evict {
            if *victim == primary_id {
                continue;
            }
            let idle =
                warm.get(victim).is_some_and(|e| e.handle.inflight.load(Ordering::SeqCst) == 0);
            if idle {
                if let Some(removed) = warm.remove(victim) {
                    let b = removed.backend;
                    tokio::task::spawn_blocking(move || drop(b)); // join the !Send worker off-thread
                    crate::obs::log_event(json!({ "event": "warm_evicted", "model": victim }));
                }
            }
        }
        // Build the new warm model.
        let n_ctx = self.spec.lock().unwrap().n_ctx;
        let backend = builder(model.to_string(), n_ctx, None).await?;
        let handle = WarmHandle::new(now);
        warm.insert(
            model.to_string(),
            WarmEntry { backend: backend.clone(), weight_bytes: weight, handle: handle.clone() },
        );
        // Republish this process's TOTAL reservation (primary + warm) so other gateways'
        // admission accounts for the warm set, not just the primary (residency-unify U1
        // wiring). `primary_id` is in hand and `update_my_reservation` does ledger-file IO
        // only — no map/spec lock — so this is deadlock-safe under the held `warm` lock.
        let primary_fp = (self.warm_cfg.weight)(&primary_id).unwrap_or(0);
        let total = warm.values().map(|e| e.weight_bytes).fold(primary_fp, u64::saturating_add);
        crate::share::update_my_reservation(&primary_id, total);
        self.usage.record(model, weight, now);
        crate::obs::log_event(json!({ "event": "warm_built", "model": model }));
        Some((backend, handle))
    }

    /// The single-resident entry: park while a swap drains, lazily reload if unloaded, then take a
    /// `generating` token. Returns a guard the handler holds for the whole request so the (primary)
    /// model can't be swapped out from under it.
    async fn enter_primary(self: &Arc<Self>) -> Result<ChatLease, Response> {
        loop {
            // A graceful shutdown rejects new work outright (don't park — there's no
            // resume coming); the load balancer has already been told we're not ready.
            if self.is_shutting_down() {
                return Err(error_json(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "gateway is shutting down",
                    "shutting_down",
                ));
            }
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
                warm: None,
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
        // Fast-swap prewarm (smmr-C): warm the NEW model's weights into the OS page cache
        // NOW, so it overlaps the drain below and the rebuild reads from RAM instead of
        // disk. Best-effort, fire-and-forget — page cache is reclaimable and is NOT GPU
        // residency, so it never counts against the RAM budget (no overcommit while the
        // old model is still resident during the drain). Only warms an already-cached
        // model (a swap between known models); `ROZUM_SWAP_PREWARM=0` disables.
        if std::env::var("ROZUM_SWAP_PREWARM").as_deref() != Ok("0") {
            let m = model.clone();
            tokio::task::spawn_blocking(move || {
                if let Some(dir) = rozum_models::model_source::resolve_model_dir(&m) {
                    let cancel = std::sync::atomic::AtomicBool::new(false);
                    let bytes = rozum_core::prefetch::warm_dir_page_cache(&dir, &cancel);
                    crate::obs::log_event(json!({
                        "event": "gateway_swap_prewarm", "model": m, "bytes": bytes,
                    }));
                }
            });
        }
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

    /// True once the process has begun a graceful shutdown.
    fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Begin graceful shutdown: stop reporting ready and reject new chats.
    fn mark_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Readiness for a load balancer: can this instance serve a new request right
    /// now? `false` while shutting down (drain) or when the model is gone and this
    /// gateway can't rebuild it. A transient swap-drain does NOT flip readiness —
    /// those requests park briefly and still succeed.
    fn is_ready(&self) -> bool {
        !self.is_shutting_down() && (self.is_loaded() || self.can_reload())
    }

    /// Free the resident model but keep the daemon listening; the next chat
    /// lazily reloads it. Frees RAM while idle.
    async fn unload(&self) -> Result<u64, String> {
        if self.builder.is_none() {
            return Err("this gateway cannot reload after unload (dedicated)".into());
        }
        self.begin_drain().await?;
        // Take the backend OUT of the lock, then free it OUTSIDE the guard on a blocking
        // thread. The MLX backend's `Drop` now joins its worker thread (blocking until the
        // model's ~GB of buffers are actually freed), so holding the `backend` RwLock across
        // that drop would stall every concurrent `current()` reader, and doing it inline
        // would block a tokio runtime thread. `current()` already reports unloaded the moment
        // we `take()`; the physical free completes on the blocking thread.
        let old = self.backend.write().unwrap().take();
        let spec = self.spec.lock().unwrap().clone();
        let g = self.bump_and_republish(&spec);
        if old.is_some() {
            tokio::task::spawn_blocking(move || drop(old)).await.ok();
        }
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

/// Rough token estimate of the *whole* prompt the model will see — used for the
/// context-overflow preflight and reported as `est_prompt_tokens`. Unlike a Text-only
/// count it includes the parts that actually dominate an agentic request: prior tool-call
/// args, **tool results** (file dumps / command output, often the largest blocks), and the
/// **tool schemas** (which the chat template renders into the prompt — easily ~5K tokens
/// of Claude Code's ~33 tools). Counting only `Text` blocks under-counts a real coding
/// turn several-fold and can let an over-long prompt slip past the overflow guard.
fn estimate_prompt_tokens(messages: &[Message], tools: &[ToolDef]) -> u32 {
    let mut chars = 0usize;
    for m in messages {
        for b in &m.content {
            chars += match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content.len(),
            };
        }
    }
    for t in tools {
        chars += t.name.len() + t.description.len() + t.input_schema.to_string().len();
    }
    (chars as f32 / 3.5) as u32 + 1
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
        ModelError::Timeout(msg) => error_json(StatusCode::GATEWAY_TIMEOUT, msg, fallback_type),
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

// ─── Generation inactivity timeout ────────────────────────────────────────────

/// Inactivity ceiling between two backend events. `ROZUM_GEN_TIMEOUT_SECS`
/// (default 300; `0` disables). Must exceed the worst legitimate gap — a cold
/// hybrid/MoE first token (Metal kernel JIT + weight page-in) ran ~33s, and a big
/// quantized model under memory pressure can stall longer, so keep headroom.
fn gen_inactivity_timeout() -> Duration {
    std::env::var("ROZUM_GEN_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(300))
}

/// Wrap a backend stream so a stalled generation can't hang the client forever.
/// If no event arrives within `gen_inactivity_timeout()`, cancel the job and end
/// the stream with `ModelError::Timeout` (HTTP 504). This is the backstop the
/// per-token cancel check can't provide: a Metal eval wedged under memory
/// pressure blocks inside one FFI call, so the decode loop's `is_cancelled()`
/// check never runs until it returns. Cancelling here lets the worker abandon the
/// job the moment it unblocks; the client gets an error instead of hanging.
fn with_gen_timeout(mut stream: ChatStream, cancel: CancellationToken, dur: Duration) -> ChatStream {
    if dur.is_zero() {
        return stream;
    }
    Box::pin(async_stream::stream! {
        loop {
            match tokio::time::timeout(dur, stream.next()).await {
                Ok(Some(item)) => yield item,
                Ok(None) => break,
                Err(_) => {
                    cancel.cancel();
                    crate::obs::log_event(json!({
                        "event": "generation_timeout", "inactivity_secs": dur.as_secs(),
                    }));
                    yield Err(ModelError::Timeout(format!(
                        "no output for {}s; generation aborted",
                        dur.as_secs()
                    )));
                    break;
                }
            }
        }
    })
}

/// Number of identical, consecutively-failing tool calls that mark a stuck agent.
const STUCK_LOOP_THRESHOLD: usize = 3;

/// Edit-churn (signature 3): a single file edited this many times *with* a ping-pong
/// (an added line re-introduces a previously-removed one) marks a model going in circles.
const EDIT_CHURN_MIN: usize = 3;
/// Backstop: a single file edited this many times is churning even without a strict ping-pong.
const EDIT_CHURN_BACKSTOP: usize = 6;

/// From one tool-call input, extract `(file, removed_lines, added_lines)` if it carries a
/// patch/edit. Shape-agnostic: stringifies the input and scans for a V4A/unified envelope, so
/// it matches an `apply_patch` function call, a `{patch: …}` arg, or a rewritten `patch -p0`
/// heredoc alike. Lines are normalized (leading `+`/`-` and surrounding whitespace stripped)
/// for content comparison; `+++`/`---`/`@@`/`***` header lines are not change lines.
fn edit_target_and_lines(input: &Value) -> Option<(String, Vec<String>, Vec<String>)> {
    // Pull every string leaf out of the input so we see the patch body whatever key holds it.
    fn collect_strings(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
            Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
            _ => {}
        }
    }
    let mut text = String::new();
    collect_strings(input, &mut text);
    if !text.contains("*** Update File:") && !(text.contains("--- ") && text.contains("+++ ")) {
        return None;
    }
    let mut file: Option<String> = None;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for ln in text.lines() {
        if let Some(p) = ln.strip_prefix("*** Update File:") {
            file.get_or_insert_with(|| p.trim().to_string());
        } else if let Some(p) = ln.strip_prefix("+++ ") {
            let p = p.trim();
            file.get_or_insert_with(|| {
                p.strip_prefix("b/").or_else(|| p.strip_prefix("a/")).unwrap_or(p).to_string()
            });
        } else if let Some(p) = ln.strip_prefix("--- ") {
            let p = p.trim();
            file.get_or_insert_with(|| {
                p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
            });
        } else if ln.starts_with("@@") || ln.starts_with("*** ") {
            continue;
        } else if let Some(rest) = ln.strip_prefix('+') {
            let c = rest.trim();
            if !c.is_empty() {
                added.push(c.to_string());
            }
        } else if let Some(rest) = ln.strip_prefix('-') {
            let c = rest.trim();
            if !c.is_empty() {
                removed.push(c.to_string());
            }
        }
    }
    file.map(|f| (f, removed, added))
}

/// Detect the agentic stuck-loop signature in the incoming conversation. A weak local
/// model that re-issues the same already-applied edit gets stuck retrying and runs to
/// `--max-turns` instead of stopping (root cause: SPRINT.md "agentic-loop-root-cause").
/// The gateway sees the whole conversation each turn, so it can short-circuit the next
/// doomed turn. Two signatures, because the loop surfaces differently per harness:
///
///  1. **Structured** (Codex / Responses, and CC when tool use completes): the last
///     `STUCK_LOOP_THRESHOLD` tool calls are byte-identical (same name + input) and each
///     got an **error** result — the model keeps re-sending an edit whose target text is
///     already gone (`String to replace not found`).
///  2. **Text-repeat** (Claude Code headless): CC *interrupts* the doomed tool use and
///     records the turn as a placeholder (`[Tool use interrupted]` / `(no content)`), so
///     the gateway never sees a structured call — only the same assistant text repeated.
///
/// Both are conservative: a healthy agent never re-sends a byte-identical failed call nor
/// repeats the same assistant text `STUCK_LOOP_THRESHOLD` times, so neither trips it.
fn detect_stuck_loop(messages: &[Message]) -> Option<String> {
    use std::collections::HashMap;
    // ── Signature 1: identical, consecutively-failing structured tool calls ──
    let mut errored: HashMap<&str, bool> = HashMap::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolResult { tool_use_id, is_error, .. } = b {
                errored.insert(tool_use_id.as_str(), *is_error);
            }
        }
    }
    let mut calls: Vec<(&str, &Value, bool)> = Vec::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, input } = b {
                let err = errored.get(id.as_str()).copied().unwrap_or(false);
                calls.push((name.as_str(), input, err));
            }
        }
    }
    if calls.len() >= STUCK_LOOP_THRESHOLD {
        let tail = &calls[calls.len() - STUCK_LOOP_THRESHOLD..];
        let (name0, input0, _) = tail[0];
        if tail.iter().all(|(n, i, e)| *e && *n == name0 && *i == input0) {
            return Some(format!(
                "The `{name0}` tool was called {STUCK_LOOP_THRESHOLD} times in a row with identical \
                 arguments and every call returned an error — the change has most likely already \
                 been applied. Stopping to avoid an infinite retry loop; verify and report."
            ));
        }
    }

    // ── Signature 2: no-progress repetition in the recent assistant turns ──
    // CC's interrupted-tool loop doesn't repeat one text *consecutively* — it ping-pongs
    // between a re-diagnosis ("The bug is in `reverse`…") and the `[Tool use interrupted]`
    // placeholder. So instead of "N identical in a row", fire when, within the recent
    // window, any single assistant text recurs `STUCK_LOOP_THRESHOLD` times — the model is
    // cycling the same outputs without making progress.
    let asst_texts: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.trim()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .collect();
    const WINDOW: usize = 2 * STUCK_LOOP_THRESHOLD;
    let start = asst_texts.len().saturating_sub(WINDOW);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in &asst_texts[start..] {
        if !t.is_empty() {
            *counts.entry(t.as_str()).or_default() += 1;
        }
    }
    if counts.values().any(|&c| c >= STUCK_LOOP_THRESHOLD) {
        return Some(
            "The last several assistant turns cycled the same outputs without progress (the tool \
             cycle is stuck — repeated re-attempts/interruptions). Stopping to avoid an infinite \
             loop; verify the current result and report it in one short line."
                .to_string(),
        );
    }

    // ── Signature 3: edit-churn / ping-pong ──
    // The model re-edits one file with *different, mostly-succeeding* patches, undoing and
    // redoing its own changes (toggling equivalent forms, re-anchoring on stale context). The
    // patches differ and don't error, so signatures 1 & 2 miss them; left running, fuzzy
    // re-applies corrupt the file (dup lines / unbalanced braces) and the run burns to timeout
    // with a broken file. Fire when one file is edited >=3 times AND a ping-pong occurred (an
    // added line re-introduces a previously-removed one), or >=6 times outright.
    let mut edits_per_file: HashMap<String, usize> = HashMap::new();
    let mut removed_seen: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut pingpong_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolUse { input, .. } = b {
                if let Some((file, removed, added)) = edit_target_and_lines(input) {
                    *edits_per_file.entry(file.clone()).or_default() += 1;
                    let seen = removed_seen.entry(file.clone()).or_default();
                    if added.iter().any(|a| seen.contains(a)) {
                        pingpong_files.insert(file.clone());
                    }
                    seen.extend(removed);
                }
            }
        }
    }
    let churn = edits_per_file.iter().find(|(f, c)| {
        let n = **c;
        n >= EDIT_CHURN_BACKSTOP || (n >= EDIT_CHURN_MIN && pingpong_files.contains(f.as_str()))
    });
    if let Some((file, n)) = churn {
        return Some(format!(
            "The file `{file}` has been edited {n} times, re-doing and undoing the same change \
             without net progress — the fix has most likely already been applied. Stopping to \
             avoid corrupting the file in a churn loop; verify it builds and report in one line."
        ));
    }
    None
}

/// A one-shot `ChatStream` that emits `text` then `Done{EndTurn}` without touching the
/// model. Used by the loop-breaker: it slots in where `backend.chat` would, so every
/// existing per-protocol serializer (OpenAI / Responses / Anthropic, streaming or not)
/// renders it as an ordinary final assistant turn with `finish_reason: stop`.
fn synthetic_stop_stream(text: String) -> ChatStream {
    Box::pin(async_stream::stream! {
        yield Ok(ChatEvent::TextDelta { text });
        yield Ok(ChatEvent::Done { input_tokens: 0, output_tokens: 0, stop_reason: StopReason::EndTurn });
    })
}

/// `backend.chat`, but first break a detected agentic stuck-loop with a synthetic stop.
async fn chat_or_loopbreak(
    backend: &Arc<dyn ChatBackend>,
    req: ChatRequest,
) -> ModelResult<ChatStream> {
    if let Some(reason) = detect_stuck_loop(&req.messages) {
        crate::obs::log_event(json!({ "event": "stuck_loop_broken", "detail": reason }));
        return Ok(synthetic_stop_stream(reason));
    }
    backend.chat(req).await
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
    tool_choice: Value,
    #[serde(default)]
    response_format: Value,
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
fn parse_oai_tool_choice(v: &Value) -> ToolChoice {
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

/// Parse the Anthropic `tool_choice` object (`{"type":"auto"|"any"|"none"|"tool","name":…}`).
fn parse_anthropic_tool_choice(v: &Value) -> ToolChoice {
    match v.get("type").and_then(Value::as_str) {
        Some("none") => ToolChoice::None,
        Some("any") => ToolChoice::Required,
        Some("tool") => match v.get("name").and_then(Value::as_str) {
            Some(n) => ToolChoice::Named(n.to_string()),
            None => ToolChoice::Auto,
        },
        _ => ToolChoice::Auto,
    }
}

/// Parse OpenAI `response_format` into the JSON Schema to constrain the response to (or
/// `None` for free text). `{"type":"json_object"}` → any JSON object; `{"type":"json_schema",
/// "json_schema":{"schema":{…}}}` → that schema; `{"type":"text"}` / absent → `None`.
fn parse_response_format(v: &Value) -> Option<Value> {
    match v.get("type").and_then(Value::as_str) {
        Some("json_object") => Some(json!({ "type": "object" })),
        Some("json_schema") => v
            .get("json_schema")
            .and_then(|js| js.get("schema"))
            .cloned()
            .or_else(|| Some(json!({ "type": "object" }))),
        _ => None,
    }
}

/// Apply a [`ToolChoice`] to the resolved tool set: `None` → empty, `Named` → only that tool
/// (empty if the client named a tool it didn't define), `Auto`/`Required` → unchanged.
fn apply_tool_choice(tools: Vec<ToolDef>, choice: &ToolChoice) -> Vec<ToolDef> {
    match choice {
        ToolChoice::Auto | ToolChoice::Required => tools,
        ToolChoice::None => Vec::new(),
        ToolChoice::Named(name) => tools.into_iter().filter(|t| &t.name == name).collect(),
    }
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
    tool_choice: Value,
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

/// Codex's `apply_patch` requires its bespoke envelope (`*** Update File: <path>` + bare `@@`
/// hunk markers). Local models routinely emit a **standard unified diff** inside the
/// `*** Begin Patch` wrapper (`--- /+++ /@@ -a,b +c,d @@`), which codex rejects with
/// "Invalid patch hunk". The change lines (` `/`-`/`+`) are identical in both dialects, so we
/// translate just the headers and the (already-correct) edit lands. See
/// `docs/matrix-failure-analysis.md` Finding 4. Returns the input unchanged unless it is exactly
/// this malformed hybrid (codex envelope + unified-diff headers).
fn rewrite_unified_diff_to_apply_patch(patch: &str) -> String {
    // Fire on EITHER unified malformation: a `--- ` file header, or a `@@ -a,b +c,d @@` hunk header
    // (the model sometimes emits the codex `*** Update File:` header itself but keeps unified `@@`).
    let has_unified = patch.starts_with("--- ") || patch.contains("\n--- ") || patch.contains("@@ -");
    if !patch.contains("*** Begin Patch") || !has_unified {
        return patch.to_string();
    }
    let strip = |p: &str| -> String {
        let p = p.trim();
        p.strip_prefix("a/")
            .or_else(|| p.strip_prefix("b/"))
            .unwrap_or(p)
            .to_string()
    };
    let mut out = String::with_capacity(patch.len() + 16);
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(path) = line.strip_prefix("--- ") {
            // `--- a/x` [`+++ b/x`] → `*** Update File: x` (prefer the +++ path; both name the file)
            let mut file = strip(path);
            if let Some(next) = lines.peek() {
                if let Some(p2) = next.strip_prefix("+++ ") {
                    if p2.trim() != "/dev/null" {
                        file = strip(p2);
                    }
                    lines.next();
                }
            }
            out.push_str("*** Update File: ");
            out.push_str(&file);
            out.push('\n');
        } else if line.starts_with("@@ -") || line.starts_with("@@-") {
            // unified hunk header (`@@ -a,b +c,d @@`) — DROP it. codex's V4A apply_patch locates the
            // change via the surrounding context lines; a literal `@@ -a,b...` is read as a context
            // string to find ("Failed to find context '-a,b...'") which never matches the file.
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Method B (the robust fix). codex's `apply_patch` uses a finicky proprietary V4A format that a
/// local model can't reliably hit (header dialect + strict context-matching — see
/// `docs/matrix-failure-analysis.md` Finding 4 and the mock-codex probe). But the model DOES emit a
/// correct unified diff (its `-` lines match the file verbatim). So instead of translating to V4A,
/// reconstruct a MINIMAL unified diff from the model's patch and rewrite the whole
/// `apply_patch "<patch>"` shell command into `patch --fuzz` of it — standard tooling codex runs
/// verbatim (codex only intercepts `apply_patch`, not `patch`). `patch --fuzz` locates the change by
/// context, tolerant of the line-number/whitespace drift that breaks V4A. Returns the new shell
/// command, or None when it isn't a reconstructable apply_patch (→ caller falls back to the V4A bridge).
fn rewrite_apply_patch_command(cmd: &str) -> Option<String> {
    if !cmd.contains("apply_patch") {
        return None;
    }
    let begin = cmd.find("*** Begin Patch")?;
    let end_rel = cmd[begin..].find("*** End Patch")?;
    let end = begin + end_rel + "*** End Patch".len();
    // The patch lives inside a shell double-quoted string — undo the shell escaping.
    let block = cmd[begin..end]
        .replace("\\\"", "\"")
        .replace("\\$", "$")
        .replace("\\`", "`")
        .replace("\\\\", "\\");
    apply_patch_block_to_fuzz(&block)
}

/// Render a verbatim file-create as a shell command: write `content` to `path` ONLY if the path is
/// still absent (so a re-sent create is an idempotent no-op and never clobbers a real edit), with
/// `mkdir -p` of the parent for nested targets. Single-quoted heredoc → the body lands byte-for-byte
/// (no `$`/backtick/`\` expansion). Shared by the explicit `*** Add/Create File:` path and the
/// `*** Update File:`-against-an-absent-file fallback.
fn synth_create_command(path: &str, content: &str) -> String {
    format!(
        "[ -e '{path}' ] || {{ mkdir -p \"$(dirname '{path}')\" 2>/dev/null; \
         cat > '{path}' <<'ROZUM_CREATE_EOF'\n{content}\nROZUM_CREATE_EOF\n}}\n"
    )
}

/// Extract explicit file-creations from a V4A patch block: each `*** Add File: <path>` /
/// `*** Create File: <path>` directive plus the lines that follow it (the new file's content, bare
/// or `+`-prefixed) up to the next `*** ` directive. This is the canonical — and, for gpt-oss, the
/// dominant — create-from-scratch shape (`*** Create File:` is gpt-oss's variant of the standard
/// `*** Add File:`). codex serves `apply_patch` only as a shell command for local models, so these
/// reach the bare `apply_patch` (absent in the jail) and the file never lands; we turn each into a
/// real write instead. Returns (path, content) pairs; empty when the block has no create directive.
fn parse_create_directives(block: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, Vec<String>)> = Vec::new();
    let mut active = false;
    for ln in block.lines() {
        if let Some(p) = ln
            .strip_prefix("*** Add File:")
            .or_else(|| ln.strip_prefix("*** Create File:"))
        {
            files.push((p.trim().to_string(), Vec::new()));
            active = true;
        } else if ln.starts_with("*** ") {
            active = false; // Begin/End Patch or an Update File hunk ends the create body
        } else if active {
            if let Some((_, body)) = files.last_mut() {
                body.push(ln.strip_prefix('+').unwrap_or(ln).to_string());
            }
        }
    }
    files
        .into_iter()
        .filter(|(p, b)| !p.is_empty() && !b.is_empty())
        .map(|(p, b)| (p, b.join("\n")))
        .collect()
}

/// Detect a "whole file dumped as a fake patch": `*** Update File: <path>` whose body (after the
/// `@@`) is the file's RAW content with NO diff markers at all — gpt-oss does this for a brand-new
/// file (esp. a nested `src/main.rs`), often inside a broken `apply_patch <<'…'` heredoc that runs
/// bare and lands nothing. There is no diff to apply; the body verbatim IS the intended file, so we
/// create it (when absent). Returns None the moment a real `+`/`-` marker appears — a genuine diff
/// belongs to the patch path, untouched. Structural lines (`@@`, `+++ `, `--- `) are skipped.
fn parse_bare_file_block(block: &str) -> Option<(String, String)> {
    let mut path: Option<String> = None;
    let mut content: Vec<&str> = Vec::new();
    let mut started = false;
    for ln in block.lines() {
        if let Some(p) = ln.strip_prefix("*** Update File:") {
            path = Some(p.trim().to_string());
            started = true;
            content.clear();
        } else if ln.starts_with("*** ") {
            if started && !content.is_empty() {
                break; // End Patch / next directive closes this file's content
            }
            started = false;
        } else if started {
            if ln.starts_with("@@") || ln.starts_with("+++ ") || ln.starts_with("--- ") {
                continue; // structural, skip
            }
            if ln.starts_with('+') || ln.starts_with('-') {
                return None; // real diff markers → not bare content; leave it to the patch path
            }
            content.push(ln);
        }
    }
    let path = path?;
    if content.is_empty() {
        return None;
    }
    Some((path, content.join("\n")))
}

/// Convert an unescaped V4A patch block (`*** Begin Patch` … `*** End Patch`, or a bare
/// `*** Update File:` + hunk) into a `patch -p0 --fuzz=3 -N --forward` heredoc — a small
/// ±3-context match surface that standard `patch` applies reliably and *idempotently* (`-N`:
/// a re-submitted, already-applied patch is ignored, never reversed — see the note at the
/// `format!` below). Shared by the apply_patch *shell-command* bridge (Method B) and the
/// apply_patch-*function* re-route (gpt-oss). None when there are no change lines to anchor on.
fn apply_patch_block_to_fuzz(block: &str) -> Option<String> {
    // Explicit `*** Add File:` / `*** Create File:` directives → real file writes (the dominant
    // gpt-oss create-from-scratch shape). One directive can carry several files; write each.
    let creates = parse_create_directives(block);
    if !creates.is_empty() {
        return Some(creates.iter().map(|(p, c)| synth_create_command(p, c)).collect());
    }
    // A whole new file dumped as a fake `*** Update File:` patch (bare body, no diff markers) →
    // create it from the verbatim body. (A real diff bails out of parse_bare_file_block to None.)
    if let Some((p, content)) = parse_bare_file_block(block) {
        return Some(synth_create_command(&p, &content));
    }
    let mut path: Option<String> = None;
    let mut body: Vec<String> = Vec::new();
    for ln in block.lines() {
        if let Some(p) = ln.strip_prefix("*** Update File:") {
            path = Some(p.trim().to_string());
        } else if let Some(p) = ln.strip_prefix("--- ") {
            let p = p.trim();
            let p = p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p);
            path.get_or_insert_with(|| p.to_string());
        } else if ln.starts_with("+++ ") || ln.starts_with("@@") || ln.starts_with("*** ") {
            continue;
        } else if ln.is_empty() {
            body.push(" ".to_string()); // a blank context line in the diff
        } else if matches!(ln.as_bytes()[0], b' ' | b'+' | b'-') {
            body.push(ln.to_string());
        }
        // anything else is stray prose — skip it
    }
    let path = path?;
    let chg: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('+') || l.starts_with('-'))
        .map(|(i, _)| i)
        .collect();
    let (&first, &last) = (chg.first()?, chg.last()?);
    // Trim to ±3 lines of context around the change → small, reliable match surface.
    let lo = first.saturating_sub(3);
    let hi = (last + 1 + 3).min(body.len());
    let hunk = &body[lo..hi];
    let old = hunk.iter().filter(|l| l.starts_with(' ') || l.starts_with('-')).count();
    let new = hunk.iter().filter(|l| l.starts_with(' ') || l.starts_with('+')).count();
    let mut diff = format!("--- {path}\n+++ {path}\n@@ -1,{old} +1,{new} @@\n");
    for l in hunk {
        diff.push_str(l);
        diff.push('\n');
    }
    // `-N --forward`: make re-application idempotent. A weak model (gpt-oss) flails — it
    // re-submits the SAME patch after it has already landed. Without `-N`, GNU/BSD `patch`
    // hits "Reversed (or previously applied) patch detected!  Assume -R? [y]" and, with no
    // tty, assumes yes → it REVERSES the already-applied fix, putting the bug back. The file
    // then oscillates fixed↔buggy across the model's retries and whichever state the timeout
    // freezes decides pass/fail (observed coin-flip pass=0/1). `-N` turns a redundant patch
    // into a no-op ("Ignoring previously applied patch") instead of a revert, so the fix is
    // sticky and the outcome is deterministic. A genuinely new patch still applies normally.
    // Whitespace-tolerant FALLBACK: gpt-oss often drops the leading indentation on changed lines
    // (`-s.to_string()` instead of `-    s.to_string()`), and BSD `patch` — even `--ignore-whitespace`
    // — refuses to match it, so the hunk fails to a `.rej` and the fix never lands (looks like the
    // model "reverting" itself; it never applied). When `patch` leaves a `.rej`, a tiny static python
    // helper reads that `.rej`, matches the removed block against the file by *trimmed* content
    // (ignoring leading whitespace and the line number), and re-applies it preserving the file's own
    // indentation. Fires only after `patch` already failed → zero effect on patches that apply.
    let fuzz = patch_fuzz();
    // gpt-oss creating a file FROM SCRATCH emits the new content as an `*** Update File:` /
    // unified-diff hunk whose "old" side is bogus (a lone `---`, empty context) because the target
    // does not exist yet. `patch` then can't update the absent file → `.rej`, nothing lands (the
    // codex×gpt-oss `build`/`test` create reds, matrix Finding 5). Detect it — additions present
    // but the removed/context side carries no real content — and CREATE the file from the `+` lines
    // instead, only if it's still absent (so a re-sent create is an idempotent no-op and never
    // clobbers a real edit). A genuine edit (real removed/context lines) is byte-identical to
    // before: it falls through to the patch path unchanged, so the `fix` task is unaffected.
    let has_real_old = body
        .iter()
        .filter(|l| l.starts_with(' ') || l.starts_with('-'))
        .any(|l| l[1..].trim().chars().any(|c| c.is_alphanumeric()));
    let added: Vec<&str> = body.iter().filter(|l| l.starts_with('+')).map(|l| &l[1..]).collect();
    if !added.is_empty() && !has_real_old {
        return Some(synth_create_command(&path, &added.join("\n")));
    }
    Some(format!(
        "patch -p0 --fuzz={fuzz} -N --forward <<'ROZUM_PATCH_EOF'\n{diff}ROZUM_PATCH_EOF\n\
         f={path}; if [ -f \"$f.rej\" ]; then python3 - \"$f\" <<'ROZUM_PY_EOF'\n{py}\nROZUM_PY_EOF\n\
         rm -f \"$f.rej\" \"$f.orig\"; fi\n",
        py = PATCH_WS_FALLBACK_PY,
    ))
}

/// Static python helper for the whitespace-tolerant apply fallback (see `apply_patch_block_to_fuzz`).
/// Reads `<file>.rej`, extracts the removed (`-`) and added (`+`) lines, finds the removed block in
/// the file by trimmed comparison, and replaces it with the added lines re-indented to the file's
/// own leading whitespace. Best-effort + single-block: only the file path is dynamic (argv), so the
/// script needs no escaping when embedded in the command heredoc.
const PATCH_WS_FALLBACK_PY: &str = r#"import sys
f=sys.argv[1]
old=[]; new=[]
for ln in open(f+".rej").read().split("\n"):
    if ln.startswith("---") or ln.startswith("+++"): continue
    if ln[:1]=="-": old.append(ln[1:])
    elif ln[:1]=="+": new.append(ln[1:])
no=[s.strip() for s in old]
if no:
    L=open(f).read().split("\n")
    h=next((i for i in range(len(L)-len(no)+1) if [L[i+j].strip() for j in range(len(no))]==no), -1)
    if h>=0:
        ind=L[h][:len(L[h])-len(L[h].lstrip())]
        L[h:h+len(no)]=[ind+n.strip() for n in new]
        open(f,"w").write("\n".join(L))
        sys.stderr.write("[rozum-apply] whitespace-tolerant fallback applied\n")"#;

/// The `--fuzz` context-slack `patch` is allowed when matching a hunk. Higher = more lenient
/// (lands a model's slightly-off-context patch, but can mis-apply a stale-anchored churn patch
/// at the wrong line and corrupt the file); lower = stricter (a misanchored patch fails to a
/// `.rej`, leaving the file intact, but the model's first imperfect patch may not land).
/// `ROZUM_PATCH_FUZZ` overrides the default (3); clamped to GNU/BSD patch's 0..=3.
fn patch_fuzz() -> u8 {
    std::env::var("ROZUM_PATCH_FUZZ")
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map(|n| n.min(3))
        .unwrap_or(3)
}

/// gpt-oss (trained on the OpenAI/codex tool surface) emits a native `apply_patch` *function*
/// call, but codex serves apply_patch only as a shell command for the rozum-backed local-model
/// config — so the function call is rejected (`unsupported call: apply_patch`) and the edit is
/// silently lost. Re-route it: convert the function args into an `exec_command` payload that
/// applies the patch with standard tooling (Method B `patch --fuzz`; failing that, a quote-safe
/// `apply_patch` heredoc so codex's own V4A applier still gets a shot). Returns the exec_command
/// args JSON, or None when there is no reconstructable patch (caller keeps the original args).
fn rewrite_apply_patch_function_args(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    // The model passes the patch text in one of a few shapes:
    //   {"command":["apply_patch","<patch>"]}  (gpt-oss, observed) — the last array string is it
    //   {"input":"<patch>"} / {"patch":"<patch>"} / a bare string
    let patch = v
        .get("command")
        .and_then(|c| c.as_array())
        .and_then(|a| a.iter().rev().find_map(|x| x.as_str()))
        .or_else(|| v.get("input").and_then(|x| x.as_str()))
        .or_else(|| v.get("patch").and_then(|x| x.as_str()))
        .or_else(|| v.as_str())?;
    if !patch.contains("*** Begin Patch") && !patch.contains("*** Update File") {
        return None;
    }
    // Decode the `\uXXXX` escapes gpt-oss double-escapes into the body (`&`→&, `<`/`>`→
    // </>). A Rust fix is full of these (`&str`, `&arg`, `collect::<String>()`, `->`);
    // left literal they land verbatim and break compilation. The shell-command path
    // (normalize_codex_tool_args) already decodes — this FUNCTION-call path (the dominant gpt-oss
    // edit shape) did not, which is a major source of the codex×gpt-oss corruption.
    let patch = decode_unicode_escapes(patch);
    // Prefer Method B: codex runs `patch --fuzz` verbatim (it only intercepts `apply_patch`).
    let cmd = apply_patch_block_to_fuzz(&patch).unwrap_or_else(|| {
        // Fallback: hand codex the raw apply_patch via a quote-safe heredoc (its V4A applier).
        format!("apply_patch <<'ROZUM_AP_EOF'\n{patch}\nROZUM_AP_EOF\n")
    });
    eprintln!("[apply_patch-fn] re-routed apply_patch function call → exec_command (gpt-oss)");
    Some(json!({ "cmd": cmd, "login": true }).to_string())
}

/// gpt-oss, asked to CREATE a file from scratch, routes a write-INTENT through the codex shell
/// tool: `{cmd:"apply_patch", path:"Cargo.toml", content:"<whole file body>"}`. `content` is a full
/// file, NOT a patch (no `*** Begin Patch`), so the apply_patch fold finds nothing and codex runs
/// bare `apply_patch` → "Usage: apply_patch 'PATCH'" → the file never lands (build/test create-from-
/// scratch tasks time out, matrix Finding 5). The intent is unambiguous (a path + its full content),
/// so synthesize the real write codex can't perform from the malformed call. None unless there is a
/// non-empty `path` plus a `content` string that is NOT a patch (patches go through the fold above).
fn synthesize_write_from_obj(o: &serde_json::Map<String, Value>) -> Option<String> {
    let path = o
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    let content = o.get("content").and_then(Value::as_str)?;
    let content = decode_unicode_escapes(content);
    if content.contains("*** Begin Patch") || content.contains("*** Update File") {
        return None; // a patch body, not a file — leave it to the apply_patch path
    }
    Some(synthesize_file_write(path, &content))
}

/// Render a verbatim file write as one shell command: `mkdir -p <dir>` (so a nested target like
/// `src/main.rs` into a fresh dir doesn't fail on a missing directory) then a *single-quoted* heredoc
/// `cat > <path>` so the body lands byte-for-byte — no `$`/backtick/`\` expansion. The path is
/// single-quoted to tolerate spaces; a literal `'` in a path is pathological and not handled.
fn synthesize_file_write(path: &str, content: &str) -> String {
    format!(
        "mkdir -p \"$(dirname '{path}')\" 2>/dev/null; cat > '{path}' <<'ROZUM_WRITE_EOF'\n\
         {content}\n\
         ROZUM_WRITE_EOF\n"
    )
}

/// Decode literal `\uXXXX` (4-hex) escapes that gpt-oss sometimes double-escapes *into* patch
/// content (`&` for `&`, `>` for `>`) — the literal 6-char sequence survives in the
/// string, so the patch's context/`-` lines no longer match the file and the apply fails. Only the
/// bare 4-hex form is touched (Rust's own escape is `\u{..}` with braces, so source code is safe).
fn decode_unicode_escapes(s: &str) -> String {
    if !s.contains("\\u") {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\'
            && i + 5 < chars.len()
            && chars[i + 1] == 'u'
            && chars[i + 2] != '{'
        {
            let hex: String = chars[i + 2..i + 6].iter().collect();
            if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(c);
                i += 6;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Read-repair (translate a malformed `sed/head/tail` file-read → `cat <file>`) is ON by default.
/// Reading the file is the decisive success factor: a weak model (gpt-oss) that emits a broken read
/// (`sed -n "src/main.rs"` with no line range) never sees the code and so never fixes it — it retries
/// the same broken read and gives up. The repair is conservative and only fires on a *genuinely
/// broken* read (a `sed` whose script slot holds the filename or a range with no print command);
/// well-formed ranged reads and `head`/`tail` are left intact (see `repair_broken_read`).
/// `ROZUM_CODEX_READ_REPAIR=0` turns it off.
fn read_repair_enabled() -> bool {
    std::env::var("ROZUM_CODEX_READ_REPAIR")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// A token that looks like a source-file path the model wants to view (has a slash, or a known
/// code/text extension). Used to recognize a file-read intent in a malformed command.
fn is_source_path(w: &str) -> bool {
    (w.contains('/') && w.contains('.'))
        || w.rsplit('.').next().is_some_and(|e| {
            matches!(
                e,
                "rs" | "py" | "js" | "ts" | "go" | "toml" | "txt" | "md" | "json" | "c" | "cpp"
                    | "h" | "java" | "rb" | "yaml" | "yml" | "sh" | "lock"
            )
        })
}

/// Translate a *genuinely broken* file-READ command into a plain `cat <file>`. gpt-oss emits broken
/// `sed` reads (filename in the script slot, a range with no `p` command, scrambled args) that exit
/// non-zero, so it never sees the file. The intent — view a source file — is unambiguous from the
/// path, and reading is non-destructive. Only fires when the read would actually fail, so it is safe
/// to default ON: a WELL-FORMED ranged read (`sed -n '1,200p' f`) and any `head`/`tail` (which work
/// with a file) are left intact — never collapsed to a full `cat`. Edits (`s/…/`, `-i`) and
/// redirects (`>`) are left alone. None when it isn't a recognizable broken read.
fn repair_broken_read(cmd: &str) -> Option<String> {
    let t = cmd.trim();
    let tool = t.split_whitespace().next()?;
    if !matches!(tool, "sed" | "head" | "tail") {
        return None;
    }
    if t.contains("s/") || t.contains(" -i") || t.contains('>') {
        return None; // an intentional edit / transform / redirect — not a read
    }
    // Unquoted positional (non-flag) args.
    let args: Vec<String> = t
        .split_whitespace()
        .skip(1)
        .filter(|w| !w.starts_with('-'))
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"').to_string())
        .collect();
    let path = args.iter().find(|w| is_source_path(w))?.clone();
    // `head`/`tail` with a file are valid reads — leave them (respect a deliberate partial read).
    if tool != "sed" {
        return None;
    }
    // A well-formed `sed -n` read has a print-script arg (`1,200p`, `5p`, `$p`). If one is present
    // the read works as written → don't touch it. Broken only when the script slot holds the file
    // or a range with no print command.
    let has_print_script = args.iter().any(|a| {
        a != &path && a.ends_with('p') && a.chars().all(|c| c.is_ascii_digit() || ",$p".contains(c))
    });
    if has_print_script {
        return None;
    }
    Some(format!("cat {path}"))
}

/// Walk a tool-call `arguments` JSON string and rewrite any embedded malformed codex `apply_patch`
/// (Finding 4). The patch text is nested inside the shell tool's command (and JSON-escaped), so we
/// parse, recurse over every string value, and re-serialize — keeping escaping correct. A no-op for
/// non-codex agents (only the Responses path calls this) and for well-formed / non-patch args.
fn normalize_codex_tool_args(args: &str) -> String {
    let mut v: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return args.to_string(),
    };
    // gpt-oss often emits the patch in a field SIBLING to a bare `apply_patch` command
    // ({"cmd":"apply_patch","patch":"*** Begin Patch …"}); these keys carry it.
    const PATCH_KEYS: &[&str] = &["patch", "input", "stdin", "patch_text", "content", "text"];
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => {
                if s.contains("*** Begin Patch") {
                    // Decode any literal \uXXXX the model double-escaped into the patch body.
                    let s2 = decode_unicode_escapes(s);
                    if let Some(rw) = rewrite_apply_patch_command(&s2) {
                        eprintln!("[apply_patch-bridge] rewrote apply_patch → patch --fuzz (Method B)");
                        *s = rw;
                    } else {
                        let fixed = rewrite_unified_diff_to_apply_patch(&s2);
                        if fixed != *s {
                            eprintln!("[apply_patch-bridge] rewrote unified-diff headers → codex V4A (fallback)");
                            *s = fixed;
                        }
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            Value::Object(o) => {
                // The dominant gpt-oss edit-delivery shape: a bare `apply_patch` command with the
                // patch stranded in a sibling field. codex runs bare `apply_patch` (ignoring the
                // sibling) → "Usage: apply_patch 'PATCH'" and the edit is lost. Fold the sibling
                // patch INTO the command (Method B `patch --fuzz`, unicode-decoded) so it lands.
                let cmd_is_apply = o
                    .get("cmd")
                    .and_then(Value::as_str)
                    .map(|c| c.trim() == "apply_patch")
                    .unwrap_or(false);
                if cmd_is_apply {
                    let patch = PATCH_KEYS.iter().find_map(|k| {
                        o.get(*k)
                            .and_then(Value::as_str)
                            .filter(|p| {
                                p.contains("*** Begin Patch") || p.contains("*** Update File")
                            })
                            .map(|p| decode_unicode_escapes(p))
                    });
                    if let Some(fuzz) = patch.as_deref().and_then(apply_patch_block_to_fuzz) {
                        eprintln!(
                            "[apply_patch-bridge] folded {{cmd:apply_patch, patch sibling}} → patch --fuzz"
                        );
                        o.insert("cmd".into(), Value::String(fuzz));
                        for k in PATCH_KEYS {
                            o.remove(*k);
                        }
                    } else if let Some(write) = synthesize_write_from_obj(o) {
                        // Create-from-scratch (Finding 5): `content` is a whole file, not a patch, so
                        // the fold found nothing. Synthesize the real write codex can't perform from
                        // the malformed `{cmd:apply_patch, path, content}` so the file actually lands.
                        eprintln!(
                            "[apply_patch-bridge] synthesized file write from {{path, content}} (create-from-scratch, Finding 5)"
                        );
                        o.insert("cmd".into(), Value::String(write));
                        o.remove("path");
                        o.remove("content");
                    }
                }
                // Read-repair: gpt-oss frequently emits broken file reads (`sed -n 'src/main.rs'`,
                // `sed -n '1' '1' f`) that fail, so it never sees the file and can't build a matching
                // patch — reading is the decisive success factor. Its intent is unambiguous (a source
                // path in a read tool) and reading is non-destructive, so translate it to `cat <file>`.
                if read_repair_enabled() {
                    if let Some(fixed) = o
                        .get("cmd")
                        .and_then(Value::as_str)
                        .and_then(repair_broken_read)
                    {
                        eprintln!("[read-repair] broken file-read → {fixed}");
                        o.insert("cmd".into(), Value::String(fixed));
                    }
                }
                o.values_mut().for_each(walk);
            }
            _ => {}
        }
    }
    walk(&mut v);
    v.to_string()
}

/// codex-lean: codex hands a LOCAL model ~18 tools (most are meta-tool noise — plans, goals,
/// plugins, MCP listing, `request_user_input`, …) on top of a ~21 KB system prompt. A small model
/// drowns in it: it stalls after diagnosing, or grabs a meta-tool instead of editing
/// (`docs/matrix-failure-analysis.md` Findings 1a/3). Dropping the non-coding tools is the codex
/// analog of claude `--lean` (which lifts the same model to 5/5). Gated by `ROZUM_CODEX_LEAN`
/// (off → codex's full tool set, unchanged). The keep-set is the actual coding surface.
fn codex_lean_keep(name: &str) -> bool {
    // Shell + file I/O + patching: everything a coding agent needs. Anything containing these
    // stems survives (covers exec_command, write_stdin, apply_patch, shell, read/write/edit, …).
    const KEEP_STEMS: &[&str] = &[
        "exec", "shell", "command", "stdin", "apply_patch", "patch", "read_file", "write_file",
        "edit", "view_image",
    ];
    let n = name.to_ascii_lowercase();
    KEEP_STEMS.iter().any(|s| n.contains(s))
}

/// A short, focused replacement for codex's ~21 KB system prompt, for load-sensitive local
/// reasoning models. The load bisection (`docs/specs/constrained-gptoss-delivery.md`) proved
/// CONTEXT SIZE is the DOMINANT breaker of tool-call delivery on gpt-oss — more than the V4A
/// format or tool count: with the easy `write_file` tool, a 30 KB prompt drops it to 0/3 (it
/// emits empty content, no tool call), while a ~20-byte prompt is 3/3. `codex_lean_keep` trims
/// only TOOLS; this trims the INSTRUCTIONS too. The tool *schemas* (kept by lean) carry the
/// argument shapes, so a short prompt suffices.
const LEAN_CODING_PROMPT: &str = "You are a coding agent working in a sandboxed shell, already \
in the project's working directory. Use the provided tools to complete the user's task directly. \
Run shell commands with the exec_command tool — including creating files (e.g. \
`cat > path <<'EOF'` … `EOF`), building, and running. Edit an existing file with apply_patch. \
Do the task, verify it works, then reply with one short confirmation line and stop. Do not ask \
for confirmation or permission.";

/// Models whose tool-calling collapses under a large context (so they get [`LEAN_CODING_PROMPT`]
/// instead of codex's full instructions). gpt-oss reasons 4-8× more than Qwen3.6-35B and emits no
/// tool call at all under codex's 21 KB+ prompt; the capable tier (35B) is fine with the full
/// instructions (4/5) and is deliberately excluded so it is never regressed.
fn model_is_load_sensitive(model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    m.contains("gpt-oss") || m.contains("gpt_oss")
}

/// The instructions to actually send for a codex `/v1/responses` request: codex's own when the
/// trim doesn't apply, or [`LEAN_CODING_PROMPT`] when it does. Gated by `ROZUM_CODEX_LEAN` (shares
/// the tool-lean switch) AND `model_is_load_sensitive`; override with `ROZUM_CODEX_LEAN_PROMPT`
/// (`0`/`off` = never trim, anything else = always trim). Behaviour-preserving for non-gpt-oss
/// models (returns codex's instructions verbatim).
fn codex_effective_instructions(model_id: &str, original: Option<&str>) -> Option<String> {
    let force = std::env::var("ROZUM_CODEX_LEAN_PROMPT").ok();
    let lean_tools = std::env::var("ROZUM_CODEX_LEAN").map(|v| v != "0").unwrap_or(true);
    if lean_prompt_on(model_id, force.as_deref(), lean_tools) {
        Some(LEAN_CODING_PROMPT.to_string())
    } else {
        original.map(str::to_string)
    }
}

/// Pure decision for [`codex_effective_instructions`] (env split out so it is race-free to test).
/// `force` is `ROZUM_CODEX_LEAN_PROMPT` (`0`/`off` = never, any other value = always); when unset,
/// the trim follows the tool-lean switch AND model load-sensitivity.
fn lean_prompt_on(model_id: &str, force: Option<&str>, lean_tools: bool) -> bool {
    match force {
        Some("0" | "false" | "off") => false,
        Some(_) => true,
        None => lean_tools && model_is_load_sensitive(model_id),
    }
}

fn responses_tools_to_internal(tools: &[RespTool]) -> Vec<ToolDef> {
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

fn codex_tool_capture_enabled() -> bool {
    std::env::var("ROZUM_CODEX_TOOL_CAPTURE")
        .map(|v| v != "0")
        .unwrap_or(false)
}

fn codex_tool_capture_max_bytes() -> usize {
    std::env::var("ROZUM_CODEX_TOOL_CAPTURE_MAX_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(65_536)
}

fn capture_text_json(text: &str, max_bytes: usize) -> Value {
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
fn codex_tool_call_capture_json(
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

fn log_codex_tool_inventory(
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

fn log_codex_tool_call_capture(event: Value) {
    if codex_tool_capture_enabled() {
        crate::obs::log_event(event);
    }
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
    #[serde(default)]
    tool_choice: Value,
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

/// Completes on SIGTERM/SIGINT, after flipping the gateway to "not ready" and waiting a
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

    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    crate::obs::log_event(json!({ "event": "gateway_shutdown_signal" }));
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

/// Reproducibility instrument for the agentic matrix (and any caller wanting a
/// deterministic local model). The gateway passes the client's sampling params through
/// verbatim and leaves `seed` unset, so the sampler + MLX RNG seed from entropy: a
/// `temperature > 0` request (Claude Code's main loop sends 1.0) produces a DIFFERENT
/// token stream every run → a matrix cell flips pass↔fail on a byte-identical config,
/// which undermines every other matrix reading. These env knobs pin a run WITHOUT
/// changing the wire protocol. Both default OFF → behaviour is byte-for-byte unchanged
/// unless explicitly set (so it is purely a benchmark/diagnosis instrument here):
///   `ROZUM_SAMPLING_SEED=<u64>`   pin the RNG seed (only fills it when the client sent none)
///   `ROZUM_FORCE_GREEDY=1|true|on` force temperature 0 (argmax — removes the RNG entirely)
fn apply_determinism_env(s: SamplingParams) -> SamplingParams {
    let force_greedy = matches!(
        std::env::var("ROZUM_FORCE_GREEDY").ok().as_deref(),
        Some("1" | "true" | "on")
    );
    let seed = std::env::var("ROZUM_SAMPLING_SEED")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok());
    apply_determinism(s, force_greedy, seed)
}

/// Pure core of [`apply_determinism_env`] (env read split out so it is race-free to test).
/// `force_greedy` wins over the client's temperature; `seed` only fills an unset seed so a
/// caller that genuinely sent its own seed keeps it.
fn apply_determinism(mut s: SamplingParams, force_greedy: bool, seed: Option<u64>) -> SamplingParams {
    if force_greedy {
        s.temperature = Some(0.0);
        s.top_p = None;
        s.top_k = None;
    }
    if s.seed.is_none() {
        if let Some(sd) = seed {
            s.seed = Some(sd);
        }
    }
    s
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
    let lease = match state.sb.enter(req.model.as_deref()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let messages = oai_messages_to_internal(&req.messages);
    let tools = apply_tool_choice(
        oai_tools_to_internal(&req.tools),
        &parse_oai_tool_choice(&req.tool_choice),
    );

    // Approximate context overflow check
    let est = estimate_prompt_tokens(&messages, &tools);
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
        sampling: apply_determinism_env(SamplingParams {
            temperature: req.temperature,
            top_p: req.top_p,
            max_tokens: req.max_tokens,
            top_k: req.top_k,
            response_schema: parse_response_format(&req.response_format),
            ..Default::default()
        }),
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = req.model.unwrap_or_else(|| lease.model_id.clone());
    // OpenAI/Anthropic spec default for an absent `stream` is non-streaming JSON.
    // (Streaming clients — CC, Codex — always send `stream:true` explicitly.)
    let stream_mode = req.stream.unwrap_or(false);

    match chat_or_loopbreak(&lease.backend, chat_req).await {
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
            let chat_stream = with_gen_timeout(chat_stream, cancel.clone(), gen_inactivity_timeout());
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
    let lease = match state.sb.enter(req.model.as_deref()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    // Trim codex's ~21 KB instructions to a short focused prompt for load-sensitive models
    // (gpt-oss) — the bisection-proven dominant breaker of tool delivery. Verbatim for 35B et al.
    let effective_instructions = codex_effective_instructions(&lease.model_id, req.instructions.as_deref());
    if effective_instructions.as_deref() != req.instructions.as_deref() {
        tracing::debug!(
            model = %lease.model_id,
            from_bytes = req.instructions.as_deref().map(str::len).unwrap_or(0),
            to_bytes = effective_instructions.as_deref().map(str::len).unwrap_or(0),
            "codex-lean: replaced instructions with the focused coding prompt"
        );
    }
    let messages = responses_input_to_internal(effective_instructions.as_deref(), &req.input);
    // Did codex offer `apply_patch` as a function tool for this request? If not, a model that calls
    // it as a function (gpt-oss) would hit "unsupported call: apply_patch" — so we re-route those to
    // exec_command. When codex DID offer it as a tool, the call is legit and we leave it alone.
    let apply_patch_is_tool = req
        .tools
        .iter()
        .any(|t| t.name.as_deref() == Some("apply_patch"));
    let mut tools = apply_tool_choice(
        responses_tools_to_internal(&req.tools),
        &parse_oai_tool_choice(&req.tool_choice),
    );
    // EXPERIMENT (ROZUM_CODEX_INJECT_APPLY_PATCH): gpt-oss is trained to call `apply_patch` as a
    // function, but codex offers it only as a shell command for our config — so the model GUESSES
    // the schema (keys begin_patch / cmd / update …) and we drop the guesses. Give it the tool it
    // expects, with a CLEAR schema, so it stops guessing; its clean {patch:…} call is re-routed to
    // exec_command by the Responses handler (apply_patch_is_tool stays false → reroute fires).
    let inject_ap = std::env::var("ROZUM_CODEX_INJECT_APPLY_PATCH")
        .map(|v| v != "0")
        .unwrap_or(false);
    if inject_ap && !apply_patch_is_tool {
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
        req.model.as_deref(),
        req.stream.unwrap_or(false),
        &req.tools,
        &tools,
        apply_patch_is_tool,
        inject_ap,
    );

    let est = estimate_prompt_tokens(&messages, &tools);
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
        sampling: apply_determinism_env(SamplingParams {
            temperature: req.temperature,
            top_p: req.top_p,
            max_tokens: req.max_output_tokens,
            top_k: req.top_k,
            ..Default::default()
        }),
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = req.model.unwrap_or_else(|| lease.model_id.clone());
    let stream_mode = req.stream.unwrap_or(false);

    match chat_or_loopbreak(&lease.backend, chat_req).await {
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
            let chat_stream = with_gen_timeout(chat_stream, cancel.clone(), gen_inactivity_timeout());
            if stream_mode {
                Sse::new(responses_sse_stream(
                    chat_stream,
                    cancel,
                    model,
                    Some(lease),
                    apply_patch_is_tool,
                ))
                .into_response()
            } else {
                responses_collect(chat_stream, cancel, &model, Some(lease), apply_patch_is_tool)
                    .await
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
async fn responses_collect(
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
    let lease = match state.sb.enter(req.model.as_deref()).await {
        Ok(l) => l,
        Err(resp) => return resp,
    };
    let messages = anthropic_messages_to_internal(req.system.as_ref(), &req.messages);
    let tools = apply_tool_choice(
        anthropic_tools_to_internal(&req.tools),
        &parse_anthropic_tool_choice(&req.tool_choice),
    );

    // Approximate context overflow check
    let est = estimate_prompt_tokens(&messages, &tools);
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
        sampling: apply_determinism_env(SamplingParams {
            temperature: req.temperature,
            max_tokens: req.max_tokens,
            ..Default::default()
        }),
        cancel: cancel.clone(),
        session_id: None,
    };

    let model = req.model.unwrap_or_else(|| lease.model_id.clone());
    // OpenAI/Anthropic spec default for an absent `stream` is non-streaming JSON.
    // (Streaming clients — CC, Codex — always send `stream:true` explicitly.)
    let stream_mode = req.stream.unwrap_or(false);

    match chat_or_loopbreak(&lease.backend, chat_req).await {
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
            let chat_stream = with_gen_timeout(chat_stream, cancel.clone(), gen_inactivity_timeout());
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
        .route("/control/switch", post(control_switch))
        .route("/control/unload", post(control_unload))
        .route("/control/reload", post(control_reload))
        .layer(middleware::from_fn(poison_layer))
        .layer(middleware::from_fn_with_state(state.clone(), auth_layer))
        .with_state(state.clone());

    tracing::info!(addr = ?listener.local_addr().ok(), "rozum gateway listening");
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(state.sb.clone()))
        .await;
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

    #[test]
    fn estimate_prompt_tokens_counts_text_results_and_tools() {
        // Large user text → large estimate (baseline behaviour).
        let long_text: String = "word ".repeat(200_000);
        let messages = oai_messages_to_internal(&[OaiMsg {
            role: "user".into(),
            content: Value::String(long_text),
            tool_calls: vec![],
            tool_call_id: None,
        }]);
        let base = estimate_prompt_tokens(&messages, &[]);
        assert!(base > 100_000, "expected large token estimate, got {base}");

        // A big tool RESULT (e.g. a file dump) must be counted — the old Text-only
        // count ignored it entirely, under-counting an agentic turn several-fold.
        let dump = vec![Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "x".repeat(40_000),
                is_error: false,
            }],
        }];
        assert!(estimate_prompt_tokens(&dump, &[]) > 8_000, "tool results must be counted");

        // Tool schemas render into the prompt → they must add to the estimate.
        let tools = vec![ToolDef {
            name: "Bash".into(),
            description: "run a shell command ".repeat(100),
            input_schema: json!({"type":"object","properties":{"command":{"type":"string"}}}),
        }];
        assert!(
            estimate_prompt_tokens(&dump, &tools) > estimate_prompt_tokens(&dump, &[]),
            "tool schemas must increase the estimate"
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
            weight: Arc::new(move |spec: &str| map.get(spec).copied()),
            budget: Arc::new(move || budget_gb * GB),
        }
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
