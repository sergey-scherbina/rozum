//! Residency lanes — the parallel scheduler (Phase 6).
//!
//! Concurrent requests through one `CascadeBackend` already run as independent futures; what they
//! must *not* do is contend for the same scarce local memory. A single GPU can't hold two big
//! models at once, so two requests routed to two mutually-exclusive locals have to serialize — but
//! a *simple* request routed to a small fast model must never be stuck behind a *complex* request
//! on the big one, and remote (HTTP) requests should parallelize freely.
//!
//! A **lane** is a residency group: models in the same [`Lane::Pool`] compete for one memory pool
//! and are serialized through a per-pool semaphore (single-resident = 1 slot; multi-resident later
//! = more slots, same code). [`Lane::Free`] models (remotes) are ungated. The scheduler sits
//! *above* per-backend admission (`concurrency::admit_wrap`): it owns lane assignment, and within a
//! lane delegates finer concurrency to the backend. See `docs/specs/cascade-router.md`.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::Location;

/// The residency lane a model competes in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    /// A named local memory pool. Models sharing a name can't co-reside, so the scheduler
    /// serializes them (single-resident) or admits up to the pool's slot count (multi-resident).
    Pool(String),
    /// Ungated — unbounded parallelism (remote HTTP, or a model that always co-resides).
    Free,
}

impl Lane {
    /// The safe default lane for a model: every local shares one `"local"` pool (one GPU, mutually
    /// exclusive), and every remote is `Free` (parallel HTTP). A caller with more than one local
    /// memory pool (multi-GPU / known co-residency) overrides per model.
    pub fn default_for(location: Location) -> Lane {
        match location {
            Location::Local => Lane::Pool("local".into()),
            Location::Remote => Lane::Free,
        }
    }
}

/// One semaphore per distinct local pool; acquiring a model's lane permit before its attempt
/// serializes co-residents while leaving other lanes (and all `Free` models) running in parallel.
pub struct LaneSet {
    pools: HashMap<String, Arc<Semaphore>>,
}

impl LaneSet {
    /// Build the lanes for `lanes`' distinct pools. Each pool gets `slots[name]` permits, defaulting
    /// to `1` (single-resident) when unspecified or `0`.
    pub fn new<'a>(lanes: impl IntoIterator<Item = &'a Lane>, slots: &HashMap<String, usize>) -> Self {
        let mut pools = HashMap::new();
        for lane in lanes {
            if let Lane::Pool(name) = lane {
                pools.entry(name.clone()).or_insert_with(|| {
                    let n = slots.get(name).copied().unwrap_or(1).max(1);
                    Arc::new(Semaphore::new(n))
                });
            }
        }
        Self { pools }
    }

    /// Acquire `lane`'s permit, parking until a slot frees. The permit is held by the caller for
    /// the attempt's lifetime and frees the slot on drop. `Free` (or an unknown pool) → `None`,
    /// i.e. ungated.
    pub async fn enter(&self, lane: &Lane) -> Option<OwnedSemaphorePermit> {
        match lane {
            Lane::Pool(name) => match self.pools.get(name) {
                // Never closed → acquire can't fail; on the impossible error, degrade to ungated.
                Some(sem) => Arc::clone(sem).acquire_owned().await.ok(),
                None => None,
            },
            Lane::Free => None,
        }
    }

    /// Currently-available permits in `pool` (tests / observability).
    #[cfg(test)]
    pub fn available(&self, pool: &str) -> usize {
        self.pools.get(pool).map(|s| s.available_permits()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn slots() -> HashMap<String, usize> {
        HashMap::new()
    }

    #[tokio::test]
    async fn distinct_pools_run_in_parallel() {
        let a = Lane::Pool("a".into());
        let b = Lane::Pool("b".into());
        let set = LaneSet::new([&a, &b], &slots());
        // Holding A does not block entering B.
        let _pa = set.enter(&a).await;
        let pb = tokio::time::timeout(Duration::from_millis(100), set.enter(&b)).await;
        assert!(pb.is_ok(), "a different pool must not be blocked by A");
    }

    #[tokio::test]
    async fn same_pool_serializes_then_frees_on_drop() {
        let a = Lane::Pool("a".into());
        let set = LaneSet::new([&a], &slots());
        let first = set.enter(&a).await; // takes the only slot
        assert_eq!(set.available("a"), 0);
        // A second entry into the same single-slot pool must park…
        let blocked = tokio::time::timeout(Duration::from_millis(50), set.enter(&a)).await;
        assert!(blocked.is_err(), "single-resident pool serializes a second entrant");
        // …until the first permit drops.
        drop(first);
        let second = tokio::time::timeout(Duration::from_millis(50), set.enter(&a)).await;
        assert!(second.is_ok(), "the freed slot admits the waiter");
    }

    #[tokio::test]
    async fn free_lane_is_ungated() {
        let set = LaneSet::new([&Lane::Free], &slots());
        assert!(set.enter(&Lane::Free).await.is_none(), "Free never gates");
        // Many concurrent Free entries all proceed.
        for _ in 0..100 {
            assert!(set.enter(&Lane::Free).await.is_none());
        }
    }

    #[tokio::test]
    async fn multi_resident_pool_admits_up_to_slot_count() {
        let a = Lane::Pool("a".into());
        let mut s = HashMap::new();
        s.insert("a".to_string(), 2usize); // two co-resident slots
        let set = LaneSet::new([&a], &s);
        let _p1 = set.enter(&a).await;
        let _p2 = tokio::time::timeout(Duration::from_millis(50), set.enter(&a)).await;
        assert!(_p2.is_ok(), "two slots admit two concurrent entrants");
        let p3 = tokio::time::timeout(Duration::from_millis(50), set.enter(&a)).await;
        assert!(p3.is_err(), "the third entrant parks until a slot frees");
    }

    #[test]
    fn default_lane_maps_location() {
        assert_eq!(Lane::default_for(Location::Local), Lane::Pool("local".into()));
        assert_eq!(Lane::default_for(Location::Remote), Lane::Free);
    }
}
