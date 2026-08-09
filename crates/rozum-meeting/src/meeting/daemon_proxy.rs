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
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
use super::room_path::{meeting_sock, rozum_runtime_dir};
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
     <channel source=\"rozum\" room=\"…\" from=\"…\" seq=\"…\" mentioned=\"true|false\"> events: treat \
     each as a wakeup — and when mentioned=\"true\" the message ADDRESSES YOU by handle (@you / -> you), \
     so prioritize it. Call meeting.wait_my_turn for the authoritative delta, then act. The channel \
     body is a preview, not the turn API. If your client does NOT deliver <channel> events, keep a \
     meeting.wait_my_turn poll outstanding while idle so you never miss a message; you can also run \
     `rozum meetings inbox --as <your-handle>` anytime to see messages addressed to you.";

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
    /// A shared room to join instead of the per-project room (`ROZUM_MEETING_ROOM`), so the
    /// operator can route all agents into one common room (e.g. `commons`) for a single
    /// overview. `None` = the project's canonical room. Spec: agent-meeting-coordination P1.2.
    shared_room: Option<String>,
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
    /// Whether this proxy has posted its `joined:` presence line yet — posted once on the
    /// first join (NOT on reconnects), via the agent's own session so it shares the agent's
    /// handle and works for every agent (no per-client hooks). Spec: agent-meeting-coordination.
    presence_announced: bool,
}

#[derive(Clone)]
pub struct DaemonProxy {
    state: Arc<Mutex<State>>,
    tool_router: ToolRouter<Self>,
    /// Epoch-seconds of the agent's last MCP request, updated in `forward_raw`. The idle
    /// watchdog reaps a proxy whose agent has gone silent (abandoned it on reconfig) so it
    /// doesn't linger — an MCP stdio proxy otherwise only exits on stdin-EOF.
    last_active: Arc<AtomicU64>,
}

fn now_epoch() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Append a timestamped line to the proxy's OWN log (`$RUNTIME/mcp-proxy.log`). The proxy's
/// only other trace is `eprintln!` captured into Claude Code's per-server MCP log — which
/// records NOTHING on a clean `exit(0)` (the idle reap) and only an opaque transport-close on a
/// crash. So without this an "MCP tools vanished" incident is invisible. Best-effort, lock-free
/// (an append is atomic enough for low-volume lifecycle lines); off with `ROZUM_MCP_PROXY_LOG=0`.
pub(crate) fn proxy_log(msg: &str) {
    if std::env::var_os("ROZUM_MCP_PROXY_LOG").is_some_and(|v| v == "0") {
        return;
    }
    use std::io::Write;
    let path = rozum_runtime_dir().join("mcp-proxy.log");
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Rotate once past ~256 KiB so the log can't grow unbounded across many sessions.
    if std::fs::metadata(&path).map(|m| m.len() > 256 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} pid={} {msg}", now_epoch(), std::process::id());
    }
}

/// Log a panic (payload + location) to the proxy log before the default hook runs — otherwise a
/// panic in the serve path dies with no rozum-side trace (Claude Code only sees the pipe close).
pub(crate) fn install_panic_logger() {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let loc = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic>".to_string());
        proxy_log(&format!("PANIC at {loc}: {msg}"));
        prev(info);
    }));
}

/// Soft idle window: after this long with no MCP traffic AND no room activity, the watchdog
/// considers reaping — but only actually reaps if the client transport is also gone (see
/// `spawn_idle_watchdog`). Default 2h; `0` disables the watchdog entirely.
fn idle_secs() -> u64 {
    std::env::var("ROZUM_MCP_PROXY_IDLE_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(7200)
}

/// Hard cap: reap unconditionally after this long idle even if the client is still connected, to
/// bound a truly-stuck orphan (agent abandoned us without closing the pipe). Default 24h; `0`
/// disables the hard cap so a live session is never reaped while its transport stays open.
fn max_idle_secs() -> u64 {
    std::env::var("ROZUM_MCP_PROXY_MAX_IDLE_SECS")
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(86400)
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

    /// Like `new()` but with the project pinned explicitly instead of detected from the process
    /// cwd. The HTTP transport (`http_proxy`) needs this: one long-lived daemon serves many
    /// clients, so the project must come from the request (URL), not the server's cwd. `None`
    /// falls back to cwd detection (single-project / dev use).
    pub fn for_project(project: Option<String>) -> Self {
        let project = project.or_else(detect_project);
        Self::build(meeting_sock(), project, "agent".to_string(), true)
    }

    fn build(sock: PathBuf, project: Option<String>, client: String, auto_spawn: bool) -> Self {
        let session_token = uuid::Uuid::new_v4().simple().to_string();
        let shared_room = std::env::var("ROZUM_MEETING_ROOM")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            state: Arc::new(Mutex::new(State {
                sock,
                project,
                shared_room,
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
                presence_announced: false,
            })),
            tool_router: Self::tool_router(),
            last_active: Arc::new(AtomicU64::new(now_epoch())),
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
        // A configured shared room (`ROZUM_MEETING_ROOM`) → `rooms.new` (create-or-open + join);
        // otherwise the project's canonical room via `_join_internal`. Both return
        // `{room, root, participant_id}`.
        let (tool, args) = match &s.shared_room {
            Some(name) => (
                "rooms.new",
                json!({
                    "name": name,
                    "client_info_name": s.client_info_name,
                    "session_token": s.session_token,
                }),
            ),
            None => {
                let mut a = json!({
                    "client_info_name": s.client_info_name,
                    "session_token": s.session_token,
                });
                if let Some(p) = &s.project {
                    a["project"] = json!(p);
                }
                ("_join_internal", a)
            }
        };
        let join = conn
            .call_tool(tool, args, CONNECT_TIMEOUT)
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
        proxy_log(&format!(
            "daemon-connect room={:?} reconnect={}",
            s.room_name, s.presence_announced
        ));
        // Announce presence ONCE (not on reconnects), via this same session so it carries the
        // agent's handle — unifies the join line with the agent's messages and works for every
        // agent (no per-client hooks). Best-effort.
        if !s.presence_announced {
            s.presence_announced = true;
            let content = format!("joined: {} is here", s.client_info_name);
            if let Some(conn) = s.conn.as_mut() {
                let _ = conn
                    .call_tool("meeting.submit", json!({ "content": content }), CONNECT_TIMEOUT)
                    .await;
            }
        }
        Ok(())
    }

    /// Best-effort `left:` presence line, posted after the agent's stdio session ends (the
    /// daemon connection usually outlives it for a moment). Mirrors the `joined:` line.
    async fn announce_left(&self) {
        let mut s = self.state.lock().await;
        if !s.presence_announced {
            return;
        }
        let content = format!("left: {} ended its session", s.client_info_name);
        if let Some(conn) = s.conn.as_mut() {
            let _ = conn
                .call_tool("meeting.submit", json!({ "content": content }), CONNECT_TIMEOUT)
                .await;
        }
    }

    /// Forward a tool call to the daemon, returning the daemon's raw result value.
    /// On transport error the connection is dropped (next call reconnects).
    async fn forward_raw(&self, tool: &str, params: Value) -> Result<Value, CallToolResult> {
        self.last_active.store(now_epoch(), Ordering::Relaxed);
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
        // Room activity keeps this proxy alive (see the bump below): an agent that
        // coordinates via `rozum meetings post` / the TUI rather than the MCP tools should
        // not lose its push channel to the idle watchdog while the room is live.
        let last_active = Arc::clone(&self.last_active);
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
                // Room activity (any new turn, incl. a `meetings post` from this agent's own
                // CLI) refreshes the idle watchdog, so a CLI-active agent keeps its push
                // channel. A truly-dead agent still exits on stdin-EOF; the watchdog still
                // reaps a proxy once BOTH its MCP traffic and the room go quiet for the window.
                last_active.store(now_epoch(), Ordering::Relaxed);
                // Advance past everything read (own entries included), so we neither re-read
                // nor re-push them on the next tick.
                since = Some((last.date.clone(), last.n + 1));
                // Does any new turn from SOMEONE ELSE address this agent's handle
                // (`@agent` / `-> agent`)? Then the wakeup is *for you*: flag the channel event
                // (`mentioned`/`your_turn`) and prefix the piggyback note so the agent prioritizes
                // it over ambient room chatter. See `docs/specs/meeting-mention-inbox.md`.
                let mentioned = turns.iter().any(|t| {
                    self_pid.as_deref() != Some(t.participant_id.as_str())
                        && super::mention::addresses(&t.content, &agent)
                });
                if let Some((content, from, seq)) = render_stored_delta(&turns, self_pid.as_deref())
                {
                    // Tier-3 piggyback fallback: also drop the delta where the launch-local HTTP
                    // proxy can inject it (clients with neither channels nor a wait loop). Auto-off
                    // when Tier-1 channels are active. Spec: `docs/specs/rozum-native-channels.md`.
                    if super::piggyback::enabled() {
                        let note =
                            if mentioned { format!("‹for you› {content}") } else { content.clone() };
                        super::piggyback::append(&super::piggyback::project_slug(), &agent, &note);
                    }
                    let meta = json!({
                        "room": room_name,
                        "from": from,
                        "seq": seq,
                        "your_turn": mentioned.to_string(),
                        "mentioned": mentioned,
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
            proxy_log(&format!("initialize client={:?} project={:?}", s.client_info_name, s.project));
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
    install_panic_logger();
    proxy_log(&format!(
        "start version={} idle_secs={} max_idle_secs={}",
        env!("CARGO_PKG_VERSION"),
        idle_secs(),
        max_idle_secs()
    ));
    let server = DaemonProxy::new();
    let presence = server.clone(); // shares the Arc<State>; serve() consumes `server`
    // Spawn the idle watchdog BEFORE serve(): serve() blocks until the MCP `initialize`
    // handshake, so a proxy abandoned before (or after) initialize must still be reaped.
    spawn_idle_watchdog(server.clone());
    let service = match server.serve(stdio()).await {
        Ok(s) => s,
        Err(e) => {
            // A serve() error here means the stdio handshake itself failed — there is no client to
            // keep serving, so exiting is correct; we just record WHY (previously silent).
            proxy_log(&format!("exit reason=serve-error err={e}"));
            return Err(e.into());
        }
    };
    // `waiting()` returns when the stdio transport ends. For a single-pipe stdio server that is
    // the genuine end of the session (the client closed it / went away) — there is no second
    // channel to recover on, so we exit. The win over the old code is that we now log the reason
    // and never surface a spurious non-zero exit for an ordinary EOF.
    match service.waiting().await {
        Ok(reason) => proxy_log(&format!("exit reason=stdin-eof quit={reason:?}")),
        Err(e) => proxy_log(&format!("exit reason=join-error err={e}")),
    }
    // The agent's stdio session ended — post a best-effort `left:` before exiting.
    presence.announce_left().await;
    Ok(())
}

/// Reap a proxy whose agent has gone silent. An MCP stdio proxy normally exits only on
/// stdin-EOF; if the agent *dies* it does (its pipe end closes), but if the agent stays alive
/// yet **abandons** this proxy — e.g. it re-spawned a fresh MCP server on a config reload and
/// never closed this one's stdin — `service.waiting()` blocks forever and the process lingers
/// (the orphaned `mpc-proxy` pile-up). An actively room-using agent calls `meeting.wait_my_turn`
/// every ~25s (each request stamps `last_active`), so silence past the threshold means it has
/// been abandoned.
///
/// BUG-FIX (mcp-proxy-resilience): the old watchdog reaped ANY proxy idle past the window with an
/// unconditional `exit(0)` — so an *interactive* Claude Code session whose human merely stepped
/// away for >2h lost all `mcp__rozum__*` tools mid-session (Claude Code does not re-spawn a dead
/// stdio server). Now, past the soft window we reap only if the **client transport is actually
/// gone** (`Peer::is_transport_closed()` — flips when the rmcp loop tears down, i.e. Claude Code
/// disconnected). A live-but-idle session keeps its transport open → it is NOT reaped. A truly
/// stuck orphan (agent abandoned us without closing the pipe, so the transport never closes) is
/// still bounded by the generous hard cap. `ROZUM_MCP_PROXY_IDLE_SECS=0` disables the watchdog.
fn spawn_idle_watchdog(proxy: DaemonProxy) {
    let secs = idle_secs();
    if secs == 0 {
        return;
    }
    let hard = max_idle_secs();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(60));
        loop {
            tick.tick().await;
            let idle = now_epoch().saturating_sub(proxy.last_active.load(Ordering::Relaxed));
            if idle < secs {
                continue;
            }
            // Past the soft window. Reap only if the client is genuinely gone — a live but idle
            // interactive session (human away) keeps its transport open and must NOT lose tools.
            let peer_gone = {
                let s = proxy.state.lock().await;
                match &s.upstream_peer {
                    Some(p) => p.is_transport_closed(),
                    None => true, // never initialized → abandoned before the handshake
                }
            };
            if peer_gone {
                proxy_log(&format!("exit reason=idle-reap idle={idle}s peer=gone"));
                std::process::exit(0);
            }
            // Client still connected: keep serving. Only the hard cap can reap it now, to bound a
            // stuck orphan whose pipe never closed.
            if hard != 0 && idle >= hard {
                proxy_log(&format!("exit reason=idle-reap-hardcap idle={idle}s peer=alive"));
                std::process::exit(0);
            }
        }
    });
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

/// Resolve a binary that actually has the `meetings` subcommand. The daemon lives in the ENGINE
/// binary `rozum-gateway` (or behind the `rozum` dispatcher, which forwards `meetings` to it) — NOT
/// in this thin bridge bin: `rozum-meet` only has `mcp-proxy`/`mcp-http`, so the old
/// `current_exe meetings start` spawned `rozum-meet meetings start` → "unrecognized subcommand
/// 'meetings'" and the bridge could never self-heal a dropped daemon. Prefer `current_exe` when it
/// already is meetings-capable, else a sibling `rozum-gateway`/`rozum` (covers a target/release run
/// finding its just-built siblings), else bare `rozum-gateway` so the OS searches `PATH`.
fn meetings_binary() -> PathBuf {
    let capable = |p: &std::path::Path| {
        matches!(
            p.file_name().and_then(|n| n.to_str()),
            Some("rozum-gateway") | Some("rozum")
        )
    };
    if let Ok(exe) = std::env::current_exe() {
        if capable(&exe) {
            return exe;
        }
        if let Some(dir) = exe.parent() {
            for name in ["rozum-gateway", "rozum"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return cand;
                }
            }
        }
    }
    PathBuf::from("rozum-gateway")
}

const MESSENGER_BRIDGE_ENV_VARS: &[&str] = &[
    "TELEGRAM_BOT_TOKEN",
    "TELEGRAM_CHAT_ID",
    "TELEGRAM_ALLOWED_USER_IDS",
    "DISCORD_BOT_TOKEN",
    "DISCORD_CHANNEL_ID",
    "DISCORD_ALLOWED_USER_IDS",
];

fn is_messenger_bridge_env_key(key: &std::ffi::OsStr) -> bool {
    key.to_str().is_some_and(|key| {
        key.starts_with("TELEGRAM_")
            || key.starts_with("DISCORD_")
            || key.starts_with("ROZUM_TELEGRAM_")
            || key.starts_with("ROZUM_DISCORD_")
    })
}

/// Prevent a shared, long-lived meeting daemon from retaining bridge credentials.
///
/// This mutates only a child command's environment; the Telegram or Discord bridge process that
/// requested the daemon keeps its own configuration. The explicit names keep today's contract
/// covered even when a variable is absent from the parent, while the prefix pass also catches
/// future messenger-specific settings.
pub fn scrub_messenger_bridge_env(command: &mut std::process::Command) {
    let mut keys: Vec<std::ffi::OsString> = MESSENGER_BRIDGE_ENV_VARS
        .iter()
        .map(std::ffi::OsString::from)
        .collect();
    keys.extend(
        std::env::vars_os()
            .map(|(key, _)| key)
            .filter(|key| is_messenger_bridge_env_key(key)),
    );
    keys.extend(
        command
            .get_envs()
            .map(|(key, _)| key.to_os_string())
            .filter(|key| is_messenger_bridge_env_key(key)),
    );
    for key in keys {
        command.env_remove(key);
    }
}

/// The launchd job that OWNS this daemon where one is installed.
pub const DAEMON_JOB: &str = "com.rozum.meeting-daemon";

/// Bring the daemon up — by asking its OWNER first, and only spawning our own where there is none.
///
/// **The ownership question this answers** (`docs/specs/meeting-daemon-ownership.md`). Every client
/// used to start its own detached daemon; whoever won the `flock` beside the socket then served
/// `:8401` and the MCP socket for everyone, and launchd's job — the copy with `KeepAlive`, the one
/// thing that would restart the service at 4am — sat there owning nothing. It worked, which is why
/// it survived: the service ran, and the guarantee behind it did not.
///
/// So on a machine where the job exists, ask launchd; the daemon that results is the one launchd
/// can restart. Where it does not exist — another checkout, a CI box, a second machine — spawn our
/// own exactly as before, because "works anywhere" is the property that made this convenient and
/// removing it would trade one failure for another.
pub async fn spawn_daemon() {
    if launchd_job_exists(DAEMON_JOB).await && kickstart_and_wait(DAEMON_JOB).await {
        return;
    }
    // `meetings start` spawns the detached daemon and waits for its socket.
    let mut command = tokio::process::Command::new(meetings_binary());
    command.args(["meetings", "start"]);
    scrub_messenger_bridge_env(command.as_std_mut());
    let _ = command.status().await;
}

/// Is this label installed on this machine at all? `launchctl print` answers for a job that is
/// loaded whether or not it is currently running, which is the question here — a periodic or
/// crashed job is still the owner.
pub async fn launchd_job_exists(label: &str) -> bool {
    let Some(uid) = current_uid() else { return false };
    tokio::process::Command::new("launchctl")
        .args(["print", &format!("gui/{uid}/{label}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Ask launchd to run the job, then wait for the socket it exists to serve.
///
/// `kickstart` WITHOUT `-k`: a job already running must not be restarted just because a client
/// wanted to talk to it. Returns false if the socket never appears, so the caller can fall back
/// rather than leave the client with nothing.
async fn kickstart_and_wait(label: &str) -> bool {
    let Some(uid) = current_uid() else { return false };
    let _ = tokio::process::Command::new("launchctl")
        .args(["kickstart", &format!("gui/{uid}/{label}")])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await;
    let sock = crate::meeting::room_path::meeting_sock();
    for _ in 0..40 {
        if crate::meeting::daemon::daemon_alive(&sock).await {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    false
}

fn current_uid() -> Option<u32> {
    // `id -u` rather than a libc call: this crate has no libc dependency and the answer is stable.
    std::process::Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
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

        // wait_my_turn (no args) returns the messages via the tracked cursor: the proxy's
        // auto-posted `joined:` presence line, then the submitted message.
        let wait = proxy.wait_my_turn().await;
        let payload = tool_result_json(&wait);
        assert_eq!(payload["still_waiting"], false);
        let turns = payload["turns"].as_array().unwrap();
        let contents: Vec<&str> = turns.iter().filter_map(|t| t["content"].as_str()).collect();
        assert!(
            contents.iter().any(|c| c.starts_with("joined:")),
            "proxy posts a joined: presence line on first join: {contents:?}"
        );
        assert!(
            contents.contains(&"hello via proxy"),
            "the submitted message is present: {contents:?}"
        );

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
            ..Default::default()
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

    #[test]
    fn meetings_binary_never_resolves_to_a_non_meetings_bin() {
        // Regression: the bridge used `current_exe meetings start`, but the thin `rozum-meet` bin has
        // no `meetings` subcommand → self-heal broke. Whatever the current_exe (here the test runner),
        // the resolver must yield a meetings-capable NAME (rozum-gateway / rozum), never the thin bin.
        let bin = meetings_binary();
        let name = bin.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            name == "rozum-gateway" || name == "rozum",
            "must resolve to a meetings-capable binary, got {name:?}"
        );
    }

    #[test]
    fn daemon_child_env_excludes_messenger_configuration() {
        let mut command = std::process::Command::new("unused-test-command");
        command
            .env("TELEGRAM_BOT_TOKEN", "telegram-secret")
            .env("DISCORD_CHANNEL_ID", "123")
            .env("ROZUM_TELEGRAM_FUTURE_SETTING", "future-secret")
            .env("UNRELATED_SETTING", "preserved");

        scrub_messenger_bridge_env(&mut command);

        let envs: std::collections::HashMap<_, _> = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(std::ffi::OsStr::to_os_string)))
            .collect();
        for key in MESSENGER_BRIDGE_ENV_VARS {
            assert_eq!(
                envs.get(std::ffi::OsStr::new(key)),
                Some(&None),
                "{key} must be removed from the daemon child"
            );
        }
        assert_eq!(
            envs.get(std::ffi::OsStr::new("ROZUM_TELEGRAM_FUTURE_SETTING")),
            Some(&None)
        );
        assert_eq!(
            envs.get(std::ffi::OsStr::new("UNRELATED_SETTING")),
            Some(&Some(std::ffi::OsString::from("preserved")))
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
