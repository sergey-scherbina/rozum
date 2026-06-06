use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, broadcast};

use super::budget::BudgetGuard;
use super::participant::{Participant, ParticipantId};

// ── Events ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum MeetingEvent {
    TurnAdded {
        participant_id: ParticipantId,
        display_name: String,
        content: String,
        seq: usize,
    },
    ParticipantJoined {
        display_name: String,
        is_human: bool,
    },
    ParticipantLeft {
        display_name: String,
    },
    BudgetWarning {
        chars_used: usize,
    },
    MeetingEnded {
        reason: String,
    },
    Renamed {
        old_name: String,
        new_name: String,
    },
    RespondingChanged {
        participant_id: ParticipantId,
        display_name: String,
        started: bool,
    },
    PollingChanged {
        participant_id: ParticipantId,
        display_name: String,
        started: bool,
    },
}

/// Stale window after which a `responding` marker is considered abandoned
/// and filtered out at read time. Keeps a crashed agent from appearing as
/// "typing" forever without needing a background reaper.
pub const RESPONDING_STALE_SECS: u64 = 30;

/// Stale window for `polling` markers — slightly longer than the 25 s
/// long-poll timeout so a marker that wasn't cleared because the connection
/// dropped mid-call still drains naturally.
pub const POLLING_STALE_SECS: u64 = 30;

// ── Transcript ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub seq: usize,
    pub participant_id: String,
    pub display_name: String,
    pub content: String,
    pub ts: u64,
    pub injected: bool,
}

// ── Phase ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Active,
    Paused,
    Ended,
}

// ── Liveness ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ParticipantLiveness {
    pub joined_at: u64,
    pub last_poll_at: Option<u64>,
    pub last_submit_at: Option<u64>,
}

impl ParticipantLiveness {
    fn new(joined_at: u64) -> Self {
        Self {
            joined_at,
            last_poll_at: None,
            last_submit_at: None,
        }
    }
}

// ── Meeting ──────────────────────────────────────────────────────────────────

pub struct Meeting {
    pub name: String,
    pub topic: String,
    pub participants: Vec<Participant>,
    pub transcript: Vec<Turn>,
    pub phase: Phase,
    pub budget: BudgetGuard,
    pub participant_liveness: HashMap<ParticipantId, ParticipantLiveness>,
    /// Participants currently marked as composing a response. Stale entries
    /// (older than `RESPONDING_STALE_SECS`) are filtered at read time.
    pub responding: HashMap<ParticipantId, u64>,
    /// Participants whose `wait_my_turn` long-poll is currently in flight.
    /// Cleared when the poll returns (any path) or on `leave`.
    pub polling: HashMap<ParticipantId, u64>,

    /// Notified whenever the transcript grows or the phase changes — wakes
    /// long-poll subscribers (wait_my_turn, TUI watchers, etc.).
    pub transcript_notify: std::sync::Arc<Notify>,

    /// If set, every submitted Turn is appended to this file as one JSON
    /// line, and the file is loaded back into `transcript` when
    /// `enable_persistence` is called. None disables persistence
    /// (e.g. for tests, --no-persist runs, and proxy-internal rooms).
    pub persist_path: Option<std::path::PathBuf>,

    pub events: broadcast::Sender<MeetingEvent>,
}

impl Meeting {
    pub fn new(
        name: impl Into<String>,
        topic: impl Into<String>,
        human_display_name: impl Into<String>,
        budget: BudgetGuard,
        events: broadcast::Sender<MeetingEvent>,
    ) -> Self {
        let now = unix_ts();
        let human = Participant::human(human_display_name.into());
        let mut participant_liveness = HashMap::new();
        participant_liveness.insert(human.id.clone(), ParticipantLiveness::new(now));
        Self {
            name: name.into(),
            topic: topic.into(),
            participants: vec![human],
            transcript: vec![],
            phase: Phase::Active,
            budget,
            participant_liveness,
            responding: HashMap::new(),
            polling: HashMap::new(),
            transcript_notify: std::sync::Arc::new(Notify::new()),
            persist_path: None,
            events,
        }
    }

    /// Turn on disk persistence for this meeting: load any existing transcript
    /// from `path` into memory (skipping malformed lines), and remember the
    /// path so future submits are appended.
    ///
    /// Idempotent: calling twice with the same path is harmless. Calling with
    /// a different path is not (the caller is responsible for not doing that
    /// during a meeting's lifetime).
    pub fn enable_persistence(&mut self, path: std::path::PathBuf) {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(contents) = std::fs::read_to_string(&path) {
            let mut loaded = 0usize;
            for line in contents.lines() {
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Turn>(line) {
                    Ok(mut turn) => {
                        // Re-number on load so that seq is always
                        // monotonic from 0, even if the file was edited.
                        turn.seq = self.transcript.len();
                        self.transcript.push(turn);
                        loaded += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = ?e,
                            path = %path.display(),
                            "skipping malformed persisted transcript line"
                        );
                    }
                }
            }
            if loaded > 0 {
                tracing::info!(
                    loaded,
                    path = %path.display(),
                    "loaded persisted transcript entries"
                );
            }
        }
        self.persist_path = Some(path);
    }

    // ── Participant management ────────────────────────────────────────────────

    pub fn join_mcp(&mut self, client_info_name: impl Into<String>) -> ParticipantId {
        self.join_non_human(client_info_name, false)
    }

    pub fn join_bridge(&mut self, client_info_name: impl Into<String>) -> ParticipantId {
        self.join_non_human(client_info_name, true)
    }

    fn join_non_human(
        &mut self,
        client_info_name: impl Into<String>,
        bridge: bool,
    ) -> ParticipantId {
        let name = client_info_name.into();
        let now = unix_ts();

        // Find a stale participant with the same base name (exact match or #N suffix).
        // Stale = hasn't polled in 30s (or never polled and joined >30s ago).
        let stale_id = self
            .participants
            .iter()
            .find(|p| {
                if p.is_human() {
                    return false;
                }
                let dn = p.display_name();
                let same_name = dn == name
                    || dn
                        .strip_prefix(&format!("{name}#"))
                        .map_or(false, |s| s.parse::<u32>().is_ok());
                if !same_name {
                    return false;
                }
                self.participant_liveness
                    .get(&p.id)
                    .map_or(true, |l| match l.last_poll_at {
                        Some(ts) => now.saturating_sub(ts) > 30,
                        None => now.saturating_sub(l.joined_at) > 30,
                    })
            })
            .map(|p| p.id.clone());

        let final_name = if stale_id.is_some() {
            name.clone()
        } else {
            self.unique_name(name)
        };
        let p = if bridge {
            Participant::bridge(&final_name)
        } else {
            Participant::mcp(&final_name)
        };
        let id = p.id.clone();
        self.participants.push(p);
        self.participant_liveness
            .insert(id.clone(), ParticipantLiveness::new(now));

        if let Some(sid) = stale_id {
            if let Some(pos) = self.participants.iter().position(|p| p.id == sid) {
                let dn = self.participants[pos].display_name().to_owned();
                self.participants.remove(pos);
                self.participant_liveness.remove(&sid);
                let _ = self
                    .events
                    .send(MeetingEvent::ParticipantLeft { display_name: dn });
            }
        }

        let _ = self.events.send(MeetingEvent::ParticipantJoined {
            display_name: final_name,
            is_human: false,
        });
        id
    }

    pub fn leave(&mut self, id: &ParticipantId) {
        if let Some(pos) = self.participants.iter().position(|p| &p.id == id) {
            let display_name = self.participants[pos].display_name().to_owned();
            self.participants.remove(pos);
            self.participant_liveness.remove(id);
            self.clear_responding(id);
            self.unmark_polling(id);
            let _ = self.events.send(MeetingEvent::ParticipantLeft {
                display_name: display_name.clone(),
            });
            // The room stays alive even if everyone leaves. Agents and humans
            // can rejoin later and pick up the same transcript. The room ends
            // only on explicit `/stop` (operator) or process exit.
        }
    }

    // ── Responding ───────────────────────────────────────────────────────────

    /// Mark a participant as currently composing a response. Idempotent —
    /// re-marking refreshes the timestamp but only emits an event the first
    /// time. Cleared by `submit`, `leave`, or `RESPONDING_STALE_SECS` expiry.
    pub fn mark_responding(&mut self, id: &ParticipantId) {
        if !self.participants.iter().any(|p| &p.id == id) {
            return;
        }
        let now = unix_ts();
        let newly_inserted = self.responding.insert(id.clone(), now).is_none();
        if newly_inserted {
            let display_name = self
                .participants
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.display_name().to_owned())
                .unwrap_or_else(|| id.0.clone());
            let _ = self.events.send(MeetingEvent::RespondingChanged {
                participant_id: id.clone(),
                display_name,
                started: true,
            });
            self.transcript_notify.notify_waiters();
        }
    }

    /// Clear a participant's responding marker. No-op if absent.
    pub fn clear_responding(&mut self, id: &ParticipantId) {
        if self.responding.remove(id).is_some() {
            let display_name = self
                .participants
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.display_name().to_owned())
                .unwrap_or_else(|| id.0.clone());
            let _ = self.events.send(MeetingEvent::RespondingChanged {
                participant_id: id.clone(),
                display_name,
                started: false,
            });
            self.transcript_notify.notify_waiters();
        }
    }

    /// Snapshot of currently responding participants, with stale entries
    /// filtered out. Returns `(participant_id, display_name, started_at,
    /// age_ms)`.
    pub fn active_responding(&self) -> Vec<(ParticipantId, String, u64, u64)> {
        let now = unix_ts();
        self.responding
            .iter()
            .filter(|&(_, &ts)| now.saturating_sub(ts) <= RESPONDING_STALE_SECS)
            .filter_map(|(id, &ts)| {
                let display = self
                    .participants
                    .iter()
                    .find(|p| &p.id == id)
                    .map(|p| p.display_name().to_owned())?;
                let age_ms = now.saturating_sub(ts) * 1000;
                Some((id.clone(), display, ts, age_ms))
            })
            .collect()
    }

    /// Mark a participant as currently waiting on a `wait_my_turn` long-poll.
    /// Idempotent; only emits an event the first time.
    pub fn mark_polling(&mut self, id: &ParticipantId) {
        if !self.participants.iter().any(|p| &p.id == id) {
            return;
        }
        let now = unix_ts();
        let newly_inserted = self.polling.insert(id.clone(), now).is_none();
        if newly_inserted {
            let display_name = self
                .participants
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.display_name().to_owned())
                .unwrap_or_else(|| id.0.clone());
            let _ = self.events.send(MeetingEvent::PollingChanged {
                participant_id: id.clone(),
                display_name,
                started: true,
            });
            self.transcript_notify.notify_waiters();
        }
    }

    /// Clear a participant's polling marker. No-op if absent.
    pub fn unmark_polling(&mut self, id: &ParticipantId) {
        if self.polling.remove(id).is_some() {
            let display_name = self
                .participants
                .iter()
                .find(|p| &p.id == id)
                .map(|p| p.display_name().to_owned())
                .unwrap_or_else(|| id.0.clone());
            let _ = self.events.send(MeetingEvent::PollingChanged {
                participant_id: id.clone(),
                display_name,
                started: false,
            });
            self.transcript_notify.notify_waiters();
        }
    }

    /// Snapshot of participants currently mid-poll. Stale entries
    /// (>`POLLING_STALE_SECS`) are filtered at read time.
    pub fn active_polling(&self) -> Vec<(ParticipantId, String, u64, u64)> {
        let now = unix_ts();
        self.polling
            .iter()
            .filter(|&(_, &ts)| now.saturating_sub(ts) <= POLLING_STALE_SECS)
            .filter_map(|(id, &ts)| {
                let display = self
                    .participants
                    .iter()
                    .find(|p| &p.id == id)
                    .map(|p| p.display_name().to_owned())?;
                let age_ms = now.saturating_sub(ts) * 1000;
                Some((id.clone(), display, ts, age_ms))
            })
            .collect()
    }

    pub fn kick(&mut self, id: &ParticipantId) {
        self.leave(id);
    }

    // ── Submit ────────────────────────────────────────────────────────────────

    /// Anyone can submit at any time. Messages are always posted immediately;
    /// there are no turns and no buffering.
    pub fn submit(
        &mut self,
        participant_id: &ParticipantId,
        content: String,
    ) -> Result<usize, String> {
        if self.phase == Phase::Ended {
            return Err("meeting-ended".into());
        }
        if !self.participants.iter().any(|p| &p.id == participant_id) {
            return Err("not-joined".into());
        }
        self.clear_responding(participant_id);
        Ok(self.post_submission(participant_id, content))
    }

    /// Submit from the TUI human (kept as a thin wrapper for ergonomics).
    pub fn submit_human(&mut self, content: String) {
        if let Some(id) = self.human_participant_id() {
            let _ = self.submit(&id, content);
        }
    }

    /// Append a submission to the transcript and run budget bookkeeping.
    fn post_submission(&mut self, participant_id: &ParticipantId, content: String) -> usize {
        let (warn, exceeded) = self.budget.record_turn(&content);
        let seq = self.transcript.len();
        let display_name = self
            .participants
            .iter()
            .find(|p| &p.id == participant_id)
            .map(|p| p.display_name().to_owned())
            .unwrap_or_else(|| participant_id.0.clone());
        let turn = Turn {
            seq,
            participant_id: participant_id.0.clone(),
            display_name: display_name.clone(),
            content: content.clone(),
            ts: unix_ts(),
            injected: false,
        };
        if let Some(path) = &self.persist_path {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                if let Ok(line) = serde_json::to_string(&turn) {
                    let _ = writeln!(f, "{}", line);
                }
            }
        }
        self.transcript.push(turn);
        if let Some(liveness) = self.participant_liveness.get_mut(participant_id) {
            liveness.last_submit_at = Some(unix_ts());
        }
        let _ = self.events.send(MeetingEvent::TurnAdded {
            participant_id: participant_id.clone(),
            display_name,
            content,
            seq,
        });
        if warn {
            let _ = self.events.send(MeetingEvent::BudgetWarning {
                chars_used: self.budget.total_chars(),
            });
        }
        if exceeded {
            self.end_with_reason("budget");
        }
        self.transcript_notify.notify_waiters();
        seq
    }

    // ── Control ───────────────────────────────────────────────────────────────

    pub fn pause(&mut self) {
        if self.phase == Phase::Active {
            self.phase = Phase::Paused;
        }
    }

    pub fn resume(&mut self) {
        if self.phase == Phase::Paused {
            self.phase = Phase::Active;
            self.transcript_notify.notify_waiters();
        }
    }

    pub fn end(&mut self) {
        self.end_with_reason("user-stopped");
    }

    pub fn end_with_reason(&mut self, reason: &str) {
        if self.phase != Phase::Ended {
            self.phase = Phase::Ended;
            let _ = self.events.send(MeetingEvent::MeetingEnded {
                reason: reason.to_owned(),
            });
            self.transcript_notify.notify_waiters();
        }
    }

    pub fn rename(&mut self, new_name: impl Into<String>) {
        let old_name = std::mem::replace(&mut self.name, new_name.into());
        let _ = self.events.send(MeetingEvent::Renamed {
            old_name,
            new_name: self.name.clone(),
        });
    }

    pub fn record_poll(&mut self, id: &ParticipantId) {
        let now = unix_ts();
        self.participant_liveness
            .entry(id.clone())
            .or_insert_with(|| ParticipantLiveness::new(now))
            .last_poll_at = Some(now);
    }

    // ── Queries ───────────────────────────────────────────────────────────────

    pub fn participant_ids(&self) -> Vec<ParticipantId> {
        self.participants.iter().map(|p| p.id.clone()).collect()
    }

    pub fn human_participant_id(&self) -> Option<ParticipantId> {
        self.participants
            .iter()
            .find(|p| p.is_human())
            .map(|p| p.id.clone())
    }

    pub fn turns_since(&self, since_seq: usize) -> &[Turn] {
        let start = since_seq.min(self.transcript.len());
        &self.transcript[start..]
    }

    pub fn is_human(&self, participant_id: &ParticipantId) -> bool {
        self.participants
            .iter()
            .any(|p| &p.id == participant_id && p.is_human())
    }

    pub fn is_bridge(&self, participant_id: &ParticipantId) -> bool {
        self.participants
            .iter()
            .any(|p| &p.id == participant_id && p.is_bridge())
    }

    pub fn display_name_for(&self, participant_id: &ParticipantId) -> String {
        self.participants
            .iter()
            .find(|p| &p.id == participant_id)
            .map(|p| p.display_name().to_owned())
            .unwrap_or_else(|| participant_id.0.clone())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn unique_name(&self, base: String) -> String {
        let taken: std::collections::HashSet<_> = self
            .participants
            .iter()
            .map(|p| p.display_name().to_owned())
            .collect();
        if !taken.contains(&base) {
            return base;
        }
        for i in 2usize.. {
            let candidate = format!("{base}#{i}");
            if !taken.contains(&candidate) {
                return candidate;
            }
        }
        unreachable!()
    }
}

pub(crate) fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_meeting() -> Meeting {
        let (events, _) = broadcast::channel(16);
        Meeting::new(
            "test-room",
            "test topic",
            "operator",
            BudgetGuard::default(),
            events,
        )
    }

    #[test]
    fn anyone_can_submit_any_time() {
        let mut meeting = fresh_meeting();
        let agent = meeting.join_mcp("codex");
        let human = meeting.human_participant_id().unwrap();

        meeting.submit(&agent, "hi".to_owned()).unwrap();
        meeting.submit(&human, "hello".to_owned()).unwrap();
        meeting.submit(&agent, "again".to_owned()).unwrap();

        assert_eq!(meeting.transcript.len(), 3);
        assert_eq!(meeting.transcript[0].content, "hi");
        assert_eq!(meeting.transcript[1].content, "hello");
        assert_eq!(meeting.transcript[2].content, "again");
    }

    #[test]
    fn submit_from_unknown_participant_errors() {
        let mut meeting = fresh_meeting();
        let phantom = ParticipantId::new("nobody");
        assert!(meeting.submit(&phantom, "x".into()).is_err());
    }

    #[test]
    fn leaving_keeps_the_meeting_alive() {
        let mut meeting = fresh_meeting();
        let agent = meeting.join_mcp("codex");
        let operator_id = meeting.participants[0].id.clone();
        assert_eq!(meeting.phase, Phase::Active);
        // Drop the agent first: 1 participant left, used to auto-end.
        meeting.leave(&agent);
        assert_eq!(meeting.phase, Phase::Active);
        // Drop the operator too: 0 participants, room still alive.
        meeting.leave(&operator_id);
        assert_eq!(meeting.phase, Phase::Active);
        assert!(meeting.participants.is_empty());
    }

    #[test]
    fn responding_marker_lifecycle() {
        let mut meeting = fresh_meeting();
        let agent = meeting.join_mcp("codex");

        assert!(meeting.active_responding().is_empty());

        meeting.mark_responding(&agent);
        let active = meeting.active_responding();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, agent);

        // Submit clears the marker.
        meeting.submit(&agent, "done".to_owned()).unwrap();
        assert!(meeting.active_responding().is_empty());
    }

    #[test]
    fn responding_marker_cleared_on_leave() {
        let mut meeting = fresh_meeting();
        let a = meeting.join_mcp("codex");
        let b = meeting.join_mcp("claude");
        meeting.mark_responding(&a);
        meeting.mark_responding(&b);
        assert_eq!(meeting.active_responding().len(), 2);

        meeting.leave(&a);
        let active = meeting.active_responding();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, b);
    }

    #[test]
    fn responding_marker_ignores_unknown_participant() {
        let mut meeting = fresh_meeting();
        let phantom = ParticipantId::new("nobody");
        meeting.mark_responding(&phantom);
        assert!(meeting.active_responding().is_empty());
    }

    #[test]
    fn responding_marker_filters_stale_at_read_time() {
        let mut meeting = fresh_meeting();
        let agent = meeting.join_mcp("codex");
        // Hand-insert a stale timestamp older than the threshold.
        let stale_ts = unix_ts().saturating_sub(RESPONDING_STALE_SECS + 5);
        meeting.responding.insert(agent.clone(), stale_ts);
        assert!(meeting.active_responding().is_empty());
        // Map still holds the entry — cleanup is lazy.
        assert!(meeting.responding.contains_key(&agent));
    }

    #[test]
    fn polling_marker_lifecycle_and_leave_cleanup() {
        let mut meeting = fresh_meeting();
        let a = meeting.join_mcp("codex");
        let b = meeting.join_mcp("claude");

        assert!(meeting.active_polling().is_empty());

        meeting.mark_polling(&a);
        meeting.mark_polling(&b);
        assert_eq!(meeting.active_polling().len(), 2);

        meeting.unmark_polling(&a);
        let active = meeting.active_polling();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, b);

        meeting.leave(&b);
        assert!(meeting.active_polling().is_empty());
    }
}
