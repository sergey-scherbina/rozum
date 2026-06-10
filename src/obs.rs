//! Gateway observability: a persistent JSONL event log plus in-memory metrics
//! exposed at `GET /stats`.
//!
//! Motivation: `rozum launch <agent>` runs the gateway in-process and the
//! agent's TUI owns the terminal, so any `eprintln!` from the gateway is
//! invisible (and lost). Without this, you cannot tell which backend was
//! selected, whether the model loaded, or whether a request is prefilling,
//! decoding, or stuck. Everything here writes to a file (default
//! `~/.rozum/gateway.jsonl`, override with `ROZUM_GATEWAY_LOG`) and to a small
//! in-memory ring buffer, never to the agent's terminal.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use serde_json::{Value, json};

const RECENT_CAP: usize = 30;

fn log_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let p = match std::env::var("ROZUM_GATEWAY_LOG") {
            Ok(v) if v.is_empty() => return None,
            Ok(v) => PathBuf::from(v),
            Err(_) => {
                let home = std::env::var("HOME").ok()?;
                let dir = PathBuf::from(home).join(".rozum");
                let _ = std::fs::create_dir_all(&dir);
                dir.join("gateway.jsonl")
            }
        };
        Some(p)
    })
    .as_ref()
}

/// Append one structured event to the JSONL log (best-effort; a `ts` field is
/// added automatically). Usable before any `Observer` exists, e.g. during
/// backend selection at launch.
pub fn log_event(mut event: Value) {
    let Some(path) = log_path() else { return };
    if let Value::Object(ref mut map) = event {
        map.insert("ts".into(), json!(now_rfc3339()));
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{event}");
    }
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Metadata captured when a request arrives.
#[derive(Clone)]
pub struct ReqMeta {
    pub endpoint: &'static str,
    pub model: String,
    pub n_messages: usize,
    pub n_tools: usize,
    pub est_prompt_tokens: u32,
}

struct Active {
    id: u64,
    endpoint: &'static str,
    started: Instant,
    est_prompt_tokens: u32,
    first_token_ms: Option<u64>,
    output_tokens: u32,
}

#[derive(Clone, serde::Serialize)]
struct Summary {
    id: u64,
    endpoint: &'static str,
    est_prompt_tokens: u32,
    ttft_ms: Option<u64>,
    output_tokens: u32,
    duration_ms: u64,
    tokens_per_sec: f64,
    stop_reason: String,
    had_tool_use: bool,
}

#[derive(Default)]
struct Inner {
    total_requests: u64,
    total_output_tokens: u64,
    active: Vec<Active>,
    recent: VecDeque<Summary>,
}

pub struct Observer {
    backend_label: Mutex<String>,
    next_id: AtomicU64,
    inner: Mutex<Inner>,
    started: Instant,
}

impl Observer {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            backend_label: Mutex::new("unknown".into()),
            next_id: AtomicU64::new(1),
            inner: Mutex::new(Inner::default()),
            started: Instant::now(),
        })
    }

    pub fn set_backend_label(&self, label: impl Into<String>) {
        *self.backend_label.lock().unwrap() = label.into();
    }

    fn request_start(&self, meta: &ReqMeta) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut inner = self.inner.lock().unwrap();
            inner.total_requests += 1;
            inner.active.push(Active {
                id,
                endpoint: meta.endpoint,
                started: Instant::now(),
                est_prompt_tokens: meta.est_prompt_tokens,
                first_token_ms: None,
                output_tokens: 0,
            });
        }
        log_event(json!({
            "event": "request_start",
            "id": id,
            "endpoint": meta.endpoint,
            "model": meta.model,
            "messages": meta.n_messages,
            "tools": meta.n_tools,
            "est_prompt_tokens": meta.est_prompt_tokens,
        }));
        id
    }

    fn first_token(&self, id: u64, ttft_ms: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(a) = inner.active.iter_mut().find(|a| a.id == id) {
            a.first_token_ms = Some(ttft_ms);
        }
    }

    fn bump_token(&self, id: u64) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(a) = inner.active.iter_mut().find(|a| a.id == id) {
            a.output_tokens += 1;
        }
    }

    fn request_finish(
        &self,
        id: u64,
        duration_ms: u64,
        output_tokens: u32,
        stop_reason: &str,
        had_tool_use: bool,
    ) {
        let tokens_per_sec = if duration_ms > 0 {
            output_tokens as f64 * 1000.0 / duration_ms as f64
        } else {
            0.0
        };
        let mut ttft = None;
        let mut est_prompt = 0;
        let mut endpoint = "";
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(pos) = inner.active.iter().position(|a| a.id == id) {
                let a = inner.active.remove(pos);
                ttft = a.first_token_ms;
                est_prompt = a.est_prompt_tokens;
                endpoint = a.endpoint;
            }
            inner.total_output_tokens += output_tokens as u64;
            let summary = Summary {
                id,
                endpoint,
                est_prompt_tokens: est_prompt,
                ttft_ms: ttft,
                output_tokens,
                duration_ms,
                tokens_per_sec,
                stop_reason: stop_reason.to_string(),
                had_tool_use,
            };
            if inner.recent.len() == RECENT_CAP {
                inner.recent.pop_front();
            }
            inner.recent.push_back(summary);
        }
        log_event(json!({
            "event": "request_done",
            "id": id,
            "ttft_ms": ttft,
            "output_tokens": output_tokens,
            "duration_ms": duration_ms,
            "tokens_per_sec": (tokens_per_sec * 10.0).round() / 10.0,
            "stop_reason": stop_reason,
            "had_tool_use": had_tool_use,
        }));
    }

    /// Snapshot for the `GET /stats` endpoint.
    pub fn snapshot(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let active: Vec<Value> = inner
            .active
            .iter()
            .map(|a| {
                json!({
                    "id": a.id,
                    "endpoint": a.endpoint,
                    "elapsed_ms": a.started.elapsed().as_millis() as u64,
                    "est_prompt_tokens": a.est_prompt_tokens,
                    "first_token_ms": a.first_token_ms,
                    "output_tokens": a.output_tokens,
                })
            })
            .collect();
        let recent: Vec<&Summary> = inner.recent.iter().rev().collect();
        json!({
            "backend": *self.backend_label.lock().unwrap(),
            "uptime_s": self.started.elapsed().as_secs(),
            "total_requests": inner.total_requests,
            "total_output_tokens": inner.total_output_tokens,
            "active_requests": active,
            "recent": recent,
        })
    }
}

/// Wrap a backend stream so every event is recorded (TTFT, token count,
/// duration, stop reason, tool use) as it passes through to the SSE encoder.
/// Transparent: yields exactly the same items in the same order.
pub fn meter(
    inner: crate::backend::ChatStream,
    obs: Arc<Observer>,
    meta: ReqMeta,
) -> crate::backend::ChatStream {
    use crate::backend::{ChatEvent, StopReason};
    use futures_util::StreamExt as _;
    Box::pin(async_stream::stream! {
        let start = Instant::now();
        let id = obs.request_start(&meta);
        let mut ttft_seen = false;
        let mut out_tokens: u32 = 0;
        let mut had_tool = false;
        let mut stop = "incomplete".to_string();
        let mut inner = inner;
        while let Some(ev) = inner.next().await {
            if let Ok(e) = &ev {
                match e {
                    ChatEvent::TextDelta { .. } => {
                        if !ttft_seen {
                            ttft_seen = true;
                            obs.first_token(id, start.elapsed().as_millis() as u64);
                        }
                        out_tokens += 1;
                        obs.bump_token(id);
                    }
                    ChatEvent::ToolUseStart { .. } => {
                        had_tool = true;
                        if !ttft_seen {
                            ttft_seen = true;
                            obs.first_token(id, start.elapsed().as_millis() as u64);
                        }
                    }
                    ChatEvent::Done { output_tokens, stop_reason, .. } => {
                        if *output_tokens > 0 {
                            out_tokens = *output_tokens;
                        }
                        stop = match stop_reason {
                            StopReason::EndTurn => "end_turn",
                            StopReason::MaxTokens => "max_tokens",
                            StopReason::ToolUse => "tool_use",
                            StopReason::Cancelled => "cancelled",
                        }
                        .to_string();
                    }
                    _ => {}
                }
            }
            yield ev;
        }
        obs.request_finish(id, start.elapsed().as_millis() as u64, out_tokens, &stop, had_tool);
    })
}
