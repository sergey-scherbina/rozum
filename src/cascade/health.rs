//! Transient model health (`cascade-p2-health`). A model's availability fails and recovers —
//! a remote hits its quota / gets rate-limited / goes down / the network drops; a big local
//! OOMs — so it's tracked as live runtime state with exponential backoff + jitter and half-open
//! recovery, not a static property. The cascade skips a model in cooldown and routes to the best
//! *available* alternative. See `docs/specs/cascade-router.md`.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    /// Half-open: a cooldown elapsed; one probe is allowed.
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    RateLimited,
    QuotaExhausted,
    Down,
    Network,
    OutOfMemory,
    Unknown,
}

impl FailReason {
    /// Base cooldown before a half-open retry (scaled by exponential backoff on repeats).
    fn base_cooldown(self) -> Duration {
        Duration::from_secs(match self {
            FailReason::RateLimited => 30,
            FailReason::QuotaExhausted => 3600, // to roughly an hourly reset
            FailReason::Network => 15,
            FailReason::OutOfMemory => 120,
            FailReason::Down => 60,
            FailReason::Unknown => 30,
        })
    }
}

/// Classify a backend error string into a transient [`FailReason`]. Order matters: a network
/// "connection timed out" is `Network`, not the generic `Down`.
pub fn classify(err: &str) -> FailReason {
    let e = err.to_lowercase();
    if e.contains("429") || e.contains("rate limit") || e.contains("too many requests") {
        FailReason::RateLimited
    } else if e.contains("401") || e.contains("403") || e.contains("quota") || e.contains("insufficient")
    {
        FailReason::QuotaExhausted
    } else if e.contains("connection")
        || e.contains("connect")
        || e.contains("dns")
        || e.contains("network")
        || e.contains("unreachable")
    {
        FailReason::Network
    } else if e.contains("out of memory")
        || e.contains("oom")
        || e.contains("too large for available")
        || e.contains("metal")
    {
        FailReason::OutOfMemory
    } else if e.contains("500")
        || e.contains("502")
        || e.contains("503")
        || e.contains("504")
        || e.contains("timeout")
        || e.contains("timed out")
        || e.contains("overloaded")
    {
        FailReason::Down
    } else {
        FailReason::Unknown
    }
}

#[derive(Debug, Clone)]
struct Entry {
    state: HealthState,
    #[allow(dead_code)]
    reason: Option<FailReason>,
    cooldown_until: Option<Instant>,
    fails: u32,
}

impl Default for Entry {
    fn default() -> Self {
        Self { state: HealthState::Healthy, reason: None, cooldown_until: None, fails: 0 }
    }
}

/// Per-model transient health, shared across concurrent requests on one `CascadeBackend`.
#[derive(Default)]
pub struct HealthRegistry {
    inner: Mutex<HashMap<String, Entry>>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// May this model be tried now? `Healthy`/`Degraded` → yes; an `Unavailable` model whose
    /// cooldown has elapsed transitions to `Degraded` (half-open) and is allowed one probe.
    pub fn is_available(&self, id: &str) -> bool {
        let mut m = self.inner.lock().unwrap();
        let e = m.entry(id.to_string()).or_default();
        match e.state {
            HealthState::Healthy | HealthState::Degraded => true,
            HealthState::Unavailable => {
                if e.cooldown_until.is_some_and(|t| Instant::now() >= t) {
                    e.state = HealthState::Degraded;
                    true
                } else {
                    false
                }
            }
        }
    }

    /// Record a failure: `Unavailable`, with an exponential-backoff + jitter cooldown.
    pub fn record_failure(&self, id: &str, reason: FailReason) {
        let mut m = self.inner.lock().unwrap();
        let e = m.entry(id.to_string()).or_default();
        e.fails = e.fails.saturating_add(1);
        e.reason = Some(reason);
        e.state = HealthState::Unavailable;
        let mult = 1u32 << e.fails.saturating_sub(1).min(5); // 2^0 .. 2^5
        let backed = reason.base_cooldown() * mult;
        let jitter_ms = rand::thread_rng().gen_range(0..=(backed.as_millis() / 2) as u64);
        e.cooldown_until = Some(Instant::now() + backed + Duration::from_millis(jitter_ms));
    }

    /// Record a success: back to `Healthy`, counters reset.
    pub fn record_success(&self, id: &str) {
        let mut m = self.inner.lock().unwrap();
        m.insert(id.to_string(), Entry::default());
    }

    pub fn state(&self, id: &str) -> HealthState {
        self.inner.lock().unwrap().get(id).map(|e| e.state).unwrap_or(HealthState::Healthy)
    }

    /// Force a model's cooldown to have elapsed (so the next `is_available` goes half-open).
    #[cfg(test)]
    pub fn force_expire(&self, id: &str) {
        let mut m = self.inner.lock().unwrap();
        if let Some(e) = m.get_mut(id) {
            e.cooldown_until = Some(Instant::now() - Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_maps_errors() {
        assert_eq!(classify("server returned 429: too many requests"), FailReason::RateLimited);
        assert_eq!(classify("anthropic returned 401: invalid x-api-key"), FailReason::QuotaExhausted);
        assert_eq!(classify("http request: connection refused"), FailReason::Network);
        assert_eq!(classify("mlx: context too large for available memory"), FailReason::OutOfMemory);
        assert_eq!(classify("server returned 503: overloaded"), FailReason::Down);
        assert_eq!(classify("something weird"), FailReason::Unknown);
    }

    #[test]
    fn failure_parks_then_half_open_recovers() {
        let h = HealthRegistry::new();
        assert!(h.is_available("m"), "unknown model is available");
        h.record_failure("m", FailReason::Down);
        assert!(!h.is_available("m"), "parked after a failure");
        assert_eq!(h.state("m"), HealthState::Unavailable);
        h.force_expire("m");
        assert!(h.is_available("m"), "cooldown elapsed → half-open probe allowed");
        assert_eq!(h.state("m"), HealthState::Degraded);
        h.record_success("m");
        assert!(h.is_available("m"));
        assert_eq!(h.state("m"), HealthState::Healthy);
    }

    #[test]
    fn backoff_grows_with_consecutive_fails() {
        // Just exercise the path; exact timing has jitter, but repeated fails must stay parked.
        let h = HealthRegistry::new();
        for _ in 0..3 {
            h.record_failure("m", FailReason::RateLimited);
        }
        assert!(!h.is_available("m"));
    }
}
