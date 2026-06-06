use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    response::{Html, IntoResponse},
    routing::{any, get},
};
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::meeting::room_client::{RoomConnection, tool_result_text_json};
use crate::meeting::room_path::room_socket;

type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
struct AppState {
    conn: Arc<Mutex<RoomConnection>>,
    broadcast_tx: broadcast::Sender<String>,
}

pub async fn run_bridge(room: &str, display_name: &str, port: u16) -> BridgeResult<()> {
    let socket_path = room_socket(room);
    if !socket_path.exists() {
        return Err(format!("room not found: {room}").into());
    }

    let mut conn = RoomConnection::connect(&socket_path, display_name, Duration::from_secs(5))
        .await
        .map_err(|e| format!("connect to room: {e}"))?;

    let join_result = conn
        .call_tool(
            "_join_internal",
            serde_json::json!({ "client_info_name": display_name, "kind": "bridge" }),
            Duration::from_secs(5),
        )
        .await
        .map_err(|e| format!("join room: {e}"))?;

    let join_payload = tool_result_text_json(&join_result).ok_or("invalid join response")?;
    let my_id = join_payload["participant_id"]
        .as_str()
        .unwrap_or(display_name)
        .to_owned();
    eprintln!(
        "[web-bridge] joined room '{room}' as '{my_id}', listening on http://localhost:{port}"
    );

    let (broadcast_tx, _) = broadcast::channel::<String>(64);
    let conn = Arc::new(Mutex::new(conn));
    let state = AppState {
        conn: Arc::clone(&conn),
        broadcast_tx: broadcast_tx.clone(),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", any(ws_handler))
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tokio::select! {
        result = axum::serve(listener, app) => {
            result.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        result = room_loop(conn, broadcast_tx) => {
            result?;
        }
    }
    Ok(())
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let mut rx = state.broadcast_tx.subscribe();
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let text = text.as_str().trim().to_owned();
                        if text.is_empty() { continue; }
                        let (sender, content) =
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                                (
                                    v["name"].as_str().unwrap_or("web").to_owned(),
                                    v["content"].as_str().unwrap_or("").to_owned(),
                                )
                            } else {
                                ("web".to_owned(), text)
                            };
                        if content.is_empty() { continue; }
                        let payload = format!("[{}]: {}", sender, content);
                        let mut conn = state.conn.lock().await;
                        let _ = conn
                            .call_tool(
                                "meeting.submit",
                                serde_json::json!({ "content": payload }),
                                Duration::from_secs(5),
                            )
                            .await;
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    _ => {}
                }
            }
            Ok(broadcast_msg) = rx.recv() => {
                if socket.send(Message::text(broadcast_msg)).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Listen for new transcript entries and presence changes via wait_my_turn
/// long-polls. Re-broadcasts tagged JSON envelopes to all WebSocket clients:
///
/// * `{"kind":"msg",      "speaker", "content", "injected", "seq", "ts"}`
/// * `{"kind":"presence", "responding":[...], "polling":[...]}`
/// * `{"kind":"joined",   "participant_id", "display_name"}`
/// * `{"kind":"left",     "participant_id", "display_name"}`
///
/// Presence envelopes are emitted only when polling/responding diff against
/// the last snapshot. Joined / left are derived from per-participant
/// last-seen timestamps with a 60 s absence threshold.
async fn room_loop(
    conn: Arc<Mutex<RoomConnection>>,
    broadcast_tx: broadcast::Sender<String>,
) -> BridgeResult<()> {
    let mut since_seq: usize = 0;
    let mut last_polling: HashSet<String> = HashSet::new();
    let mut last_responding: HashSet<String> = HashSet::new();
    // participant_id -> (display_name, last_seen_unix_seconds)
    let mut last_seen: HashMap<String, (String, u64)> = HashMap::new();
    const LEFT_AFTER_SECS: u64 = 60;

    loop {
        let result = {
            let mut c = conn.lock().await;
            c.call_tool(
                "meeting.wait_my_turn",
                serde_json::json!({ "since_seq": since_seq }),
                Duration::from_secs(35),
            )
            .await
            .map_err(|e| format!("wait_my_turn: {e}"))?
        };

        let payload = tool_result_text_json(&result).ok_or("invalid wait_my_turn response")?;

        if payload["ended"].as_bool() == Some(true) {
            eprintln!("[web-bridge] room ended");
            return Ok(());
        }

        let (polling_arr, responding_arr, seq_in_payload, transcript_delta) =
            if payload["still_waiting"].as_bool() == Some(true) {
                (
                    payload["polling"].as_array().cloned().unwrap_or_default(),
                    payload["responding"].as_array().cloned().unwrap_or_default(),
                    payload["seq"].as_u64(),
                    Vec::new(),
                )
            } else {
                let turn = &payload["turn"];
                (
                    turn["polling"].as_array().cloned().unwrap_or_default(),
                    turn["responding"].as_array().cloned().unwrap_or_default(),
                    turn["seq"].as_u64(),
                    turn["transcript_delta"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default(),
                )
            };

        if let Some(seq) = seq_in_payload {
            since_seq = seq as usize;
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let new_polling: HashSet<String> = polling_arr
            .iter()
            .filter_map(|e| e["participant_id"].as_str().map(str::to_owned))
            .collect();
        let new_responding: HashSet<String> = responding_arr
            .iter()
            .filter_map(|e| e["participant_id"].as_str().map(str::to_owned))
            .collect();

        // Refresh last_seen for anyone currently active. Emit `joined` the
        // first time we see a participant.
        for entry in polling_arr.iter().chain(responding_arr.iter()) {
            let pid = match entry["participant_id"].as_str() {
                Some(s) => s.to_owned(),
                None => continue,
            };
            let name = entry["display_name"]
                .as_str()
                .unwrap_or(&pid)
                .to_owned();
            let first_seen = !last_seen.contains_key(&pid);
            last_seen.insert(pid.clone(), (name.clone(), now));
            if first_seen {
                let env = json!({
                    "kind": "joined",
                    "participant_id": pid,
                    "display_name": name,
                });
                broadcast_tx.send(env.to_string()).ok();
            }
        }

        // Emit `left` for participants whose last-seen is older than the
        // threshold and who are not currently polling or responding.
        let mut to_drop: Vec<String> = Vec::new();
        for (pid, (name, ts)) in last_seen.iter() {
            if new_polling.contains(pid) || new_responding.contains(pid) {
                continue;
            }
            if now.saturating_sub(*ts) >= LEFT_AFTER_SECS {
                let env = json!({
                    "kind": "left",
                    "participant_id": pid,
                    "display_name": name,
                });
                broadcast_tx.send(env.to_string()).ok();
                to_drop.push(pid.clone());
            }
        }
        for pid in to_drop {
            last_seen.remove(&pid);
        }

        // Emit `presence` only when polling/responding diff.
        if new_polling != last_polling || new_responding != last_responding {
            let env = json!({
                "kind": "presence",
                "responding": responding_arr.iter().map(presence_entry).collect::<Vec<_>>(),
                "polling":    polling_arr.iter().map(presence_entry).collect::<Vec<_>>(),
            });
            broadcast_tx.send(env.to_string()).ok();
            last_polling = new_polling;
            last_responding = new_responding;
        }

        // Emit `msg` for each transcript delta entry.
        for entry in transcript_delta {
            let content = entry["content"].as_str().unwrap_or("").trim();
            if content.is_empty() {
                continue;
            }
            let env = json!({
                "kind":      "msg",
                "speaker":   entry["display_name"].as_str().unwrap_or("?"),
                "content":   content,
                "injected":  entry["injected"].as_bool().unwrap_or(false),
                "seq":       entry["seq"].as_u64().unwrap_or(0),
                "ts":        entry["ts"].as_u64().unwrap_or(0),
            });
            broadcast_tx.send(env.to_string()).ok();
        }
    }
}

fn presence_entry(v: &Value) -> Value {
    json!({
        "participant_id": v["participant_id"].as_str().unwrap_or(""),
        "display_name":   v["display_name"].as_str().unwrap_or(""),
        "age_ms":         v["age_ms"].as_u64().unwrap_or(0),
    })
}
