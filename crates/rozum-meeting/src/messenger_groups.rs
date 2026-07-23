//! Registry of extra chats one messenger bot serves — the operator edits it LIVE from
//! inside the bot (`/addgroup`, `/removegroup`, `/groups`). The bridge reads it to route
//! group chats to their rooms, and the participant pool reads it to run one model per
//! room. Persisted as JSON under the rozum state dir so both processes share it.
//! Spec: `docs/specs/messenger-access-control.md`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::meeting::store::rozum_state_dir;

/// One connected group: its Telegram chat id, the meeting room it maps to, and a title.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub chat_id: i64,
    pub room: String,
    #[serde(default)]
    pub title: String,
}

/// The set of connected groups for one platform.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub groups: Vec<Group>,
}

/// Deterministic room name for a chat id, so re-adding the same group maps to the same
/// room. Telegram group ids are negative; use the magnitude for a clean name.
pub fn default_room(chat_id: i64) -> String {
    format!("group-{}", chat_id.unsigned_abs())
}

impl Registry {
    /// Canonical on-disk path for a platform's group registry (e.g. `telegram`).
    pub fn path(platform: &str) -> PathBuf {
        rozum_state_dir().join("messenger-groups").join(format!("{platform}.json"))
    }

    /// Load the registry, or an empty one if missing/unreadable (a corrupt file is treated
    /// as empty rather than crashing — the operator can re-add groups).
    pub fn load(path: &Path) -> Registry {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Registry::default(),
        }
    }

    /// Atomically persist (write temp + rename), creating the parent dir.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("registry path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let encoded = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, encoded)?;
        std::fs::rename(&tmp, path)
    }

    pub fn contains(&self, chat_id: i64) -> bool {
        self.groups.iter().any(|g| g.chat_id == chat_id)
    }

    pub fn room_for(&self, chat_id: i64) -> Option<&str> {
        self.groups.iter().find(|g| g.chat_id == chat_id).map(|g| g.room.as_str())
    }

    /// Add a group if its chat id is not already present. Returns the room name it maps to
    /// (existing one if already present, so `/addgroup` is idempotent).
    pub fn add(&mut self, chat_id: i64, room: &str, title: &str) -> String {
        if let Some(existing) = self.room_for(chat_id) {
            return existing.to_string();
        }
        self.groups.push(Group {
            chat_id,
            room: room.to_string(),
            title: title.to_string(),
        });
        room.to_string()
    }

    /// Remove a group by chat id; returns it if present.
    pub fn remove(&mut self, chat_id: i64) -> Option<Group> {
        let idx = self.groups.iter().position(|g| g.chat_id == chat_id)?;
        Some(self.groups.remove(idx))
    }

    /// All (chat_id, room) routes.
    pub fn routes(&self) -> Vec<(i64, String)> {
        self.groups.iter().map(|g| (g.chat_id, g.room.clone())).collect()
    }

    /// All room names.
    pub fn rooms(&self) -> Vec<String> {
        self.groups.iter().map(|g| g.room.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_room_uses_magnitude() {
        assert_eq!(default_room(-1004378341901), "group-1004378341901");
    }

    #[test]
    fn add_is_idempotent_and_remove_works() {
        let mut r = Registry::default();
        let room = r.add(-100, &default_room(-100), "Team");
        assert_eq!(room, "group-100");
        // re-add returns the same room, no duplicate
        assert_eq!(r.add(-100, "different", "Team"), "group-100");
        assert_eq!(r.groups.len(), 1);
        assert_eq!(r.room_for(-100), Some("group-100"));
        assert!(r.contains(-100));

        let removed = r.remove(-100).unwrap();
        assert_eq!(removed.chat_id, -100);
        assert!(!r.contains(-100));
        assert!(r.remove(-100).is_none());
    }

    #[test]
    fn save_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");
        let mut r = Registry::default();
        r.add(-1004378341901, "assistant-group", "Rozum Group");
        r.add(-200, &default_room(-200), "Team2");
        r.save(&path).unwrap();

        let back = Registry::load(&path);
        assert_eq!(back.groups.len(), 2);
        assert_eq!(back.room_for(-1004378341901), Some("assistant-group"));
        assert_eq!(back.routes(), vec![(-1004378341901, "assistant-group".into()), (-200, "group-200".into())]);
    }

    #[test]
    fn missing_or_corrupt_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("telegram.json");
        assert!(Registry::load(&path).groups.is_empty());
        std::fs::write(&path, "not json").unwrap();
        assert!(Registry::load(&path).groups.is_empty());
    }
}
