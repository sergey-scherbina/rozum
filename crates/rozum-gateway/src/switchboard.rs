//! The Switchboard: the gateway's resident-model manager.
//!
//! Extracted from `gateway.rs` (`gw-monolith-decompose`). It holds the PRIMARY resident backend
//! behind a swap cell plus a `warm` map of secondary residents, drains in-flight work across a
//! switch, and admits/evicts warm models against a host-aware budget.
//!
//! **What the extraction made visible.** The server builds three of these structs by literal and
//! calls 18 of the component's 31 methods, so the interface is wide. That was true before and
//! invisible, because everything shared one file; the `pub(crate)` markers here ARE that surface
//! and nothing more — the other 13 methods stay private. Narrowing it (a constructor instead of a
//! 22-field literal) is a follow-up, deliberately not folded in: an extraction that also redesigns
//! what it moves cannot be reviewed as either one.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::response::Response;

use crate::backend::ChatBackend;
// The one dependency this component has on the rest of the crate, and it points at the error
// module rather than back at `gateway` — which is what keeps this a leaf.
use crate::errors::error_json;
use serde_json::json;

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
pub(crate) struct ModelSpec {
    pub(crate) model_id: String,
    pub(crate) n_ctx: u32,
    pub(crate) backend: Option<String>,
}

/// Holds the resident backend behind a swap cell. `rozum gateway switch` /
/// `unload` drain in-flight work, drop the old model (never two resident —
/// memory), build the new one, bump `generation`, and resume. `reload` (binary
/// upgrade) re-execs instead; this covers in-place model/backend changes.
pub(crate) struct Switchboard {
    /// `None` = unloaded (model freed; the next chat lazily rebuilds from `spec`).
    pub(crate) backend: std::sync::RwLock<Option<Arc<dyn ChatBackend>>>,
    /// Backend factory; `None` on a `--dedicated` gateway (switching disabled).
    pub(crate) builder: Option<BackendBuilder>,
    pub(crate) spec: std::sync::Mutex<ModelSpec>,
    pub(crate) generation: AtomicU64,
    pub(crate) started_at: u64,
    /// Set while a switch/unload finishes in-flight work; chat requests park on
    /// `resume` until it clears so none is served mid-swap.
    pub(crate) draining: AtomicBool,
    pub(crate) resume: tokio::sync::Notify,
    /// Active generations — NOT the idle-watchdog `in_flight`. A drain waits for
    /// this to reach 0; parked/queued requests don't count, so it can't deadlock
    /// on requests that are themselves waiting for the drain to finish.
    pub(crate) generating: AtomicU64,
    /// Serializes lazy reload so racing requests rebuild the model only once.
    pub(crate) reload_lock: tokio::sync::Mutex<()>,
    /// `(pid, port)` when registered, so a switch can republish `active.json`.
    pub(crate) register: Option<(u32, u16)>,
    /// Set once on SIGTERM/SIGINT: `/ready` flips to 503 (the load balancer stops
    /// routing) and new chats are rejected rather than parked, so in-flight work
    /// drains and the process exits cleanly during a rolling deploy.
    pub(crate) shutting_down: AtomicBool,
    /// Secondary resident models kept warm (multislot Phase 2). Empty when off / single-model.
    pub(crate) warm: tokio::sync::Mutex<std::collections::HashMap<String, WarmEntry>>,
    /// Learned per-model usefulness (frequency × recency) ranking warm eviction.
    pub(crate) usage: crate::resident::UsageStats,
    /// Injectable memory inputs for the warm admission decision.
    pub(crate) warm_cfg: WarmConfig,
}

/// Held by a chat handler for the whole request (prefill + stream). Keeps the
/// chosen backend alive and counts against `generating` so a `switch` waits for
/// real work to finish before swapping the model.
pub(crate) struct ChatLease {
    pub(crate) backend: Arc<dyn ChatBackend>,
    pub(crate) model_id: String,
    pub(crate) sb: Arc<Switchboard>,
    /// `Some` for a **warm** (secondary-resident) lease — drop decrements that model's own counter
    /// (not the primary `generating`) and refreshes its last-activity time.
    pub(crate) warm: Option<WarmHandle>,
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
pub(crate) fn unload_idle_secs() -> u64 {
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

/// Find a warm resident for `model` under **any** valid spelling of the same weights.
///
/// The map is keyed by whatever string the first requester used, and one model has several
/// valid specs (`org:repo`, `org/repo`, `hf:org/repo`). An exact-key lookup therefore misses
/// its own entry when the second caller spells it differently, and builds a duplicate. The
/// scan is over a handful of entries and only runs when the exact key misses.
fn warm_lookup<'a>(
    warm: &'a std::collections::HashMap<String, WarmEntry>,
    model: &str,
) -> Option<&'a WarmEntry> {
    warm.get(model).or_else(|| {
        warm.iter()
            .find(|(k, _)| rozum_models::model_source::same_model(k, model))
            .map(|(_, e)| e)
    })
}

/// A secondary resident model kept warm alongside the primary (avoids thrashing).
pub(crate) struct WarmEntry {
    pub(crate) backend: Arc<dyn ChatBackend>,
    pub(crate) weight_bytes: u64,
    /// Per-model in-flight + last-activity, shared with each lease (see [`WarmHandle`]).
    pub(crate) handle: WarmHandle,
}

/// The shared per-warm-model counters a lease holds: its own in-flight count (NOT the primary
/// `generating`, so warm traffic never holds up a primary swap/unload drain) and last-activity unix
/// time (drives idle eviction).
#[derive(Clone)]
pub(crate) struct WarmHandle {
    pub(crate) inflight: Arc<AtomicU64>,
    pub(crate) last_used: Arc<AtomicU64>,
}

impl WarmHandle {
    pub(crate) fn new(now: u64) -> Self {
        Self { inflight: Arc::new(AtomicU64::new(0)), last_used: Arc::new(AtomicU64::new(now)) }
    }
}

/// Injectable memory inputs for the warm-cache admission decision (real on the daemon; deterministic
/// stubs in tests). `weight(spec)` = a model's resident bytes (`None` ⇒ not a known cached local ⇒
/// not warmable); `budget()` = usable model-memory bytes; `reserve()` = the process-shared activation
/// reserve baked into each `weight`, which the planner charges ONCE across all co-residents (must be
/// consistent with the `weight` model: production weights are full footprints ⇒ a real reserve;
/// reserve-less test weights ⇒ `0`).
pub(crate) struct WarmConfig {
    pub(crate) weight: Arc<dyn Fn(&str) -> Option<u64> + Send + Sync>,
    pub(crate) budget: Arc<dyn Fn() -> u64 + Send + Sync>,
    pub(crate) reserve: Arc<dyn Fn() -> u64 + Send + Sync>,
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
    pub(crate) fn new(n_ctx: u32) -> Self {
        Self {
            weight: Arc::new(move |spec: &str| {
                crate::models::scan_all_installed()
                    .into_iter()
                    .find(|m| rozum_models::model_source::same_model(&m.spec, spec))
                    .map(|m| rozum_models::model_source::runtime_footprint_bytes(spec, n_ctx, m.size_bytes))
            }),
            budget: Arc::new(|| {
                crate::share::host_ram_budget_bytes()
                    .unwrap_or(0)
                    .saturating_sub(crate::share::committed_by_others_bytes(std::process::id()))
            }),
            // The shared MLX cache + prefill pool, baked into every `weight` footprint above. The
            // planner backs out all-but-one of these across co-residents; `(0)` = the smallest,
            // always-reboot-safe reserve (weight-independent with the default cache cap).
            reserve: Arc::new(|| rozum_models::model_source::process_reserve_bytes(0)),
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
            // Legacy weights-only sizing carries no reserve ⇒ nothing to back out.
            reserve: Arc::new(|| 0),
        }
    }
}

/// Combine a process's per-model footprints (primary + each warm resident) into the single
/// reservation it publishes to the host ledger, counting the **process-shared** activation reserve
/// (the MLX buffer cache + prefill spike — one process-global pool) ONCE instead of once per model.
///
/// `primary_fp` and every `warm_fps[i]` are full `runtime_footprint_bytes`, each of which already
/// bundles one reserve. For N co-resident models only ONE reserve is physically real (`set_cache_limit`
/// is process-global; prefill serializes under `max_num_seqs`), so the naive `Σ fp_i` over-reserves by
/// (N-1) reserves and needlessly refuses co-residents that actually fit. We subtract those (N-1)
/// redundant reserves using `process_reserve_bytes(0)` — the SMALLEST possible reserve — so the result
/// is provably never below the real co-resident peak (`Σ active_i + max reserve`); admission stays
/// reboot-safe, it just stops over-refusing. Single-model (no warm) ⇒ bare `primary_fp`, unchanged.
// Used by the gateway's own tests, which assert what a warm load publishes to the ledger.
pub(crate) fn published_reservation(primary_fp: u64, warm_fps: &[u64]) -> u64 {
    let naive = warm_fps.iter().fold(primary_fp, |a, &b| a.saturating_add(b));
    let redundant =
        rozum_models::model_source::process_reserve_bytes(0).saturating_mul(warm_fps.len() as u64);
    naive.saturating_sub(redundant)
}

impl Switchboard {
    pub(crate) fn current(&self) -> Option<Arc<dyn ChatBackend>> {
        self.backend.read().unwrap().clone()
    }

    pub(crate) fn model_id(&self) -> String {
        self.spec.lock().unwrap().model_id.clone()
    }

    pub(crate) fn n_ctx(&self) -> u32 {
        self.spec.lock().unwrap().n_ctx
    }

    pub(crate) fn generation(&self) -> u64 {
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
    pub(crate) async fn enter(self: &Arc<Self>, requested: Option<&str>) -> Result<ChatLease, Response> {
        if self.is_shutting_down() {
            return Err(error_json(
                StatusCode::SERVICE_UNAVAILABLE,
                "gateway is shutting down",
                "shutting_down",
            ));
        }
        if multislot_enabled() {
            if let Some(model) = requested.map(str::trim).filter(|m| !m.is_empty()) {
                // Compared by IDENTITY, not by spelling. One model has several valid specs —
                // `mlx-community:Qwen3.5-4B-MLX-4bit` and `mlx-community/Qwen3.5-4B-MLX-4bit`
                // are the same weights, and the second is what anyone copying the id off the
                // Hub will send. A `!=` here read them as two models and warmed a SECOND
                // resident copy of the one already loaded: double the RAM for one model, and
                // on a machine sized for one of them, an admission refusal or a swap. Observed
                // as a `warm_built` for the primary's own weights.
                if !rozum_models::model_source::same_model(model, &self.model_id()) {
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
    pub(crate) async fn sweep_idle_warm(&self, idle_secs: u64) {
        let now = crate::share::now_unix();
        let mut removed_any = false;
        // Evict under the lock; capture the surviving warm footprints to republish after release.
        let warm_fps: Vec<u64> = {
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
            warm.values().map(|e| e.weight_bytes).collect()
        };
        // Republish the reduced total reservation (residency-unify U1 wiring) only when the
        // set changed. `model_id` is read with the warm lock RELEASED → no lock-order risk.
        // The process-shared activation reserve is counted ONCE (see `published_reservation`).
        if removed_any {
            let primary_fp = (self.warm_cfg.weight)(&self.model_id()).unwrap_or(0);
            crate::share::update_my_reservation(
                &self.model_id(),
                published_reservation(primary_fp, &warm_fps),
            );
        }
    }

    /// Get-or-build a **warm** secondary resident for `model`, admitting/evicting via the memory
    /// planner. `None` ⇒ not warmable (unknown model, dedicated gateway, won't fit, or build failed)
    /// → the caller falls back to the primary path. Only known cached local models are warmable.
    pub(crate) async fn ensure_warm(
        self: &Arc<Self>,
        model: &str,
    ) -> Option<(Arc<dyn ChatBackend>, WarmHandle)> {
        let now = crate::share::now_unix();
        // Fast path: already warm.
        {
            let warm = self.warm.lock().await;
            if let Some(e) = warm_lookup(&warm, model) {
                self.usage.record(model, e.weight_bytes, now);
                return Some((e.backend.clone(), e.handle.clone()));
            }
        }
        // Only warm a known cached local model (we know its weight + can build it).
        let weight = (self.warm_cfg.weight)(model)?;
        let builder = self.builder.clone()?; // dedicated gateway → no warming

        // Hold the warm lock for the decision + build (serializes warm builds — rare; fine for v1).
        let mut warm = self.warm.lock().await;
        if let Some(e) = warm_lookup(&warm, model) {
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
            // `weight`/`weight_bytes` are full `runtime_footprint_bytes` (each bundles one activation
            // reserve), but co-residents share a single process-global MLX cache + serialized prefill,
            // so the planner backs out all-but-one reserve. The same full footprints still flow to
            // `published_reservation` below, keeping the cross-process ledger reboot-safe.
            process_reserve_bytes: (self.warm_cfg.reserve)(),
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
        let warm_fps: Vec<u64> = warm.values().map(|e| e.weight_bytes).collect();
        crate::share::update_my_reservation(&primary_id, published_reservation(primary_fp, &warm_fps));
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
    pub(crate) async fn begin_drain(&self) -> Result<(), String> {
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

    pub(crate) fn end_drain(&self) {
        self.draining.store(false, Ordering::SeqCst);
        self.resume.notify_waiters();
    }

    /// In-place model swap. With multislot ON (default) this is **cache-when-fits**: if the target
    /// is already a warm resident it is promoted with no rebuild, and the old primary is kept warm
    /// when the planner says both fit — so a later switch back (or the next run / the matrix) reuses
    /// the resident copy instead of paying a full reload. When they don't fit (or multislot is off /
    /// a custom backend / a different n_ctx) it falls back to the destructive single-resident swap
    /// (drain → drop old → build new). Co-residency is RAM-gated by the same planner the warm cache
    /// uses (reboot-safe) and was proven crash-free (`tests/mlx_evals.rs` co-residency probe).
    pub(crate) async fn switch(
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
        let out = if multislot_enabled() {
            self.switch_cached(&builder, model, n_ctx, backend).await
        } else {
            let old = self.spec.lock().unwrap().clone();
            let new = ModelSpec { model_id: model, n_ctx: n_ctx.unwrap_or(old.n_ctx), backend };
            self.swap_destructive(&builder, old, new).await
        };
        self.end_drain();
        out
    }

    /// The destructive single-resident swap: drop the old model first (frees RAM — they never
    /// coexist), build the new one, bump generation. On build failure the spec reverts so the next
    /// request lazily reloads the old model. Caller must hold the drain (`begin_drain`).
    async fn swap_destructive(
        &self,
        builder: &BackendBuilder,
        old: ModelSpec,
        new: ModelSpec,
    ) -> Result<u64, String> {
        *self.backend.write().unwrap() = None;
        *self.spec.lock().unwrap() = new.clone();
        crate::obs::log_event(json!({
            "event": "gateway_switch_start", "from": old.model_id, "to": new.model_id, "n_ctx": new.n_ctx,
        }));
        match builder(new.model_id.clone(), new.n_ctx, new.backend.clone()).await {
            Some(b) => {
                *self.backend.write().unwrap() = Some(b);
                let g = self.bump_and_republish(&new);
                crate::obs::log_event(json!({
                    "event": "gateway_switch_done", "model": new.model_id, "generation": g,
                }));
                Ok(g)
            }
            None => {
                *self.spec.lock().unwrap() = old;
                crate::obs::log_event(json!({
                    "event": "gateway_switch_failed", "model": new.model_id,
                }));
                Err(format!("failed to load model '{}'", new.model_id))
            }
        }
    }

    /// Drop every IDLE warm resident (frees their RAM), restoring the single-resident invariant
    /// before a destructive swap. A busy warm model (in-flight > 0) is left in place.
    async fn evict_all_idle_warm(&self) {
        let mut warm = self.warm.lock().await;
        let victims: Vec<String> = warm
            .iter()
            .filter(|(_, e)| e.handle.inflight.load(Ordering::SeqCst) == 0)
            .map(|(m, _)| m.clone())
            .collect();
        for m in victims {
            if let Some(removed) = warm.remove(&m) {
                let b = removed.backend;
                tokio::task::spawn_blocking(move || drop(b)); // join the !Send worker off-thread
                crate::obs::log_event(json!({ "event": "warm_evicted", "model": m }));
            }
        }
    }

    /// Republish this process's TOTAL reservation (primary + the current warm set) to the host
    /// ledger, counting the process-shared activation reserve once. Caller holds the `warm` guard.
    fn republish_warm_reservation(
        &self,
        primary_id: &str,
        warm: &std::collections::HashMap<String, WarmEntry>,
    ) {
        let primary_fp = (self.warm_cfg.weight)(primary_id).unwrap_or(0);
        let warm_fps: Vec<u64> = warm.values().map(|e| e.weight_bytes).collect();
        crate::share::update_my_reservation(primary_id, published_reservation(primary_fp, &warm_fps));
    }

    /// Cache-when-fits swap (multislot on). Promotes a warm target with no rebuild; otherwise plans
    /// whether the old primary can stay resident (warm) while the target builds. Falls back to the
    /// warm-clearing destructive swap when the pair can't co-reside or the swap isn't cacheable.
    async fn switch_cached(
        &self,
        builder: &BackendBuilder,
        model: String,
        n_ctx: Option<u32>,
        backend: Option<String>,
    ) -> Result<u64, String> {
        let old = self.spec.lock().unwrap().clone();
        let new_n_ctx = n_ctx.unwrap_or(old.n_ctx);
        let new = ModelSpec { model_id: model.clone(), n_ctx: new_n_ctx, backend: backend.clone() };
        let now = crate::share::now_unix();

        let old_weight = (self.warm_cfg.weight)(&old.model_id);
        let new_weight = (self.warm_cfg.weight)(&model);
        // Cacheable only for a plain local model swap whose footprints we know and whose n_ctx
        // matches the warm set (warm residents are all built at one n_ctx). Otherwise restore the
        // single-resident invariant (drop every idle warm) and do the destructive swap.
        let cacheable = backend.is_none()
            && new_n_ctx == old.n_ctx
            && model != old.model_id
            && old_weight.is_some()
            && new_weight.is_some();
        if !cacheable {
            self.evict_all_idle_warm().await;
            let out = self.swap_destructive(builder, old, new.clone()).await;
            if out.is_ok() {
                let warm = self.warm.lock().await;
                self.republish_warm_reservation(&new.model_id, &warm);
            }
            return out;
        }
        let old_weight = old_weight.unwrap();
        let new_weight = new_weight.unwrap();
        let primary_id = old.model_id.clone();

        let mut warm = self.warm.lock().await;

        // CASE A — the target is already a warm resident: promote it to primary with NO rebuild and
        // demote the old primary into the warm set. They were already co-resident, so this is a
        // memory-neutral relabeling (the win that makes a switch-back / re-run instant).
        if let Some(entry) = warm.remove(&model) {
            let new_primary = entry.backend.clone();
            let old_backend = { self.backend.write().unwrap().replace(new_primary) };
            if let Some(ob) = old_backend {
                warm.insert(
                    primary_id.clone(),
                    WarmEntry { backend: ob, weight_bytes: old_weight, handle: WarmHandle::new(now) },
                );
            }
            *self.spec.lock().unwrap() = new.clone();
            let g = self.bump_and_republish(&new);
            self.republish_warm_reservation(&new.model_id, &warm);
            self.usage.record(&model, new_weight, now);
            crate::obs::log_event(json!({
                "event": "gateway_switch_promote", "from": primary_id, "to": new.model_id, "generation": g,
            }));
            return Ok(g);
        }

        // CASE B — the target must be built. Plan whether the old primary (and existing warm) can
        // stay resident alongside it. The old primary is idle here (we drained), so it's evictable.
        let mut residents = vec![crate::resident::ResidentInfo {
            model: primary_id.clone(),
            weight_bytes: old_weight,
            busy: false,
        }];
        for (m, e) in warm.iter() {
            residents.push(crate::resident::ResidentInfo {
                model: m.clone(),
                weight_bytes: e.weight_bytes,
                busy: e.handle.inflight.load(Ordering::SeqCst) > 0,
            });
        }
        let req = crate::resident::ResidentRequest {
            requested: &model,
            requested_weight: new_weight,
            residents: &residents,
            process_reserve_bytes: (self.warm_cfg.reserve)(),
            budget_bytes: (self.warm_cfg.budget)(),
        };
        let plan = crate::resident::plan_residency(&req, |m| self.usage.utility(m, now));
        let drop_old = plan.oversubscribed || plan.evict.iter().any(|m| *m == primary_id);

        // Evict the planned idle warm victims (not the primary — handled via `drop_old`) BEFORE the
        // build so it never coexists with models the budget can't hold (overcommit-safe). On
        // oversubscription, free the whole warm set (thrash).
        let evict: Vec<String> = if plan.oversubscribed {
            warm.keys().cloned().collect()
        } else {
            plan.evict.iter().filter(|m| **m != primary_id).cloned().collect()
        };
        for victim in evict {
            if let Some(removed) = warm.remove(&victim) {
                if removed.handle.inflight.load(Ordering::SeqCst) == 0 {
                    let b = removed.backend;
                    tokio::task::spawn_blocking(move || drop(b));
                    crate::obs::log_event(json!({ "event": "warm_evicted", "model": victim }));
                } else {
                    warm.insert(victim, removed); // a busy warm model can't be dropped — keep it
                }
            }
        }

        // Free the old primary up-front when it can't co-reside with the target.
        if drop_old {
            let ob = { self.backend.write().unwrap().take() };
            if let Some(b) = ob {
                tokio::task::spawn_blocking(move || drop(b));
            }
        }

        match builder(model.clone(), new_n_ctx, None).await {
            Some(b) => {
                if !drop_old {
                    // Keep the old primary resident as a warm secondary (the cache).
                    let ob = { self.backend.read().unwrap().clone() };
                    if let Some(ob) = ob {
                        warm.insert(
                            primary_id.clone(),
                            WarmEntry {
                                backend: ob,
                                weight_bytes: old_weight,
                                handle: WarmHandle::new(now),
                            },
                        );
                    }
                }
                *self.backend.write().unwrap() = Some(b);
                *self.spec.lock().unwrap() = new.clone();
                let g = self.bump_and_republish(&new);
                self.republish_warm_reservation(&new.model_id, &warm);
                self.usage.record(&model, new_weight, now);
                crate::obs::log_event(json!({
                    "event": "gateway_switch_cached", "from": primary_id, "to": new.model_id,
                    "kept_old_warm": !drop_old, "generation": g,
                }));
                Ok(g)
            }
            None => {
                if drop_old {
                    // The old model was freed → revert the spec so the next request lazily reloads it.
                    *self.spec.lock().unwrap() = old;
                }
                // else: the old primary is still resident and the spec is unchanged → keep serving it.
                crate::obs::log_event(json!({
                    "event": "gateway_switch_failed", "model": new.model_id,
                }));
                Err(format!("failed to load model '{model}'"))
            }
        }
    }

    /// True while a model is resident (vs freed by `unload`/idle-unload).
    pub(crate) fn is_loaded(&self) -> bool {
        self.backend.read().unwrap().is_some()
    }

    /// True when the model can be rebuilt in process (has a builder). A
    /// `--dedicated` gateway returns false — it must never auto-unload.
    pub(crate) fn can_reload(&self) -> bool {
        self.builder.is_some()
    }

    /// True once the process has begun a graceful shutdown.
    pub(crate) fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }

    /// Begin graceful shutdown: stop reporting ready and reject new chats.
    pub(crate) fn mark_shutting_down(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// Readiness for a load balancer: can this instance serve a new request right
    /// now? `false` while shutting down (drain) or when the model is gone and this
    /// gateway can't rebuild it. A transient swap-drain does NOT flip readiness —
    /// those requests park briefly and still succeed.
    pub(crate) fn is_ready(&self) -> bool {
        !self.is_shutting_down() && (self.is_loaded() || self.can_reload())
    }

    /// Free the resident model but keep the daemon listening; the next chat
    /// lazily reloads it. Frees RAM while idle.
    pub(crate) async fn unload(&self) -> Result<u64, String> {
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
