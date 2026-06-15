//! Backend-agnostic concurrency control: admission scheduling, a memory budget,
//! and a circuit breaker that any `ChatBackend` can opt into.
//!
//! Specs: `concurrency-backend-abstraction.md` (this generic layer) and
//! `mistralrs-concurrency-scheduling.md` (the original mistralrs design + the
//! per-phase rationale).
//!
//! The pieces:
//! - [`AdmissionScheduler`] — a runtime-adjustable concurrency gate with
//!   shortest-job-first ordering, a reserved fast lane, bounded-queue
//!   backpressure, and a trip/recover circuit breaker.
//! - [`budgeted_max_num_seqs`] — derive a safe concurrent-prefill count from a
//!   model's memory footprint vs available RAM.
//! - [`AdmittingBackend`] — a decorator that bolts the above onto any backend;
//!   [`admit_wrap`] applies it iff the backend advertises a capacity.
//!
//! Responsiveness scope: admission controls *ordering* — a short request runs as
//! soon as a slot frees, ahead of queued big requests, and never waits for a big
//! request's full generation. It does not preempt an in-flight engine step
//! (see `concurrency-engine-yield` in BACKLOG). Nothing here depends on any
//! backend feature, so it builds and unit-tests with no features (no Xcode).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt as _;
use tokio::sync::oneshot;

use crate::backend::{ChatBackend, ChatEvent, ChatRequest, ChatStream, ModelError, ModelResult};

/// Estimated cost of a request, used for shortest-job-first ordering and fast-
/// lane classification. Cheap heuristic — see `weight`.
#[derive(Clone, Copy, Debug)]
pub struct RequestCost {
    pub prompt_tokens: usize,
    pub max_tokens: usize,
}

impl RequestCost {
    /// Single scalar the scheduler orders by: total tokens the request will move
    /// through the engine (prompt prefill + generated).
    pub fn weight(&self) -> usize {
        self.prompt_tokens.saturating_add(self.max_tokens)
    }
}

/// Why an admission was refused outright (Phase D backpressure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmitError {
    /// The wait queue is full; the caller should shed load (HTTP 429).
    Overloaded,
}

impl std::fmt::Display for AdmitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdmitError::Overloaded => write!(f, "overloaded: admission queue full"),
        }
    }
}

/// Static configuration of the scheduler.
#[derive(Clone, Copy, Debug)]
pub struct AdmissionConfig {
    /// Initial admission limit (≤ engine capacity); also the recovery ceiling for
    /// the circuit breaker.
    pub limit: usize,
    /// A request whose `weight()` is below this uses the reserved fast-lane slot;
    /// `0` disables the fast lane.
    pub fastlane_tokens: usize,
    /// Max requests allowed to wait in the queue before new ones are rejected
    /// with `Overloaded`; `0` = unbounded.
    pub queue_max: usize,
}

impl AdmissionConfig {
    /// Build from a backend's advertised capacity and the generic admission env
    /// overrides: `ROZUM_ADMIT` (limit, default = capacity, clamped to capacity),
    /// `ROZUM_ADMIT_FASTLANE_TOKENS` (default 1024, `0` off),
    /// `ROZUM_ADMIT_QUEUE_MAX` (default 32, `0` unbounded).
    pub fn for_capacity(capacity: usize) -> Self {
        let cap = capacity.max(1);
        let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
        let limit = env_usize("ROZUM_ADMIT")
            .filter(|&n| n >= 1)
            .unwrap_or(cap)
            .min(cap);
        let fastlane_tokens = env_usize("ROZUM_ADMIT_FASTLANE_TOKENS").unwrap_or(1024);
        let queue_max = env_usize("ROZUM_ADMIT_QUEUE_MAX").unwrap_or(32);
        Self {
            limit,
            fastlane_tokens,
            queue_max,
        }
    }
}

/// Ordering key for a queued waiter: fast lane first, then shortest-job-first,
/// then FIFO. We scan the waiter list for the best *admittable* one rather than
/// popping a heap, because admissibility depends on live slot counts.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Priority {
    is_fast: bool,
    cost: usize,
    seq: u64,
}

struct Waiter {
    prio: Priority,
    tx: oneshot::Sender<AdmitGuard>,
}

struct State {
    /// Live admission limit; the circuit breaker moves this between 1 and
    /// `capacity`.
    limit: usize,
    /// Recovery ceiling — the configured admission limit the breaker recovers to.
    capacity: usize,
    fastlane_tokens: usize,
    queue_max: usize,
    general_in_use: usize,
    fast_in_use: usize,
    waiters: Vec<Waiter>,
    next_seq: u64,
    // Cumulative counters (observability, monotonic over the daemon's life).
    /// Requests that got a slot (immediately or after queueing).
    admitted_total: u64,
    /// Subset of `admitted_total` that took a reserved fast-lane slot.
    fast_admitted_total: u64,
    /// Requests rejected with HTTP 429 because the wait queue was full (shed load).
    shed_total: u64,
    /// Requests that had to WAIT for a slot (no free slot at arrival).
    queued_total: u64,
}

impl State {
    fn in_use(&self) -> usize {
        self.general_in_use + self.fast_in_use
    }

    /// General requests may occupy all but one slot, leaving one reserved for a
    /// fast request — but only when there are ≥2 slots and the fast lane is on.
    /// At a single slot the reservation would deadlock big requests, so it is
    /// disabled (the queue still orders fast requests first).
    fn general_cap(&self) -> usize {
        if self.fastlane_tokens > 0 && self.limit >= 2 {
            self.limit - 1
        } else {
            self.limit
        }
    }

    fn can_admit(&self, is_fast: bool) -> bool {
        // Hard total cap first; then general requests additionally yield the
        // reserved fast-lane slot(s).
        if self.in_use() >= self.limit {
            return false;
        }
        is_fast || self.general_in_use < self.general_cap()
    }

    fn take(&mut self, is_fast: bool) {
        if is_fast {
            self.fast_in_use += 1;
            self.fast_admitted_total += 1;
        } else {
            self.general_in_use += 1;
        }
        self.admitted_total += 1;
    }

    fn put_back(&mut self, is_fast: bool) {
        if is_fast {
            self.fast_in_use = self.fast_in_use.saturating_sub(1);
        } else {
            self.general_in_use = self.general_in_use.saturating_sub(1);
        }
    }

    /// Index of the best admittable waiter (fast first, then SJF, then FIFO),
    /// or `None` if no queued waiter can be admitted under the current counts.
    fn best_admittable(&self) -> Option<usize> {
        let mut best: Option<(usize, Priority)> = None;
        for (i, w) in self.waiters.iter().enumerate() {
            if !self.can_admit(w.prio.is_fast) {
                continue;
            }
            let better = match best {
                None => true,
                Some((_, b)) => prefer(w.prio, b),
            };
            if better {
                best = Some((i, w.prio));
            }
        }
        best.map(|(i, _)| i)
    }
}

/// `true` if `a` should be admitted before `b`: fast lane first, then lower cost,
/// then lower seq (FIFO).
fn prefer(a: Priority, b: Priority) -> bool {
    (
        a.is_fast,
        std::cmp::Reverse(a.cost),
        std::cmp::Reverse(a.seq),
    ) > (
        b.is_fast,
        std::cmp::Reverse(b.cost),
        std::cmp::Reverse(b.seq),
    )
}

/// Runtime-adjustable admission gate in front of the engine. Cheap to clone
/// (shares one inner state).
#[derive(Clone)]
pub struct AdmissionScheduler {
    inner: Arc<Mutex<State>>,
}

impl AdmissionScheduler {
    pub fn new(cfg: AdmissionConfig) -> Self {
        let limit = cfg.limit.max(1);
        Self {
            inner: Arc::new(Mutex::new(State {
                limit,
                capacity: limit,
                fastlane_tokens: cfg.fastlane_tokens,
                queue_max: cfg.queue_max,
                general_in_use: 0,
                fast_in_use: 0,
                waiters: Vec::new(),
                next_seq: 0,
                admitted_total: 0,
                fast_admitted_total: 0,
                shed_total: 0,
                queued_total: 0,
            })),
        }
    }

    /// Acquire a slot before calling the engine. Resolves immediately if a slot
    /// is free, otherwise queues (ordered fast-lane → SJF → FIFO) until one
    /// frees. The returned guard releases the slot on drop — including when the
    /// awaiting future is cancelled (client disconnect), so abandoned requests
    /// never hold a slot.
    pub async fn admit(&self, cost: RequestCost) -> Result<AdmitGuard, AdmitError> {
        let rx = {
            let mut s = self.inner.lock().unwrap();
            let is_fast = s.fastlane_tokens > 0 && cost.weight() < s.fastlane_tokens;
            if s.can_admit(is_fast) {
                s.take(is_fast);
                return Ok(AdmitGuard {
                    inner: Arc::clone(&self.inner),
                    is_fast,
                    armed: true,
                });
            }
            // No free slot: queue, unless the queue is full (Phase D backpressure).
            if s.queue_max > 0 && s.waiters.len() >= s.queue_max {
                s.shed_total += 1;
                return Err(AdmitError::Overloaded);
            }
            let seq = s.next_seq;
            s.next_seq += 1;
            s.queued_total += 1;
            let (tx, rx) = oneshot::channel();
            s.waiters.push(Waiter {
                prio: Priority {
                    is_fast,
                    cost: cost.weight(),
                    seq,
                },
                tx,
            });
            rx
        };
        // Woken with a fully-accounted guard. If the scheduler is dropped while we
        // wait (shutdown), fall back to an inert guard so callers never panic.
        Ok(rx.await.unwrap_or(AdmitGuard {
            inner: Arc::clone(&self.inner),
            is_fast: false,
            armed: false,
        }))
    }

    /// Circuit breaker: drop the live admission limit by one (floor 1) after a
    /// runtime allocation failure, letting in-flight requests drain before new
    /// concurrency is allowed. Returns the new limit.
    pub fn trip(&self) -> usize {
        let mut s = self.inner.lock().unwrap();
        s.limit = s.limit.saturating_sub(1).max(1);
        s.limit
    }

    /// Circuit breaker recovery: raise the live limit by one toward `capacity`
    /// and admit any waiters the extra slot allows. Returns the new limit.
    pub fn recover_step(&self) -> usize {
        let mut s = self.inner.lock().unwrap();
        s.limit = (s.limit + 1).min(s.capacity);
        Self::pump(&self.inner, &mut s);
        s.limit
    }

    /// Move the live admission limit (Phase D circuit breaker). Lowering it lets
    /// in-flight requests drain; raising it immediately admits queued waiters.
    pub fn set_limit(&self, limit: usize) {
        let mut s = self.inner.lock().unwrap();
        s.limit = limit.max(1);
        Self::pump(&self.inner, &mut s);
    }

    /// **Adaptive steady-state ceiling** (cascade Phase 9). The per-model AIMD controller owns this:
    /// it's both the live admission limit *and* the breaker's recovery ceiling. The breaker (`trip`
    /// / `recover_step`) then operates as a fast inner loop *within* `[1, ceiling]` — an acute OOM
    /// still drops `limit` instantly and drains, but recovery can't climb back above what the
    /// controller has learned the model sustains. The two controllers compose instead of fighting:
    /// the breaker handles sub-update transients, the controller sets the steady state.
    pub fn set_ceiling(&self, ceiling: usize) {
        let mut s = self.inner.lock().unwrap();
        let c = ceiling.max(1);
        s.capacity = c;
        // The controller raises the ceiling only on healthy evidence and lowers it on load, so its
        // recommendation is authoritative for the steady-state live limit; the breaker dips below
        // between updates.
        s.limit = c;
        Self::pump(&self.inner, &mut s);
    }

    /// Snapshot for tests / observability: `(in_use, waiting, limit)`.
    pub fn stats(&self) -> (usize, usize, usize) {
        let s = self.inner.lock().unwrap();
        (s.in_use(), s.waiters.len(), s.limit)
    }

    /// Cumulative observability counters `(admitted, fast_lane, shed, queued)`, monotonic
    /// over the daemon's life: total requests admitted, the fast-lane subset, requests shed
    /// with HTTP 429 (full queue), and requests that had to wait for a slot.
    pub fn counters(&self) -> (u64, u64, u64, u64) {
        let s = self.inner.lock().unwrap();
        (
            s.admitted_total,
            s.fast_admitted_total,
            s.shed_total,
            s.queued_total,
        )
    }

    /// Admit as many queued waiters as currently fit. Called after a slot frees
    /// or the limit rises. Each successful hand-off transfers an already-counted
    /// guard; a waiter whose receiver has gone (cancelled) is skipped and its
    /// reserved slot reclaimed inline (the returned guard is disarmed so its drop
    /// is a no-op — avoiding re-entry into this locked section).
    fn pump(inner: &Arc<Mutex<State>>, s: &mut State) {
        loop {
            s.waiters.retain(|w| !w.tx.is_closed());
            let Some(idx) = s.best_admittable() else {
                break;
            };
            let w = s.waiters.remove(idx);
            s.take(w.prio.is_fast);
            let guard = AdmitGuard {
                inner: Arc::clone(inner),
                is_fast: w.prio.is_fast,
                armed: true,
            };
            if let Err(mut returned) = w.tx.send(guard) {
                returned.armed = false; // receiver gone: no-op drop
                s.put_back(w.prio.is_fast);
                // `take` counted an admission that never reached the (gone) client — undo it.
                s.admitted_total = s.admitted_total.saturating_sub(1);
                if w.prio.is_fast {
                    s.fast_admitted_total = s.fast_admitted_total.saturating_sub(1);
                }
            }
        }
    }

    fn release(inner: &Arc<Mutex<State>>, is_fast: bool) {
        let mut s = inner.lock().unwrap();
        s.put_back(is_fast);
        Self::pump(inner, &mut s);
    }
}

/// Held for the lifetime of an admitted request; releasing on drop frees the slot
/// and wakes the next queued waiter.
pub struct AdmitGuard {
    inner: Arc<Mutex<State>>,
    is_fast: bool,
    armed: bool,
}

impl Drop for AdmitGuard {
    fn drop(&mut self) {
        if self.armed {
            AdmissionScheduler::release(&self.inner, self.is_fast);
        }
    }
}

// ── Memory budget (any in-process backend can use it to size its capacity) ──────

/// Prefill activation peak per token (`mistralrs-chunked-prefill.md`: ~465 KB,
/// one hidden-state-sized tensor per token). With chunked prefill the peak is
/// bounded by the chunk size, so each concurrent prefill slot costs a *constant*
/// `chunk * this` regardless of prompt length — which makes a memory budget over
/// slots meaningful.
pub const PREFILL_PEAK_BYTES_PER_TOKEN: u64 = 465 * 1024;

/// Compute sweet-spot ceiling on budgeted concurrency. A single GPU saturates
/// past a handful of concurrent prefills, so extra slots only add tail latency.
pub const DEFAULT_SEQS_CEILING: usize = 8;

/// Fraction of currently-available RAM we commit, leaving slack for the OS and
/// for transient spikes the per-seq estimate doesn't capture.
const BUDGET_SAFETY_FRAC: f64 = 0.8;

/// Transient memory of one concurrent prefill slot: `chunk_tokens` × the
/// per-token peak.
pub fn per_seq_prefill_peak(chunk_tokens: usize) -> u64 {
    chunk_tokens as u64 * PREFILL_PEAK_BYTES_PER_TOKEN
}

/// Inputs to the concurrency budget, gathered at load time from the actual model.
/// All memory terms in bytes.
#[derive(Clone, Copy, Debug)]
pub struct ConcurrencyBudget {
    /// RAM free right now, before weights load.
    pub available_ram: Option<u64>,
    /// Model weights that become resident on load.
    pub weights: Option<u64>,
    /// Paged KV pool, sized from the context window.
    pub kv_pool: Option<u64>,
    /// Transient cost of one concurrent prefill ([`per_seq_prefill_peak`]).
    pub per_seq_peak: u64,
    /// Compute sweet-spot cap.
    pub ceiling: usize,
}

/// Budgeted concurrent capacity: how many concurrent prefills fit in the RAM left
/// after the resident model, clamped to `[1, ceiling]`.
///
/// `slots = floor((safety·available − weights − kv_pool) / per_seq_peak)`.
/// Floors at `1` (one request must always run; the preflight decides whether it
/// fits at all). Falls back to the `1` floor when any memory term is unknown
/// (e.g. weights not cached yet on first run) — the **safe stub** for a backend
/// that advertises capacity but can't yet size its footprint.
pub fn budgeted_max_num_seqs(b: &ConcurrencyBudget) -> usize {
    let (Some(available), Some(weights), Some(kv_pool)) = (b.available_ram, b.weights, b.kv_pool)
    else {
        return 1;
    };
    if b.per_seq_peak == 0 || b.ceiling == 0 {
        return 1;
    }
    let headroom = available as f64 * BUDGET_SAFETY_FRAC - weights as f64 - kv_pool as f64;
    if headroom <= 0.0 {
        return 1;
    }
    let slots = (headroom / b.per_seq_peak as f64).floor() as usize;
    slots.clamp(1, b.ceiling)
}

// ── Cost estimation (shared, backend-independent) ───────────────────────────────

/// Cheap cost estimate for admission ordering: ~4 chars/token over the rendered
/// message text plus the requested generation budget. Only needs to separate a
/// "quick follow-up" from a "large context read".
pub fn estimate_cost(req: &ChatRequest) -> RequestCost {
    use crate::backend::ContentBlock;
    let chars: usize = req
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .map(|b| match b {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolResult { content, .. } => content.len(),
            _ => 0,
        })
        .sum();
    let max_tokens = req.sampling.max_tokens.map(|m| m as usize).unwrap_or(4096);
    RequestCost {
        prompt_tokens: chars / 4,
        max_tokens,
    }
}

// ── Circuit-breaker trip on a backend's resource-exhaustion error ───────────────

/// Best-effort: if `err` looks like a runtime allocation failure, trip the
/// breaker (lower the live limit) and schedule a one-step recovery after a
/// cooldown. Detection is substring-based — runtime OOM (esp. on Metal) is often
/// fatal, so the load-time budget is the primary defence and this is a secondary
/// guard that calms concurrency after a near miss.
pub fn note_backend_error(scheduler: &AdmissionScheduler, err: &ModelError) {
    const OOM_COOLDOWN_SECS: u64 = 30;
    let m = err.to_string().to_lowercase();
    let looks_like_oom = m.contains("out of memory")
        || m.contains("oom")
        || m.contains("failed to allocate")
        || m.contains("insufficient memory")
        || m.contains("exceeds total capacity");
    if !looks_like_oom {
        return;
    }
    let limit = scheduler.trip();
    eprintln!("concurrency: allocation failure → admission limit lowered to {limit}");
    crate::obs::log_event(serde_json::json!({
        "event": "admission_circuit_trip", "new_limit": limit, "reason": err.to_string(),
    }));
    let s = scheduler.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(OOM_COOLDOWN_SECS)).await;
        let restored = s.recover_step();
        crate::obs::log_event(serde_json::json!({
            "event": "admission_circuit_recover", "new_limit": restored,
        }));
    });
}

// ── AdmittingBackend: the decorator ─────────────────────────────────────────────

/// Wraps any [`ChatBackend`] with admission control, backpressure, and the
/// circuit breaker. Construct directly, or use [`admit_wrap`] to apply it only to
/// backends that advertise a capacity.
pub struct AdmittingBackend {
    inner: Arc<dyn ChatBackend>,
    scheduler: AdmissionScheduler,
    /// Optional per-model AIMD controller (Phase 9). When set, every completed request feeds a
    /// [`ConcurrencySample`] and the controller drives [`AdmissionScheduler::set_ceiling`] —
    /// closing the adaptive loop on real traffic, with the breaker as the fast inner loop.
    adaptive: Option<Arc<AdaptiveConcurrency>>,
    /// EWMA of low-concurrency latency (ms/token), the baseline the latency signal compares
    /// against. `None` until the first unsaturated request establishes it.
    latency_baseline: Arc<Mutex<Option<f64>>>,
}

impl AdmittingBackend {
    pub fn new(inner: Arc<dyn ChatBackend>, cfg: AdmissionConfig) -> Self {
        Self {
            inner,
            scheduler: AdmissionScheduler::new(cfg),
            adaptive: None,
            latency_baseline: Arc::new(Mutex::new(None)),
        }
    }

    /// Like [`new`](Self::new) but with the AIMD controller live: the admission ceiling self-tunes
    /// from each request's outcome (success → probe up; overload/error → back off). The scheduler
    /// **starts serial** (limit 1) — measured, not assumed — and the controller opens it up to the
    /// advertised capacity as healthy traffic accumulates.
    pub fn with_adaptive(
        inner: Arc<dyn ChatBackend>,
        cfg: AdmissionConfig,
        adaptive: AdaptiveConcurrency,
    ) -> Self {
        let cfg = AdmissionConfig { limit: 1, ..cfg };
        Self {
            inner,
            scheduler: AdmissionScheduler::new(cfg),
            adaptive: Some(Arc::new(adaptive)),
            latency_baseline: Arc::new(Mutex::new(None)),
        }
    }
}

/// A backend error that signals *overload* (vs a one-off failure) — a resource-exhaustion / OOM or
/// a provider rate-limit / quota. Drives the AIMD controller to back off (and the breaker to trip).
fn looks_like_overload(err: &ModelError) -> bool {
    let m = err.to_string().to_lowercase();
    m.contains("out of memory")
        || m.contains("oom")
        || m.contains("failed to allocate")
        || m.contains("insufficient memory")
        || m.contains("exceeds total capacity")
        || m.contains("429")
        || m.contains("rate limit")
        || m.contains("too many requests")
        || m.contains("overloaded")
        || m.contains("quota")
}

/// Apply one [`ConcurrencySample`] to the per-model controller and push the new ceiling. No-op
/// when the controller is off.
fn apply_sample(
    adaptive: &Option<Arc<AdaptiveConcurrency>>,
    scheduler: &AdmissionScheduler,
    model: &str,
    sample: ConcurrencySample,
) {
    if let Some(ctrl) = adaptive {
        scheduler.set_ceiling(ctrl.record(model, sample));
    }
}

/// EWMA factor for the latency baseline (slow-moving — it's the model's *unsaturated* speed).
const LAT_BASELINE_ALPHA: f64 = 0.2;
/// Ignore latency from tiny outputs — prefill dominates and `ms/token` is noise below this.
const LAT_MIN_TOKENS: u32 = 4;

/// The latency signal: at **low concurrency** the per-token latency *is* the baseline (fold it into
/// the EWMA, no ratio — an unsaturated request can't be "too slow"); under concurrency the ratio is
/// `per_token / baseline` (saturation shows as a ratio > 1). Returns `(new_baseline, ratio)`.
/// A baseline of `None` (not yet established) yields no ratio.
fn latency_signal(
    prev: Option<f64>,
    per_token_ms: f64,
    low_concurrency: bool,
) -> (Option<f64>, Option<f32>) {
    if low_concurrency {
        let base = match prev {
            Some(b) => LAT_BASELINE_ALPHA * per_token_ms + (1.0 - LAT_BASELINE_ALPHA) * b,
            None => per_token_ms,
        };
        (Some(base), None)
    } else {
        (prev, prev.map(|b| (per_token_ms / b.max(1e-6)) as f32))
    }
}

#[async_trait]
impl ChatBackend for AdmittingBackend {
    async fn chat(&self, req: ChatRequest) -> ModelResult<ChatStream> {
        // Admit before delegating, so an overloaded queue surfaces as a real
        // HTTP 429 (the gateway maps `Overloaded`) rather than an in-band error
        // after the response has started. A non-overloaded request parks here
        // until a slot frees; a client disconnect drops this future and the
        // queued waiter self-cleans.
        let guard = match self.scheduler.admit(estimate_cost(&req)).await {
            Ok(g) => g,
            Err(AdmitError::Overloaded) => {
                return Err(ModelError::Overloaded(
                    "backend at capacity; retry shortly".into(),
                ));
            }
        };
        let inner = Arc::clone(&self.inner);
        let scheduler = self.scheduler.clone();
        let adaptive = self.adaptive.clone();
        let baseline = Arc::clone(&self.latency_baseline);
        let model = self.inner.label().to_string();
        let stream: ChatStream = Box::pin(async_stream::stream! {
            // Hold the slot for the whole stream; dropping it on
            // completion/disconnect frees the slot and wakes the next waiter.
            let _guard = guard;
            // Measure generation latency and the concurrency it ran at (for the adaptive signal).
            let started = std::time::Instant::now();
            let low_concurrency = scheduler.stats().0 <= 1; // only this request in flight → unsaturated
            let mut output_tokens = 0u32;
            let mut inner_stream = match inner.chat(req).await {
                Ok(s) => s,
                Err(e) => {
                    note_backend_error(&scheduler, &e);
                    apply_sample(&adaptive, &scheduler, &model, ConcurrencySample {
                        overload: looks_like_overload(&e), ok: false, headroom: None, latency_ratio: None,
                    });
                    yield Err(e);
                    return;
                }
            };
            let mut errored = false;
            while let Some(item) = inner_stream.next().await {
                match &item {
                    Err(e) => {
                        note_backend_error(&scheduler, e);
                        if !errored {
                            apply_sample(&adaptive, &scheduler, &model, ConcurrencySample {
                                overload: looks_like_overload(e), ok: false, headroom: None, latency_ratio: None,
                            });
                            errored = true;
                        }
                    }
                    Ok(ChatEvent::Done { output_tokens: ot, .. }) => output_tokens = *ot,
                    _ => {}
                }
                yield item;
            }
            // A clean run probes concurrency up, with the latency signal when the output is large
            // enough to be meaningful (small outputs are prefill-dominated noise).
            if !errored && adaptive.is_some() {
                let latency_ratio = if output_tokens >= LAT_MIN_TOKENS {
                    let per_token = started.elapsed().as_secs_f64() * 1000.0 / output_tokens as f64;
                    let prev = *baseline.lock().unwrap();
                    let (new_base, ratio) = latency_signal(prev, per_token, low_concurrency);
                    *baseline.lock().unwrap() = new_base;
                    ratio
                } else {
                    None
                };
                apply_sample(&adaptive, &scheduler, &model, ConcurrencySample {
                    overload: false, ok: true, headroom: None, latency_ratio,
                });
            }
        });
        Ok(stream)
    }

    fn context_window(&self) -> u32 {
        self.inner.context_window()
    }

    fn label(&self) -> &'static str {
        self.inner.label()
    }

    fn concurrency_capacity(&self) -> Option<usize> {
        self.inner.concurrency_capacity()
    }

    fn admission_stats(&self) -> Option<crate::backend::AdmissionSnapshot> {
        let (in_use, waiting, limit) = self.scheduler.stats();
        let (admitted, fast_lane, shed, queued) = self.scheduler.counters();
        Some(crate::backend::AdmissionSnapshot {
            limit,
            in_use,
            waiting,
            admitted,
            fast_lane,
            shed,
            queued,
        })
    }

    fn report_quality(&self, ok: bool) {
        // A rejected answer is a quality-vs-concurrency red signal; an accepted one isn't rewarded
        // here (the throughput path already probes up). No-op when adaptive control is off.
        if !ok {
            let model = self.inner.label();
            apply_sample(
                &self.adaptive,
                &self.scheduler,
                model,
                ConcurrencySample { overload: false, ok: false, headroom: None, latency_ratio: None },
            );
        }
    }
}

/// Wrap `inner` with admission control **iff** it advertises a concurrency
/// capacity; otherwise return it unchanged. Remote / self-serializing backends
/// (which return `None`) pass through untouched — the safe default.
///
/// With `ROZUM_ADAPTIVE_CONCURRENCY=1` the wrapped backend's admission ceiling **self-tunes** via
/// the Phase-9 AIMD controller (start serial, probe up on healthy traffic, back off on overload),
/// bounded by the advertised capacity. Off by default — the static budgeted limit is unchanged.
pub fn admit_wrap(inner: Arc<dyn ChatBackend>) -> Arc<dyn ChatBackend> {
    match inner.concurrency_capacity() {
        Some(cap) => {
            let cfg = AdmissionConfig::for_capacity(cap);
            if adaptive_concurrency_enabled() {
                let ctrl = AdaptiveConcurrency::new(AdaptiveConfig::for_capacity(cap));
                Arc::new(AdmittingBackend::with_adaptive(inner, cfg, ctrl))
            } else {
                Arc::new(AdmittingBackend::new(inner, cfg))
            }
        }
        None => inner,
    }
}

/// Whether the adaptive admission ceiling is enabled (`ROZUM_ADAPTIVE_CONCURRENCY` truthy).
fn adaptive_concurrency_enabled() -> bool {
    matches!(std::env::var("ROZUM_ADAPTIVE_CONCURRENCY").ok().as_deref(), Some("1" | "true" | "on"))
}

// ── Adaptive per-model concurrency (cascade Phase 9) ─────────────────────────────

/// One completed request's worth of evidence about whether a model can take *more* concurrency.
/// The cascade / agent fills these from what it already tracks — a load failure (`FailReason`
/// classified to `overload`), the local resource snapshot, the observed latency, and whether the
/// answer actually worked.
#[derive(Debug, Clone, Copy)]
pub struct ConcurrencySample {
    /// A failure attributable to *load*: rate-limit / quota / OOM (a hard red — back off now).
    pub overload: bool,
    /// Local free-resource headroom at dispatch as a fraction `[0,1]` (`None` for remotes). Below
    /// [`AdaptiveConfig::min_headroom`] → back off *before* the next OOM, not after.
    pub headroom: Option<f32>,
    /// Observed latency over the model's low-load baseline (`observed / baseline`; `None` until a
    /// baseline exists). Above [`AdaptiveConfig::max_latency_ratio`] → the model is saturating.
    pub latency_ratio: Option<f32>,
    /// Did the answer pass (exec-feedback / judge)? A failure *while running concurrently* is a
    /// quality-vs-concurrency red flag; at the floor it's not a concurrency problem and is ignored.
    pub ok: bool,
}

impl ConcurrencySample {
    /// A healthy sample with no resource pressure — the common case (probe up).
    pub fn healthy() -> Self {
        Self { overload: false, headroom: None, latency_ratio: None, ok: true }
    }
    /// A load-failure sample (429 / quota / OOM) — back off.
    pub fn overloaded() -> Self {
        Self { overload: true, ..Self::healthy() }
    }
}

/// AIMD thresholds for [`AdaptiveConcurrency`].
#[derive(Debug, Clone, Copy)]
pub struct AdaptiveConfig {
    /// Floor — one request must always be admissible.
    pub min: usize,
    /// Ceiling — never probe past the model's hard capacity (e.g. the budgeted slot count).
    pub max: usize,
    /// Multiplicative back-off on a red signal (`0.5` halves the limit).
    pub backoff: f64,
    /// Consecutive green samples before the additive +1 probe.
    pub probe_after: u32,
    /// Local headroom floor; below this fraction is red.
    pub min_headroom: f32,
    /// Latency-ratio ceiling; above this is red.
    pub max_latency_ratio: f32,
}

impl AdaptiveConfig {
    /// Sensible defaults for a model whose hard capacity is `max` (start serial, probe up cautiously).
    pub fn for_capacity(max: usize) -> Self {
        Self {
            min: 1,
            max: max.max(1),
            backoff: 0.5,
            probe_after: 5,
            min_headroom: 0.15,
            max_latency_ratio: 2.0,
        }
    }
}

struct AimdState {
    limit: usize,
    green: u32,
}

/// Per-model **AIMD** concurrency controller (Phase 9). Each model's right concurrency level is
/// unknown and model-specific, so we *measure* it: probe the admission limit up by one after a run
/// of healthy requests (additive increase), and the moment the model shows load — an overload
/// failure, thin resource headroom, a latency cliff, or quality degrading under concurrency —
/// multiplicatively back off. The limit oscillates around each model's demonstrated sweet spot
/// without ever assuming it (classic TCP-style congestion control over request admission).
///
/// Feed it [`ConcurrencySample`]s via [`record`](Self::record); push the returned target onto the
/// model's [`AdmissionScheduler::set_limit`]. It composes with the Phase-6 residency lanes: the
/// effective live width of a model is `min(this adaptive limit, its lane's residency share)`.
pub struct AdaptiveConcurrency {
    cfg: AdaptiveConfig,
    state: Mutex<HashMap<String, AimdState>>,
}

impl AdaptiveConcurrency {
    pub fn new(cfg: AdaptiveConfig) -> Self {
        Self { cfg, state: Mutex::new(HashMap::new()) }
    }

    /// Fold one sample for `model` and return its new target limit.
    pub fn record(&self, model: &str, sample: ConcurrencySample) -> usize {
        let floor = self.cfg.min.max(1);
        let mut map = self.state.lock().unwrap();
        let st = map.entry(model.to_string()).or_insert(AimdState { limit: floor, green: 0 });

        // Hard reds (load / resource / latency) back off at any level.
        let hard_red = sample.overload
            || sample.headroom.is_some_and(|h| h < self.cfg.min_headroom)
            || sample.latency_ratio.is_some_and(|r| r > self.cfg.max_latency_ratio);

        if hard_red || (!sample.ok && st.limit > floor) {
            // Back off — a load signal, or quality degrading *under concurrency*.
            st.limit = ((st.limit as f64 * self.cfg.backoff).floor() as usize).max(floor);
            st.green = 0;
        } else if sample.ok {
            // A clean run: count toward the next additive probe.
            st.green += 1;
            if st.green >= self.cfg.probe_after && st.limit < self.cfg.max {
                st.limit += 1;
                st.green = 0;
            }
        }
        // else: a quality failure at the floor — neutral. Not a concurrency problem, and we can't
        // go lower; don't reward it with a probe either.
        st.limit
    }

    /// The model's current target limit (the floor if it's never been seen).
    pub fn target_limit(&self, model: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .get(model)
            .map(|s| s.limit)
            .unwrap_or(self.cfg.min.max(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(limit: usize, fastlane: usize) -> AdmissionConfig {
        AdmissionConfig {
            limit,
            fastlane_tokens: fastlane,
            queue_max: 0, // unbounded unless a test sets it
        }
    }

    fn big() -> RequestCost {
        RequestCost {
            prompt_tokens: 20_000,
            max_tokens: 1024,
        }
    }
    fn small() -> RequestCost {
        RequestCost {
            prompt_tokens: 50,
            max_tokens: 64,
        }
    }

    #[tokio::test]
    async fn admits_up_to_limit_then_queues() {
        let s = AdmissionScheduler::new(cfg(2, 0)); // fast lane off
        let g1 = s.admit(big()).await.unwrap();
        let g2 = s.admit(big()).await.unwrap();
        assert_eq!(s.stats(), (2, 0, 2));

        // Third request must queue.
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.stats().1, 1, "third request should be queued");

        drop(g1); // frees a slot → queued waiter admitted
        let _g3 = waiter.await.unwrap();
        assert_eq!(s.stats(), (2, 0, 2));
        drop(g2);
    }

    // Observability counters (admitted / fast_lane / shed / queued) track the whole flow.
    #[tokio::test]
    async fn counters_track_admit_fastlane_queue_and_shed() {
        // 2 slots (1 reserved for the fast lane), queue holds 1.
        let s = AdmissionScheduler::new(AdmissionConfig {
            limit: 2,
            fastlane_tokens: 1024,
            queue_max: 1,
        });
        assert_eq!(s.counters(), (0, 0, 0, 0));

        let g1 = s.admit(big()).await.unwrap(); // general slot
        assert_eq!(s.counters(), (1, 0, 0, 0), "first admit");
        let _gfast = s.admit(small()).await.unwrap(); // reserved fast-lane slot
        assert_eq!(s.counters(), (2, 1, 0, 0), "fast-lane hit counted");

        // Both slots busy → the next big request queues.
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.counters(), (2, 1, 0, 1), "queued counted");

        // Queue is full (queue_max 1) → the next request is shed with 429.
        assert!(matches!(s.admit(big()).await, Err(AdmitError::Overloaded)));
        assert_eq!(s.counters(), (2, 1, 1, 1), "shed counted");

        // Free the general slot → the queued waiter is admitted (admitted increments).
        drop(g1);
        let _g3 = waiter.await.unwrap();
        assert_eq!(s.counters(), (3, 1, 1, 1), "queued waiter admitted");
    }

    #[tokio::test]
    async fn fast_lane_jumps_ahead_of_a_queued_big_request() {
        let s = AdmissionScheduler::new(cfg(2, 1024)); // reserve 1 of 2 for fast
        // Two big requests: the first takes the single general slot; the second
        // cannot (general_cap = 1) and queues.
        let g1 = s.admit(big()).await.unwrap();
        let s_big = s.clone();
        let big_waiter = tokio::spawn(async move { s_big.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.stats(), (1, 1, 2), "second big request queued");

        // A small request uses the reserved slot immediately, despite the queue.
        let _gfast = s.admit(small()).await.unwrap();
        assert_eq!(
            s.stats().0,
            2,
            "fast request admitted into the reserved slot"
        );

        // The big request is still waiting (no free slot for it yet).
        assert!(!big_waiter.is_finished());
        drop(g1);
        let _gbig = big_waiter.await.unwrap();
    }

    #[tokio::test]
    async fn single_slot_serialises_but_orders_small_first() {
        let s = AdmissionScheduler::new(cfg(1, 1024)); // fast lane inert at 1 slot
        let g1 = s.admit(big()).await.unwrap(); // occupies the only slot
        // Queue a big then a small; when the slot frees, the small should win SJF.
        let sb = s.clone();
        let big_w = tokio::spawn(async move { sb.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(10)).await;
        let ss = s.clone();
        let small_w = tokio::spawn(async move { ss.admit(small()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(s.stats().1, 2, "both queued");

        drop(g1);
        // The small one is admitted first (SJF), the big one stays queued.
        let _gs = small_w.await.unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !big_w.is_finished(),
            "big request waits behind the small one"
        );
        assert_eq!(s.stats(), (1, 1, 1));
        drop(_gs);
        let _gb = big_w.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_waiter_releases_its_queue_slot() {
        let s = AdmissionScheduler::new(cfg(1, 0));
        let g1 = s.admit(big()).await.unwrap();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.stats().1, 1);

        waiter.abort(); // client disconnect while queued
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Freeing the slot must not get wedged on the dead waiter.
        drop(g1);
        let _g = tokio::time::timeout(Duration::from_millis(100), s.admit(big()))
            .await
            .expect("admit must resolve after a cancelled waiter")
            .unwrap();
        assert_eq!(s.stats().0, 1);
    }

    #[tokio::test]
    async fn raising_the_limit_admits_queued_waiters() {
        let s = AdmissionScheduler::new(cfg(1, 0));
        let _g1 = s.admit(big()).await.unwrap();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.stats().1, 1);

        s.set_limit(2); // raise the limit
        let _g2 = waiter.await.unwrap();
        assert_eq!(s.stats(), (2, 0, 2));
    }

    #[tokio::test]
    async fn full_queue_sheds_with_overloaded() {
        // 1 slot, queue capacity 1: one in-flight + one queued is the ceiling.
        let mut c = cfg(1, 0);
        c.queue_max = 1;
        let s = AdmissionScheduler::new(c);

        let _g1 = s.admit(big()).await.unwrap(); // fills the slot
        let s2 = s.clone();
        let _queued = tokio::spawn(async move { s2.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.stats().1, 1, "queue holds one waiter");

        // The next request finds the queue full → shed immediately, no waiting.
        assert!(matches!(s.admit(big()).await, Err(AdmitError::Overloaded)));
    }

    #[tokio::test]
    async fn circuit_breaker_trips_and_recovers() {
        let s = AdmissionScheduler::new(cfg(3, 0));
        // Trip twice on simulated allocation failures: 3 → 2 → 1 (floor).
        assert_eq!(s.trip(), 2);
        assert_eq!(s.trip(), 1);
        assert_eq!(s.trip(), 1, "limit floors at 1");
        assert_eq!(s.stats().2, 1);

        // With the limit at 1, a second request queues.
        let _g1 = s.admit(big()).await.unwrap();
        let s2 = s.clone();
        let waiter = tokio::spawn(async move { s2.admit(big()).await.unwrap() });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(s.stats().1, 1);

        // Recovery raises the limit toward capacity and admits the waiter; it
        // never exceeds the configured capacity (3).
        assert_eq!(s.recover_step(), 2);
        let _g2 = waiter.await.unwrap();
        assert_eq!(s.recover_step(), 3);
        assert_eq!(s.recover_step(), 3, "recovery caps at capacity");
    }

    // ── Budget math ─────────────────────────────────────────────────────────

    const GB: u64 = 1 << 30;

    /// 4-bit ~20 GB model + ~4 GB KV pool, 4096-token chunk (~1.82 GiB/slot).
    fn budget(available_gb: u64) -> ConcurrencyBudget {
        ConcurrencyBudget {
            available_ram: Some(available_gb * GB),
            weights: Some(20 * GB),
            kv_pool: Some(4 * GB),
            per_seq_peak: per_seq_prefill_peak(4096),
            ceiling: DEFAULT_SEQS_CEILING,
        }
    }

    #[test]
    fn budget_serialises_when_no_headroom_for_a_second_prefill() {
        assert_eq!(budgeted_max_num_seqs(&budget(32)), 1);
        assert_eq!(budgeted_max_num_seqs(&budget(36)), 2);
    }

    #[test]
    fn budget_scales_with_memory_up_to_the_ceiling() {
        assert_eq!(budgeted_max_num_seqs(&budget(48)), 7);
        assert_eq!(budgeted_max_num_seqs(&budget(64)), DEFAULT_SEQS_CEILING);
    }

    #[test]
    fn budget_floors_to_one_on_unknown_or_overcommitted_memory() {
        let mut b = budget(64);
        b.weights = None; // first run, weights not cached → safe stub
        assert_eq!(budgeted_max_num_seqs(&b), 1);
        assert_eq!(budgeted_max_num_seqs(&budget(16)), 1);
    }

    // ── Decorator ───────────────────────────────────────────────────────────

    use crate::backend::{ChatEvent, StopReason};

    /// A trivial backend that yields one Done event, with a configurable capacity.
    struct FakeBackend {
        capacity: Option<usize>,
    }

    #[async_trait]
    impl ChatBackend for FakeBackend {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            Ok(Box::pin(async_stream::stream! {
                yield Ok(ChatEvent::Done {
                    input_tokens: 0,
                    output_tokens: 0,
                    stop_reason: StopReason::EndTurn,
                });
            }))
        }
        fn context_window(&self) -> u32 {
            4096
        }
        fn concurrency_capacity(&self) -> Option<usize> {
            self.capacity
        }
    }

    #[test]
    fn admit_wrap_passes_through_when_no_capacity() {
        let inner: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: None });
        let wrapped = admit_wrap(Arc::clone(&inner));
        // Not wrapped: same allocation behind the Arc.
        assert!(Arc::ptr_eq(&inner, &wrapped));
    }

    #[test]
    fn admit_wrap_wraps_when_capacity_advertised() {
        let inner: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: Some(2) });
        let wrapped = admit_wrap(Arc::clone(&inner));
        assert!(!Arc::ptr_eq(&inner, &wrapped));
        assert_eq!(wrapped.context_window(), 4096, "delegates context_window");
        assert_eq!(
            wrapped.concurrency_capacity(),
            Some(2),
            "delegates capacity"
        );
    }

    #[tokio::test]
    async fn admitting_backend_streams_through_and_releases() {
        let inner: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: Some(2) });
        let backend = AdmittingBackend::new(
            inner,
            AdmissionConfig {
                limit: 2,
                fastlane_tokens: 0,
                queue_max: 32,
            },
        );

        // Drive a request end-to-end: the inner Done event flows through.
        let mut stream = backend.chat(ChatRequest::simple("hi")).await.unwrap();
        let first = stream.next().await.expect("an event").unwrap();
        assert!(matches!(first, ChatEvent::Done { .. }));
        assert!(stream.next().await.is_none(), "stream ends after Done");
        drop(stream);

        // The slot was released on stream drop: capacity is free again.
        assert_eq!(
            backend.scheduler.stats().0,
            0,
            "slot released after the stream"
        );
    }

    #[tokio::test]
    async fn admission_stats_reports_free_window() {
        let inner: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: Some(2) });
        let backend = AdmittingBackend::new(
            inner,
            AdmissionConfig {
                limit: 2,
                fastlane_tokens: 0,
                queue_max: 32,
            },
        );
        let s = backend.admission_stats().expect("admission-controlled");
        assert_eq!((s.limit, s.in_use, s.waiting), (2, 0, 0));
        assert_eq!(s.free(), 2);

        // A plain backend reports no admission state (ungated).
        let plain: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: None });
        assert!(plain.admission_stats().is_none());
    }

    /// A backend that succeeds for its first `ok_until` calls, then fails with `msg` (drives the
    /// probe-up-then-back-off path through one controller).
    struct FlakyBackend {
        ok_until: usize,
        msg: &'static str,
        calls: Arc<std::sync::atomic::AtomicUsize>,
        capacity: Option<usize>,
    }
    #[async_trait]
    impl ChatBackend for FlakyBackend {
        async fn chat(&self, _req: ChatRequest) -> ModelResult<ChatStream> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n < self.ok_until {
                Ok(Box::pin(async_stream::stream! {
                    yield Ok(ChatEvent::Done {
                        input_tokens: 0, output_tokens: 0, stop_reason: StopReason::EndTurn,
                    });
                }))
            } else {
                Err(ModelError::BackendUnavailable(self.msg.into()))
            }
        }
        fn context_window(&self) -> u32 {
            4096
        }
        fn concurrency_capacity(&self) -> Option<usize> {
            self.capacity
        }
    }

    async fn drain(mut s: ChatStream) {
        while s.next().await.is_some() {}
    }

    // ── Phase 9: adaptive per-model concurrency ─────────────────────────────

    // ── Phase 9 live: AIMD ⇄ circuit-breaker reconciliation ─────────────────

    #[test]
    fn set_ceiling_bounds_breaker_recovery() {
        // The controller (AIMD) owns the ceiling; the breaker recovers only up to it, not the
        // original capacity. Start at 4, controller lowers the ceiling to 2.
        let s = AdmissionScheduler::new(cfg(4, 0));
        s.set_ceiling(2);
        assert_eq!(s.stats().2, 2, "ceiling sets the live limit");
        // An acute OOM trips below the ceiling…
        assert_eq!(s.trip(), 1);
        // …and recovery walks back up to the controller's ceiling (2), never to the old 4.
        assert_eq!(s.recover_step(), 2);
        assert_eq!(s.recover_step(), 2, "breaker cannot climb above the AIMD ceiling");
    }

    #[test]
    fn set_ceiling_raises_and_lowers_the_live_limit() {
        let s = AdmissionScheduler::new(cfg(1, 0));
        s.set_ceiling(4); // controller opens it up
        assert_eq!(s.stats().2, 4);
        s.set_ceiling(2); // controller backs off
        assert_eq!(s.stats().2, 2);
    }

    #[tokio::test]
    async fn adaptive_admitting_backend_probes_up_on_clean_traffic() {
        let inner: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: Some(8) });
        let ctrl =
            AdaptiveConcurrency::new(AdaptiveConfig { probe_after: 2, ..AdaptiveConfig::for_capacity(8) });
        let backend = AdmittingBackend::with_adaptive(inner, AdmissionConfig::for_capacity(8), ctrl);
        assert_eq!(backend.scheduler.stats().2, 1, "adaptive backends start serial");

        for _ in 0..2 {
            drain(backend.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        }
        assert_eq!(backend.scheduler.stats().2, 2, "two clean runs → ceiling probes up to 2");
        for _ in 0..2 {
            drain(backend.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        }
        assert_eq!(backend.scheduler.stats().2, 3);
    }

    #[tokio::test]
    async fn adaptive_admitting_backend_backs_off_on_overload() {
        // Probe the ceiling up with clean runs, then one OOM halves it — all through one
        // controller, exercising both the success and the overload feed paths.
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inner: Arc<dyn ChatBackend> = Arc::new(FlakyBackend {
            ok_until: 4,
            msg: "out of memory",
            calls: calls.clone(),
            capacity: Some(8),
        });
        let ctrl =
            AdaptiveConcurrency::new(AdaptiveConfig { probe_after: 1, ..AdaptiveConfig::for_capacity(8) });
        let backend = AdmittingBackend::with_adaptive(inner, AdmissionConfig::for_capacity(8), ctrl);

        // 4 clean runs (probe_after 1) → ceiling 1→5.
        for _ in 0..4 {
            drain(backend.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        }
        let raised = backend.scheduler.stats().2;
        assert!(raised >= 4, "probed up first, got {raised}");

        // The 5th request OOMs → the controller halves the ceiling.
        let mut s = backend.chat(ChatRequest::simple("hi")).await.unwrap();
        assert!(s.next().await.unwrap().is_err(), "the backend OOMed");
        drop(s);
        let dropped = backend.scheduler.stats().2;
        assert!(dropped < raised, "an OOM backed the ceiling off: {raised} → {dropped}");
    }

    #[tokio::test]
    async fn report_quality_rejection_backs_off_adaptive_ceiling() {
        // A higher layer (the cascade) reporting a rejected answer backs the ceiling off; an
        // accepted answer is a no-op (the throughput path already rewards success).
        let inner: Arc<dyn ChatBackend> = Arc::new(FakeBackend { capacity: Some(8) });
        let ctrl =
            AdaptiveConcurrency::new(AdaptiveConfig { probe_after: 1, ..AdaptiveConfig::for_capacity(8) });
        let backend = AdmittingBackend::with_adaptive(inner, AdmissionConfig::for_capacity(8), ctrl);
        for _ in 0..3 {
            drain(backend.chat(ChatRequest::simple("hi")).await.unwrap()).await;
        }
        let raised = backend.scheduler.stats().2;
        assert!(raised >= 3, "probed up first, got {raised}");

        backend.report_quality(false);
        let dropped = backend.scheduler.stats().2;
        assert!(dropped < raised, "a quality rejection backs off: {raised} → {dropped}");
        backend.report_quality(true);
        assert_eq!(backend.scheduler.stats().2, dropped, "an accepted answer doesn't move it");
    }

    #[test]
    fn latency_signal_sets_baseline_then_ratios() {
        // A low-concurrency request establishes the baseline (no ratio — it can't be "too slow").
        let (b1, r1) = latency_signal(None, 10.0, true);
        assert_eq!(b1, Some(10.0));
        assert_eq!(r1, None);
        // Under concurrency, a 3× slowdown surfaces as ratio 3.0 (the saturation red signal).
        let (b2, r2) = latency_signal(b1, 30.0, false);
        assert_eq!(b2, Some(10.0), "baseline is not moved by a loaded sample");
        assert!((r2.unwrap() - 3.0).abs() < 1e-5);
        // A faster unsaturated sample folds into the EWMA baseline (toward 8.8), still no ratio.
        let (b3, r3) = latency_signal(b1, 6.0, true);
        assert!((b3.unwrap() - (0.2 * 6.0 + 0.8 * 10.0)).abs() < 1e-9);
        assert_eq!(r3, None);
        // No baseline yet + under load → no opinion.
        assert_eq!(latency_signal(None, 50.0, false), (None, None));
    }

    #[test]
    fn adaptive_latency_cliff_backs_off_via_signal() {
        // The controller treats a latency_ratio over its ceiling as red (back off), under it as
        // green (probe up) — the AdmittingBackend feeds this from the latency_signal above.
        let c = adaptive(8, 1);
        for _ in 0..3 {
            c.record("m", ConcurrencySample::healthy());
        }
        let raised = c.target_limit("m");
        assert!(raised >= 3);
        let slow = ConcurrencySample { latency_ratio: Some(3.0), ..ConcurrencySample::healthy() };
        assert!(c.record("m", slow) < raised, "a latency cliff backs the limit off");
    }

    fn adaptive(max: usize, probe_after: u32) -> AdaptiveConcurrency {
        AdaptiveConcurrency::new(AdaptiveConfig { probe_after, ..AdaptiveConfig::for_capacity(max) })
    }

    #[test]
    fn adaptive_probes_up_under_healthy_load() {
        let c = adaptive(4, 2);
        assert_eq!(c.target_limit("m"), 1, "starts serial");
        // Two clean runs → +1; capped at max.
        assert_eq!(c.record("m", ConcurrencySample::healthy()), 1);
        assert_eq!(c.record("m", ConcurrencySample::healthy()), 2, "additive increase after probe_after");
        c.record("m", ConcurrencySample::healthy());
        assert_eq!(c.record("m", ConcurrencySample::healthy()), 3);
        for _ in 0..10 {
            c.record("m", ConcurrencySample::healthy());
        }
        assert_eq!(c.target_limit("m"), 4, "never probes past the hard ceiling");
    }

    #[test]
    fn adaptive_backs_off_on_overload() {
        let c = adaptive(8, 2);
        // Probe up to 4 (3 increments → 6 clean runs).
        for _ in 0..6 {
            c.record("m", ConcurrencySample::healthy());
        }
        assert_eq!(c.target_limit("m"), 4);
        // A load failure halves it immediately.
        assert_eq!(c.record("m", ConcurrencySample::overloaded()), 2, "multiplicative back-off");
    }

    #[test]
    fn adaptive_floor_holds_under_overload() {
        let c = adaptive(4, 2);
        // Repeated overload can't drive the limit below 1 (one request must always run).
        for _ in 0..5 {
            c.record("m", ConcurrencySample::overloaded());
        }
        assert_eq!(c.target_limit("m"), 1);
    }

    #[test]
    fn adaptive_low_headroom_and_latency_cliff_are_red() {
        let c = adaptive(8, 2);
        for _ in 0..6 {
            c.record("m", ConcurrencySample::healthy());
        }
        assert_eq!(c.target_limit("m"), 4);
        // Thin local memory headroom → back off (before an OOM).
        let tight = ConcurrencySample { headroom: Some(0.05), ..ConcurrencySample::healthy() };
        assert_eq!(c.record("m", tight), 2);
        // A latency cliff → back off again.
        let slow = ConcurrencySample { latency_ratio: Some(3.0), ..ConcurrencySample::healthy() };
        assert_eq!(c.record("m", slow), 1);
    }

    #[test]
    fn adaptive_quality_failure_is_red_only_above_floor() {
        let c = adaptive(8, 2);
        for _ in 0..6 {
            c.record("m", ConcurrencySample::healthy());
        }
        assert_eq!(c.target_limit("m"), 4);
        // A bad answer while running concurrently → quality-vs-concurrency back-off.
        let bad = ConcurrencySample { ok: false, ..ConcurrencySample::healthy() };
        assert_eq!(c.record("m", bad), 2);
        // Drive to the floor, then a bad answer is neutral (serial → not a concurrency problem).
        c.record("m", ConcurrencySample { ok: false, ..ConcurrencySample::healthy() }); // 2 → 1
        assert_eq!(c.target_limit("m"), 1);
        assert_eq!(c.record("m", bad), 1, "a failure at the floor neither backs off nor probes up");
    }

    #[test]
    fn adaptive_drives_a_real_admission_scheduler() {
        // The actuator binding: push the controller's target onto a live AdmissionScheduler.
        let sched = AdmissionScheduler::new(AdmissionConfig::for_capacity(8));
        let c = adaptive(8, 2);
        for _ in 0..6 {
            let target = c.record("m", ConcurrencySample::healthy());
            sched.set_limit(target);
        }
        assert_eq!(sched.stats().2, 4, "scheduler limit rose with the adaptive target");
        let target = c.record("m", ConcurrencySample::overloaded());
        sched.set_limit(target);
        assert_eq!(sched.stats().2, 2, "scheduler limit dropped on back-off");
    }
}
