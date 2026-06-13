use std::{collections::HashMap, sync::Arc};

use rmcp::{
    ErrorData, Peer, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeRequestParams, InitializeResult,
        ServerCapabilities,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::participant::ParticipantId;
use super::state::{Meeting, Phase};

pub type PeerRegistry = Arc<Mutex<HashMap<ParticipantId, Peer<RoleServer>>>>;
type PeerSlot = Arc<Mutex<Option<Peer<RoleServer>>>>;

// ── Tool parameter types ──────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct WaitMyTurnParams {
    /// Sequence number of last transcript entry seen. Omit to receive all.
    pub since_seq: Option<usize>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    /// Content of your reply.
    pub content: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct JoinInternalParams {
    pub client_info_name: String,
    /// Optional participant kind. Defaults to "agent". Bridges (web/telegram/
    /// discord) pass "bridge" so the meeting can distinguish them.
    #[serde(default)]
    pub kind: Option<String>,
}

// ── Room MCP server ───────────────────────────────────────────────────────────

pub type ConnParticipant = Arc<Mutex<Option<ParticipantId>>>;

pub struct RoomServer {
    pub meeting: Arc<Mutex<Meeting>>,
    pub conn_participant: ConnParticipant,
    peer_slot: PeerSlot,
    pub peer_registry: PeerRegistry,
    pub tool_router: ToolRouter<Self>,
}

impl RoomServer {
    pub fn new(
        meeting: Arc<Mutex<Meeting>>,
        conn_participant: ConnParticipant,
        peer_registry: PeerRegistry,
    ) -> Self {
        Self {
            meeting,
            conn_participant,
            peer_slot: Arc::new(Mutex::new(None)),
            peer_registry,
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router(router = tool_router)]
impl RoomServer {
    /// Long-poll for new transcript entries. Returns whenever the transcript
    /// grew past `since_seq`, the meeting ended, or 25 seconds elapsed.
    /// The legacy name is kept for backward compat with existing clients;
    /// there are no turns anymore — anyone may submit at any time.
    #[tool(
        name = "meeting.wait_my_turn",
        description = "Long-poll for new transcript entries (25s). Returns transcript_delta — call again with updated since_seq."
    )]
    pub async fn wait_my_turn(&self, params: Parameters<WaitMyTurnParams>) -> CallToolResult {
        let since_seq = params.0.since_seq.unwrap_or(0);

        let my_id = self.conn_participant.lock().await.clone();
        if my_id.is_none() {
            return err_result("not-joined: call _join_internal first");
        }
        let my_id = my_id.unwrap();

        let notify = {
            let mut m = self.meeting.lock().await;
            m.record_poll(&my_id);
            m.mark_polling(&my_id);
            m.transcript_notify.clone()
        };
        let _polling_guard = PollingGuard {
            meeting: Arc::clone(&self.meeting),
            id: my_id.clone(),
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);

        loop {
            // Arm the wakeup BEFORE checking the transcript. `post_submission` wakes
            // pollers with `notify_waiters()`, which stores NO permit — so a submit
            // landing between this check and the `.await` below would be lost and the
            // poller would stall until the 25s deadline. Registering interest first
            // (`enable()`) means such a `notify_waiters()` wakes this already-armed
            // future instead. (notify_one would fix the race but only wake ONE of N
            // concurrent pollers; we need wake-all.)
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let m = self.meeting.lock().await;

                if m.phase == Phase::Ended {
                    return text_result("{\"ended\":true,\"reason\":\"meeting-ended\"}");
                }

                let delta = m.turns_since(since_seq);
                if !delta.is_empty() {
                    let payload = serde_json::json!({
                        "still_waiting": false,
                        "turn": {
                            "seq": m.transcript.len(),
                            "transcript_delta": delta,
                            "your_turn": true,
                            "responding": responding_json(&m),
                            "polling": polling_json(&m),
                            "instruction": format!(
                                "New messages in '{}'. You are '{}'. Reply via meeting.submit if you have something to say, or wait silently.",
                                m.topic, my_id.0
                            ),
                        }
                    });
                    return text_result(&payload.to_string());
                }
            }

            if tokio::time::Instant::now() >= deadline {
                let m = self.meeting.lock().await;
                let payload = serde_json::json!({
                    "still_waiting": true,
                    "seq": m.transcript.len(),
                    "responding": responding_json(&m),
                    "polling": polling_json(&m),
                });
                return text_result(&payload.to_string());
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let _ = tokio::time::timeout(remaining, notified.as_mut()).await;
        }
    }

    /// Mark this connection's participant as currently composing a response.
    /// Cleared automatically by the next `meeting.submit` from this
    /// participant, by `meeting.leave`, or after `RESPONDING_STALE_SECS`.
    #[tool(
        name = "meeting.mark_responding",
        description = "Signal that you are composing a response. Cleared on submit/leave or 30s stale."
    )]
    pub async fn mark_responding(&self) -> CallToolResult {
        let my_id = self.conn_participant.lock().await.clone();
        let Some(my_id) = my_id else {
            return err_result("not-joined: call _join_internal first");
        };
        let mut m = self.meeting.lock().await;
        m.mark_responding(&my_id);
        let ts = super::state::unix_ts();
        text_result(&format!("{{\"ok\":true,\"ts\":{ts}}}"))
    }

    /// Submit a message. Anyone can submit at any time — no turn restrictions.
    #[tool(
        name = "meeting.submit",
        description = "Submit a message to the meeting. Anyone can submit at any time."
    )]
    pub async fn submit(&self, params: Parameters<SubmitParams>) -> CallToolResult {
        let my_id = self.conn_participant.lock().await.clone();
        let Some(my_id) = my_id else {
            return err_result("not-joined: call _join_internal first");
        };

        let mut m = self.meeting.lock().await;
        match m.submit(&my_id, params.0.content) {
            Ok(seq) => text_result(&format!("{{\"seq\":{seq}}}")),
            Err(e) => err_result(&e),
        }
    }

    /// Leave the meeting room.
    #[tool(name = "meeting.leave", description = "Leave the meeting room.")]
    pub async fn leave(&self) -> CallToolResult {
        let my_id = self.conn_participant.lock().await.take();
        if let Some(id) = my_id {
            self.peer_registry.lock().await.remove(&id);
            self.meeting.lock().await.leave(&id);
        }
        text_result("{\"ok\":true}")
    }

    /// Get the current meeting status including the last 20 turns.
    #[tool(
        name = "meeting.status",
        description = "Get current meeting status: participants, topic, budget, recent turns."
    )]
    pub async fn status(&self) -> CallToolResult {
        let m = self.meeting.lock().await;
        let now = super::state::unix_ts();
        let last_turns: Vec<_> = m
            .transcript
            .iter()
            .rev()
            .take(20)
            .rev()
            .map(|t| {
                serde_json::json!({
                    "seq": t.seq,
                    "speaker": t.display_name,
                    "content": t.content,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "name": m.name,
            "topic": m.topic,
            "phase": format!("{:?}", m.phase),
            "participants": m.participants.iter().map(|p| serde_json::json!({
                "id": p.id.0,
                "display_name": p.display_name(),
                "kind": if p.is_human() { "human" } else if p.is_bridge() { "bridge" } else { "mcp" },
                "liveness": m.participant_liveness.get(&p.id).map(|l| serde_json::json!({
                    "joined_at": l.joined_at,
                    "last_poll_at": l.last_poll_at,
                    "last_poll_age_ms": l.last_poll_at.map(|ts| now.saturating_sub(ts) * 1000),
                    "last_submit_at": l.last_submit_at,
                    "last_submit_age_ms": l.last_submit_at.map(|ts| now.saturating_sub(ts) * 1000),
                }))
            })).collect::<Vec<_>>(),
            "budget": {
                "chars_used": m.budget.total_chars(),
                "max_total_chars": m.budget.max_total_chars,
            },
            "last_turns": last_turns,
            "responding": responding_json(&m),
            "polling": polling_json(&m),
        });
        text_result(&payload.to_string())
    }

    /// Internal: register this connection as a named participant.
    #[tool(
        name = "_join_internal",
        description = "Internal: register as a participant. Returns your assigned participant_id."
    )]
    pub async fn join_internal(&self, params: Parameters<JoinInternalParams>) -> CallToolResult {
        let mut m = self.meeting.lock().await;
        let id = match params.0.kind.as_deref() {
            Some("bridge") => m.join_bridge(&params.0.client_info_name),
            _ => m.join_mcp(&params.0.client_info_name),
        };
        *self.conn_participant.lock().await = Some(id.clone());
        if let Some(peer) = self.peer_slot.lock().await.clone() {
            self.peer_registry.lock().await.insert(id.clone(), peer);
        }
        let payload = serde_json::json!({
            "participant_id": id.0,
            "topic": m.topic,
            "participants": m.participants.iter().map(|p| p.display_name()).collect::<Vec<_>>(),
        });
        text_result(&payload.to_string())
    }

    /// Internal: room metadata for discovery (used by rooms.list and rozum list).
    #[tool(
        name = "_room_info",
        description = "Internal: room metadata for discovery."
    )]
    pub async fn room_info(&self) -> CallToolResult {
        let m = self.meeting.lock().await;
        let payload = serde_json::json!({
            "name": m.name,
            "topic": m.topic,
            "participants": m.participants.iter().map(|p| p.display_name()).collect::<Vec<_>>(),
            "phase": format!("{:?}", m.phase),
        });
        text_result(&payload.to_string())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for RoomServer {
    async fn initialize(
        &self,
        _params: InitializeRequestParams,
        context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        *self.peer_slot.lock().await = Some(context.peer.clone());
        let m = self.meeting.lock().await;
        Ok(
            InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new(
                    format!("rozum-room-{}", m.name),
                    env!("CARGO_PKG_VERSION"),
                ))
                .with_instructions(format!(
                    "Meeting room '{}'. Topic: '{}'. \
                 Call _join_internal to register. Loop: meeting.wait_my_turn (25s long-poll) \
                 → meeting.submit if you have something to add. There are no turns: \
                 anyone may submit at any time, and you'll be sampling-pinged when others speak.",
                    m.name, m.topic
                )),
        )
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn text_result(content: &str) -> CallToolResult {
    CallToolResult::success(vec![Content::text(content)])
}

fn err_result(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

fn responding_json(m: &Meeting) -> Vec<serde_json::Value> {
    m.active_responding()
        .into_iter()
        .map(|(id, display_name, started_at, age_ms)| {
            serde_json::json!({
                "participant_id": id.0,
                "display_name": display_name,
                "started_at": started_at,
                "age_ms": age_ms,
            })
        })
        .collect()
}

fn polling_json(m: &Meeting) -> Vec<serde_json::Value> {
    m.active_polling()
        .into_iter()
        .map(|(id, display_name, started_at, age_ms)| {
            serde_json::json!({
                "participant_id": id.0,
                "display_name": display_name,
                "started_at": started_at,
                "age_ms": age_ms,
            })
        })
        .collect()
}

/// RAII guard that clears a participant's `polling` marker when dropped.
/// Spawns a background task because `Drop` is sync but the meeting mutex is
/// async. Handles normal returns, errors, and async cancellation alike.
struct PollingGuard {
    meeting: Arc<Mutex<Meeting>>,
    id: ParticipantId,
}

impl Drop for PollingGuard {
    fn drop(&mut self) {
        let meeting = Arc::clone(&self.meeting);
        let id = self.id.clone();
        tokio::spawn(async move {
            meeting.lock().await.unmark_polling(&id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::budget::BudgetGuard;
    use std::time::Duration;
    use tokio::sync::broadcast;

    // Regression guard for the long-poll wakeup: a `meeting.submit` must wake an
    // in-flight `wait_my_turn` promptly (well under its 25s deadline), not stall.
    // Guards the `notify_waiters` + armed-`Notified` wiring against a lost wakeup.
    #[tokio::test]
    async fn wait_my_turn_wakes_on_submit() {
        let (events, _rx) = broadcast::channel(16);
        let mut meeting = Meeting::new("room", "topic", "operator", BudgetGuard::default(), events);
        let agent = meeting.join_mcp("codex"); // the poller
        let human = meeting.human_participant_id().unwrap(); // the submitter
        let meeting = Arc::new(Mutex::new(meeting));

        let server = RoomServer::new(
            meeting.clone(),
            Arc::new(Mutex::new(Some(agent))),
            Arc::new(Mutex::new(HashMap::new())),
        );

        // No turns yet → this would block up to 25s without a wakeup.
        let poll = tokio::spawn(async move {
            server
                .wait_my_turn(Parameters(WaitMyTurnParams { since_seq: Some(0) }))
                .await
        });

        // Let the poller enter its wait, then submit.
        tokio::time::sleep(Duration::from_millis(150)).await;
        meeting
            .lock()
            .await
            .submit(&human, "hello-from-human".to_owned())
            .unwrap();

        let result = tokio::time::timeout(Duration::from_secs(5), poll)
            .await
            .expect("wait_my_turn did not return within 5s — lost wakeup?")
            .expect("poll task panicked");
        let s = serde_json::to_string(&result).unwrap();
        assert!(s.contains("hello-from-human"), "expected the new turn; got {s}");
        assert!(s.contains("your_turn"), "expected your_turn payload; got {s}");
    }
}
