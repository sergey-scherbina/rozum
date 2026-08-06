//! Shared machinery for launching and tracking child processes: agents, coders, terminal sessions.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`) ahead of the session routes, so those
//! depend on this rather than on the file they came from — the ordering that has kept every module
//! in this refactor pointing downward.
//!
//! **A naming trap this module does NOT contain.** "Session" means two different things in the
//! control API: a tmux TERMINAL session (launched, attachable, alive while its tmux session is) and
//! an auth LOGIN session (a cookie in `ucc-auth-sessions.json`). `sess_path` is the second one and
//! stayed with the auth code; grouping by the word would have merged two unrelated subjects.

use std::process::{Command, Stdio};

/// Serializes every read-modify-write of the ucc-{sessions,agents,coders}.json registries. The
/// launch/stop routes and the status-poll prune paths all load→mutate→save the same small JSON
/// files; without this a poll's save can clobber a concurrent launch (lost update → orphan process
/// / row stuck at "starting…"). Held only around fast in-memory + file ops, so contention is nil.
pub(crate) fn registry_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Monotonic per-process suffix so two launches of the same agent/room/model within one wall-clock
/// second get distinct ids (and distinct tmux names). `now_unix()` alone is second-granularity —
/// two `/session/launch` in the same second would collide on the tmux session name.
pub(crate) fn next_launch_seq() -> u64 {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

pub(crate) fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// True if `s` cannot contain shell metacharacters — unlike `sanitize` (which mangles a string into a
/// safe one), this REJECTS instead of mutating, for values that get interpolated into a shell-command
/// string rather than passed as a plain argv element (`session_launch_route`'s tmux `inner` string).
/// Permits the charset real model/agent identifiers use (HF-style `org/repo:tag`, dots, plus signs).
pub(crate) fn shell_safe(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '+'))
}

pub(crate) fn tmux_name(id: &str) -> String { format!("rozum-{id}") }

/// Does the tmux session exist? (`tmux has-session` exit 0). The liveness source of truth.
pub(crate) fn tmux_alive(id: &str) -> bool {
    Command::new("tmux").args(["has-session", "-t", &tmux_name(id)])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

pub(crate) fn is_executable_file(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// Is `agent` actually runnable — an executable of that name on THIS process's PATH? The UCC offers
/// a fixed set of agent chips, but which of them are installed is a property of the machine, and
/// launchd's PATH is not the operator's shell PATH. Without this check a missing CLI is discovered
/// deep inside the spawned `rozum launch`, which exits 127 into a log file — so the row reports
/// "exited", which is indistinguishable from a run that finished.
pub(crate) fn agent_on_path(agent: &str) -> bool {
    let p = std::path::Path::new(agent);
    if p.components().count() > 1 {
        return is_executable_file(p);
    }
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|dir| is_executable_file(&dir.join(agent)))
}

/// The refusal for an agent the machine cannot run, naming the fix rather than the symptom.
/// `None` when the agent is installed.
pub(crate) fn agent_missing_reason(agent: &str) -> Option<String> {
    if agent_on_path(agent) {
        return None;
    }
    let how = match agent {
        "nadia" => "build + install it: `cargo install --path crates/nadia`",
        _ => "install its CLI and make sure it is on the service's PATH",
    };
    Some(format!("agent `{agent}` is not on PATH — {how}"))
}
