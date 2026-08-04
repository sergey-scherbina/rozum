//! The supervisor, over HTTP.
//!
//! A Telegram bot, the UCC, a shell script and a cron job all want the same seven verbs —
//! spawn, list, status, tell, pause, resume, stop, kill — and none of them want to be
//! linked into nadia to get them. So the protocol is the surface, and a front-end is
//! whatever speaks it.
//!
//! Deliberately *not* a second Telegram bot inside nadia. rozum already runs one
//! (`com.rozum.telegram`, with its per-room rosters and its own ACL); a bot that maps
//! `/spawn fix the flaky test` onto `POST /agents` is a dozen lines there, and everything
//! about who may spawn an agent stays where the identities already live. Two bots would
//! mean two ACLs, and the second one would be the one nobody remembered to update.
//!
//! Auth is a shared secret, required unless the listener is on loopback. That is the same
//! bargain the rest of this project makes: a local-only port is already behind the machine
//! boundary, and anything reachable from elsewhere must prove it was invited.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use rozum_agent::agent::Budget;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::supervisor::{AgentId, Spec, Status, Supervisor};

#[derive(Clone)]
pub struct Config {
    pub workspace: PathBuf,
    pub gateway: String,
    pub model: String,
    pub budget: Budget,
    pub allow_net: bool,
    pub confine: bool,
    /// Required in the `x-nadia-token` header. Empty means "loopback only, no token".
    pub token: String,
}

#[derive(Clone)]
struct App {
    sup: Arc<Supervisor>,
    cfg: Config,
}

#[derive(Deserialize)]
struct SpawnBody {
    task: String,
    /// Defaults to the server's workspace. A front-end that wants an isolated tree says so.
    workspace: Option<String>,
}

#[derive(Deserialize)]
struct TellBody {
    message: String,
}

pub fn router(sup: Arc<Supervisor>, cfg: Config) -> Router {
    Router::new()
        .route("/agents", get(list).post(spawn))
        .route("/agents/{id}", get(status).delete(kill))
        .route("/agents/{id}/tell", post(tell))
        .route("/agents/{id}/pause", post(pause))
        .route("/agents/{id}/resume", post(resume))
        .route("/agents/{id}/stop", post(stop))
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .with_state(App { sup, cfg })
}

pub async fn serve(sup: Arc<Supervisor>, cfg: Config, addr: SocketAddr) -> Result<(), String> {
    if cfg.token.is_empty() && !addr.ip().is_loopback() {
        // Refuse rather than warn. A control surface that can start processes on a
        // non-loopback port with no token is not a configuration mistake to be logged,
        // it is an open remote-execution endpoint.
        return Err(format!(
            "refusing to listen on {addr} without a token — pass --token, or bind 127.0.0.1"
        ));
    }
    let listener = tokio::net::TcpListener::bind(addr).await.map_err(|e| format!("bind {addr}: {e}"))?;
    axum::serve(listener, router(sup, cfg)).await.map_err(|e| e.to_string())
}

fn authorized(cfg: &Config, headers: &HeaderMap) -> bool {
    if cfg.token.is_empty() {
        return true;
    }
    headers
        .get("x-nadia-token")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == cfg.token)
        .unwrap_or(false)
}

fn as_json(s: &Status) -> Value {
    json!({
        "id": s.id,
        "parent": s.parent,
        "task": s.task,
        "workspace": s.workspace,
        "phase": s.phase.label(),
        "tool_calls": s.tool_calls,
        "last_tool": s.last_tool,
        "elapsed_secs": s.elapsed.as_secs(),
        "result": s.result,
        // Which files it actually wrote — the question every front-end asks next, and the
        // one a model's own summary is not a reliable answer to.
        "touched": s.touched,
        // What the verify gate concluded — the ground truth about this run, as opposed to the
        // model's own summary of it. `checked: null` means nothing was checkable, said out loud
        // rather than left to look like a pass.
        "check": s.report.check,
        "checked": s.report.passed,
        "check_detail": s.report.detail,
        "repairs": s.report.rounds,
    })
}

type Reply = (StatusCode, Json<Value>);

fn ok(v: Value) -> Reply {
    (StatusCode::OK, Json(v))
}

fn err(code: StatusCode, message: String) -> Reply {
    (code, Json(json!({"error": message})))
}

/// Map a supervisor error onto a status code. "no agent 7" is a 404, and everything else
/// is a refusal the caller can read — front-ends show these to a human verbatim.
fn from_supervisor(e: String) -> Reply {
    let code = if e.starts_with("no agent ") { StatusCode::NOT_FOUND } else { StatusCode::CONFLICT };
    err(code, e)
}

async fn list(State(app): State<App>, headers: HeaderMap) -> Reply {
    if !authorized(&app.cfg, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad or missing x-nadia-token".into());
    }
    ok(json!({"agents": app.sup.list().iter().map(as_json).collect::<Vec<_>>()}))
}

async fn spawn(State(app): State<App>, headers: HeaderMap, Json(body): Json<SpawnBody>) -> Reply {
    if !authorized(&app.cfg, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad or missing x-nadia-token".into());
    }
    if body.task.trim().is_empty() {
        return err(StatusCode::BAD_REQUEST, "task is empty".into());
    }
    let workspace = body.workspace.map(PathBuf::from).unwrap_or_else(|| app.cfg.workspace.clone());
    let spec = Spec {
        task: body.task,
        workspace,
        gateway: app.cfg.gateway.clone(),
        model: app.cfg.model.clone(),
        budget: app.cfg.budget.clone(),
        parent: None,
        allow_net: app.cfg.allow_net,
        confine: app.cfg.confine,
    };
    match app.sup.spawn(spec) {
        Ok(id) => ok(json!({"id": id})),
        Err(e) => err(StatusCode::BAD_REQUEST, e),
    }
}

async fn status(State(app): State<App>, headers: HeaderMap, Path(id): Path<AgentId>) -> Reply {
    if !authorized(&app.cfg, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad or missing x-nadia-token".into());
    }
    match app.sup.status(id) {
        Ok(s) => ok(as_json(&s)),
        Err(e) => from_supervisor(e),
    }
}

async fn tell(
    State(app): State<App>,
    headers: HeaderMap,
    Path(id): Path<AgentId>,
    Json(body): Json<TellBody>,
) -> Reply {
    if !authorized(&app.cfg, &headers) {
        return err(StatusCode::UNAUTHORIZED, "bad or missing x-nadia-token".into());
    }
    match app.sup.tell(id, body.message) {
        Ok(()) => ok(json!({"queued": true})),
        Err(e) => from_supervisor(e),
    }
}

macro_rules! control_route {
    ($name:ident, $call:ident, $field:literal) => {
        async fn $name(State(app): State<App>, headers: HeaderMap, Path(id): Path<AgentId>) -> Reply {
            if !authorized(&app.cfg, &headers) {
                return err(StatusCode::UNAUTHORIZED, "bad or missing x-nadia-token".into());
            }
            match app.sup.$call(id) {
                Ok(()) => ok(json!({ $field: true })),
                Err(e) => from_supervisor(e),
            }
        }
    };
}

control_route!(pause, pause, "paused");
control_route!(resume, resume, "resumed");
control_route!(stop, stop, "stopping");
control_route!(kill, kill, "killed");

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(token: &str) -> Config {
        Config {
            workspace: std::env::temp_dir(),
            gateway: "http://127.0.0.1:1/v1".into(),
            model: "test".into(),
            budget: crate::session::default_budget(),
            allow_net: false,
            confine: false,
            token: token.into(),
        }
    }

    #[test]
    fn a_token_is_required_once_one_is_set() {
        let c = cfg("s3cret");
        let mut h = HeaderMap::new();
        assert!(!authorized(&c, &h), "no header must not pass");
        h.insert("x-nadia-token", "wrong".parse().unwrap());
        assert!(!authorized(&c, &h));
        h.insert("x-nadia-token", "s3cret".parse().unwrap());
        assert!(authorized(&c, &h));
    }

    #[test]
    fn no_token_configured_means_open() {
        assert!(authorized(&cfg(""), &HeaderMap::new()));
    }

    #[tokio::test]
    async fn a_tokenless_listener_off_loopback_is_refused() {
        // The failure this prevents: binding 0.0.0.0 with no token publishes an endpoint
        // that starts processes for anyone who can reach the port.
        let e = serve(Supervisor::new(), cfg(""), "0.0.0.0:0".parse().unwrap()).await.unwrap_err();
        assert!(e.contains("refusing to listen"), "{e}");
    }

    #[test]
    fn a_missing_agent_is_404_and_everything_else_is_a_conflict() {
        let (code, _) = from_supervisor("no agent 7".into());
        assert_eq!(code, StatusCode::NOT_FOUND);
        let (code, _) = from_supervisor("agent 7 was killed".into());
        assert_eq!(code, StatusCode::CONFLICT);
    }
}
