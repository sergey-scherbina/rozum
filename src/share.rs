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
}
