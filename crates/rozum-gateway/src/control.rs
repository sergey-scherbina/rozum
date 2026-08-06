//! Control-API for the models/gateway service — the read aggregation a dashboard (the CLI today, the
//! future UCC client) consumes: the active shared gateway, the host residency ledger, and the
//! installed model catalog. The symmetric counterpart to `rozum-meeting::client`; the same snapshot is
//! served over the gateway's HTTP surface for the web/UCC target. See
//! `docs/specs/services-and-clients.md`.

pub(crate) use crate::auth::*;
pub(crate) use crate::agents::*;
pub(crate) use crate::coders::*;
pub(crate) use crate::gateway_control::*;
pub(crate) use crate::messenger::*;
pub(crate) use crate::matrix::*;
use crate::errors::json_err;
pub(crate) use crate::view_tokens::*;
pub(crate) use crate::sessions::*;
use crate::wire_body::*;
use crate::paths::{state_dir, ucc_site_dir};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        // Messenger console. Admin-gated on purpose: these routes install bot tokens, control
        // launchd services and edit the permission rosters that decide who may run shell commands
        // through the assistant — the same blast radius as user/role management, not "read".
        .route("/control/messenger/status", get(messenger_status_route))
        .route("/control/messenger/group/add", post(messenger_group_add_route))
        .route("/control/messenger/group/remove", post(messenger_group_remove_route))
        .route("/control/messenger/acl", get(messenger_acl_route))
        .route("/control/messenger/acl/grant", post(messenger_acl_grant_route))
        .route("/control/messenger/acl/revoke", post(messenger_acl_revoke_route))
        .route("/control/messenger/bot/service", post(messenger_bot_service_route))
        .route("/control/messenger/bot/add", post(messenger_bot_add_route))
        .route("/control/messenger/bot/remove", post(messenger_bot_remove_route))
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
        // Conversational chat: talk to the resident model DIRECTLY (streamed), no agent, no repo
        // exploration — the phone chat's default "Собеседник" mode. The agentic path stays
        // `/control/coder/launch` (the "Агент" toggle). See docs/specs/unified-control-center.md.
        .route("/control/chat/stream", post(chat_stream_route))
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
        // Chat-style session I/O (session.html): read the pane as CLEAN text + send a line — no PTY,
        // no xterm.js, so none of the escape-sequence / terminal-probe garbage the raw attach fights.
        .route("/control/session/send", post(session_send_route))
        .route("/control/session/output", get(session_output_route))
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


fn serve_site_file(name: &str) -> axum::response::Response {
    use axum::{http::{header, HeaderValue, StatusCode}, response::IntoResponse};
    let path = ucc_site_dir().join(name);
    match std::fs::read(&path) {
        Ok(bytes) => {
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
            // Intentionally NO service-worker registration injection. The former network-only
            // sw.js gave no caching/offline benefit and could leave iOS standalone PWAs blank once
            // its registration claimed the client — both index.html and chat.html went white on the
            // phone while Chrome was fine, with byte-identical page content. sw.js is now a
            // self-destruct that unregisters any lingering worker; not re-registering keeps it gone.
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

/// System prompt for the conversational "Собеседник" mode: tells the local model who it is and what
/// rozum is, so a plain question ("О чём проект?") gets a good answer from context WITHOUT the agentic
/// repo exploration. Kept short — a 4B follows a crisp instruction; a heavy blob degrades it.
const ROZUM_CHAT_SYSTEM: &str = "Ты — ассистент rozum, работающий ЛОКАЛЬНО на Mac пользователя: ты \
модель (Qwen), которую обслуживает локальный гейтвей rozum, и пользователь пишет тебе с телефона. \
rozum — это local-first система, чтобы запускать LLM и ИИ-агентов на своём железе (Apple Silicon / \
MLX): локальный OpenAI/Anthropic-совместимый гейтвей для MLX и GGUF моделей; комнаты-встречи, где \
ИИ-агенты и люди координируются; телефонный контрол-центр (UCC) с этим чатом; безопасная резидентность \
нескольких моделей (контроль допуска, чтобы модели не переполняли память). Отвечай в диалоге, кратко и \
по делу, на языке пользователя. Ты сейчас именно БЕСЕДУЕШЬ, а не выполняешь задачи в проекте — если \
просят что-то СДЕЛАТЬ в проекте (править файлы, запускать команды), скажи переключиться в режим «Агент».";

#[derive(Deserialize)]
struct ChatMsgIn {
    role: String,
    content: String,
}
#[derive(Deserialize)]
struct ChatStreamReq {
    model: String,
    messages: Vec<ChatMsgIn>,
    #[serde(default)]
    max_tokens: Option<u32>,
}

/// Conversational chat: forward the phone's message history to the resident model's
/// `/v1/chat/completions` with `stream:true` and pipe the SSE straight back — token-by-token, no
/// agent, no repo exploration, so it can never hang the way a 40-turn `claude -p` can. Prepends
/// [`ROZUM_CHAT_SYSTEM`] so the model knows what rozum is.
async fn chat_stream_route(body: String) -> axum::response::Response {
    let req: ChatStreamReq = match parse_action_json(&body) {
        Ok(r) => r,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    if req.model.trim().is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "model required");
    }
    // The reactive client's stream primitive fires one POST at page mount with an empty
    // `messages` (the body only carries the conversation while a send is in flight). Treat that
    // as a graceful no-op — an immediately-terminated SSE stream — rather than a 400, so the
    // mount-fire leaves no error in the log and never reaches the model.
    if req.messages.is_empty() {
        return match axum::response::Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-store")
            .body(axum::body::Body::from("data: [DONE]\n\n"))
        {
            Ok(r) => r,
            Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        };
    }
    let port = match ensure_gateway(&req.model).await {
        Ok(p) => p,
        Err(e) => return json_err(axum::http::StatusCode::SERVICE_UNAVAILABLE, &e),
    };
    let mut messages = vec![serde_json::json!({"role": "system", "content": ROZUM_CHAT_SYSTEM})];
    for m in &req.messages {
        // Only user/assistant turns pass through; ignore any stray roles from the client.
        let role = if m.role == "user" { "user" } else { "assistant" };
        messages.push(serde_json::json!({"role": role, "content": m.content}));
    }
    let upstream = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": true,
        "max_tokens": req.max_tokens.unwrap_or(1024),
    });
    let resp = match reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&upstream)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return json_err(axum::http::StatusCode::BAD_GATEWAY, &format!("gateway: {e}")),
    };
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return json_err(
            axum::http::StatusCode::BAD_GATEWAY,
            &format!("gateway {s}: {}", t.chars().take(200).collect::<String>()),
        );
    }
    // Pipe the upstream OpenAI SSE bytes straight through to the phone.
    match axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-store")
        .body(axum::body::Body::from_stream(resp.bytes_stream()))
    {
        Ok(r) => r,
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
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

















/// True while the model's HF cache still has `*.incomplete` shards — i.e. it's downloading, not yet
/// loading. Best-effort: an unmapped spec (ollama/lmstudio/path) just reports "not downloading".
fn model_is_downloading(spec: &str) -> bool {
    let Some(rest) = spec.strip_prefix("mlx-community:").map(|r| format!("mlx-community/{r}"))
        .or_else(|| spec.strip_prefix("hf:").map(|s| s.to_owned()))
        .or_else(|| (!spec.contains(':') && spec.contains('/')).then(|| spec.to_owned()))
    else {
        return false;
    };
    let Some((org, name)) = rest.split_once('/') else { return false };
    let Some(home) = std::env::var_os("HOME") else { return false };
    let dir = PathBuf::from(home)
        .join(".cache/huggingface/hub")
        .join(format!("models--{org}--{name}"));
    fn has_incomplete(dir: &std::path::Path) -> bool {
        let Ok(rd) = std::fs::read_dir(dir) else { return false };
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().map_or(false, |x| x == "incomplete") {
                return true;
            }
            if p.is_dir() && has_incomplete(&p) {
                return true;
            }
        }
        false
    }
    has_incomplete(&dir)
}











// ── Action route handlers ────────────────────────────────────────────────────────────────────────



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
    let Some(model) = installed
        .iter()
        .find(|m| rozum_models::model_source::same_model(&m.spec, &spec))
    else {
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
        .find(|m| rozum_models::model_source::same_model(m.spec, &spec));
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


















// ── Phase 4: live interactive terminal sessions (tmux + PTY ↔ WebSocket) ────────────────────────────
//
// A "session" is a coding-agent running INTERACTIVELY under a detached tmux session, so it survives the
// phone sleeping / the WS dropping. `POST /control/session/launch` creates the tmux session; the WS
// `GET /control/session/attach/:id` bridges a PTY running `tmux attach` to the browser xterm.js. tmux
// is the source of truth for liveness; a small registry holds display metadata.

















// ── Matrix benchmark queue ────────────────────────────────────────────────────────────────────────────
//
// Manages a persistent in-memory queue of matrix benchmark jobs. Each job runs `agentic.sh` with
// scoped AGENTIC_MODELS / AGENTS / TASKS env vars. A background tokio task processes one job at a
// time; the frontend polls `/control/matrix/status` for queue state + the latest per-run.csv cells.

































// ── View tokens: secret public share links for the matrix ────────────────────────────────────────────
//
// A view token is a random 64-char hex string that grants read-only access to the matrix results
// without any login. The admin creates/revokes tokens; anyone with the URL can view.











// ── RBAC: users, roles, invites ──────────────────────────────────────────────────────────────────────
//
// Permissions: read | chat | agents | matrix | projects | admin (admin implies all).
// Roles = named groups of flags. Built-in: readonly, operator, admin.
// Users link one or more passkeys (one per device) to a role.
// Invites = tokens generated by admin; encode the role; single-use by default.



















// ── Admin RBAC route handlers ──────────────────────────────────────────────────────────────────────
















// ── Phase 4a: auth gate — own Face ID (WebAuthn) OR busi SSO ─────────────────────────────────────────
//
// Two auth sources, either suffices: (1) the UCC's OWN passkey (Face ID) via webauthn-rs — a `rozum_sess`
// cookie after login; (2) busi SSO — a `busi_device` cookie that is a paired busi token. The gate protects
// every control action + the terminal WS. RP ID = the Tailscale host (same passkey domain as busi).



























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
    /// "★★★★" adequacy rating for direct display (empty when unrated). See `model_stars`.
    pub stars: String,
    /// Short operator-facing driver hint. Capability is model × agent-driver, not model alone.
    pub driver_hint: String,
    /// Combined rating + driver for a single stacked sub-line under the model name in the pickers
    /// (e.g. "★★★★  claude") — keeps the phone model picker to two columns so it fits without a
    /// separate Driver column squeezing the name.
    pub rating_line: String,
}

/// Live matrix-derived ratings: `~/.rozum/ucc/model-ratings.json`, written by
/// `scripts/bench/export_model_ratings.py` after a matrix run (claude-driver pass-rate over the
/// real coding tasks; greet + rc=2 excluded). Loaded once per status request; exact spec keys.
fn load_live_ratings() -> std::collections::HashMap<String, u8> {
    let mut out = std::collections::HashMap::new();
    let Some(home) = std::env::var_os("HOME") else { return out };
    let path = PathBuf::from(home).join(".rozum/ucc/model-ratings.json");
    let Ok(body) = std::fs::read_to_string(path) else { return out };
    let Ok(doc) = serde_json::from_str::<serde_json::Value>(&body) else { return out };
    if let Some(models) = doc.get("models").and_then(|m| m.as_object()) {
        for (spec, v) in models {
            if let Some(stars) = v.get("stars").and_then(|s| s.as_u64()) {
                out.insert(spec.clone(), stars.min(5) as u8);
            }
        }
    }
    out
}

/// UCC model "adequacy" rating (1–5): the live matrix export when present (exact spec match),
/// else the static fallback distilled from past matrix sessions — how reliably the model completes
/// agentic coding tasks under its best driver on this machine. Unknown models get `None` (sorted
/// last, shown unrated). The UCC model pickers sort by this and show it as stars.
fn model_stars(spec: &str, live: &std::collections::HashMap<String, u8>) -> Option<u8> {
    if let Some(s) = live.get(spec) {
        return Some(*s);
    }
    const RATINGS: &[(&str, u8)] = &[
        ("GLM-4.7-Flash", 5),          // 15/15 full matrix, in DEFAULT_MODELS
        ("Qwen3.6-35B-A3B", 4),        // reliable under claude after the delivery fixes
        ("gpt-oss-20b", 4),            // claude 5/5; codex fails are model-ceiling (rc10)
        ("GLM-4-32B", 4),              // reliable edit/debug/chat; artifact-synth covers create
        ("Qwen3.6-27B", 3),
        ("DeepSeek-Coder-V2-Lite", 3),
        ("Qwen3-Coder-30B", 3),        // post-parser-fix needs a fresh full matrix before promotion
        ("Devstral", 3),               // good under claude; poor codex/opencode match (B3)
        ("Qwen2.5-Coder-7B", 2),
        ("GLM-4-9B", 2),
        ("Qwen3-4B", 2),               // fine cascade planner, weak solo coder
        ("Qwen3-0.6B", 1),
    ];
    RATINGS.iter().find(|(k, _)| spec.contains(k)).map(|(_, s)| *s)
}

/// Short UCC model-picker hint: the agent driver that best matches this model's tool dialect and
/// measured matrix behaviour. Keep terse so the table remains scannable on mobile.
fn model_driver_hint(spec: &str) -> &'static str {
    if spec.contains("Qwen3.6-35B") {
        "any"
    } else if spec.contains("gpt-oss-20b") {
        "claude/opencode"
    } else if spec.contains("Qwen3-Coder")
        || spec.contains("Qwen3-30B")
        || spec.contains("Qwen3.6-27B")
        || spec.contains("GLM-4.7")
        || spec.contains("GLM-4-32B")
    {
        "claude"
    } else if spec.contains("Devstral") || spec.contains("Mistral") {
        "claude"
    } else {
        ""
    }
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
    /// Async-load status for the panel: "" idle, "downloading…"/"loading…" while a background load is
    /// in flight (both action buttons hidden), or "✗ <error>" if the last load failed (load button
    /// shown again so the user can retry). Driven by `loading_models()`.
    pub load_status: String,
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
                    .find(|m| rozum_models::model_source::same_model(&m.spec, &model))
                    .map(|m| fmt_gib(m.size_bytes))
                    .unwrap_or_default();
                let active_peak_gib = rozum_core::footprint::cached_active_peak(&model).map(fmt_gib);
                let total_peak_gib = rozum_core::footprint::cached_peak(&model).map(fmt_gib);
                ResidentBrief { pid, model, size_gib, active_peak_gib, total_peak_gib }
            })
            .collect(),
    };
    // Model pickers show this list top-down: best-rated first (see `model_stars`), unrated last.
    let live_ratings = load_live_ratings();
    let mut installed: Vec<InstalledBrief> = installed_catalog
        .iter()
        .map(|m| {
            let stars = model_stars(&m.spec, &live_ratings).map(|n| "★".repeat(n as usize)).unwrap_or_default();
            let driver_hint = model_driver_hint(&m.spec).to_string();
            let rating_line = match (stars.is_empty(), driver_hint.is_empty()) {
                (false, false) => format!("{stars}  {driver_hint}"),
                (false, true) => stars.clone(),
                (true, false) => driver_hint.clone(),
                (true, true) => String::new(),
            };
            InstalledBrief {
                size_gib: fmt_gib(m.size_bytes),
                stars,
                driver_hint,
                rating_line,
                spec: m.spec.clone(),
                size_bytes: m.size_bytes,
            }
        })
        .collect();
    installed.sort_by(|a, b| {
        model_stars(&b.spec, &live_ratings)
            .unwrap_or(0)
            .cmp(&model_stars(&a.spec, &live_ratings).unwrap_or(0))
            .then_with(|| a.spec.cmp(&b.spec))
    });
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
    // Snapshot in-flight loads (drop stale failures so a week-old error doesn't haunt the panel).
    let load_snapshot: std::collections::HashMap<String, Option<String>> = {
        let mut l = loading_models().lock().unwrap();
        let now = crate::share::now_unix();
        l.retain(|_, s| s.error.is_none() || now.saturating_sub(s.started) < 600);
        l.iter().map(|(k, s)| (k.clone(), s.error.clone())).collect()
    };
    let mut models: Vec<UnifiedModelBrief> = installed_catalog.iter().map(|m| {
        let loaded = resident_specs.contains(m.spec.as_str());
        // Async-load state: actively loading (no error) hides both buttons and shows a status; a failed
        // load shows "✗ <err>" AND leaves the load button so the user can retry.
        let load_entry = load_snapshot.get(&m.spec);
        let loading_now = matches!(load_entry, Some(None));
        let load_status = match load_entry {
            Some(Some(err)) => format!("✗ {err}"),
            Some(None) if model_is_downloading(&m.spec) => "downloading…".to_string(),
            Some(None) => "loading…".to_string(),
            None => String::new(),
        };
        UnifiedModelBrief {
            spec: m.spec.clone(),
            size_gib: fmt_gib(m.size_bytes),
            size_bytes: m.size_bytes,
            stop_label: if loaded && !loading_now { "выгрузить".to_string() } else { String::new() },
            load_spec: if !loaded && !loading_now { m.spec.clone() } else { String::new() },
            load_status,
        }
    }).collect();
    models.sort_by(|a, b| b.size_bytes.cmp(&a.size_bytes));
    ControlStatus { gateway, residency, installed, residency_metrics, meetings, agents, coders, sessions, projects, models }
}

// ── Messenger admin console ────────────────────────────────────────────────────────────────────
// Bots, their group registries and the per-room permission rosters, for the `#/messenger` screen.
// Every handler delegates to `rozum-gateway messenger … --json`, which is the SAME code path the
// CLI and the in-chat commands use — the console cannot drift from the shell, because it IS the
// shell. Spec: `docs/specs/messenger-admin-console.md`. All routes are admin-gated.













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

    #[tokio::test]
    async fn chat_stream_empty_messages_is_a_noop_stream_not_400() {
        // The reactive chat client fires one `/control/chat/stream` POST at page mount with an
        // empty `messages` (the stream primitive fires unconditionally at mount). That must be a
        // graceful empty SSE, not a 400 — the empty-messages branch returns before ensure_gateway,
        // so this asserts the no-op path without a live model.
        let resp = chat_stream_route(r#"{"model":"m","messages":[]}"#.to_string()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
    }

    #[tokio::test]
    async fn chat_stream_missing_model_is_still_400() {
        // An empty model is a real client error and must keep returning 400.
        let resp = chat_stream_route(r#"{"model":"","messages":[]}"#.to_string()).await;
        assert_eq!(resp.status(), axum::http::StatusCode::BAD_REQUEST);
    }



    #[test]
    fn agent_invocations_carry_each_cli_headless_verb() {
        assert_eq!(agent_invocation("claude", "do X")[..3], ["claude", "-p", "do X"]);
        assert_eq!(agent_invocation("codex", "do X"), ["codex", "exec", "do X"]);
        assert_eq!(agent_invocation("opencode", "do X"), ["opencode", "run", "do X"]);
        // Without the `run` verb nadia reads the prompt as the MODE and exits 2 — the fallback
        // arm below is right only for a CLI that takes a bare prompt.
        assert_eq!(agent_invocation("nadia", "do X"), ["nadia", "run", "do X"]);
    }


    #[test]
    fn model_driver_hints_capture_known_model_driver_matches() {
        assert_eq!(model_driver_hint("mlx-community:Qwen3.6-35B-A3B-4bit-DWQ"), "any");
        assert_eq!(model_driver_hint("mlx-community:gpt-oss-20b-MXFP4-Q4"), "claude/opencode");
        assert_eq!(model_driver_hint("mlx-community:Qwen3-Coder-30B-A3B-Instruct-4bit"), "claude");
        assert_eq!(model_driver_hint("mlx-community:GLM-4.7-Flash-4bit"), "claude");
        assert_eq!(model_driver_hint("mlx-community:Devstral-Small-2507-4bit"), "claude");
        assert_eq!(model_driver_hint("unknown:model"), "");
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


}
