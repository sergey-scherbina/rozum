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
    response::{
        Html, IntoResponse, Json, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::Stream;
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
    state_dir: PathBuf,
}

/// The authenticated operator's RBAC role, attached by `auth_layer`.
#[derive(Clone, Copy)]
struct ConsoleRole(store::Role);

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
    let Some(secret) = web_secret() else {
        // LOUD on purpose (BUG-024). Without a secret this daemon serves the socket and nothing on
        // :8401 — rooms keep working, every process looks healthy, and the console, the web client
        // and the generated terminal client all go quiet. Whoever is reading a log when that
        // happens deserves to be told, rather than left with the silence this used to return.
        tracing::warn!(
            "meeting REST read NOT started: no ROZUM_WEB_SECRET in the environment and no {} on \
             disk. Rooms work over the socket; :8401 does not. If this daemon was started by a \
             client rather than by its service, that is BUG-024.",
            web_secret_path().display()
        );
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

/// The REST secret: the environment first, then `~/.rozum/secrets/web-secret`.
///
/// The file matters because of BUG-024. `daemon_proxy::spawn_daemon` resurrects this daemon with
/// the CALLER's environment — an agent's MCP proxy, a bare CLI run — which carries no
/// `ROZUM_WEB_SECRET`. Reading it from the same place regardless of who started the process makes
/// `:8401` a property of the INSTALLATION rather than of the accident of who won the socket.
///
/// The env still wins, so a deliberately-configured service keeps overriding the file.
fn web_secret() -> Option<String> {
    resolve_web_secret(std::env::var("ROZUM_WEB_SECRET").ok(), &web_secret_path())
}

/// The precedence itself, separated from the process so it can be tested without mutating global
/// environment state — which in a parallel test run is a race, not a fixture.
fn resolve_web_secret(from_env: Option<String>, path: &Path) -> Option<String> {
    let clean = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(v) = from_env.and_then(clean) {
        return Some(v);
    }
    std::fs::read_to_string(path).ok().and_then(clean)
}

/// `~/.rozum/secrets/web-secret` — the same directory, ownership and 600 the messenger tokens use.
fn web_secret_path() -> PathBuf {
    super::room_path::dirs_home_public()
        .join(".rozum")
        .join("secrets")
        .join("web-secret")
}

pub async fn serve(
    listener: TcpListener,
    registry: Arc<RoomRegistry>,
    secret: String,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = router(registry.clone(), secret);
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
    let state_dir = registry.state_dir().clone();
    Router::new()
        .route("/", get(console))
        .route("/rooms", get(rooms).post(create_room))
        .route("/rooms/{name}/days", get(days))
        .route("/rooms/{name}/messages/{date}", get(messages))
        .route("/rooms/{name}/inbox/{handle}", get(inbox))
        .route("/rooms/{name}/threads", get(threads).post(thread_open))
        .route("/rooms/{name}/threads/{id}", get(thread_one))
        .route("/rooms/{name}/threads/{id}/escalate", post(thread_escalate))
        .route("/rooms/{name}/threads/{id}/assign", post(thread_assign))
        .route("/rooms/{name}/threads/{id}/resolve", post(thread_resolve))
        .route("/rooms/{name}/threads/{id}/state", post(thread_state))
        .route("/rooms/{name}/threads/{id}/pin", post(thread_pin))
        .route("/rooms/{name}/threads/{id}/link", post(thread_link))
        .route("/rooms/{name}/messages", post(submit))
        .route("/rooms/{name}/redact", post(redact))
        .route("/rooms/{name}/reactions", get(reactions))
        .route("/rooms/{name}/react", post(react))
        .route("/rooms/{name}/roles", get(roles).post(set_role))
        .route("/rooms/{name}/phase", post(set_phase))
        .route("/rooms/{name}/queue", get(queue))
        .route("/rooms/{name}/metrics", get(metrics))
        .route("/rooms/{name}/events", get(events))
        .route("/rooms/{name}/search", get(search))
        .route("/whoami", get(whoami))
        .route("/rooms/{name}/whoami", get(whoami))
        .route("/roster", get(roster))
        .layer(middleware::from_fn_with_state(
            AuthCfg { secret, state_dir },
            auth_layer,
        ))
        .with_state(RestState { registry })
}

/// `GET /whoami` — the authenticated operator's handle + role, so the console can label itself and
/// hide actions the role can't perform.
async fn whoami(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    Extension(ConsoleRole(role)): Extension<ConsoleRole>,
) -> Response {
    Json(json!({ "handle": user, "role": role.as_str() })).into_response()
}

/// `GET /` — the support console (single-page incident dashboard). Static HTML;
/// it reads `?room=<name>` from its own URL and drives the JSON endpoints (GET to
/// read, POST to act — escalate / resolve / open / compose) with the Basic-auth
/// credentials the browser already holds.
async fn console() -> Html<&'static str> {
    Html(include_str!("console.html"))
}

/// `GET /rooms` — the registered rooms (so the console can offer a picker).
async fn rooms(
    State(state): State<RestState>,
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    req_headers: header::HeaderMap,
) -> Response {
    let locs = state.registry.list();
    let names: Vec<String> = locs.iter().map(|loc| loc.name.clone()).collect();

    // `entries` is ADDITIVE — `rooms` keeps its shape for anything already reading it.
    //
    // Each entry carries a READY-MADE transcript url, because the generated meeting client cannot
    // build one: string composition is not expressible on the terminal target. Same call as `badge`
    // and `time` — the client is deliberately dumb, and the server is the one place that can be
    // clever once. The url is absolute and derived from the REQUEST, so a client reaching this
    // daemon through a proxy gets a url on the origin it actually used, not on `127.0.0.1`.
    let base = request_origin(&req_headers);
    let today = store::date_of_ts(now_secs());
    let entries: Vec<Value> = locs
        .iter()
        .map(|loc| {
            let last = day_listing(&loc.root).last().map(|d| d.date.clone());
            // "Unread" here means what this daemon can honestly say: messages ADDRESSING you that
            // you have not seen, counted against the same per-handle cursor `inbox` uses. A raw
            // unread count would need a per-viewer read marker for every message, which does not
            // exist — and in a busy room "what wants me" is the more useful number anyway.
            let mentions = super::client::inbox(&loc.root, &user, false).len();
            let date = last.clone().unwrap_or_else(|| today.clone());
            json!({
                "name": loc.name,
                "url": format!("{base}/rooms/{}/messages/{date}", loc.name),
                "last": last.unwrap_or_default(),
                "mentions": mentions,
            })
        })
        .collect();
    Json(json!({ "rooms": names, "entries": entries })).into_response()
}

/// The origin a client actually reached us on — `X-Forwarded-Proto`/`Host` when a proxy is in
/// front, else the `Host` header, else the loopback default. Used to hand out absolute urls that
/// work from wherever the request came from.
fn request_origin(h: &header::HeaderMap) -> String {
    let host = h
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_BIND);
    let scheme = h
        .get("x-forwarded-proto")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .unwrap_or("http");
    format!("{scheme}://{host}")
}

/// `POST /rooms` — create an ad-hoc room and return it, with a ready-made transcript url.
///
/// The last thing `rozum meetings attach` could do that the generated client could not. Rooms are
/// otherwise only born by joining a project (automatic) — `rozum rooms` prunes and nothing else —
/// so without this, retiring the hand-written TUI would REMOVE the only interactive way to make one.
///
/// Unlike every other write here it does not go through `console_call`: that joins a room first, and
/// there is no room yet. `MeetingClient::new_room` is the same call the TUI's picker makes, so
/// creation still goes through the daemon's single-writer and identity machinery rather than around
/// it. RBAC needs no special case — it is a POST, so `required_role` already demands `Responder`.
///
/// The body is the topic, plain text or `{"topic": "..."}`, for the same reason `submit` takes both:
/// a generated client cannot compose JSON.
/// The topic of a `POST /rooms` body: an object's `topic`, a bare JSON string, or the raw text.
///
/// Split out from the handler so it is testable without a daemon — the handler proxies through
/// `MeetingClient`, which needs one, while the interesting behaviour is entirely here. Blank is
/// `None` rather than `Some("")`: an unnamed room is a real thing, an empty-string-named one is not.
fn create_topic(body: &str) -> Option<String> {
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(o)) => o.get("topic").and_then(Value::as_str).map(str::to_owned),
        Ok(Value::String(t)) => Some(t),
        _ => Some(body.to_owned()),
    }
    .map(|t| t.trim().to_owned())
    .filter(|t| !t.is_empty())
}

async fn create_room(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    req_headers: header::HeaderMap,
    body: String,
) -> Response {
    use super::room_path::meeting_sock;
    use super::tui_client::MeetingClient;

    let topic = create_topic(&body);

    let mut client = match MeetingClient::connect(&meeting_sock(), &user).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("rooms.new: {e}\n")).into_response(),
    };
    match client.new_room(topic.as_deref()).await {
        Ok(name) => {
            let base = request_origin(&req_headers);
            let date = store::date_of_ts(now_secs());
            Json(json!({
                "room": name,
                "url": format!("{base}/rooms/{name}/messages/{date}"),
            }))
            .into_response()
        }
        Err(e) => (StatusCode::BAD_GATEWAY, format!("rooms.new: {e}\n")).into_response(),
    }
}

/// `GET /rooms/{name}/threads` — the incident/topic threads + a metrics summary,
/// the support console's left rail. Reads `threads.json` directly from disk.
async fn threads(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let threads: Vec<store::Thread> = store::read_threads(&root).into_values().collect();
    let now = now_secs();
    // Augment each thread with derived SLA signals (stale + age) — the support dashboard's
    // "needs attention" cue — without changing the stored `Thread` shape.
    let mut needs_attention = 0u64;
    let augmented: Vec<Value> = threads
        .iter()
        .map(|t| {
            let stale = store::thread_is_stale(t, now);
            if stale {
                needs_attention += 1;
            }
            let mut v = serde_json::to_value(t).unwrap_or_else(|_| json!({}));
            if let Value::Object(o) = &mut v {
                o.insert("stale".into(), json!(stale));
                o.insert("age_secs".into(), json!(now.saturating_sub(t.created_ts)));
            }
            v
        })
        .collect();
    let mut metrics = store::thread_metrics(&root);
    metrics["needs_attention"] = json!(needs_attention);
    Json(json!({
        "room": name,
        "count": augmented.len(),
        "threads": augmented,
        "metrics": metrics,
    }))
    .into_response()
}

/// Current unix time in seconds (for SLA/staleness derivation).
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
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
    let now = now_secs();
    let needs_attention = store::read_threads(&root)
        .values()
        .filter(|t| store::thread_is_stale(t, now))
        .count();
    let mut out = store::thread_metrics(&root);
    out["room"] = json!(name);
    out["needs_attention"] = json!(needs_attention);
    Json(out).into_response()
}

/// `GET /rooms/{name}/events` — Server-Sent Events stream that emits a `changed` event whenever the room
/// is written to (taps the daemon's per-room `Notify`, so it's event-driven, not polled). The console
/// refreshes on each event → near-instant updates with no idle polling.
async fn events(
    State(state): State<RestState>,
    AxumPath(name): AxumPath<String>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    // Resolve (and open) the live room so we share the writer's Notify; None → a stream that only
    // keep-alives (the client still has its fallback poll).
    let handle = state.registry.get_by_name(&name).ok().flatten();
    let stream = async_stream::stream! {
        // Fire once on connect so the client syncs immediately.
        yield Ok(Event::default().event("changed").data("init"));
        if let Some(handle) = handle {
            let notify = { handle.lock().await.notify.clone() };
            loop {
                notify.notified().await;
                yield Ok(Event::default().event("changed").data("x"));
            }
        }
    };
    Sse::new(stream).keep_alive(KeepAlive::default())
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

async fn days(
    State(state): State<RestState>,
    AxumPath(name): AxumPath<String>,
    req_headers: header::HeaderMap,
) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    // Each day carries its own ready-made transcript url, for the same reason `/rooms` does: the
    // generated client cannot build one. That turns day-paging into the SAME interaction as room
    // switching — pick a row, the transcript follows — instead of a bespoke PgUp that would need
    // date arithmetic the terminal target cannot express. Newest first: a reader opening the list
    // wants today, not the first day the room ever had.
    let base = request_origin(&req_headers);
    let mut days = day_listing(&root);
    days.reverse();
    let entries: Vec<Value> = days
        .iter()
        .map(|d| {
            json!({
                "date": d.date,
                "count": d.count,
                "url": format!("{base}/rooms/{name}/messages/{}", d.date),
            })
        })
        .collect();
    Json(json!({ "room": name, "days": days, "entries": entries })).into_response()
}

async fn messages(
    State(state): State<RestState>,
    AxumPath((name, date)): AxumPath<(String, String)>,
    Query(q): Query<PageQuery>,
) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    // `today` is a symbolic date, and it exists because a generated client cannot compute one:
    // `env()` on the terminal target resolves at EMIT time, so a shipped binary would carry
    // whatever day it was built on. The server is the only side that knows what today is — the
    // same call as the ready-made urls and the derived badge.
    let date = if date == "today" { store::date_of_ts(now_secs()) } else { date };
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
    let count = turns.len();
    let messages: Vec<Value> = turns.iter().map(message_json).collect();
    Json(json!({
        "room": name,
        "date": date,
        "from": from,
        "count": count,
        "next_from": next_from,
        "has_more": has_more,
        "messages": messages,
    }))
    .into_response()
}

/// A stored turn as the read API returns it: every stored field, plus the two DERIVED display
/// strings a client cannot compute for itself.
///
/// `badge` and `time` are additive — existing consumers keep seeing exactly the keys they saw
/// before. They exist because of `ucc-meetings-in-tk`: the generated meeting client binds a fetch
/// to a table, so it has no place to run `StoredTurn::badge()` or format a unix epoch. Sending the
/// computed strings keeps ONE implementation of the incident badge (this crate's) instead of a
/// second one in `.ssc` that would drift from it — which is the entire point of generating the
/// client rather than hand-writing it a second time.
///
/// `badge` is `""` rather than absent when a message carries no incident metadata: a table column
/// bound to a sometimes-missing key is a client-side edge case, and an always-present string is
/// the least surprising thing for a deliberately dumb client.
fn message_json(turn: &store::StoredTurn) -> Value {
    let mut v = serde_json::to_value(turn).unwrap_or_else(|_| json!({}));
    if let Some(obj) = v.as_object_mut() {
        obj.insert(
            "badge".into(),
            Value::String(turn.badge().unwrap_or_default()),
        );
        obj.insert("time".into(), Value::String(turn.time_hm()));
    }
    v
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
            // Advertise both, Bearer first. Browsers do not act on a Bearer challenge, so this
            // does not change what a browser does — it tells a MACHINE client that the scheme it
            // can actually construct is available.
            [(
                header::WWW_AUTHENTICATE,
                "Bearer realm=\"rozum meeting\", Basic realm=\"rozum meeting\"",
            )],
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
    // Two accepted schemes, resolving to the same `(user, pass)` the logic below already expects.
    //
    // `Bearer <token>` is the one to prefer and exists because of the GENERATED clients
    // (`ucc-meetings-in-tk`): Basic requires base64 of `":" + token`, and a `.ssc` view has no
    // base64 — so every generated client would otherwise have to be handed a pre-built header
    // through its environment, which is both awkward and the shape that leaks secrets into
    // artifacts. Bearer also says what is actually happening: the username field was always empty
    // here and the token always travelled in the password.
    //
    // `Basic` stays, unchanged, because the CLI, the console and the existing tests use it.
    let (user, pass) = if let Some(tok) = h
        .strip_prefix("Bearer ")
        .or_else(|| h.strip_prefix("bearer "))
    {
        (String::new(), tok.trim().to_owned())
    } else if let Some(b64) = h
        .strip_prefix("Basic ")
        .or_else(|| h.strip_prefix("basic "))
    {
        let Ok(dec) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
            return unauth();
        };
        match std::str::from_utf8(&dec).ok().and_then(|c| c.split_once(':')) {
            Some((u, p)) => (u.to_owned(), p.to_owned()),
            None => return unauth(),
        }
    } else {
        return unauth();
    };
    // The password is EITHER an issued token (→ a trusted handle + role) OR the shared secret (→ admin,
    // back-compat). A token's handle is authoritative (ignore the self-asserted X-Rozum-Actor); the
    // shared-secret path keeps the actor-header convenience. The role is the token's EFFECTIVE role for
    // the room in the path (per-room override else global) — so a token can be admin in one room, observer
    // in another.
    let path_room = room_in_path(req.uri().path());
    let (actor, role) = if let Some(info) = store::resolve_token(&cfg.state_dir, &pass, now_secs()) {
        let role = info.effective_role(path_room.as_deref());
        (info.handle, role)
    } else if pass == cfg.secret {
        let header_actor = req
            .headers()
            .get("x-rozum-actor")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let actor = header_actor
            .or_else(|| (!user.is_empty()).then_some(user))
            .unwrap_or_else(|| "console".to_string());
        (actor, store::Role::Admin)
    } else {
        return unauth();
    };

    // RBAC: reads need Observer; writes need Responder; redact needs Admin.
    let need = required_role(req.method(), req.uri().path());
    if role < need {
        return (
            StatusCode::FORBIDDEN,
            format!("403: {} requires {} (you are {})\n", req.uri().path(), need.as_str(), role.as_str()),
        )
            .into_response();
    }

    let mut req = req;
    req.extensions_mut().insert(ConsoleUser(actor));
    req.extensions_mut().insert(ConsoleRole(role));
    next.run(req).await
}

/// Extract the room name from a `/rooms/<name>/…` path (URL-decoded by axum), for per-room role lookup.
fn room_in_path(path: &str) -> Option<String> {
    let rest = path.strip_prefix("/rooms/")?;
    let name = rest.split('/').next()?;
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// The role a request needs: writes (POST) need Responder, a redact needs Admin, reads need Observer.
fn required_role(method: &axum::http::Method, path: &str) -> store::Role {
    if *method != axum::http::Method::POST {
        store::Role::Observer
    } else if path.ends_with("/redact") {
        store::Role::Admin
    } else {
        store::Role::Responder
    }
}

/// The authenticated console actor (handle), attached by `auth_layer` and used to attribute write
/// actions (open/escalate/resolve/submit) when the console drives the daemon.
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

/// `POST /rooms/{name}/threads/{id}/assign` — body `{ "to": "<handle>", "note": "..." }` (no state change).
async fn thread_assign(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath((name, id)): AxumPath<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["id"] = json!(id);
    console_call(&name, &user, "meeting.thread_assign", body).await
}

/// `POST /rooms/{name}/threads/{id}/pin` — body `{ "msg_id": "<date>/<n>", "pin": true|false }`.
async fn thread_pin(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath((name, id)): AxumPath<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["id"] = json!(id);
    console_call(&name, &user, "meeting.thread_pin", body).await
}

/// `POST /rooms/{name}/threads/{id}/link` — body `{ "msg_id": "<date>/<n>", "link": true|false }`.
async fn thread_link(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath((name, id)): AxumPath<(String, String)>,
    Json(mut body): Json<Value>,
) -> Response {
    body["id"] = json!(id);
    console_call(&name, &user, "meeting.thread_link", body).await
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

/// `GET /rooms/{name}/reactions` — the room's reaction map (`msg_id → emoji → [who]`).
async fn reactions(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    Json(json!({ "room": name, "reactions": store::load_reactions(&root) })).into_response()
}

/// `GET /rooms/{name}/queue` — the room's open threads, worst first, SLA arithmetic already done.
async fn queue(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let rows = store::room_queue(&root, crate::meeting::state::unix_ts());
    Json(json!({ "room": name, "queue": rows })).into_response()
}

/// `POST /rooms/{name}/phase` — body `{ "phase": "active" | "paused" | "ended" }`.
async fn set_phase(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    console_call(&name, &user, "meeting.room_phase", body).await
}

/// `GET /rooms/{name}/roles` — who holds which role, for a console that wants to show it without
/// re-deriving it from the roster file.
async fn roles(State(state): State<RestState>, AxumPath(name): AxumPath<String>) -> Response {
    let Some(root) = room_root(&state.registry, &name) else {
        return (StatusCode::NOT_FOUND, "unknown room\n").into_response();
    };
    let roster = crate::meeting::identity::Roster::load(&root.join("roster.json"));
    let who: Vec<Value> = roster
        .participants
        .iter()
        .filter(|e| !e.roles.is_empty())
        .map(|e| json!({ "handle": e.handle, "roles": e.roles }))
        .collect();
    Json(json!({ "room": name, "participants": who })).into_response()
}

/// `POST /rooms/{name}/roles` — body `{ "handle": "eager-otter", "role": "on_call", "grant": true }`.
async fn set_role(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    console_call(&name, &user, "meeting.role", body).await
}

/// `POST /rooms/{name}/react` — body `{ "msg_id": "<date>/<n>", "emoji": "👍", "add": true|false }`.
async fn react(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    console_call(&name, &user, "meeting.react", body).await
}

/// `POST /rooms/{name}/redact` — body `{ "msg_id": "<date>/<n>", "redact": true|false, "reason": "..." }`.
async fn redact(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    Json(body): Json<Value>,
) -> Response {
    console_call(&name, &user, "meeting.redact", body).await
}

/// `POST /rooms/{name}/messages` — post a message with optional support metadata. Body:
/// `{ "content": "...", "kind": "alert", "severity": "high", "thread_id": "...", "tags": [...] }`
/// — **or just the message text**, which is the same thing said the short way.
///
/// The plain-text form exists for the generated meeting client (`ucc-meetings-in-tk`). Its composer
/// sends whatever is in the input signal verbatim, and it has no way to wrap that in JSON: string
/// composition is not expressible on the terminal target (there is no `computedSignal` in the
/// static model). Making the SERVER accept the short form is the same call already made for `badge`
/// and `time` — put the work where it can be done once, and keep the generated client deliberately
/// dumb, rather than bending the client into shapes the target cannot express.
///
/// Ambiguity is resolved in favour of the existing contract: a body that parses as a JSON OBJECT is
/// treated as the structured form exactly as before. Everything else — including a bare JSON string
/// — is the message text.
async fn submit(
    Extension(ConsoleUser(user)): Extension<ConsoleUser>,
    AxumPath(name): AxumPath<String>,
    body: String,
) -> Response {
    console_call(&name, &user, "meeting.submit", submit_payload(&body)).await
}

/// The body of `POST /rooms/{name}/messages` as the daemon wants it.
///
/// Split out from the handler so it is testable on its own: the handler proxies through
/// `console_call`, which needs a live daemon, and the interesting behaviour is entirely here.
fn submit_payload(body: &str) -> Value {
    match serde_json::from_str::<Value>(body) {
        Ok(v @ Value::Object(_)) => v,
        // A bare JSON string is TEXT that happens to be quoted — store the words, not the quotes.
        Ok(Value::String(s)) => json!({ "content": s }),
        _ => json!({ "content": body }),
    }
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
    async fn rbac_tokens_gate_by_role() {
        use store::Role;
        let dir = tempdir().unwrap();
        seed_room(dir.path(), "alpha", &["one"]);
        // Tokens live in the registry's state_dir (= dir.path() here).
        let obs = store::issue_token(dir.path(), "obs", Role::Observer, 0, 0).unwrap();
        let resp = store::issue_token(dir.path(), "resp", Role::Responder, 0, 0).unwrap();
        let adm = store::issue_token(dir.path(), "adm", Role::Admin, 0, 0).unwrap();
        let (addr, _s) = start(dir.path().to_path_buf(), "sekret").await;
        let c = reqwest::Client::new();
        let st = |b: reqwest::RequestBuilder| async move { b.send().await.unwrap().status() };

        // A token authenticates as its handle (read works for the lowest role).
        assert!(st(c.get(format!("http://{addr}/rooms/alpha/days")).basic_auth("x", Some(&obs))).await.is_success());
        // Observer is read-only: a write (POST) is 403 (blocked in auth, never reaches the daemon).
        assert_eq!(st(c.post(format!("http://{addr}/rooms/alpha/messages")).basic_auth("x", Some(&obs)).json(&json!({"content":"x"}))).await, StatusCode::FORBIDDEN);
        // Responder can write, but redact needs Admin → 403 for responder.
        assert_eq!(st(c.post(format!("http://{addr}/rooms/alpha/redact")).basic_auth("x", Some(&resp)).json(&json!({"msg_id":"a/0"}))).await, StatusCode::FORBIDDEN);
        // An unknown token (not the secret) is 401.
        assert_eq!(st(c.get(format!("http://{addr}/rooms/alpha/days")).basic_auth("x", Some("bogus"))).await, StatusCode::UNAUTHORIZED);
        // whoami reflects the token's handle + role.
        let who: Value = c.get(format!("http://{addr}/whoami")).basic_auth("x", Some(&adm)).send().await.unwrap().json().await.unwrap();
        assert_eq!(who["handle"], "adm");
        assert_eq!(who["role"], "admin");
        // The shared secret still works (admin, back-compat): redact passes RBAC.
        assert_ne!(st(c.get(format!("http://{addr}/whoami")).basic_auth("x", Some("sekret"))).await, StatusCode::FORBIDDEN);
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

    /// The read API carries the two DERIVED display strings the generated meeting client cannot
    /// compute for itself (`ucc-meetings-in-tk`): the incident badge and a local `HH:MM`.
    ///
    /// The badge assertion is the load-bearing one. It compares against `StoredTurn::badge()`
    /// rather than against a hand-written `"[ALERT CRIT #db]"`, because the point of sending the
    /// badge at all is that there stays exactly ONE implementation of it — a literal here would
    /// pass while the two drifted, which is precisely the failure this endpoint change exists to
    /// prevent.
    #[tokio::test]
    async fn messages_carry_derived_badge_and_time() {
        let dir = tempdir().unwrap();
        let paths = store::RoomPaths::ad_hoc_in(dir.path(), "alpha");
        let mut writer =
            store::TranscriptWriter::new(paths, "alpha", "topic", None, dir.path().to_path_buf());
        let plain = writer.append("p", "P", "just a note", 1_718_000_000).unwrap();
        let flagged = writer
            .append_with_meta(
                "p",
                "P",
                "the db is down",
                1_718_000_060,
                store::PostMeta {
                    kind: store::MsgKind::Alert,
                    meta: store::MsgMeta {
                        severity: Some(store::Severity::Critical),
                        tags: vec!["db".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .unwrap();
        let date = plain.date.clone();

        let (addr, _shutdown) = start(dir.path().to_path_buf(), "sekret").await;
        let page: Value = reqwest::Client::new()
            .get(format!("http://{addr}/rooms/alpha/messages/{date}"))
            .basic_auth("", Some("sekret"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // A plain note: badge present but EMPTY — the column always binds, never a missing key.
        assert_eq!(page["messages"][0]["content"], "just a note");
        assert_eq!(page["messages"][0]["badge"], "");
        // A flagged one: byte-identical to what the Rust side renders.
        let expected = flagged.badge().expect("an alert with severity has a badge");
        assert!(!expected.is_empty());
        assert_eq!(page["messages"][1]["badge"], expected);

        // Time is the local clock, and it is a string the client can print as-is.
        let t = page["messages"][0]["time"].as_str().unwrap();
        assert_eq!(t.len(), 5, "HH:MM, got {t:?}");
        assert_eq!(t.as_bytes()[2], b':', "HH:MM, got {t:?}");
        assert_eq!(page["messages"][0]["time"], plain.time_hm());

        // Additive: every stored field a client already relied on is still there.
        assert_eq!(page["messages"][0]["n"], 0);
        assert_eq!(page["messages"][0]["display_name"], "P");
        assert_eq!(page["messages"][0]["date"], date);
        assert!(page["messages"][0]["ts"].is_u64());
    }

    /// `Authorization: Bearer <token>` is accepted alongside Basic, and resolves to the SAME
    /// handle and role.
    ///
    /// It exists for the generated clients: Basic needs base64 of `":" + token`, and a `.ssc` view
    /// has no base64, so a generated client would have to be handed a pre-built header through its
    /// environment — awkward, and the shape that ends with secrets baked into artifacts. The
    /// assertions below deliberately compare Bearer against Basic rather than against expected
    /// literals: the property that matters is that the two schemes are the same door, not that
    /// either one returns some particular string.
    #[tokio::test]
    async fn bearer_is_accepted_alongside_basic() {
        use store::Role;
        let dir = tempdir().unwrap();
        seed_room(dir.path(), "alpha", &["one"]);
        let obs = store::issue_token(dir.path(), "obs", Role::Observer, 0, 0).unwrap();
        let (addr, _s) = start(dir.path().to_path_buf(), "sekret").await;
        let c = reqwest::Client::new();

        // Same token, both schemes → same identity.
        let via_bearer: Value = c
            .get(format!("http://{addr}/whoami"))
            .bearer_auth(&obs)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let via_basic: Value = c
            .get(format!("http://{addr}/whoami"))
            .basic_auth("x", Some(&obs))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(via_bearer, via_basic);
        assert_eq!(via_bearer["handle"], "obs");

        // RBAC is unchanged by the scheme: an observer still cannot write.
        let write = c
            .post(format!("http://{addr}/rooms/alpha/messages"))
            .bearer_auth(&obs)
            .json(&json!({"content": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(write.status(), StatusCode::FORBIDDEN);

        // The shared secret works as a bearer too (admin, same back-compat as Basic).
        let secret = c
            .get(format!("http://{addr}/rooms/alpha/days"))
            .bearer_auth("sekret")
            .send()
            .await
            .unwrap();
        assert!(secret.status().is_success());

        // A bad bearer is 401, and the challenge offers Bearer FIRST so a machine client is told
        // about the scheme it can actually build.
        let bad = c
            .get(format!("http://{addr}/rooms/alpha/days"))
            .bearer_auth("bogus")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), StatusCode::UNAUTHORIZED);
        let challenge = bad
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        assert!(challenge.starts_with("Bearer "), "got {challenge:?}");
        assert!(challenge.contains("Basic"), "got {challenge:?}");
    }

    /// The submit body takes the message text on its own, not only the structured object — the
    /// short form the generated composer can actually produce (a terminal target cannot compose
    /// strings, so it cannot wrap what was typed in JSON).
    ///
    /// The middle case is the one worth having: a bare JSON string is the only input that is BOTH
    /// valid JSON and not the structured form, so it is exactly where a naive "parse, else wrap"
    /// implementation posts the quotes along with the words.
    #[test]
    fn submit_payload_takes_text_or_the_structured_form() {
        assert_eq!(submit_payload("just the words")["content"], "just the words");
        assert_eq!(submit_payload("\"quoted words\"")["content"], "quoted words");
        assert_eq!(
            submit_payload("{\"content\":\"structured\",\"kind\":\"alert\"}")["content"],
            "structured"
        );
        // The structured form keeps ALL of its fields — this is the existing contract, unchanged.
        assert_eq!(
            submit_payload("{\"content\":\"structured\",\"kind\":\"alert\"}")["kind"],
            "alert"
        );
        // Text that merely looks structured is still text.
        assert_eq!(submit_payload("{not json")["content"], "{not json");
        // An empty body is an empty message, not a crash.
        assert_eq!(submit_payload("")["content"], "");
    }

    /// `GET /rooms` carries a ready-made transcript url per room, and a mentions count for the
    /// authenticated handle.
    ///
    /// The url is the load-bearing part: the generated meeting client cannot build one (no string
    /// composition on the terminal target), so a picker can only work if the row already holds the
    /// address it should switch to. It is absolute and taken from the REQUEST, so a client that
    /// reached the daemon through a proxy is not handed a `127.0.0.1` url it cannot use.
    #[tokio::test]
    async fn rooms_carry_a_ready_made_url_and_a_mentions_count() {
        let dir = tempdir().unwrap();
        seed_room(dir.path(), "alpha", &["-> picker hello", "unrelated chatter"]);
        seed_room(dir.path(), "beta", &["nothing for anyone"]);
        let tok = store::issue_token(dir.path(), "picker", store::Role::Observer, 0, 0).unwrap();
        let (addr, _s) = start(dir.path().to_path_buf(), "sekret").await;

        let body: Value = reqwest::Client::new()
            .get(format!("http://{addr}/rooms"))
            .bearer_auth(&tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // The old shape is untouched — anything already reading `rooms` keeps working.
        let names: Vec<&str> = body["rooms"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert!(names.contains(&"alpha") && names.contains(&"beta"), "{names:?}");

        let entries = body["entries"].as_array().unwrap();
        let alpha = entries.iter().find(|e| e["name"] == "alpha").expect("alpha entry");
        let beta = entries.iter().find(|e| e["name"] == "beta").expect("beta entry");

        // Absolute, on the origin the request actually used, and pointing at a real day.
        let url = alpha["url"].as_str().unwrap();
        assert!(url.starts_with(&format!("http://{addr}/rooms/alpha/messages/")), "{url}");
        assert_eq!(url.rsplit('/').next().unwrap(), alpha["last"].as_str().unwrap());

        // The url a picker writes must be one the transcript endpoint actually answers.
        let page: Value = reqwest::Client::new()
            .get(url)
            .bearer_auth(&tok)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(page["room"], "alpha");
        assert!(page["messages"].as_array().unwrap().len() >= 2);

        // Mentions are per-handle and unseen-only: `picker` is addressed once in alpha, never in beta.
        assert_eq!(alpha["mentions"], 1);
        assert_eq!(beta["mentions"], 0);
    }

    /// `POST /rooms` is gated like every other write.
    ///
    /// ⚠️ It deliberately does NOT exercise the success path, and the reason is worth keeping: the
    /// handler proxies to `meeting_sock()`, which resolves to whatever daemon is running on the
    /// machine. An earlier version of this test asserted `502 Bad Gateway` for an authorised POST
    /// on the assumption that no daemon would be there — the operator's daemon WAS, so the test
    /// created two live ad-hoc rooms in it before anyone noticed. A test that reaches a socket is
    /// not a unit test; it is a client of whatever happens to be listening. The creation path is
    /// covered by the dual-target smoke against an isolated fixture instead.
    #[tokio::test]
    async fn create_room_is_gated_like_every_other_write() {
        let dir = tempdir().unwrap();
        let obs = store::issue_token(dir.path(), "watcher", store::Role::Observer, 0, 0).unwrap();
        let (addr, _s) = start(dir.path().to_path_buf(), "sekret").await;
        let c = reqwest::Client::new();
        let url = format!("http://{addr}/rooms");

        assert_eq!(
            c.post(&url).body("topic".to_string()).send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        // An observer may READ the room list and may not create — the same rule as every write.
        assert_eq!(
            c.post(&url).bearer_auth(&obs).body("topic".to_string()).send().await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert!(c.get(&url).bearer_auth(&obs).send().await.unwrap().status().is_success());
    }

    #[test]
    fn create_topic_takes_text_or_the_structured_form() {
        assert_eq!(create_topic("planning the release").as_deref(), Some("planning the release"));
        assert_eq!(create_topic("\"quoted topic\"").as_deref(), Some("quoted topic"));
        assert_eq!(create_topic("{\"topic\":\"structured\"}").as_deref(), Some("structured"));
        // An unnamed room is a real thing; a room named "" is not.
        assert_eq!(create_topic(""), None);
        assert_eq!(create_topic("   "), None);
        assert_eq!(create_topic("{\"topic\":\"  \"}"), None);
    }

    /// `messages/today` means the current day.
    ///
    /// A generated client cannot compute a date — `env()` resolves in the emitting process, so a
    /// shipped binary would carry the day it was BUILT on and read an empty transcript forever
    /// after. The alias moves that knowledge to the only side that has it.
    #[tokio::test]
    async fn messages_today_resolves_to_the_current_day() {
        let dir = tempdir().unwrap();
        let (date, _root) = seed_room(dir.path(), "alpha", &["hello"]);
        let (addr, _s) = start(dir.path().to_path_buf(), "sekret").await;
        let c = reqwest::Client::new();

        let by_alias: Value = c.get(format!("http://{addr}/rooms/alpha/messages/today"))
            .bearer_auth("sekret").send().await.unwrap().json().await.unwrap();
        let by_date: Value = c.get(format!("http://{addr}/rooms/alpha/messages/{date}"))
            .bearer_auth("sekret").send().await.unwrap().json().await.unwrap();

        // The seeded turn is written at a FIXED timestamp, so `date` is that day and today may not
        // be — what must hold is that the alias resolves to a real day and answers, and that when
        // they are the same day the two reads agree.
        assert_eq!(by_alias["room"], "alpha");
        assert_ne!(by_alias["date"], "today", "the alias was passed through unresolved");
        assert_eq!(by_alias["date"], store::date_of_ts(now_secs()));
        if by_alias["date"] == by_date["date"] {
            assert_eq!(by_alias["messages"], by_date["messages"]);
        }
    }

    /// The REST secret is a property of the INSTALLATION, not of who started the daemon (BUG-024).
    ///
    /// `daemon_proxy::spawn_daemon` resurrects the daemon with the caller's environment, which
    /// carries no `ROZUM_WEB_SECRET`; before the file fallback that daemon served the socket and
    /// nothing on `:8401`, with every surface still looking healthy.
    #[test]
    fn the_web_secret_falls_back_to_disk_but_the_environment_still_wins() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("web-secret");

        // Neither source: None, and the caller warns rather than starting a half-daemon quietly.
        assert_eq!(resolve_web_secret(None, &file), None);

        // File only — the autostart case this exists for.
        std::fs::write(&file, "  from-disk\n").unwrap();
        assert_eq!(resolve_web_secret(None, &file).as_deref(), Some("from-disk"));

        // Environment wins, so a deliberately-configured service still overrides the file.
        assert_eq!(
            resolve_web_secret(Some("from-env".into()), &file).as_deref(),
            Some("from-env")
        );

        // An empty or blank value is not a secret — it must not silently authenticate everything.
        assert_eq!(resolve_web_secret(Some("   ".into()), &file).as_deref(), Some("from-disk"));
        std::fs::write(&file, "\n\n").unwrap();
        assert_eq!(resolve_web_secret(Some("".into()), &file), None);
    }

    /// A turn with no timestamp renders no clock rather than 1970 — the client prints the string
    /// verbatim, so an empty one is the only honest "unknown".
    #[test]
    fn time_hm_is_empty_without_a_timestamp() {
        let turn = store::StoredTurn::default();
        assert_eq!(turn.ts, 0);
        assert_eq!(turn.time_hm(), "");
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
        // Structural smoke: the production-grade feature wiring must be present in the served console.
        // Not a behavioral test (that needs a browser / Playwright — the remaining depth), but it locks
        // in that a feature's wiring can't silently vanish from console.html.
        for hook in [
            "new EventSource",                 // SSE realtime (1/5)
            "/events",
            "checkAlerts",                     // desktop alerts (2/5)
            "requestPermission",
            "function askForm",                // inline modal forms (3/5)
            "id=\"modal\"",
            "X-Rozum-Actor",                   // named operator identity (4/5)
            "rozumActor",
            "whoami",                          // RBAC role-awareness
            "can(\"responder\")",
            "FEED_WINDOW",                     // feed pagination (latest window + load-older)
            "load older",
            // core action endpoints the UI must drive
            "/escalate",
            "/resolve",
            "/threads/",
            "/messages",
            "/redact",
            "/react",
            "/search",
            "needs_attention",                 // SLA/staleness metric
        ] {
            assert!(body.contains(hook), "console.html missing feature hook: {hook}");
        }
        // No native dialogs should remain (replaced by askForm).
        assert!(!body.contains("prompt("), "console still uses prompt()");

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
