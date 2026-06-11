//! stdio MCP proxy. Agents configure this as their MCP server.
//! Implements rooms.list + rooms.join and forwards meeting.* to the chosen room.

use std::sync::Arc;

use rmcp::{
    ClientHandler, ErrorData, Peer, RoleClient, RoleServer, ServerHandler, ServiceError,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolRequestParams, CallToolResult, ClientCapabilities, ClientInfo, Content,
        CreateMessageRequestParams, CreateMessageResult, Implementation, InitializeRequestParams,
        InitializeResult, JsonObject, SamplingCapability, SamplingTaskCapability,
        ServerCapabilities, TaskRequestsCapability, TasksCapability,
    },
    service::{RequestContext, RunningService},
    tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use tokio::net::UnixStream;
use tokio::sync::Mutex;

use super::list::list_rooms;
use super::room_path::room_socket;

// ── State ─────────────────────────────────────────────────────────────────────

#[derive(Default)]
struct ProxyState {
    /// Live room connection for this proxy MCP session.
    current_room: Option<RoomConn>,
    /// Name of the room we are currently joined to (or last attempted).
    /// Remembered so `try_reconnect_current_room` can re-establish the
    /// same connection after the underlying Unix socket dies — e.g. when
    /// the operator restarts the `rozum --room <name>` process.
    current_room_name: Option<String>,
    /// The agent's self-reported name from MCP initialize.
    client_info_name: String,
    upstream_peer: Option<Peer<RoleServer>>,
    upstream_supports_sampling: bool,
    /// Background task that re-emits `meeting.mark_responding` every 15 s
    /// while the agent holds an active turn. Aborted on submit/leave/new turn.
    heartbeat_task: Option<tokio::task::JoinHandle<()>>,
}

/// Capped exponential backoff schedule used by `try_reconnect_current_room`.
/// Sum ≈ 18 s — long enough to cover a typical `cargo build && rerun` of
/// `rozum`, short enough not to hide a genuine failure.
const RECONNECT_DELAYS_MS: &[u64] = &[200, 200, 500, 500, 1000, 1000, 2000, 2000, 5000, 5000];
const HEARTBEAT_INTERVAL_SECS: u64 = 30;

struct RoomConn {
    service: RunningService<RoleClient, RoomProxyClient>,
}

#[derive(Clone)]
struct RoomProxyClient {
    client_info_name: String,
    upstream_peer: Option<Peer<RoleServer>>,
    upstream_supports_sampling: bool,
}

// ── Proxy server ──────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct ProxyServer {
    state: Arc<Mutex<ProxyState>>,
    tool_router: ToolRouter<Self>,
}

impl ProxyServer {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(ProxyState::default())),
            tool_router: Self::tool_router(),
        }
    }

    async fn call_room_tool(&self, tool_name: &str, params: serde_json::Value) -> CallToolResult {
        let peer = {
            let state = self.state.lock().await;
            let Some(room) = state.current_room.as_ref() else {
                return err_result("not-joined: call rooms.join first");
            };
            room.service.peer().clone()
        };

        match call_room_tool_via_peer(&peer, tool_name, params.clone()).await {
            Ok(result) => result,
            Err(_) => {
                // Underlying Unix socket likely died (e.g. operator restarted
                // `rozum`). Try to re-establish the same room transparently
                // before surfacing the failure to the agent.
                self.state.lock().await.current_room = None;
                match self.try_reconnect_current_room().await {
                    Ok(new_peer) => {
                        match call_room_tool_via_peer(&new_peer, tool_name, params).await {
                            Ok(result) => result,
                            Err(e) => err_result(&format!("room-error: {e}")),
                        }
                    }
                    Err(e) => err_result(&format!("room-error: {e}")),
                }
            }
        }
    }

    /// Re-establish the current room connection after a transport failure.
    /// Uses `RECONNECT_DELAYS_MS` for capped backoff between attempts. On
    /// success, stores the new `RoomConn` in `state.current_room` and
    /// returns the new peer for the caller to retry the failed tool call.
    async fn try_reconnect_current_room(
        &self,
    ) -> Result<Peer<RoleClient>, Box<dyn std::error::Error + Send + Sync>> {
        let (name, client_info_name, upstream_peer, upstream_supports_sampling) = {
            let s = self.state.lock().await;
            let name = s
                .current_room_name
                .clone()
                .ok_or("no current room name to reconnect to")?;
            (
                name,
                client_name_or_default(&s.client_info_name),
                s.upstream_peer.clone(),
                s.upstream_supports_sampling,
            )
        };

        let socket_path = room_socket(&name);
        for (attempt, delay_ms) in RECONNECT_DELAYS_MS.iter().enumerate() {
            tokio::time::sleep(std::time::Duration::from_millis(*delay_ms)).await;
            if !socket_path.exists() {
                continue;
            }
            let stream = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                UnixStream::connect(&socket_path),
            )
            .await
            {
                Ok(Ok(stream)) => stream,
                _ => continue,
            };
            let room_client = RoomProxyClient {
                client_info_name: client_info_name.clone(),
                upstream_peer: upstream_peer.clone(),
                upstream_supports_sampling,
            };
            use rmcp::ServiceExt;
            let service = match room_client.serve(stream).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            let join_result = call_room_tool_via_peer(
                service.peer(),
                "_join_internal",
                serde_json::json!({ "client_info_name": client_info_name.clone() }),
            )
            .await;
            if !matches!(&join_result, Ok(r) if r.is_error != Some(true)) {
                continue;
            }
            let peer = service.peer().clone();
            tracing::info!(
                room = %name,
                attempt = attempt + 1,
                "mcp-proxy reconnected to room"
            );
            self.state.lock().await.current_room = Some(RoomConn { service });
            return Ok(peer);
        }
        Err(format!(
            "reconnect failed after {} attempts; room '{}' did not return",
            RECONNECT_DELAYS_MS.len(),
            name
        )
        .into())
    }
}

// ── Tool parameter types ──────────────────────────────────────────────────────

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct JoinParams {
    /// Name of the room to join (from rooms.list output).
    pub name: String,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct WaitMyTurnParams {
    /// Sequence number of last turn seen (optional).
    pub since_seq: Option<usize>,
}

#[derive(Serialize, Deserialize, schemars::JsonSchema)]
pub struct SubmitParams {
    /// Your reply content.
    pub content: String,
}

// ── Tools ─────────────────────────────────────────────────────────────────────

#[tool_router(router = tool_router)]
impl ProxyServer {
    /// List active rozum meeting rooms on this machine.
    #[tool(
        name = "rooms.list",
        description = "List active rozum meeting rooms. Returns name, topic, participants."
    )]
    pub async fn rooms_list(&self) -> CallToolResult {
        let rooms = list_rooms().await;
        if rooms.is_empty() {
            return text_result(
                "{\"rooms\":[],\"message\":\"No active rozum rooms. Start one with: rozum --topic \\\"Your topic\\\"\"}",
            );
        }
        let arr: Vec<_> = rooms
            .iter()
            .map(|r| {
                serde_json::json!({
                    "name": r.name,
                    "topic": r.topic,
                    "participants": r.participants,
                })
            })
            .collect();
        text_result(&serde_json::json!({ "rooms": arr }).to_string())
    }

    /// Join a named meeting room.
    #[tool(
        name = "rooms.join",
        description = "Join a rozum meeting room by name. After joining, use meeting.wait_my_turn and meeting.submit."
    )]
    pub async fn rooms_join(&self, params: Parameters<JoinParams>) -> CallToolResult {
        let name = params.0.name.clone();
        let (client_info_name, upstream_peer, upstream_supports_sampling) = {
            let s = self.state.lock().await;
            if s.current_room.is_some() {
                return err_result("already-joined: call meeting.leave first");
            }
            (
                client_name_or_default(&s.client_info_name),
                s.upstream_peer.clone(),
                s.upstream_supports_sampling,
            )
        };

        let socket_path = room_socket(&name);
        if !socket_path.exists() {
            return err_result(&format!("room-not-found: {name}"));
        }

        let stream = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            UnixStream::connect(&socket_path),
        )
        .await
        {
            Ok(Ok(stream)) => stream,
            Ok(Err(e)) => return err_result(&format!("join-error: connect error: {e}")),
            Err(_) => return err_result("join-error: connection timeout"),
        };

        let room_client = RoomProxyClient {
            client_info_name: client_info_name.clone(),
            upstream_peer,
            upstream_supports_sampling,
        };

        use rmcp::ServiceExt;
        let service = match room_client.serve(stream).await {
            Ok(service) => service,
            Err(e) => return err_result(&format!("join-error: initialize room client: {e}")),
        };

        let join_result = call_room_tool_via_peer(
            service.peer(),
            "_join_internal",
            serde_json::json!({ "client_info_name": client_info_name.clone() }),
        )
        .await;

        match join_result {
            Ok(r) if r.is_error != Some(true) => {
                let mut s = self.state.lock().await;
                s.current_room = Some(RoomConn { service });
                s.current_room_name = Some(name);
                r
            }
            Ok(r) => r,
            Err(e) => err_result(&format!("join-error: {e}")),
        }
    }

    /// Wait for your turn to speak. Long-polls up to 25 seconds. Call again on still_waiting.
    #[tool(
        name = "meeting.wait_my_turn",
        description = "Wait for your turn to speak. Long-polls 25s. Retry immediately on still_waiting."
    )]
    pub async fn wait_my_turn(&self, params: Parameters<WaitMyTurnParams>) -> CallToolResult {
        let result = self
            .call_room_tool(
                "meeting.wait_my_turn",
                serde_json::json!({ "since_seq": params.0.since_seq }),
            )
            .await;
        if your_turn_in_result(&result) {
            self.start_heartbeat().await;
        }
        result
    }

    /// Submit a message to the meeting. Anyone can submit at any time.
    #[tool(
        name = "meeting.submit",
        description = "Submit a message to the meeting. Anyone can submit at any time."
    )]
    pub async fn submit(&self, params: Parameters<SubmitParams>) -> CallToolResult {
        self.cancel_heartbeat().await;
        self.call_room_tool(
            "meeting.submit",
            serde_json::json!({ "content": params.0.content }),
        )
        .await
    }

    /// Leave the current meeting room.
    #[tool(
        name = "meeting.leave",
        description = "Leave the current meeting room."
    )]
    pub async fn leave(&self) -> CallToolResult {
        self.cancel_heartbeat().await;
        let result = self
            .call_room_tool("meeting.leave", serde_json::json!({}))
            .await;
        let mut s = self.state.lock().await;
        s.current_room = None;
        s.current_room_name = None;
        result
    }

    /// Get the current meeting status.
    #[tool(
        name = "meeting.status",
        description = "Get meeting status: participants, topic, budget."
    )]
    pub async fn status(&self) -> CallToolResult {
        self.call_room_tool("meeting.status", serde_json::json!({}))
            .await
    }

    /// Signal that you are composing a reply right now. Surfaces a
    /// "typing" indicator to other participants. Cleared automatically by
    /// `meeting.submit`, `meeting.leave`, or 30 seconds of inactivity.
    #[tool(
        name = "meeting.mark_responding",
        description = "Signal that you are composing a response. Cleared on submit/leave or 30s stale."
    )]
    pub async fn mark_responding(&self) -> CallToolResult {
        self.call_room_tool("meeting.mark_responding", serde_json::json!({}))
            .await
    }

    /// Spawn a background task that re-emits `mark_responding` every 15 s so
    /// the agent shows as "typing" even if it never calls the tool itself.
    /// Aborts any prior task first so a fresh turn always starts a fresh
    /// heartbeat.
    async fn start_heartbeat(&self) {
        let peer = {
            let state = self.state.lock().await;
            state
                .current_room
                .as_ref()
                .map(|r| r.service.peer().clone())
        };
        let Some(peer) = peer else { return };

        self.cancel_heartbeat().await;

        // Fire one immediate mark so the typing indicator appears without a
        // 15 s wait. Ignore failures: the loop below will retry.
        let _ =
            call_room_tool_via_peer(&peer, "meeting.mark_responding", serde_json::json!({})).await;

        let peer_for_loop = peer;
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(HEARTBEAT_INTERVAL_SECS)).await;
                if call_room_tool_via_peer(
                    &peer_for_loop,
                    "meeting.mark_responding",
                    serde_json::json!({}),
                )
                .await
                .is_err()
                {
                    break;
                }
            }
        });
        self.state.lock().await.heartbeat_task = Some(task);
    }

    async fn cancel_heartbeat(&self) {
        let mut state = self.state.lock().await;
        if let Some(h) = state.heartbeat_task.take() {
            h.abort();
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for ProxyServer {
    async fn initialize(
        &self,
        params: InitializeRequestParams,
        context: RequestContext<rmcp::service::RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // Record the agent's identity for use when joining rooms.
        let client_name = params
            .client_info
            .name
            .trim()
            .to_lowercase()
            .replace(' ', "-");
        let supports_sampling = client_supports_sampling(&params.capabilities);
        {
            let mut state = self.state.lock().await;
            state.client_info_name = client_name;
            state.upstream_peer = Some(context.peer.clone());
            state.upstream_supports_sampling = supports_sampling;
        }
        Ok(InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                "rozum-proxy",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Use rooms.list to see active meeting rooms, then rooms.join(name) to join one. \
                 After joining, loop: meeting.wait_my_turn (25s long-poll, retry immediately on \
                 still_waiting) → meeting.submit if you have something to add. Anyone may submit \
                 any time — there are no turns. \
                 Before composing, check the responding[] array: if a sibling agent is already \
                 typing the same reply, wait. Keep replies short. \
                 For long offline work, post 'working: <what>' before going dark and 'done: \
                 <result>' on return so other participants see your status. \
                 Call meeting.leave when finished. \
                 Full etiquette: see vendor/agent-plugins/rozum/commands/rozum.md.",
            ))
    }
}

impl ClientHandler for RoomProxyClient {
    async fn create_message(
        &self,
        params: CreateMessageRequestParams,
        _context: RequestContext<RoleClient>,
    ) -> Result<CreateMessageResult, ErrorData> {
        // Always try forwarding to upstream peer first (even if it didn't declare
        // sampling capability — Claude Code supports it but may not advertise it).
        if let Some(peer) = &self.upstream_peer {
            tracing::info!(
                client = %self.client_info_name,
                upstream_supports_sampling = self.upstream_supports_sampling,
                "sampling: forwarding create_message to upstream peer"
            );
            match peer.create_message(params.clone()).await {
                Ok(result) => {
                    tracing::info!(
                        client = %self.client_info_name,
                        model = %result.model,
                        "sampling: upstream responded successfully"
                    );
                    return Ok(result);
                }
                Err(ServiceError::McpError(ref e))
                    if e.code == rmcp::model::ErrorCode::METHOD_NOT_FOUND =>
                {
                    tracing::info!(
                        client = %self.client_info_name,
                        "sampling: upstream returned method_not_found, trying API fallback"
                    );
                }
                Err(ref e) => {
                    tracing::info!(
                        client = %self.client_info_name,
                        error = %e,
                        "sampling: upstream error, trying API fallback"
                    );
                }
            }
        } else {
            tracing::info!(client = %self.client_info_name, "sampling: no upstream peer, trying API fallback");
        }

        // Fallback: call Anthropic API directly if a key is configured.
        tracing::info!(client = %self.client_info_name, "sampling: calling Anthropic API fallback");
        match anthropic_sample(params).await {
            Ok(result) => {
                tracing::info!(client = %self.client_info_name, model = %result.model, "sampling: API fallback succeeded");
                Ok(result)
            }
            Err(e) => {
                tracing::info!(client = %self.client_info_name, error = %e, "sampling: API fallback failed");
                Err(ErrorData::internal_error(
                    format!("anthropic-fallback error: {e}"),
                    None,
                ))
            }
        }
    }

    fn get_info(&self) -> ClientInfo {
        ClientInfo::new(
            client_capabilities(self.upstream_supports_sampling || self.upstream_peer.is_some()),
            Implementation::new(self.client_info_name.clone(), env!("CARGO_PKG_VERSION")),
        )
    }
}

// ── Forward to room via bidirectional MCP client ─────────────────────────────

async fn call_room_tool_via_peer(
    peer: &Peer<RoleClient>,
    method: &str,
    params: serde_json::Value,
) -> Result<CallToolResult, Box<dyn std::error::Error + Send + Sync>> {
    let arguments = params.as_object().cloned().unwrap_or_default();
    Ok(peer
        .call_tool(CallToolRequestParams::new(method.to_owned()).with_arguments(arguments))
        .await?)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn text_result(content: &str) -> CallToolResult {
    CallToolResult::success(vec![Content::text(content)])
}

fn err_result(msg: &str) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

/// Look inside a `meeting.wait_my_turn` result for `turn.your_turn == true`.
fn your_turn_in_result(result: &CallToolResult) -> bool {
    if result.is_error.unwrap_or(false) {
        return false;
    }
    let json = match serde_json::to_value(result) {
        Ok(v) => v,
        Err(_) => return false,
    };
    let Some(payload) = super::room_client::tool_result_text_json(&json) else {
        return false;
    };
    payload
        .get("turn")
        .and_then(|t| t.get("your_turn"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
        || payload
            .get("your_turn")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
}

fn client_name_or_default(client_info_name: &str) -> String {
    let name = client_info_name.trim();
    let base = if name.is_empty() { "agent" } else { name };
    match project_name() {
        Some(p) if !p.is_empty() => format!("{p}-{base}"),
        _ => base.to_owned(),
    }
}

/// Basename of the current working directory. Used to scope agent display
/// names so different projects' agents do not collide in a shared room.
fn project_name() -> Option<String> {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| cwd.file_name().and_then(|s| s.to_str()).map(str::to_owned))
}

fn client_supports_sampling(capabilities: &ClientCapabilities) -> bool {
    capabilities.sampling.is_some()
        || capabilities
            .tasks
            .as_ref()
            .is_some_and(|tasks| tasks.supports_sampling_create_message())
}

fn client_capabilities(enable_sampling: bool) -> ClientCapabilities {
    let mut capabilities = ClientCapabilities::default();
    if enable_sampling {
        capabilities.sampling = Some(SamplingCapability::default());
        capabilities.tasks = Some(TasksCapability {
            requests: Some(TaskRequestsCapability {
                sampling: Some(SamplingTaskCapability {
                    create_message: Some(JsonObject::new()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });
    }
    capabilities
}

async fn anthropic_sample(
    params: CreateMessageRequestParams,
) -> Result<CreateMessageResult, Box<dyn std::error::Error + Send + Sync>> {
    let api_key = std::env::var("ANTHROPIC_API_KEY").map_err(|_| "ANTHROPIC_API_KEY not set")?;
    let model =
        std::env::var("ANTHROPIC_MODEL").unwrap_or_else(|_| "claude-haiku-4-5-20251001".to_owned());

    // Convert SamplingMessages to Anthropic format.
    let messages: Vec<serde_json::Value> = params
        .messages
        .iter()
        .map(|m| {
            let role = match m.role {
                rmcp::model::Role::User => "user",
                rmcp::model::Role::Assistant => "assistant",
            };
            let text = m
                .content
                .iter()
                .filter_map(|c| c.as_text())
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join("");
            serde_json::json!({ "role": role, "content": text })
        })
        .collect();

    let mut body = serde_json::json!({
        "model": model,
        "max_tokens": params.max_tokens,
        "messages": messages,
    });
    if let Some(system) = &params.system_prompt {
        body["system"] = serde_json::Value::String(system.clone());
    }

    let response = reqwest::Client::new()
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", &api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await?;

    let status = response.status();
    let json: serde_json::Value = response.json().await?;

    if !status.is_success() {
        let msg = json["error"]["message"]
            .as_str()
            .unwrap_or("unknown error")
            .to_owned();
        return Err(format!("Anthropic API {status}: {msg}").into());
    }

    let text = json["content"]
        .as_array()
        .and_then(|arr| arr.iter().find(|c| c["type"] == "text"))
        .and_then(|c| c["text"].as_str())
        .unwrap_or("")
        .to_owned();
    let resp_model = json["model"].as_str().unwrap_or(&model).to_owned();
    let stop_reason = json["stop_reason"].as_str().map(|s| s.to_owned());

    let mut result = rmcp::model::CreateMessageResult::new(
        rmcp::model::SamplingMessage::assistant_text(text),
        resp_model,
    );
    if let Some(reason) = stop_reason {
        result = result.with_stop_reason(reason);
    }
    Ok(result)
}

/// Run the proxy as a stdio MCP server (blocking until stdin closes).
pub async fn run_proxy() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use rmcp::{ServiceExt, transport::stdio};
    let server = ProxyServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use rmcp::ServiceExt;
    use rmcp::model::{CreateMessageResult, SamplingMessage};
    use tokio::net::UnixListener;
    use tokio::sync::broadcast;

    #[test]
    fn detects_legacy_sampling_capability() {
        let mut capabilities = ClientCapabilities::default();
        capabilities.sampling = Some(SamplingCapability::default());

        assert!(client_supports_sampling(&capabilities));
    }

    #[test]
    fn detects_task_sampling_capability() {
        let mut capabilities = ClientCapabilities::default();
        capabilities.tasks = Some(TasksCapability {
            requests: Some(TaskRequestsCapability {
                sampling: Some(SamplingTaskCapability {
                    create_message: Some(JsonObject::new()),
                }),
                ..Default::default()
            }),
            ..Default::default()
        });

        assert!(client_supports_sampling(&capabilities));
    }

    #[test]
    fn advertised_room_client_capability_matches_upstream() {
        assert!(client_capabilities(true).sampling.is_some());
        assert!(
            client_capabilities(true)
                .tasks
                .as_ref()
                .is_some_and(|tasks| tasks.supports_sampling_create_message())
        );
        assert!(client_capabilities(false).sampling.is_none());
    }

    #[derive(Clone)]
    struct FakeSamplingClient {
        calls: Arc<AtomicUsize>,
    }

    impl ClientHandler for FakeSamplingClient {
        async fn create_message(
            &self,
            params: CreateMessageRequestParams,
            _context: RequestContext<RoleClient>,
        ) -> Result<CreateMessageResult, ErrorData> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(params.messages.len(), 1);
            assert!(
                params
                    .system_prompt
                    .as_deref()
                    .unwrap_or_default()
                    .contains("wake test")
            );
            Ok(CreateMessageResult::new(
                SamplingMessage::assistant_text("woken through proxy"),
                "fake-sampling-model".to_owned(),
            )
            .with_stop_reason(CreateMessageResult::STOP_REASON_END_TURN))
        }

        fn get_info(&self) -> ClientInfo {
            ClientInfo::new(
                client_capabilities(true),
                Implementation::new("codex-test", env!("CARGO_PKG_VERSION")),
            )
        }
    }

    #[tokio::test]
    async fn forwards_room_sampling_to_upstream_client()
    -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::meeting::budget::BudgetGuard;
        use crate::meeting::mcp_server::{ConnParticipant, PeerRegistry, RoomServer};
        use crate::meeting::participant::ParticipantId;
        use crate::meeting::room_path::{ensure_rooms_dir, room_socket};
        use crate::meeting::state::{Meeting, MeetingEvent};

        ensure_rooms_dir()?;
        let room_name = format!("proxy-sampling-{}", uuid::Uuid::new_v4());
        let socket_path = room_socket(&room_name);
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)?;
        let (events_tx, _) = broadcast::channel::<MeetingEvent>(8);
        let meeting = Arc::new(tokio::sync::Mutex::new(Meeting::new(
            &room_name,
            "wake test",
            "sergiy",
            BudgetGuard::default(),
            events_tx,
        )));
        let peer_registry: PeerRegistry =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));

        let accept_meeting = Arc::clone(&meeting);
        let accept_registry = Arc::clone(&peer_registry);
        let accept_task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let conn_participant: ConnParticipant = Arc::new(tokio::sync::Mutex::new(None));
            let handler = RoomServer::new(accept_meeting, conn_participant, accept_registry);
            if let Ok(service) = handler.serve(stream).await {
                service.waiting().await.ok();
            }
        });

        let (proxy_server_transport, proxy_client_transport) = tokio::io::duplex(16384);
        let proxy_task = tokio::spawn(async move {
            let service = ProxyServer::new()
                .serve(proxy_server_transport)
                .await
                .unwrap();
            service.waiting().await.ok();
        });

        let calls = Arc::new(AtomicUsize::new(0));
        let upstream = FakeSamplingClient {
            calls: Arc::clone(&calls),
        };
        let upstream_service = upstream.serve(proxy_client_transport).await?;

        let join = call_room_tool_via_peer(
            upstream_service.peer(),
            "rooms.join",
            serde_json::json!({ "name": room_name }),
        )
        .await?;
        assert_ne!(join.is_error, Some(true));

        // The proxy scopes agent display names with the cwd basename
        // (`client_name_or_default`), so the registered participant id is the
        // prefixed name, not the bare client_info name.
        let expected_id = ParticipantId::new(client_name_or_default("codex-test"));
        let room_peer = peer_registry
            .lock()
            .await
            .get(&expected_id)
            .cloned()
            .expect("proxy should register room-side peer for the joined client");
        assert!(
            room_peer
                .peer_info()
                .is_some_and(|info| info.capabilities.sampling.is_some())
        );

        let result = room_peer
            .create_message(
                CreateMessageRequestParams::new(vec![SamplingMessage::user_text("Your turn.")], 64)
                    .with_system_prompt("wake test prompt"),
            )
            .await?;
        let response_text = result
            .message
            .content
            .iter()
            .find_map(|content| content.as_text())
            .map(|text| text.text.as_str())
            .unwrap_or_default();

        assert_eq!(response_text, "woken through proxy");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        upstream_service.cancel().await?;
        accept_task.abort();
        proxy_task.abort();
        let _ = std::fs::remove_file(&socket_path);

        Ok(())
    }
}
