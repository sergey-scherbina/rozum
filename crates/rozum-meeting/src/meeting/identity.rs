//! Stable, session-lifetime participant identity for daemon-hosted rooms.
//!
//! A participant gets an **opaque** `ParticipantId` (a UUID, decoupled from the
//! display name) and a friendly **handle** (`eager-otter`) minted once and unique
//! within the room. Continuity across reconnects is keyed on a `session_token`
//! the proxy holds for its lifetime — re-presenting the token rebinds the same
//! id and handle (no `#N` reshuffle). The binding is persisted in the room's
//! `roster.json`, so a daemon restart rebinds a live proxy's token.
//!
//! Additive: this module provides the primitives; wiring them into the room
//! model (replacing the old name+staleness reclaim) happens in a later phase.
//! See `docs/specs/agent-meetings-daemon.md`.

use std::collections::HashSet;
use std::path::Path;

use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::participant::ParticipantId;

/// A participant's durable record within a room's roster.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RosterEntry {
    /// Opaque `ParticipantId` value (a UUID).
    pub id: String,
    /// Friendly, unique-within-room handle, e.g. `eager-otter`.
    pub handle: String,
    /// The client's base name (e.g. `claude`), shown decorated with the handle.
    pub base_name: String,
    /// `"mcp" | "human" | "bridge"`.
    pub kind: String,
    pub project: Option<String>,
    /// The proxy's session token; `None` for the human/operator. The reconnect
    /// key.
    pub session_token: Option<String>,
}

/// A room's participant roster, persisted as `roster.json`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Roster {
    pub participants: Vec<RosterEntry>,
}

impl Roster {
    /// Load from `path`; a missing or unreadable file yields an empty roster.
    pub fn load(path: &Path) -> Self {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist to `path` (atomic via tmp + rename).
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)
    }

    fn taken_handles(&self) -> HashSet<String> {
        self.participants.iter().map(|e| e.handle.clone()).collect()
    }

    /// Look up an entry by its session token.
    pub fn by_token(&self, token: &str) -> Option<&RosterEntry> {
        self.participants
            .iter()
            .find(|e| e.session_token.as_deref() == Some(token))
    }

    /// Resolve a participant by `session_token` — rebinding the same id+handle on
    /// reconnect — or mint a fresh opaque id + unique handle and record it.
    /// Returns `(id, handle, is_new)`.
    pub fn resolve_or_mint(
        &mut self,
        session_token: Option<&str>,
        base_name: &str,
        kind: &str,
        project: Option<&str>,
    ) -> (ParticipantId, String, bool) {
        if let Some(tok) = session_token {
            if let Some(e) = self.by_token(tok) {
                return (ParticipantId::new(e.id.clone()), e.handle.clone(), false);
            }
        }
        let id = Uuid::new_v4().simple().to_string();
        let handle = mint_handle(&self.taken_handles());
        self.participants.push(RosterEntry {
            id: id.clone(),
            handle: handle.clone(),
            base_name: base_name.to_owned(),
            kind: kind.to_owned(),
            project: project.map(|p| p.to_owned()),
            session_token: session_token.map(|t| t.to_owned()),
        });
        (ParticipantId::new(id), handle, true)
    }
}

/// How a participant is shown: by its **identity name** (the human's account / the agent's own
/// name). The minted `handle` stays internal (uniqueness, plus a distinct label for an un-named
/// client) but is no longer mashed into the display — so a human never looks like an agent
/// (`Sergiy`, not `Sergiy · plucky-fox`) and a named agent shows its own name. Falls back to the
/// handle only when there is no base name. See `docs/specs/meeting-identity-roster.md`.
pub fn display_name(base_name: &str, handle: &str) -> String {
    if base_name.trim().is_empty() {
        handle.to_string()
    } else {
        base_name.to_string()
    }
}

const ADJECTIVES: &[&str] = &[
    "eager", "calm", "brave", "bright", "swift", "keen", "wise", "bold", "merry", "nimble",
    "quiet", "lucky", "sunny", "clever", "gentle", "jolly", "spry", "witty", "mellow", "plucky",
];

const ANIMALS: &[&str] = &[
    "otter", "lynx", "heron", "fox", "wren", "marten", "ibex", "tapir", "civet", "gecko", "raven",
    "stoat", "shrew", "vole", "newt", "quail", "finch", "crane", "sable", "perch",
];

/// Mint a friendly handle (`adjective-animal`) not already in `taken`. Falls
/// back to a numeric suffix only if the (400-combo) space is exhausted.
pub fn mint_handle(taken: &HashSet<String>) -> String {
    let mut rng = rand::thread_rng();
    for _ in 0..64 {
        let adj = ADJECTIVES.choose(&mut rng).unwrap_or(&"eager");
        let animal = ANIMALS.choose(&mut rng).unwrap_or(&"otter");
        let candidate = format!("{adj}-{animal}");
        if !taken.contains(&candidate) {
            return candidate;
        }
    }
    // Space effectively exhausted — deterministic fallback.
    for adj in ADJECTIVES {
        for animal in ANIMALS {
            for i in 2.. {
                let candidate = format!("{adj}-{animal}-{i}");
                if !taken.contains(&candidate) {
                    return candidate;
                }
                if i > taken.len() + 2 {
                    break;
                }
            }
        }
    }
    format!("agent-{}", Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn same_token_rebinds_same_id_and_handle() {
        let mut r = Roster::default();
        let (id1, h1, new1) = r.resolve_or_mint(Some("tok-A"), "claude", "mcp", Some("rozum"));
        assert!(new1);
        // Reconnect with the same token → same identity, not new.
        let (id2, h2, new2) = r.resolve_or_mint(Some("tok-A"), "claude", "mcp", Some("rozum"));
        assert!(!new2);
        assert_eq!(id1, id2);
        assert_eq!(h1, h2);
        assert_eq!(r.participants.len(), 1, "no duplicate entry on reconnect");
    }

    #[test]
    fn different_tokens_get_different_handles_and_ids() {
        let mut r = Roster::default();
        let (id1, h1, _) = r.resolve_or_mint(Some("tok-A"), "claude", "mcp", None);
        let (id2, h2, _) = r.resolve_or_mint(Some("tok-B"), "claude", "mcp", None);
        assert_ne!(id1, id2);
        assert_ne!(h1, h2, "two same-named agents must not collide on handle");
        assert_eq!(r.participants.len(), 2);
    }

    #[test]
    fn ids_are_opaque_uuids_no_hash_suffix() {
        let mut r = Roster::default();
        let (id, handle, _) = r.resolve_or_mint(Some("t"), "claude", "mcp", None);
        assert!(!id.0.contains('#'), "no positional #N suffix");
        assert!(!handle.contains('#'));
        assert_eq!(id.0.len(), 32, "uuid simple form");
        // Display is the identity name; the minted handle stays internal (not mashed in).
        assert_eq!(display_name("claude", &handle), "claude");
        // An un-named client still gets a distinct label from its handle.
        assert_eq!(display_name("", &handle), handle);
    }

    #[test]
    fn roster_round_trips_through_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("roster.json");
        let mut r = Roster::default();
        r.resolve_or_mint(Some("tok-A"), "claude", "mcp", Some("rozum"));
        r.resolve_or_mint(None, "operator", "human", None);
        r.save(&path).unwrap();

        let loaded = Roster::load(&path);
        assert_eq!(loaded.participants.len(), 2);
        // The token binding survives the reload (daemon-restart rebind).
        let e = loaded.by_token("tok-A").unwrap();
        assert_eq!(e.base_name, "claude");
        assert_eq!(e.kind, "mcp");
    }

    #[test]
    fn minted_handle_avoids_taken() {
        // Pre-fill many handles; minted one must still avoid them all.
        let mut taken = HashSet::new();
        for adj in &ADJECTIVES[..10] {
            for animal in &ANIMALS[..10] {
                taken.insert(format!("{adj}-{animal}"));
            }
        }
        let h = mint_handle(&taken);
        assert!(!taken.contains(&h));
    }

    #[test]
    fn human_without_token_mints_each_time() {
        let mut r = Roster::default();
        // No token → no reconnect key → a fresh identity each call.
        let (id1, _, new1) = r.resolve_or_mint(None, "operator", "human", None);
        let (id2, _, new2) = r.resolve_or_mint(None, "operator", "human", None);
        assert!(new1 && new2);
        assert_ne!(id1, id2);
    }
}
