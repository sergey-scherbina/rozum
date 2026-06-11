//! Admission scheduling in front of the mistralrs engine.
//!
//! Phase B+C of `docs/specs/mistralrs-concurrency-scheduling.md`. The engine's
//! `max_num_seqs` is fixed at build time (Phase A budgets it); this layer gates
//! the *actual* concurrency with a runtime-adjustable semaphore so we can add
//! priority, a reserved fast lane, and (Phase D) backpressure — none of which
//! the static engine knob can express.
//!
//! Responsiveness scope: this controls **admission order** — a short request is
//! admitted as soon as a slot frees, ahead of queued big requests, and never
//! waits for a big request's full generation. It does **not** preempt an
//! in-flight engine step: the fork runs a prompt's chunked prefill within one
//! `pipeline::step` (it does not yield between chunks), so mid-big-prefill
//! preemption would need an engine-side change (see the spec's escalation note).
//!
//! The component is engine-agnostic (no `mistralrs` types), so it builds and is
//! unit-tested without the `mistralrs` feature.

use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

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
    /// Build from the engine capacity and env overrides:
    /// `ROZUM_MISTRALRS_ADMIT` (admission limit, default = capacity, clamped to
    /// capacity) and `ROZUM_MISTRALRS_FASTLANE_TOKENS` (default 1024, `0` off).
    pub fn from_engine_capacity(engine_capacity: usize) -> Self {
        let cap = engine_capacity.max(1);
        let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
        let limit = env_usize("ROZUM_MISTRALRS_ADMIT")
            .filter(|&n| n >= 1)
            .unwrap_or(cap)
            .min(cap);
        let fastlane_tokens = env_usize("ROZUM_MISTRALRS_FASTLANE_TOKENS").unwrap_or(1024);
        let queue_max = env_usize("ROZUM_MISTRALRS_QUEUE_MAX").unwrap_or(32);
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
        } else {
            self.general_in_use += 1;
        }
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
                return Err(AdmitError::Overloaded);
            }
            let seq = s.next_seq;
            s.next_seq += 1;
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

    /// Snapshot for tests / observability: `(in_use, waiting, limit)`.
    pub fn stats(&self) -> (usize, usize, usize) {
        let s = self.inner.lock().unwrap();
        (s.in_use(), s.waiters.len(), s.limit)
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
}
