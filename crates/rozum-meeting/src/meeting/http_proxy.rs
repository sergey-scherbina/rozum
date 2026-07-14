//! `rozum mcp-http` — the meeting MCP bridge over **HTTP** (rmcp streamable-http) instead of a
//! per-session stdio child.
//!
//! Why: the stdio `mcp-proxy` is a per-session child Claude Code spawns; if it dies the
//! `mcp__rozum__*` tools vanish and CC does NOT restart it (BUG-004,
//! `docs/specs/mcp-proxy-resilience.md`). An HTTP endpoint is hosted by a long-lived server that
//! CC connects to as `{type:"http", url}` and **reconnects** to on drop — nothing per-session to
//! crash. This reuses the exact same [`DaemonProxy`] tool surface; only the transport changes.
//!
//! ## Per-project multiplexing (`?project=`)
//!
//! rmcp's session factory is argless (`Fn() -> Result<S>`) — it never sees the request, so a single
//! [`StreamableHttpService`] can only pin ONE project (its default room). That made a scalascript
//! agent connecting to a `rozum`-pinned server keep landing in `rozum`. Fix: keep a lazily-populated
//! map of ONE `StreamableHttpService` per project (each with its own session manager + a factory
//! pinned to that project) and a thin axum layer that reads `?project=<path>` from the request URL
//! and routes to the matching service. A client's configured URL carries the query on every request
//! (including reconnects), so its `Mcp-Session-Id` always resolves within its own project's service.
//! No `?project=` → the server's `--project` (or cwd) default, preserving the old single-project
//! behaviour exactly.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Query, Request, State};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use tower::ServiceExt;

use super::daemon_proxy::{DaemonProxy, install_panic_logger, proxy_log};

/// One rmcp streamable-HTTP service, pinned to a single project's default room.
type ProjectService = StreamableHttpService<DaemonProxy, LocalSessionManager>;

/// Routes each request to a per-project [`ProjectService`], creating it on first use. Cheap to
/// clone (an `Arc` map + an `Option<String>`); shared as axum handler state.
#[derive(Clone)]
struct ProjectRouter {
    /// The `--project` (or cwd) fallback used when a request carries no `?project=`.
    default_project: Option<String>,
    /// project-key → its service. Key is the resolved project string (`""` = daemon cwd default).
    services: Arc<Mutex<HashMap<String, ProjectService>>>,
}

impl ProjectRouter {
    fn new(default_project: Option<String>) -> Self {
        Self {
            default_project,
            services: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Get (or lazily build) the service pinned to `project`, falling back to the server default.
    /// Building a service is cheap — it only stores the factory + a fresh session manager; the
    /// `DaemonProxy` (and thus the room join) is created per-session by rmcp on `initialize`.
    fn service_for(&self, project: Option<String>) -> ProjectService {
        // Empty / whitespace `?project=` is treated as "unset" → server default.
        let resolved = project
            .filter(|s| !s.trim().is_empty())
            .or_else(|| self.default_project.clone());
        let key = resolved.clone().unwrap_or_default();

        let mut map = self.services.lock().expect("project service map poisoned");
        map.entry(key)
            .or_insert_with(|| {
                let pinned = resolved.clone();
                // Per-project factory: a fresh DaemonProxy pinned to THIS project each session.
                let factory = move || Ok(DaemonProxy::for_project(pinned.clone()));
                // stateful (default) → SSE priming for reconnection; loopback allowed_hosts guard.
                StreamableHttpService::new(
                    factory,
                    Arc::new(LocalSessionManager::default()),
                    StreamableHttpServerConfig::default(),
                )
            })
            .clone()
    }
}

/// `?project=<path>` — the repo whose default room the session should land in. Absent → default.
#[derive(serde::Deserialize)]
struct ProjectParam {
    project: Option<String>,
}

/// Single `/mcp` handler: pick the project's service by `?project=` and drive the (Infallible)
/// tower service with the untouched request. `Query` reads only the URI parts, so the full request
/// — query intact — still forwards to rmcp (which ignores the query).
async fn dispatch(
    State(router): State<ProjectRouter>,
    Query(param): Query<ProjectParam>,
    req: Request,
) -> Response {
    let service = router.service_for(param.project);
    match service.oneshot(req).await {
        Ok(resp) => resp.into_response(),
        // `StreamableHttpService::Error` is `Infallible` — this arm is unreachable.
        Err(never) => match never {},
    }
}

/// Serve the meeting MCP tools over streamable-HTTP on `127.0.0.1:port` at path `/mcp`.
///
/// `project` is the DEFAULT room pin (used when a request has no `?project=`); `None` → cwd
/// detection. Any number of projects are served concurrently, each selected by `?project=<path>`
/// in the client's URL — one long-lived server, one room-space per repo. Bound to loopback with
/// rmcp's default loopback `allowed_hosts` (DNS-rebinding guard) on every per-project service.
pub async fn run_http_proxy(
    port: u16,
    project: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    install_panic_logger();
    proxy_log(&format!("http-start port={port} default-project={project:?}"));

    let router = ProjectRouter::new(project);
    let app = axum::Router::new()
        .route("/mcp", axum::routing::any(dispatch))
        .with_state(router);

    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    proxy_log(&format!("http-listening addr=http://{addr}/mcp (per-project ?project= mux)"));
    eprintln!("rozum mcp-http listening on http://{addr}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_for_is_one_per_project_reused_and_defaults() {
        let router = ProjectRouter::new(Some("/default/proj".to_string()));
        // Distinct projects → distinct services.
        let _a = router.service_for(Some("/repo/a".to_string()));
        let _b = router.service_for(Some("/repo/b".to_string()));
        // Repeat of a known project → reuse, NOT a new entry.
        let _a_again = router.service_for(Some("/repo/a".to_string()));
        // Absent / whitespace-only `?project=` both fall back to the server default.
        let _d1 = router.service_for(None);
        let _d2 = router.service_for(Some("   ".to_string()));

        let map = router.services.lock().unwrap();
        assert_eq!(map.len(), 3, "one service per distinct project (a, b, default), reused on repeat");
        assert!(map.contains_key("/repo/a"));
        assert!(map.contains_key("/repo/b"));
        assert!(map.contains_key("/default/proj"), "None and whitespace both resolve to the default");
    }

    #[test]
    fn service_for_with_no_default_keys_on_empty() {
        // No server default + no `?project=` → the daemon-cwd key (empty string), still one service.
        let router = ProjectRouter::new(None);
        let _s = router.service_for(None);
        let map = router.services.lock().unwrap();
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(""));
    }
}
