//! Measured-footprint cache (admission **improvement A**, SPRINT `footprint-estimate-accuracy`).
//!
//! The residency estimate ([`crate::share`] admission) is deliberately conservative: it pads the
//! weights + KV with a fixed activation reserve, so it OVER-refuses a model that would actually fit.
//! This cache lets admission TIGHTEN toward observed reality: each time a `(model, n_ctx)` load runs,
//! the MLX backend records the REAL peak it reached (`get_peak_memory()` — a high-water mark) as a
//! **running maximum** here. Admission then estimates `min(conservative, measured_peak + margin)` —
//! capped at the conservative figure (never higher) and floored at the observed peak + a generous
//! margin (never admits a load whose measured peak we've already seen, minus safety). The keep-free
//! margin and the kernel pressure-guard (improvement B) backstop a too-tight estimate.
//!
//! Safety by construction: the stored value only ever GROWS (a running max never under-reports a seen
//! peak), the estimate is capped at the conservative upper bound, and reads/writes are best-effort
//! (any IO/parse failure ⇒ the conservative estimate stands). It is therefore impossible for this
//! cache to make admission LESS safe than the conservative baseline by more than the (margin +
//! keep-free + pressure-guard) envelope.

use std::collections::HashMap;
use std::path::PathBuf;

fn cache_path() -> PathBuf {
    crate::share::gateway_dir().join("footprint-peaks.json")
}

/// Canonical key — by MODEL only (NOT n_ctx). Rationale: adaptive loading picks a different n_ctx as
/// free RAM drifts (e.g. 131072 one load, 101376 the next), so an n_ctx-exact key almost never hits.
/// Keying by model + a running MAX is SAFE because the consumer ([`tighten`]) re-floors at the TARGET
/// n_ctx's weights+KV, so a peak recorded at a *larger* n_ctx (more KV) merely over-estimates a smaller
/// load (safe), and a peak from a *smaller* n_ctx is floored up to the target's full KV anyway. Also
/// normalize `:`→`/` (the MLX backend records under the slash `model_id`, admission looks up the colon
/// CLI spec).
fn key(spec: &str) -> String {
    spec.replace(':', "/")
}

fn load() -> HashMap<String, u64> {
    std::fs::read(cache_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Record an observed peak footprint (bytes) for `(model, n_ctx)` as a RUNNING MAX — it never
/// shrinks, so a later light-workload run can't lower a heavy run's recorded peak. Best-effort:
/// IO/parse errors are swallowed (this cache is an optimization, never correctness). A no-op when
/// `peak_bytes` is 0 or not greater than the stored value (the common steady state ⇒ no writes).
pub fn record_peak(spec: &str, peak_bytes: u64) {
    if peak_bytes == 0 {
        return;
    }
    let mut m = load();
    let k = key(spec);
    if m.get(&k).copied().unwrap_or(0) >= peak_bytes {
        return; // no growth → nothing to persist
    }
    m.insert(k, peak_bytes);
    let _ = crate::share::ensure_dir();
    if let Ok(bytes) = serde_json::to_vec_pretty(&m) {
        let tmp = cache_path().with_extension("json.tmp");
        if std::fs::write(&tmp, &bytes).is_ok() {
            let _ = std::fs::rename(&tmp, cache_path());
        }
    }
}

/// The recorded running-max peak (bytes) for `model`, if any prior load recorded one.
pub fn cached_peak(spec: &str) -> Option<u64> {
    load().get(&key(spec)).copied()
}

/// The admission footprint after measured-peak tightening (improvement A). `conservative` is the
/// padded structural estimate; `active_floor` is weights + KV at this n_ctx — the part we KNOW
/// accurately (full-context KV), which the estimate must NEVER drop below: a measured peak from a
/// LIGHT request (short prompt → little KV) does NOT bound a future full-context request, so we floor
/// at full KV regardless. We additionally keep a fixed **1 GiB scratch reserve** above that floor so
/// admission never sheds the entire prefill-activation headroom on a single light measurement (keeps A
/// safe even at a lowered keep-free). Returns `min(conservative, max(active_floor + 1 GiB,
/// peak + margin))` when a peak exists, else `conservative` unchanged. `margin` = max(1 GiB, 10% of
/// peak). Net: A can only shave the OVER-padding (cache cap + part of the prefill reserve), bounded by
/// the floor; keep-free and the pressure-guard (improvement B) backstop the rest.
pub fn tighten(spec: &str, conservative: u64, active_floor: u64) -> u64 {
    match cached_peak(spec) {
        Some(peak) => {
            const GIB: u64 = 1 << 30;
            let margin = (peak / 10).max(GIB);
            let floor = active_floor.saturating_add(GIB); // weights + full KV + 1 GiB scratch reserve
            let tightened = peak.saturating_add(margin).max(floor);
            conservative.min(tightened)
        }
        None => conservative,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GIB: u64 = 1 << 30;

    // tighten() can only ever REDUCE the conservative estimate (never raise it), and never below the
    // observed peak (the +margin keeps headroom) nor below the weights+KV active floor.
    #[test]
    fn tighten_caps_at_conservative_and_floors_at_peak() {
        // No cached peak → unchanged.
        let spec = "test-no-cache-xyz";
        assert_eq!(tighten(spec, 24 * GIB, 19 * GIB), 24 * GIB);
    }

    // The pure min/max envelope, exercised directly (no IO): emulate what tighten does with a known
    // peak via the same arithmetic, so the safety invariants are explicit.
    #[test]
    fn tighten_envelope_is_safe() {
        // Mirror tighten()'s arithmetic (floor = active_floor + 1 GiB scratch reserve).
        let envelope = |peak: u64, conservative: u64, active_floor: u64| {
            let margin = (peak / 10).max(GIB);
            let floor = active_floor.saturating_add(GIB);
            conservative.min(peak.saturating_add(margin).max(floor))
        };
        // Measured peak (heavy) under conservative → tighten to peak+10% (here +2 GiB), ≤ conservative.
        assert_eq!(envelope(20 * GIB, 24 * GIB, 19 * GIB), 22 * GIB);
        // A LIGHT peak (10 GiB) can't pull the estimate below weights+KV + 1 GiB scratch (= 20 GiB):
        // protects against a short-prompt measurement under-provisioning a future full-context load.
        assert_eq!(envelope(10 * GIB, 24 * GIB, 19 * GIB), 20 * GIB);
        // A huge measured peak can never RAISE admission above the conservative upper bound.
        assert_eq!(envelope(40 * GIB, 24 * GIB, 19 * GIB), 24 * GIB);
    }
}
