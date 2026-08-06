//! The messenger admin surface: bots, their group registries, and per-room ACL rosters.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Nine routes and their shared JSON helper,
//! driving the same operations the `messenger` CLI and the in-chat commands do — the console is a
//! third caller of one ops module, not a second implementation of it.

use crate::errors::json_err;
use crate::gateway_control::{run_rozum, run_rozum_stdin};
use crate::paths::safe_path_seg;
use crate::wire_body::parse_flat_body;

/// Run a `messenger` subcommand and hand its JSON straight back. The CLI already emits
/// `{ok:…}` shapes, so there is nothing to re-encode — and nothing to get subtly different.
pub(crate) fn messenger_json(args: &[&str]) -> axum::response::Response {
    use axum::response::IntoResponse;
    let (ok, out) = run_rozum(args);
    match serde_json::from_str::<serde_json::Value>(out.trim()) {
        Ok(v) => {
            let code = if ok && v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
                axum::http::StatusCode::OK
            } else {
                axum::http::StatusCode::BAD_REQUEST
            };
            (code, axum::Json(v)).into_response()
        }
        Err(_) => json_err(
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            // The CLI redacts bot tokens in its own errors; this is the raw fallback for the case
            // where it did not produce JSON at all (a panic, a missing binary).
            &format!("messenger command produced no JSON: {}", out.trim()),
        ),
    }
}

pub(crate) async fn messenger_status_route() -> axum::response::Response {
    messenger_json(&["messenger", "status", "--json"])
}

pub(crate) async fn messenger_group_add_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let registry = f.get("registry").cloned().unwrap_or_else(|| "telegram".into());
    let Some(chat_id) = f.get("chat_id").map(|s| s.trim().to_string()) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "chat_id required");
    };
    if chat_id.parse::<i64>().is_err() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "chat_id must be a number");
    }
    let title = f.get("title").cloned().unwrap_or_default();
    let mut args: Vec<&str> =
        vec!["messenger", "groups", "add", &chat_id, "--registry", &registry, "--title", &title, "--json"];
    let room = f.get("room").cloned().unwrap_or_default();
    if !room.trim().is_empty() {
        args.push("--room");
        args.push(&room);
    }
    messenger_json(&args)
}

pub(crate) async fn messenger_group_remove_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let registry = f.get("registry").cloned().unwrap_or_else(|| "telegram".into());
    let Some(chat_id) = f.get("chat_id").map(|s| s.trim().to_string()) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "chat_id required");
    };
    if chat_id.parse::<i64>().is_err() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "chat_id must be a number");
    }
    messenger_json(&["messenger", "groups", "remove", &chat_id, "--registry", &registry, "--json"])
}

pub(crate) async fn messenger_acl_route(
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    let Some(room) = q.get("room").filter(|r| !r.trim().is_empty()) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "room required");
    };
    if !safe_path_seg(room) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad room");
    }
    messenger_json(&["messenger", "acl", "show", room, "--json"])
}

pub(crate) async fn messenger_acl_grant_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let (Some(room), Some(user_id)) = (f.get("room"), f.get("user_id")) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "room and user_id required");
    };
    if !safe_path_seg(room) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad room");
    }
    if user_id.trim().parse::<i64>().is_err() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "user_id must be a number");
    }
    let name = f.get("name").cloned().unwrap_or_default();
    let caps_raw = f.get("caps").cloned().unwrap_or_default();
    let caps: Vec<&str> = caps_raw.split_whitespace().collect();
    if caps.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "caps required (chat read write shell | all | none)");
    }
    let mut args: Vec<&str> = vec!["messenger", "acl", "grant", room, user_id.trim()];
    args.extend(caps);
    args.extend(["--name", &name, "--json"]);
    messenger_json(&args)
}

pub(crate) async fn messenger_acl_revoke_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let (Some(room), Some(user_id)) = (f.get("room"), f.get("user_id")) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "room and user_id required");
    };
    if !safe_path_seg(room) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad room");
    }
    if user_id.trim().parse::<i64>().is_err() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "user_id must be a number");
    }
    messenger_json(&["messenger", "acl", "revoke", room, user_id.trim(), "--json"])
}

pub(crate) async fn messenger_bot_service_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let (Some(bot), Some(action)) = (f.get("bot"), f.get("action")) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bot and action required");
    };
    if !safe_path_seg(bot) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad bot");
    }
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "action must be start|stop|restart");
    }
    let (ok, out) = run_rozum(&["messenger", "service", bot, action]);
    use axum::response::IntoResponse;
    let code = if ok { axum::http::StatusCode::OK } else { axum::http::StatusCode::BAD_REQUEST };
    (code, axum::Json(serde_json::json!({ "ok": ok, "output": out.trim() }))).into_response()
}

/// Install a bot. The TOKEN NEVER BECOMES AN ARGUMENT — it goes to the child on stdin, because
/// argv is readable by every process on the machine (`ps`). It is also never echoed back: the
/// response carries the bot's public identity only.
pub(crate) async fn messenger_bot_add_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let (Some(name), Some(token)) = (f.get("name"), f.get("token")) else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "name and token required");
    };
    if !safe_path_seg(name) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad bot name");
    }
    if token.trim().is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "token required");
    }
    let room = f.get("room").cloned().unwrap_or_default();
    let alias = f.get("mention_alias").cloned().unwrap_or_default();
    let mut args: Vec<String> =
        ["messenger", "bot-add", name, "--mention-alias", &alias, "--json"].map(String::from).to_vec();
    if !room.trim().is_empty() {
        args.push("--room".into());
        args.push(room);
    }
    let (ok, out) = run_rozum_stdin(&args, token.trim());
    use axum::response::IntoResponse;
    match serde_json::from_str::<serde_json::Value>(out.trim()) {
        Ok(v) => {
            let code = if ok { axum::http::StatusCode::OK } else { axum::http::StatusCode::BAD_REQUEST };
            (code, axum::Json(v)).into_response()
        }
        // Deliberately does NOT echo the child's raw output here: on this one route that output
        // is the only place a token could plausibly surface.
        Err(_) => json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "bot add failed (see the gateway log)"),
    }
}

pub(crate) async fn messenger_bot_remove_route(body: String) -> axum::response::Response {
    let f = parse_flat_body(&body);
    let Some(bot) = f.get("bot") else {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bot required");
    };
    if !safe_path_seg(bot) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad bot name");
    }
    messenger_json(&["messenger", "bot-remove", bot, "--json"])
}

