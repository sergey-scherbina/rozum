//! `rozum mcp-proxy` for the meeting daemon.
//!
//! A stdio MCP server each agent (Claude Code / Codex) launches. It bridges the
//! agent to the single daemon `meeting.sock`: it generates a `session_token`
//! once (held for its lifetime — the reconnect key), detects the agent's project
//! (git root / cwd), auto-spawns the daemon if needed, auto-joins the project's
//! room, and forwards `rooms.*` / `meeting.*` to the daemon. It tracks the
//! `(date, n)` read cursor so the agent's `meeting.wait_my_turn` loop needs no
//! arguments. See `docs/specs/agent-meetings-daemon.md`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rmcp::{
    ErrorData, Peer, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, CustomNotification, Implementation, InitializeRequestParams,
        InitializeResult, JsonObject, ServerCapabilities, ServerNotification,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use super::daemon::daemon_alive;
use super::room_client::{RoomConnection, tool_result_text_json};
use super::room_path::meeting_sock;
use super::store;

const CALL_TIMEOUT: Duration = Duration::from_secs(35);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How often the channel-wakeup task tails the room transcript on disk. Well within
/// the 25 s `wait_my_turn` cycle the spec measures against, and cheap (a seek-from-cursor
/// read of small day files). See `docs/specs/channel-wakeup.md`.
const WAKEUP_POLL: Duration = Duration::from_millis(1500);

/// The proxy's MCP `instructions`: the room loop plus how to read channel wakeup events.
const PROXY_INSTRUCTIONS: &str =
    "You are in a rozum meeting room with the human operator and any sibling agents working on \
     this project. Use it to COORDINATE — it is how you avoid clashing with other agents and how \
     the human sees + steers your work.\n\
     \n\
     Coordinate on your own judgement, when it helps (not on every step):\n\
     - When you START something non-trivial, meeting.submit a short `working: <what>` so siblings \
       and the human know.\n\
     - BEFORE editing files or starting a task, check recent room messages (meeting.wait_my_turn / \
       meeting.status): if a sibling is already on the same files/task, coordinate instead of \
       clashing. Check the `responding` list so two agents don't write the same reply.\n\
     - When you are BLOCKED or unsure, ask in the room — the human or a sibling may answer.\n\
     - When you FINISH (or stop), meeting.submit `done: <result>` or `blocked: <why>`.\n\
     - The human may post at any time to redirect you; treat their messages as priority.\n\
     Keep messages short. Anyone may submit at any time — no turn-taking, no moderator.\n\
     \n\
     Mechanics: meeting.wait_my_turn (25s long-poll, no args) → meeting.submit. \
     rooms.list / rooms.join switch rooms. While idle you may also receive room activity pushed as \
     <channel source=\"rozum\" room=\"…\" from=\"…\" seq=\"…\"> events: treat each as a wakeup — if \
     it concerns you, call meeting.wait_my_turn for the authoritative delta, then act. The channel \
     body is a preview, not the turn API. If your client does NOT deliver <channel> events, keep a \
     meeting.wait_my_turn poll outstanding while idle so you never miss a message.";

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct JoinParams {
    /// Room name (from rooms.list). Switches the session to this room.
    pub name: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    /// Your message.
    pub content: String,
}

struct State {
    sock: PathBuf,
    project: Option<String>,
    session_token: String,
    client_info_name: String,
    conn: Option<RoomConnection>,
    /// The joined room's on-disk dir — the proxy reads content from here.
    room_root: Option<PathBuf>,
    /// Last `(date, n)` high-water the agent has seen; the `wait` cursor.
    cursor: Option<(String, u64)>,
    auto_spawn: bool,
    /// The Claude Code session peer (set at `initialize`). The channel-wakeup task
    /// pushes `notifications/claude/channel` events here. Spec: `docs/specs/channel-wakeup.md`.
    upstream_peer: Option<Peer<RoleServer>>,
    /// Our own `participant_id` in the joined room (from the join result) — the
    /// channel task skips the agent's own transcript entries so it isn't echoed back.
    self_pid: Option<String>,
    /// The joined room's name, for the channel notification `meta`.
    room_name: Option<String>,
    /// The background channel-wakeup task (disk-tails the room, pushes deltas to the
    /// session). One per proxy; started lazily at `initialize`, aborted on teardown.
    wakeup_task: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Clone)]
pub struct DaemonProxy {
    state: Arc<Mutex<State>>,
    tool_router: ToolRouter<Self>,
}

impl Default for DaemonProxy {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonProxy {
    pub fn new() -> Self {
        Self::build(meeting_sock(), detect_project(), "agent".to_string(), true)
    }

    fn build(sock: PathBuf, project: Option<String>, client: String, auto_spawn: bool) -> Self {
        let session_token = uuid::Uuid::new_v4().simple().to_string();
        Self {
            state: Arc::new(Mutex::new(State {
                sock,
                project,
                session_token,
                client_info_name: client,
                conn: None,
                room_root: None,
                cursor: None,
                auto_spawn,
                upstream_peer: None,
                self_pid: None,
                room_name: None,
                wakeup_task: None,
            })),
            tool_router: Self::tool_router(),
        }
    }

    /// Connect to the daemon (spawning it if needed) and `_join_internal` the
    /// project room. Idempotent while the connection is live.
    async fn ensure(&self) -> Result<(), String> {
        let mut s = self.state.lock().await;
        if s.conn.is_some() {
            return Ok(());
        }
        if s.auto_spawn && !daemon_alive(&s.sock).await {
            spawn_daemon().await;
        }
        let mut conn = RoomConnection::connect(&s.sock, &s.client_info_name, CONNECT_TIMEOUT)
            .await
            .map_err(|e| format!("connect-daemon: {e}"))?;
        let mut args = json!({
            "client_info_name": s.client_info_name,
            "session_token": s.session_token,
        });
        if let Some(p) = &s.project {
            args["project"] = json!(p);
        }
        let join = conn
            .call_tool("_join_internal", args, CONNECT_TIMEOUT)
            .await
            .map_err(|e| format!("join: {e}"))?;
        if let Some(j) = tool_result_text_json(&join) {
            if let Some(root) = j.get("root").and_then(Value::as_str) {
                s.room_root = Some(PathBuf::from(root));
            }
            if let Some(pid) = j.get("participant_id").and_then(Value::as_str) {
                s.self_pid = Some(pid.to_string());
            }
            if let Some(name) = j.get("room").and_then(Value::as_str) {
                s.room_name = Some(name.to_string());
            }
        }
        s.conn = Some(conn);
        Ok(())
    }

    /// Forward a tool call to the daemon, returning the daemon's raw result value.
    /// On transport error the connection is dropped (next call reconnects).
    async fn forward_raw(&self, tool: &str, params: Value) -> Result<Value, CallToolResult> {
        if let Err(e) = self.ensure().await {
            return Err(err_result(&e));
        }
        let mut s = self.state.lock().await;
        let res = {
            let Some(conn) = s.conn.as_mut() else {
                return Err(err_result("no daemon connection"));
            };
            conn.call_tool(tool, params, CALL_TIMEOUT).await
        };
        match res {
            Ok(v) => Ok(v),
            Err(e) => {
                s.conn = None;
                Err(err_result(&format!("daemon-error: {e}")))
            }
        }
    }

    /// Forward a tool call to the daemon, converting the result for the agent.
    async fn forward(&self, tool: &str, params: Value) -> CallToolResult {
        match self.forward_raw(tool, params).await {
            Ok(v) => value_to_call_result(&v),
            Err(e) => e,
        }
    }

    /// Start the channel-wakeup task once. It disk-tails the joined room and pushes new
    /// transcript deltas to the agent session as `notifications/claude/channel` events, so an
    /// idle agent is woken without holding its own `wait_my_turn`. Reads the room from `State`
    /// every tick, so it idles before a join and re-primes on a room switch; it never consumes
    /// the agent's turn (a read-only disk tail), skips the agent's own entries, and primes the
    /// baseline to the current head so a fresh join replays no backlog. Best-effort: a send
    /// failure (no channel listener / dead peer) is dropped. Spec: `docs/specs/channel-wakeup.md`.
    async fn ensure_wakeup_task(&self) {
        let mut s = self.state.lock().await;
        if s.wakeup_task.is_some() {
            return;
        }
        let state = Arc::clone(&self.state);
        s.wakeup_task = Some(tokio::spawn(async move {
            // The room this loop is primed against, and the next `(date, n)` to deliver
            // (one past the last delivered entry — `read_since` is inclusive of `n`).
            let mut primed_root: Option<PathBuf> = None;
            let mut since: Option<(String, u64)> = None;
            loop {
                tokio::time::sleep(WAKEUP_POLL).await;
                let (root, peer, self_pid, room_name, agent) = {
                    let s = state.lock().await;
                    match (s.room_root.clone(), s.upstream_peer.clone()) {
                        (Some(root), Some(peer)) => (
                            root,
                            peer,
                            s.self_pid.clone(),
                            s.room_name.clone().unwrap_or_default(),
                            s.client_info_name.clone(),
                        ),
                        // Not joined yet, or no session peer: idle.
                        _ => continue,
                    }
                };
                // First arm, or a room switch: prime to the current head and skip the push,
                // so a join/switch never replays the backlog as a notification storm.
                if primed_root.as_deref() != Some(root.as_path()) {
                    since = transcript_head(&root);
                    primed_root = Some(root);
                    continue;
                }
                let (sd, sn) = match &since {
                    Some((d, n)) => (Some(d.as_str()), *n),
                    None => (None, 0),
                };
                let turns = store::read_since(&root, sd, sn);
                let Some(last) = turns.last() else { continue };
                // Advance past everything read (own entries included), so we neither re-read
                // nor re-push them on the next tick.
                since = Some((last.date.clone(), last.n + 1));
                if let Some((content, from, seq)) = render_stored_delta(&turns, self_pid.as_deref())
                {
                    // Tier-3 piggyback fallback: also drop the delta where the launch-local HTTP
                    // proxy can inject it (clients with neither channels nor a wait loop). Auto-off
                    // when Tier-1 channels are active. Spec: `docs/specs/rozum-native-channels.md`.
                    if super::piggyback::enabled() {
                        super::piggyback::append(&super::piggyback::project_slug(), &agent, &content);
                    }
                    let meta = json!({
                        "room": room_name,
                        "from": from,
                        "seq": seq,
                        "your_turn": "false",
                    });
                    let notif = ServerNotification::CustomNotification(CustomNotification::new(
                        "notifications/claude/channel",
                        Some(json!({ "content": content, "meta": meta })),
                    ));
                    let _ = peer.send_notification(notif).await;
                }
            }
        }));
    }
}

#[tool_router(router = tool_router)]
impl DaemonProxy {
    #[tool(
        name = "rooms.list",
        description = "List meeting rooms known to the daemon."
    )]
    pub async fn rooms_list(&self) -> CallToolResult {
        self.forward("rooms.list", json!({})).await
    }

    #[tool(
        name = "rooms.join",
        description = "Switch to a room by name (you start in your project's room)."
    )]
    pub async fn rooms_join(&self, params: Parameters<JoinParams>) -> CallToolResult {
        let v = match self
            .forward_raw("rooms.join", json!({ "name": params.0.name }))
            .await
        {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Re-point the proxy at the new room so both the agent's disk reads and the
        // channel-wakeup task follow the switch (the task re-primes on the new root).
        if let Some(j) = tool_result_text_json(&v) {
            let mut s = self.state.lock().await;
            if let Some(root) = j.get("root").and_then(Value::as_str) {
                s.room_root = Some(PathBuf::from(root));
            }
            if let Some(pid) = j.get("participant_id").and_then(Value::as_str) {
                s.self_pid = Some(pid.to_string());
            }
            if let Some(name) = j.get("room").and_then(Value::as_str) {
                s.room_name = Some(name.to_string());
            }
            s.cursor = None; // fresh room → reset the wait cursor
        }
        value_to_call_result(&v)
    }

    #[tool(
        name = "meeting.submit",
        description = "Submit a message. Anyone can submit at any time."
    )]
    pub async fn submit(&self, params: Parameters<SubmitParams>) -> CallToolResult {
        self.forward("meeting.submit", json!({ "content": params.0.content }))
            .await
    }

    #[tool(
        name = "meeting.wait_my_turn",
        description = "Long-poll (25s) for new messages. No args — the proxy tracks your cursor. Retry on still_waiting."
    )]
    pub async fn wait_my_turn(&self) -> CallToolResult {
        if let Err(e) = self.ensure().await {
            return err_result(&e);
        }
        let mut s = self.state.lock().await;
        let since_cursor = s.cursor.clone();
        let since = match &since_cursor {
            Some((d, n)) => json!({ "since_date": d, "since_n": n }),
            None => json!({}),
        };
        let res = {
            let Some(conn) = s.conn.as_mut() else {
                return err_result("no daemon connection");
            };
            conn.call_tool("meeting.wait_my_turn", since, CALL_TIMEOUT)
                .await
        };
        match res {
            Ok(v) => {
                let payload = tool_result_text_json(&v);
                let still_waiting = payload
                    .as_ref()
                    .and_then(|p| p.get("still_waiting"))
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let hw = payload.as_ref().and_then(|p| p.get("high_water")).cloned();
                if let Some(hw) = &hw {
                    if let (Some(d), Some(n)) = (
                        hw.get("date").and_then(Value::as_str),
                        hw.get("n").and_then(Value::as_u64),
                    ) {
                        s.cursor = Some((d.to_string(), n));
                    }
                }
                // New messages: read the content from disk and hand it to the agent.
                if !still_waiting {
                    if let Some(root) = s.room_root.clone() {
                        let (sd, sn) = match &since_cursor {
                            Some((d, n)) => (Some(d.as_str()), *n),
                            None => (None, 0),
                        };
                        let turns = store::read_since(&root, sd, sn);
                        let payload = json!({
                            "still_waiting": false,
                            "turns": turns,
                            "high_water": hw,
                        });
                        return CallToolResult::success(vec![Content::text(payload.to_string())]);
                    }
                }
                value_to_call_result(&v)
            }
            Err(e) => {
                s.conn = None;
                err_result(&format!("daemon-error: {e}"))
            }
        }
    }

    #[tool(
        name = "meeting.mark_responding",
        description = "Signal you are composing a response."
    )]
    pub async fn mark_responding(&self) -> CallToolResult {
        self.forward("meeting.mark_responding", json!({})).await
    }

    #[tool(name = "meeting.status", description = "Current room status.")]
    pub async fn status(&self) -> CallToolResult {
        self.forward("meeting.status", json!({})).await
    }

    #[tool(name = "meeting.leave", description = "Leave the current room.")]
    pub async fn leave(&self) -> CallToolResult {
        let r = self.forward("meeting.leave", json!({})).await;
        // Stop pushing wakeups for a room we've left (the task idles on a null room_root).
        let mut s = self.state.lock().await;
        s.room_root = None;
        s.self_pid = None;
        s.cursor = None;
        r
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DaemonProxy {
    async fn initialize(
        &self,
        params: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let name = params.client_info.name;
        {
            let mut s = self.state.lock().await;
            if !name.is_empty() {
                s.client_info_name = name;
            }
            // Hold the session peer so the channel-wakeup task can push to it.
            s.upstream_peer = Some(context.peer.clone());
        }
        // Interactive Claude Code registers a channel listener for this experimental
        // capability; clients that ignore it fall back to `wait_my_turn` unchanged.
        self.ensure_wakeup_task().await;
        Ok(InitializeResult::new(channel_capabilities())
            .with_server_info(Implementation::new(
                "rozum-mcp-proxy",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(PROXY_INSTRUCTIONS))
    }
}

/// Serve the proxy over stdio (the entry point for `rozum mcp-proxy`).
pub async fn run_daemon_proxy() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use rmcp::{ServiceExt, transport::stdio};
    let server = DaemonProxy::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// The agent's project: the nearest ancestor with a `.git`, else the cwd.
pub fn detect_project() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir = cwd.as_path();
    loop {
        if dir.join(".git").exists() {
            return Some(dir.to_string_lossy().into_owned());
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
    Some(cwd.to_string_lossy().into_owned())
}

pub async fn spawn_daemon() {
    if let Ok(exe) = std::env::current_exe() {
        // `meetings start` spawns the detached daemon and waits for its socket.
        let _ = tokio::process::Command::new(exe)
            .args(["meetings", "start"])
            .status()
            .await;
    }
}

fn value_to_call_result(v: &Value) -> CallToolResult {
    let is_error = v.get("isError").and_then(Value::as_bool).unwrap_or(false);
    let text = v
        .get("content")
        .and_then(Value::as_array)
        .and_then(|a| a.iter().find_map(|c| c.get("text").and_then(Value::as_str)))
        .unwrap_or("");
    if is_error {
        CallToolResult::error(vec![Content::text(text)])
    } else {
        CallToolResult::success(vec![Content::text(text)])
    }
}

fn err_result(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

/// The proxy's server capabilities: tools + the experimental Claude Code channel capability
/// (`experimental: {"claude/channel": {}}`). Spec: `docs/specs/channel-wakeup.md`.
fn channel_capabilities() -> ServerCapabilities {
    let mut caps = ServerCapabilities::builder().enable_tools().build();
    caps.experimental
        .get_or_insert_with(Default::default)
        .insert("claude/channel".to_owned(), JsonObject::new());
    caps
}

/// Render new transcript turns into a channel-event body, skipping the agent's own entries
/// (`self_pid`) so it isn't echoed back. Returns `(content, last_from, seq)`, or `None` when
/// nothing remains to push.
fn render_stored_delta(
    turns: &[store::StoredTurn],
    self_pid: Option<&str>,
) -> Option<(String, String, String)> {
    let mut lines = Vec::new();
    let mut last_from = String::new();
    let mut last_seq = String::new();
    for t in turns {
        if self_pid == Some(t.participant_id.as_str()) {
            continue;
        }
        let from = if t.display_name.is_empty() {
            t.participant_id.as_str()
        } else {
            t.display_name.as_str()
        };
        lines.push(format!("{from}: {}", t.content));
        last_from = from.to_owned();
        last_seq = format!("{}:{}", t.date, t.n);
    }
    if lines.is_empty() {
        None
    } else {
        Some((lines.join("\n"), last_from, last_seq))
    }
}

/// The `(date, next_n)` just past the transcript head — the baseline a fresh join/switch primes
/// to, so the wakeup task pushes only what arrives afterwards. `None` for an empty room.
fn transcript_head(root: &std::path::Path) -> Option<(String, u64)> {
    store::read_since(root, None, 0)
        .last()
        .map(|t| (t.date.clone(), t.n + 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::daemon::serve_daemon;
    use crate::meeting::registry::RoomRegistry;
    use std::path::Path;
    use tempfile::tempdir;

    async fn wait_for_socket(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("socket never appeared");
    }

    #[tokio::test]
    async fn proxy_auto_joins_submits_and_tails() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("meeting.sock");
        let registry = Arc::new(RoomRegistry::new(dir.path().join("state")));
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let _ = serve_daemon(&sock, registry).await;
            });
        }
        wait_for_socket(&sock).await;

        // A project dir for this agent (its room materializes here).
        let project = tempdir().unwrap();
        let proxy = DaemonProxy::build(
            sock,
            Some(project.path().to_string_lossy().into_owned()),
            "claude".into(),
            false, // no auto-spawn; the daemon is already up
        );

        // Submit (auto-connects + auto-joins the project room first).
        let submitted = proxy
            .submit(Parameters(SubmitParams {
                content: "hello via proxy".into(),
            }))
            .await;
        assert_ne!(submitted.is_error, Some(true), "submit should succeed");

        // wait_my_turn (no args) returns the message via the tracked cursor.
        let wait = proxy.wait_my_turn().await;
        let payload = tool_result_json(&wait);
        assert_eq!(payload["still_waiting"], false);
        let turns = payload["turns"].as_array().unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["content"], "hello via proxy");

        // The cursor advanced: a second wait with no new messages → still_waiting.
        let again = proxy.wait_my_turn().await;
        assert_eq!(tool_result_json(&again)["still_waiting"], true);

        // rooms.list now shows the materialized project room.
        let list = tool_result_json(&proxy.rooms_list().await);
        let names: Vec<_> = list["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect();
        let expected = Path::new(project.path())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(
            names.contains(&expected),
            "expected room {expected} in {names:?}"
        );
    }

    // ── Channel-wakeup unit tests (pure, no MCP peer) ───────────────────────────

    fn sturn(pid: &str, name: &str, content: &str, date: &str, n: u64) -> store::StoredTurn {
        store::StoredTurn {
            date: date.into(),
            n,
            participant_id: pid.into(),
            display_name: name.into(),
            content: content.into(),
            ts: 0,
        }
    }

    #[test]
    fn render_stored_delta_skips_own_and_formats() {
        let turns = vec![
            sturn("p1", "alice", "hello", "2026-06-18", 0),
            sturn("me", "bob", "my own message", "2026-06-18", 1),
            sturn("p2", "carol", "hi there", "2026-06-18", 2),
        ];
        let (content, from, seq) = render_stored_delta(&turns, Some("me")).unwrap();
        assert_eq!(content, "alice: hello\ncarol: hi there");
        assert_eq!(from, "carol");
        assert_eq!(seq, "2026-06-18:2");
    }

    #[test]
    fn render_stored_delta_all_own_is_none() {
        let turns = vec![sturn("me", "bob", "x", "2026-06-18", 0)];
        assert!(render_stored_delta(&turns, Some("me")).is_none());
    }

    #[test]
    fn channel_capability_and_instructions_declared() {
        let caps = channel_capabilities();
        assert!(
            caps.experimental
                .as_ref()
                .is_some_and(|e| e.contains_key("claude/channel")),
            "experimental claude/channel capability must be advertised"
        );
        assert!(PROXY_INSTRUCTIONS.contains("channel"), "instructions must teach channel wakeup");
    }

    #[test]
    fn transcript_head_primes_past_the_backlog() {
        // A room with two existing turns: priming a fresh join must skip both, then deliver
        // only what arrives afterwards. Exercises transcript_head + read_since + render together.
        let dir = tempdir().unwrap();
        let state = dir.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let paths = store::RoomPaths::ad_hoc_in(&state, "wakeup-test");
        let root = paths.root.clone();
        let mut w = store::TranscriptWriter::new(paths, "wakeup-test", "", None, state.clone());
        w.append("p1", "alice", "old one", 0).unwrap();
        w.append("p1", "alice", "old two", 0).unwrap();

        // Prime: baseline is just past the head → no backlog replayed.
        let since = transcript_head(&root).unwrap();
        let (sd, sn) = (Some(since.0.as_str()), since.1);
        assert!(store::read_since(&root, sd, sn).is_empty(), "backlog must not replay");

        // A new turn from someone else → delivered; the agent's own turn → skipped.
        w.append("p2", "carol", "fresh news", 0).unwrap();
        w.append("me", "bob", "my reply", 0).unwrap();
        let turns = store::read_since(&root, sd, sn);
        let (content, from, _) = render_stored_delta(&turns, Some("me")).unwrap();
        assert_eq!(content, "carol: fresh news");
        assert_eq!(from, "carol");
    }

    /// Extract a tool result's text payload as JSON (the proxy returns the
    /// daemon's text verbatim).
    fn tool_result_json(r: &CallToolResult) -> Value {
        let v = serde_json::to_value(r).unwrap_or(Value::Null);
        let text = v
            .get("content")
            .and_then(Value::as_array)
            .and_then(|a| a.iter().find_map(|c| c.get("text").and_then(Value::as_str)))
            .unwrap_or("");
        serde_json::from_str(text).unwrap_or(Value::Null)
    }
}
