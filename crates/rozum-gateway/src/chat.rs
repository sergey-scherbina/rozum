//! The console's chat pane: read a meeting room, post to it, stream a model's reply.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Thirteen items, including the room-name
//! guard and the two readers that go straight to the room files on disk — those came along because
//! nothing else calls them, which is the measurement that made this a subject rather than a
//! scattering of routes.

use std::path::PathBuf;

use serde::Deserialize;

use crate::errors::json_err;
use crate::paths::state_dir;
use crate::gateway_control::ensure_gateway;
use crate::wire_body::*;

pub(crate) async fn chat_messages_route(
    axum::extract::Query(q): axum::extract::Query<RoomQuery>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    if !valid_room_name(&q.room) { return StatusCode::BAD_REQUEST.into_response(); }
    axum::Json(read_room_messages(&q.room, 80)).into_response()
}

pub(crate) async fn chat_incidents_route(
    axum::extract::Query(q): axum::extract::Query<RoomQuery>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    if !valid_room_name(&q.room) { return StatusCode::BAD_REQUEST.into_response(); }
    axum::Json(read_room_incidents(&q.room)).into_response()
}

pub(crate) async fn chat_post_route(
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    let room = headers
        .get("X-Room")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if !valid_room_name(&room) { return StatusCode::BAD_REQUEST.into_response(); }
    // Proxy to the meeting daemon at :8405/p.
    let client = reqwest::Client::new();
    match client
        .post("http://127.0.0.1:8405/p")
        .header("X-Room", &room)
        .header("Content-Type", "text/plain")
        .body(body.to_vec())
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => StatusCode::OK.into_response(),
        Ok(r) => (StatusCode::BAD_GATEWAY, r.status().as_str().to_string()).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, e.to_string()).into_response(),
    }
}

/// System prompt for the conversational "Собеседник" mode: tells the local model who it is and what
/// rozum is, so a plain question ("О чём проект?") gets a good answer from context WITHOUT the agentic
/// repo exploration. Kept short — a 4B follows a crisp instruction; a heavy blob degrades it.
pub(crate) const ROZUM_CHAT_SYSTEM: &str = "Ты — ассистент rozum, работающий ЛОКАЛЬНО на Mac пользователя: ты \
модель (Qwen), которую обслуживает локальный гейтвей rozum, и пользователь пишет тебе с телефона. \
rozum — это local-first система, чтобы запускать LLM и ИИ-агентов на своём железе (Apple Silicon / \
MLX): локальный OpenAI/Anthropic-совместимый гейтвей для MLX и GGUF моделей; комнаты-встречи, где \
ИИ-агенты и люди координируются; телефонный контрол-центр (UCC) с этим чатом; безопасная резидентность \
нескольких моделей (контроль допуска, чтобы модели не переполняли память). Отвечай в диалоге, кратко и \
по делу, на языке пользователя. Ты сейчас именно БЕСЕДУЕШЬ, а не выполняешь задачи в проекте — если \
просят что-то СДЕЛАТЬ в проекте (править файлы, запускать команды), скажи переключиться в режим «Агент».";

#[derive(Deserialize)]
pub(crate) struct ChatMsgIn {
    pub(crate) role: String,
    pub(crate) content: String,
}

#[derive(Deserialize)]
pub(crate) struct ChatStreamReq {
    pub(crate) model: String,
    pub(crate) messages: Vec<ChatMsgIn>,
    #[serde(default)]
    pub(crate) max_tokens: Option<u32>,
}

/// Conversational chat: forward the phone's message history to the resident model's
/// `/v1/chat/completions` with `stream:true` and pipe the SSE straight back — token-by-token, no
/// agent, no repo exploration, so it can never hang the way a 40-turn `claude -p` can. Prepends
/// [`ROZUM_CHAT_SYSTEM`] so the model knows what rozum is.
pub(crate) async fn chat_stream_route(body: String) -> axum::response::Response {
    let req: ChatStreamReq = match parse_action_json(&body) {
        Ok(r) => r,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    if req.model.trim().is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model required");
    }
    // The reactive client's stream primitive fires one POST at page mount with an empty
    // `messages` (the body only carries the conversation while a send is in flight). Treat that
    // as a graceful no-op — an immediately-terminated SSE stream — rather than a 400, so the
    // mount-fire leaves no error in the log and never reaches the model.
    if req.messages.is_empty() {
        return match axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-store")
            .body(axum::body::Body::from("data: [DONE]\n\n"))
        {
            Ok(r) => r,
            Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }
    let port = match ensure_gateway(&req.model).await {
        Ok(p) => p,
        Err(e) => return json_err(axum::http::StatusCode::SERVICE_UNAVAILABLE, &e),
    };
    let mut messages = vec![serde_json::json!({"role": "system", "content": ROZUM_CHAT_SYSTEM})];
    for m in &req.messages {
        // Only user/assistant turns pass through; ignore any stray roles from the client.
        let role = if m.role == "user" { "user" } else { "assistant" };
        messages.push(serde_json::json!({"role": role, "content": m.content}));
    }
    let upstream = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "max_tokens": req.max_tokens.unwrap_or(1024),
    });
    let resp = match reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&upstream)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return json_err(axum::http::StatusCode::BAD_GATEWAY, &format!("gateway: {e}")),
    };
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return json_err(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!("gateway {s}: {}", t.chars().take(200).collect::<String>()),
        );
    }
    // Pipe the upstream OpenAI SSE bytes straight through to the phone.
    match axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(axum::body::Body::from_stream(resp.bytes_stream()))
    {
        Ok(r) => r,
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

pub(crate) fn valid_room_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(|c| matches!(c, '\r' | '\n' | '\0' | '/'))
}

pub(crate) fn rooms_json_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("rooms.json"))
}

pub(crate) fn room_root(name: &str) -> Option<PathBuf> {
    let path = rooms_json_path()?;
    let val: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
    let rooms = match &val {
        serde_json::Value::Array(a) => a.clone(),
        serde_json::Value::Object(o) => o.get("rooms").and_then(|v| v.as_array()).cloned().unwrap_or_default(),
        _ => return None,
    };
    for r in &rooms {
        if r.get("name").and_then(|v| v.as_str()) == Some(name) {
            return r.get("root").and_then(|v| v.as_str()).map(PathBuf::from);
        }
    }
    None
}

#[derive(serde::Serialize)]
pub(crate) struct ChatMessage { time: String, author: String, content: String }

pub(crate) fn read_room_messages(room: &str, limit: usize) -> Vec<ChatMessage> {
    let Some(root) = room_root(room) else { return vec![]; };
    if !root.is_dir() { return vec![]; }
    let mut files: Vec<_> = std::fs::read_dir(&root)
        .into_iter().flatten().filter_map(|e| e.ok()).map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    let mut out = vec![];
    for fp in &files {
        let Ok(text) = std::fs::read_to_string(fp) else { continue; };
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() { continue; }
            let Ok(m) = serde_json::from_str::<serde_json::Value>(line) else { continue; };
            let Some(content) = m.get("content").and_then(|v| v.as_str()) else { continue; };
            let author = m.get("display_name")
                .or_else(|| m.get("author"))
                .and_then(|v| v.as_str()).unwrap_or("?");
            let ts = m.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
            let time = { let h = (ts / 3600) % 24; let min = (ts / 60) % 60; format!("{h:02}:{min:02}") };
            out.push(ChatMessage { time, author: author.to_string(), content: content.to_string() });
        }
    }
    if out.len() > limit { out.drain(..out.len() - limit); }
    out
}

pub(crate) fn read_room_incidents(room: &str) -> Vec<Incident> {
    let Some(root) = room_root(room) else { return vec![]; };
    let Ok(bytes) = std::fs::read(root.join("threads.json")) else { return vec![]; };
    let Ok(serde_json::Value::Array(arr)) = serde_json::from_slice::<serde_json::Value>(&bytes) else { return vec![]; };
    arr.iter().filter_map(|t| {
        Some(Incident {
            title:    t.get("title")?.as_str()?.to_string(),
            state:    t.get("state").and_then(|v| v.as_str()).unwrap_or("open").to_string(),
            severity: t.get("severity").and_then(|v| v.as_str()).unwrap_or("low").to_string(),
            owner:    t.get("owner").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        })
    }).collect()
}

#[derive(serde::Deserialize)]
pub(crate) struct RoomQuery { room: String }

#[derive(serde::Serialize)]
pub(crate) struct Incident { severity: String, state: String, title: String, owner: String }

