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
/// Shared across modules and **across crates** (the binary's proxy tests lock it
/// too) — so it is `pub` and not `#[cfg(test)]`: a `#[cfg(test)]` item is invisible
/// when `rozum-core` is built as a dependency, and the cross-crate test callers need
/// it. The cost is one tiny always-present static.
pub static POISON_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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

// ── Host-wide model-residency admission gate (BUG-003) ────────────────────────
// A whole-system OOM — vm-compressor-space-shortage → jetsam cascade → watchdogd
// starved → kernel watchdog panic → REBOOT — happens when the model-loaded gateways
// resident at once on a memory-bounded box exceed RAM (two concurrent matrix runs
// were ~18-25 GB each ⇒ ~61.6 GB on a 36 GiB Mac). The shared-gateway port singleton
// (`DEFAULT_GATEWAY_PORT`) only governs gateways that go through the rendezvous; a
// dedicated `rozum gateway --port N` (what the matrix bench starts) bypasses it, so
// the registry never sees the second resident model.
//
// v2 — a RAM ledger, not a hard mutex. Every model-loaded gateway *reserves* its
// estimated footprint BEFORE bringing weights resident; a new load is admitted only
// if it is the sole model OR the reserved total still fits a host RAM budget — so a
// genuinely-small 2nd model can co-reside, while the case that reboots (two big
// models) is refused. The ledger is host-wide and independent of port/run/worktree.
//
// Mechanism (all advisory `flock`, released by the OS on fd close incl. SIGKILL — no
// stale-lock cleanup to get wrong):
//   • Each resident gateway holds an exclusive `flock` on `residents/<pid>` for its
//     process lifetime; the file's content is its `{model, footprint}` reservation.
//     Liveness needs no heartbeat — a reader `try_lock`s the file: success ⇒ the
//     holder died ⇒ reap; would-block ⇒ alive ⇒ count its footprint.
//   • Admission is serialized by a *briefly*-held `flock` on `residency.lock`, so the
//     scan→decide→reserve is atomic across processes (no admit TOCTOU).
// A reservation up front (not a post-hoc free-RAM read) is what makes it correct: two
// gateways racing to load both reserve under the admit lock, so the second sees the
// first's footprint even though neither has finished loading.

/// Brief admission mutex (NOT held for the model's lifetime in v2 — only around the
/// scan→decide→reserve critical section). A leftover v1 gateway that holds this for
/// life simply makes a v2 arrival wait, which is safe (conservative).
pub fn residency_lock_path() -> PathBuf {
    gateway_dir().join("residency.lock")
}

/// Directory of per-pid reservation files (the RAM ledger).
pub fn residents_dir() -> PathBuf {
    gateway_dir().join("residents")
}

fn resident_path(pid: u32) -> PathBuf {
    residents_dir().join(pid.to_string())
}

/// Seconds an arriving gateway waits for resident models to free enough budget
/// before refusing. `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS`, default 240 — generously
/// past the matrix teardown window (`TEARDOWN_GRACE` 180s + `GPU_SETTLE`), so a
/// back-to-back bench model swap (old gateway exiting as the new one starts) never
/// falsely refuses. `0` = refuse immediately.
pub fn residency_wait_secs() -> u64 {
    std::env::var("ROZUM_GATEWAY_RESIDENCY_WAIT_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(240)
}

/// Operator escape hatch: skip the gate entirely (admit unconditionally, reserve
/// nothing). `ROZUM_ALLOW_CONCURRENT_RESIDENT=1`.
pub fn concurrent_resident_allowed() -> bool {
    std::env::var("ROZUM_ALLOW_CONCURRENT_RESIDENT")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Host RAM budget for the SUM of all resident model gateways' reserved footprints.
/// Absolute override `ROZUM_GATEWAY_RAM_BUDGET_BYTES`; else `total_ram *
/// ROZUM_GATEWAY_RAM_BUDGET_FRAC`. `None` ⇒ budget unknown ⇒ only a sole model is
/// admitted (any 2nd refused), the safe fallback.
///
/// **Default 0.75** (raised from 0.65, smmr-D-justified 2026-06-22). This bounds the SUM
/// of *reserved footprints*, and footprints are conservative over-estimates of real peak
/// (smmr-D: a 4B reserves ~13 GiB but peaks ~6 GiB at 14k ctx / ~10 GiB at full 32k; the
/// cache is hard-bounded by `set_cache_limit`). So real usage stays well under the budget:
/// at 0.75 on 36 GiB the reserved cap is 27 GiB but real co-resident usage is ~20 GiB →
/// ~16 GiB genuinely free (the reboot needed ~0 free / 1.7× overcommit). 0.75 lets two
/// small models actually co-reside (the operator's goal) while keeping a large real
/// margin. Raise further (≤0.95) only with measured footprints; lower for a busier host.
pub fn host_ram_budget_bytes() -> Option<u64> {
    if let Some(abs) = std::env::var("ROZUM_GATEWAY_RAM_BUDGET_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Some(abs);
    }
    let frac = std::env::var("ROZUM_GATEWAY_RAM_BUDGET_FRAC")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f > 0.0)
        .unwrap_or(0.75)
        .clamp(0.1, 0.95);
    crate::concurrency::total_ram_bytes().map(|t| (t as f64 * frac) as u64)
}

/// RAM (bytes) to keep **actually free** after a model loads — the headroom the actual-free-RAM
/// admission lever ([`admits`]) preserves on top of the model's own footprint, for the OS and
/// non-model spikes. Default **2 GiB**; override `ROZUM_GATEWAY_MIN_FREE_RAM_BYTES`. This is what
/// keeps a load from driving the host toward the ~0-free state that triggered the jetsam/watchdog
/// reboot ([[project-reboot-watchdog-oom]]).
///
/// Lowered 3→2 GiB (2026-06-23) now that keep-free is no longer the *leading* prefill-spike buffer:
/// improvement A folds the model's REAL measured peak (incl. the prefill spike) INTO the footprint
/// estimate, and improvement B refuses at admission under kernel memory-pressure — so keep-free is now
/// just the single-load external-growth cushion. The host-wide *ledger* (not this margin) is what
/// blocks concurrent overcommit (the reboot was a 25 GiB overcommit no keep-free size would have
/// gated). Validated: gpt-oss loaded at 2 GiB + a heavy prefill request held kernel pressure at Normal.
pub fn min_free_ram_bytes() -> u64 {
    const GIB: u64 = 1 << 30;
    std::env::var("ROZUM_GATEWAY_MIN_FREE_RAM_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2 * GIB)
}

/// RAM available for the admission decision: the absolute override `ROZUM_GATEWAY_AVAILABLE_RAM_BYTES`
/// if set (lets an operator pin a conservative figure on a shared host, and lets tests isolate the
/// ledger lever), else the live measurement [`crate::concurrency::available_ram_bytes`]. `None` ⇒
/// can't measure ⇒ the free-RAM lever doesn't gate (see [`admits`]).
pub fn available_ram_for_admission() -> Option<u64> {
    if let Some(v) = std::env::var("ROZUM_GATEWAY_AVAILABLE_RAM_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        return Some(v);
    }
    crate::concurrency::available_ram_bytes()
}

/// The admission decision, pure + unit-testable. Loading `footprint` is admitted iff BOTH levers pass:
///
/// 1. **Ledger (cross-process reservations):** the model is the SOLE resident (`in_use == 0`) or the
///    reserved total (others + this) fits the reserved-footprint `budget`. This coordinates *our own*
///    gateways with each other.
/// 2. **Actual free RAM (the truth lever):** `footprint + min_free` fits in the RAM `available` right
///    now. This is independent of the ledger, so it ALSO catches what the ledger can't see — heavy
///    non-model RAM (browsers/builds), gateways not in the ledger (a stale/other-worktree binary), and
///    even a sole model that would overcommit on its own. `available` already excludes any resident
///    model's RAM, so for a co-resident it correctly asks "does this fit in what's left".
///
/// A `None` for either input means that lever can't measure ⇒ it doesn't block (fail-open on the
/// unknown lever; the other still gates). Refusing here turns "B loads → host overcommits → OS jetsam
/// kills a model mid-work / reboot" into "B is refused or waits" — the no-reboot invariant.
fn admits(
    in_use: u64,
    footprint: u64,
    budget: Option<u64>,
    available: Option<u64>,
    min_free: u64,
    pressure: crate::shed::PressureLevel,
) -> bool {
    let ledger_fits =
        in_use == 0 || budget.is_some_and(|b| in_use.saturating_add(footprint) <= b);
    let ram_fits = available.map_or(true, |a| footprint.saturating_add(min_free) <= a);
    // 3. **Kernel memory-pressure (the OS's own jetsam signal):** the page arithmetic above can read
    //    "fits" moments before pressure spikes; the kernel computes availability far better than we do
    //    (same signal the [`crate::shed`] runtime watchdog keys on). If the host is ALREADY at warn /
    //    critical, a big new model is exactly what tips it into the jetsam→reboot cascade — refuse,
    //    independent of the byte math. Fail-safe: an unreadable level reports `Normal` ⇒ doesn't block.
    let pressure_ok = matches!(pressure, crate::shed::PressureLevel::Normal);
    ledger_fits && ram_fits && pressure_ok
}

/// A non-mutating snapshot of the admission decision for `footprint_bytes` at the CURRENT host
/// state — the SAME inputs [`acquire_residency`] reads (ledger reservations, host budget, live free
/// RAM, keep-free), run through the SAME [`admits`] math, but WITHOUT taking the admit lock or
/// reserving anything. Powers `gateway --dry-run`: report whether a load would be admitted, and why.
#[derive(Clone, Debug)]
pub struct AdmissionReport {
    pub footprint: u64,
    pub in_use: u64,
    pub holders: Vec<(u32, String)>,
    pub budget: Option<u64>,
    pub available: Option<u64>,
    pub min_free: u64,
    pub ledger_fits: bool,
    pub ram_fits: bool,
    pub pressure: crate::shed::PressureLevel,
    pub pressure_ok: bool,
    pub admit: bool,
}

/// The host's current kernel memory-pressure level as a label (`normal`/`warn`/`critical`) — the
/// jetsam ladder the admission gate now also keys on. For `--dry-run` display.
pub fn host_pressure_label() -> &'static str {
    crate::shed::read_host_pressure().as_str()
}

pub fn dry_run_admission(footprint_bytes: u64) -> AdmissionReport {
    let available = available_ram_for_admission();
    let min_free = min_free_ram_bytes();
    let pressure = crate::shed::read_host_pressure();
    if concurrent_resident_allowed() {
        // Operator override (ROZUM_ALLOW_CONCURRENT_RESIDENT=1): no gating — always admits.
        return AdmissionReport {
            footprint: footprint_bytes, in_use: 0, holders: Vec::new(), budget: None,
            available, min_free, ledger_fits: true, ram_fits: true,
            pressure, pressure_ok: true, admit: true,
        };
    }
    let mypid = std::process::id();
    let (in_use, holders) = scan_residents(mypid);
    let budget = host_ram_budget_bytes();
    let ledger_fits =
        in_use == 0 || budget.is_some_and(|b| in_use.saturating_add(footprint_bytes) <= b);
    let ram_fits = available.map_or(true, |a| footprint_bytes.saturating_add(min_free) <= a);
    let pressure_ok = matches!(pressure, crate::shed::PressureLevel::Normal);
    AdmissionReport {
        footprint: footprint_bytes, in_use, holders, budget, available, min_free,
        ledger_fits, ram_fits, pressure, pressure_ok, admit: ledger_fits && ram_fits && pressure_ok,
    }
}

/// One resident gateway's reservation (the content of `residents/<pid>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
struct ResidentEntry {
    model: String,
    footprint_bytes: u64,
}

/// Held for the lifetime of a resident model: the exclusive `flock` on this
/// gateway's `residents/<pid>` file. Dropping it — or the process dying — releases
/// the reservation (the OS drops the `flock`; `Drop` also unlinks the file).
pub struct ResidencyGuard {
    _lock: std::fs::File,
    path: PathBuf,
}

impl ResidencyGuard {
    /// Update this process's published reservation footprint IN PLACE, keeping the held
    /// `flock` (the reservation stays valid throughout). For the in-process Switchboard
    /// (residency-unify U1): a gateway holding several models should publish its **TOTAL**
    /// footprint (primary + warm) so other gateways' [`committed_by_others_bytes`] account
    /// for the warm set too — not just the primary reserved at load time.
    ///
    /// Write-then-truncate (not truncate-first) so a concurrent reader during a **grow**
    /// (a warm model just loaded → more memory) sees either the complete new larger entry
    /// or the old one — **never an under-count in the memory-increasing direction** (the
    /// safety-critical one). A **shrink** (warm evicted) may transiently read as `0` —
    /// benign, since memory is being *freed* and the `shed` governor backstops any race.
    /// Best-effort: IO errors are swallowed (the gate is a safety net, not correctness).
    pub fn update_footprint(&self, model: &str, footprint_bytes: u64) {
        use std::io::{Seek, SeekFrom, Write};
        let body = serde_json::to_vec(&ResidentEntry {
            model: model.to_string(),
            footprint_bytes,
        })
        .unwrap_or_default();
        let mut f: &std::fs::File = &self._lock;
        let _ = f.seek(SeekFrom::Start(0));
        let _ = f.write_all(&body); // overwrites from 0; a grow extends, never shortens mid-read
        let _ = self._lock.set_len(body.len() as u64); // drop any old tail (shrink case)
        let _ = f.flush();
    }
}

impl Drop for ResidencyGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        // `_lock` (the File) closes after this, releasing the flock.
    }
}

/// The gate refused: admitting this load would overcommit host RAM, and no resident
/// freed enough within the wait window. Carries the numbers + live holders so the
/// caller can explain exactly why.
#[derive(Debug)]
pub struct ResidencyDenied {
    pub footprint_bytes: u64,
    pub in_use_bytes: u64,
    pub budget_bytes: Option<u64>,
    pub waited_secs: u64,
    /// `(pid, model)` of the gateways currently holding reservations.
    pub holders: Vec<(u32, String)>,
}

/// Sum of resident-model footprints reserved by OTHER live gateway processes (skips
/// `skip_pid`; dead holders are reaped). For the in-process Switchboard's host-aware warm
/// admission (residency-unify U1): a process's budget for its OWN residents = the host
/// budget minus this. Best-effort (0 if the ledger is empty/unreadable). NOT under the
/// admit lock — a momentary read for sizing, not the admission decision itself.
pub fn committed_by_others_bytes(skip_pid: u32) -> u64 {
    scan_residents(skip_pid).0
}

/// The live resident model gateways on this host — `(pid, model)` per held reservation. A read for
/// status/control surfaces (the control-API dashboard); reaps any dead reservation it probes, like
/// [`committed_by_others_bytes`]. Empty if the ledger is empty/unreadable.
pub fn list_residents() -> Vec<(u32, String)> {
    scan_residents(0).1
}

/// Update THIS process's published reservation footprint, if a reservation file exists
/// (i.e. a [`ResidencyGuard`] is held). The convenient wiring API for the in-process
/// Switchboard (residency-unify): call on each warm load/evict to republish the process's
/// **total** footprint (primary + Σ warm) so other gateways' [`committed_by_others_bytes`]
/// account for the warm set, not just the primary reserved at load. Opens the EXISTING
/// `residents/<pid>` (never creates — a stray file with no flock holder would just be
/// reaped); write-then-truncate so a concurrent reader during a grow never under-counts in
/// the memory-increasing direction (see [`ResidencyGuard::update_footprint`]). Best-effort;
/// a no-op when no reservation is held (gate bypassed / not yet reserved).
pub fn update_my_reservation(model: &str, footprint_bytes: u64) {
    use std::io::{Seek, SeekFrom, Write};
    let Ok(mut f) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(resident_path(std::process::id()))
    else {
        return;
    };
    let body = serde_json::to_vec(&ResidentEntry {
        model: model.to_string(),
        footprint_bytes,
    })
    .unwrap_or_default();
    let _ = f.seek(SeekFrom::Start(0));
    let _ = f.write_all(&body);
    let _ = f.set_len(body.len() as u64);
    let _ = f.flush();
}

/// Scan the ledger under the admit lock: reap dead reservations (their `flock` is
/// free) and sum the live ones (skipping our own pid). Returns `(sum_bytes, holders)`.
fn scan_residents(skip_pid: u32) -> (u64, Vec<(u32, String)>) {
    let mut sum = 0u64;
    let mut holders = Vec::new();
    let Ok(entries) = std::fs::read_dir(residents_dir()) else {
        return (0, holders);
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let pid: u32 = match path.file_name().and_then(|n| n.to_str()).and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        if pid == skip_pid {
            continue;
        }
        // Liveness: a successful try_lock means nobody holds it ⇒ the owner died.
        match std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            Ok(f) => match f.try_lock() {
                Ok(()) => {
                    let _ = std::fs::remove_file(&path); // reap dead reservation
                    continue;
                }
                Err(std::fs::TryLockError::WouldBlock) => {} // alive — fall through to count
                // Can't probe (FS without locks): count it, conservatively (alive).
                Err(std::fs::TryLockError::Error(_)) => {}
            },
            Err(_) => continue,
        }
        let ent: ResidentEntry = match std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
            Some(e) => e,
            None => ResidentEntry { model: String::new(), footprint_bytes: 0 },
        };
        sum = sum.saturating_add(ent.footprint_bytes);
        holders.push((pid, ent.model));
    }
    (sum, holders)
}

/// Reap reservation files whose owning gateway has died — their `flock` is free, so a
/// `try_lock` succeeds. [`acquire_residency`] already reaps lazily on each admission;
/// this standalone pass lets a long-lived host (or a `doctor`/maintenance path) clean
/// up orphaned `residents/<pid>` files left by a `SIGKILL`'d gateway *between* loads,
/// instead of waiting for the next admission. Conservative: a file it cannot lock-probe
/// is left in place (treated as possibly-live). Returns the number reaped.
pub fn reap_orphan_residents() -> usize {
    let mut reaped = 0usize;
    let Ok(entries) = std::fs::read_dir(residents_dir()) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Only numeric pid files are reservations.
        let is_pid = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|s| s.parse::<u32>().ok())
            .is_some();
        if !is_pid {
            continue;
        }
        if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(&path) {
            // try_lock success ⇒ nobody holds the flock ⇒ the owner died ⇒ reap.
            if matches!(f.try_lock(), Ok(())) && std::fs::remove_file(&path).is_ok() {
                reaped += 1;
            }
        }
    }
    reaped
}

/// Acquire a host-wide RAM reservation for a model about to load (BUG-003 v2).
///
/// `footprint_bytes` is the caller's estimate of this model's resident size (weights
/// + KV + overhead); the caller computes it from the model catalog (rozum-core stays
/// engine/model-free). A model is admitted iff it is the **sole** resident OR the
/// reserved total (incl. it) fits [`host_ram_budget_bytes`].
///
/// - `Ok(Some(guard))` — reserved; hold `guard` for as long as the model is resident.
/// - `Ok(None)` — gate bypassed (escape hatch) or unusable (lockfile/FS IO error):
///   fail **open** — a load must never be blocked by a lockfile problem, the gate is a
///   safety net, not a correctness requirement.
/// - `Err(ResidencyDenied)` — admitting would overcommit and nothing freed in time.
///
/// Blocking (polls every 2s while waiting). Call via `spawn_blocking` from async.
pub fn acquire_residency(
    model: &str,
    footprint_bytes: u64,
) -> Result<Option<ResidencyGuard>, ResidencyDenied> {
    if concurrent_resident_allowed() {
        return Ok(None);
    }
    let _ = ensure_dir();
    let _ = std::fs::create_dir_all(residents_dir());
    let mypid = std::process::id();
    let prio = residency_prio();
    let wait_secs = residency_wait_secs();
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(wait_secs);
    // Event-driven wake (P1b): a watcher fires on any residents/ or waiters/ change (a model freed, the
    // queue moved) so we re-check IMMEDIATELY instead of only on a 2s tick. The 2s cap stays as a safety
    // fallback so a missed/unsupported event still makes progress (degrades to the old poll, never worse).
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let _watcher = watch_residency(tx);
    let mut announced = false;
    let mut ticket: Option<WaiterTicket> = None; // our place in the admission queue (P1/P3)
    loop {
        let admit = match std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(residency_lock_path())
        {
            Ok(f) => f,
            Err(_) => return Ok(None), // can't open admit lock → fail open
        };
        match admit.try_lock() {
            Ok(()) => {
                // ── critical section: enqueue → scan → decide (front-that-fits) → reserve.
                // Join the queue once (seq assigned under this lock → globally ordered arrival).
                if ticket.is_none() {
                    ticket = enqueue_waiter(mypid, footprint_bytes, prio);
                }
                let (in_use, holders) = scan_residents(mypid);
                let budget = host_ram_budget_bytes();
                let available = available_ram_for_admission();
                let min_free = min_free_ram_bytes();
                let pressure = crate::shed::read_host_pressure();
                // ORDERED admission: only the front-most queued waiter whose footprint fits proceeds —
                // no herd racing the instant budget frees. If we couldn't enqueue (ticket None), fall
                // back to a direct admits check (never block on a queue we couldn't join).
                let my_turn = match ticket.as_ref() {
                    Some(t) => {
                        pick_front_that_fits(
                            &scan_waiters(mypid),
                            in_use,
                            budget,
                            available,
                            min_free,
                            pressure,
                        ) == Some((t.prio, t.seq, t.pid))
                    }
                    None => admits(in_use, footprint_bytes, budget, available, min_free, pressure),
                };
                if my_turn {
                    // front-that-fits == us already implies admits(our footprint) holds → reserve:
                    // own `residents/<pid>` and hold its flock for life.
                    match reserve(mypid, model, footprint_bytes) {
                        Some(guard) => {
                            drop(ticket); // leave the queue immediately
                            return Ok(Some(guard));
                        }
                        None => return Ok(None), // couldn't write reservation → fail open
                    }
                }
                // Not our turn — drop the admit lock before sleeping so others proceed.
                drop(admit);
                if !announced {
                    announced = true;
                    let who = holders
                        .iter()
                        .map(|(p, m)| format!("pid {p} {m}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let b = budget.map(|b| b / 1_048_576).unwrap_or(0);
                    let avail_mb = available.map(|a| (a / 1_048_576).to_string()).unwrap_or_else(|| "?".into());
                    eprintln!(
                        "rozum gateway: loading this model (~{} MB) would overcommit host RAM \
                         — {} MB already reserved by [{}], budget ~{} MB; actual free RAM ~{} MB, \
                         keep-free ~{} MB. Waiting up to {}s for it to free \
                         (ROZUM_ALLOW_CONCURRENT_RESIDENT=1 to override) …",
                        footprint_bytes / 1_048_576,
                        in_use / 1_048_576,
                        who,
                        b,
                        avail_mb,
                        min_free / 1_048_576,
                        wait_secs,
                    );
                }
                if std::time::Instant::now() >= deadline {
                    return Err(ResidencyDenied {
                        footprint_bytes,
                        in_use_bytes: in_use,
                        budget_bytes: budget,
                        waited_secs: start.elapsed().as_secs(),
                        holders,
                    });
                }
                wait_for_change(&rx, deadline);
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                // Another gateway is mid-admit (or a v1 holds this for life) — brief wait.
                if std::time::Instant::now() >= deadline {
                    let (in_use, holders) = scan_residents(mypid);
                    return Err(ResidencyDenied {
                        footprint_bytes,
                        in_use_bytes: in_use,
                        budget_bytes: host_ram_budget_bytes(),
                        waited_secs: start.elapsed().as_secs(),
                        holders,
                    });
                }
                wait_for_change(&rx, deadline);
            }
            // FS without advisory locks → fail open.
            Err(std::fs::TryLockError::Error(_)) => return Ok(None),
        }
    }
}

/// Watch `residents/` + `waiters/` for changes; every event pings `tx` so a blocked admission re-checks
/// immediately. The watcher thread lives as long as the returned handle (held for the wait's duration).
/// `None` if the platform watcher can't start → caller degrades to the pure 2s poll fallback.
fn watch_residency(tx: std::sync::mpsc::Sender<()>) -> Option<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let _ = std::fs::create_dir_all(residents_dir());
    let _ = std::fs::create_dir_all(waiters_dir());
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if res.is_ok() {
            let _ = tx.send(()); // coalesced; the waiter drains extras
        }
    })
    .ok()?;
    w.watch(&residents_dir(), RecursiveMode::NonRecursive).ok()?;
    let _ = w.watch(&waiters_dir(), RecursiveMode::NonRecursive); // best-effort second watch
    Some(w)
}

/// Block until the next residency/queue change event, or ~2s (the safety fallback), whichever first —
/// but never past `deadline`. Drains coalesced events so we don't spin.
fn wait_for_change(rx: &std::sync::mpsc::Receiver<()>, deadline: std::time::Instant) {
    let now = std::time::Instant::now();
    if now >= deadline {
        return;
    }
    let cap = std::time::Duration::from_secs(2).min(deadline - now);
    let _ = rx.recv_timeout(cap); // wakes on the first event, else after the fallback cap
    while rx.try_recv().is_ok() {} // drain extras
}

/// Create + flock `residents/<pid>` and write the reservation. `None` if the file
/// can't be created/locked (→ caller fails open).
fn reserve(pid: u32, model: &str, footprint_bytes: u64) -> Option<ResidencyGuard> {
    use std::io::Write as _;
    let path = resident_path(pid);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    // Hold the lifetime flock.
    file.try_lock().ok()?;
    let body = serde_json::to_vec(&ResidentEntry {
        model: model.to_string(),
        footprint_bytes,
    })
    .unwrap_or_default();
    let _ = file.write_all(&body);
    let _ = file.flush();
    Some(ResidencyGuard { _lock: file, path })
}

// ── P1: the admission WAIT QUEUE (spec docs/specs/residency-admission-queue.md) ───────────────
// When a load doesn't fit, the gateway ENQUEUES (a flock'd `waiters/<seq>.<pid>` file, footprint in
// the body) instead of every arrival racing to grab budget the instant it frees. Only the front-most
// waiter whose footprint FITS the current budget proceeds (evaluated under the admit lock, so it is
// serialized → no herd, no admit-TOCTOU). "Front-most-that-fits" (not strict FIFO) avoids head-of-line
// blocking: a small load behind a too-big one still proceeds. Crash-safe like the ledger — the OS drops
// the flock on death, so a dead waiter is reaped (its try_lock succeeds). Replaces the lone 240 s poll
// with COORDINATION; the poll stays as the wake for now (P1b swaps it for a kqueue event).

/// Directory of per-pid wait-queue tickets (parallel to [`residents_dir`]).
pub fn waiters_dir() -> PathBuf {
    gateway_dir().join("waiters")
}

/// The monotonic ticket counter (bumped under the admit lock → globally ordered arrivals).
fn waiter_seq_path() -> PathBuf {
    waiters_dir().join(".seq")
}

/// Queue priority tier (P3). Lower = served first. `interactive` = a live agent load (`rozum launch`);
/// `batch` = a bench/matrix sweep that should YIELD to interactive (`ROZUM_RESIDENCY_PRIO=batch`).
pub const PRIO_INTERACTIVE: u8 = 0;
pub const PRIO_BATCH: u8 = 1;

/// This process's queue priority. Default interactive (a real load); a sweep tags itself batch.
pub fn residency_prio() -> u8 {
    match std::env::var("ROZUM_RESIDENCY_PRIO").ok().as_deref() {
        Some("batch" | "1" | "low") => PRIO_BATCH,
        _ => PRIO_INTERACTIVE,
    }
}

/// A held place in the admission queue. Drop (incl. on the OS reaping our flock at death) removes the
/// ticket file, so we leave the queue the instant we reserve, give up, or crash.
struct WaiterTicket {
    prio: u8,
    seq: u64,
    pid: u32,
    path: PathBuf,
    _lock: std::fs::File,
}
impl Drop for WaiterTicket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Assign the next global ticket number. MUST be called while holding the admit lock (the read-bump-
/// write is only atomic across processes under that lock).
fn next_waiter_seq() -> u64 {
    let p = waiter_seq_path();
    let cur = std::fs::read_to_string(&p).ok().and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
    let next = cur.wrapping_add(1);
    let _ = std::fs::write(&p, next.to_string());
    next
}

/// Enqueue this loader: write a flock-held `waiters/<prio>.<seq>.<pid>` (prio + zero-padded seq so the
/// filename sorts numerically by (prio, seq)) with the footprint in the body. Call under the admit lock.
/// `None` if we can't create/lock the ticket (→ caller falls back to a direct admits check).
fn enqueue_waiter(pid: u32, footprint_bytes: u64, prio: u8) -> Option<WaiterTicket> {
    use std::io::Write as _;
    let _ = std::fs::create_dir_all(waiters_dir());
    let seq = next_waiter_seq();
    let path = waiters_dir().join(format!("{prio}.{seq:020}.{pid}"));
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .ok()?;
    file.try_lock().ok()?;
    let _ = write!(file, "{footprint_bytes}");
    let _ = file.flush();
    Some(WaiterTicket { prio, seq, pid, path, _lock: file })
}

/// Scan the queue: parse `(prio, seq, pid, footprint)` per live ticket, reaping any whose owner died
/// (its flock is free → `try_lock` succeeds). Never reaps `mypid` (our own ticket's flock would block).
fn scan_waiters(mypid: u32) -> Vec<(u8, u64, u32, u64)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(waiters_dir()) else {
        return out;
    };
    for ent in entries.flatten() {
        let fname = ent.file_name();
        let name = fname.to_string_lossy();
        if name.starts_with('.') {
            continue; // the .seq counter
        }
        // `<prio>.<seq>.<pid>`
        let parts: Vec<&str> = name.splitn(3, '.').collect();
        let [pr, s, p] = parts[..] else {
            continue;
        };
        let (Ok(prio), Ok(seq), Ok(pid)) = (pr.parse::<u8>(), s.parse::<u64>(), p.parse::<u32>()) else {
            continue;
        };
        // Reap a dead waiter (not ourselves): if we can flock it, its owner is gone.
        if pid != mypid {
            if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(ent.path()) {
                if f.try_lock().is_ok() {
                    let _ = std::fs::remove_file(ent.path());
                    continue;
                }
            }
        }
        let footprint = std::fs::read_to_string(ent.path())
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(u64::MAX); // unreadable → treat as huge so it never wrongly "fits"
        out.push((prio, seq, pid, footprint));
    }
    out
}

/// The waiter that should proceed: the highest-priority, then lowest-`seq`, ticket whose footprint fits
/// the current budget. Pure (queue passed in) so the ordering is unit-tested without the filesystem.
/// `None` = no queued waiter fits right now (everyone keeps waiting for a resident to free). Priority
/// ordering means an interactive load is served before a batch one even if the batch queued first —
/// but only among waiters that FIT (a high-prio load too big for the current budget never blocks a
/// smaller lower-prio one; that's what cooperative preemption (P4) is for, not the queue).
fn pick_front_that_fits(
    waiters: &[(u8, u64, u32, u64)],
    in_use: u64,
    budget: Option<u64>,
    available: Option<u64>,
    min_free: u64,
    pressure: crate::shed::PressureLevel,
) -> Option<(u8, u64, u32)> {
    let mut w = waiters.to_vec();
    w.sort_by_key(|&(prio, seq, _, _)| (prio, seq));
    for &(prio, seq, pid, fp) in &w {
        if admits(in_use, fp, budget, available, min_free, pressure) {
            return Some((prio, seq, pid));
        }
    }
    None
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

    const GIB: u64 = 1 << 30;

    // The admission decision must pass BOTH levers: the cross-process reserved-footprint ledger AND
    // the actual free-RAM check (the truth lever that prevents a load from overcommitting the host →
    // OS jetsam killing a model mid-work / reboot).
    #[test]
    fn admits_requires_both_ledger_and_actual_free_ram() {
        use crate::shed::PressureLevel::Normal;
        let budget = Some(27 * GIB); // 36 GiB * 0.75
        let min_free = 3 * GIB;
        // Sole model that fits free RAM → admit.
        assert!(admits(0, 20 * GIB, budget, Some(26 * GIB), min_free, Normal));
        // HOLE #3 closed: a SOLE model that does NOT fit actual free RAM is REFUSED (used to be an
        // unconditional admit) — 20 + 3 > 18 available.
        assert!(!admits(0, 20 * GIB, budget, Some(18 * GIB), min_free, Normal));
        // Second model: ledger fits (10 + 12 ≤ 27) AND RAM fits (12 + 3 ≤ 20) → admit.
        assert!(admits(10 * GIB, 12 * GIB, budget, Some(20 * GIB), min_free, Normal));
        // HOLES #1/#2 closed: ledger says OK (10 + 12 ≤ 27) but ACTUAL free RAM is low (heavy
        // non-model use, or a sibling gateway not in the ledger) → REFUSED. 12 + 3 > 8 available.
        assert!(!admits(10 * GIB, 12 * GIB, budget, Some(8 * GIB), min_free, Normal));
        // Ledger overcommits (20 + 12 > 27) even though RAM looks fine → refused.
        assert!(!admits(20 * GIB, 12 * GIB, budget, Some(30 * GIB), min_free, Normal));
    }

    #[test]
    fn admits_fail_open_per_lever_when_unmeasurable() {
        use crate::shed::PressureLevel::Normal;
        let min_free = 3 * GIB;
        // available unknown (vm_stat failed) → the RAM lever doesn't block; the ledger still gates.
        assert!(admits(0, 20 * GIB, Some(27 * GIB), None, min_free, Normal)); // sole, ledger ok
        assert!(!admits(20 * GIB, 12 * GIB, Some(27 * GIB), None, min_free, Normal)); // ledger overcommit
        // budget unknown → only a sole model is admitted (RAM permitting); a 2nd is refused.
        assert!(admits(0, 20 * GIB, None, Some(30 * GIB), min_free, Normal));
        assert!(!admits(5 * GIB, 5 * GIB, None, Some(30 * GIB), min_free, Normal));
        // Both unknown → fail-open to the old sole-only behavior.
        assert!(admits(0, 99 * GIB, None, None, min_free, Normal));
        assert!(!admits(1, 1 * GIB, None, None, min_free, Normal));
    }

    // Kernel memory-pressure guard: even when both byte levers say "fits", an elevated OS pressure
    // level (the jetsam ladder) REFUSES — loading a big model under warn/critical is what tips the
    // host into the jetsam→reboot cascade. Normal pressure leaves the byte decision unchanged.
    #[test]
    fn admits_refuses_under_elevated_pressure() {
        use crate::shed::PressureLevel::{Critical, Normal, Warn};
        let budget = Some(27 * GIB);
        let min_free = 3 * GIB;
        // A load that fits both byte levers comfortably (20 + 3 ≤ 30):
        assert!(admits(0, 20 * GIB, budget, Some(30 * GIB), min_free, Normal));
        // …is REFUSED the instant the host is at warn or critical, regardless of the headroom.
        assert!(!admits(0, 20 * GIB, budget, Some(30 * GIB), min_free, Warn));
        assert!(!admits(0, 20 * GIB, budget, Some(30 * GIB), min_free, Critical));
        // The pressure guard only ADDS refusals — it never rescues a byte-over-budget load.
        assert!(!admits(0, 40 * GIB, budget, Some(10 * GIB), min_free, Normal));
    }

    // P1 queue ordering: the front-most waiter that FITS proceeds — strict FIFO when all fit, but a
    // too-big front waiter does NOT head-of-line-block a smaller one behind it (it just keeps waiting).
    #[test]
    fn queue_picks_front_most_waiter_that_fits() {
        use crate::shed::PressureLevel::Normal;
        let budget = Some(27 * GIB);
        let avail = Some(30 * GIB);
        let min_free = 3 * GIB;
        // All interactive (prio 0). #1 (seq 1) huge 40 GiB won't fit; #2/#3 (10 GiB) fit → head-of-line
        // AVOIDED, #2 goes.
        let waiters = vec![(0u8, 1u64, 100u32, 40 * GIB), (0, 2, 200, 10 * GIB), (0, 3, 300, 10 * GIB)];
        assert_eq!(pick_front_that_fits(&waiters, 0, budget, avail, min_free, Normal), Some((0, 2, 200)));
        // Unsorted, all fit, same prio → FIFO by seq (lowest seq 3 → pid 2).
        let small = vec![(0u8, 5u64, 1u32, 5 * GIB), (0, 3, 2, 5 * GIB), (0, 9, 3, 5 * GIB)];
        assert_eq!(pick_front_that_fits(&small, 0, budget, avail, min_free, Normal), Some((0, 3, 2)));
        // Nothing fits → None (all keep waiting for a resident to free; no spin-load).
        let toobig = vec![(0u8, 1u64, 1u32, 50 * GIB)];
        assert_eq!(pick_front_that_fits(&toobig, 0, budget, avail, min_free, Normal), None);
        // A resident eats budget (in_use 20): the 10 GiB front no longer fits (20+10>27), the 5 GiB
        // behind it does (20+5≤27) → the smaller one is served, not blocked.
        let mixed = vec![(0u8, 1u64, 1u32, 10 * GIB), (0, 2, 2, 5 * GIB)];
        assert_eq!(pick_front_that_fits(&mixed, 20 * GIB, budget, avail, min_free, Normal), Some((0, 2, 2)));
        // P3: an INTERACTIVE (prio 0) that queued LATER (seq 9) beats a BATCH (prio 1) that queued first
        // (seq 1) when both fit — priority wins over arrival order.
        let prio_mix = vec![(PRIO_BATCH, 1u64, 50u32, 5 * GIB), (PRIO_INTERACTIVE, 9, 60, 5 * GIB)];
        assert_eq!(pick_front_that_fits(&prio_mix, 0, budget, avail, min_free, Normal), Some((0, 9, 60)));
    }

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

    const GB: u64 = 1 << 30;

    /// Isolate the gate's state dir + force a known budget + immediate refusal.
    /// Returns the temp dir (remove it when done). Caller holds `POISON_ENV_LOCK`.
    fn residency_env(budget_bytes: u64) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "rozum-residency-{}-{}",
            std::process::id(),
            budget_bytes
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: single-threaded test holding the shared env lock.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", &dir);
            std::env::set_var("ROZUM_GATEWAY_RESIDENCY_WAIT_SECS", "0");
            std::env::set_var("ROZUM_GATEWAY_RAM_BUDGET_BYTES", budget_bytes.to_string());
            std::env::remove_var("ROZUM_GATEWAY_RAM_BUDGET_FRAC");
            std::env::remove_var("ROZUM_ALLOW_CONCURRENT_RESIDENT");
            // These tests exercise the cross-process LEDGER lever; pin actual-available RAM huge so the
            // separate free-RAM lever never interferes (it has its own pure tests). Override cleared by
            // `residency_env_clear`.
            std::env::set_var("ROZUM_GATEWAY_AVAILABLE_RAM_BYTES", (1u64 << 60).to_string());
        }
        dir
    }

    fn residency_env_clear(dir: &std::path::Path) {
        let _ = std::fs::remove_dir_all(dir);
        // SAFETY: single-threaded test holding the shared env lock.
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var("ROZUM_GATEWAY_RESIDENCY_WAIT_SECS");
            std::env::remove_var("ROZUM_GATEWAY_RAM_BUDGET_BYTES");
            std::env::remove_var("ROZUM_GATEWAY_AVAILABLE_RAM_BYTES");
        }
    }

    /// A stand-in for another live resident gateway: writes `residents/<pid>` and
    /// holds its flock (returned File kept alive ⇒ the scan sees it as alive). `pid`
    /// is just the filename — liveness is purely flock-based, no real process needed.
    fn fake_resident(pid: u32, model: &str, footprint: u64) -> std::fs::File {
        use std::io::Write as _;
        let _ = std::fs::create_dir_all(residents_dir());
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(true)
            .open(resident_path(pid))
            .unwrap();
        f.try_lock().unwrap();
        write!(f, "{{\"model\":\"{model}\",\"footprint_bytes\":{footprint}}}").unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn residency_sole_model_always_admitted() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        // Budget absurdly small: a lone model must STILL load — single-gateway
        // operation never caused a reboot, so we never refuse the only model.
        let dir = residency_env(1);
        let g = acquire_residency("only/model", 99 * GB)
            .expect("sole model never denied")
            .expect("a guard for the sole model");
        drop(g);
        residency_env_clear(&dir);
    }

    #[test]
    fn residency_refuses_even_sole_model_that_overcommits_actual_free_ram() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        // Huge budget (ledger lever is permissive) but only ~10 GiB ACTUALLY free → a sole 20 GiB
        // model is REFUSED, because loading it would overcommit the host (jetsam/reboot). This is the
        // hole the free-RAM lever closes that the reserved-footprint ledger alone could not.
        let dir = residency_env(1000 * GB);
        unsafe {
            std::env::set_var("ROZUM_GATEWAY_AVAILABLE_RAM_BYTES", (10 * GB).to_string());
            std::env::set_var("ROZUM_GATEWAY_MIN_FREE_RAM_BYTES", (3 * GB).to_string());
        }
        let denied = acquire_residency("big/model", 20 * GB);
        assert!(denied.is_err(), "20 GiB must be refused with only 10 GiB free, even as the sole model");
        unsafe { std::env::remove_var("ROZUM_GATEWAY_MIN_FREE_RAM_BYTES") }
        residency_env_clear(&dir);
    }

    #[test]
    fn residency_guard_update_footprint_republishes() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        let dir = residency_env(100 * GB);
        // Reserve 3 GiB (sole → admitted), then republish the process's total as warm
        // models come/go (residency-unify U1). A reader from another pid's view must see
        // the live updated value — grow AND shrink — and 0 after release.
        let g = acquire_residency("m", 3 * GB).expect("ok").expect("guard");
        let other = std::process::id().wrapping_add(1);
        assert_eq!(committed_by_others_bytes(other), 3 * GB, "initial reservation visible");
        g.update_footprint("m+warm", 9 * GB);
        assert_eq!(committed_by_others_bytes(other), 9 * GB, "grow republished");
        g.update_footprint("m", 1 * GB);
        assert_eq!(committed_by_others_bytes(other), 1 * GB, "shrink republished");
        drop(g);
        assert_eq!(committed_by_others_bytes(other), 0, "released on drop");
        residency_env_clear(&dir);
    }

    #[test]
    fn update_my_reservation_republishes_or_noops() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        let dir = residency_env(100 * GB);
        let other = std::process::id().wrapping_add(1);
        // No reservation held → no-op (must NOT create a stray unlocked file).
        update_my_reservation("m", 5 * GB);
        assert_eq!(committed_by_others_bytes(other), 0, "no-op without a reservation");
        // With a held reservation, the free fn republishes this process's total.
        let g = acquire_residency("m", 3 * GB).expect("ok").expect("guard");
        update_my_reservation("m+warm", 12 * GB);
        assert_eq!(committed_by_others_bytes(other), 12 * GB, "free-fn republish visible");
        drop(g);
        residency_env_clear(&dir);
    }

    #[test]
    fn residency_refuses_overcommit_admits_fitting_second() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        let dir = residency_env(20 * GB);

        // A 15 GB resident is up. A 2nd 8 GB load → 23 GB > 20 GB budget → REFUSED
        // (this is exactly the overcommit that reboots the host).
        let big = fake_resident(900_001, "resident/big-15g", 15 * GB);
        match acquire_residency("arriving/8g", 8 * GB) {
            Err(d) => {
                assert_eq!(d.in_use_bytes, 15 * GB);
                assert_eq!(d.footprint_bytes, 8 * GB);
                assert_eq!(d.budget_bytes, Some(20 * GB));
                assert_eq!(d.holders.len(), 1);
            }
            Ok(_) => panic!("a 2nd load that overcommits must be refused"),
        }
        drop(big);

        // Now only a 6 GB resident is up. A 2nd 8 GB load → 14 GB ≤ 20 GB → ADMITTED
        // (the v2 win: a genuinely-small 2nd model co-resides).
        let small = fake_resident(900_002, "resident/small-6g", 6 * GB);
        let g = acquire_residency("arriving/8g", 8 * GB)
            .expect("a fitting 2nd model is admitted")
            .expect("a guard for the fitting 2nd model");
        drop(g);
        drop(small);

        residency_env_clear(&dir);
    }

    #[test]
    fn residency_reaps_dead_reservation_and_frees_budget() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        let dir = residency_env(20 * GB);

        // A reservation file whose owner is GONE: write it but DON'T hold the flock
        // (drop the File). It must not count against the budget — the scan reaps it.
        {
            let dead = fake_resident(900_003, "dead/18g", 18 * GB);
            drop(dead); // releases the flock; file stays on disk like a crashed gateway
        }
        assert!(resident_path(900_003).exists(), "stale file present before reap");

        // 18 GB "reserved" by a dead holder would block an 18 GB load (36 > 20); since
        // it's reaped, the arriving model is sole → admitted.
        let g = acquire_residency("arriving/18g", 18 * GB)
            .expect("dead reservation reaped → admitted")
            .expect("a guard");
        assert!(!resident_path(900_003).exists(), "stale reservation was reaped");
        drop(g);

        residency_env_clear(&dir);
    }

    #[test]
    fn residency_escape_hatch_skips_the_gate() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        // SAFETY: single-threaded test holding the shared env lock. The hatch is
        // checked before any file IO, so this never touches a real state dir.
        unsafe { std::env::set_var("ROZUM_ALLOW_CONCURRENT_RESIDENT", "1") };
        let bypass = acquire_residency("x", 99 * GB).expect("hatch never denies");
        assert!(bypass.is_none(), "escape hatch returns no guard (gate skipped)");
        // SAFETY: single-threaded test holding the shared env lock.
        unsafe { std::env::remove_var("ROZUM_ALLOW_CONCURRENT_RESIDENT") };
    }

    #[test]
    fn reap_orphan_residents_removes_dead_keeps_live() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        let dir = residency_env(20 * GB);

        // A dead reservation: written but NOT flock-held (drop the File), like a
        // SIGKILL'd gateway. And a live one held by a kept File.
        {
            let dead = fake_resident(910_001, "dead/x", 5 * GB);
            drop(dead); // releases the flock; file remains on disk
        }
        let _live = fake_resident(910_002, "live/y", 5 * GB);
        // A non-pid file must be ignored by the reaper.
        let _ = std::fs::write(residents_dir().join("notapid"), b"ignore me");

        let reaped = reap_orphan_residents();
        assert_eq!(reaped, 1, "exactly the one dead reservation is reaped");
        assert!(!resident_path(910_001).exists(), "dead reservation removed");
        assert!(resident_path(910_002).exists(), "live reservation kept");
        assert!(residents_dir().join("notapid").exists(), "non-pid file untouched");

        residency_env_clear(&dir);
    }

    #[test]
    fn residency_reserve_overwrites_stale_own_pid_file() {
        let _env = POISON_ENV_LOCK.lock().unwrap();
        let dir = residency_env(20 * GB);

        // A stale reservation file at OUR pid from a prior (dead) process that reused
        // this pid — unlocked, with junk content. Acquire must overwrite it cleanly.
        let mypid = std::process::id();
        let _ = std::fs::create_dir_all(residents_dir());
        std::fs::write(resident_path(mypid), b"{\"model\":\"stale\",\"footprint_bytes\":999999999999}")
            .unwrap();

        let g = acquire_residency("fresh/model", 8 * GB)
            .expect("acquire ok over a stale own-pid file")
            .expect("a guard");
        let body = std::fs::read_to_string(resident_path(mypid)).unwrap();
        assert!(body.contains("fresh/model"), "stale own-pid reservation was overwritten");
        drop(g);

        residency_env_clear(&dir);
    }
}
