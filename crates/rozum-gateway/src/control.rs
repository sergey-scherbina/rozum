//! Control-API for the models/gateway service — the read aggregation a dashboard (the CLI today, the
//! future UCC client) consumes: the active shared gateway, the host residency ledger, and the
//! installed model catalog. The symmetric counterpart to `rozum-meeting::client`; the same snapshot is
//! served over the gateway's HTTP surface for the web/UCC target. See
//! `docs/specs/services-and-clients.md`.

use serde::Serialize;

/// Run a tiny always-up HTTP server exposing the control snapshot, independent of any running
/// gateway (it reads the host residency ledger + catalog from disk). `GET /control/status` → the
/// `status()` JSON, with permissive CORS so a web/UCC client on another origin can fetch it. For the
/// Tailscale path-routed case the client fetches it same-origin and CORS is moot.
pub async fn serve(port: u16) -> std::io::Result<()> {
    use axum::{response::IntoResponse, routing::get, Router};
    async fn status_route() -> impl IntoResponse {
        ([(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], axum::Json(status().await))
    }
    let app = Router::new().route("/control/status", get(status_route));
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("control server: http://{addr}/control/status");
    axum::serve(listener, app).await
}

/// A coherent snapshot of the models/gateway service.
#[derive(Debug, Clone, Serialize)]
pub struct ControlStatus {
    /// The active shared gateway, or `None` if none is running.
    pub gateway: Option<GatewayStatus>,
    /// Host residency (RAM budget / committed / available / resident set).
    pub residency: ResidencyStatus,
    /// Installed local models (the catalog).
    pub installed: Vec<InstalledBrief>,
    /// Flat, display-ready residency metrics (gateway label + GiB-formatted RAM). An ARRAY so a
    /// declarative table (`remoteTable(st, cols, "residency_metrics")`) renders them identically on
    /// web AND tui — no client-side `computedSignal` (which the tui backend can't recompute).
    pub residency_metrics: Vec<MetricBrief>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GatewayStatus {
    pub model: String,
    pub port: u16,
    pub pid: u32,
    pub n_ctx: u32,
    pub generation: u64,
    pub uptime_secs: u64,
    pub clients: usize,
    pub healthy: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResidencyStatus {
    pub host_budget_bytes: Option<u64>,
    pub committed_bytes: u64,
    pub available_bytes: Option<u64>,
    pub residents: Vec<ResidentBrief>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResidentBrief {
    pub pid: u32,
    pub model: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledBrief {
    pub spec: String,
    pub size_bytes: u64,
    /// GiB-formatted size for direct display in a declarative table column.
    pub size_gib: String,
}

/// A flat `{metric, value}` pair — a row in the display-ready `residency_metrics` table.
#[derive(Debug, Clone, Serialize)]
pub struct MetricBrief {
    pub metric: String,
    pub value: String,
}

/// Format a byte count as a one-decimal GiB string (e.g. `"25.1 GiB"`).
fn fmt_gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / 1_073_741_824.0)
}

/// Aggregate the live models/gateway control status — the active gateway (if any), the host residency
/// ledger, and the installed catalog. Read-only; never loads a model or invokes an engine.
pub async fn status() -> ControlStatus {
    use crate::share;
    let gateway = match share::read_active() {
        Some(a) => Some(GatewayStatus {
            healthy: share::health_ok(a.port).await,
            uptime_secs: share::now_unix().saturating_sub(a.started_at),
            clients: share::live_lease_count(share::LEASE_FRESH_SECS),
            model: a.model,
            port: a.port,
            pid: a.pid,
            n_ctx: a.n_ctx,
            generation: a.generation,
        }),
        None => None,
    };
    let residency = ResidencyStatus {
        host_budget_bytes: share::host_ram_budget_bytes(),
        committed_bytes: share::committed_by_others_bytes(0), // skip nothing → the whole ledger
        available_bytes: share::available_ram_for_admission(),
        residents: share::list_residents()
            .into_iter()
            .map(|(pid, model)| ResidentBrief { pid, model })
            .collect(),
    };
    let installed = rozum_models::models::scan_all_installed()
        .into_iter()
        .map(|m| InstalledBrief { size_gib: fmt_gib(m.size_bytes), spec: m.spec, size_bytes: m.size_bytes })
        .collect();
    let residency_metrics = vec![
        MetricBrief {
            metric: "gateway".into(),
            value: gateway.as_ref().map(|g| g.model.clone()).unwrap_or_else(|| "none running".into()),
        },
        MetricBrief {
            metric: "available".into(),
            value: residency.available_bytes.map(fmt_gib).unwrap_or_else(|| "—".into()),
        },
        MetricBrief {
            metric: "host budget".into(),
            value: residency.host_budget_bytes.map(fmt_gib).unwrap_or_else(|| "—".into()),
        },
        MetricBrief { metric: "committed".into(), value: fmt_gib(residency.committed_bytes) },
        MetricBrief { metric: "residents".into(), value: residency.residents.len().to_string() },
    ];
    ControlStatus { gateway, residency, installed, residency_metrics }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn status_snapshots_and_serializes() {
        // Env-independent: it aggregates a coherent snapshot (residency + catalog) without panicking,
        // and serializes to the JSON contract the HTTP/UCC consumer reads.
        let s = status().await;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"residency\""));
        assert!(json.contains("\"installed\""));
        // residents in the ledger each have a non-empty model name.
        assert!(s.residency.residents.iter().all(|r| !r.model.is_empty()));
    }
}
