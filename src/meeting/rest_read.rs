//! Read-only HTTP API for daemon-backed meeting transcripts.
//!
//! This is intentionally smaller than `meetings web`: no UI, no submit path, no
//! SSE, and no room mutation. It resolves rooms through the daemon registry and
//! reads day files directly from disk, gated by the same Basic-auth shared secret
//! pattern as the web client.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    Router,
    extract::{Path as AxumPath, Query, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Json, Response},
    routing::get,
};
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
        .route("/rooms/{name}/days", get(days))
        .route("/rooms/{name}/messages/{date}", get(messages))
        .layer(middleware::from_fn_with_state(
            AuthCfg { secret },
            auth_layer,
        ))
        .with_state(RestState { registry })
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
    registry
        .list()
        .into_iter()
        .find(|loc| loc.name == name)
        .map(|loc| loc.root)
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
    let pass = std::str::from_utf8(&dec)
        .ok()
        .and_then(|c| c.split_once(':').map(|(_, p)| p.to_owned()))
        .unwrap_or_default();
    if pass != cfg.secret {
        return unauth();
    }
    next.run(req).await
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
