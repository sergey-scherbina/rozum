//! Who may use this console, and what they may do with it.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`) — 72 items, the largest subject in the
//! file and the one that most deserves to be readable in one place: passkey registration and login,
//! the session cookie, users, roles and their permissions, invites, the first-run bootstrap token,
//! and the six `require_perm_*` middlewares.
//!
//! Those six are the reason this slice was worth waiting for. Earlier slices left
//! `require_perm_matrix` and `require_perm_agents` behind rather than split a family of six across
//! module boundaries; now the whole family lives with the `require_perm` it delegates to, and
//! nothing about authorisation is spread across two files.
//!
//! Measured on the way in: with the names the first regex missed (`bootstrap_token_*`,
//! `mint_session`, `sess_path`, `rp_id`, `rp_origin`) included, this family calls NOTHING outside
//! itself.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use uuid::Uuid;
use webauthn_rs::prelude::*;

use crate::errors::json_err;
use crate::spawn_support::sanitize;
use crate::paths::state_dir;
use crate::private_store::{atomic_write_private, json_load, json_save_rbac, rand_token};

/// On startup, tighten perms on any pre-existing secret files (some may have been written 0644 before
/// this hardening) and the state dir, so a redeploy remediates them without waiting for the next write.
#[cfg(unix)]
pub(crate) fn harden_state_perms() {
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
pub(crate) fn harden_state_perms() {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UccUser {
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) role_ids: Vec<String>,
    pub(crate) passkey_ids: Vec<String>, // hex-encoded cred IDs
    pub(crate) created_at: u64,
    pub(crate) created_by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UccRole {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) permissions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct UccInvite {
    pub(crate) token: String,
    pub(crate) role_id: String,
    pub(crate) label: String,
    pub(crate) created_by: String,
    pub(crate) created_at: u64,
    pub(crate) expires_at: Option<u64>,
    pub(crate) max_uses: Option<u32>,
    pub(crate) uses: u32,
    pub(crate) revoked: bool,
}

pub(crate) fn users_path()   -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-users.json")) }

pub(crate) fn roles_path()   -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-roles.json")) }

pub(crate) fn invites_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-invites.json")) }

pub(crate) fn load_users()   -> Vec<UccUser>   { json_load(users_path())   }

pub(crate) fn load_roles()   -> Vec<UccRole>   { json_load(roles_path())   }

pub(crate) fn load_invites() -> Vec<UccInvite> { json_load(invites_path()) }

pub(crate) fn save_users(v: &[UccUser])     { json_save_rbac(users_path(),   v); }

pub(crate) fn save_roles(v: &[UccRole])     { json_save_rbac(roles_path(),   v); }

pub(crate) fn save_invites(v: &[UccInvite]) { json_save_rbac(invites_path(), v); }

pub(crate) fn default_roles() -> Vec<UccRole> {
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
pub(crate) fn ensure_rbac_initialized() {
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

pub(crate) fn cred_id_hex(id: &CredentialID) -> String {
    id.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn find_user_by_cred(cred_hex: &str) -> Option<UccUser> {
    load_users().into_iter().find(|u| u.passkey_ids.iter().any(|id| id == cred_hex))
}

pub(crate) fn user_has_perm(user_id: &str, perm: &str) -> bool {
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

pub(crate) fn check_invite(token: &str) -> Result<UccInvite, &'static str> {
    let now = crate::share::now_unix();
    let Some(inv) = load_invites().into_iter().find(|i| i.token == token) else { return Err("invite not found"); };
    if inv.revoked { return Err("invite revoked"); }
    if let Some(exp) = inv.expires_at { if exp < now { return Err("invite expired"); } }
    if inv.uses >= inv.max_uses.unwrap_or(1) { return Err("invite already used"); }
    Ok(inv)
}

pub(crate) fn consume_invite(token: &str) {
    let mut invites = load_invites();
    if let Some(inv) = invites.iter_mut().find(|i| i.token == token) { inv.uses += 1; }
    save_invites(&invites);
}

pub(crate) fn bootstrap_token_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-bootstrap-token.txt")) }

/// First-registration TOFU hardening (ucc-tofu-bootstrap-token): while `users.is_empty()`, whoever
/// reaches `/control/auth/register/begin`+`finish` first would otherwise become the permanent admin
/// with no allowlist. A random token is generated once (persisted + logged, mirrors busi's own
/// "code shown on the computer" phone-pairing pattern) and required as the `invite` field on that
/// FIRST registration only — the same `check_invite`-gated mechanism every later registration
/// already uses, just bootstrapped before any admin/invite exists to issue one.
pub(crate) fn ensure_bootstrap_token() -> Option<String> {
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
pub(crate) fn consume_bootstrap_token() {
    if let Some(path) = bootstrap_token_path() { let _ = std::fs::remove_file(path); }
}

/// Constant-shape comparison isn't needed here (the token is single-use and file-based, not a
/// timing oracle worth defending) — this just centralizes the "both present and equal" rule so it
/// has one definition and one test, instead of being reconstructed inline at the call site.
pub(crate) fn bootstrap_token_matches(provided: Option<&str>, expected: Option<&str>) -> bool {
    provided.zip(expected).is_some_and(|(a, b)| a == b)
}

pub(crate) async fn admin_users_route() -> axum::response::Response {
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

#[derive(Deserialize)] pub(crate) struct SetRoleReq { role_ids: Vec<String> }

pub(crate) async fn admin_set_role_route(
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

pub(crate) async fn admin_delete_user_route(
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

pub(crate) async fn admin_roles_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let all_perms = ["read","chat","agents","matrix","projects","admin"];
    axum::Json(serde_json::json!({ "roles": load_roles(), "all_permissions": all_perms })).into_response()
}

#[derive(Deserialize)] pub(crate) struct RoleReq { name: String, #[serde(default)] permissions: Vec<String> }

pub(crate) async fn admin_create_role_route(axum::Json(req): axum::Json<RoleReq>) -> axum::response::Response {
    use axum::response::IntoResponse;
    if req.name.trim().is_empty() { return json_err(axum::http::StatusCode::BAD_REQUEST, "name required"); }
    let id = sanitize(&req.name.to_lowercase());
    let mut roles = load_roles();
    if roles.iter().any(|r| r.id == id) { return json_err(axum::http::StatusCode::CONFLICT, "role id exists"); }
    roles.push(UccRole { id: id.clone(), name: req.name, permissions: req.permissions });
    save_roles(&roles);
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

pub(crate) async fn admin_update_role_route(
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

pub(crate) async fn admin_delete_role_route(
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

pub(crate) async fn admin_invites_route() -> axum::response::Response {
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
pub(crate) struct InviteCreateReq {
    pub(crate) role_id: String,
    #[serde(default)] label: String,
    pub(crate) max_uses: Option<u32>,
    pub(crate) ttl_hours: Option<u64>,
}

pub(crate) async fn admin_invite_create_route(
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

pub(crate) async fn admin_revoke_invite_route(
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

#[derive(Deserialize)] pub(crate) struct InviteInfoQuery { token: String }

pub(crate) async fn invite_info_route(
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

pub(crate) fn rp_id() -> String {
    std::env::var("ROZUM_UCC_RP_ID").unwrap_or_else(|_| "busi.tail1174e2.ts.net".into())
}

pub(crate) fn rp_origin() -> String {
    // Stale default was `:8447`, left over from the old two-port (SPA + control-API) layout
    // (docs/specs/unified-control-center.md). The SPA and API are now consolidated behind one
    // Tailscale-serve port, `:8448` (see `deploy-ucc-web.sh`) — a WebAuthn ceremony validates the
    // browser's actual origin against this, so a stale port here would reject every login/register.
    std::env::var("ROZUM_UCC_ORIGIN").unwrap_or_else(|_| "https://busi.tail1174e2.ts.net:8448".into())
}

pub(crate) fn webauthn() -> Option<&'static Webauthn> {
    static W: OnceLock<Option<Webauthn>> = OnceLock::new();
    W.get_or_init(|| {
        let origin = url::Url::parse(&rp_origin()).ok()?;
        WebauthnBuilder::new(&rp_id(), &origin).ok()?.rp_name("rozum control").build().ok()
    }).as_ref()
}

pub(crate) fn creds_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-credentials.json")) }

pub(crate) fn load_creds_raw() -> Vec<Passkey> {
    creds_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub(crate) fn load_creds() -> Vec<Passkey> { load_creds_raw() }

pub(crate) fn save_creds(c: &[Passkey]) {
    if let Some(p) = creds_path() {
        if let Ok(b) = serde_json::to_vec_pretty(c) {
            atomic_write_private(&p, &b); // 0600 — WebAuthn credentials
        }
    }
}

pub(crate) fn reg_inflight() -> &'static Mutex<Option<RegInflight>> {
    static S: OnceLock<Mutex<Option<RegInflight>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

pub(crate) fn auth_inflight() -> &'static Mutex<Option<PasskeyAuthentication>> {
    static S: OnceLock<Mutex<Option<PasskeyAuthentication>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

pub(crate) fn sess_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-auth-sessions.json")) }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SessEntry { token: String, user_id: String, expires_at: u64 }

pub(crate) fn load_auth_sessions() -> Vec<SessEntry> {
    sess_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub(crate) fn save_auth_sessions(s: &[SessEntry]) {
    if let Some(p) = sess_path() {
        if let Ok(b) = serde_json::to_vec(s) {
            atomic_write_private(&p, &b); // 0600 — holds live rozum_sess bearer tokens
        }
    }
}

pub(crate) fn mint_session(user_id: &str) -> String {
    let token = Uuid::new_v4().simple().to_string();
    let now = crate::share::now_unix();
    let mut s = load_auth_sessions();
    s.retain(|e| e.expires_at > now);
    s.push(SessEntry { token: token.clone(), user_id: user_id.to_string(), expires_at: now + SESSION_TTL_SECS });
    save_auth_sessions(&s);
    token
}

pub(crate) fn session_user(token: &str) -> Option<String> {
    let now = crate::share::now_unix();
    load_auth_sessions().into_iter().find(|e| e.token == token && e.expires_at > now).map(|e| e.user_id)
}

#[allow(dead_code)]
pub(crate) fn valid_session(token: &str) -> bool { session_user(token).is_some() }

/// Parse a `name=value; …` Cookie header into a lookup.
pub(crate) fn cookie(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let h = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    h.split(';').find_map(|p| {
        let (k, v) = p.split_once('=')?;
        (k.trim() == name).then(|| v.trim().to_string())
    })
}

/// busi SSO: the `busi_device` cookie must be a paired busi token (membership in ~/.busi/tokens.txt) —
/// exactly busi v2's `isPaired`.
pub(crate) fn busi_authed(headers: &axum::http::HeaderMap) -> bool {
    let Some(tok) = cookie(headers, "busi_device") else { return false };
    if tok.is_empty() { return false; }
    let path = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".busi/tokens.txt"));
    let Some(path) = path else { return false };
    let Ok(content) = std::fs::read_to_string(path) else { return false };
    content.lines().any(|l| l.trim() == tok)
}

/// Returns the authenticated user_id: "busi-sso" for busi users, actual UUID for webauthn users.
pub(crate) fn authed_user_id(headers: &axum::http::HeaderMap) -> Option<String> {
    if let Some(s) = cookie(headers, "rozum_sess") {
        if let Some(uid) = session_user(&s) { return Some(uid); }
    }
    if busi_authed(headers) { return Some("busi-sso".to_string()); }
    None
}

#[allow(dead_code)]
pub(crate) fn authed(headers: &axum::http::HeaderMap) -> bool { authed_user_id(headers).is_some() }

/// Middleware: 401 unless authenticated. Injects `Extension<String>` (user_id) for downstream use.
pub(crate) async fn require_auth(mut req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
    use axum::response::IntoResponse;
    match authed_user_id(req.headers()) {
        Some(uid) => { req.extensions_mut().insert(uid); next.run(req).await }
        None => (axum::http::StatusCode::UNAUTHORIZED, axum::Json(serde_json::json!({ "error": "auth required" }))).into_response(),
    }
}

/// Shared body for the `require_perm_*` middlewares below: 403 unless the user (injected by
/// `require_auth`) holds `perm` (or "admin", which satisfies every permission — see `user_has_perm`).
pub(crate) async fn require_perm(user_id: String, perm: &str, req: axum::extract::Request, next: axum::middleware::Next) -> axum::response::Response {
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
pub(crate) async fn require_perm_read(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "read", req, next).await
}

pub(crate) async fn require_perm_chat(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "chat", req, next).await
}

/// Gates agent/coder/session/gateway launch+control — the routes that can run arbitrary commands or
/// attach a live shell. Previously reachable by ANY authenticated session regardless of role.
pub(crate) async fn require_perm_agents(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "agents", req, next).await
}

pub(crate) async fn require_perm_matrix(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "matrix", req, next).await
}

pub(crate) async fn require_perm_projects(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "projects", req, next).await
}

pub(crate) fn set_cookie(name: &str, value: &str, max_age: u64) -> [(axum::http::HeaderName, String); 1] {
    // SPA + API are same-origin (see `serve`'s doc comment) — SameSite=Lax is enough for normal
    // top-level use and, unlike `None`, stops the cookie riding along on a cross-site POST/fetch
    // (CSRF against `coder_stop_route`/`session_stop_route`, which accept any Content-Type).
    [(axum::http::header::SET_COOKIE,
      format!("{name}={value}; Path=/; Max-Age={max_age}; Secure; HttpOnly; SameSite=Lax"))]
}

pub(crate) async fn auth_status_route(headers: axum::http::HeaderMap) -> axum::response::Response {
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

pub(crate) async fn register_begin_route(body: axum::body::Bytes) -> axum::response::Response {
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

pub(crate) async fn register_finish_route(axum::Json(reg): axum::Json<RegisterPublicKeyCredential>) -> axum::response::Response {
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

pub(crate) async fn login_begin_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let Some(w) = webauthn() else { return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "webauthn init failed"); };
    let creds = load_creds();
    if creds.is_empty() { return json_err(axum::http::StatusCode::BAD_REQUEST, "no passkey enrolled"); }
    match w.start_passkey_authentication(&creds) {
        Ok((rcr, state)) => { *auth_inflight().lock().unwrap() = Some(state); axum::Json(rcr).into_response() }
        Err(e) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("{e:?}")),
    }
}

pub(crate) async fn login_finish_route(axum::Json(auth): axum::Json<PublicKeyCredential>) -> axum::response::Response {
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

#[allow(dead_code)]
pub(crate) fn operator_uuid() -> Uuid { Uuid::from_u128(0x_524f_5a55_4d00_0000_0000_0000_0000_0001) }

// In-flight ceremony state.
pub(crate) struct RegInflight { state: PasskeyRegistration, invite_token: Option<String>, display_name: String }

pub(crate) const SESSION_TTL_SECS: u64 = 30 * 24 * 3600;

/// Inner middleware for admin-only routes: reads the user_id injected by `require_auth`, checks admin perm.
pub(crate) async fn require_admin(
    axum::extract::Extension(user_id): axum::extract::Extension<String>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    require_perm(user_id, "admin", req, next).await
}

#[derive(Deserialize, Default)]
pub(crate) struct RegisterBeginReq {
    #[serde(default)] invite: Option<String>,
    #[serde(default)] display_name: Option<String>,
}
