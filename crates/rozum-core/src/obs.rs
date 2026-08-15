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

/// Is this process a `cargo test` binary?
///
/// Cargo builds test executables into `target/<profile>/deps/` and nothing else this workspace
/// ships lives there: the gateway runs from `target/release/rozum-gateway`, `~/.cargo/bin`, or
/// `~/.rozum/bin`. The check is a heuristic, and it is deliberately biased toward the safe side —
/// a mistake here loses a log line from an unusual debug run, while the mistake it prevents writes
/// fabricated events into the operator's evidence (BUG-039).
///
/// An explicit `ROZUM_GATEWAY_LOG` still wins, so a test that WANTS to assert on the log can point
/// it at a temp file.
fn is_test_binary() -> bool {
    std::env::current_exe().ok().is_some_and(|p| exe_is_test(&p))
}

/// The rule, as a pure function of the path, so both directions are testable — the one that must
/// say "test" and the one that must NOT, which a test running inside a test binary cannot check
/// about itself.
fn exe_is_test(exe: &std::path::Path) -> bool {
    exe.parent().is_some_and(|d| d.ends_with("deps"))
}

fn log_path() -> Option<&'static PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let p = match std::env::var("ROZUM_GATEWAY_LOG") {
            Ok(v) if v.is_empty() => return None,
            Ok(v) => PathBuf::from(v),
            // No explicit destination AND we are a test binary: write nothing. Measured
            // 2026-08-14 before this existed — `cargo test -p rozum-gateway --lib` appended 5,303
            // bytes of `request_done` and `generation_timeout` events to the operator's live
            // `~/.rozum/gateway.jsonl`, the same file `meeting::store::gateway_log_slice` cuts
            // incident evidence out of. A test that manufactures evidence is worse than a test
            // that logs nothing.
            Err(_) if is_test_binary() => return None,
            Err(_) => {
                // Same rule as the reader: `meeting::store::gateway_log_slice` opens this file
                // to cut an incident's evidence out of it, so writer and reader must resolve the
                // same home or the evidence is silently empty (`rozum_paths`).
                let dir = rozum_paths::home_dir()?.join(".rozum");
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
    /// What the SAMPLER was told, after the process defaults and the request's own decode policy:
    /// `"greedy"` (temperature 0 / argmax) or `"sampled"`. Logged because the alternative is
    /// arguing about it: the matrix spent eight days believing it ran greedy while a borrowed
    /// gateway sampled at the client's temperature, and nothing in any log could have said so.
    pub decode: &'static str,
    /// The RNG seed the request ended up with, when it has one.
    pub seed: Option<u64>,
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
            "decode": meta.decode,
            "seed": meta.seed,
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

    // The SSE encoder stops polling at the `Done` event and drops this stream
    // without draining it, so any code after the `while` loop never runs. Put
    // the finalization in a Drop guard so request_done fires exactly once on
    // normal completion AND on early drop (client disconnect / cancellation).
    struct FinishGuard {
        obs: Arc<Observer>,
        id: u64,
        start: Instant,
        out_tokens: u32,
        stop: &'static str,
        had_tool: bool,
    }
    impl Drop for FinishGuard {
        fn drop(&mut self) {
            self.obs.request_finish(
                self.id,
                self.start.elapsed().as_millis() as u64,
                self.out_tokens,
                self.stop,
                self.had_tool,
            );
        }
    }

    Box::pin(async_stream::stream! {
        let start = Instant::now();
        let id = obs.request_start(&meta);
        let mut guard = FinishGuard {
            obs: obs.clone(),
            id,
            start,
            out_tokens: 0,
            stop: "incomplete",
            had_tool: false,
        };
        let mut ttft_seen = false;
        let mut inner = inner;
        while let Some(ev) = inner.next().await {
            if let Ok(e) = &ev {
                match e {
                    ChatEvent::TextDelta { .. } => {
                        if !ttft_seen {
                            ttft_seen = true;
                            obs.first_token(id, start.elapsed().as_millis() as u64);
                        }
                        guard.out_tokens += 1;
                        obs.bump_token(id);
                    }
                    ChatEvent::ToolUseStart { .. } => {
                        guard.had_tool = true;
                        if !ttft_seen {
                            ttft_seen = true;
                            obs.first_token(id, start.elapsed().as_millis() as u64);
                        }
                    }
                    ChatEvent::Done { output_tokens, stop_reason, .. } => {
                        if *output_tokens > 0 {
                            guard.out_tokens = *output_tokens;
                        }
                        guard.stop = match stop_reason {
                            StopReason::EndTurn => "end_turn",
                            StopReason::MaxTokens => "max_tokens",
                            StopReason::ToolUse => "tool_use",
                            StopReason::Cancelled => "cancelled",
                            StopReason::StopSequence(_) => "stop_sequence",
                        };
                    }
                    _ => {}
                }
            }
            yield ev;
        }
    })
}

// ─── Engine telemetry hooks (inversion of control) ──────────────────────────────
//
// The gateway's `/stats` reports native-MLX memory + batched-decode occupancy, but
// `rozum-core` must not depend on the `rozum-mlx` engine. So the engine registers
// its stat accessors here at startup (like the backend registry's engine hooks) and
// the gateway reads them through `rozum-core`. No registration → `None` (the engine
// is absent or its feature is off), exactly as the old `#[cfg(not)]` stubs returned.

/// Batched-decode occupancy snapshot (native MLX). `avg_occupancy = rows/runs`.
/// `serial_{seed,penalty,constrained}` count jobs that bypassed the batch path.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BatchStats {
    pub runs: u64,
    pub rows: u64,
    pub admits: u64,
    pub max: u64,
    pub serial_seed: u64,
    pub serial_penalty: u64,
    pub serial_constrained: u64,
}

static MLX_MEMORY: OnceLock<fn() -> Option<(u64, u64, u64)>> = OnceLock::new();
static MLX_BATCH: OnceLock<fn() -> Option<BatchStats>> = OnceLock::new();
static MLX_SQUEEZE: OnceLock<fn() -> u64> = OnceLock::new();

/// Register the native-MLX unified-memory accessor (active/peak/cache MB).
pub fn register_mlx_memory(f: fn() -> Option<(u64, u64, u64)>) {
    let _ = MLX_MEMORY.set(f);
}

/// Register the native-MLX cache-squeeze (frees the reclaimable Metal buffer cache, returns bytes freed).
pub fn register_mlx_squeeze_cache(f: fn() -> u64) {
    let _ = MLX_SQUEEZE.set(f);
}

/// Free the native-MLX reclaimable buffer cache (a light graceful step under memory pressure — frees GB
/// of reclaimable RAM cross-process without unloading a whole model). Returns bytes freed; 0 if the MLX
/// engine is absent or nothing was cached. Call only when the model is idle (no generation in flight).
pub fn mlx_squeeze_cache() -> u64 {
    MLX_SQUEEZE.get().map(|f| f()).unwrap_or(0)
}

/// Register the native-MLX batched-decode occupancy accessor.
pub fn register_mlx_batch_stats(f: fn() -> Option<BatchStats>) {
    let _ = MLX_BATCH.set(f);
}

/// Native-MLX unified-memory footprint (active, peak, cache) in MB, or `None` when
/// the MLX engine is absent / its feature is off.
pub fn mlx_memory_mb() -> Option<(u64, u64, u64)> {
    MLX_MEMORY.get().and_then(|f| f())
}

/// Native-MLX batched-decode occupancy, or `None` when nothing has batched yet /
/// the MLX engine is absent.
pub fn batch_stats() -> Option<BatchStats> {
    MLX_BATCH.get().and_then(|f| f())
}

#[cfg(test)]
mod obs_log_path_tests {
    use super::exe_is_test;
    use std::path::Path;

    #[test]
    fn a_cargo_test_binary_is_recognised_and_a_shipped_one_is_not() {
        assert!(exe_is_test(Path::new("/w/rozum/target/debug/deps/rozum_gateway-7b2b0fd1")));
        assert!(!exe_is_test(Path::new("/w/rozum/target/release/rozum-gateway")));
        assert!(!exe_is_test(Path::new("/Users/x/.cargo/bin/rozum-gateway")));
    }

    #[test]
    fn this_very_process_is_a_test_binary() {
        assert!(super::is_test_binary());
    }
}
