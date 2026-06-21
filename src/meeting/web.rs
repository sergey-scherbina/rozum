//! Daemon-backed human **web client** for meeting rooms — a browser front-end equal to the
//! TUI (the legacy `crate::web` bridges the in-process room; this reads the *daemon's* on-disk
//! transcript and submits through the daemon). Gated behind a single shared secret.
//!
//! `rozum meetings web [--port P] [--room name] [--bind addr]`:
//! - `GET /`            — the chat page (history + live + an input box).
//! - `GET /api/messages`— the room transcript, read directly from disk.
//! - `POST /api/submit` — post a message (as the local human identity).
//! - `GET /api/stream`  — Server-Sent Events tailing the room → new messages appear live (the
//!   **wakeup**: when an agent posts, the browser sees it without reloading).
//!
//! Auth: HTTP Basic, password = the shared secret (`ROZUM_WEB_SECRET`, else generated + printed).
//! The username field is ignored — the secret is the gate; posts use the operator's identity.
//! First equal non-TUI client; P3 groundwork. See `docs/specs/agent-meeting-coordination.md`.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response, Sse, sse::Event},
    routing::{get, post},
};
use base64::Engine;
use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::{Mutex, broadcast};

use super::daemon::daemon_alive;
use super::daemon_proxy::{detect_project, spawn_daemon};
use super::model_participant::{ReplyPolicy, derive_handle};
use super::room_path::meeting_sock;
use super::state::unix_ts;
use super::store::{self, StoredTurn};
use super::tui_client::MeetingClient;

type WebResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:8080/v1";

#[derive(Clone)]
struct WebState {
    /// One persistent daemon client (the operator's identity) — all web posts go through it,
    /// so the room shows a single stable web participant.
    client: Arc<Mutex<MeetingClient>>,
    /// The joined room's on-disk dir (read the transcript from here).
    root: PathBuf,
    /// New-message fan-out to SSE subscribers (the tail task feeds it).
    tx: broadcast::Sender<String>,
    room: String,
    model: Arc<Mutex<ModelControl>>,
}

#[derive(Clone)]
struct AuthCfg {
    secret: String,
}

#[derive(Deserialize)]
struct SubmitBody {
    content: String,
}

#[derive(Deserialize)]
struct ModelStartBody {
    model: String,
    #[serde(default)]
    as_handle: Option<String>,
    #[serde(default)]
    reply_policy: Option<String>,
    #[serde(default)]
    gateway_url: Option<String>,
    #[serde(default)]
    peers: Option<String>,
    #[serde(default)]
    persona: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct ModelParticipantConfig {
    model: String,
    as_handle: String,
    reply_policy: String,
    gateway_url: String,
    peers: Vec<String>,
    persona: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct GatewayProbe {
    url: String,
    ok: bool,
    status: Option<u16>,
    error: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
struct ModelStatus {
    running: bool,
    pid: Option<u32>,
    config: Option<ModelParticipantConfig>,
    started_at: Option<u64>,
    last_exit: Option<String>,
    gateway: Option<GatewayProbe>,
}

#[derive(Default)]
struct ModelControl {
    child: Option<Child>,
    config: Option<ModelParticipantConfig>,
    started_at: Option<u64>,
    last_exit: Option<String>,
}

enum ModelControlError {
    AlreadyRunning,
    Spawn(String),
}

/// Serve the web client for a room (`room` = a named room, else the cwd project room),
/// bound to `bind:port`, gated by `secret`. Runs until the process is stopped.
pub async fn run_web(
    room: Option<String>,
    port: u16,
    bind: String,
    secret: String,
) -> WebResult<()> {
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        spawn_daemon().await;
    }
    let id = super::local_identity::load_or_create();
    let mut client = MeetingClient::connect_as(&sock, &id.display, &id.token)
        .await
        .map_err(|e| format!("connect daemon: {e}"))?;
    let room_name = match &room {
        Some(name) => client
            .enter_or_create(name)
            .await
            .map_err(|e| format!("join '{name}': {e}"))?,
        None => {
            let project =
                detect_project().ok_or("no project (run inside a repo, or pass --room)")?;
            client
                .enter_project(&project)
                .await
                .map_err(|e| format!("join project room: {e}"))?
        }
    };
    let root = client
        .room_root()
        .ok_or("no room root after join")?
        .to_path_buf();

    let (tx, _) = broadcast::channel::<String>(256);
    let state = WebState {
        client: Arc::new(Mutex::new(client)),
        root: root.clone(),
        tx: tx.clone(),
        room: room_name.clone(),
        model: Arc::new(Mutex::new(ModelControl::default())),
    };

    // Tail the room transcript on disk → fan out new turns to SSE clients (the wakeup). Starts
    // at the current head so the stream carries only what arrives after page load; /api/messages
    // serves history. Reads are cheap (seek-from-cursor of small day files).
    {
        let root = root.clone();
        tokio::spawn(async move {
            let mut cursor = store::read_since(&root, None, 0)
                .last()
                .map(|t| (t.date.clone(), t.n + 1));
            loop {
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let (sd, sn) = match &cursor {
                    Some((d, n)) => (Some(d.as_str()), *n),
                    None => (None, 0),
                };
                let turns = store::read_since(&root, sd, sn);
                if let Some(last) = turns.last() {
                    cursor = Some((last.date.clone(), last.n + 1));
                    for t in &turns {
                        let _ = tx.send(turn_json(t).to_string());
                    }
                }
            }
        });
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/messages", get(messages))
        .route("/api/submit", post(submit))
        .route("/api/stream", get(stream))
        .route("/api/model/status", get(model_status))
        .route("/api/model/start", post(model_start))
        .route("/api/model/stop", post(model_stop))
        .layer(middleware::from_fn_with_state(
            AuthCfg { secret },
            auth_layer,
        ))
        .with_state(state);

    let addr: SocketAddr = format!("{bind}:{port}")
        .parse()
        .map_err(|e| format!("bad bind '{bind}:{port}': {e}"))?;
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| format!("bind {addr}: {e}"))?;
    eprintln!(
        "rozum web: room '{room_name}' on http://{addr}  (log in with the secret as the password)"
    );
    axum::serve(listener, app)
        .await
        .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
    Ok(())
}

fn turn_json(t: &StoredTurn) -> Value {
    json!({ "from": t.display_name, "content": t.content, "date": t.date, "n": t.n, "ts": t.ts })
}

async fn index() -> Html<&'static str> {
    Html(include_str!("web_index.html"))
}

async fn messages(State(s): State<WebState>) -> Json<Value> {
    let msgs: Vec<Value> = store::read_since(&s.root, None, 0)
        .iter()
        .map(turn_json)
        .collect();
    Json(json!({ "room": s.room, "messages": msgs }))
}

async fn submit(State(s): State<WebState>, Json(b): Json<SubmitBody>) -> Response {
    let content = b.content.trim();
    if content.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message").into_response();
    }
    let mut client = s.client.lock().await;
    match client.submit(content).await {
        Ok(()) => (StatusCode::OK, "ok").into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("submit failed: {e}"),
        )
            .into_response(),
    }
}

async fn stream(State(s): State<WebState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let mut rx = s.tx.subscribe();
    let body = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(json) => yield Ok(Event::default().data(json)),
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };
    Sse::new(body).keep_alive(axum::response::sse::KeepAlive::default())
}

async fn model_status(State(s): State<WebState>) -> Json<ModelStatus> {
    let mut status = {
        let mut model = s.model.lock().await;
        model.status()
    };
    status.gateway = gateway_probe(&status.config).await;
    Json(status)
}

async fn model_start(State(s): State<WebState>, Json(b): Json<ModelStartBody>) -> Response {
    let cfg = match model_config_from_body(b) {
        Ok(cfg) => cfg,
        Err(e) => return (StatusCode::BAD_REQUEST, format!("{e}\n")).into_response(),
    };
    let mut status = {
        let mut model = s.model.lock().await;
        match model.start(&s.room, cfg) {
            Ok(status) => status,
            Err(ModelControlError::AlreadyRunning) => {
                return (StatusCode::CONFLICT, "model participant already running\n")
                    .into_response();
            }
            Err(ModelControlError::Spawn(e)) => {
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("start failed: {e}\n"),
                )
                    .into_response();
            }
        }
    };
    status.gateway = gateway_probe(&status.config).await;
    Json(status).into_response()
}

async fn model_stop(State(s): State<WebState>) -> Json<ModelStatus> {
    let mut status = {
        let mut model = s.model.lock().await;
        model.stop().await
    };
    status.gateway = gateway_probe(&status.config).await;
    Json(status)
}

impl ModelControl {
    fn reap(&mut self) {
        let status = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => Some(status.to_string()),
                Ok(None) => None,
                Err(e) => Some(format!("poll failed: {e}")),
            },
            None => None,
        };
        if let Some(status) = status {
            self.child = None;
            self.last_exit = Some(status);
        }
    }

    fn status(&mut self) -> ModelStatus {
        self.reap();
        ModelStatus {
            running: self.child.is_some(),
            pid: self.child.as_ref().and_then(|child| child.id()),
            config: self.config.clone(),
            started_at: self.started_at,
            last_exit: self.last_exit.clone(),
            gateway: None,
        }
    }

    fn start(
        &mut self,
        room: &str,
        cfg: ModelParticipantConfig,
    ) -> Result<ModelStatus, ModelControlError> {
        self.reap();
        if self.child.is_some() {
            return Err(ModelControlError::AlreadyRunning);
        }
        let spec = participant_command_spec(room, &cfg)
            .map_err(|e| ModelControlError::Spawn(format!("resolve participant command: {e}")))?;
        let mut cmd = TokioCommand::new(&spec.program);
        cmd.args(&spec.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        cmd.kill_on_drop(true);
        let child = cmd
            .spawn()
            .map_err(|e| ModelControlError::Spawn(format!("{}: {e}", spec.program.display())))?;
        self.child = Some(child);
        self.config = Some(cfg);
        self.started_at = Some(unix_ts());
        self.last_exit = None;
        Ok(self.status())
    }

    async fn stop(&mut self) -> ModelStatus {
        self.reap();
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
            let status = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            self.last_exit = Some(match status {
                Ok(Ok(status)) => status.to_string(),
                Ok(Err(e)) => format!("wait failed: {e}"),
                Err(_) => "kill timeout".to_string(),
            });
        }
        self.status()
    }
}

struct ParticipantCommandSpec {
    program: PathBuf,
    args: Vec<String>,
}

fn participant_command_spec(
    room: &str,
    cfg: &ModelParticipantConfig,
) -> std::io::Result<ParticipantCommandSpec> {
    let program = std::env::var_os("ROZUM_MODEL_PARTICIPANT_EXE")
        .map(PathBuf::from)
        .map(Ok)
        .unwrap_or_else(std::env::current_exe)?;
    Ok(ParticipantCommandSpec {
        program,
        args: participant_command_args(room, cfg),
    })
}

fn participant_command_args(room: &str, cfg: &ModelParticipantConfig) -> Vec<String> {
    let mut args = vec![
        "meetings".to_string(),
        "participant".to_string(),
        "--model".to_string(),
        cfg.model.clone(),
        "--room".to_string(),
        room.to_string(),
        "--as".to_string(),
        cfg.as_handle.clone(),
        "--reply-policy".to_string(),
        cfg.reply_policy.clone(),
        "--gateway-url".to_string(),
        cfg.gateway_url.clone(),
    ];
    for peer in &cfg.peers {
        args.push("--peer".to_string());
        args.push(peer.clone());
    }
    if let Some(persona) = &cfg.persona {
        args.push("--persona".to_string());
        args.push(persona.clone());
    }
    args
}

fn model_config_from_body(b: ModelStartBody) -> Result<ModelParticipantConfig, String> {
    let model = required_trimmed(&b.model, "model")?;
    let policy = b
        .reply_policy
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("mention");
    let reply_policy = match policy.parse::<ReplyPolicy>()? {
        ReplyPolicy::Mention => "mention",
        ReplyPolicy::Always => "always",
        ReplyPolicy::Manual => "manual",
    }
    .to_string();
    let as_handle = b
        .as_handle
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| derive_handle(&model));
    let gateway_url = b
        .gateway_url
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_GATEWAY_URL)
        .to_string();
    let peers = split_peers(b.peers.as_deref().unwrap_or(""));
    let persona = b
        .persona
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Ok(ModelParticipantConfig {
        model,
        as_handle,
        reply_policy,
        gateway_url,
        peers,
        persona,
    })
}

fn required_trimmed(value: &str, name: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        Err(format!("{name} is required"))
    } else {
        Ok(value.to_string())
    }
}

fn split_peers(peers: &str) -> Vec<String> {
    peers
        .split([',', '\n'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

async fn gateway_probe(config: &Option<ModelParticipantConfig>) -> Option<GatewayProbe> {
    let cfg = config.as_ref()?;
    let url = format!("{}/models", cfg.gateway_url.trim_end_matches('/'));
    match reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_millis(800))
        .send()
        .await
    {
        Ok(resp) => Some(GatewayProbe {
            url,
            ok: resp.status().is_success(),
            status: Some(resp.status().as_u16()),
            error: None,
        }),
        Err(e) => Some(GatewayProbe {
            url,
            ok: false,
            status: None,
            error: Some(e.to_string()),
        }),
    }
}

/// HTTP Basic auth: any username, password must equal the shared secret. The browser's native
/// credential prompt is the "enter the secret code" UX; once entered it's cached for the origin
/// (so `fetch` + `EventSource` carry it automatically).
async fn auth_layer(
    State(cfg): State<AuthCfg>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let unauth = || -> Response {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"rozum meeting\"")],
            "401 Unauthorized\n",
        )
            .into_response()
    };
    let Some(h) = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return unauth();
    };
    let Some(b64) = h
        .strip_prefix("Basic ")
        .or_else(|| h.strip_prefix("basic "))
    else {
        return unauth();
    };
    let Ok(dec) = base64::engine::general_purpose::STANDARD.decode(b64.trim()) else {
        return unauth();
    };
    let pass = std::str::from_utf8(&dec)
        .ok()
        .and_then(|c| c.split_once(':').map(|(_, p)| p.to_owned()))
        .unwrap_or_default();
    if pass != cfg.secret {
        return unauth();
    }
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(model: &str) -> ModelStartBody {
        ModelStartBody {
            model: model.to_string(),
            as_handle: None,
            reply_policy: None,
            gateway_url: None,
            peers: None,
            persona: None,
        }
    }

    #[test]
    fn model_config_defaults_handle_policy_and_gateway() {
        let cfg = model_config_from_body(body("mlx-community:gpt-oss-20b-MXFP4-Q4")).unwrap();
        assert_eq!(cfg.model, "mlx-community:gpt-oss-20b-MXFP4-Q4");
        assert_eq!(cfg.as_handle, "gpt-oss");
        assert_eq!(cfg.reply_policy, "mention");
        assert_eq!(cfg.gateway_url, DEFAULT_GATEWAY_URL);
        assert!(cfg.peers.is_empty());
    }

    #[test]
    fn model_config_validates_policy_and_peers() {
        let mut b = body("qwen");
        b.reply_policy = Some("always".into());
        b.peers = Some("gpt-oss, qwen3.6\nreviewer".into());
        b.persona = Some("  review code  ".into());
        let cfg = model_config_from_body(b).unwrap();
        assert_eq!(cfg.reply_policy, "always");
        assert_eq!(cfg.peers, ["gpt-oss", "qwen3.6", "reviewer"]);
        assert_eq!(cfg.persona.as_deref(), Some("review code"));

        let mut bad = body("qwen");
        bad.reply_policy = Some("sometimes".into());
        assert!(model_config_from_body(bad).is_err());
        assert!(model_config_from_body(body(" ")).is_err());
    }

    #[test]
    fn participant_command_args_include_room_policy_peers_and_persona() {
        let cfg = ModelParticipantConfig {
            model: "m".into(),
            as_handle: "h".into(),
            reply_policy: "manual".into(),
            gateway_url: "http://127.0.0.1:8080/v1".into(),
            peers: vec!["p1".into(), "p2".into()],
            persona: Some("be terse".into()),
        };
        assert_eq!(
            participant_command_args("demo", &cfg),
            vec![
                "meetings",
                "participant",
                "--model",
                "m",
                "--room",
                "demo",
                "--as",
                "h",
                "--reply-policy",
                "manual",
                "--gateway-url",
                "http://127.0.0.1:8080/v1",
                "--peer",
                "p1",
                "--peer",
                "p2",
                "--persona",
                "be terse",
            ]
        );
    }
}
