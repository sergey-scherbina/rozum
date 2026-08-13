//! Control-API for the models/gateway service — the read aggregation a dashboard (the CLI today, the
//! future UCC client) consumes: the active shared gateway, the host residency ledger, and the
//! installed model catalog. The symmetric counterpart to `rozum-meeting::client`; the same snapshot is
//! served over the gateway's HTTP surface for the web/UCC target. See
//! `docs/specs/services-and-clients.md`.

pub(crate) use crate::auth::*;
pub(crate) use crate::agents::*;
pub(crate) use crate::coders::*;
pub(crate) use crate::gateway_control::*;
pub(crate) use crate::chat::*;
pub(crate) use crate::projects::*;
pub(crate) use crate::messenger::*;
pub(crate) use crate::matrix::*;
use crate::errors::json_err;
pub(crate) use crate::view_tokens::*;
pub(crate) use crate::sessions::*;
use crate::paths::ucc_site_dir;
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

pub async fn serve(port: u16) -> std::io::Result<()> {
    use crate::status::{ControlStatus, model_driver_hint, model_stars, status};
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
        ;
    // ucc-ssc-backend: these two routes are served EITHER in-process (as they always were) OR by
    // the .ssc program, chosen once here by `ROZUM_UCC_SSC_ORIGIN`.
    //
    // A SWITCH rather than a replacement on purpose — moving a route to another process adds a
    // failure mode this console did not have (that process being down), so turning it on and
    // turning it back off must cost the same: one variable and a restart. Unset ⇒ byte-for-byte
    // the old behaviour.
    //
    // Chosen BEFORE registration, not layered over it: `.route()` on a path this router already
    // has panics at startup, and that panic is invisible to the compiler.
    let public = match std::env::var("ROZUM_UCC_SSC_ORIGIN").ok().filter(|v| !v.is_empty()) {
        None => public
            .route("/control/public/matrix/cell", get(public_matrix_cell_route))
            .route("/view/{token}", get(view_token_page_route)),
        Some(origin) => {
            eprintln!("control server: /control/public/matrix/cell and /view/{{token}} → {origin} (.ssc)");
            public
                .route("/control/public/matrix/cell", get(ucc_ssc_proxy))
                .route("/view/{token}", get(ucc_ssc_proxy))
        }
    };
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

/// Forward one public read route to the .ssc server. Status, body and Content-Type are passed
/// through; hop-by-hop headers are not, since this is a fresh request on a fresh connection.
///
/// An unreachable .ssc server answers 502 rather than falling back to the Rust handler: a silent
/// fallback would make the switch untestable — it would look like it worked while serving the
/// implementation it was supposed to replace.
async fn ucc_ssc_proxy(req: axum::extract::Request) -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(origin) = std::env::var("ROZUM_UCC_SSC_ORIGIN").ok().filter(|v| !v.is_empty()) else {
        return (axum::http::StatusCode::BAD_GATEWAY, "ucc-ssc origin not configured").into_response();
    };
    let path_and_query = req.uri().path_and_query().map(|p| p.as_str()).unwrap_or("/");
    let url = format!("{}{}", origin.trim_end_matches('/'), path_and_query);
    match reqwest::Client::new().get(&url).send().await {
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            format!("ucc-ssc unreachable at {origin}: {e}"),
        ).into_response(),
        Ok(resp) => {
            let status = axum::http::StatusCode::from_u16(resp.status().as_u16())
                .unwrap_or(axum::http::StatusCode::BAD_GATEWAY);
            let ctype = resp.headers().get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()).unwrap_or("application/json; charset=utf-8")
                .to_string();
            let body = resp.bytes().await.unwrap_or_default();
            axum::response::Response::builder()
                .status(status)
                .header("Content-Type", ctype)
                .header("Cache-Control", "no-store")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
    }
}

// ── Chat read/write endpoints (replaces ucc-web-server.py proxy logic) ───────────────────────────















// ── Agent registry + actions (write side of the control API) ─────────────────────────────────────
//
// Chat-agents are `rozum meetings participant` processes. There is no built-in registry, so we keep a
// small JSON file (`<state>/rozum/ucc-agents.json`) the launch/stop maintain, keyed by a generated id,
// and check liveness with `kill(pid, 0)`. Actions exec the `rozum` CLI via the current binary (which IS
// the full gateway CLI) so there is no extra crate dependency, mirroring `list_meetings`' read-only
// disk approach.




























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



























// Bots, their group registries and the per-room permission rosters, for the `#/messenger` screen.
// Every handler delegates to `rozum-gateway messenger … --json`, which is the SAME code path the
// CLI and the in-chat commands use — the console cannot drift from the shell, because it IS the
// shell. Spec: `docs/specs/messenger-admin-console.md`. All routes are admin-gated.













#[cfg(test)]
mod tests {
    use super::*;

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




}
