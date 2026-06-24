//! `rozum mcp-http` — the meeting MCP bridge over **HTTP** (rmcp streamable-http) instead of a
//! per-session stdio child.
//!
//! Why: the stdio `mcp-proxy` is a per-session child Claude Code spawns; if it dies the
//! `mcp__rozum__*` tools vanish and CC does NOT restart it (BUG-004,
//! `docs/specs/mcp-proxy-resilience.md`). An HTTP endpoint is hosted by a long-lived server that
//! CC connects to as `{type:"http", url}` and **reconnects** to on drop — nothing per-session to
//! crash. This reuses the exact same [`DaemonProxy`] tool surface; only the transport changes.
//!
//! Spike scope: one project per server instance (from `--project`, else cwd). Multiplexing many
//! projects through one daemon via `?project=…` in the URL is the documented follow-up — it needs
//! a per-project service map keyed off the request, which rmcp's argless session factory doesn't
//! expose directly.

use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};

use super::daemon_proxy::{DaemonProxy, proxy_log};

/// Serve the meeting MCP tools over streamable-HTTP on `127.0.0.1:port` at path `/mcp`.
///
/// `project` pins the room (None → cwd detection). Stateful sessions are on (the rmcp default),
/// so a dropped connection can resume via its `Mcp-Session-Id` rather than re-initializing. Bound
/// to loopback with rmcp's default loopback `allowed_hosts` (DNS-rebinding guard).
pub async fn run_http_proxy(
    port: u16,
    project: Option<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    super::daemon_proxy::install_panic_logger();
    proxy_log(&format!("http-start port={port} project={project:?}"));

    // Per-session factory: a fresh DaemonProxy pinned to this server's project. `Fn` (called per
    // new session), `Send + Sync + 'static` (the project String is cloned each call).
    let factory = move || Ok(DaemonProxy::for_project(project.clone()));

    // stateful_mode = true (default) → SSE priming events for reconnection; loopback-only hosts.
    let config = StreamableHttpServerConfig::default();
    let service =
        StreamableHttpService::new(factory, Arc::new(LocalSessionManager::default()), config);

    let app = axum::Router::new().nest_service("/mcp", service);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    proxy_log(&format!("http-listening addr=http://{addr}/mcp"));
    eprintln!("rozum mcp-http listening on http://{addr}/mcp");
    axum::serve(listener, app).await?;
    Ok(())
}
