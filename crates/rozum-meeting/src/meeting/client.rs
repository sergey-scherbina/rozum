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
