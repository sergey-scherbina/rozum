//! The `rozum meetings` daemon: one rmcp server on `meeting.sock` driving the
//! [`RoomRegistry`]. A session selects a room (`_join_internal` for the caller's
//! project room, or `rooms.join` by name), then `meeting.*` operate on it.
//!
//! `wait_my_turn` returns **coordination only** (a high-water `(date, n)`) —
//! message content never transits the daemon socket; local clients read it from
//! disk themselves. See `docs/specs/agent-meetings-daemon.md`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::{
    ErrorData, Peer, RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, Content, Implementation, InitializeRequestParams, InitializeResult,
        ServerCapabilities,
    },
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::net::UnixListener;
use tokio::sync::{Mutex, watch};

use super::identity::Roster;
use super::participant::ParticipantId;
use super::registry::{RoomHandle, RoomRegistry};
use super::room::Phase;
use super::store::{self, RoomPaths};

// ── Tool params ───────────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct JoinInternalParams {
    pub client_info_name: String,
    /// The caller's project (cwd / git root); selects its canonical room.
    #[serde(default)]
    pub project: Option<String>,
    /// Session token held by the proxy for its lifetime — the reconnect key.
    #[serde(default)]
    pub session_token: Option<String>,
    /// `"mcp"` (default) or `"bridge"`.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoomsJoinParams {
    pub name: String,
    /// Identity for clients that join by name without a prior `_join_internal`
    /// (e.g. the human TUI in picker mode).
    #[serde(default)]
    pub client_info_name: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct RoomsNewParams {
    /// Room name; default is a generated adjective-noun.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub topic: Option<String>,
    #[serde(default)]
    pub client_info_name: Option<String>,
    #[serde(default)]
    pub session_token: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    pub content: String,
    /// Optional message kind: note|question|event|alert|resolution (default note). Support metadata —
    /// all optional + back-compat (an old client sends only `content`).
    #[serde(default)]
    pub kind: Option<String>,
    /// Optional thread id to post into (an incident/topic thread).
    #[serde(default)]
    pub thread_id: Option<String>,
    /// Optional severity: info|low|medium|high|critical.
    #[serde(default)]
    pub severity: Option<String>,
    /// Optional tags.
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadOpenParams {
    /// Message id (`<date>/<n>`) the thread is anchored on — the message that starts it.
    pub anchor_id: String,
    /// Human title for the thread/incident.
    pub title: String,
    /// `topic` (default) or `incident`.
    #[serde(default)]
    pub kind: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadStateParams {
    /// The thread id.
    pub id: String,
    /// New state: open|triaging|escalated|resolved|closed.
    pub state: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct EscalateParams {
    /// The thread id.
    pub id: String,
    /// Who/what to escalate to (an agent handle, on-call, a tier). Becomes the thread assignee.
    #[serde(default)]
    pub to: Option<String>,
    /// Optional escalation note.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct ResolveParams {
    /// The thread id.
    pub id: String,
    /// The resolution note.
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct ThreadContextParams {
    /// The thread id to gather context for.
    pub thread_id: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct WaitParams {
    /// Day of the last message seen (`YYYY-MM-DD`). Omit to receive all.
    #[serde(default)]
    pub since_date: Option<String>,
    /// Per-day index of the last message seen in `since_date`.
    #[serde(default)]
    pub since_n: Option<u64>,
}

// ── Per-connection session ─────────────────────────────────────────────────────

#[derive(Default)]
struct Session {
    room: Option<RoomHandle>,
    room_name: Option<String>,
    participant_id: Option<ParticipantId>,
    session_token: Option<String>,
    project: Option<String>,
    client_name: String,
}

pub struct MeetingServer {
    registry: Arc<RoomRegistry>,
    session: Arc<Mutex<Session>>,
    peer_slot: Arc<Mutex<Option<Peer<RoleServer>>>>,
    tool_router: ToolRouter<Self>,
    /// Flips to `true` when the daemon is draining; long-polls return `{ended}`.
    shutdown: watch::Receiver<bool>,
    /// Keeps a self-owned shutdown channel alive (so `shutdown.changed()` never
    /// errors) when the server isn't driven by `serve_daemon`.
    _shutdown_keep: Option<watch::Sender<bool>>,
}

impl MeetingServer {
    pub fn new(registry: Arc<RoomRegistry>) -> Self {
        let (tx, rx) = watch::channel(false);
        Self::with_shutdown(registry, rx, Some(tx))
    }

    fn with_shutdown(
        registry: Arc<RoomRegistry>,
        shutdown: watch::Receiver<bool>,
        keep: Option<watch::Sender<bool>>,
    ) -> Self {
        Self {
            registry,
            session: Arc::new(Mutex::new(Session::default())),
            peer_slot: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
            shutdown,
            _shutdown_keep: keep,
        }
    }

    /// Bind `room` to this session and join the participant (mint or rebind).
    async fn enter_room(
        &self,
        room: RoomHandle,
        room_name: String,
        client_name: &str,
        kind: &str,
        session_token: Option<&str>,
        project: Option<&str>,
    ) -> (ParticipantId, String) {
        let (id, handle) = {
            let mut r = room.lock().await;
            r.join(session_token, client_name, kind, project)
        };
        let mut s = self.session.lock().await;
        s.room = Some(room);
        s.room_name = Some(room_name);
        s.participant_id = Some(id.clone());
        s.session_token = session_token.map(str::to_owned);
        s.project = project.map(str::to_owned);
        s.client_name = client_name.to_owned();
        (id, handle)
    }
}

#[tool_router(router = tool_router)]
impl MeetingServer {
    #[tool(
        name = "rooms.list",
        description = "List meeting rooms known to the daemon."
    )]
    pub async fn rooms_list(&self) -> CallToolResult {
        guard("rooms.list", async move {
            let rooms: Vec<_> = self
                .registry
                .list()
                .into_iter()
                .map(|l| {
                    let topic = store::read_meta(&l.root)
                        .map(|m| m.topic)
                        .unwrap_or_default();
                    let participants = Roster::load(&RoomPaths::raw(l.root.clone()).roster_path())
                        .participants
                        .len();
                    let last_date = store::day_dates(&l.root).last().cloned();
                    serde_json::json!({
                        "name": l.name,
                        "project": l.project,
                        "root": l.root,
                        "topic": topic,
                        "participants": participants,
                        "last_date": last_date,
                    })
                })
                .collect();
            text_result(&serde_json::json!({ "rooms": rooms }).to_string())
        })
        .await
    }

    #[tool(
        name = "rooms.join",
        description = "Join a room by name (switches the session's room)."
    )]
    pub async fn rooms_join(&self, params: Parameters<RoomsJoinParams>) -> CallToolResult {
        guard("rooms.join", async move {
        let p = params.0;
        let name = p.name;
        let room = match self.registry.get_by_name(&name) {
            Ok(Some(r)) => r,
            Ok(None) => return err_result(&format!("no such room: {name}")),
            Err(e) => return err_result(&format!("open error: {e}")),
        };
        // Prefer identity supplied with the call (picker flow), else what a prior
        // `_join_internal` recorded on this session.
        let (sess_name, sess_token) = {
            let s = self.session.lock().await;
            (s.client_name.clone(), s.session_token.clone())
        };
        let client_name = p.client_info_name.unwrap_or(sess_name);
        let token = p.session_token.or(sess_token);
        let kind = match p.kind.as_deref() {
            Some("bridge") => "bridge",
            Some("human") => "human",
            _ => "mcp",
        };
        let root = room.lock().await.root().to_path_buf();
        let (id, handle) = self
            .enter_room(
                room,
                name.clone(),
                &client_name,
                kind,
                token.as_deref(),
                None,
            )
            .await;
        register_peer(&self.peer_slot, &self.session, &id).await;
        text_result(
            &serde_json::json!({ "room": name, "participant_id": id.0, "handle": handle, "root": root })
                .to_string(),
        )
        })
        .await
    }

    #[tool(
        name = "rooms.new",
        description = "Create + join a new ad-hoc room (not tied to a project). Returns room + root."
    )]
    pub async fn rooms_new(&self, params: Parameters<RoomsNewParams>) -> CallToolResult {
        guard("rooms.new", async move {
            let p = params.0;
            let name = p
                .name
                .unwrap_or_else(super::room_path::generate_room_name);
            let paths = RoomPaths::ad_hoc_in(self.registry.state_dir(), &name);
            let root = paths.root.clone();
            let topic = p.topic.unwrap_or_default();
            let room = match self.registry.get_or_create(paths, &name, &topic, None) {
                Ok(r) => r,
                Err(e) => return err_result(&format!("open error: {e}")),
            };
            let (sess_name, sess_token) = {
                let s = self.session.lock().await;
                (s.client_name.clone(), s.session_token.clone())
            };
            let client_name = p.client_info_name.unwrap_or(sess_name);
            let token = p.session_token.or(sess_token);
            let (id, handle) = self
                .enter_room(room, name.clone(), &client_name, "human", token.as_deref(), None)
                .await;
            register_peer(&self.peer_slot, &self.session, &id).await;
            text_result(
                &serde_json::json!({ "room": name, "root": root, "participant_id": id.0, "handle": handle })
                    .to_string(),
            )
        })
        .await
    }

    #[tool(
        name = "_join_internal",
        description = "Internal: register in your project's room. Returns participant_id + handle."
    )]
    pub async fn join_internal(&self, params: Parameters<JoinInternalParams>) -> CallToolResult {
        guard("_join_internal", async move {
            let p = params.0;
            let Some(project) = p.project else {
                return err_result("no project: call rooms.join(name) to pick a room");
            };
            let name = project_room_name(&project);
            let paths = RoomPaths::for_project(Path::new(&project));
            let root = paths.root.clone();
            let room =
                match self
                    .registry
                    .get_or_create(paths, &name, "", Some(PathBuf::from(&project)))
                {
                    Ok(r) => r,
                    Err(e) => return err_result(&format!("open error: {e}")),
                };
            let kind = match p.kind.as_deref() {
                Some("bridge") => "bridge",
                Some("human") => "human",
                _ => "mcp",
            };
            let (id, handle) = self
                .enter_room(
                    room,
                    name.clone(),
                    &p.client_info_name,
                    kind,
                    p.session_token.as_deref(),
                    Some(&project),
                )
                .await;
            register_peer(&self.peer_slot, &self.session, &id).await;
            text_result(
                &serde_json::json!({
                    "participant_id": id.0,
                    "handle": handle,
                    "room": name,
                    "root": root,
                })
                .to_string(),
            )
        })
        .await
    }

    #[tool(
        name = "meeting.submit",
        description = "Submit a message. Anyone can submit at any time."
    )]
    pub async fn submit(&self, params: Parameters<SubmitParams>) -> CallToolResult {
        guard("meeting.submit", async move {
            let (room, id) = self.session_room().await;
            let (Some(room), Some(id)) = (room, id) else {
                return err_result("not-joined: call _join_internal first");
            };
            let p = &params.0;
            let pm = store::PostMeta {
                kind: p.kind.as_deref().and_then(store::MsgKind::parse).unwrap_or_default(),
                thread_id: p.thread_id.clone(),
                meta: store::MsgMeta {
                    severity: p.severity.as_deref().and_then(store::Severity::parse),
                    tags: p.tags.clone(),
                    ..Default::default()
                },
                ..Default::default()
            };
            let res = room.lock().await.submit_with_meta(&id, &p.content, pm);
            match res {
                Ok(turn) => text_result(
                    &serde_json::json!({ "date": turn.date, "n": turn.n, "id": turn.id() }).to_string(),
                ),
                Err(e) => err_result(&e),
            }
        })
        .await
    }

    /// Open (or get) a thread/incident anchored on a message id. Messages join it by posting with
    /// `thread_id = <this id>`.
    #[tool(
        name = "meeting.thread_open",
        description = "Open (or get) a thread/incident anchored on a message id (anchor_id). kind: topic|incident."
    )]
    pub async fn thread_open(&self, params: Parameters<ThreadOpenParams>) -> CallToolResult {
        guard("meeting.thread_open", async move {
            let (room, _id) = self.session_room().await;
            let Some(room) = room else {
                return err_result("not-joined: call _join_internal first");
            };
            let p = &params.0;
            let kind = p.kind.as_deref().and_then(store::ThreadKind::parse).unwrap_or_default();
            match room.lock().await.open_thread(&p.anchor_id, &p.title, kind) {
                Ok(t) => text_result(&serde_json::to_string(&t).unwrap_or_default()),
                Err(e) => err_result(&e),
            }
        })
        .await
    }

    /// Move a thread/incident through the resolving state machine.
    #[tool(
        name = "meeting.thread_set_state",
        description = "Set a thread/incident state: open|triaging|escalated|resolved|closed."
    )]
    pub async fn thread_set_state(&self, params: Parameters<ThreadStateParams>) -> CallToolResult {
        guard("meeting.thread_set_state", async move {
            let (room, _id) = self.session_room().await;
            let Some(room) = room else {
                return err_result("not-joined: call _join_internal first");
            };
            let p = &params.0;
            let Some(state) = store::ThreadState::parse(&p.state) else {
                return err_result("bad state (open|triaging|escalated|resolved|closed)");
            };
            match room.lock().await.set_thread_state(&p.id, state) {
                Ok(Some(t)) => text_result(&serde_json::to_string(&t).unwrap_or_default()),
                Ok(None) => err_result("unknown thread id"),
                Err(e) => err_result(&e),
            }
        })
        .await
    }

    /// List the room's threads/incidents (metadata; membership is derived from messages).
    #[tool(name = "meeting.threads", description = "List the room's threads/incidents.")]
    pub async fn threads(&self) -> CallToolResult {
        guard("meeting.threads", async move {
            let (room, _id) = self.session_room().await;
            let Some(room) = room else {
                return err_result("not-joined: call _join_internal first");
            };
            let ts = room.lock().await.threads();
            text_result(&serde_json::to_string(&ts).unwrap_or_default())
        })
        .await
    }

    /// Escalate a thread/incident: state→escalated, set the assignee, post an escalation note.
    #[tool(
        name = "meeting.escalate",
        description = "Escalate a thread/incident: state→escalated, set assignee (to), post a note."
    )]
    pub async fn escalate(&self, params: Parameters<EscalateParams>) -> CallToolResult {
        guard("meeting.escalate", async move {
            let (room, caller) = self.session_room().await;
            let (Some(room), Some(caller)) = (room, caller) else {
                return err_result("not-joined: call _join_internal first");
            };
            let p = &params.0;
            let mut r = room.lock().await;
            match r.set_thread_state(&p.id, store::ThreadState::Escalated) {
                Ok(None) => return err_result("unknown thread id"),
                Err(e) => return err_result(&e),
                Ok(Some(_)) => {}
            }
            if let Some(to) = &p.to {
                let _ = r.set_thread_owner(&p.id, Some(to.clone()), None);
            }
            let to = p.to.clone().unwrap_or_else(|| "on-call".into());
            let note = p.note.clone().unwrap_or_default();
            let content = if note.is_empty() {
                format!("escalated to {to}")
            } else {
                format!("escalated to {to}: {note}")
            };
            let pm = store::PostMeta {
                kind: store::MsgKind::Event,
                thread_id: Some(p.id.clone()),
                ..Default::default()
            };
            match r.submit_with_meta(&caller, &content, pm) {
                Ok(turn) => text_result(
                    &serde_json::json!({ "thread": p.id, "state": "escalated", "to": to, "msg_id": turn.id() })
                        .to_string(),
                ),
                Err(e) => err_result(&e),
            }
        })
        .await
    }

    /// Resolve a thread/incident: state→resolved, post a resolution note.
    #[tool(
        name = "meeting.resolve",
        description = "Resolve a thread/incident: state→resolved, post a resolution note."
    )]
    pub async fn resolve(&self, params: Parameters<ResolveParams>) -> CallToolResult {
        guard("meeting.resolve", async move {
            let (room, caller) = self.session_room().await;
            let (Some(room), Some(caller)) = (room, caller) else {
                return err_result("not-joined: call _join_internal first");
            };
            let p = &params.0;
            let mut r = room.lock().await;
            match r.set_thread_state(&p.id, store::ThreadState::Resolved) {
                Ok(None) => return err_result("unknown thread id"),
                Err(e) => return err_result(&e),
                Ok(Some(_)) => {}
            }
            let note = p.note.clone().unwrap_or_else(|| "resolved".into());
            let pm = store::PostMeta {
                kind: store::MsgKind::Resolution,
                thread_id: Some(p.id.clone()),
                ..Default::default()
            };
            match r.submit_with_meta(&caller, &note, pm) {
                Ok(turn) => text_result(
                    &serde_json::json!({ "thread": p.id, "state": "resolved", "msg_id": turn.id() })
                        .to_string(),
                ),
                Err(e) => err_result(&e),
            }
        })
        .await
    }

    /// Support metrics for the room's threads: counts by state + average time-to-resolve.
    #[tool(
        name = "meeting.thread_metrics",
        description = "Room thread/incident metrics: counts by state + average time-to-resolve (secs)."
    )]
    pub async fn thread_metrics(&self) -> CallToolResult {
        guard("meeting.thread_metrics", async move {
            let (room, _id) = self.session_room().await;
            let Some(room) = room else {
                return err_result("not-joined: call _join_internal first");
            };
            let threads = room.lock().await.threads();
            let total = threads.len();
            let mut by_state: std::collections::BTreeMap<&'static str, usize> = Default::default();
            let mut ttr_sum = 0u64;
            let mut terminal = 0u64;
            for t in &threads {
                *by_state.entry(t.state.as_str()).or_default() += 1;
                if t.state.is_terminal() {
                    ttr_sum += t.updated_ts.saturating_sub(t.created_ts);
                    terminal += 1;
                }
            }
            let avg_ttr = if terminal > 0 { ttr_sum / terminal } else { 0 };
            text_result(
                &serde_json::json!({
                    "total": total, "by_state": by_state,
                    "resolved": terminal, "avg_time_to_resolve_secs": avg_ttr,
                })
                .to_string(),
            )
        })
        .await
    }

    /// Incident-context gather — the whole incident in one bundle (thread + its messages + participants
    /// + timespan), so an agent or human picking it up has the full picture. The highest-leverage verb.
    #[tool(
        name = "meeting.thread_context",
        description = "Gather an incident's full context: the thread + all its messages + participants + timespan."
    )]
    pub async fn thread_context(&self, params: Parameters<ThreadContextParams>) -> CallToolResult {
        guard("meeting.thread_context", async move {
            let (room, _id) = self.session_room().await;
            let Some(room) = room else {
                return err_result("not-joined: call _join_internal first");
            };
            let bundle = room.lock().await.thread_context(&params.0.thread_id);
            text_result(&bundle.to_string())
        })
        .await
    }

    #[tool(
        name = "meeting.wait_my_turn",
        description = "Long-poll (25s) for new messages since (since_date, since_n). Returns a transcript delta."
    )]
    pub async fn wait_my_turn(&self, params: Parameters<WaitParams>) -> CallToolResult {
        guard("meeting.wait_my_turn", async move {
            let (room, my_id) = self.session_room().await;
            let (Some(room), Some(my_id)) = (room, my_id) else {
                return err_result("not-joined: call _join_internal first");
            };
            let since_date = params.0.since_date;
            let since_n = params.0.since_n.unwrap_or(0);

            let notify = {
                let mut r = room.lock().await;
                r.mark_polling(&my_id);
                r.notify.clone()
            };
            let _guard = PollGuard {
                room: room.clone(),
                id: my_id.clone(),
            };
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(25);
            let mut shutdown = self.shutdown.clone();

            loop {
                if *shutdown.borrow() {
                    return text_result("{\"ended\":true,\"reason\":\"server-shutdown\"}");
                }
                let notified = notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();

                {
                    let r = room.lock().await;
                    if r.phase() == Phase::Ended {
                        return text_result("{\"ended\":true,\"reason\":\"meeting-ended\"}");
                    }
                    // Coordination only — compare high-water to the cursor; the
                    // client reads the content itself from disk (no bytes transit).
                    let hw = r.high_water();
                    if has_new(&hw, since_date.as_deref(), since_n) {
                        return text_result(
                            &serde_json::json!({
                                "still_waiting": false,
                                "high_water": high_water_json(&hw),
                                "responding": responding_ids(&r),
                            })
                            .to_string(),
                        );
                    }
                }

                if tokio::time::Instant::now() >= deadline {
                    let r = room.lock().await;
                    return text_result(
                        &serde_json::json!({
                            "still_waiting": true,
                            "high_water": high_water_json(&r.high_water()),
                            "responding": responding_ids(&r),
                        })
                        .to_string(),
                    );
                }
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                tokio::select! {
                    _ = notified.as_mut() => {}
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(remaining) => {}
                }
            }
        })
        .await
    }

    #[tool(
        name = "meeting.mark_responding",
        description = "Signal you are composing a response."
    )]
    pub async fn mark_responding(&self) -> CallToolResult {
        guard("meeting.mark_responding", async move {
            let (room, id) = self.session_room().await;
            let (Some(room), Some(id)) = (room, id) else {
                return err_result("not-joined: call _join_internal first");
            };
            room.lock().await.mark_responding(&id);
            text_result("{\"ok\":true}")
        })
        .await
    }

    #[tool(
        name = "meeting.status",
        description = "Current room status: name, phase, participants, high-water."
    )]
    pub async fn status(&self) -> CallToolResult {
        guard("meeting.status", async move {
            let (room, _) = self.session_room().await;
            let Some(room) = room else {
                return err_result("not-joined: call _join_internal first");
            };
            let r = room.lock().await;
            let hw = r.high_water();
            let participants: Vec<_> = r
                .roster()
                .participants
                .iter()
                .map(|e| {
                    serde_json::json!({
                        "id": e.id, "handle": e.handle, "base_name": e.base_name, "kind": e.kind,
                    })
                })
                .collect();
            text_result(
                &serde_json::json!({
                    "name": r.name(),
                    "topic": r.topic(),
                    "phase": format!("{:?}", r.phase()),
                    "high_water": high_water_json(&hw),
                    "budget_chars": r.budget_chars(),
                    "participants": participants,
                    "responding": responding_ids(&r),
                })
                .to_string(),
            )
        })
        .await
    }

    #[tool(name = "meeting.leave", description = "Leave the current room.")]
    pub async fn leave(&self) -> CallToolResult {
        guard("meeting.leave", async move {
            let mut s = self.session.lock().await;
            if let (Some(room), Some(id)) = (s.room.clone(), s.participant_id.take()) {
                room.lock().await.leave(&id);
            }
            s.room = None;
            s.room_name = None;
            text_result("{\"ok\":true}")
        })
        .await
    }
}

impl MeetingServer {
    async fn session_room(&self) -> (Option<RoomHandle>, Option<ParticipantId>) {
        let s = self.session.lock().await;
        (s.room.clone(), s.participant_id.clone())
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for MeetingServer {
    async fn initialize(
        &self,
        _params: InitializeRequestParams,
        context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        *self.peer_slot.lock().await = Some(context.peer.clone());
        Ok(
            InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
                .with_server_info(Implementation::new(
                    "rozum-meetings",
                    env!("CARGO_PKG_VERSION"),
                ))
                .with_instructions(
                    "rozum meeting daemon. Call _join_internal{client_info_name, project, \
                     session_token} to enter your project's room (or rooms.join{name}). \
                     Loop meeting.wait_my_turn → meeting.submit. Free-form: anyone may submit \
                     any time.",
                ),
        )
    }
}

/// Serve the daemon on `socket_path` until SIGINT/SIGTERM. On shutdown, pending
/// `wait_my_turn` long-polls return `{ended:"server-shutdown"}` and the socket is
/// removed after a short drain.
pub async fn serve_daemon(socket_path: &Path, registry: Arc<RoomRegistry>) -> std::io::Result<()> {
    let (tx, rx) = watch::channel(false);
    // Translate OS signals into the shutdown flag, then keep `tx` alive until one
    // arrives (so subscribers' `changed()` never errors prematurely).
    tokio::spawn(async move {
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = async {
                match term.as_mut() {
                    Some(t) => { t.recv().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {}
        }
        let _ = tx.send(true);
    });
    serve_daemon_until(socket_path, registry, rx).await
}

/// The accept loop, parameterized on a `shutdown` flag (so it is driveable from
/// tests without OS signals).
async fn serve_daemon_until(
    socket_path: &Path,
    registry: Arc<RoomRegistry>,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    tracing::info!("meeting daemon listening on {}", socket_path.display());
    super::rest_read::maybe_spawn_from_env(Arc::clone(&registry), shutdown.clone());

    // Idle-evict watchdog: sweep long-idle rooms out of the open set (files stay,
    // they reopen on demand). `ROZUM_MEETINGS_IDLE_SECS=0` disables it.
    let idle_secs = std::env::var("ROZUM_MEETINGS_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(1800);
    if idle_secs > 0 {
        let reg = Arc::clone(&registry);
        let mut sh = shutdown.clone();
        let interval = idle_secs.clamp(30, 300);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(interval)) => {}
                    _ = sh.changed() => break,
                }
                if *sh.borrow() {
                    break;
                }
                let n = reg.evict_idle(super::state::unix_ts(), idle_secs).await;
                if n > 0 {
                    tracing::debug!(evicted = n, "idle-evicted rooms");
                }
            }
        });
    }

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _) = accept?;
                serve_conn(stream, Arc::clone(&registry), shutdown.clone());
            }
            _ = shutdown.changed() => break,
        }
        if *shutdown.borrow() {
            break;
        }
    }
    // Give in-flight long-polls a moment to observe the flag and return `{ended}`.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = std::fs::remove_file(socket_path);
    Ok(())
}

fn serve_conn(
    stream: tokio::net::UnixStream,
    registry: Arc<RoomRegistry>,
    shutdown: watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let server = MeetingServer::with_shutdown(registry, shutdown, None);
        let session = Arc::clone(&server.session);
        if let Ok(service) = server.serve(stream).await {
            let _ = service.waiting().await;
        }
        // Connection dropped → leave the room (roster record stays on disk).
        let s = session.lock().await;
        if let (Some(room), Some(id)) = (s.room.clone(), s.participant_id.clone()) {
            room.lock().await.leave(&id);
        }
    });
}

// ── Helpers ────────────────────────────────────────────────────────────────────

fn project_room_name(project: &str) -> String {
    Path::new(project)
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("room")
        .to_string()
}

fn text_result(content: &str) -> CallToolResult {
    CallToolResult::success(vec![Content::text(content)])
}
fn err_result(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

/// Run a room handler with **panic isolation**: a panic is caught and returned
/// as a tool error, so it cannot abort the connection, other rooms, or the
/// daemon. (tokio already isolates a panicked task; this also keeps the agent's
/// connection alive instead of dropping it.)
async fn guard<F>(tool: &str, fut: F) -> CallToolResult
where
    F: std::future::Future<Output = CallToolResult>,
{
    use futures::FutureExt;
    match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(_) => {
            tracing::error!(tool, "room handler panicked — isolated");
            err_result("internal-error: the operation panicked and was isolated")
        }
    }
}

fn high_water_json(hw: &super::store::HighWater) -> serde_json::Value {
    serde_json::json!({ "date": hw.date, "n": hw.n, "end_offset": hw.end_offset })
}

/// Is there a message at/after the cursor `(since_date, since_n)`? Cheap compare
/// against the high-water — no content read. `hw.n` is the active day's message
/// count; days are append-ordered, so a newer high-water date implies new data.
fn has_new(hw: &super::store::HighWater, since_date: Option<&str>, since_n: u64) -> bool {
    match since_date {
        None => hw.n > 0,
        Some(sd) => hw.date.as_str() > sd || (hw.date.as_str() == sd && hw.n > since_n),
    }
}

fn responding_ids(r: &super::room::DaemonRoom) -> Vec<String> {
    r.active_responding().into_iter().map(|id| id.0).collect()
}

async fn register_peer(
    peer_slot: &Arc<Mutex<Option<Peer<RoleServer>>>>,
    _session: &Arc<Mutex<Session>>,
    _id: &ParticipantId,
) {
    // The peer is recorded for potential server→client sampling (parity with the
    // legacy room); kept minimal for now.
    let _ = peer_slot.lock().await.clone();
}

/// True if a meeting daemon answers on `socket_path` (used by `status`/`start`).
pub async fn daemon_alive(socket_path: &Path) -> bool {
    use super::room_client::RoomConnection;
    RoomConnection::connect(
        socket_path,
        "rozum-status",
        std::time::Duration::from_secs(2),
    )
    .await
    .is_ok()
}

/// Connect and fetch the daemon's room list as `(name, project)` pairs.
pub async fn daemon_rooms(
    socket_path: &Path,
) -> Result<Vec<(String, Option<String>)>, Box<dyn std::error::Error + Send + Sync>> {
    use super::room_client::{RoomConnection, tool_result_text_json};
    let t = std::time::Duration::from_secs(3);
    let mut conn = RoomConnection::connect(socket_path, "rozum-status", t).await?;
    let res = conn
        .call_tool("rooms.list", serde_json::json!({}), t)
        .await?;
    let v = tool_result_text_json(&res).ok_or("bad rooms.list result")?;
    let rooms = v
        .get("rooms")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    Ok(rooms
        .into_iter()
        .map(|r| {
            let name = r
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("?")
                .to_string();
            let project = r
                .get("project")
                .and_then(|p| p.as_str())
                .map(str::to_string);
            (name, project)
        })
        .collect())
}

struct PollGuard {
    room: RoomHandle,
    id: ParticipantId,
}

impl Drop for PollGuard {
    fn drop(&mut self) {
        let room = self.room.clone();
        let id = self.id.clone();
        tokio::spawn(async move {
            room.lock().await.unmark_polling(&id);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meeting::room_client::{RoomConnection, tool_result_text_json};
    use std::time::Duration;
    use tempfile::tempdir;

    async fn wait_for_socket(path: &Path) {
        for _ in 0..100 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("socket never appeared: {}", path.display());
    }

    #[tokio::test]
    async fn daemon_join_submit_wait_roundtrip() {
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

        let t = Duration::from_secs(5);
        let mut conn = RoomConnection::connect(&sock, "claude", t).await.unwrap();
        let project = dir.path().to_string_lossy().to_string();

        // Join the project room.
        let join = conn
            .call_tool(
                "_join_internal",
                serde_json::json!({
                    "client_info_name": "claude",
                    "project": project,
                    "session_token": "tok-A",
                }),
                t,
            )
            .await
            .unwrap();
        let join = tool_result_text_json(&join).unwrap();
        assert_eq!(join["room"], project_room_name(&project));
        assert!(join["handle"].as_str().unwrap().contains('-'));
        // The room's disk location is returned so the client can read content.
        let root = std::path::PathBuf::from(join["root"].as_str().unwrap());

        // Submit a message.
        let sub = conn
            .call_tool(
                "meeting.submit",
                serde_json::json!({ "content": "hello daemon" }),
                t,
            )
            .await
            .unwrap();
        let date = tool_result_text_json(&sub).unwrap()["date"]
            .as_str()
            .unwrap()
            .to_string();

        // wait_my_turn returns coordination only (no content on the wire).
        let wait = conn
            .call_tool("meeting.wait_my_turn", serde_json::json!({}), t)
            .await
            .unwrap();
        let wait = tool_result_text_json(&wait).unwrap();
        assert_eq!(wait["still_waiting"], false);
        assert_eq!(wait["high_water"]["n"], 1);
        assert!(
            wait.get("turns").is_none(),
            "content must not transit the daemon"
        );

        // The content is on disk, where the client reads it directly.
        let turns = crate::meeting::store::read_day(&root, &date, 0, None).unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].content, "hello daemon");

        // rooms.list now shows the project room.
        let list = conn
            .call_tool("rooms.list", serde_json::json!({}), t)
            .await
            .unwrap();
        let list = tool_result_text_json(&list).unwrap();
        let names: Vec<_> = list["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect();
        assert!(names.contains(&project_room_name(&project)));
    }

    #[tokio::test]
    async fn guard_isolates_panics() {
        // A normal handler passes through.
        let ok = guard("t", async { text_result("{\"ok\":true}") }).await;
        assert_ne!(ok.is_error, Some(true));
        // A panicking handler is caught and returned as an error — not propagated.
        let bad = guard("t", async {
            panic!("boom");
            #[allow(unreachable_code)]
            text_result("never")
        })
        .await;
        assert_eq!(bad.is_error, Some(true));
    }

    #[tokio::test]
    async fn rooms_new_creates_ad_hoc_and_list_enriches() {
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

        let t = Duration::from_secs(5);
        let mut conn = RoomConnection::connect(&sock, "alice", t).await.unwrap();

        // Create an ad-hoc room with a topic.
        let new = tool_result_text_json(
            &conn
                .call_tool("rooms.new", serde_json::json!({ "topic": "hi there" }), t)
                .await
                .unwrap(),
        )
        .unwrap();
        let name = new["room"].as_str().unwrap().to_string();
        assert!(!name.is_empty());
        // Materialize it (lazy until first message).
        conn.call_tool("meeting.submit", serde_json::json!({ "content": "x" }), t)
            .await
            .unwrap();

        // rooms.list shows it, enriched with topic + participant count.
        let list = tool_result_text_json(
            &conn
                .call_tool("rooms.list", serde_json::json!({}), t)
                .await
                .unwrap(),
        )
        .unwrap();
        let room = list["rooms"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == serde_json::json!(name))
            .expect("new room is listed");
        assert_eq!(room["topic"], "hi there");
        assert!(room["participants"].as_u64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn graceful_shutdown_ends_pending_waits() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("meeting.sock");
        let registry = Arc::new(RoomRegistry::new(dir.path().join("state")));
        let (tx, rx) = watch::channel(false);
        {
            let sock = sock.clone();
            tokio::spawn(async move {
                let _ = serve_daemon_until(&sock, registry, rx).await;
            });
        }
        wait_for_socket(&sock).await;

        let t = Duration::from_secs(10);
        let project = dir.path().to_string_lossy().into_owned();
        let mut conn = RoomConnection::connect(&sock, "claude", t).await.unwrap();
        conn.call_tool(
            "_join_internal",
            serde_json::json!({ "client_info_name": "claude", "project": project }),
            t,
        )
        .await
        .unwrap();

        // Start a long-poll (empty room → it blocks), then signal shutdown.
        let waiter = tokio::spawn(async move {
            conn.call_tool("meeting.wait_my_turn", serde_json::json!({}), t)
                .await
        });
        tokio::time::sleep(Duration::from_millis(150)).await;
        tx.send(true).unwrap();

        let res = waiter.await.unwrap().unwrap();
        let payload = tool_result_text_json(&res).unwrap();
        assert_eq!(payload["ended"], true);
        assert_eq!(payload["reason"], "server-shutdown");
    }

    #[tokio::test]
    async fn same_token_rebinds_identity_across_reconnect() {
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
        let t = Duration::from_secs(5);
        let project = dir.path().to_string_lossy().to_string();

        let join_once = |token: &'static str| {
            let sock = sock.clone();
            let project = project.clone();
            async move {
                let mut conn = RoomConnection::connect(&sock, "claude", t).await.unwrap();
                let j = conn
                    .call_tool(
                        "_join_internal",
                        serde_json::json!({
                            "client_info_name": "claude",
                            "project": project,
                            "session_token": token,
                        }),
                        t,
                    )
                    .await
                    .unwrap();
                // Must submit so the room materializes + roster persists.
                conn.call_tool("meeting.submit", serde_json::json!({"content":"hi"}), t)
                    .await
                    .unwrap();
                tool_result_text_json(&j).unwrap()
            }
        };

        let first = join_once("tok-A").await;
        let second = join_once("tok-A").await; // reconnect, same token
        assert_eq!(first["participant_id"], second["participant_id"]);
        assert_eq!(first["handle"], second["handle"]);
    }
}
