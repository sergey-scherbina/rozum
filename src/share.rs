//! Shared-gateway rendezvous: discover and reuse one resident model across many
//! `rozum launch` clients, instead of each launch loading its own copy.
//!
//! Spec: `docs/specs/shared-gateway.md` (phase `shared-gateway-mvp`). The TCP
//! port bind is the singleton guarantee (one process binds it); this module is
//! the registry + health probe + reuse policy around it.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// Default stable port for the shared gateway. A respawn reuses the same port so
/// already-connected agents reconnect transparently.
pub const DEFAULT_GATEWAY_PORT: u16 = 8089;

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `$XDG_STATE_HOME/rozum/gateway/` (or `~/.local/state/...`).
pub fn gateway_dir() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local/state"));
    base.join("rozum").join("gateway")
}

pub fn active_path() -> PathBuf {
    gateway_dir().join("active.json")
}

pub fn ensure_dir() -> std::io::Result<()> {
    std::fs::create_dir_all(gateway_dir())
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The registry record published by a live shared gateway.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActiveGateway {
    pub model: String,
    pub port: u16,
    pub pid: u32,
    pub n_ctx: u32,
    pub started_at: u64,
    /// Increments on every (re)spawn and every in-place switch, so a proxy can
    /// tell "the daemon I was talking to was replaced" (model/backend swapped)
    /// from "same daemon, transient blip". Defaults to 0 for records written by
    /// an older daemon that predates the field.
    #[serde(default)]
    pub generation: u64,
}

/// Read the registry, or `None` if absent/unparseable.
pub fn read_active() -> Option<ActiveGateway> {
    let bytes = std::fs::read(active_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Publish the registry atomically (write-temp + rename).
pub fn write_active(g: &ActiveGateway) -> std::io::Result<()> {
    ensure_dir()?;
    let tmp = gateway_dir().join(format!("active.json.tmp.{}", g.pid));
    let body = serde_json::to_vec_pretty(g).unwrap_or_default();
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, active_path())
}

/// Remove the registry only if it still points at `pid` — so a process never
/// deletes a newer gateway's record on its way out.
pub fn remove_active_if_mine(pid: u32) {
    if let Some(g) = read_active() {
        if g.pid == pid {
            let _ = std::fs::remove_file(active_path());
        }
    }
}

/// Reuse policy (MVP): same model spec. An `n_ctx` difference is tolerated — the
/// running context window wins. Mismatch handling is the `launch-model-picker`
/// phase's job.
pub fn is_reusable(active: &ActiveGateway, want_model: &str) -> bool {
    active.model == want_model
}

// ── Client leases ───────────────────────────────────────────────────────────
// Each `rozum launch` writes leases/<pid> and heartbeats it (rewrites → bumps
// mtime). The daemon counts leases whose mtime is fresh as "a client is using
// me", and idle-exits only when none remain. A launch that exits without cleanup
// (process::exit) simply lets its lease go stale and be reaped.

/// A lease is "live" if heartbeated within this window.
pub const LEASE_FRESH_SECS: u64 = 60;

pub fn leases_dir() -> PathBuf {
    gateway_dir().join("leases")
}

pub fn lease_path(pid: u32) -> PathBuf {
    leases_dir().join(pid.to_string())
}

/// Create/refresh this client's lease (rewrite bumps mtime).
pub fn touch_lease(pid: u32) {
    if std::fs::create_dir_all(leases_dir()).is_ok() {
        let _ = std::fs::write(lease_path(pid), now_unix().to_string());
    }
}

pub fn remove_lease(pid: u32) {
    let _ = std::fs::remove_file(lease_path(pid));
}

/// Count leases heartbeated within `fresh_secs`, reaping clearly-dead ones
/// (mtime older than 10× the freshness window).
pub fn live_lease_count(fresh_secs: u64) -> usize {
    let mut live = 0;
    let Ok(entries) = std::fs::read_dir(leases_dir()) else {
        return 0;
    };
    for entry in entries.flatten() {
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|e| e.as_secs());
        match age {
            Some(secs) if secs <= fresh_secs => live += 1,
            Some(secs) if secs > fresh_secs * 10 => {
                let _ = std::fs::remove_file(entry.path()); // reap stale
            }
            _ => {}
        }
    }
    live
}

// ── Poison set (shared, TTL'd) ───────────────────────────────────────────────
// A request that crashes the daemon as the *sole* in-flight request is an
// unambiguous crasher: we record its fingerprint here so every proxy (and a
// freshly respawned daemon) can fast-refuse it before it kills the model again —
// protection that survives the very crash it guards against. Entries are
// advisory and short-TTL'd; a clean success decays them. Ambiguous crashes
// (concurrent in-flight) are never written here — they stay local to one proxy.

/// Default TTL for a shared poison entry (`ROZUM_POISON_TTL_SECS`).
pub const POISON_TTL_SECS: u64 = 3600;

pub fn poison_path() -> PathBuf {
    gateway_dir().join("poison.json")
}

/// Stable 64-bit fingerprint of a request body. We hash the **raw bytes the
/// proxy forwards verbatim to the daemon**, so both sides derive the same value
/// without re-implementing dialect normalization (the spec's "normalized
/// messages + sampling params" — raw-body equality is a robust superset: the
/// agent re-sending the same turn sends byte-identical JSON). `DefaultHasher`
/// (SipHash, fixed keys) is deterministic across processes of the same binary;
/// if it ever changes across an upgrade, stale entries simply expire.
pub fn fingerprint(body: &[u8]) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    body.hash(&mut h);
    h.finish()
}

fn read_poison_map() -> std::collections::HashMap<String, u64> {
    std::fs::read(poison_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_poison_map(map: &std::collections::HashMap<String, u64>) {
    let _ = ensure_dir();
    if map.is_empty() {
        let _ = std::fs::remove_file(poison_path());
        return;
    }
    let tmp = gateway_dir().join(format!("poison.json.tmp.{}", std::process::id()));
    if std::fs::write(&tmp, serde_json::to_vec(map).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, poison_path());
    }
}

/// Is `fp` a confirmed crasher whose entry is still within `ttl_secs`?
pub fn is_poisoned(fp: u64, ttl_secs: u64) -> bool {
    match read_poison_map().get(&fp.to_string()) {
        Some(&added) => now_unix().saturating_sub(added) < ttl_secs,
        None => false,
    }
}

/// Confirm `fp` machine-wide (sole-in-flight high confidence). Refreshes the
/// timestamp and prunes anything past `ttl_secs` while we hold the file.
pub fn record_poison(fp: u64, ttl_secs: u64) {
    let now = now_unix();
    let mut map = read_poison_map();
    map.retain(|_, &mut added| now.saturating_sub(added) < ttl_secs);
    map.insert(fp.to_string(), now);
    write_poison_map(&map);
}

/// Decay `fp` on a clean success — drop it from the shared set so a prompt that
/// now fits (memory freed) is no longer refused.
pub fn clear_poison(fp: u64) {
    let mut map = read_poison_map();
    if map.remove(&fp.to_string()).is_some() {
        write_poison_map(&map);
    }
}

/// Serializes the tests that mutate `XDG_STATE_HOME` (poison-set IO), so the
/// `unsafe` env writes never race a concurrent read on another harness thread.
/// Shared across modules (proxy tests lock it too).
#[cfg(test)]
pub(crate) static POISON_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn spawn_lock_path() -> PathBuf {
    gateway_dir().join("spawn.lock")
}

/// Best-effort anti-stampede lock for (re)spawning the daemon, so a crowd of
/// launches that all notice the daemon is down don't each spawn one. Correctness
/// does NOT depend on it — the TCP-port bind already guarantees a single daemon;
/// this just avoids wasted spawns. Held via O_EXCL create; a lock older than
/// `stale_secs` is treated as abandoned and stolen. The guard removes the file on
/// drop. `None` = someone else holds a fresh lock (let them spawn).
pub struct SpawnLock {
    _priv: (),
}

impl Drop for SpawnLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(spawn_lock_path());
    }
}

fn create_lock_excl() -> Option<SpawnLock> {
    use std::io::Write as _;
    let _ = ensure_dir();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(spawn_lock_path())
    {
        Ok(mut f) => {
            let _ = f.write_all(now_unix().to_string().as_bytes());
            Some(SpawnLock { _priv: () })
        }
        Err(_) => None,
    }
}

pub fn try_spawn_lock(stale_secs: u64) -> Option<SpawnLock> {
    if let Some(lock) = create_lock_excl() {
        return Some(lock);
    }
    // Exists — steal it only if it looks abandoned (older than stale_secs).
    let stale = std::fs::metadata(spawn_lock_path())
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map(|e| e.as_secs() > stale_secs)
        .unwrap_or(true);
    if stale {
        let _ = std::fs::remove_file(spawn_lock_path());
        create_lock_excl()
    } else {
        None
    }
}

/// Does a gateway answer on this port? The authoritative liveness signal (a stale
/// registry whose process is gone simply won't respond).
pub async fn health_ok(port: u16) -> bool {
    let url = format!("http://127.0.0.1:{port}/v1/models");
    let client = reqwest::Client::new();
    matches!(
        client
            .get(&url)
            .timeout(std::time::Duration::from_secs(1))
            .send()
            .await,
        Ok(r) if r.status().is_success()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ActiveGateway {
        ActiveGateway {
            model: "mlx-community/Qwen3-30B-A3B-Instruct-4bit".into(),
            port: 8089,
            pid: 4242,
            n_ctx: 32768,
            started_at: 1_700_000_000,
            generation: 1,
        }
    }

    #[test]
    fn registry_round_trips_through_json() {
        let g = sample();
        let bytes = serde_json::to_vec(&g).unwrap();
        let back: ActiveGateway = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(g, back);
    }

    #[test]
    fn reusable_only_on_matching_model() {
        let g = sample();
        assert!(is_reusable(&g, "mlx-community/Qwen3-30B-A3B-Instruct-4bit"));
        assert!(!is_reusable(&g, "mlx-community/Qwen3.6-35B-A3B-4bit"));
    }

    #[test]
    fn n_ctx_difference_does_not_block_reuse() {
        // Reuse keys on model only; the running n_ctx wins.
        let g = sample();
        assert!(is_reusable(&g, &g.model));
    }

    #[test]
    fn fingerprint_is_stable_and_distinguishes_bodies() {
        // Same bytes → same fingerprint (so proxy and daemon agree); different
        // bytes → (overwhelmingly) different.
        assert_eq!(
            fingerprint(b"{\"q\":\"hi\"}"),
            fingerprint(b"{\"q\":\"hi\"}")
        );
        assert_ne!(
            fingerprint(b"{\"q\":\"hi\"}"),
            fingerprint(b"{\"q\":\"bye\"}")
        );
    }

    #[test]
    fn poison_set_records_refuses_decays_and_expires() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        // Isolate the state dir so we don't touch a real ~/.local/state.
        let dir = std::env::temp_dir().join(format!("rozum-poison-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test; we own the env for its duration.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        let fp = 0xDEAD_BEEF_u64;
        assert!(!is_poisoned(fp, 3600), "unknown fp is not poisoned");

        record_poison(fp, 3600);
        assert!(is_poisoned(fp, 3600), "recorded fp is refused within TTL");
        assert!(
            !is_poisoned(fp, 0),
            "a zero TTL treats every entry as expired"
        );

        clear_poison(fp);
        assert!(!is_poisoned(fp, 3600), "cleared fp decays");

        let _ = std::fs::remove_dir_all(&dir);
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }
}
