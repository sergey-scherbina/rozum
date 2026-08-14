use std::{collections::HashMap, sync::Arc};

use rmcp::model::{CreateMessageRequestParams, SamplingMessage};
use rmcp::{Peer, RoleServer};
use crate::meeting::ipc;
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::meeting::budget::BudgetGuard;
use crate::meeting::mcp_server::{ConnParticipant, PeerRegistry, RoomServer};
use crate::meeting::participant::ParticipantId;
use crate::meeting::room_path::{ensure_rooms_dir, room_socket};
use crate::meeting::state::{Meeting, MeetingEvent, Phase};

const SAMPLING_COMPOSE_TIMEOUT_SECS: u64 = 120;
/// Max consecutive LLM-only messages without any human/bridge input. Prevents
/// agents from chatting forever among themselves.
const MAX_CONSECUTIVE_LLM_MESSAGES: u32 = 10;

pub struct RoomConfig {
    pub name: String,
    pub topic: String,
    pub human_display_name: String,
    pub budget: BudgetGuard,
    pub web_url: Option<String>,
    /// When true, the meeting's transcript is loaded from and appended to
    /// `$XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl` so that
    /// a process restart preserves history. Defaults to true.
    pub persist: bool,
}

impl Default for RoomConfig {
    fn default() -> Self {
        let username = std::env::var("USER").unwrap_or_else(|_| "user".into());
        Self {
            name: crate::meeting::room_path::generate_room_name(),
            topic: String::new(),
            human_display_name: username,
            budget: BudgetGuard::default(),
            web_url: None,
            persist: true,
        }
    }
}

fn room_transcript_path(name: &str) -> std::path::PathBuf {
    rozum_paths::state_dir()
        .unwrap_or_else(|| std::path::PathBuf::from(".local").join("state").join("rozum"))
        .join("rooms")
        .join(name)
        .join("room-transcript.jsonl")
}

/// Launch the meeting room: MCP server + TUI. Runs until the user quits.
pub async fn run_room(
    config: RoomConfig,
    headless: bool,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    ensure_rooms_dir()?;
    let socket_path = room_socket(&config.name);

    let _ = std::fs::remove_file(&socket_path);

    let (events_tx, _events_rx) = broadcast::channel::<MeetingEvent>(256);

    let mut meeting = Meeting::new(
        config.name.clone(),
        config.topic.clone(),
        config.human_display_name.clone(),
        config.budget,
        events_tx.clone(),
    );
    if config.persist {
        meeting.enable_persistence(room_transcript_path(&config.name));
    }
    let meeting = Arc::new(Mutex::new(meeting));
    let peer_registry: PeerRegistry = Arc::new(Mutex::new(HashMap::new()));

    let mut listener = ipc::Listener::bind(&socket_path)?;
    tracing::info!(
        "Room '{}' listening on {}",
        config.name,
        socket_path.display()
    );

    spawn_sampling_workers(
        Arc::clone(&meeting),
        Arc::clone(&peer_registry),
        events_tx.subscribe(),
    );

    if headless {
        loop {
            let stream = listener.accept().await?;
            spawn_connection(stream, Arc::clone(&meeting), Arc::clone(&peer_registry));
        }
    } else {
        let meeting_for_mcp = Arc::clone(&meeting);
        let registry_for_mcp = Arc::clone(&peer_registry);
        let events_rx = events_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok(stream) => {
                        spawn_connection(
                            stream,
                            Arc::clone(&meeting_for_mcp),
                            Arc::clone(&registry_for_mcp),
                        );
                    }
                    Err(e) => {
                        tracing::error!("listener error: {e}");
                        break;
                    }
                }
            }
        });

        crate::tui::run_tui(meeting, events_rx, events_tx, config.web_url).await?;
    }

    let _ = std::fs::remove_file(&socket_path);
    Ok(())
}

/// Spawn the event-driven sampling worker pair:
///  - the dispatcher subscribes to TurnAdded events and decides which agents
///    should be pinged; it enforces the consecutive-LLM cap.
///  - the worker drains the job queue and runs samplings one at a time.
fn spawn_sampling_workers(
    meeting: Arc<Mutex<Meeting>>,
    registry: PeerRegistry,
    mut events_rx: broadcast::Receiver<MeetingEvent>,
) {
    let (jobs_tx, mut jobs_rx) = mpsc::unbounded_channel::<ParticipantId>();

    // Dispatcher: events → jobs.
    let dispatcher_meeting = Arc::clone(&meeting);
    tokio::spawn(async move {
        let mut consecutive_llm: u32 = 0;
        loop {
            match events_rx.recv().await {
                Ok(MeetingEvent::TurnAdded { participant_id, .. }) => {
                    let (is_human_or_bridge, agents) = {
                        let m = dispatcher_meeting.lock().await;
                        let kind_resets_counter =
                            m.is_human(&participant_id) || m.is_bridge(&participant_id);
                        let agents: Vec<ParticipantId> = m
                            .participants
                            .iter()
                            .filter(|p| !p.is_human() && !p.is_bridge() && p.id != participant_id)
                            .map(|p| p.id.clone())
                            .collect();
                        (kind_resets_counter, agents)
                    };

                    if is_human_or_bridge {
                        consecutive_llm = 0;
                    } else {
                        consecutive_llm = consecutive_llm.saturating_add(1);
                        if consecutive_llm > MAX_CONSECUTIVE_LLM_MESSAGES {
                            tracing::debug!(
                                "consecutive-llm cap ({MAX_CONSECUTIVE_LLM_MESSAGES}) reached; \
                                 not pinging more agents until a human/bridge speaks"
                            );
                            continue;
                        }
                    }

                    for id in agents {
                        let _ = jobs_tx.send(id);
                    }
                }
                Ok(MeetingEvent::MeetingEnded { .. }) => break,
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Worker: drains jobs sequentially.
    let worker_meeting = Arc::clone(&meeting);
    let worker_registry = registry;
    tokio::spawn(async move {
        while let Some(id) = jobs_rx.recv().await {
            {
                let m = worker_meeting.lock().await;
                if m.phase == Phase::Ended {
                    return;
                }
            }
            let peer = worker_registry.lock().await.get(&id).cloned();
            if let Some(peer) = peer {
                run_sampling(&worker_meeting, &id, peer).await;
            }
        }
    });
}

/// Spawn one MCP connection handler. Each connection gets its own RoomServer
/// instance. Auto-leaves the meeting when the connection drops.
fn spawn_connection(
    stream: ipc::Stream,
    meeting: Arc<Mutex<Meeting>>,
    peer_registry: PeerRegistry,
) {
    use rmcp::ServiceExt;

    let conn_participant: ConnParticipant = Arc::new(Mutex::new(None));
    let conn_participant_for_cleanup = Arc::clone(&conn_participant);
    let meeting_for_cleanup = Arc::clone(&meeting);
    let registry_for_cleanup = Arc::clone(&peer_registry);

    let handler = RoomServer::new(meeting, conn_participant, peer_registry);

    tokio::spawn(async move {
        if let Ok(service) = handler.serve(stream).await {
            service.waiting().await.ok();
        }
        if let Some(id) = conn_participant_for_cleanup.lock().await.take() {
            registry_for_cleanup.lock().await.remove(&id);
            meeting_for_cleanup.lock().await.leave(&id);
        }
    });
}

/// Issue one sampling request to the agent's peer. If the peer returns
/// non-empty content, submit it as that agent's turn. Errors and empty
/// responses are treated as "stay silent" — no transcript entry, no skip
/// message.
async fn run_sampling(
    meeting: &Arc<Mutex<Meeting>>,
    participant_id: &ParticipantId,
    peer: Peer<RoleServer>,
) {
    let (transcript_text, topic) = {
        let m = meeting.lock().await;
        if m.phase == Phase::Ended {
            return;
        }
        if !m.participants.iter().any(|p| &p.id == participant_id) {
            return;
        }
        let transcript = m
            .transcript
            .iter()
            .map(|t| format!("[{}]: {}", t.display_name, t.content))
            .collect::<Vec<_>>()
            .join("\n");
        (transcript, m.topic.clone())
    };

    let system_prompt = format!(
        "You are '{}' in a chat-style meeting. Topic: '{}'\n\nTranscript so far:\n{}\n\n\
         Reply if you have something useful to add. Return an empty response to stay silent.",
        participant_id.0, topic, transcript_text
    );

    let params = CreateMessageRequestParams::new(
        vec![SamplingMessage::user_text(
            "Your reply (empty = stay silent).",
        )],
        4096,
    )
    .with_system_prompt(system_prompt);

    {
        let mut m = meeting.lock().await;
        m.mark_responding(participant_id);
    }

    let response = match tokio::time::timeout(
        std::time::Duration::from_secs(SAMPLING_COMPOSE_TIMEOUT_SECS),
        peer.create_message(params),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            tracing::debug!("sampling failed for {}: {e}", participant_id.0);
            meeting.lock().await.clear_responding(participant_id);
            return;
        }
        Err(_) => {
            tracing::debug!("sampling timed out for {}", participant_id.0);
            meeting.lock().await.clear_responding(participant_id);
            return;
        }
    };

    let content: String = response
        .message
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("");

    let trimmed = content.trim();
    if trimmed.is_empty() {
        meeting.lock().await.clear_responding(participant_id);
        return;
    }

    let mut m = meeting.lock().await;
    let _ = m.submit(participant_id, trimmed.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_room_config_uses_username() {
        let config = RoomConfig::default();
        assert!(!config.human_display_name.is_empty());
    }
}
