//! Adaptive, memory-gated model residency for the shared gateway (`shared-gateway-multislot`).
//!
//! Decide which models stay *loaded* so the daemon can serve more than one at once **without
//! thrashing** — when memory permits, keep the most **useful** (frequently + recently requested)
//! models co-resident, evicting the least useful to make room for a newly requested one; a model
//! too big to co-reside falls back to a swap (thrash is unavoidable there). Usefulness is learned
//! from request history ([`UsageStats`], persisted JSONL), so the warm set adapts to actual usage.
//!
//! This module is the **pure decision core** — it reasons over `(model id, weight bytes, busy,
//! utility)` only, no backends and no daemon state, so it's fully unit-testable. The live
//! `Switchboard` wiring (build/evict the actual resident backends) consumes [`plan_residency`].

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How fast a model's usefulness decays when it isn't requested: after this many seconds an unused
/// model's utility halves. Recent + frequent ranks high; frequent-but-stale decays out of the warm
/// set so it can be evicted for something live.
const UTILITY_HALF_LIFE_SECS: f64 = 3600.0; // 1 hour

/// Per-model usage rollup driving the keep-warm decision.
#[derive(Debug, Clone, Default)]
pub struct ModelUsage {
    /// Total times this model was requested.
    pub count: u64,
    /// Last request (unix secs) — drives the recency decay.
    pub last_used: u64,
    /// Resident weight in bytes (the latest seen), for the memory-fit math.
    pub weight_bytes: u64,
}

impl ModelUsage {
    /// Usefulness `≥ 0`: request frequency with an exponential recency decay (half-life
    /// [`UTILITY_HALF_LIFE_SECS`]). Higher = more worth keeping resident.
    pub fn utility(&self, now: u64) -> f64 {
        if self.count == 0 {
            return 0.0;
        }
        let age = now.saturating_sub(self.last_used) as f64;
        let recency = 0.5_f64.powf(age / UTILITY_HALF_LIFE_SECS);
        self.count as f64 * recency
    }
}

/// One persisted request event (JSONL row).
#[derive(Serialize, Deserialize)]
struct UsageEvent {
    model: String,
    weight_bytes: u64,
    ts: u64,
}

/// Append-only, replay-on-open request history → per-model [`ModelUsage`]. The warm set's
/// "statistically useful" signal that survives a daemon restart.
pub struct UsageStats {
    inner: Mutex<HashMap<String, ModelUsage>>,
    path: Option<PathBuf>,
}

impl UsageStats {
    /// Open backed by a JSONL log, replaying it into the per-model rollups.
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut map: HashMap<String, ModelUsage> = HashMap::new();
        if path.exists() {
            if let Ok(f) = File::open(&path) {
                for line in BufReader::new(f).lines().map_while(Result::ok) {
                    let t = line.trim();
                    if t.is_empty() {
                        continue;
                    }
                    if let Ok(ev) = serde_json::from_str::<UsageEvent>(t) {
                        fold(&mut map, &ev);
                    }
                }
            }
        }
        Self { inner: Mutex::new(map), path: Some(path) }
    }

    /// An ephemeral store (no file) for tests / scratch.
    pub fn in_memory() -> Self {
        Self { inner: Mutex::new(HashMap::new()), path: None }
    }

    /// Record a request for `model` (its resident weight), at `now` (unix secs).
    pub fn record(&self, model: &str, weight_bytes: u64, now: u64) {
        let ev = UsageEvent { model: model.to_string(), weight_bytes, ts: now };
        if let Some(path) = &self.path {
            if let Ok(line) = serde_json::to_string(&ev) {
                if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "{line}");
                }
            }
        }
        fold(&mut self.inner.lock().unwrap(), &ev);
    }

    /// `model`'s current usefulness at `now` (`0.0` if never seen).
    pub fn utility(&self, model: &str, now: u64) -> f64 {
        self.inner.lock().unwrap().get(model).map(|u| u.utility(now)).unwrap_or(0.0)
    }

    /// `model`'s last-seen resident weight, if recorded.
    pub fn weight(&self, model: &str) -> Option<u64> {
        self.inner.lock().unwrap().get(model).map(|u| u.weight_bytes).filter(|&w| w > 0)
    }
}

fn fold(map: &mut HashMap<String, ModelUsage>, ev: &UsageEvent) {
    let u = map.entry(ev.model.clone()).or_default();
    u.count += 1;
    u.last_used = u.last_used.max(ev.ts);
    if ev.weight_bytes > 0 {
        u.weight_bytes = ev.weight_bytes;
    }
}

/// A currently-resident model (input to the planner).
#[derive(Debug, Clone)]
pub struct ResidentInfo {
    pub model: String,
    pub weight_bytes: u64,
    /// Has in-flight work — must not be evicted (can't drop a model mid-stream).
    pub busy: bool,
}

/// What the planner wants done for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResidentPlan {
    /// The requested model must be loaded (it wasn't already resident).
    pub load: bool,
    /// Idle residents to evict to make room, lowest-utility first.
    pub evict: Vec<String>,
    /// `true` when even after evicting every idle resident the request doesn't fit alongside the
    /// busy ones — the unavoidable case (a big model, or all slots busy): the caller must swap /
    /// over-subscribe rather than co-reside.
    pub oversubscribed: bool,
}

/// Inputs to [`plan_residency`].
pub struct ResidentRequest<'a> {
    /// The model this request needs (it must end up resident).
    pub requested: &'a str,
    /// Its resident weight in bytes.
    pub requested_weight: u64,
    /// The currently-resident models.
    pub residents: &'a [ResidentInfo],
    /// Usable memory budget for resident model weights (bytes) — the caller derives it from free
    /// RAM × a safety fraction.
    pub budget_bytes: u64,
}

/// Decide the resident set for a request: **keep the highest-utility models that fit, always
/// including the requested one; evict the rest (idle only)**. Greedy by descending utility — the
/// adaptive memory-gated policy. Busy residents are never evicted; if the requested model can't fit
/// alongside them, `oversubscribed` is set (the caller swaps / queues — the inevitable thrash for a
/// big model).
pub fn plan_residency(req: &ResidentRequest, utility: impl Fn(&str) -> f64) -> ResidentPlan {
    // Already resident → just serve it; don't disturb the warm set.
    if req.residents.iter().any(|r| r.model == req.requested) {
        return ResidentPlan { load: false, evict: Vec::new(), oversubscribed: false };
    }

    // Mandatory residents we can't drop: busy ones + the requested model.
    let busy_bytes: u64 = req.residents.iter().filter(|r| r.busy).map(|r| r.weight_bytes).sum();
    let mandatory = busy_bytes.saturating_add(req.requested_weight);

    // Idle residents are the candidates to keep (high utility) or evict (to free room), ranked.
    let mut idle: Vec<&ResidentInfo> = req.residents.iter().filter(|r| !r.busy).collect();
    idle.sort_by(|a, b| {
        utility(&b.model)
            .partial_cmp(&utility(&a.model))
            .unwrap_or(std::cmp::Ordering::Equal)
            // Tie-break on a stable key so the plan is deterministic.
            .then_with(|| a.model.cmp(&b.model))
    });

    if mandatory > req.budget_bytes {
        // Even with no idle co-residents, the request (+ busy) overflows → evict all idle and let
        // the caller over-subscribe / swap. This is the big-model thrash case.
        return ResidentPlan {
            load: true,
            evict: idle.iter().map(|r| r.model.clone()).collect(),
            oversubscribed: true,
        };
    }

    // Fill the remaining budget with idle residents by descending utility; evict those that don't
    // fit. The just-requested model and all busy ones are already counted in `mandatory`.
    let mut used = mandatory;
    let mut evict = Vec::new();
    for r in idle {
        if used.saturating_add(r.weight_bytes) <= req.budget_bytes {
            used += r.weight_bytes; // keep this useful idle resident
        } else {
            evict.push(r.model.clone());
        }
    }
    ResidentPlan { load: true, evict, oversubscribed: false }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GB: u64 = 1024 * 1024 * 1024;

    fn resident(model: &str, gb: u64, busy: bool) -> ResidentInfo {
        ResidentInfo { model: model.into(), weight_bytes: gb * GB, busy }
    }

    // ── utility / usage stats ───────────────────────────────────────────────

    #[test]
    fn utility_rewards_frequency_and_recency() {
        let now = 100_000;
        let frequent_recent = ModelUsage { count: 10, last_used: now, weight_bytes: GB };
        let frequent_stale =
            ModelUsage { count: 10, last_used: now - 2 * 3600, weight_bytes: GB }; // 2 half-lives
        let rare_recent = ModelUsage { count: 1, last_used: now, weight_bytes: GB };
        assert!(frequent_recent.utility(now) > frequent_stale.utility(now));
        assert!(frequent_recent.utility(now) > rare_recent.utility(now));
        // 2 half-lives → ~1/4 of the fresh score.
        assert!((frequent_stale.utility(now) - 10.0 * 0.25).abs() < 1e-6);
        assert_eq!(ModelUsage::default().utility(now), 0.0);
    }

    #[test]
    fn usage_stats_record_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.jsonl");
        {
            let s = UsageStats::open(&path);
            s.record("a", 2 * GB, 1000);
            s.record("a", 2 * GB, 1100);
            s.record("b", GB, 1050);
        }
        // Reopen → counts/weights replay.
        let s = UsageStats::open(&path);
        assert!(s.utility("a", 1100) > s.utility("b", 1100), "a is twice as frequent");
        assert_eq!(s.weight("a"), Some(2 * GB));
        assert_eq!(s.weight("missing"), None);
    }

    // ── the planner ─────────────────────────────────────────────────────────

    fn flat_utility(_: &str) -> f64 {
        1.0
    }

    #[test]
    fn small_models_co_reside_without_thrash() {
        // A(2) resident; request B(2); 8 GB budget → both fit, nothing evicted.
        let residents = [resident("A", 2, false)];
        let req = ResidentRequest {
            requested: "B",
            requested_weight: 2 * GB,
            residents: &residents,
            budget_bytes: 8 * GB,
        };
        let plan = plan_residency(&req, flat_utility);
        assert!(plan.load);
        assert!(plan.evict.is_empty(), "B co-resides with A");
        assert!(!plan.oversubscribed);
    }

    #[test]
    fn evicts_the_least_useful_to_make_room() {
        // A(3, util 1) + C(3, util 10) resident; request B(3); budget 7 → only one of A/C fits
        // alongside B. Keep the more useful (C), evict A.
        let residents = [resident("A", 3, false), resident("C", 3, false)];
        let util = |m: &str| if m == "C" { 10.0 } else { 1.0 };
        let req = ResidentRequest {
            requested: "B",
            requested_weight: 3 * GB,
            residents: &residents,
            budget_bytes: 7 * GB,
        };
        let plan = plan_residency(&req, util);
        assert!(plan.load);
        assert_eq!(plan.evict, vec!["A".to_string()], "the less useful idle model is evicted");
        assert!(!plan.oversubscribed);
    }

    #[test]
    fn big_model_thrashes_unavoidably() {
        // Two small idle residents; a 20 GB model is requested into a 16 GB budget → evict both,
        // load it, oversubscribed (the caller swaps).
        let residents = [resident("A", 2, false), resident("C", 2, false)];
        let req = ResidentRequest {
            requested: "BIG",
            requested_weight: 20 * GB,
            residents: &residents,
            budget_bytes: 16 * GB,
        };
        let plan = plan_residency(&req, flat_utility);
        assert!(plan.load);
        assert!(plan.oversubscribed, "a too-big model can't co-reside → thrash");
        assert_eq!(plan.evict.len(), 2, "all idle residents freed");
    }

    #[test]
    fn busy_residents_are_never_evicted() {
        // A(4) is busy; request B(4) into a 7 GB budget → can't evict A; B + A overflow → oversub.
        let residents = [resident("A", 4, true)];
        let req = ResidentRequest {
            requested: "B",
            requested_weight: 4 * GB,
            residents: &residents,
            budget_bytes: 7 * GB,
        };
        let plan = plan_residency(&req, flat_utility);
        assert!(plan.load);
        assert!(plan.evict.is_empty(), "a busy resident is never evicted");
        assert!(plan.oversubscribed);
    }

    #[test]
    fn already_resident_just_serves() {
        let residents = [resident("A", 2, false)];
        let req = ResidentRequest {
            requested: "A",
            requested_weight: 2 * GB,
            residents: &residents,
            budget_bytes: 8 * GB,
        };
        let plan = plan_residency(&req, flat_utility);
        assert!(!plan.load, "already resident → no load");
        assert!(plan.evict.is_empty());
        assert!(!plan.oversubscribed);
    }
}
