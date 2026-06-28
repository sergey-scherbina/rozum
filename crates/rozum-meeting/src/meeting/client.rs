//! The meeting **client API** — the single contract every client (CLI / web / TUI / the future UCC)
//! uses for room operations, so NO client touches the on-disk jsonl / principal / cursor format.
//! Local clients call these in-process (reading disk *behind* the API for efficiency — the format
//! stays internal here); the same operations are served over HTTP (`rest_read`) for remote/web.
//!
//! See `docs/specs/services-and-clients.md`.

use super::agent_identity::AgentPrincipal;
use super::store::{self, StoredTurn};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Resolve a room name to its on-disk transcript root. `None` → the cwd project's room. A named room
/// resolves via the daemon registry (a project room → `<project>/.rozum/room`), else an ad-hoc room
/// dir under `rooms_dir()`. Returns `None` only when there is no project AND no room name (the caller
/// reports the user-facing error — the API doesn't exit the process).
pub async fn resolve_room_root(room: Option<String>) -> Option<PathBuf> {
    use super::daemon_proxy::detect_project;
    use super::room_path::rooms_dir;
    match room {
        None => detect_project().map(|p| PathBuf::from(p).join(".rozum").join("room")),
        Some(name) => {
            // The registry (`rooms.json`) holds the authoritative root for a registered room —
            // including SHARED rooms (project=None) which live under `$XDG_STATE_HOME/rozum/rooms`,
            // not the runtime dir. Prefer a same-name entry whose root still exists (mtg-registry-dup-name).
            let registered: Vec<PathBuf> = store::list_registered(&store::rozum_state_dir())
                .into_iter()
                .filter(|loc| loc.name == name)
                .map(|loc| loc.root)
                .collect();
            if let Some(root) = registered.iter().find(|r| r.exists()).cloned().or_else(|| registered.into_iter().next_back()) {
                return Some(root);
            }
            // Not registered: fall back to a daemon-reported project, else the ad-hoc rooms dir.
            let project = {
                use super::daemon::{daemon_alive, daemon_rooms};
                use super::room_path::meeting_sock;
                let sock = meeting_sock();
                if daemon_alive(&sock).await {
                    daemon_rooms(&sock)
                        .await
                        .ok()
                        .and_then(|rooms| rooms.into_iter().find(|(n, _)| n == &name).and_then(|(_, p)| p))
                } else {
                    None
                }
            };
            Some(match project {
                Some(p) => PathBuf::from(p).join(".rozum").join("room"),
                None => rooms_dir().join(&name),
            })
        }
    }
}

/// The most-recent `count` turns of a room (oldest-first), or empty if the room has none.
pub fn read(root: &Path, count: usize) -> Vec<StoredTurn> {
    let turns = store::read_since(root, None, 0);
    let start = turns.len().saturating_sub(count);
    turns[start..].to_vec()
}

/// A per-handle seen-cursor for the inbox — the `(date, n)` of the last mention shown to that handle.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct InboxCursor {
    pub date: String,
    pub n: u64,
}

fn cursor_path(root: &Path, handle: &str) -> PathBuf {
    root.join(".inbox").join(format!("{handle}.json"))
}

/// This handle's seen-cursor (default = the start, so an inbox first-look shows all mentions).
pub fn load_inbox_cursor(root: &Path, handle: &str) -> InboxCursor {
    std::fs::read_to_string(cursor_path(root, handle))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Turns that ADDRESS `handle` (`@h` / `-> h`), oldest-first — optionally only those past the
/// seen-cursor (`all = false`), or every such turn (`all = true`).
pub fn inbox(root: &Path, handle: &str, all: bool) -> Vec<StoredTurn> {
    let cursor = load_inbox_cursor(root, handle);
    store::read_since(root, None, 0)
        .into_iter()
        .filter(|t| super::mention::addresses(&t.content, handle))
        .filter(|t| all || (t.date.as_str(), t.n) > (cursor.date.as_str(), cursor.n))
        .collect()
}

/// Advance a handle's inbox seen-cursor to `(date, n)` (after those mentions have been shown).
pub fn advance_inbox_cursor(root: &Path, handle: &str, date: &str, n: u64) {
    let p = cursor_path(root, handle);
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string(&InboxCursor { date: date.to_string(), n }) {
        let _ = std::fs::write(p, s);
    }
}

/// The live agent roster — every established Agent principal, most-recently-active first.
pub fn roster() -> Vec<AgentPrincipal> {
    super::agent_identity::list()
}

// ── identity (who am I) ──────────────────────────────────────────────────────

/// Who a post is authored as, by the principal priority: an explicit `as_display` /
/// `$ROZUM_MEETING_AS` override → THIS session's Agent principal (touched for roster liveness) → the
/// human (the stable local identity). `token = None` ⇒ an ephemeral explicit post. The single place
/// the agent-vs-human posting rule lives, so every client resolves identity identically.
pub fn post_identity(as_display: Option<String>) -> (String, Option<String>) {
    let explicit = as_display
        .or_else(|| std::env::var("ROZUM_MEETING_AS").ok())
        .filter(|s| !s.trim().is_empty());
    match explicit {
        Some(d) => (d, None),
        None => match super::agent_identity::current() {
            Some(agent) => {
                super::agent_identity::touch();
                (agent.display, Some(agent.principal_id))
            }
            None => {
                let id = super::local_identity::load_or_create();
                (id.display, Some(id.token))
            }
        },
    }
}

/// Who this session acts as.
pub enum Identity {
    /// An established Agent principal.
    Agent(AgentPrincipal),
    /// An agent session that hasn't run `hello` yet (has a session id, no principal).
    AgentUnnamed,
    /// The human, by account/login display.
    Human(String),
}

/// Resolve this session's identity (agent principal / un-named agent / human).
pub fn whoami() -> Identity {
    match super::agent_identity::current() {
        Some(p) => Identity::Agent(p),
        None if super::agent_identity::session_id().is_some() => Identity::AgentUnnamed,
        None => Identity::Human(super::local_identity::load_or_create().display),
    }
}

/// Establish this session's Agent principal (once; `name = None` mints a stable one). `None` outside
/// an agent session (no `$CLAUDE_CODE_SESSION_ID`).
pub fn establish(name: Option<String>) -> Option<AgentPrincipal> {
    let project = super::daemon_proxy::detect_project();
    super::agent_identity::establish(name, project)
}

// ── daemon status ────────────────────────────────────────────────────────────

/// The meeting daemon's status: `None` if it isn't running, else its rooms (`(name, project)`).
pub async fn daemon_status() -> Option<Vec<(String, Option<String>)>> {
    use super::daemon::{daemon_alive, daemon_rooms};
    use super::room_path::meeting_sock;
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        return None;
    }
    Some(daemon_rooms(&sock).await.unwrap_or_default())
}
