//! Daemon-backed human **web client** for meeting rooms — a browser front-end equal to the
//! TUI (the legacy `crate::web` bridges the in-process room; this reads the *daemon's* on-disk
//! transcript and submits through the daemon). Gated behind a single shared secret.
//!
//! `rozum meetings web [--port P] [--room name] [--bind addr]`:
//! - `GET /`            — the chat page (history + live + an input box).
//! - `GET /api/messages`— the room transcript, read directly from disk.
//! - `POST /api/submit` — post a message (as the local human identity).
//! - `GET /api/stream`  — Server-Sent Events tailing the room → new messages appear live (the
//!   **wakeup**: when an agent posts, the browser sees it without reloading).
//!
//! Auth: HTTP Basic, password = the shared secret (`ROZUM_WEB_SECRET`, else generated + printed).
//! The username field is ignored — the secret is the gate; posts use the operator's identity.
//! First equal non-TUI client; P3 groundwork. See `docs/specs/agent-meeting-coordination.md`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response, Sse, sse::Event},
    routing::{get, post},
};
use base64::Engine;
use futures::Stream;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast};

use super::daemon::daemon_alive;
use super::daemon_proxy::{detect_project, spawn_daemon};
use super::room_path::meeting_sock;
use super::store::{self, StoredTurn};
use super::tui_client::MeetingClient;

type WebResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Clone)]
struct WebState {
    /// One persistent daemon client (the operator's identity) — all web posts go through it,
    /// so the room shows a single stable web participant.
    client: Arc<Mutex<MeetingClient>>,
    /// The joined room's on-disk dir (read the transcript from here).
    root: PathBuf,
    /// New-message fan-out to SSE subscribers (the tail task feeds it).
    tx: broadcast::Sender<String>,
    room: String,
}

#[derive(Clone)]
struct AuthCfg {
    secret: String,
}

#[derive(Deserialize)]
struct SubmitBody {
    content: String,
}

/// Serve the web client for a room (`room` = a named room, else the cwd project room),
/// bound to `bind:port`, gated by `secret`. Runs until the process is stopped.
pub async fn run_web(
    room: Option<String>,
    port: u16,
    bind: String,
    secret: String,
) -> WebResult<()> {
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        spawn_daemon().await;
    }
    let id = super::local_identity::load_or_create();
    let mut client = MeetingClient::connect_as(&sock, &id.display, &id.token)
        .await
        .map_err(|e| format!("connect daemon: {e}"))?;
    let room_name = match &room {
        Some(name) => client
            .enter_or_create(name)
            .await
            .map_err(|e| format!("join '{name}': {e}"))?,
        None => {
            let project = detect_project().ok_or("no project (run inside a repo, or pass --room)")?;
            client
                .enter_project(&project)
                .await
                .map_err(|e| format!("join project room: {e}"))?
        }
    };
    let root = client.room_root().ok_or("no room root after join")?.to_path_buf();

    let (tx, _) = broadcast::channel::<String>(256);
    let state = WebState {
        client: Arc::new(Mutex::new(client)),
        root: root.clone(),
        tx: tx.clone(),
        room: room_name.clone(),
    };

    // Tail the room transcript on disk → fan out new turns to SSE clients (the wakeup). Starts
    // at the current head so the stream carries only what arrives after page load; /api/messages
    // serves history. Reads are cheap (seek-from-cursor of small day files).
    {
        let root = root.clone();
        tokio::spawn(async move {
            let mut cursor = store::read_since(&root, None, 0)
                .last()
                .map(|t| (t.date.clone(), t.n + 1));
            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let (sd, sn) = match &cursor {
                    Some((d, n)) => (Some(d.as_str()), *n),
                    None => (None, 0),
                };
                let turns = store::read_since(&root, sd, sn);
                if let Some(last) = turns.last() {
                    cursor = Some((last.date.clone(), last.n + 1));
                    for t in &turns {
                        let _ = tx.send(turn_json(t).to_string());
                    }
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/messages", get(messages))
        .route("/api/submit", post(submit))
        .route("/api/stream", get(stream))
        .layer(middleware::from_fn_with_state(
            AuthCfg { secret },
            auth_layer,
        ))
        .with_state(state);

    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| format!("bad bind '{bind}:{port}': {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!("rozum web: room '{room_name}' on http://{addr}  (log in with the secret as the password)");
    axum::serve(listener, app)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

fn turn_json(t: &StoredTurn) -> Value {
    json!({ "from": t.display_name, "content": t.content, "date": t.date, "n": t.n, "ts": t.ts })
}

async fn index() -> Html<&'static str> {
    Html(include_str!("web_index.html"))
}

async fn messages(State(s): State<WebState>) -> Json<Value> {
    let msgs: Vec<Value> = store::read_since(&s.root, None, 0).iter().map(turn_json).collect();
    Json(json!({ "room": s.room, "messages": msgs }))
}

async fn submit(State(s): State<WebState>, Json(b): Json<SubmitBody>) -> Response {
    let content = b.content.trim();
    if content.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message").into_response();
    }
    let mut client = s.client.lock().await;
    match client.submit(content).await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("submit failed: {e}")).into_response(),
    }
}

async fn stream(State(s): State<WebState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = s.tx.subscribe();
    let body = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(json) => yield Ok(Event::default().data(json)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(body).keep_alive(axum::response::sse::KeepAlive::default())
}

/// HTTP Basic auth: any username, password must equal the shared secret. The browser's native
/// credential prompt is the "enter the secret code" UX; once entered it's cached for the origin
/// (so `fetch` + `EventSource` carry it automatically).
async fn auth_layer(State(cfg): State<AuthCfg>, req: axum::extract::Request, next: Next) -> Response {
    let unauth = || -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"rozum meeting\"")],
            "401 Unauthorized\n",
        )
            .into_response()
    };
    let Some(h) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return unauth();
    };
    let Some(b64) = h.strip_prefix("Basic ").or_else(|| h.strip_prefix("basic ")) else {
        return unauth();
    };
    let Ok(dec) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return unauth();
    };
    let pass = std::str::from_utf8(&dec)
        .ok()
        .and_then(|c| c.split_once(':').map(|(_, p)| p.to_owned()))
        .unwrap_or_default();
    if pass != cfg.secret {
        return unauth();
    }
    next.run(req).await
}
