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
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeRequestParams, InitializeResult,
        ServerCapabilities,
    },
    service::RequestContext,
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
        if let Some(root) = tool_result_text_json(&join)
            .as_ref()
            .and_then(|v| v.get("root"))
            .and_then(Value::as_str)
        {
            s.room_root = Some(PathBuf::from(root));
        }
        s.conn = Some(conn);
        Ok(())
    }

    /// Forward a tool call to the daemon, reconnecting once on transport error.
    async fn forward(&self, tool: &str, params: Value) -> CallToolResult {
        if let Err(e) = self.ensure().await {
            return err_result(&e);
        }
        let mut s = self.state.lock().await;
        let res = {
            let Some(conn) = s.conn.as_mut() else {
                return err_result("no daemon connection");
            };
            conn.call_tool(tool, params, CALL_TIMEOUT).await
        };
        match res {
            Ok(v) => value_to_call_result(&v),
            Err(e) => {
                s.conn = None;
                err_result(&format!("daemon-error: {e}"))
            }
        }
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
        self.forward("rooms.join", json!({ "name": params.0.name }))
            .await
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
        self.forward("meeting.leave", json!({})).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for DaemonProxy {
    async fn initialize(
        &self,
        params: InitializeRequestParams,
        _context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let name = params.client_info.name;
        if !name.is_empty() {
            self.state.lock().await.client_info_name = name;
        }
        Ok(
            InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new(
                    "rozum-mcp-proxy",
                    env!("CARGO_PKG_VERSION"),
                ))
                .with_instructions(
                    "You are connected to a rozum meeting room (your project's room). \
                     Loop: meeting.wait_my_turn (25s long-poll, no args) → meeting.submit if \
                     you have something to add. Anyone may submit at any time. rooms.list / \
                     rooms.join switch rooms.",
                ),
        )
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
fn detect_project() -> Option<String> {
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

async fn spawn_daemon() {
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
