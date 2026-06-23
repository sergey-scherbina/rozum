//! Per-session AGENT identity — the Agent side of the `Principal` model (the human side is
//! [`super::local_identity`]). Keyed by `CLAUDE_CODE_SESSION_ID`, established ONCE at session start
//! (`rozum meetings hello`) and read automatically thereafter, so an agent posts as ITSELF — never
//! the operator's identity. Disk-backed (env doesn't persist between shell calls; there is no tty).
//!
//! See `docs/specs/meeting-identity-roster.md`.

use super::store::rozum_state_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One agent's stable per-session identity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentPrincipal {
    pub principal_id: String,
    pub display: String,
    pub session_id: String,
    pub cwd: String,
    pub project: Option<String>,
    pub started: u64,
    pub ts: u64,
}

/// The stable per-session key (`CLAUDE_CODE_SESSION_ID`), or `None` outside an agent session
/// (a bare human shell) — in which case the caller falls through to the human identity.
pub fn session_id() -> Option<String> {
    std::env::var("CLAUDE_CODE_SESSION_ID").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect()
}

fn agents_dir() -> PathBuf {
    rozum_state_dir().join("principals").join("agents")
}

fn path_for(session_id: &str) -> PathBuf {
    agents_dir().join(format!("{}.json", sanitize(session_id)))
}

/// A deterministic, distinct-from-human adjective-animal name from the session id — the fallback when
/// an agent establishes itself without passing its own name. Stable for a given session.
pub fn mint_name(session_id: &str) -> String {
    const ADJ: [&str; 8] = ["sunny", "nimble", "plucky", "rapid", "quiet", "brisk", "keen", "bold"];
    const NOUN: [&str; 8] = ["civet", "raven", "finch", "fox", "lark", "wren", "heron", "swift"];
    let h = session_id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(b as u32));
    format!("{}-{}", ADJ[(h as usize) % ADJ.len()], NOUN[((h / 8) as usize) % NOUN.len()])
}

/// The Agent principal for THIS session, if one has been established.
pub fn current() -> Option<AgentPrincipal> {
    let sid = session_id()?;
    let s = std::fs::read_to_string(path_for(&sid)).ok()?;
    serde_json::from_str(&s).ok()
}

/// Establish (once) the Agent principal for this session, returning it. A second call refreshes
/// `ts`/`cwd` but KEEPS the established name (a name is assigned once). `name = None` mints a stable
/// name from the session id. Returns `None` only outside an agent session (no `CLAUDE_CODE_SESSION_ID`).
pub fn establish(name: Option<String>, project: Option<String>) -> Option<AgentPrincipal> {
    let sid = session_id()?;
    let existing = current();
    // The name is assigned ONCE: an established identity keeps its name on a re-`hello` (which then
    // just refreshes liveness). First establish takes the passed name, else a stable minted one.
    let display = existing
        .as_ref()
        .map(|e| e.display.clone())
        .or_else(|| name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()))
        .unwrap_or_else(|| mint_name(&sid));
    let cwd = std::env::current_dir().ok().map(|p| p.display().to_string()).unwrap_or_default();
    let principal = AgentPrincipal {
        principal_id: existing
            .as_ref()
            .map(|e| e.principal_id.clone())
            .unwrap_or_else(|| format!("agent-{}", sanitize(&sid))),
        display,
        session_id: sid.clone(),
        cwd,
        project,
        started: existing.as_ref().map(|e| e.started).unwrap_or_else(now),
        ts: now(),
    };
    let _ = std::fs::create_dir_all(agents_dir());
    if let Ok(s) = serde_json::to_string(&principal) {
        let _ = std::fs::write(path_for(&sid), s);
    }
    Some(principal)
}

/// Refresh this session's principal liveness (`ts`), if established — called when the agent acts
/// (e.g. posts), so the roster's TTL liveness stays accurate without a separate heartbeat.
pub fn touch() {
    if let Some(mut p) = current() {
        p.ts = now();
        if let Ok(s) = serde_json::to_string(&p) {
            let _ = std::fs::write(path_for(&p.session_id), s);
        }
    }
}

/// All established Agent principals, most-recently-active first.
pub fn list() -> Vec<AgentPrincipal> {
    let mut v: Vec<AgentPrincipal> = std::fs::read_dir(agents_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| serde_json::from_str::<AgentPrincipal>(&s).ok())
        .collect();
    v.sort_by(|a, b| b.ts.cmp(&a.ts));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mint_is_deterministic_and_distinct() {
        let a = mint_name("0345c8d4-6227-4d6f-a0b3-39caebef1509");
        assert_eq!(a, mint_name("0345c8d4-6227-4d6f-a0b3-39caebef1509")); // stable per session
        assert!(a.contains('-')); // adjective-animal, never a human account name
        assert_ne!(a, mint_name("ffffffff-0000-0000-0000-000000000000")); // varies by session
    }

    #[test]
    fn sanitize_keeps_handle_chars() {
        assert_eq!(sanitize("0345c8d4-62/27"), "0345c8d4-62_27");
    }
}
