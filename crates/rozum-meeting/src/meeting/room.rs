//! `DaemonRoom` — the in-daemon, disk-backed room model.
//!
//! This is the object a `rozum meetings` daemon holds per room (P3 wraps many of
//! them in a `RoomRegistry`). It owns a single-writer [`TranscriptWriter`] (so it
//! keeps **no message content in RAM**), an identity [`Roster`], and the
//! in-memory coordination state (responding / polling / phase). Events it emits
//! are **shrunk**: they carry `(date, n, end_offset)` and roster/phase deltas,
//! never message content — local clients read content from disk.
//!
//! Built additively alongside the legacy in-process [`super::state::Meeting`],
//! which is retired when bare `rozum` becomes a daemon client (P5). See
//! `docs/specs/agent-meetings-daemon.md`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{Notify, broadcast};

use super::identity::Roster;
use super::participant::ParticipantId;
use super::state::{POLLING_STALE_SECS, RESPONDING_STALE_SECS, unix_ts};
use super::store::{HighWater, RoomPaths, StoredTurn, TranscriptWriter};

/// Shrunk coordination events — **no message content**. Clients read the bytes
/// from disk using the `(date, n, end_offset)` high-water.
#[derive(Clone, Debug)]
pub enum RoomEvent {
    /// A message was appended. Read `[.., end_offset)` of the `date` day file.
    Posted {
        participant_id: ParticipantId,
        date: String,
        n: u64,
        end_offset: u64,
        ts: u64,
    },
    ParticipantJoined {
        id: ParticipantId,
        handle: String,
        base_name: String,
    },
    ParticipantLeft {
        id: ParticipantId,
        handle: String,
    },
    RespondingChanged {
        id: ParticipantId,
        started: bool,
    },
    PollingChanged {
        id: ParticipantId,
        started: bool,
    },
    PhaseChanged {
        ended: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Active,
    Ended,
}

/// One daemon-hosted room.
pub struct DaemonRoom {
    writer: TranscriptWriter,
    roster: Roster,
    phase: Phase,
    /// Participants currently composing (id → started_at); stale entries filtered
    /// at read time.
    responding: HashMap<ParticipantId, u64>,
    /// Participants mid-`wait` long-poll.
    polling: HashMap<ParticipantId, u64>,
    /// Optional hard cap on total chars; `None` = unlimited.
    max_total_chars: Option<u64>,
    /// Last time anything happened in this room (join/submit/responding) — drives
    /// idle eviction.
    last_activity: u64,
    pub notify: Arc<Notify>,
    pub events: broadcast::Sender<RoomEvent>,
}

impl DaemonRoom {
    /// A fresh room (lazy on disk — nothing written until the first message).
    pub fn new(
        paths: RoomPaths,
        name: impl Into<String>,
        topic: impl Into<String>,
        project: Option<PathBuf>,
        registry_state_dir: PathBuf,
    ) -> Self {
        let roster = Roster::load(&paths.roster_path());
        let writer = TranscriptWriter::new(paths, name, topic, project, registry_state_dir);
        Self::wrap(writer, roster)
    }

    /// Reopen an existing room from disk (used on daemon startup).
    pub fn open(paths: RoomPaths, registry_state_dir: PathBuf) -> std::io::Result<Self> {
        let roster = Roster::load(&paths.roster_path());
        let writer = TranscriptWriter::open(paths, registry_state_dir)?;
        Ok(Self::wrap(writer, roster))
    }

    fn wrap(writer: TranscriptWriter, roster: Roster) -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            writer,
            roster,
            phase: Phase::Active,
            responding: HashMap::new(),
            polling: HashMap::new(),
            max_total_chars: None,
            last_activity: unix_ts(),
            notify: Arc::new(Notify::new()),
            events,
        }
    }

    fn touch(&mut self) {
        self.last_activity = unix_ts();
    }

    pub fn last_activity(&self) -> u64 {
        self.last_activity
    }

    /// True if anyone is actively polling or composing right now (stale markers
    /// filtered) — such a room must not be evicted.
    pub fn has_live_pollers(&self) -> bool {
        !self.active_polling().is_empty() || !self.active_responding().is_empty()
    }

    pub fn with_max_total_chars(mut self, max: u64) -> Self {
        self.max_total_chars = Some(max);
        self
    }

    pub fn high_water(&self) -> HighWater {
        self.writer.high_water()
    }

    pub fn name(&self) -> &str {
        self.writer.name()
    }
    pub fn topic(&self) -> &str {
        self.writer.topic()
    }
    pub fn root(&self) -> &std::path::Path {
        &self.writer.paths().root
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn roster(&self) -> &Roster {
        &self.roster
    }
    pub fn budget_chars(&self) -> u64 {
        self.writer.budget_chars()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<RoomEvent> {
        self.events.subscribe()
    }

    // ── Membership ────────────────────────────────────────────────────────────

    /// Join (or rebind, on a known `session_token`) a participant. Persists the
    /// roster and emits `ParticipantJoined` only for a genuinely new identity.
    pub fn join(
        &mut self,
        session_token: Option<&str>,
        base_name: &str,
        kind: &str,
        project: Option<&str>,
    ) -> (ParticipantId, String) {
        self.touch();
        let (id, handle, is_new) =
            self.roster
                .resolve_or_mint(session_token, base_name, kind, project);
        if is_new {
            let _ = self.roster.save(&self.writer.paths().roster_path());
            let _ = self.events.send(RoomEvent::ParticipantJoined {
                id: id.clone(),
                handle: handle.clone(),
                base_name: base_name.to_owned(),
            });
            self.notify.notify_waiters();
        }
        (id, handle)
    }

    /// Forget a participant's live (in-memory) presence. The roster record stays
    /// on disk (so a reconnect by token rebinds the same identity).
    pub fn leave(&mut self, id: &ParticipantId) {
        self.responding.remove(id);
        self.polling.remove(id);
        let handle = self.handle_for(id);
        let _ = self.events.send(RoomEvent::ParticipantLeft {
            id: id.clone(),
            handle,
        });
        self.notify.notify_waiters();
    }

    pub fn handle_for(&self, id: &ParticipantId) -> String {
        self.roster
            .participants
            .iter()
            .find(|e| e.id == id.0)
            .map(|e| e.handle.clone())
            .unwrap_or_else(|| id.0.clone())
    }

    /// The author label written into the transcript: the participant's **identity name** — the
    /// agent's own name or the human's account (`sunny-civet`, `Sergiy`), so readers see WHO posted
    /// and a human never looks like an agent. The base name comes from the joining client (the
    /// agent's `hello`/`--as`, or `$USER`); falls back to the minted handle when no base name is
    /// recorded. (The handle stays internal for uniqueness — see `identity::display_name`.)
    pub fn display_for(&self, id: &ParticipantId) -> String {
        self.roster
            .participants
            .iter()
            .find(|e| e.id == id.0)
            .map(|e| {
                if e.base_name.trim().is_empty() {
                    e.handle.clone()
                } else {
                    super::identity::display_name(&e.base_name, &e.handle)
                }
            })
            .unwrap_or_else(|| id.0.clone())
    }

    fn is_member(&self, id: &ParticipantId) -> bool {
        self.roster.participants.iter().any(|e| e.id == id.0)
    }

    // ── Submit ────────────────────────────────────────────────────────────────

    /// Append a message (free-form — anyone, any time). Writes through the
    /// single writer, clears the sender's responding marker, emits a shrunk
    /// `Posted`, and wakes long-polls. Errors if ended or not a member.
    pub fn submit(&mut self, id: &ParticipantId, content: &str) -> Result<StoredTurn, String> {
        if self.phase == Phase::Ended {
            return Err("meeting-ended".into());
        }
        if !self.is_member(id) {
            return Err("not-joined".into());
        }
        self.touch();
        let display = self.display_for(id);
        let turn = self
            .writer
            .append(&id.0, display, content, unix_ts())
            .map_err(|e| e.to_string())?;
        self.clear_responding(id);

        let hw = self.writer.high_water();
        let _ = self.events.send(RoomEvent::Posted {
            participant_id: id.clone(),
            date: turn.date.clone(),
            n: turn.n,
            end_offset: hw.end_offset,
            ts: turn.ts,
        });
        if let Some(max) = self.max_total_chars {
            if self.writer.budget_chars() >= max {
                self.end();
            }
        }
        self.notify.notify_waiters();
        Ok(turn)
    }

    pub fn end(&mut self) {
        if self.phase != Phase::Ended {
            self.phase = Phase::Ended;
            let _ = self.events.send(RoomEvent::PhaseChanged { ended: true });
            self.notify.notify_waiters();
        }
    }

    // ── Responding / polling markers (liveness, not identity) ─────────────────

    pub fn mark_responding(&mut self, id: &ParticipantId) {
        self.touch();
        if self.is_member(id) && self.responding.insert(id.clone(), unix_ts()).is_none() {
            let _ = self.events.send(RoomEvent::RespondingChanged {
                id: id.clone(),
                started: true,
            });
            self.notify.notify_waiters();
        }
    }

    pub fn clear_responding(&mut self, id: &ParticipantId) {
        if self.responding.remove(id).is_some() {
            let _ = self.events.send(RoomEvent::RespondingChanged {
                id: id.clone(),
                started: false,
            });
            self.notify.notify_waiters();
        }
    }

    pub fn active_responding(&self) -> Vec<ParticipantId> {
        let now = unix_ts();
        self.responding
            .iter()
            .filter(|&(_, &ts)| now.saturating_sub(ts) <= RESPONDING_STALE_SECS)
            .map(|(id, _)| id.clone())
            .collect()
    }

    pub fn mark_polling(&mut self, id: &ParticipantId) {
        if self.is_member(id) && self.polling.insert(id.clone(), unix_ts()).is_none() {
            let _ = self.events.send(RoomEvent::PollingChanged {
                id: id.clone(),
                started: true,
            });
        }
    }

    pub fn unmark_polling(&mut self, id: &ParticipantId) {
        if self.polling.remove(id).is_some() {
            let _ = self.events.send(RoomEvent::PollingChanged {
                id: id.clone(),
                started: false,
            });
        }
    }

    pub fn active_polling(&self) -> Vec<ParticipantId> {
        let now = unix_ts();
        self.polling
            .iter()
            .filter(|&(_, &ts)| now.saturating_sub(ts) <= POLLING_STALE_SECS)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::store::read_day;
    use tempfile::tempdir;

    fn room_in(dir: &std::path::Path) -> DaemonRoom {
        let paths = RoomPaths::for_project(dir);
        DaemonRoom::new(
            paths,
            "rozum",
            "topic",
            Some(dir.to_path_buf()),
            dir.join("state"),
        )
    }

    #[test]
    fn join_mints_persists_and_emits() {
        let dir = tempdir().unwrap();
        let mut room = room_in(dir.path());
        let mut rx = room.subscribe();

        let (id, handle) = room.join(Some("tok"), "claude", "mcp", Some("rozum"));
        assert!(!handle.is_empty());
        assert!(matches!(
            rx.try_recv(),
            Ok(RoomEvent::ParticipantJoined { .. })
        ));

        // Roster persisted to disk.
        let roster = Roster::load(&room.writer.paths().roster_path());
        assert_eq!(roster.participants.len(), 1);
        assert_eq!(roster.by_token("tok").unwrap().id, id.0);

        // Rebind on the same token: no new identity, no second join event.
        let (id2, handle2) = room.join(Some("tok"), "claude", "mcp", Some("rozum"));
        assert_eq!(id, id2);
        assert_eq!(handle, handle2);
        assert!(rx.try_recv().is_err(), "no event on rebind");
    }

    #[test]
    fn submit_writes_disk_and_emits_shrunk_event() {
        let dir = tempdir().unwrap();
        let mut room = room_in(dir.path());
        let (id, _) = room.join(Some("tok"), "claude", "mcp", None);
        let mut rx = room.subscribe();

        let turn = room.submit(&id, "hello world").unwrap();
        assert_eq!(turn.n, 0);

        // Event carries the address but NOT the content.
        match rx.try_recv().unwrap() {
            RoomEvent::Posted {
                date,
                n,
                end_offset,
                ..
            } => {
                assert_eq!(n, 0);
                assert_eq!(date, turn.date);
                assert_eq!(end_offset, room.high_water().end_offset);
            }
            other => panic!("expected Posted, got {other:?}"),
        }

        // Content is on disk, read back via the store.
        let ondisk = read_day(&room.writer.paths().root, &turn.date, 0, None).unwrap();
        assert_eq!(ondisk.len(), 1);
        assert_eq!(ondisk[0].content, "hello world");
        assert_eq!(room.budget_chars(), "hello world".len() as u64);
    }

    #[test]
    fn submit_requires_membership_and_active_phase() {
        let dir = tempdir().unwrap();
        let mut room = room_in(dir.path());
        let phantom = ParticipantId::new("nobody");
        assert_eq!(room.submit(&phantom, "x"), Err("not-joined".into()));

        let (id, _) = room.join(Some("t"), "claude", "mcp", None);
        room.end();
        assert_eq!(room.submit(&id, "x"), Err("meeting-ended".into()));
    }

    #[test]
    fn responding_marker_clears_on_submit() {
        let dir = tempdir().unwrap();
        let mut room = room_in(dir.path());
        let (id, _) = room.join(Some("t"), "claude", "mcp", None);
        room.mark_responding(&id);
        assert_eq!(room.active_responding().len(), 1);
        room.submit(&id, "done").unwrap();
        assert!(room.active_responding().is_empty());
    }

    #[test]
    fn reopen_restores_high_water_and_roster() {
        let dir = tempdir().unwrap();
        let date;
        {
            let mut room = room_in(dir.path());
            let (id, _) = room.join(Some("t"), "claude", "mcp", None);
            date = room.submit(&id, "first").unwrap().date;
        }
        let room2 =
            DaemonRoom::open(RoomPaths::for_project(dir.path()), dir.path().join("state")).unwrap();
        assert_eq!(room2.high_water().date, date);
        assert_eq!(room2.high_water().n, 1);
        // Roster (identity binding) survived the restart.
        assert!(room2.roster().by_token("t").is_some());
    }

    #[test]
    fn max_chars_ends_room() {
        let dir = tempdir().unwrap();
        let paths = RoomPaths::for_project(dir.path());
        let mut room = DaemonRoom::new(paths, "r", "t", None, dir.path().join("state"))
            .with_max_total_chars(3);
        let (id, _) = room.join(Some("t"), "claude", "mcp", None);
        room.submit(&id, "abcd").unwrap(); // 4 >= 3 → ends
        assert_eq!(room.phase(), Phase::Ended);
    }
}
