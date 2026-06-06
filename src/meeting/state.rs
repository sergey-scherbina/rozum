use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::{Notify, broadcast};

use super::budget::BudgetGuard;
use super::moderator::{Moderator, NextChoice};
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
    WaitingFor {
        display_name: String,
        timeout_ms: u64,
        turn_id: Option<u64>,
    },
    ModeChanged {
        new_mode: String,
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
}

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

// ── ActiveTurn ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct ActiveTurn {
    pub participant_id: ParticipantId,
    pub turn_id: u64,
    pub started_at: u64,
    pub deadline_at: Option<u64>,
    pub response_started_at: Option<u64>,
    pub sampling_started: bool,
}

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
    pub moderator: Box<dyn Moderator>,
    pub moderator_name: String,
    pub budget: BudgetGuard,
    pub active_turn: Option<ActiveTurn>,
    pub waiting_for_operator: bool,
    pub agent_turn_timeout_secs: u64,
    pub human_turn_timeout_secs: u64,
    pub next_turn_id: u64,
    pub participant_liveness: HashMap<ParticipantId, ParticipantLiveness>,

    // Notified whenever turn state changes (unblocks wait_my_turn pollers).
    pub turn_notify: std::sync::Arc<Notify>,

    // Broadcast for TUI and any future subscribers.
    pub events: broadcast::Sender<MeetingEvent>,
}

impl Meeting {
    pub fn new(
        name: impl Into<String>,
        topic: impl Into<String>,
        human_display_name: impl Into<String>,
        moderator: Box<dyn Moderator>,
        budget: BudgetGuard,
        events: broadcast::Sender<MeetingEvent>,
    ) -> Self {
        let moderator_name = moderator.name().to_owned();
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
            moderator,
            moderator_name,
            budget,
            active_turn: None,
            waiting_for_operator: false,
            agent_turn_timeout_secs: 60,
            human_turn_timeout_secs: 90,
            next_turn_id: 1,
            participant_liveness,
            turn_notify: std::sync::Arc::new(Notify::new()),
            events,
        }
    }

    // ── Participant management ────────────────────────────────────────────────

    pub fn join_mcp(&mut self, client_info_name: impl Into<String>) -> ParticipantId {
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

        // Add new participant first (keeps count >= 2), then evict the stale one.
        let final_name = if stale_id.is_some() {
            name.clone() // Reclaim the exact name since we'll remove the stale entry.
        } else {
            self.unique_name(name)
        };
        let p = Participant::mcp(&final_name);
        let id = p.id.clone();
        self.participants.push(p);
        self.participant_liveness
            .insert(id.clone(), ParticipantLiveness::new(now));

        if let Some(sid) = stale_id {
            if let Some(pos) = self.participants.iter().position(|p| p.id == sid) {
                let dn = self.participants[pos].display_name().to_owned();
                self.participants.remove(pos);
                self.participant_liveness.remove(&sid);
                let _ = self.events.send(MeetingEvent::ParticipantLeft { display_name: dn });
                // If the evicted participant was the active speaker, clear the turn.
                if self
                    .active_turn
                    .as_ref()
                    .map_or(false, |t| t.participant_id == sid)
                {
                    self.active_turn = None;
                    self.turn_notify.notify_waiters();
                }
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
            let _ = self.events.send(MeetingEvent::ParticipantLeft {
                display_name: display_name.clone(),
            });
            // End meeting if fewer than 2 participants remain.
            if self.participants.len() < 2 {
                self.end_with_reason("insufficient-participants");
            }
            // Clear active turn if the leaver was active.
            if self
                .active_turn
                .as_ref()
                .map_or(false, |t| &t.participant_id == id)
            {
                self.active_turn = None;
                self.turn_notify.notify_waiters();
            }
        }
    }

    // ── Turns ─────────────────────────────────────────────────────────────────

    pub fn submit(
        &mut self,
        participant_id: &ParticipantId,
        content: String,
        expected_turn_id: Option<u64>,
    ) -> Result<usize, String> {
        if self.phase == Phase::Ended {
            return Err("meeting-ended".into());
        }
        match &self.active_turn {
            Some(t) if &t.participant_id != participant_id => {
                return Err("not-your-turn".into());
            }
            Some(t) if expected_turn_id.is_some_and(|turn_id| turn_id != t.turn_id) => {
                return Err("stale-turn".into());
            }
            None if expected_turn_id.is_some() => {
                return Err("stale-turn".into());
            }
            _ => {}
        }
        let (warn, exceeded) = self.budget.record_turn(&content);
        let seq = self.transcript.len();
        let display_name = self
            .participants
            .iter()
            .find(|p| &p.id == participant_id)
            .map(|p| p.display_name().to_owned())
            .unwrap_or_else(|| participant_id.0.clone());
        self.transcript.push(Turn {
            seq,
            participant_id: participant_id.0.clone(),
            display_name: display_name.clone(),
            content: content.clone(),
            ts: unix_ts(),
            injected: false,
        });
        if let Some(liveness) = self.participant_liveness.get_mut(participant_id) {
            liveness.last_submit_at = Some(unix_ts());
        }
        self.active_turn = None;
        self.waiting_for_operator = false;
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
        self.turn_notify.notify_waiters();
        Ok(seq)
    }

    /// Human turn submitted directly from TUI (not through MCP).
    /// If it's not the human's turn, falls back to interject so the message
    /// still reaches agents via transcript_delta without interrupting the
    /// active speaker's turn.
    pub fn submit_human(&mut self, content: String) {
        let human_id = self.human_participant_id();
        let Some(id) = human_id else { return };
        let is_human_turn = self
            .active_turn
            .as_ref()
            .map_or(true, |t| t.participant_id == id);
        if is_human_turn {
            let _ = self.submit(&id, content, None);
        } else {
            self.interject(content);
        }
    }

    /// Inserts a human interject turn before the next scheduled speaker.
    pub fn interject(&mut self, content: String) {
        let human_id = self
            .participants
            .iter()
            .find(|p| p.is_human())
            .map(|p| p.id.clone());
        let display_name = self
            .participants
            .iter()
            .find(|p| p.is_human())
            .map(|p| p.display_name().to_owned())
            .unwrap_or_else(|| "user".into());
        if let Some(id) = human_id {
            let (warn, exceeded) = self.budget.record_turn(&content);
            let seq = self.transcript.len();
            self.transcript.push(Turn {
                seq,
                participant_id: id.0.clone(),
                display_name: display_name.clone(),
                content: content.clone(),
                ts: unix_ts(),
                injected: true,
            });
            let _ = self.events.send(MeetingEvent::TurnAdded {
                participant_id: id.clone(),
                display_name,
                content,
                seq,
            });
            if let Some(liveness) = self.participant_liveness.get_mut(&id) {
                liveness.last_submit_at = Some(unix_ts());
            }
            if warn {
                let _ = self.events.send(MeetingEvent::BudgetWarning {
                    chars_used: self.budget.total_chars(),
                });
            }
            if exceeded {
                self.end_with_reason("budget");
            }
            self.waiting_for_operator = false;
            self.turn_notify.notify_waiters();
        }
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
            self.waiting_for_operator = false;
            self.turn_notify.notify_waiters();
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
            self.turn_notify.notify_waiters();
        }
    }

    pub fn kick(&mut self, id: &ParticipantId) {
        self.leave(id);
    }

    pub fn skip_turn(&mut self) -> Option<usize> {
        self.skip_active_turn("operator")
    }

    pub fn skip_active_turn(&mut self, reason: &str) -> Option<usize> {
        let active = self.active_turn.take()?;
        let display_name = self.display_name_for(&active.participant_id);
        let content = format!(
            "turn skipped for {display_name} (turn #{}, {reason})",
            active.turn_id
        );
        let seq = self.transcript.len();
        self.transcript.push(Turn {
            seq,
            participant_id: "system".to_owned(),
            display_name: "system".to_owned(),
            content: content.clone(),
            ts: unix_ts(),
            injected: true,
        });
        self.waiting_for_operator = false;
        let _ = self.events.send(MeetingEvent::TurnAdded {
            participant_id: ParticipantId::new("system"),
            display_name: "system".to_owned(),
            content,
            seq,
        });
        self.turn_notify.notify_waiters();
        Some(seq)
    }

    pub fn expire_active_turn_if_due(&mut self) -> bool {
        if self.phase != Phase::Active {
            return false;
        }
        let Some(active) = &self.active_turn else {
            return false;
        };
        let Some(deadline_at) = active.deadline_at else {
            return false;
        };
        if deadline_at > unix_ts() {
            return false;
        }
        self.skip_active_turn("timeout");
        true
    }

    pub fn start_active_turn_response(
        &mut self,
        participant_id: &ParticipantId,
        expected_turn_id: Option<u64>,
    ) -> Result<u64, String> {
        if self.phase == Phase::Ended {
            return Err("meeting-ended".into());
        }
        let now = unix_ts();
        match &mut self.active_turn {
            Some(t) if &t.participant_id != participant_id => Err("not-your-turn".into()),
            Some(t) if expected_turn_id.is_some_and(|turn_id| turn_id != t.turn_id) => {
                Err("stale-turn".into())
            }
            Some(t) => {
                t.deadline_at = None;
                t.response_started_at.get_or_insert(now);
                let turn_id = t.turn_id;
                self.turn_notify.notify_waiters();
                Ok(turn_id)
            }
            None => Err("no-active-turn".into()),
        }
    }

    pub fn resume_active_turn_timeout(
        &mut self,
        participant_id: &ParticipantId,
        expected_turn_id: Option<u64>,
    ) -> Result<u64, String> {
        if self.phase == Phase::Ended {
            return Err("meeting-ended".into());
        }
        let timeout_secs = self.timeout_secs_for(participant_id);
        let now = unix_ts();
        match &mut self.active_turn {
            Some(t) if &t.participant_id != participant_id => Err("not-your-turn".into()),
            Some(t) if expected_turn_id.is_some_and(|turn_id| turn_id != t.turn_id) => {
                Err("stale-turn".into())
            }
            Some(t) => {
                t.deadline_at = Some(now + timeout_secs);
                t.response_started_at = None;
                let turn_id = t.turn_id;
                self.turn_notify.notify_waiters();
                Ok(turn_id)
            }
            None => Err("no-active-turn".into()),
        }
    }

    pub fn record_poll(&mut self, id: &ParticipantId) {
        let now = unix_ts();
        self.participant_liveness
            .entry(id.clone())
            .or_insert_with(|| ParticipantLiveness::new(now))
            .last_poll_at = Some(now);
    }

    pub fn switch_mode(&mut self, moderator: Box<dyn Moderator>) {
        let name = moderator.name().to_owned();
        self.moderator = moderator;
        self.moderator_name = name.clone();
        self.waiting_for_operator = false;
        let _ = self
            .events
            .send(MeetingEvent::ModeChanged { new_mode: name });
    }

    pub fn choose_next_speaker(&mut self, selector: &str) -> Result<ParticipantId, String> {
        if self.phase != Phase::Active {
            return Err("meeting-not-active".into());
        }
        let selector = selector.trim();
        if selector.is_empty() {
            return Err("missing-participant".into());
        }
        let Some((id, display_name)) = self
            .participants
            .iter()
            .find(|p| {
                p.id.0.eq_ignore_ascii_case(selector)
                    || p.display_name().eq_ignore_ascii_case(selector)
            })
            .map(|p| (p.id.clone(), p.display_name().to_owned()))
        else {
            return Err(format!("participant-not-found: {selector}"));
        };

        let active = self.start_turn_for(id.clone());
        self.waiting_for_operator = false;
        let _ = self.events.send(MeetingEvent::WaitingFor {
            display_name,
            timeout_ms: self.timeout_secs_for(&id) * 1000,
            turn_id: Some(active.turn_id),
        });
        self.turn_notify.notify_waiters();
        Ok(id)
    }

    pub fn rename(&mut self, new_name: impl Into<String>) {
        let old_name = std::mem::replace(&mut self.name, new_name.into());
        let _ = self.events.send(MeetingEvent::Renamed {
            old_name,
            new_name: self.name.clone(),
        });
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

    // ── Moderator step ────────────────────────────────────────────────────────

    /// Choose the next speaker and set active_turn. Returns None if ended/paused.
    pub fn advance_turn(&mut self) -> Option<ParticipantId> {
        if self.phase != Phase::Active || self.active_turn.is_some() {
            return None;
        }
        let ids = self.participant_ids();
        let last = self
            .transcript
            .last()
            .map(|t| ParticipantId::new(&t.participant_id));
        match self.moderator.next_speaker(&ids, last.as_ref()) {
            NextChoice::Speaker(id) => {
                self.waiting_for_operator = false;
                let display_name = self.display_name_for(&id);
                let active = self.start_turn_for(id.clone());
                let _ = self.events.send(MeetingEvent::WaitingFor {
                    display_name,
                    timeout_ms: self.timeout_secs_for(&id) * 1000,
                    turn_id: Some(active.turn_id),
                });
                self.turn_notify.notify_waiters();
                Some(id)
            }
            NextChoice::WaitForHuman => {
                if !self.waiting_for_operator {
                    let display_name = self
                        .participants
                        .iter()
                        .find(|p| p.is_human())
                        .map(|p| p.display_name().to_owned())
                        .unwrap_or_else(|| "operator".into());
                    let _ = self.events.send(MeetingEvent::WaitingFor {
                        display_name,
                        timeout_ms: self.human_turn_timeout_secs * 1000,
                        turn_id: None,
                    });
                    self.waiting_for_operator = true;
                }
                None
            }
            NextChoice::End => {
                self.end_with_reason("moderator-ended");
                None
            }
        }
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

    fn start_turn_for(&mut self, participant_id: ParticipantId) -> ActiveTurn {
        let now = unix_ts();
        let timeout_secs = self.timeout_secs_for(&participant_id);
        let active = ActiveTurn {
            participant_id,
            turn_id: self.next_turn_id,
            started_at: now,
            deadline_at: Some(now + timeout_secs),
            response_started_at: None,
            sampling_started: false,
        };
        self.next_turn_id += 1;
        self.active_turn = Some(active.clone());
        active
    }

    pub fn timeout_secs_for(&self, participant_id: &ParticipantId) -> u64 {
        if self
            .participants
            .iter()
            .any(|p| &p.id == participant_id && p.is_human())
        {
            self.human_turn_timeout_secs
        } else {
            self.agent_turn_timeout_secs
        }
    }

    pub fn display_name_for(&self, participant_id: &ParticipantId) -> String {
        self.participants
            .iter()
            .find(|p| &p.id == participant_id)
            .map(|p| p.display_name().to_owned())
            .unwrap_or_else(|| participant_id.0.clone())
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
    use crate::meeting::moderator::manual::Manual;
    use crate::meeting::moderator::round_robin::RoundRobin;

    fn manual_meeting() -> Meeting {
        let (events, _) = broadcast::channel(16);
        Meeting::new(
            "test-room",
            "test topic",
            "operator",
            Box::new(Manual::new()),
            BudgetGuard::default(),
            events,
        )
    }

    fn round_robin_meeting() -> Meeting {
        let (events, _) = broadcast::channel(16);
        Meeting::new(
            "test-room",
            "test topic",
            "operator",
            Box::new(RoundRobin::new()),
            BudgetGuard::default(),
            events,
        )
    }

    #[test]
    fn operator_can_choose_next_speaker_by_display_name() {
        let mut meeting = manual_meeting();
        let participant_id = meeting.join_mcp("codex");

        let chosen = meeting.choose_next_speaker("codex").unwrap();

        assert_eq!(chosen, participant_id);
        assert_eq!(
            meeting
                .active_turn
                .as_ref()
                .map(|turn| &turn.participant_id),
            Some(&participant_id)
        );
        assert!(!meeting.waiting_for_operator);
    }

    #[test]
    fn operator_choice_reports_unknown_participant() {
        let mut meeting = manual_meeting();

        assert_eq!(
            meeting.choose_next_speaker("missing").unwrap_err(),
            "participant-not-found: missing"
        );
    }

    #[test]
    fn active_turn_ids_are_monotonic() {
        let mut meeting = round_robin_meeting();
        meeting.join_mcp("codex");

        meeting.advance_turn();
        assert_eq!(meeting.active_turn.as_ref().map(|t| t.turn_id), Some(1));

        meeting.skip_turn();
        meeting.advance_turn();
        assert_eq!(meeting.active_turn.as_ref().map(|t| t.turn_id), Some(2));
    }

    #[test]
    fn submit_rejects_stale_turn_id() {
        let mut meeting = manual_meeting();
        let participant_id = meeting.join_mcp("codex");
        meeting.choose_next_speaker("codex").unwrap();
        let turn_id = meeting.active_turn.as_ref().unwrap().turn_id;

        assert_eq!(
            meeting
                .submit(&participant_id, "late".to_owned(), Some(turn_id + 1))
                .unwrap_err(),
            "stale-turn"
        );

        assert!(
            meeting
                .submit(&participant_id, "on time".to_owned(), Some(turn_id))
                .is_ok()
        );
    }

    #[test]
    fn expired_turn_adds_system_skip_turn() {
        let mut meeting = manual_meeting();
        meeting.agent_turn_timeout_secs = 0;
        meeting.join_mcp("codex");
        meeting.choose_next_speaker("codex").unwrap();

        assert!(meeting.expire_active_turn_if_due());
        assert!(meeting.active_turn.is_none());
        assert_eq!(meeting.transcript.len(), 1);
        assert_eq!(meeting.transcript[0].display_name, "system");
        assert!(meeting.transcript[0].content.contains("timeout"));
    }

    #[test]
    fn started_response_stops_turn_timeout() {
        let mut meeting = manual_meeting();
        meeting.agent_turn_timeout_secs = 0;
        let participant_id = meeting.join_mcp("codex");
        meeting.choose_next_speaker("codex").unwrap();
        let turn_id = meeting.active_turn.as_ref().unwrap().turn_id;

        assert!(
            meeting
                .start_active_turn_response(&participant_id, Some(turn_id))
                .is_ok()
        );

        let active = meeting.active_turn.as_ref().unwrap();
        assert_eq!(active.deadline_at, None);
        assert!(active.response_started_at.is_some());
        assert!(!meeting.expire_active_turn_if_due());
        assert!(meeting.active_turn.is_some());
        assert!(meeting.transcript.is_empty());
    }

    #[test]
    fn resumed_response_timeout_can_expire_again() {
        let mut meeting = manual_meeting();
        meeting.agent_turn_timeout_secs = 0;
        let participant_id = meeting.join_mcp("codex");
        meeting.choose_next_speaker("codex").unwrap();
        let turn_id = meeting.active_turn.as_ref().unwrap().turn_id;
        meeting
            .start_active_turn_response(&participant_id, Some(turn_id))
            .unwrap();

        assert!(
            meeting
                .resume_active_turn_timeout(&participant_id, Some(turn_id))
                .is_ok()
        );

        assert!(meeting.expire_active_turn_if_due());
        assert!(meeting.active_turn.is_none());
        assert_eq!(meeting.transcript[0].display_name, "system");
        assert!(meeting.transcript[0].content.contains("timeout"));
    }
}
