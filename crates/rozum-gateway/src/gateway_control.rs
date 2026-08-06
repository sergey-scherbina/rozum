//! Making sure a gateway is up and serving the model a request needs.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Thirteen items: reuse a healthy gateway,
//! switch it if it holds a different model, cold-start one if the registry record is stale, and
//! track a download in progress so the console can show it rather than hanging on a request that
//! will take minutes.
//!
//! It came out ahead of the agent and coder routes, which reach it through `spawn_launch_task` —
//! the same ordering that has kept every module in this refactor pointing downward. Measured on the
//! way in: with `loading_models`, `LoadState` and the same-site guard included, the family calls
//! NOTHING outside itself.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::errors::json_err;

/// Run a `rozum` subcommand to completion, capturing output. Uses the current binary (the full gateway
/// CLI), so no PATH dependency. Returns (success, combined stdout+stderr).
pub(crate) fn run_rozum(args: &[&str]) -> (bool, String) {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    match Command::new(&exe).args(args).output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), s)
        }
        Err(e) => (false, format!("spawn failed: {e}")),
    }
}

/// Ensure the shared gateway serves `model`. Reuses it if already serving; `gateway switch` swaps
/// the resident model in place when a different one is loaded (the switch runs the residency
/// admission gate itself and exits non-zero if it won't fit); when NO gateway is running at all —
/// the cold-start case `gateway switch` refuses — spawn a detached daemon like `rozum launch` does
/// and wait for it to come up. Returns the gateway port on success, or an error message (incl. an
/// admission refusal) on failure.
pub(crate) async fn ensure_gateway(model: &str) -> Result<u16, String> {
    if let Some(g) = crate::share::read_active() {
        if crate::share::health_ok(g.port).await {
            if g.model == model {
                return Ok(g.port);
            }
            let (ok, out) = run_rozum(&["gateway", "switch", "--model", model]);
            if !ok {
                return Err(format!("could not load {model}: {}", out.trim()));
            }
            return crate::share::read_active()
                .map(|g| g.port)
                .ok_or_else(|| "gateway not running after load".into());
        }
        // Stale registry record (gateway died without cleanup): fall through to a fresh
        // start — the new daemon bumps `generation` and overwrites the record.
    }
    cold_start_gateway(model).await
}

/// In-flight model loads driven by `POST /control/gateway/load`. Presence = loading (async); a set
/// `error` = the last attempt failed (kept so the panel shows why, cleared on retry). Lets an uncached
/// model download for as long as it needs without the phone's request blocking or a false timeout.
pub(crate) struct LoadState {
    pub(crate) started: u64,
    pub(crate) error: Option<String>,
}

pub(crate) fn loading_models() -> &'static std::sync::Mutex<std::collections::HashMap<String, LoadState>> {
    use std::sync::OnceLock;
    static L: OnceLock<std::sync::Mutex<std::collections::HashMap<String, LoadState>>> = OnceLock::new();
    L.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Background load driven by `gateway_load_route`: the same resolution as `ensure_gateway` (reuse a
/// healthy same-model gateway, else switch, else cold-start) but with a generous wait so a multi-GB
/// download completes, and the outcome recorded in `loading_models()` for the status endpoint.
pub(crate) async fn gateway_load_bg(model: String) {
    let result: Result<u16, String> = async {
        if let Some(g) = crate::share::read_active() {
            if crate::share::health_ok(g.port).await {
                if g.model == model {
                    return Ok(g.port);
                }
                let (ok, out) = run_rozum(&["gateway", "switch", "--model", &model]);
                if !ok {
                    return Err(format!("could not load {model}: {}", out.trim()));
                }
                return crate::share::read_active()
                    .map(|g| g.port)
                    .ok_or_else(|| "gateway not running after load".into());
            }
        }
        // 30 min ceiling: enough for the largest weights on a slow link; a real load stall still
        // fails via child-exit long before this. The RAM gate's own refusal also lands here.
        cold_start_gateway_wait(&model, std::time::Duration::from_secs(1800)).await
    }
    .await;
    let mut l = loading_models().lock().unwrap();
    match result {
        Ok(_) => {
            l.remove(&model); // now resident — the status endpoint shows the unload button
        }
        Err(e) => {
            if let Some(s) = l.get_mut(&model) {
                s.error = Some(e); // keep the entry so the panel surfaces why; cleared on retry
            }
        }
    }
}

/// Spawn a detached `rozum gateway --model … --port <default>` daemon — the same shape
/// `rozum launch`'s spawn_detached_gateway uses — and wait for it to register and answer health.
/// The daemon runs the residency admission gate on startup and exits non-zero on refusal, which
/// surfaces here as the error; model-load progress/errors land in gateway.log.
pub(crate) async fn cold_start_gateway(model: &str) -> Result<u16, String> {
    cold_start_gateway_wait(model, std::time::Duration::from_secs(300)).await
}

pub(crate) async fn cold_start_gateway_wait(model: &str, max_wait: std::time::Duration) -> Result<u16, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = crate::share::gateway_dir();
    let _ = std::fs::create_dir_all(&dir);
    let log_path = dir.join("gateway.log");
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| format!("open {}: {e}", log_path.display()))?;
    let log2 = log.try_clone().map_err(|e| e.to_string())?;
    let port = crate::share::DEFAULT_GATEWAY_PORT;
    let mut cmd = Command::new(&exe);
    cmd.args(["gateway", "--model", model, "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0); // detach from control-serve's group so it survives a service restart
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn gateway daemon: {e}"))?;
    let deadline = std::time::Instant::now() + max_wait;
    loop {
        if crate::share::health_ok(port).await {
            return crate::share::read_active()
                .map(|g| g.port)
                .ok_or_else(|| "gateway healthy but not registered".into());
        }
        if let Ok(Some(status)) = child.try_wait() {
            return Err(format!(
                "gateway daemon exited before becoming ready ({status}); see {}",
                log_path.display()
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "gateway not ready after {}s (still loading/downloading?); see {}",
                max_wait.as_secs(),
                log_path.display()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// CSRF guard for state-changing GET routes (the SPA drives gateway load/stop via same-origin `<a>`
/// anchors). Browsers send `Sec-Fetch-Site: same-origin` for the SPA's own links, `none` for a typed
/// URL/bookmark, and `cross-site` for an attacker's cross-site link/redirect — reject only the last so a
/// cross-site navigation can't ride the SameSite=Lax session cookie. Absent header (non-browser) → allow.
pub(crate) fn same_site_get(headers: &axum::http::HeaderMap) -> bool {
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some(s) => matches!(s, "same-origin" | "same-site" | "none"),
        None => true,
    }
}

#[derive(Deserialize)]
pub(crate) struct GatewayLoadReq { model: String }

pub(crate) async fn gateway_load_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Accept JSON {"model":"..."} (from the chat form) OR plain-text spec (from rowPost table action).
    let model = serde_json::from_str::<GatewayLoadReq>(&body)
        .map(|r| r.model)
        .unwrap_or_else(|_| body.trim().to_string());
    if model.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model required");
    }
    // Fast path: already resident+healthy for this exact model — nothing to do.
    if let Some(g) = crate::share::read_active() {
        if g.model == model && crate::share::health_ok(g.port).await {
            return axum::Json(serde_json::json!({ "ok": true, "model": model, "port": g.port, "status": "resident" })).into_response();
        }
    }
    // ASYNC: a load can take minutes (a cold gateway, or a multi-GB download of an uncached model).
    // Blocking the phone's request for that long timed out with a false "not ready" while the download
    // ran on fine. Instead kick the load off in the background, track it in `loading_models()`, and
    // return immediately — `/control/status` reports the model's status (загрузка…/✗ error) and the
    // panel shows it. Idempotent: a second tap while loading is a no-op; a tap after a failure retries.
    {
        let mut l = loading_models().lock().unwrap();
        if l.get(&model).map_or(false, |s| s.error.is_none()) {
            return axum::Json(serde_json::json!({ "ok": true, "model": model, "status": "loading" })).into_response();
        }
        l.insert(model.clone(), LoadState { started: crate::share::now_unix(), error: None });
    }
    tokio::spawn(gateway_load_bg(model.clone()));
    axum::Json(serde_json::json!({ "ok": true, "model": model, "status": "loading" })).into_response()
}

// GET variants for SPA per-row links: load/stop via a plain anchor → redirect back to /.
#[derive(Deserialize)]
pub(crate) struct GatewayLoadQuery { model: String }

pub(crate) async fn gateway_load_get_route(
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<GatewayLoadQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !same_site_get(&headers) {
        return json_err(axum::http::StatusCode::FORBIDDEN, "cross-site request refused");
    }
    let model = q.model.trim().to_string();
    if model.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model required");
    }
    let _ = ensure_gateway(&model).await;
    axum::response::Redirect::to("/").into_response()
}

/// Like `run_rozum`, but feeds `stdin_data` to the child — the only safe way to hand a secret to
/// a subprocess, since arguments are world-readable via `ps`.
pub(crate) fn run_rozum_stdin(args: &[String], stdin_data: &str) -> (bool, String) {
    use std::io::Write;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let child = Command::new(&exe)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return (false, format!("spawn failed: {e}")),
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(stdin_data.as_bytes());
    }
    match child.wait_with_output() {
        Ok(out) => {
            let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), s)
        }
        Err(e) => (false, format!("wait failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_site_get_rejects_only_cross_site() {
        // Regression guard for the CSRF-on-GET fix: reject a cross-site navigation, allow the SPA's own
        // same-origin anchors, a typed URL (`none`), and non-browser clients (absent header).
        use axum::http::HeaderMap;
        let hm = |v: &str| { let mut h = HeaderMap::new(); h.insert("sec-fetch-site", v.parse().unwrap()); h };
        assert!(same_site_get(&hm("same-origin")));
        assert!(same_site_get(&hm("same-site")));
        assert!(same_site_get(&hm("none")));
        assert!(!same_site_get(&hm("cross-site")));
        assert!(same_site_get(&HeaderMap::new()));
    }
}
