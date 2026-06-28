//! Read-only HTTP API for daemon-backed meeting transcripts.
//!
//! This is intentionally smaller than `meetings web`: no UI, no submit path, no
//! SSE, and no room mutation. It resolves rooms through the daemon registry and
//! reads day files directly from disk, gated by the same Basic-auth shared secret
//! pattern as the web client.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Extension, Router,
    extract::{Path as AxumPath, Query, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
};
use serde_json::Value;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::registry::RoomRegistry;
use super::store::{self, Index};

const DEFAULT_COUNT: u64 = 100;
const MAX_COUNT: u64 = 500;
const DEFAULT_BIND: &str = "127.0.0.1:8401";

#[derive(Clone)]
struct RestState {
    registry: Arc<RoomRegistry>,
}

#[derive(Clone)]
struct AuthCfg {
    secret: String,
}

#[derive(Deserialize)]
struct PageQuery {
    from: Option<u64>,
    count: Option<u64>,
}

#[derive(Deserialize)]
struct SearchQuery {
    q: Option<String>,
    kind: Option<String>,
    severity: Option<String>,
    tag: Option<String>,
    thread: Option<String>,
    since: Option<String>,
    limit: Option<u64>,
}

#[derive(Serialize)]
struct DayJson {
    date: String,
    count: u64,
    bytes: u64,
}

/// Spawn the REST read listener when configured. A bind failure is logged but
/// does not abort the meeting daemon; the unix-socket MCP path is primary.
pub fn maybe_spawn_from_env(registry: Arc<RoomRegistry>, shutdown: watch::Receiver<bool>) {
    let Some(secret) = std::env::var("ROZUM_WEB_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let bind = std::env::var("ROZUM_MEETINGS_REST_BIND")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BIND.to_string());

    tokio::spawn(async move {
        let listener = match TcpListener::bind(&bind).await {
            Ok(l) => l,
            Err(e) => {
                tracing::warn!(bind, error = ?e, "meeting REST read bind failed");
                return;
            }
        };
        let addr = listener.local_addr().ok();
        tracing::info!(?addr, "meeting REST read listening");
        if let Err(e) = serve(listener, registry, secret, shutdown).await {
            tracing::warn!(error = ?e, "meeting REST read stopped with error");
        }
    });
}

pub async fn serve(
    listener: TcpListener,
    registry: Arc<RoomRegistry>,
    secret: String,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(registry, secret);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            loop {
                if *shutdown.borrow() {
                    break;
                }
                if shutdown.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
}

fn router(registry: Arc<RoomRegistry>, secret: String) -> Router {
    Router::new()
        .route("/", get(console))
        .route("/rooms", get(rooms))
        .route("/rooms/{name}/days", get(days))
        .route("/rooms/{name}/messages/{date}", get(messages))
        .route("/rooms/{name}/inbox/{handle}", get(inbox))
        .route("/rooms/{name}/threads", get(threads).post(thread_open))
        .route("/rooms/{name}/threads/{id}", get(thread_one))
        .route("/rooms/{name}/threads/{id}/escalate", post(thread_escalate))
        .route("/rooms/{name}/threads/{id}/resolve", post(thread_resolve))
        .route("/rooms/{name}/threads/{id}/state", post(thread_state))
        .route("/rooms/{name}/messages", post(submit))
        .route("/rooms/{name}/metrics", get(metrics))
        .route("/rooms/{name}/search", get(search))
        .route("/roster", get(roster))
        .layer(middleware::from_fn_with_state(
            AuthCfg { secret },
            auth_layer,
        ))
        .with_state(RestState { registry })
}

/// `GET /` — the support console (single-page incident dashboard). Static HTML;
/// it reads `?room=<name>` from its own URL and drives the JSON endpoints (GET to
/// read, POST to act — escalate / resolve / open / compose) with the Basic-auth
/// credentials the browser already holds.
async fn console() -> Html<&'static str> {
    Html(include_str!("console.html"))
}

/// `GET /rooms` — the registered rooms (so the console can offer a picker).
async fn rooms(State(state): State<RestState>) -> Response {
    let names: Vec<String> = state
        .registry
        .list()
        .into_iter()
        .map(|loc| loc.name)
        .collect();
    Json(json!({ "rooms": names })).into_response()
}

/// `GET /rooms/{name}/threads` — the incident/topic threads + a metrics summary,
/// the support console's left rail. Reads `threads.json` directly from disk.
async fn threads(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let threads: Vec<store::Thread> = store::read_threads(&root).into_values().collect();
    Json(json!({
        "room": name,
        "count": threads.len(),
        "threads": threads,
        "metrics": store::thread_metrics(&root),
    }))
    .into_response()
}

/// `GET /rooms/{name}/threads/{id}` — one incident's whole picture (thread record
/// + every message in it + participants + timespan). `{id}` is a `<date>/<n>`
/// message id, URL-encoded by the client (the `/` becomes `%2F`).
async fn thread_one(
    State(state): State<RestState>,
    AxumPath((name, id)): AxumPath<(String, String)>,
) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    Json(store::thread_context(&root, &id)).into_response()
}

/// `GET /rooms/{name}/metrics` — resolving metrics (totals / by-state / MTTR).
async fn metrics(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let mut out = store::thread_metrics(&root);
    out["room"] = json!(name);
    Json(out).into_response()
}

/// `GET /rooms/{name}/search?q=&kind=&severity=&tag=&thread=&since=&limit=` — full-history message
/// search (`mtg-message-ops`). `severity` is a MIN (that level and above). Unparseable kind/severity
/// values are a 400 so a typo isn't silently ignored.
async fn search(
    State(state): State<RestState>,
    AxumPath(name): AxumPath<String>,
    Query(q): Query<SearchQuery>,
) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let kind = match q.kind.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match store::MsgKind::parse(s) {
            Some(k) => Some(k),
            None => return (StatusCode::BAD_REQUEST, format!("bad kind: {s}\n")).into_response(),
        },
        None => None,
    };
    let min_severity = match q.severity.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => match store::Severity::parse(s) {
            Some(v) => Some(v),
            None => return (StatusCode::BAD_REQUEST, format!("bad severity: {s}\n")).into_response(),
        },
        None => None,
    };
    let filter = store::MsgFilter {
        text: q.q.as_deref().filter(|s| !s.is_empty()),
        kind,
        min_severity,
        tag: q.tag.as_deref().filter(|s| !s.is_empty()),
        thread_id: q.thread.as_deref().filter(|s| !s.is_empty()),
        since_date: q.since.as_deref().filter(|s| !s.is_empty()),
    };
    let limit = q.limit.unwrap_or(DEFAULT_COUNT).min(MAX_COUNT) as usize;
    let hits = store::search_messages(&root, &filter, limit);
    Json(json!({ "room": name, "count": hits.len(), "messages": hits })).into_response()
}

/// `GET /rooms/{name}/inbox/{handle}` — turns that ADDRESS `handle` (`@h`/`-> h`). Returns ALL such
/// turns; a remote client tracks its own seen-state (the local cursor is a CLI-local concern).
async fn inbox(
    State(state): State<RestState>,
    AxumPath((name, handle)): AxumPath<(String, String)>,
) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let mentions = super::client::inbox(&root, &handle, true);
    Json(json!({ "room": name, "handle": handle, "count": mentions.len(), "messages": mentions }))
        .into_response()
}

/// `GET /roster` — the live agent principals (handle → session/cwd/started/ts), most-recent first.
async fn roster() -> Response {
    Json(json!({ "agents": super::client::roster() })).into_response()
}

async fn days(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    Json(json!({ "room": name, "days": day_listing(&root) })).into_response()
}

async fn messages(
    State(state): State<RestState>,
    AxumPath((name, date)): AxumPath<(String, String)>,
    Query(q): Query<PageQuery>,
) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let from = q.from.unwrap_or(0);
    let count = q.count.unwrap_or(DEFAULT_COUNT).min(MAX_COUNT);
    let probe = count.saturating_add(1);
    let mut turns = match store::read_day(&root, &date, from, Some(probe)) {
        Ok(t) => t,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read day failed: {e}\n"),
            )
                .into_response();
        }
    };
    let has_more = turns.len() as u64 > count;
    if has_more {
        turns.truncate(count as usize);
    }
    let next_from = turns.last().map(|t| t.n + 1).unwrap_or(from);
    Json(json!({
        "room": name,
        "date": date,
        "from": from,
        "count": turns.len(),
        "next_from": next_from,
        "has_more": has_more,
        "messages": turns,
    }))
    .into_response()
}

fn room_root(registry: &RoomRegistry, name: &str) -> Option<PathBuf> {
    // A room name is not guaranteed unique (two projects can derive the same basename). Among
    // same-name entries, prefer one whose root still exists on disk, so a stale registration (a
    // deleted/moved project) never shadows the live room. See `mtg-registry-dup-name`.
    let matches: Vec<PathBuf> = registry
        .list()
        .into_iter()
        .filter(|loc| loc.name == name)
        .map(|loc| loc.root)
        .collect();
    matches
        .iter()
        .find(|r| r.exists())
        .cloned()
        .or_else(|| matches.into_iter().next_back())
}

fn day_listing(root: &Path) -> Vec<DayJson> {
    if let Ok(bytes) = std::fs::read(root.join("index.json")) {
        if let Ok(index) = serde_json::from_slice::<Index>(&bytes) {
            let days: Vec<_> = index
                .days
                .into_iter()
                .map(|(date, stat)| DayJson {
                    date,
                    count: stat.count,
                    bytes: stat.bytes,
                })
                .collect();
            if !days.is_empty() {
                return days;
            }
        }
    }

    store::day_dates(root)
        .into_iter()
        .map(|date| {
            let path = root.join(format!("{date}.jsonl"));
            let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            let count = store::read_day(root, &date, 0, None)
                .map(|turns| turns.len() as u64)
                .unwrap_or(0);
            DayJson { date, count, bytes }
        })
        .collect()
}

async fn auth_layer(
    State(cfg): State<AuthCfg>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
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
    let Some(b64) = h
        .strip_prefix("Basic ")
        .or_else(|| h.strip_prefix("basic "))
    else {
        return unauth();
    };
    let Ok(dec) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return unauth();
    };
    let creds = std::str::from_utf8(&dec).ok().and_then(|c| c.split_once(':'));
    let (user, pass) = match creds {
        Some((u, p)) => (u.to_owned(), p.to_owned()),
        None => return unauth(),
    };
    if pass != cfg.secret {
        return unauth();
    }
    // The Basic-auth username becomes the console actor for write attribution (the daemon still
    // mints/uses a participant for it). Empty username → a generic console identity.
    let actor = if user.is_empty() { "console".to_string() } else { user };
    let mut req = req;
    req.extensions_mut().insert(ConsoleUser(actor));
    next.run(req).await
}

/// The authenticated console actor (the Basic-auth username), attached by `auth_layer` and used to
/// attribute write actions (open/escalate/resolve/submit) when the console drives the daemon.
#[derive(Clone)]
struct ConsoleUser(String);

/// Drive a room MCP tool on behalf of the console user (the in-process REST server connects to the
/// daemon's own socket as an MCP client — the same path the incident CLI uses — so writes go through
/// the single-writer + identity machinery unchanged). Returns the tool's JSON payload.
async fn console_call(name: &str, user: &str, tool: &str, args: Value) -> Response {
    use super::room_path::meeting_sock;
    use super::tui_client::{PostTarget, call_once};
    match call_once(&meeting_sock(), PostTarget::Named(name.to_string()), user, None, tool, args).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, format!("{tool}: {e}\n")).into_response(),
    }
}

/// `POST /rooms/{name}/threads` — open an incident/topic thread on an anchor message id.
/// Body: `{ "anchor_id": "<date>/<n>", "title": "...", "kind": "incident"|"topic" }`.
async fn thread_open(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    console_call(&name, &user, "meeting.thread_open", body).await
}

/// `POST /rooms/{name}/threads/{id}/escalate` — body `{ "to": "<handle>", "note": "..." }`.
async fn thread_escalate(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath((name, id)): AxumPath<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["id"] = json!(id);
    console_call(&name, &user, "meeting.escalate", body).await
}

/// `POST /rooms/{name}/threads/{id}/resolve` — body `{ "note": "..." }`.
async fn thread_resolve(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath((name, id)): AxumPath<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["id"] = json!(id);
    console_call(&name, &user, "meeting.resolve", body).await
}

/// `POST /rooms/{name}/threads/{id}/state` — body `{ "state": "triaging"|... }`.
async fn thread_state(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath((name, id)): AxumPath<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["id"] = json!(id);
    console_call(&name, &user, "meeting.thread_set_state", body).await
}

/// `POST /rooms/{name}/messages` — post a message with optional support metadata. Body:
/// `{ "content": "...", "kind": "alert", "severity": "high", "thread_id": "...", "tags": [...] }`.
async fn submit(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    console_call(&name, &user, "meeting.submit", body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use serde_json::Value;
    use tempfile::tempdir;

    fn seed_room(state: &Path, name: &str, messages: &[&str]) -> (String, PathBuf) {
        let paths = store::RoomPaths::ad_hoc_in(state, name);
        let root = paths.root.clone();
        let mut writer =
            store::TranscriptWriter::new(paths, name, "topic", None, state.to_path_buf());
        let mut date = String::new();
        for (i, msg) in messages.iter().enumerate() {
            let turn = writer
                .append("p", "P", *msg, 1_718_000_000 + i as u64)
                .unwrap();
            date = turn.date;
        }
        (date, root)
    }

    async fn start(state: PathBuf, secret: &str) -> (SocketAddr, watch::Sender<bool>) {
        let registry = Arc::new(RoomRegistry::new(state));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = watch::channel(false);
        let secret = secret.to_string();
        tokio::spawn(async move {
            serve(listener, registry, secret, rx).await.unwrap();
        });
        (addr, tx)
    }

    #[tokio::test]
    async fn rejects_missing_and_wrong_secret() {
        let dir = tempdir().unwrap();
        seed_room(dir.path(), "alpha", &["one"]);
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let url = format!("http://{addr}/rooms/alpha/days");
        let client = reqwest::Client::new();

        let res = client.get(&url).send().await.unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let res = client
            .get(&url)
            .basic_auth("", Some("wrong"))
            .send()
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn days_and_messages_read_from_registered_room() {
        let dir = tempdir().unwrap();
        let (date, root) = seed_room(dir.path(), "alpha", &["zero", "one", "two"]);
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let client = reqwest::Client::new();

        let days: Value = client
            .get(format!("http://{addr}/rooms/alpha/days"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(days["room"], "alpha");
        assert_eq!(days["days"].as_array().unwrap().len(), 1);
        assert_eq!(days["days"][0]["date"], date);
        assert_eq!(days["days"][0]["count"], 3);

        let page: Value = client
            .get(format!(
                "http://{addr}/rooms/alpha/messages/{date}?from=1&count=1"
            ))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page["room"], "alpha");
        assert_eq!(page["date"], date);
        assert_eq!(page["from"], 1);
        assert_eq!(page["count"], 1);
        assert_eq!(page["next_from"], 2);
        assert_eq!(page["has_more"], true);
        assert_eq!(page["messages"][0]["n"], 1);
        assert_eq!(page["messages"][0]["content"], "one");

        let missing: Value = client
            .get(format!("http://{addr}/rooms/alpha/messages/1999-01-01"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(missing["messages"].as_array().unwrap().len(), 0);
        assert!(root.join("index.json").exists(), "seed wrote the index");
    }

    #[tokio::test]
    async fn inbox_endpoint_returns_addressed_messages() {
        let dir = tempdir().unwrap();
        seed_room(dir.path(), "beta", &["-> bob ping you", "hi everyone"]);
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let client = reqwest::Client::new();

        let inbox: Value = client
            .get(format!("http://{addr}/rooms/beta/inbox/bob"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(inbox["handle"], "bob");
        assert_eq!(inbox["count"], 1);
        assert_eq!(inbox["messages"][0]["content"], "-> bob ping you");

        // someone not addressed has an empty inbox
        let other: Value = client
            .get(format!("http://{addr}/rooms/beta/inbox/alice"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(other["count"], 0);
    }

    /// Seed a room that has an incident: an anchor message, two replies into the
    /// thread, the thread opened + escalated (owner/severity), then resolved.
    fn seed_incident(state: &Path, name: &str) -> (String, String, PathBuf) {
        use store::{MsgKind, PostMeta, Severity, ThreadKind, ThreadState};
        let paths = store::RoomPaths::ad_hoc_in(state, name);
        let root = paths.root.clone();
        let mut w = store::TranscriptWriter::new(paths, name, "topic", None, state.to_path_buf());
        let mut pm = PostMeta::default();
        pm.kind = MsgKind::Alert;
        pm.meta.severity = Some(Severity::Critical);
        pm.meta.tags = vec!["db".into()];
        let anchor = w.append_with_meta("p", "Alice", "DB is down", 1_718_000_000, pm).unwrap();
        let id = anchor.id();
        w.open_thread(&id, "DB outage", ThreadKind::Incident, 1_718_000_001).unwrap();
        let mut reply = PostMeta::default();
        reply.thread_id = Some(id.clone());
        reply.kind = MsgKind::Event;
        w.append_with_meta("p2", "Bob", "looking", 1_718_000_100, reply).unwrap();
        w.set_thread_owner_severity(&id, Some("oncall".into()), Some(Severity::High), 1_718_000_200)
            .unwrap();
        w.set_thread_state(&id, ThreadState::Escalated, 1_718_000_200).unwrap();
        w.set_thread_state(&id, ThreadState::Resolved, 1_718_003_600).unwrap();
        (store::date_of_ts(1_718_000_000), id, root)
    }

    #[tokio::test]
    async fn threads_metrics_and_context_endpoints() {
        let dir = tempdir().unwrap();
        let (_date, id, _root) = seed_incident(dir.path(), "ops");
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let client = reqwest::Client::new();
        let get = |url: String| {
            let c = client.clone();
            async move { c.get(&url).basic_auth("", Some("sekret")).send().await.unwrap().json::<Value>().await.unwrap() }
        };

        // /threads — the incident plus a metrics summary.
        let t = get(format!("http://{addr}/rooms/ops/threads")).await;
        assert_eq!(t["count"], 1);
        assert_eq!(t["threads"][0]["title"], "DB outage");
        assert_eq!(t["threads"][0]["state"], "resolved");
        assert_eq!(t["threads"][0]["owner"], "oncall");
        assert_eq!(t["metrics"]["total"], 1);
        assert_eq!(t["metrics"]["resolved"], 1);
        assert_eq!(t["metrics"]["by_state"]["resolved"], 1);
        // thread opened at 1_718_000_001 → resolved 1_718_003_600 = 3599s MTTR.
        assert_eq!(t["metrics"]["avg_time_to_resolve_secs"], 3599);

        // /metrics — same numbers, room-tagged.
        let m = get(format!("http://{addr}/rooms/ops/metrics")).await;
        assert_eq!(m["room"], "ops");
        assert_eq!(m["avg_time_to_resolve_secs"], 3599);

        // /threads/{id} — the whole incident picture (anchor + reply = 2 msgs).
        let ctx = get(format!("http://{addr}/rooms/ops/threads/{}", enc(&id))).await;
        assert_eq!(ctx["thread"]["title"], "DB outage");
        assert_eq!(ctx["message_count"], 2);
        assert_eq!(ctx["participants"].as_array().unwrap().len(), 2);
        assert_eq!(ctx["messages"][0]["content"], "DB is down");
        assert_eq!(ctx["messages"][0]["meta"]["severity"], "critical");
    }

    #[tokio::test]
    async fn search_endpoint_filters_by_metadata() {
        let dir = tempdir().unwrap();
        let (_d, _id, _root) = seed_incident(dir.path(), "ops");
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let client = reqwest::Client::new();
        let get = |url: String| {
            let c = client.clone();
            async move { c.get(&url).basic_auth("", Some("sekret")).send().await.unwrap() }
        };
        // seed_incident posts: anchor alert "DB is down" (critical, tag db) + reply event "looking".
        let r: Value = get(format!("http://{addr}/rooms/ops/search?severity=high"))
            .await.json().await.unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["messages"][0]["content"], "DB is down");
        let r: Value = get(format!("http://{addr}/rooms/ops/search?q=looking"))
            .await.json().await.unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["messages"][0]["kind"], "event");
        let r: Value = get(format!("http://{addr}/rooms/ops/search?tag=db&kind=alert"))
            .await.json().await.unwrap();
        assert_eq!(r["count"], 1);
        // A typo'd severity is a 400, not a silent empty result.
        let bad = get(format!("http://{addr}/rooms/ops/search?severity=urgent")).await;
        assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn console_and_rooms_served() {
        let dir = tempdir().unwrap();
        seed_room(dir.path(), "alpha", &["hi"]);
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let client = reqwest::Client::new();

        let html = client
            .get(format!("http://{addr}/"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap();
        assert!(html.status().is_success());
        let body = html.text().await.unwrap();
        assert!(body.contains("support console") && body.contains("rooms/"));

        let rooms: Value = client
            .get(format!("http://{addr}/rooms"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(rooms["rooms"].as_array().unwrap().iter().any(|r| r == "alpha"));
    }

    fn enc(s: &str) -> String {
        s.replace('/', "%2F")
    }

    #[tokio::test]
    async fn days_falls_back_when_index_is_missing() {
        let dir = tempdir().unwrap();
        let (date, root) = seed_room(dir.path(), "alpha", &["one", "two"]);
        std::fs::remove_file(root.join("index.json")).unwrap();
        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;

        let days: Value = reqwest::Client::new()
            .get(format!("http://{addr}/rooms/alpha/days"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(days["days"][0]["date"], date);
        assert_eq!(days["days"][0]["count"], 2);
        assert!(days["days"][0]["bytes"].as_u64().unwrap() > 0);
    }
}
