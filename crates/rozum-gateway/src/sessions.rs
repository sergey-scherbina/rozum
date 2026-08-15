//! Terminal sessions: launch a command under tmux, watch it, type into it, attach over a WebSocket.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Seventeen items — the record and its
//! registry, the live view, and the launch/stop/send/output/attach routes.
//!
//! **This is a TERMINAL session, not a login session.** The control API uses the word for both,
//! and the auth one — `mint_session`, `valid_session`, `ucc-auth-sessions.json` — stayed in
//! `control.rs` with the rest of the auth code. Selecting this family by the word rather than by
//! meaning would have moved six unrelated items along with it.
//!
//! After the `spawn_support` and `wire_body` layers came out, this family called NOTHING outside
//! itself — measured, which is why it moved in one piece.

use std::path::PathBuf;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::defaults::{default_scrollback, default_true};
use crate::errors::json_err;
use crate::paths::state_dir;
use crate::spawn_support::*;
use crate::wire_body::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub prompt: String,
    pub started_at: u64,
    /// "starting…" (background launch in flight) / "running" / "failed: …".
    /// Empty on legacy records — treated as "running" (tmux is the source of truth for those).
    #[serde(default)]
    pub status: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionBrief {
    pub id: String,
    pub agent: String,
    pub model: String,
    pub workdir: String,
    pub alive: bool,
    pub status: String,
}

pub(crate) fn sessions_registry_path() -> Option<PathBuf> {
    state_dir().map(|d| d.join("ucc-sessions.json"))
}

pub(crate) fn load_sessions() -> Vec<SessionRecord> {
    sessions_registry_path()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub(crate) fn save_sessions(s: &[SessionRecord]) {
    if let Some(p) = sessions_registry_path() {
        if let Some(dir) = p.parent() { let _ = std::fs::create_dir_all(dir); }
        if let Ok(body) = serde_json::to_vec_pretty(s) {
            let tmp = p.with_extension("json.tmp");
            if std::fs::write(&tmp, &body).is_ok() { let _ = std::fs::rename(&tmp, &p); }
        }
    }
}

/// Sessions for the UCC table: live tmux sessions plus in-flight ("starting…") and failed launches.
/// Only records whose launch COMPLETED ("running"/legacy-empty) are pruned when their tmux is gone —
/// a starting row's tmux doesn't exist yet, and a failed row must stay visible until the user
/// closes it (that's how launch errors reach the phone).
pub(crate) fn live_sessions() -> Vec<SessionBrief> {
    let _g = registry_lock();
    let now = crate::share::now_unix();
    let all = load_sessions();
    let (keep, dead): (Vec<_>, Vec<_>) = all.into_iter().partition(|s| {
        let starting_fresh = s.status.starts_with("starting")
            && now.saturating_sub(s.started_at) < STARTING_TTL_SECS;
        starting_fresh || s.status.starts_with("failed") || tmux_alive(&s.id)
    });
    if !dead.is_empty() { save_sessions(&keep); }
    keep.into_iter()
        .map(|s| {
            let alive = tmux_alive(&s.id);
            let status = if !s.status.is_empty() { s.status } else { "running".into() };
            SessionBrief { id: s.id, agent: s.agent, model: s.model, workdir: s.workdir, alive, status }
        })
        .collect()
}

#[derive(Deserialize)]
pub(crate) struct SessionLaunchReq {
    pub(crate) agent: String,
    pub(crate) model: String,
    pub(crate) workdir: String,
    #[serde(default)]
    pub(crate) prompt: String,
}

/// Terminal sessions are `tmux`, and Windows does not have it.
///
/// Everything below drives a multiplexer: `new-session`, `send-keys`, `capture-pane`, and a PTY
/// bridge attached to a running pane. The Windows equivalent is ConPTY plus a session manager
/// written from scratch — a project, not a platform arm. A seam that pretended otherwise would fail
/// at the first `Command::new("tmux")` with the OS's own "program not found", four layers below the
/// caller and after a registry entry had already been written. The decision is recorded in BACKLOG
/// as `windows-tmux-bash-refusal`; this is it, said at the door.
///
/// Only LAUNCH is guarded. `stop`, `send`, `output` and `attach` all take a session id, and on a
/// platform where none can be created they answer "no such session" — which is true, and is the
/// message they already give.
#[cfg(windows)]
pub(crate) async fn session_launch_route(_body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    (
        axum::http::StatusCode::NOT_IMPLEMENTED,
        axum::Json(serde_json::json!({
            "ok": false,
            "error": "terminal sessions need tmux, which Windows does not have. The phone \
                      terminal drives it through new-session/send-keys/capture-pane and a PTY \
                      bridge; on Windows that is ConPTY plus a session manager, not a shim. \
                      The rest of the console works.",
        })),
    )
        .into_response()
}

#[cfg(not(windows))]
pub(crate) async fn session_launch_route(body: String) -> axum::response::Response {
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
    // Same preflight as the coder path: a tmux session whose only content is "command not found"
    // still counts as `tmux_alive`, so it would sit in the list as a live session forever.
    if let Some(why) = agent_missing_reason(&agent) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, &why);
    }
    // Create the tmux session RIGHT AWAY and run `rozum launch` inside it: the CLI handles the
    // gateway cold-start itself (admission gate included) and PRINTS its progress, so the terminal
    // the launch button opens (onOpenJson) shows the WHOLE startup live — gateway spawn, model
    // load, agent REPL. tmux creation is instant, so the phone's fetch returns immediately (no
    // funnel-timeout window).
    let prompt = req.prompt.trim().to_string();
    // seq suffix: two launches of the same agent in one wall-clock second would otherwise share a
    // tmux name → the second `new-session` fails with a duplicate-name error (500, no record).
    let id = format!("{}-{}-{}", sanitize(&agent), crate::share::now_unix(), next_launch_seq());
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("rozum"));
    let name = tmux_name(&id);
    // Wrap the launch so the pane HOLDS after `rozum launch` exits (any reason) instead of closing:
    // print the exit code, then block on `sleep`. This keeps a failed launch's output on screen
    // until the row is ✕-closed — race-free, unlike a separate `set-option remain-on-exit` which
    // a sub-millisecond failure could beat (BUG-012 residual). model/agent are shell_safe-validated;
    // the exe path is single-quoted against spaces.
    let inner = format!(
        "'{}' launch --model {} {}; rc=$?; printf '\\n[rozum launch exited: %s — ✕ to close this session]\\n' \"$rc\"; exec sleep 2147483647",
        exe.to_string_lossy(),
        model,
        agent,
    );
    let ok = Command::new("tmux")
        .args(["new-session", "-d", "-s", &name, "-c", &workdir, "-x", "120", "-y", "40", &inner])
        .status().map(|s| s.success()).unwrap_or(false);
    if !ok {
        return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, "tmux new-session failed");
    }
    // mouse on: tmux consumes the client's mouse/touch reports itself (finger-scroll = tmux
    // scrollback) instead of piping them to the agent, where a phone tap leaked as literal
    // "^[[<35;17;45M" garbage into the REPL input (SGR mouse reports, seen live 2026-07-07).
    let _ = Command::new("tmux").args(["set-option", "-t", &name, "mouse", "on"]).status();
    // escape-time 0: forward a lone ESC (the on-screen Esc key) immediately instead of holding it
    // to see whether a sequence follows — held ESC glues with later bytes and wedges the agent's
    // input parser. focus-events off: on client detach tmux otherwise sends the app a focus-out
    // (^[[O) which leaks into the input line as literal text (seen live 2026-07-08).
    let _ = Command::new("tmux").args(["set-option", "-t", &name, "escape-time", "0"]).status();
    let _ = Command::new("tmux").args(["set-option", "-t", &name, "focus-events", "off"]).status();
    {
        let _g = registry_lock();
        let mut sessions = load_sessions();
        sessions.push(SessionRecord {
            id: id.clone(),
            agent: agent.clone(),
            model: model.clone(),
            workdir: workdir.clone(),
            prompt: prompt.clone(),
            started_at: crate::share::now_unix(),
            status: "running".into(),
        });
        save_sessions(&sessions);
    }
    // Once the agent's REPL is actually up (gateway health can take minutes on a cold start, and keys
    // sent into the loading phase would be swallowed): first CLEAR the terminal-probe garbage, then
    // seed the launch prompt if any. Runs for EVERY session — an empty-prompt interactive session
    // still needs its input cleaned.
    //
    // The garbage: Claude Code probes the terminal at startup (XTVERSION `^[[>q`, DA `^[[c`). tmux
    // answers those into CC's stdin AFTER CC's read window, so the responses (`^[P>|tmux 3.7b^[\` +
    // `^[[?1;2;4c`) land as literal text in the REPL input line. That not only looks like garbage — it
    // WEDGES Return (submitting the garbage-laden line does nothing useful). This all happens inside
    // the pty (tmux→CC), so the client-side xterm input filter can never reach it; a Ctrl-U from the
    // host side, once the REPL is up, is what clears it. Verified live 2026-07-08.
    {
        let seed_name = name.clone();
        let seed_model = model.clone();
        let seed_id = id.clone();
        let seed_prompt = prompt.clone();
        tokio::spawn(async move {
            for _ in 0..600u32 {
                if let Some(g) = crate::share::read_active() {
                    if g.model == seed_model && crate::share::health_ok(g.port).await {
                        break;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            tokio::time::sleep(std::time::Duration::from_secs(4)).await; // REPL warm-up
            if !load_sessions().iter().any(|s| s.id == seed_id) {
                return;
            }
            // Ctrl-U twice (800ms apart) clears the probe garbage — the second catches anything that
            // landed slightly late during CC's init. Clearing an empty input line is a harmless no-op.
            let _ = Command::new("tmux").args(["send-keys", "-t", &seed_name, "C-u"]).status();
            tokio::time::sleep(std::time::Duration::from_millis(800)).await;
            let _ = Command::new("tmux").args(["send-keys", "-t", &seed_name, "C-u"]).status();
            if !seed_prompt.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let _ = Command::new("tmux").args(["send-keys", "-t", &seed_name, &seed_prompt, "Enter"]).status();
            }
        });
    }
    axum::Json(serde_json::json!({ "ok": true, "id": id, "status": "running" })).into_response()
}

pub(crate) async fn session_stop_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let id = match parse_id_body(&body) {
        Ok(id) => id,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let _ = Command::new("tmux").args(["kill-session", "-t", &tmux_name(&id)]).status();
    {
        let _g = registry_lock();
        let mut sessions = load_sessions();
        sessions.retain(|s| s.id != id);
        save_sessions(&sessions);
    }
    axum::Json(serde_json::json!({ "ok": true, "id": id })).into_response()
}

/// A session id is `<agent>-<unix>-<seq>` — a strict alnum/`-`/`_` charset. Validate before it becomes a
/// tmux target (`rozum-<id>`), so no crafted id can reach an unintended pane.
pub(crate) fn session_id_safe(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The only tmux key-names the chat page may inject as CONTROL keys (interactive agents need Esc / ^C /
/// ^U). Everything the user types is sent as LITERAL text (`send-keys -l`) instead, so this stays tiny.
pub(crate) fn allowed_session_key(k: &str) -> bool {
    matches!(k, "Enter" | "Escape" | "Tab" | "BTab" | "Up" | "Down" | "Left" | "Right" | "C-c" | "C-u" | "C-d")
}

#[derive(Deserialize)]
pub(crate) struct SessionSendReq {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) input: String,
    /// Optional control keys (whitelist) sent AFTER the literal input — e.g. `["Enter"]` to submit.
    #[serde(default)]
    pub(crate) keys: Vec<String>,
    /// When true (default) and no explicit `keys`, an `Enter` is appended so the typed line submits.
    #[serde(default = "default_true")]
    pub(crate) submit: bool,
}

/// POST `/control/session/send` — type a line into the agent's tmux session. Literal text via
/// `send-keys -l` (tmux does NOT interpret key-names in it), then Enter (or the whitelisted keys).
pub(crate) async fn session_send_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req: SessionSendReq = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &format!("bad body: {e}")),
    };
    if !session_id_safe(&req.id) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad session id");
    }
    if req.keys.iter().any(|k| !allowed_session_key(k)) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "unsupported control key");
    }
    if !tmux_alive(&req.id) {
        return json_err(axum::http::StatusCode::NOT_FOUND, "session not running");
    }
    let name = tmux_name(&req.id);
    if !req.input.is_empty() {
        // `-l` = literal: the input is data, never a key-name, so no metachar can trigger an action.
        let _ = Command::new("tmux").args(["send-keys", "-t", &name, "-l", "--", &req.input]).status();
    }
    if !req.keys.is_empty() {
        for k in &req.keys {
            let _ = Command::new("tmux").args(["send-keys", "-t", &name, k]).status();
        }
    } else if req.submit {
        let _ = Command::new("tmux").args(["send-keys", "-t", &name, "Enter"]).status();
    }
    axum::Json(serde_json::json!({ "ok": true, "id": req.id })).into_response()
}

#[derive(Deserialize)]
pub(crate) struct SessionOutputQuery {
    pub(crate) id: String,
    #[serde(default = "default_scrollback")]
    pub(crate) scrollback: usize,
}

/// GET `/control/session/output?id=<id>` — the session pane as CLEAN text (`tmux capture-pane -p -J`
/// strips every escape sequence and joins wrapped lines), including up to `scrollback` lines of history.
/// This is what makes the chat view garbage-free — no raw PTY bytes, no terminal-probe replies.
pub(crate) async fn session_output_route(
    axum::extract::Query(q): axum::extract::Query<SessionOutputQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if !session_id_safe(&q.id) {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "bad session id");
    }
    if !tmux_alive(&q.id) {
        return axum::Json(serde_json::json!({ "id": q.id, "alive": false, "output": "" })).into_response();
    }
    let name = tmux_name(&q.id);
    let start = format!("-{}", q.scrollback.min(10000));
    let out = Command::new("tmux")
        .args(["capture-pane", "-p", "-J", "-t", &name, "-S", &start])
        .output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    axum::Json(serde_json::json!({ "id": q.id, "alive": true, "output": text })).into_response()
}

/// WS: bridge a PTY running `tmux attach -t rozum-<id>` to the browser terminal. Closing the WS ends the
/// ATTACH (the tmux session persists → reconnect re-attaches). Text frame `{"resize":{cols,rows}}` resizes.
pub(crate) async fn session_attach_route(
    ws: axum::extract::ws::WebSocketUpgrade,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if !tmux_alive(&id) {
        return json_err(axum::http::StatusCode::NOT_FOUND, "no such session");
    }
    ws.on_upgrade(move |socket| session_ws_bridge(socket, id))
}

pub(crate) async fn session_ws_bridge(mut socket: axum::extract::ws::WebSocket, id: String) {
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
    // control-serve runs under launchd, whose environment has NO TERM — the tmux client then
    // refuses the PTY with "open terminal failed: terminal does not support clear" and the WS
    // drops immediately (BUG-011, first real phone attach). xterm.js is xterm-compatible.
    cmd.env("TERM", "xterm-256color");
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
