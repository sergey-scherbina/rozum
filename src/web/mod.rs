use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Extension, Router,
    extract::{
        Query, State, WebSocketUpgrade,
        ws::{Message, WebSocket},
    },
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{any, get},
};
use base64::Engine;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use crate::meeting::room_client::{RoomConnection, tool_result_text_json};
use crate::meeting::room_path::room_socket;

type BridgeResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// In-memory transcript ring kept by the bridge. Each entry is a `msg`
/// envelope (the same shape clients receive over WebSocket). Bounded so
/// long-running rooms do not grow without limit; older entries fall off
/// the front. `web-transcript-persist` (separate slug) lifts this bound
/// by reading from `transcript.jsonl` when the in-memory window is
/// exhausted.
const TRANSCRIPT_CAP: usize = 2000;

#[derive(Clone)]
struct AppState {
    broadcast_tx: broadcast::Sender<String>,
    transcript: Arc<Mutex<Vec<Value>>>,
    /// Path to the room's Unix-domain socket. Each WS connection opens its
    /// own MCP `RoomConnection` against this socket so the authenticated
    /// alias appears in the room as a first-class participant (visible in
    /// the TUI participant list).
    socket_path: PathBuf,
}

#[derive(Deserialize)]
struct TranscriptQuery {
    /// Return entries with `seq >= from_seq`. Omit for "from the start".
    from_seq: Option<u64>,
    /// Maximum number of entries to return. Defaults to 200.
    limit: Option<usize>,
}

pub async fn run_bridge(room: &str, display_name: &str, port: u16) -> BridgeResult<()> {
    run_bridge_with(room, display_name, port, true).await
}

#[derive(Clone)]
struct AuthUser(String);

#[derive(Clone)]
struct AuthConfig {
    password: String,
    realm: String,
}

pub async fn run_bridge_with(
    room: &str,
    display_name: &str,
    port: u16,
    persist: bool,
) -> BridgeResult<()> {
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
    let persist_path = if persist { Some(transcript_path(room)) } else { None };
    let initial = match &persist_path {
        Some(p) => load_persisted_transcript(p),
        None => Vec::new(),
    };
    if !initial.is_empty() {
        eprintln!(
            "[web-bridge] loaded {} persisted transcript entries from {}",
            initial.len(),
            persist_path.as_ref().unwrap().display()
        );
    }
    let transcript = Arc::new(Mutex::new(initial));
    let state = AppState {
        broadcast_tx: broadcast_tx.clone(),
        transcript: Arc::clone(&transcript),
        socket_path: socket_path.clone(),
    };

    let auth_cfg = AuthConfig {
        password: room.to_owned(),
        realm: format!("rozum/{room}"),
    };

    let app = Router::new()
        .route("/", get(index_handler))
        .route("/ws", any(ws_handler))
        .route("/transcript", get(transcript_handler))
        .layer(middleware::from_fn_with_state(auth_cfg, auth_layer))
        .with_state(state.clone());

    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tokio::select! {
        result = axum::serve(listener, app) => {
            result.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        }
        result = room_loop(conn, broadcast_tx, transcript, persist_path, my_id.clone(), display_name.to_owned()) => {
            result?;
        }
    }
    Ok(())
}

fn transcript_path(room: &str) -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join(".local/state"))
                .unwrap_or_else(|| PathBuf::from(".local/state"))
        });
    base.join("rozum").join("rooms").join(room).join("transcript.jsonl")
}

fn load_persisted_transcript(path: &PathBuf) -> Vec<Value> {
    let contents = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<Value> = contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect();
    if out.len() > TRANSCRIPT_CAP {
        let drop = out.len() - TRANSCRIPT_CAP;
        out.drain(0..drop);
    }
    out
}

fn append_persisted(path: &PathBuf, env: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{}", env);
    }
}

async fn transcript_handler(
    State(state): State<AppState>,
    Query(q): Query<TranscriptQuery>,
) -> Json<Value> {
    let from_seq = q.from_seq.unwrap_or(0);
    let limit = q.limit.unwrap_or(200).clamp(1, TRANSCRIPT_CAP);
    let transcript = state.transcript.lock().await;
    let messages: Vec<Value> = transcript
        .iter()
        .filter(|m| m["seq"].as_u64().unwrap_or(0) >= from_seq)
        .take(limit)
        .cloned()
        .collect();
    Json(json!({ "messages": messages }))
}

async fn index_handler() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Extension(user): Extension<AuthUser>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state, user))
}

/// Verify HTTP Basic Auth on every request. Username can be anything (it
/// becomes the participant's alias in the chat); password must equal the
/// room name. Authenticated requests carry the username forward via an
/// `Extension<AuthUser>` so the WS handler can stamp every submitted
/// message with the authenticated alias.
async fn auth_layer(
    State(cfg): State<AuthConfig>,
    mut req: axum::extract::Request,
    next: Next,
) -> Response {
    let unauthorized = |realm: &str| -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                format!("Basic realm=\"{realm}\""),
            )],
            "401 Unauthorized\n",
        )
            .into_response()
    };

    let Some(raw) = req.headers().get(header::AUTHORIZATION) else {
        return unauthorized(&cfg.realm);
    };
    let Ok(s) = raw.to_str() else {
        return unauthorized(&cfg.realm);
    };
    let Some(b64) = s.strip_prefix("Basic ").or_else(|| s.strip_prefix("basic ")) else {
        return unauthorized(&cfg.realm);
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return unauthorized(&cfg.realm);
    };
    let Ok(creds) = std::str::from_utf8(&decoded) else {
        return unauthorized(&cfg.realm);
    };
    let Some((user, pass)) = creds.split_once(':') else {
        return unauthorized(&cfg.realm);
    };
    if pass != cfg.password {
        return unauthorized(&cfg.realm);
    }
    let user = user.trim();
    let user = if user.is_empty() { "web" } else { user };
    req.extensions_mut().insert(AuthUser(user.to_owned()));
    next.run(req).await
}

async fn handle_socket(mut socket: WebSocket, state: AppState, user: AuthUser) {
    // Open a per-WS room connection so this web user is a first-class
    // participant in the room (visible in TUI, agents, etc.) with their
    // authenticated alias as the display name. Outgoing messages from this
    // browser go through this connection, so the room knows the real
    // speaker — no `[<alias>]: ` content prefix is needed.
    let mut user_conn =
        match RoomConnection::connect(&state.socket_path, &user.0, Duration::from_secs(5)).await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[web-bridge] user conn for '{}': {e}", user.0);
                return;
            }
        };
    let join_result = match user_conn
        .call_tool(
            "_join_internal",
            json!({ "client_info_name": user.0, "kind": "web" }),
            Duration::from_secs(5),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[web-bridge] join for '{}': {e}", user.0);
            return;
        }
    };
    let user_pid = tool_result_text_json(&join_result)
        .and_then(|p| p["participant_id"].as_str().map(str::to_owned))
        .unwrap_or_else(|| user.0.clone());

    // Subscribe before snapshotting so we don't miss messages that arrive
    // between snapshot and the start of forwarding. Client deduplicates by
    // seq (any overlap is harmless).
    let mut rx = state.broadcast_tx.subscribe();
    // Tell the client which authenticated alias to display itself as. The
    // client uses this in `me`-styled log entries.
    let hello = json!({ "kind": "hello", "name": user.0 });
    if socket.send(Message::text(hello.to_string())).await.is_err() {
        let _ = user_conn
            .call_tool("meeting.leave", json!({}), Duration::from_secs(2))
            .await;
        return;
    }

    // Web users do not call wait_my_turn (the central bridge does that on
    // their behalf), so the room's polling array never lists them. Emit a
    // synthetic `joined` envelope so the web UI sees them in the header
    // chips immediately.
    let joined = json!({
        "kind": "joined",
        "participant_id": user_pid,
        "display_name": user.0,
    });
    state.broadcast_tx.send(joined.to_string()).ok();
    {
        let transcript = state.transcript.lock().await;
        let take = transcript.len().min(200);
        let messages: Vec<Value> = transcript[transcript.len() - take..].to_vec();
        let env = json!({ "kind": "history", "messages": messages });
        if socket.send(Message::text(env.to_string())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let text = text.as_str().trim().to_owned();
                        if text.is_empty() { continue; }
                        let content = if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                            v["content"].as_str().unwrap_or("").to_owned()
                        } else {
                            text
                        };
                        if content.is_empty() { continue; }
                        // Submit via the per-WS connection so the message is
                        // attributed to the authenticated alias by the room.
                        let _ = user_conn
                            .call_tool(
                                "meeting.submit",
                                serde_json::json!({ "content": content }),
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

    // WS closed: leave the room as this user so the participant disappears
    // from the TUI and agents immediately. Also broadcast a synthetic
    // `left` envelope so other web clients update their UI without waiting
    // for the central polling-based detection.
    let _ = user_conn
        .call_tool("meeting.leave", json!({}), Duration::from_secs(2))
        .await;
    let env = json!({
        "kind": "left",
        "participant_id": user_pid,
        "display_name": user.0,
    });
    state.broadcast_tx.send(env.to_string()).ok();
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
    transcript: Arc<Mutex<Vec<Value>>>,
    persist_path: Option<PathBuf>,
    bridge_pid: String,
    bridge_display_name: String,
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
        // first time we see a participant. Skip the bridge's own
        // participant — it is a transport, not a user.
        for entry in polling_arr.iter().chain(responding_arr.iter()) {
            let pid = match entry["participant_id"].as_str() {
                Some(s) => s.to_owned(),
                None => continue,
            };
            if pid == bridge_pid {
                continue;
            }
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

        // Emit `presence` only when polling/responding diff. The bridge's
        // own participant is filtered out so the web UI never sees "web"
        // as typing or waiting.
        if new_polling != last_polling || new_responding != last_responding {
            let env = json!({
                "kind": "presence",
                "responding": responding_arr
                    .iter()
                    .filter(|e| e["participant_id"].as_str() != Some(bridge_pid.as_str()))
                    .map(presence_entry)
                    .collect::<Vec<_>>(),
                "polling": polling_arr
                    .iter()
                    .filter(|e| e["participant_id"].as_str() != Some(bridge_pid.as_str()))
                    .map(presence_entry)
                    .collect::<Vec<_>>(),
            });
            broadcast_tx.send(env.to_string()).ok();
            last_polling = new_polling;
            last_responding = new_responding;
        }

        // Emit `msg` for each transcript delta entry, and append to the
        // in-memory transcript so later WebSocket connects can replay it.
        for entry in transcript_delta {
            let raw_speaker = entry["display_name"].as_str().unwrap_or("?");
            let raw_content = entry["content"].as_str().unwrap_or("").trim();
            if raw_content.is_empty() {
                continue;
            }
            // Messages submitted by the bridge carry a `[<alias>]: ` prefix
            // identifying the web user. Promote the alias to `speaker` so
            // the UI shows the human's name instead of the bridge's name.
            let (speaker, content) = if raw_speaker == bridge_display_name {
                strip_alias_prefix(raw_content)
                    .map(|(a, b)| (a.to_owned(), b.to_owned()))
                    .unwrap_or_else(|| (raw_speaker.to_owned(), raw_content.to_owned()))
            } else {
                (raw_speaker.to_owned(), raw_content.to_owned())
            };
            let env = json!({
                "kind":      "msg",
                "speaker":   speaker,
                "content":   content,
                "injected":  entry["injected"].as_bool().unwrap_or(false),
                "seq":       entry["seq"].as_u64().unwrap_or(0),
                "ts":        entry["ts"].as_u64().unwrap_or(0),
            });
            {
                let mut t = transcript.lock().await;
                t.push(env.clone());
                if t.len() > TRANSCRIPT_CAP {
                    let excess = t.len() - TRANSCRIPT_CAP;
                    t.drain(0..excess);
                }
            }
            if let Some(p) = &persist_path {
                append_persisted(p, &env);
            }
            broadcast_tx.send(env.to_string()).ok();
        }
    }
}

/// Parse a `[<alias>]: <body>` prefix into `(alias, body)`. Returns `None`
/// if the content does not start with `[`, lacks `]: `, or `alias` is empty.
fn strip_alias_prefix(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix('[')?;
    let close = rest.find("]: ")?;
    let alias = &rest[..close];
    if alias.is_empty() {
        return None;
    }
    let body = &rest[close + 3..];
    Some((alias, body))
}

fn presence_entry(v: &Value) -> Value {
    json!({
        "participant_id": v["participant_id"].as_str().unwrap_or(""),
        "display_name":   v["display_name"].as_str().unwrap_or(""),
        "age_ms":         v["age_ms"].as_u64().unwrap_or(0),
    })
}
