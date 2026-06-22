//! Transient model health (`cascade-p2-health`). A model's availability fails and recovers —
//! a remote hits its quota / gets rate-limited / goes down / the network drops; a big local
//! OOMs — so it's tracked as live runtime state with exponential backoff + jitter and half-open
//! recovery, not a static property. The cascade skips a model in cooldown and routes to the best
//! *available* alternative. See `docs/specs/cascade-router.md`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rand::Rng;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    Healthy,
    /// Half-open: a cooldown elapsed; one probe is allowed.
    Degraded,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// A persisted health transition (JSONL row): a failure (`reason: Some`) with its wall-clock
/// cooldown deadline, or a recovery (`reason: None`). Replayed on start so cooldowns survive a
/// restart — a remote whose hourly quota is exhausted stays parked instead of being re-probed
/// immediately, and a model's `fails` count (hence its backoff level) carries forward.
#[derive(Serialize, Deserialize)]
struct HealthEvent {
    model: String,
    reason: Option<FailReason>,
    /// Wall-clock cooldown deadline (unix secs); `0` for a recovery.
    cooldown_until_unix: u64,
    fails: u32,
    ts: u64,
}

/// Per-model transient health, shared across concurrent requests on one `CascadeBackend`.
#[derive(Default)]
pub struct HealthRegistry {
    inner: Mutex<HashMap<String, Entry>>,
    /// Backing JSONL for cross-restart persistence; `None` = in-memory only.
    path: Option<PathBuf>,
}

impl HealthRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Open a registry backed by a JSONL log, replaying it: each model's **latest** event wins, and
    /// a failure whose cooldown is still in the future is restored as an active `Unavailable`
    /// cooldown (recoveries and already-elapsed cooldowns leave the model available).
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut latest: HashMap<String, HealthEvent> = HashMap::new();
        if path.exists() {
            if let Ok(f) = File::open(&path) {
                for line in BufReader::new(f).lines().map_while(Result::ok) {
                    let t = line.trim();
                    if t.is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_str::<HealthEvent>(t) {
                        let keep = latest.get(&ev.model).is_none_or(|p| ev.ts >= p.ts);
                        if keep {
                            latest.insert(ev.model.clone(), ev);
                        }
                    }
                }
            }
        }
        let now = crate::share::now_unix();
        let mut map: HashMap<String, Entry> = HashMap::new();
        for (model, ev) in latest {
            if let Some(reason) = ev.reason {
                if ev.cooldown_until_unix > now {
                    let remaining = Duration::from_secs(ev.cooldown_until_unix - now);
                    map.insert(
                        model,
                        Entry {
                            state: HealthState::Unavailable,
                            reason: Some(reason),
                            cooldown_until: Some(Instant::now() + remaining),
                            fails: ev.fails,
                        },
                    );
                }
            }
        }
        Self { inner: Mutex::new(map), path: Some(path) }
    }

    /// Append a health transition to the backing log (no-op when in-memory).
    fn persist(&self, ev: HealthEvent) {
        let Some(path) = &self.path else { return };
        if let Ok(line) = serde_json::to_string(&ev) {
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = writeln!(f, "{line}");
            }
        }
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
        let (total, fails) = {
            let mut m = self.inner.lock().unwrap();
            let e = m.entry(id.to_string()).or_default();
            e.fails = e.fails.saturating_add(1);
            e.reason = Some(reason);
            e.state = HealthState::Unavailable;
            let mult = 1u32 << e.fails.saturating_sub(1).min(5); // 2^0 .. 2^5
            let backed = reason.base_cooldown() * mult;
            let jitter_ms = rand::thread_rng().gen_range(0..=(backed.as_millis() / 2) as u64);
            let total = backed + Duration::from_millis(jitter_ms);
            e.cooldown_until = Some(Instant::now() + total);
            (total, e.fails)
        };
        self.persist(HealthEvent {
            model: id.to_string(),
            reason: Some(reason),
            cooldown_until_unix: crate::share::now_unix() + total.as_secs(),
            fails,
            ts: crate::share::now_unix(),
        });
    }

    /// Record a success: back to `Healthy`, counters reset.
    pub fn record_success(&self, id: &str) {
        self.inner.lock().unwrap().insert(id.to_string(), Entry::default());
        self.persist(HealthEvent {
            model: id.to_string(),
            reason: None,
            cooldown_until_unix: 0,
            fails: 0,
            ts: crate::share::now_unix(),
        });
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

    #[test]
    fn cooldown_survives_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.jsonl");
        {
            let h = HealthRegistry::open(&path);
            h.record_failure("remote", FailReason::QuotaExhausted); // ~1h cooldown
            assert!(!h.is_available("remote"));
        }
        // Restart: the still-active cooldown is restored from the log, not re-probed immediately.
        let h2 = HealthRegistry::open(&path);
        assert!(!h2.is_available("remote"), "a live cooldown survives a restart");
        assert_eq!(h2.state("remote"), HealthState::Unavailable);
    }

    #[test]
    fn recovered_model_is_available_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("health.jsonl");
        {
            let h = HealthRegistry::open(&path);
            h.record_failure("m", FailReason::Down);
            h.record_success("m"); // recovered — the latest event wins on replay
        }
        let h2 = HealthRegistry::open(&path);
        assert!(h2.is_available("m"), "a recovered model is available after restart");
    }
}
