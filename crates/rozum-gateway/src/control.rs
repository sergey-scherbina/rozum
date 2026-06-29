//! Control-API for the models/gateway service — the read aggregation a dashboard (the CLI today, the
//! future UCC client) consumes: the active shared gateway, the host residency ledger, and the
//! installed model catalog. The symmetric counterpart to `rozum-meeting::client`; the same snapshot is
//! served over the gateway's HTTP surface for the web/UCC target. See
//! `docs/specs/services-and-clients.md`.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// Run a tiny always-up HTTP server exposing the control snapshot, independent of any running
/// gateway (it reads the host residency ledger + catalog from disk). `GET /control/status` → the
/// `status()` JSON, with permissive CORS so a web/UCC client on another origin can fetch it. For the
/// Tailscale path-routed case the client fetches it same-origin and CORS is moot.
///
/// Beyond the read snapshot it now also exposes WRITE actions so the UCC can drive agents from a
/// phone: `POST /control/agent/launch|stop`, `/control/gateway/load`, `/control/task`. Every action
/// that loads a model is admission-gated (`rozum gateway switch` runs the residency gate itself), and
/// the launch reports a refusal verdict rather than risking an OOM.
pub async fn serve(port: u16) -> std::io::Result<()> {
    use axum::{response::IntoResponse, routing::{get, post}, Router};
    async fn status_route() -> impl IntoResponse {
        ([(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], axum::Json(status().await))
    }
    // Public: SPA static files + read snapshot + auth ceremony + chat reads.
    let public = Router::new()
        .route("/", get(spa_root_route))
        .route("/{*path}", get(spa_static_route))
        .route("/control/status", get(status_route))
        .route("/control/auth/status", get(auth_status_route))
        .route("/control/auth/register/begin", post(register_begin_route))
        .route("/control/auth/register/finish", post(register_finish_route))
        .route("/control/auth/login/begin", post(login_begin_route))
        .route("/control/auth/login/finish", post(login_finish_route))
        .route("/chat/messages", get(chat_messages_route))
        .route("/chat/incidents", get(chat_incidents_route));
    // Protected by `require_auth` (own Face ID session OR busi SSO): every write action + the terminal WS.
    let protected = Router::new()
        .route("/chat/post", post(chat_post_route))
        .route("/control/gateway/load", post(gateway_load_route))
        .route("/control/agent/launch", post(agent_launch_route))
        .route("/control/agent/stop", post(agent_stop_route))
        .route("/control/task", post(task_route))
        .route("/control/coder/launch", post(coder_launch_route))
        .route("/control/coder/stop", post(coder_stop_route))
        .route("/control/coder/log", get(coder_log_route))
        .route("/control/session/launch", post(session_launch_route))
        .route("/control/session/stop", post(session_stop_route))
        .route("/control/session/attach/{id}", get(session_attach_route))
        .route_layer(axum::middleware::from_fn(require_auth));
    let app = public.merge(protected);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    eprintln!("control server: http://{addr}/ (SPA + API, no Python proxy needed)");
    axum::serve(listener, app).await
}

// ── Static SPA file serving (replaces ucc-web-server.py) ─────────────────────────────────────────

fn ucc_site_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".rozum/ucc/site")
}

fn serve_site_file(name: &str) -> axum::response::Response {
    use axum::{http::{header, StatusCode}, response::IntoResponse};
    let path = ucc_site_dir().join(name);
    match std::fs::read(&path) {
        Ok(bytes) => {
            let mime = match path.extension().and_then(|e| e.to_str()) {
                Some("html") => "text/html; charset=utf-8",
                Some("js")   => "application/javascript",
                Some("css")  => "text/css",
                Some("svg")  => "image/svg+xml",
                Some("png")  => "image/png",
                Some("ico")  => "image/x-icon",
                Some("webmanifest") => "application/manifest+json",
                _ => "application/octet-stream",
            };
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn spa_root_route() -> axum::response::Response {
    serve_site_file("index.html")
}

async fn spa_static_route(
    axum::extract::Path(path): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    // Guard against path traversal; only serve known static extensions.
    if path.contains("..") || path.starts_with('/') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let allowed = matches!(
        std::path::Path::new(&path).extension().and_then(|e| e.to_str()),
        Some("html" | "js" | "css" | "svg" | "png" | "ico" | "webmanifest" | "txt")
    );
    if !allowed {
        return StatusCode::NOT_FOUND.into_response();
    }
    serve_site_file(&path)
}

// ── Chat read/write endpoints (replaces ucc-web-server.py proxy logic) ───────────────────────────

#[derive(serde::Deserialize)]
struct RoomQuery { room: String }

async fn chat_messages_route(
    axum::extract::Query(q): axum::extract::Query<RoomQuery>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    if !valid_room_name(&q.room) { return StatusCode::BAD_REQUEST.into_response(); }
    axum::Json(read_room_messages(&q.room, 80)).into_response()
}

async fn chat_incidents_route(
    axum::extract::Query(q): axum::extract::Query<RoomQuery>,
) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse};
    if !valid_room_name(&q.room) { return StatusCode::BAD_REQUEST.into_response(); }
    axum::Json(read_room_incidents(&q.room)).into_response()
}

async fn chat_post_route(
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

fn valid_room_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.chars().any(|c| matches!(c, '\r' | '\n' | '\0' | '/'))
}

fn rooms_json_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("rooms.json"))
}

fn room_root(name: &str) -> Option<PathBuf> {
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
struct ChatMessage { time: String, author: String, content: String }

fn read_room_messages(room: &str, limit: usize) -> Vec<ChatMessage> {
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

#[derive(serde::Serialize)]
struct Incident { severity: String, state: String, title: String, owner: String }

fn read_room_incidents(room: &str) -> Vec<Incident> {
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

// ── Agent registry + actions (write side of the control API) ─────────────────────────────────────
//
// Chat-agents are `rozum meetings participant` processes. There is no built-in registry, so we keep a
// small JSON file (`<state>/rozum/ucc-agents.json`) the launch/stop maintain, keyed by a generated id,
// and check liveness with `kill(pid, 0)`. Actions exec the `rozum` CLI via the current binary (which IS
// the full gateway CLI) so there is no extra crate dependency, mirroring `list_meetings`' read-only
// disk approach.

/// One launched chat-agent we track. Persisted to the registry; `alive` is computed per status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRecord {
    pub id: String,
    pub model: String,
    pub room: String,
    pub handle: String,
    pub policy: String,
    pub pid: u32,
    pub started_at: u64,
}

/// Display brief for a running agent (registry entry + liveness), in `ControlStatus.agents`.
#[derive(Debug, Clone, Serialize)]
pub struct AgentBrief {
    pub id: String,
    pub model: String,
    pub room: String,
    pub handle: String,
    pub policy: String,
    pub pid: u32,
    pub alive: bool,
}

fn state_dir() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .map(|b| b.join("rozum"))
}

fn agents_registry_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("ucc-agents.json"))
}

fn load_agents() -> Vec<AgentRecord> {
    agents_registry_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_agents(agents: &[AgentRecord]) {
    if let Some(p) = agents_registry_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(body) = serde_json::to_vec_pretty(agents) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

/// Is `pid` still alive? `kill(pid, 0)` returns 0 for a live process we can signal.
fn pid_alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

/// The agents we currently know about, with a fresh liveness check; prunes dead ones from the registry.
fn live_agents() -> Vec<AgentBrief> {
    let all = load_agents();
    let (alive, dead): (Vec<_>, Vec<_>) = all.into_iter().partition(|a| pid_alive(a.pid));
    if !dead.is_empty() {
        save_agents(&alive); // self-heal: drop processes that have exited
    }
    alive
        .into_iter()
        .map(|a| AgentBrief {
            id: a.id, model: a.model, room: a.room, handle: a.handle, policy: a.policy,
            pid: a.pid, alive: true,
        })
        .collect()
}

/// Run a `rozum` subcommand to completion, capturing output. Uses the current binary (the full gateway
/// CLI), so no PATH dependency. Returns (success, combined stdout+stderr).
fn run_rozum(args: &[&str]) -> (bool, String) {
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

/// Ensure the shared gateway serves `model`. Reuses it if already serving; else `gateway switch`
/// (which runs the residency admission gate itself and exits non-zero if it won't fit). Returns the
/// gateway port on success, or an error message (incl. an admission refusal) on failure.
fn ensure_gateway(model: &str) -> Result<u16, String> {
    if let Some(g) = crate::share::read_active() {
        if g.model == model {
            return Ok(g.port);
        }
    }
    let (ok, out) = run_rozum(&["gateway", "switch", "--model", model]);
    if !ok {
        return Err(format!("could not load {model}: {}", out.trim()));
    }
    crate::share::read_active().map(|g| g.port).ok_or_else(|| "gateway not running after load".into())
}

/// Spawn `rozum meetings participant …` DETACHED (own process group, output → a per-agent log), so it
/// survives a control-serve restart. Returns the child pid.
fn spawn_participant(model: &str, room: &str, policy: &str, persona: &str, gw_port: u16) -> std::io::Result<u32> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let gw_url = format!("http://127.0.0.1:{gw_port}/v1");
    let log_path = state_dir()
        .map(|d| d.join("logs"))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::create_dir_all(&log_path);
    let log = std::fs::OpenOptions::new()
        .create(true).append(true)
        .open(log_path.join(format!("agent-{}-{}.log", sanitize(room), sanitize(model))))?;
    let log2 = log.try_clone()?;
    let mut cmd = Command::new(&exe);
    cmd.args(["meetings", "participant", "--model", model, "--room", room, "--reply-policy", policy]);
    if !persona.is_empty() {
        cmd.args(["--persona", persona]);
    }
    cmd.args(["--gateway-url", &gw_url]);
    cmd.stdin(Stdio::null()).stdout(Stdio::from(log)).stderr(Stdio::from(log2));
    cmd.process_group(0); // detach from control-serve's group so it survives a service restart
    Ok(cmd.spawn()?.id())
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

// ── Action route handlers ────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GatewayLoadReq { model: String }

async fn gateway_load_route(axum::Json(req): axum::Json<GatewayLoadReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let model = req.model.trim().to_string();
    if model.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model required");
    }
    match ensure_gateway(&model) {
        Ok(port) => axum::Json(serde_json::json!({ "ok": true, "model": model, "port": port })).into_response(),
        Err(e) => json_err(axum::http::StatusCode::CONFLICT, &e),
    }
}

#[derive(Deserialize)]
struct AgentLaunchReq {
    model: String,
    room: String,
    #[serde(default = "default_policy")]
    policy: String,
    #[serde(default)]
    persona: String,
}
fn default_policy() -> String { "mention".into() }

async fn agent_launch_route(axum::Json(req): axum::Json<AgentLaunchReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let model = req.model.trim().to_string();
    let room = req.room.trim().to_string();
    if model.is_empty() || room.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model and room required");
    }
    // 1) Ensure the gateway serves the model — admission-gated; a refusal returns the verdict.
    let port = match ensure_gateway(&model) {
        Ok(p) => p,
        Err(e) => {
            let report = footprint_report(&model);
            return (
                axum::http::StatusCode::CONFLICT,
                axum::Json(serde_json::json!({ "ok": false, "error": e, "admission": report })),
            ).into_response();
        }
    };
    // 2) Spawn the participant detached + register it.
    match spawn_participant(&model, &room, &req.policy, req.persona.trim(), port) {
        Ok(pid) => {
            let handle = derive_handle(&model);
            let id = format!("{}-{}", sanitize(&room), pid);
            let mut agents = load_agents();
            agents.push(AgentRecord {
                id: id.clone(), model: model.clone(), room: room.clone(),
                handle: handle.clone(), policy: req.policy.clone(), pid,
                started_at: crate::share::now_unix(),
            });
            save_agents(&agents);
            axum::Json(serde_json::json!({ "ok": true, "id": id, "handle": handle, "pid": pid })).into_response()
        }
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("spawn: {e}")),
    }
}

#[derive(Deserialize)]
struct AgentStopReq { id: String }

async fn agent_stop_route(axum::Json(req): axum::Json<AgentStopReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut agents = load_agents();
    let Some(pos) = agents.iter().position(|a| a.id == req.id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such agent");
    };
    let a = agents.remove(pos);
    unsafe { libc::kill(a.pid as libc::pid_t, libc::SIGTERM); }
    save_agents(&agents);
    axum::Json(serde_json::json!({ "ok": true, "id": a.id })).into_response()
}

#[derive(Deserialize)]
struct TaskReq {
    room: String,
    text: String,
    #[serde(default)]
    to: String,
}

async fn task_route(axum::Json(req): axum::Json<TaskReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let room = req.room.trim();
    let text = req.text.trim();
    if room.is_empty() || text.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "room and text required");
    }
    // Prefix @handle so a `mention`-policy agent fires on the task.
    let msg = if req.to.trim().is_empty() { text.to_string() } else { format!("@{} {}", req.to.trim(), text) };
    let (ok, out) = run_rozum(&["meetings", "post", "--room", room, &msg]);
    if ok {
        axum::Json(serde_json::json!({ "ok": true })).into_response()
    } else {
        json_err(axum::http::StatusCode::BAD_GATEWAY, out.trim())
    }
}

/// The model's admission verdict (for a 409 body): its estimated footprint vs the live ledger.
fn footprint_report(model: &str) -> serde_json::Value {
    let report = crate::share::dry_run_admission(footprint_for(model));
    serde_json::json!({
        "footprint_bytes": report.footprint,
        "available_bytes": report.available,
        "budget_bytes": report.budget,
        "fits": report.admit,
        "ledger_fits": report.ledger_fits,
        "ram_fits": report.ram_fits,
        "pressure_ok": report.pressure_ok,
    })
}

/// Estimate a model's resident footprint from its catalog weight size at a conservative ctx.
fn footprint_for(model: &str) -> u64 {
    let weight = rozum_models::models::scan_all_installed()
        .into_iter()
        .find(|m| m.spec == model)
        .map(|m| m.size_bytes)
        .unwrap_or(0);
    rozum_models::model_source::runtime_footprint_bytes(model, 8192, weight)
}

/// Derive the roster handle the participant uses, mirroring `model_participant::derive_handle` (the
/// short family name after the last `:` / `/`, before the first `-`), so the UI can @mention it.
fn derive_handle(model: &str) -> String {
    let tail = model.rsplit(['/', ':']).next().unwrap_or(model);
    let base = tail.split('-').next().unwrap_or(tail);
    base.to_lowercase()
}

fn json_err(code: axum::http::StatusCode, msg: &str) -> axum::response::Response {
    use axum::response::IntoResponse;
    (code, axum::Json(serde_json::json!({ "ok": false, "error": msg }))).into_response()
}

// ── Phase 2: coding-agents (`rozum launch`) — detached supervisor + log ─────────────────────────────
//
// A coding-agent does real file work in a repo. `rozum launch` is foreground (it execs the agent), so
// we spawn `rozum launch --model … <agent> <prompt>` DETACHED in the chosen workdir, with output → a
// per-run log file, and track it in a coders registry. Admission is enforced up front (ensure_gateway),
// and `rozum launch` then reuses that same shared gateway.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoderRecord {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub prompt: String,
    pub log: String,
    pub pid: u32,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoderBrief {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub prompt: String,
    pub pid: u32,
    pub alive: bool,
}

fn coders_registry_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("ucc-coders.json"))
}

fn load_coders() -> Vec<CoderRecord> {
    coders_registry_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save_coders(coders: &[CoderRecord]) {
    if let Some(p) = coders_registry_path() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(body) = serde_json::to_vec_pretty(coders) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() {
                let _ = std::fs::rename(&tmp, &p);
            }
        }
    }
}

/// Running coders with a fresh liveness check; coders that have exited STAY in the registry (so their
/// log is still reachable) but report `alive=false`. The UI lets the operator clear a finished one.
fn live_coders() -> Vec<CoderBrief> {
    load_coders()
        .into_iter()
        .map(|c| CoderBrief {
            alive: pid_alive(c.pid),
            id: c.id, agent: c.agent, model: c.model, workdir: c.workdir, prompt: c.prompt, pid: c.pid,
        })
        .collect()
}

/// The agent's own invocation for a non-interactive run: program + flags + the task prompt. claude runs
/// autonomously (skip-permissions + a turn cap) so it never blocks on a phone-launched run.
fn agent_invocation(agent: &str, prompt: &str) -> Vec<String> {
    match agent {
        "claude" => vec![
            "claude".into(), "-p".into(), prompt.into(),
            "--dangerously-skip-permissions".into(), "--max-turns".into(), "40".into(),
        ],
        "codex" => vec!["codex".into(), "exec".into(), prompt.into()],
        "opencode" => vec!["opencode".into(), "run".into(), prompt.into()],
        other => vec![other.into(), prompt.into()],
    }
}

/// Spawn `rozum launch --model <m> <agent invocation>` DETACHED in `workdir`, output → a log file.
/// Returns (pid, log_path).
fn spawn_coder(agent: &str, model: &str, workdir: &str, prompt: &str) -> std::io::Result<(u32, PathBuf)> {
    use std::os::unix::process::CommandExt;
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let log_dir = state_dir().map(|d| d.join("logs")).unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::create_dir_all(&log_dir);
    let stamp = crate::share::now_unix();
    let log_path = log_dir.join(format!("coder-{}-{}.log", sanitize(agent), stamp));
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let log2 = log.try_clone()?;
    let mut args: Vec<String> = vec!["launch".into(), "--model".into(), model.into()];
    args.extend(agent_invocation(agent, prompt));
    let mut cmd = Command::new(&exe);
    cmd.args(&args)
        .current_dir(workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log2))
        .process_group(0);
    Ok((cmd.spawn()?.id(), log_path))
}

#[derive(Deserialize)]
struct CoderLaunchReq {
    agent: String,
    model: String,
    workdir: String,
    prompt: String,
}

async fn coder_launch_route(axum::Json(req): axum::Json<CoderLaunchReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let agent = req.agent.trim().to_string();
    let model = req.model.trim().to_string();
    let workdir = req.workdir.trim().to_string();
    let prompt = req.prompt.trim().to_string();
    if agent.is_empty() || model.is_empty() || workdir.is_empty() || prompt.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "agent, model, workdir, prompt required");
    }
    if !std::path::Path::new(&workdir).is_dir() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "workdir is not a directory");
    }
    // Admission-gate the model load up front; `rozum launch` then reuses this shared gateway.
    if let Err(e) = ensure_gateway(&model) {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({ "ok": false, "error": e, "admission": footprint_report(&model) })),
        ).into_response();
    }
    match spawn_coder(&agent, &model, &workdir, &prompt) {
        Ok((pid, log)) => {
            let id = format!("{}-{}", sanitize(&agent), pid);
            let mut coders = load_coders();
            coders.push(CoderRecord {
                id: id.clone(), agent, model, workdir, prompt,
                log: log.to_string_lossy().into_owned(), pid, started_at: crate::share::now_unix(),
            });
            save_coders(&coders);
            axum::Json(serde_json::json!({ "ok": true, "id": id, "pid": pid })).into_response()
        }
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("spawn: {e}")),
    }
}

async fn coder_stop_route(axum::Json(req): axum::Json<AgentStopReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut coders = load_coders();
    let Some(pos) = coders.iter().position(|c| c.id == req.id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such coder");
    };
    let c = coders.remove(pos);
    unsafe { libc::kill(c.pid as libc::pid_t, libc::SIGTERM); }
    save_coders(&coders);
    axum::Json(serde_json::json!({ "ok": true, "id": c.id })).into_response()
}

#[derive(Deserialize)]
struct CoderLogQuery {
    id: String,
    #[serde(default = "default_tail")]
    tail: usize,
}
fn default_tail() -> usize { 120 }

async fn coder_log_route(axum::extract::Query(q): axum::extract::Query<CoderLogQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(c) = load_coders().into_iter().find(|c| c.id == q.id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such coder");
    };
    let text = std::fs::read_to_string(&c.log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(q.tail.min(2000));
    let tail = lines[start..].join("\n");
    axum::Json(serde_json::json!({ "id": c.id, "alive": pid_alive(c.pid), "log": tail })).into_response()
}

// ── Phase 4: live interactive terminal sessions (tmux + PTY ↔ WebSocket) ────────────────────────────
//
// A "session" is a coding-agent running INTERACTIVELY under a detached tmux session, so it survives the
// phone sleeping / the WS dropping. `POST /control/session/launch` creates the tmux session; the WS
// `GET /control/session/attach/:id` bridges a PTY running `tmux attach` to the browser xterm.js. tmux
// is the source of truth for liveness; a small registry holds display metadata.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub prompt: String,
    pub started_at: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionBrief {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub alive: bool,
}

fn sessions_registry_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("ucc-sessions.json"))
}
fn load_sessions() -> Vec<SessionRecord> {
    sessions_registry_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}
fn save_sessions(s: &[SessionRecord]) {
    if let Some(p) = sessions_registry_path() {
        if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
        if let Ok(body) = serde_json::to_vec_pretty(s) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() { let _ = std::fs::rename(&tmp, &p); }
        }
    }
}
fn tmux_name(id: &str) -> String { format!("rozum-{id}") }

/// Does the tmux session exist? (`tmux has-session` exit 0). The liveness source of truth.
fn tmux_alive(id: &str) -> bool {
    Command::new("tmux").args(["has-session", "-t", &tmux_name(id)])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .status().map(|s| s.success()).unwrap_or(false)
}

/// Running sessions (registry ∩ live tmux); prunes dead registry entries.
fn live_sessions() -> Vec<SessionBrief> {
    let all = load_sessions();
    let (alive, dead): (Vec<_>, Vec<_>) = all.into_iter().partition(|s| tmux_alive(&s.id));
    if !dead.is_empty() { save_sessions(&alive); }
    alive.into_iter()
        .map(|s| SessionBrief { id: s.id, agent: s.agent, model: s.model, workdir: s.workdir, alive: true })
        .collect()
}

#[derive(Deserialize)]
struct SessionLaunchReq {
    agent: String,
    model: String,
    workdir: String,
    #[serde(default)]
    prompt: String,
}

async fn session_launch_route(axum::Json(req): axum::Json<SessionLaunchReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let agent = req.agent.trim().to_string();
    let model = req.model.trim().to_string();
    let workdir = req.workdir.trim().to_string();
    if agent.is_empty() || model.is_empty() || workdir.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "agent, model, workdir required");
    }
    if !std::path::Path::new(&workdir).is_dir() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "workdir is not a directory");
    }
    // Admission-gate the model; `rozum launch` then reuses the shared gateway.
    if let Err(e) = ensure_gateway(&model) {
        return (
            axum::http::StatusCode::CONFLICT,
            axum::Json(serde_json::json!({ "ok": false, "error": e, "admission": footprint_report(&model) })),
        ).into_response();
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let id = format!("{}-{}", sanitize(&agent), crate::share::now_unix());
    let name = tmux_name(&id);
    // Interactive agent under a detached tmux session in the workdir.
    let inner = format!("{} launch --model {} {}", exe.to_string_lossy(), model, agent);
    let ok = Command::new("tmux")
        .args(["new-session", "-d", "-s", &name, "-c", &workdir, "-x", "120", "-y", "40", &inner])
        .status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "tmux new-session failed");
    }
    // Seed the task prompt into the interactive agent, if given.
    let prompt = req.prompt.trim().to_string();
    if !prompt.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(1500)); // let the agent's REPL come up
        let _ = Command::new("tmux").args(["send-keys", "-t", &name, &prompt, "Enter"]).status();
    }
    let mut sessions = load_sessions();
    sessions.push(SessionRecord {
        id: id.clone(), agent, model, workdir, prompt, started_at: crate::share::now_unix(),
    });
    save_sessions(&sessions);
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

async fn session_stop_route(axum::Json(req): axum::Json<AgentStopReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let _ = Command::new("tmux").args(["kill-session", "-t", &tmux_name(&req.id)]).status();
    let mut sessions = load_sessions();
    sessions.retain(|s| s.id != req.id);
    save_sessions(&sessions);
    axum::Json(serde_json::json!({ "ok": true, "id": req.id })).into_response()
}

/// WS: bridge a PTY running `tmux attach -t rozum-<id>` to the browser terminal. Closing the WS ends the
/// ATTACH (the tmux session persists → reconnect re-attaches). Text frame `{"resize":{cols,rows}}` resizes.
async fn session_attach_route(
    ws: axum::extract::ws::WebSocketUpgrade,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if !tmux_alive(&id) {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such session");
    }
    ws.on_upgrade(move |socket| session_ws_bridge(socket, id))
}

async fn session_ws_bridge(mut socket: axum::extract::ws::WebSocket, id: String) {
    use axum::extract::ws::Message;
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::{Read, Write};

    let name = tmux_name(&id);
    let pty = native_pty_system();
    let pair = match pty.openpty(PtySize { rows: 40, cols: 120, pixel_width: 0, pixel_height: 0 }) {
        Ok(p) => p,
        Err(_) => return,
    };
    let mut cmd = CommandBuilder::new("tmux");
    cmd.args(["attach", "-t", &name]);
    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(_) => return,
    };
    drop(pair.slave);
    let mut reader = match pair.master.try_clone_reader() { Ok(r) => r, Err(_) => return };
    let writer = match pair.master.take_writer() { Ok(w) => w, Err(_) => return };
    let writer = std::sync::Arc::new(std::sync::Mutex::new(writer));
    let master = pair.master;

    // PTY → channel (blocking std read on a worker thread).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(256);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => { if tx.blocking_send(buf[..n].to_vec()).is_err() { break; } }
            }
        }
    });

    loop {
        tokio::select! {
            out = rx.recv() => match out {
                Some(bytes) => { if socket.send(Message::Binary(bytes.into())).await.is_err() { break; } }
                None => break, // PTY closed (attach ended)
            },
            inc = socket.recv() => match inc {
                Some(Ok(Message::Binary(b))) => { let _ = writer.lock().unwrap().write_all(&b); }
                Some(Ok(Message::Text(t))) => {
                    let mut handled = false;
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
                        if let Some(r) = v.get("resize") {
                            let cols = r.get("cols").and_then(|x| x.as_u64()).unwrap_or(120) as u16;
                            let rows = r.get("rows").and_then(|x| x.as_u64()).unwrap_or(40) as u16;
                            let _ = master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
                            handled = true;
                        }
                    }
                    if !handled { let _ = writer.lock().unwrap().write_all(t.as_bytes()); }
                }
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                _ => {}
            },
        }
    }
    let _ = child.kill(); // end the `tmux attach` (NOT the session — it stays detached for reconnect)
}

// ── Phase 4a: auth gate — own Face ID (WebAuthn) OR busi SSO ─────────────────────────────────────────
//
// Two auth sources, either suffices: (1) the UCC's OWN passkey (Face ID) via webauthn-rs — a `rozum_sess`
// cookie after login; (2) busi SSO — a `busi_device` cookie that is a paired busi token. The gate protects
// every control action + the terminal WS. RP ID = the Tailscale host (same passkey domain as busi).

use std::sync::{Mutex, OnceLock};
use webauthn_rs::prelude::*;

fn rp_id() -> String {
    std::env::var("ROZUM_UCC_RP_ID").unwrap_or_else(|_| "busi.tail1174e2.ts.net".into())
}
fn rp_origin() -> String {
    std::env::var("ROZUM_UCC_ORIGIN").unwrap_or_else(|_| "https://busi.tail1174e2.ts.net:8447".into())
}
fn operator_uuid() -> Uuid { Uuid::from_u128(0x_524f_5a55_4d00_0000_0000_0000_0000_0001) }

fn webauthn() -> Option<&'static Webauthn> {
    static W: OnceLock<Option<Webauthn>> = OnceLock::new();
    W.get_or_init(|| {
        let origin = url::Url::parse(&rp_origin()).ok()?;
        WebauthnBuilder::new(&rp_id(), &origin).ok()?.rp_name("rozum control").build().ok()
    }).as_ref()
}

fn creds_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-credentials.json")) }
fn load_creds() -> Vec<Passkey> {
    creds_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn save_creds(c: &[Passkey]) {
    if let Some(p) = creds_path() {
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        if let Ok(b) = serde_json::to_vec_pretty(c) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &b).is_ok() { let _ = std::fs::rename(&tmp, &p); }
        }
    }
}

// In-flight ceremony state — single operator, so one slot each is enough.
fn reg_inflight() -> &'static Mutex<Option<PasskeyRegistration>> {
    static S: OnceLock<Mutex<Option<PasskeyRegistration>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}
fn auth_inflight() -> &'static Mutex<Option<PasskeyAuthentication>> {
    static S: OnceLock<Mutex<Option<PasskeyAuthentication>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
fn sess_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-auth-sessions.json")) }
fn load_auth_sessions() -> Vec<(String, u64)> {
    sess_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn save_auth_sessions(s: &[(String, u64)]) {
    if let Some(p) = sess_path() {
        if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
        if let Ok(b) = serde_json::to_vec(s) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &b).is_ok() { let _ = std::fs::rename(&tmp, &p); }
        }
    }
}
fn mint_session() -> String {
    let token = Uuid::new_v4().simple().to_string();
    let now = crate::share::now_unix();
    let mut s = load_auth_sessions();
    s.retain(|(_, exp)| *exp > now); // prune expired
    s.push((token.clone(), now + SESSION_TTL_SECS));
    save_auth_sessions(&s);
    token
}
fn valid_session(token: &str) -> bool {
    let now = crate::share::now_unix();
    load_auth_sessions().iter().any(|(t, exp)| t == token && *exp > now)
}

/// Parse a `name=value; …` Cookie header into a lookup.
fn cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let h = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    h.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// busi SSO: the `busi_device` cookie must be a paired busi token (membership in ~/.busi/tokens.txt) —
/// exactly busi v2's `isPaired`.
fn busi_authed(headers: &axum::http::HeaderMap) -> bool {
    let Some(tok) = cookie(headers, "busi_device") else { return false };
    if tok.is_empty() { return false; }
    let path = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".busi/tokens.txt"));
    let Some(path) = path else { return false };
    let Ok(content) = std::fs::read_to_string(path) else { return false };
    content.lines().any(|l| l.trim() == tok)
}

/// Authenticated iff a valid own session (`rozum_sess`) OR a valid busi session (`busi_device`).
fn authed(headers: &axum::http::HeaderMap) -> bool {
    if let Some(s) = cookie(headers, "rozum_sess") {
        if valid_session(&s) { return true; }
    }
    busi_authed(headers)
}

/// Middleware: 401 unless authenticated. Applied to every action route + the terminal WS (NOT to
/// `/control/status` or `/control/auth/*`, which must be reachable to render the login screen + log in).
async fn require_auth(req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if authed(req.headers()) {
        next.run(req).await
    } else {
        (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "auth required" }))).into_response()
    }
}

fn set_cookie(name: &str, value: &str, max_age: u64) -> [(axum::http::HeaderName, String); 1] {
    [(axum::http::header::SET_COOKIE,
      format!("{name}={value}; Path=/; Max-Age={max_age}; Secure; HttpOnly; SameSite=None"))]
}

async fn auth_status_route(headers: axum::http::HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::Json(serde_json::json!({
        "authed": authed(&headers),
        "has_credential": !load_creds().is_empty(),
        "webauthn_ok": webauthn().is_some(),
    })).into_response()
}

async fn register_begin_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let exclude: Vec<CredentialID> = load_creds().iter().map(|p| p.cred_id().clone()).collect();
    match w.start_passkey_registration(operator_uuid(), "operator", "operator", Some(exclude)) {
        Ok((ccr, state)) => { *reg_inflight().lock().unwrap() = Some(state); axum::Json(ccr).into_response() }
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")),
    }
}

async fn register_finish_route(axum::Json(reg): axum::Json<RegisterPublicKeyCredential>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let Some(state) = reg_inflight().lock().unwrap().take() else { return json_err(axum::http::StatusCode::BAD_REQUEST, "no registration in flight"); };
    match w.finish_passkey_registration(&reg, &state) {
        Ok(pk) => { let mut c = load_creds(); c.push(pk); save_creds(&c); axum::Json(serde_json::json!({ "ok": true })).into_response() }
        Err(e) => json_err(axum::http::StatusCode::BAD_REQUEST, &format!("{e:?}")),
    }
}

async fn login_begin_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let creds = load_creds();
    if creds.is_empty() { return json_err(axum::http::StatusCode::BAD_REQUEST, "no passkey enrolled"); }
    match w.start_passkey_authentication(&creds) {
        Ok((rcr, state)) => { *auth_inflight().lock().unwrap() = Some(state); axum::Json(rcr).into_response() }
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")),
    }
}

async fn login_finish_route(axum::Json(auth): axum::Json<PublicKeyCredential>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let Some(state) = auth_inflight().lock().unwrap().take() else { return json_err(axum::http::StatusCode::BAD_REQUEST, "no login in flight"); };
    match w.finish_passkey_authentication(&auth, &state) {
        Ok(_) => {
            let token = mint_session();
            (set_cookie("rozum_sess", &token, SESSION_TTL_SECS),
             axum::Json(serde_json::json!({ "ok": true }))).into_response()
        }
        Err(e) => json_err(axum::http::StatusCode::UNAUTHORIZED, &format!("{e:?}")),
    }
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
    /// The meeting rooms (read-only, from the meeting daemon's on-disk registry) so the UCC can list
    /// them with a link to each room's web view. Read directly from `rooms.json` to avoid a
    /// rozum-gateway → rozum-meeting crate dependency.
    pub meetings: Vec<MeetingBrief>,
    /// The chat-agents (model-participants) launched through the control API, with liveness — so the
    /// UCC can show a cross-room "running agents" view and stop them.
    pub agents: Vec<AgentBrief>,
    /// The coding-agents (`rozum launch`) launched through the control API, with liveness — so the UCC
    /// can show running coders, tail their logs, and stop them.
    pub coders: Vec<CoderBrief>,
    /// The live interactive terminal sessions (tmux-backed) — so the UCC can list them, open the
    /// xterm.js terminal, and stop them.
    pub sessions: Vec<SessionBrief>,
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

/// A meeting room — its `room` name is the path segment of the meeting web view (`/r/<room>`).
#[derive(Debug, Clone, Serialize)]
pub struct MeetingBrief {
    pub room: String,
}

/// List the meeting rooms from the daemon's on-disk registry (`$XDG_STATE_HOME|~/.local/state` →
/// `rozum/rooms.json`). Read-only, best-effort: a missing/garbled file yields an empty list. Test and
/// worktree rooms (project under `/tmp` or `/.worktrees/`) are filtered out so the dashboard lists the
/// real rooms (global + project).
fn list_meetings() -> Vec<MeetingBrief> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")));
    let Some(path) = base.map(|b| b.join("rozum/rooms.json")) else {
        return Vec::new();
    };
    let Ok(bytes) = std::fs::read(&path) else { return Vec::new() };
    let Ok(rooms) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) else {
        return Vec::new();
    };
    rooms
        .iter()
        .filter_map(|r| {
            let name = r.get("name")?.as_str()?;
            let project = r.get("project").and_then(|p| p.as_str()).unwrap_or("");
            if project.contains("/tmp/") || project.contains("/.worktrees/") {
                return None;
            }
            Some(MeetingBrief { room: name.to_string() })
        })
        .collect()
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
    let meetings = list_meetings();
    let agents = live_agents();
    let coders = live_coders();
    let sessions = live_sessions();
    ControlStatus { gateway, residency, installed, residency_metrics, meetings, agents, coders, sessions }
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
