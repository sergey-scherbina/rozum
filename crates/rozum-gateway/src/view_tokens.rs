//! Read-only view tokens: a link that shows one page of the console without an account.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Ten items — the record, its 0600 store,
//! the check, the page route, and the three admin routes that list, mint and revoke.
//!
//! **This slice exists to delete a line elsewhere.** `matrix.rs` had to import `check_view_token`
//! from `control.rs`, because the public matrix routes are token-gated; that import was the one
//! thing keeping a child module pointed back at its parent. It comes from here now, and the
//! dependency runs downward like the rest.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::errors::json_err;
use crate::paths::{state_dir, ucc_site_dir};
use crate::private_store::{json_load, json_save_rbac, rand_token};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ViewToken {
    pub(crate) token: String,
    pub(crate) label: String,
    pub(crate) created_at: u64,
    pub(crate) revoked: bool,
}

pub(crate) fn view_tokens_path() -> Option<PathBuf> { state_dir().map(|d| d.join("ucc-view-tokens.json")) }

pub(crate) fn load_view_tokens() -> Vec<ViewToken> { json_load(view_tokens_path()) }

pub(crate) fn save_view_tokens(v: &[ViewToken]) { json_save_rbac(view_tokens_path(), v); }

pub(crate) fn check_view_token(token: &str) -> bool {
    load_view_tokens().iter().any(|t| t.token == token && !t.revoked)
}

pub(crate) async fn view_token_page_route(
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
pub(crate) async fn admin_view_tokens_route() -> axum::response::Response {
    use axum::response::IntoResponse;
    let tokens = load_view_tokens();
    let out: Vec<_> = tokens.iter().map(|t| serde_json::json!({
        "token": t.token, "label": t.label, "created_at": t.created_at, "revoked": t.revoked,
    })).collect();
    axum::Json(serde_json::json!({ "tokens": out })).into_response()
}

#[derive(Deserialize)] pub(crate) struct ViewTokenCreateReq { #[serde(default)] label: String }

pub(crate) async fn admin_view_token_create_route(
    axum::Json(req): axum::Json<ViewTokenCreateReq>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let token = rand_token();
    let mut tokens = load_view_tokens();
    tokens.push(ViewToken { token: token.clone(), label: req.label, created_at: crate::share::now_unix(), revoked: false });
    save_view_tokens(&tokens);
    axum::Json(serde_json::json!({ "ok": true, "token": token })).into_response()
}

pub(crate) async fn admin_view_token_revoke_route(
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

