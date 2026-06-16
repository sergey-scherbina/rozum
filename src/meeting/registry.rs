//! `RoomRegistry` — the daemon's set of open rooms.
//!
//! Holds many [`DaemonRoom`]s keyed by their on-disk `root`, each behind an
//! async mutex so MCP handlers can hold it across an await (the `wait` long-poll).
//! Rooms open **lazily**: a known room is reopened from disk on first access, an
//! unknown one is created fresh (un-materialized until its first message). Idle
//! rooms can be **evicted** from the open set — their files stay, and they reopen
//! on the next access. Discovery (`list`) reads the on-disk `rooms.json` so it
//! sees rooms whether or not they are currently open.
//!
//! This is the in-memory core of the `rozum meetings` daemon (P3); the rmcp
//! server + CLI wiring layer on top. See `docs/specs/agent-meetings-daemon.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::sync::Mutex as AsyncMutex;

use super::room::DaemonRoom;
use super::store::{self, RoomLocation, RoomPaths};

/// A live room behind an async lock.
pub type RoomHandle = Arc<AsyncMutex<DaemonRoom>>;

pub struct RoomRegistry {
    state_dir: PathBuf,
    open: Mutex<HashMap<String, RoomHandle>>,
}

impl RoomRegistry {
    pub fn new(state_dir: PathBuf) -> Self {
        Self {
            state_dir,
            open: Mutex::new(HashMap::new()),
        }
    }

    pub fn state_dir(&self) -> &PathBuf {
        &self.state_dir
    }

    fn key(paths: &RoomPaths) -> String {
        paths.root.to_string_lossy().into_owned()
    }

    /// Resolve a room by its `paths`, opening it from disk if it exists or
    /// creating a fresh (lazy) one otherwise. Idempotent: repeated calls return
    /// the same handle while the room stays open.
    pub fn get_or_create(
        &self,
        paths: RoomPaths,
        name: &str,
        topic: &str,
        project: Option<PathBuf>,
    ) -> std::io::Result<RoomHandle> {
        let key = Self::key(&paths);
        let mut open = self.open.lock().unwrap();
        if let Some(h) = open.get(&key) {
            return Ok(h.clone());
        }
        let room = if paths.meta_path().exists() {
            DaemonRoom::open(paths, self.state_dir.clone())?
        } else {
            DaemonRoom::new(paths, name, topic, project, self.state_dir.clone())
        };
        let handle: RoomHandle = Arc::new(AsyncMutex::new(room));
        open.insert(key, handle.clone());
        Ok(handle)
    }

    /// Resolve a registered room by name (from `rooms.json`), opening it lazily.
    /// `None` if no room of that name is registered.
    pub fn get_by_name(&self, name: &str) -> std::io::Result<Option<RoomHandle>> {
        let Some(loc) = self.location_for_name(name) else {
            return Ok(None);
        };
        let paths = Self::paths_for(&loc);
        Ok(Some(self.get_or_create(
            paths,
            &loc.name,
            "",
            loc.project,
        )?))
    }

    fn location_for_name(&self, name: &str) -> Option<RoomLocation> {
        store::list_registered(&self.state_dir)
            .into_iter()
            .find(|l| l.name == name)
    }

    fn paths_for(loc: &RoomLocation) -> RoomPaths {
        match &loc.project {
            Some(p) => RoomPaths::for_project(p),
            None => RoomPaths::raw(loc.root.clone()),
        }
    }

    /// Evict an idle room (by its `root` key) from the open set. Its files stay;
    /// it reopens on the next access. Returns whether it was open.
    pub fn evict(&self, paths: &RoomPaths) -> bool {
        self.open
            .lock()
            .unwrap()
            .remove(&Self::key(paths))
            .is_some()
    }

    /// All known rooms (from the on-disk registry — independent of open state).
    pub fn list(&self) -> Vec<RoomLocation> {
        store::list_registered(&self.state_dir)
    }

    /// How many rooms are currently held open in memory.
    pub fn open_count(&self) -> usize {
        self.open.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn project_paths(dir: &std::path::Path) -> RoomPaths {
        RoomPaths::for_project(dir)
    }

    #[tokio::test]
    async fn get_or_create_is_idempotent_while_open() {
        let dir = tempdir().unwrap();
        let reg = RoomRegistry::new(dir.path().join("state"));
        let a = reg
            .get_or_create(project_paths(dir.path()), "rozum", "t", None)
            .unwrap();
        let b = reg
            .get_or_create(project_paths(dir.path()), "rozum", "t", None)
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b), "same handle while open");
        assert_eq!(reg.open_count(), 1);
    }

    #[tokio::test]
    async fn evict_then_reopen_continues_from_disk() {
        let dir = tempdir().unwrap();
        let state = dir.path().join("state");
        let reg = RoomRegistry::new(state.clone());

        // Create, join, submit a message → materializes + registers.
        let date;
        {
            let h = reg
                .get_or_create(project_paths(dir.path()), "rozum", "t", None)
                .unwrap();
            let mut room = h.lock().await;
            let (id, _) = room.join(Some("tok"), "claude", "mcp", Some("rozum"));
            date = room.submit(&id, "hello").unwrap().date;
        }

        // Evict from the open set (files remain).
        assert!(reg.evict(&project_paths(dir.path())));
        assert_eq!(reg.open_count(), 0);

        // Reopen lazily — high-water + roster recovered from disk.
        let h = reg
            .get_or_create(project_paths(dir.path()), "rozum", "t", None)
            .unwrap();
        let room = h.lock().await;
        assert_eq!(room.high_water().date, date);
        assert_eq!(room.high_water().n, 1);
        assert!(room.roster().by_token("tok").is_some());
    }

    #[tokio::test]
    async fn list_and_get_by_name_see_registered_rooms() {
        let dir = tempdir().unwrap();
        let reg = RoomRegistry::new(dir.path().join("state"));

        // Before any message: not registered yet (lazy), get_by_name → None.
        assert!(reg.list().is_empty());
        assert!(reg.get_by_name("rozum").unwrap().is_none());

        // First message registers the room under its name.
        {
            let h = reg
                .get_or_create(project_paths(dir.path()), "rozum", "t", None)
                .unwrap();
            let mut room = h.lock().await;
            let (id, _) = room.join(Some("tok"), "claude", "mcp", Some("rozum"));
            room.submit(&id, "hi").unwrap();
        }

        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "rozum");
        assert!(
            reg.get_by_name("rozum").unwrap().is_some(),
            "registered room resolvable by name"
        );
    }
}
