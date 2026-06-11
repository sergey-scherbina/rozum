//! Launch-local reverse proxy: the gateway analog of `meeting::proxy` (mcp-proxy)
//! for rooms.
//!
//! Each `rozum launch` runs a tiny **model-free** HTTP reverse proxy on a local
//! ephemeral port and points the agent's `ANTHROPIC_BASE_URL` / `OPENAI_BASE_URL`
//! at it; the proxy forwards every request to the shared daemon's stable port and
//! streams the response back unchanged. There is no model in the proxy — it is a
//! transparent byte pipe.
//!
//! Why a proxy in the path at all: it is the only place rozum controls the
//! request lifecycle end-to-end, which is what lets later phases add transparent
//! replay across a daemon crash, soft poison-prompt handling, smart retry, and
//! "hold the request across a model swap" — none of which are possible if the
//! agent talks to the daemon directly (we'd be at the mercy of the agent's own
//! retry behaviour). This phase (`shared-gateway-proxy`) establishes the
//! transparent pass-through; replay / poison / two-tier backpressure land on top.
//!
//! Spec: `docs/specs/shared-gateway.md`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt as _;

use crate::concurrency::{AdmissionConfig, AdmissionScheduler, AdmitGuard, RequestCost};

/// Max request body we buffer before forwarding (agent chat requests are JSON,
/// well under this). Buffering is also what makes a request replayable later.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Smart-retry policy for the replay path. Reconnect/replay uses capped
/// exponential backoff + jitter, waits for daemon health before re-firing, and
/// caps attempts per request — so a crowd of proxies reconnecting after a crash
/// doesn't stampede the fresh daemon. (`mcp-proxy` backoff idiom.)
#[derive(Clone)]
struct RetryPolicy {
    /// Total sends per request (1 = no retry). After the cap, surface a 502.
    max_attempts: u32,
    /// First backoff step; doubles each attempt up to `cap`.
    base: Duration,
    /// Backoff ceiling.
    cap: Duration,
    /// Upper bound on how long we wait for the daemon to become healthy again
    /// (re-election respawns it on the same stable port) before re-firing anyway.
    health_wait: Duration,
}

impl RetryPolicy {
    /// `ROZUM_PROXY_MAX_ATTEMPTS` (default 6, min 1), `ROZUM_PROXY_BACKOFF_MS`
    /// (default 150), `ROZUM_PROXY_HEALTH_WAIT_SECS` (default 60).
    fn from_env() -> Self {
        let env_u64 = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
        RetryPolicy {
            max_attempts: env_u64("ROZUM_PROXY_MAX_ATTEMPTS").unwrap_or(6).max(1) as u32,
            base: Duration::from_millis(env_u64("ROZUM_PROXY_BACKOFF_MS").unwrap_or(150)),
            cap: Duration::from_secs(5),
            health_wait: Duration::from_secs(env_u64("ROZUM_PROXY_HEALTH_WAIT_SECS").unwrap_or(60)),
        }
    }

    /// Exponential backoff for `attempt` (1-based) with ±50% jitter, capped.
    fn backoff(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        jitter(self.base.saturating_mul(1u32 << shift).min(self.cap))
    }
}

/// Scale `d` by a pseudo-random factor in `[0.5, 1.5)` without pulling in `rand`:
/// derive entropy from the wall clock's sub-second nanos. Jitter desynchronizes a
/// crowd of proxies retrying at the same instant after a shared daemon crash.
fn jitter(d: Duration) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|t| t.subsec_nanos())
        .unwrap_or(0);
    let frac = (nanos % 1000) as f64 / 1000.0; // [0,1)
    d.mul_f64(0.5 + frac) // [0.5,1.5)
}

#[derive(Clone)]
pub struct ProxyState {
    client: reqwest::Client,
    /// The shared daemon's stable port. Held in an atomic so a future phase can
    /// re-point the proxy at a respawned daemon without rebuilding the router.
    daemon_port: Arc<AtomicU16>,
    retry: RetryPolicy,
    /// Tier-2 of the two-tier admission: this proxy's *own* scheduler over the
    /// single agent's (possibly parallel) requests — shortest-job-first + a
    /// reserved fast lane, so a quick follow-up can overtake a big context read
    /// queued at the edge. The daemon's scheduler is the global tier-1 authority
    /// (`/v1/admit`); this just orders + caps what this client pushes.
    admit: AdmissionScheduler,
    /// Soft poison handling. `poison` is this proxy's per-fingerprint count of
    /// crash-attributed attempts (survives across separate requests, so a re-sent
    /// crashing turn keeps escalating); `poison_max` is the refuse threshold;
    /// `poison_ttl_secs` is how long a *shared* confirmation stays advisory.
    poison: Arc<Mutex<HashMap<u64, u32>>>,
    poison_max: u32,
    poison_ttl_secs: u64,
    /// Degrade-then-retry lane: a normal forward holds it shared (read) across
    /// its prefill, but a crash-attributed retry holds it exclusive (write) so the
    /// risky prefill runs serialized — no neighbour competes for memory, which
    /// clears most big-prompt OOMs.
    lane: Arc<tokio::sync::RwLock<()>>,
}

impl ProxyState {
    pub fn new(daemon_port: u16) -> Self {
        ProxyState {
            // No timeout: generations can stream for minutes. Per-request cancel
            // comes from the client disconnect propagating through the body.
            client: reqwest::Client::builder()
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            daemon_port: Arc::new(AtomicU16::new(daemon_port)),
            retry: RetryPolicy::from_env(),
            admit: AdmissionScheduler::new(proxy_admit_config()),
            poison: Arc::new(Mutex::new(HashMap::new())),
            poison_max: poison_max_from_env(),
            poison_ttl_secs: poison_ttl_from_env(),
            lane: Arc::new(tokio::sync::RwLock::new(())),
        }
    }

    pub fn daemon_port(&self) -> u16 {
        self.daemon_port.load(Ordering::Relaxed)
    }

    /// Crash-attributed attempts recorded for `fp` so far.
    fn poison_count(&self, fp: u64) -> u32 {
        self.poison
            .lock()
            .map(|m| *m.get(&fp).unwrap_or(&0))
            .unwrap_or(0)
    }

    /// Record one more crash-attributed attempt for `fp`; returns the new total.
    fn bump_poison(&self, fp: u64) -> u32 {
        match self.poison.lock() {
            Ok(mut m) => {
                let c = m.entry(fp).or_insert(0);
                *c += 1;
                *c
            }
            Err(_) => 0,
        }
    }

    /// Decay `fp` locally on a clean success.
    fn clear_poison(&self, fp: u64) {
        if let Ok(mut m) = self.poison.lock() {
            m.remove(&fp);
        }
    }
}

/// `ROZUM_POISON_MAX` (default 3, min 1): crash-attributed attempts before a
/// fingerprint is refused with a soft 422.
fn poison_max_from_env() -> u32 {
    std::env::var("ROZUM_POISON_MAX")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3)
        .max(1)
}

/// `ROZUM_POISON_TTL_SECS` (default 3600): how long a shared poison confirmation
/// stays advisory before it expires.
fn poison_ttl_from_env() -> u64 {
    std::env::var("ROZUM_POISON_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(crate::share::POISON_TTL_SECS)
}

/// Local-scheduler config for a launch's proxy. `ROZUM_PROXY_ADMIT` caps how many
/// of the agent's requests this proxy forwards concurrently (default 4);
/// `ROZUM_PROXY_FASTLANE_TOKENS` reserves a slot for small requests (default
/// 1024, `0` off). The queue is unbounded — a proxy never sheds its *own* client;
/// it holds and orders. The global limit is enforced by the daemon.
fn proxy_admit_config() -> AdmissionConfig {
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
    AdmissionConfig {
        limit: env_usize("ROZUM_PROXY_ADMIT").unwrap_or(4).max(1),
        fastlane_tokens: env_usize("ROZUM_PROXY_FASTLANE_TOKENS").unwrap_or(1024),
        queue_max: 0,
    }
}

/// Cheap per-request cost for the proxy's SJF ordering, taken straight from the
/// buffered body size (~4 chars/token) — no JSON parse. `max_tokens` is left at 0;
/// the body length alone separates a quick follow-up from a big context read,
/// which is all the fast lane needs.
fn estimate_cost(body: &[u8]) -> RequestCost {
    RequestCost {
        prompt_tokens: body.len() / 4,
        max_tokens: 0,
    }
}

/// Serve the reverse proxy on `listener`, forwarding to `daemon_port`. Runs until
/// the process exits (it dies with the launch, like the in-process gateway did).
pub async fn serve(listener: tokio::net::TcpListener, daemon_port: u16) -> std::io::Result<()> {
    serve_with(listener, ProxyState::new(daemon_port)).await
}

async fn serve_with(listener: tokio::net::TcpListener, state: ProxyState) -> std::io::Result<()> {
    axum::serve(listener, router(state))
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

fn router(state: ProxyState) -> Router {
    Router::new().fallback(forward).with_state(state)
}

/// Forward one request to the daemon and stream the response back verbatim, with
/// **replay before the first token**: while no response byte has reached the agent
/// yet (we haven't returned a `Response`), a connection failure is safe to replay
/// — we wait for the daemon to come back (re-election respawns it on the same
/// port) and re-send the buffered body. Once we hand back a `Response`, status +
/// headers are committed to the agent, so a mid-stream death surfaces an error
/// rather than being silently retried (we can't un-send tokens).
async fn forward(State(state): State<ProxyState>, req: Request) -> Response {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let req_headers = req.headers().clone();

    // Buffer the body once: it is the seed we replay on each attempt (Bytes is
    // cheap to clone — refcounted).
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&format!("failed reading request body: {e}")),
    };

    let port = state.daemon_port();
    let url = format!("http://127.0.0.1:{port}{path_and_query}");
    let policy = &state.retry;
    let ttl = state.poison_ttl_secs;

    // Soft poison gate (graduated, never a hard block):
    // 1. A confirmed machine-wide crasher: refuse *before forwarding* so we don't
    //    re-kill the shared daemon (and the other clients behind it) — this is the
    //    protection that survives the very crash it guards against.
    let fp = crate::share::fingerprint(&body_bytes);
    if crate::share::is_poisoned(fp, ttl) {
        return poisoned(fp, "previously crashed the shared model (machine-wide)");
    }
    // 2. This proxy already exhausted its graduated retries for this fingerprint
    //    on an earlier turn — keep refusing until it decays.
    if state.poison_count(fp) >= state.poison_max {
        return poisoned(fp, "keeps crashing the model");
    }

    // Tier-2: order this request among the agent's own in-flight requests (SJF +
    // fast lane). `queue_max = 0`, so this never sheds — at worst it parks until a
    // local slot frees. Held for the whole request (passed into the stream below).
    let admit_guard = state.admit.admit(estimate_cost(&body_bytes)).await.ok();
    // Tier-1: don't fire into a daemon that has no room — wait at the edge until
    // it advertises a free slot (bounded; fail-open).
    wait_for_window(&state, port).await;

    let mut attempt: u32 = 0;
    let mut crashes: u32 = 0; // crash-attributed attempts this turn
    loop {
        attempt += 1;
        // Degrade-then-retry: once this turn has crashed the daemon, serialize the
        // prefill (exclusive lane) so nothing else competes for memory; otherwise
        // a normal shared (read) lane lets concurrent prefills proceed.
        let pending = if crashes > 0 {
            let _lane = state.lane.write().await;
            send_once(&state, &method, &url, &req_headers, &body_bytes).await
        } else {
            let _lane = state.lane.read().await;
            send_once(&state, &method, &url, &req_headers, &body_bytes).await
        };
        match pending {
            // Daemon backpressure: hold the request at the proxy and retry rather
            // than bouncing a 429 back to the agent.
            Ok(resp)
                if resp.status() == StatusCode::TOO_MANY_REQUESTS
                    && attempt < policy.max_attempts =>
            {
                let wait = retry_after(&resp).unwrap_or_else(|| policy.backoff(attempt));
                tokio::time::sleep(wait).await;
                // Re-check the window before re-firing, so we don't busy-bounce.
                wait_for_window(&state, port).await;
            }
            // Headers received: from here on the response is committed to the
            // agent, so we stream it through and never replay. A clean (2xx)
            // prefill decays this fingerprint locally and machine-wide.
            Ok(resp) => {
                if resp.status().is_success() {
                    state.clear_poison(fp);
                    crate::share::clear_poison(fp);
                }
                return stream_back(resp, admit_guard);
            }
            Err(e) => {
                // Attribution: a *connect* failure means the daemon isn't
                // listening (a failover gap) — not our fault, so we just wait for
                // it to come back and replay. A failure on an *established*
                // connection means the daemon died while handling this request →
                // crash-attributed to this fingerprint.
                if !e.is_connect() {
                    crashes += 1;
                    let total = state.bump_poison(fp);
                    if total >= state.poison_max {
                        // Graduated retries (incl. the serialized degrade pass)
                        // exhausted. If this was the *sole* in-flight request, the
                        // attribution is unambiguous → confirm it machine-wide so
                        // peers and a respawned daemon fast-refuse. Ambiguous
                        // (concurrent) crashes stay local. Either way, a soft 422.
                        let (in_use, _waiting, _limit) = state.admit.stats();
                        if in_use <= 1 {
                            crate::share::record_poison(fp, ttl);
                        }
                        return poisoned(fp, "keeps crashing the model");
                    }
                }
                if attempt >= policy.max_attempts {
                    return bad_gateway(&format!(
                        "shared gateway on :{port} unreachable after {attempt} attempts: {e}"
                    ));
                }
                wait_for_health(port, attempt, policy).await;
            }
        }
    }
}

/// One forward send to the daemon. Split out so the retry loop can wrap it in the
/// degrade lane (shared on the normal path, exclusive on a crash-attributed
/// retry) without duplicating the request build.
async fn send_once(
    state: &ProxyState,
    method: &axum::http::Method,
    url: &str,
    req_headers: &HeaderMap,
    body_bytes: &axum::body::Bytes,
) -> reqwest::Result<reqwest::Response> {
    state
        .client
        .request(method.clone(), url)
        .headers(forward_request_headers(req_headers))
        .body(body_bytes.clone())
        .send()
        .await
}

/// Stream a received upstream response back to the agent verbatim (SSE token
/// streams included), dropping framing headers so hyper re-frames the body. The
/// local admission `guard` is held for the whole stream so the proxy's
/// concurrency cap reflects requests actually streaming (not just connecting); it
/// frees when the agent finishes reading or disconnects.
fn stream_back(resp: reqwest::Response, guard: Option<AdmitGuard>) -> Response {
    let status = resp.status();
    let mut response = Response::builder().status(status);
    if let Some(headers) = response.headers_mut() {
        copy_response_headers(resp.headers(), headers);
    }
    let stream = async_stream::stream! {
        let _guard = guard;
        let mut bytes = resp.bytes_stream();
        while let Some(chunk) = bytes.next().await {
            yield chunk.map_err(std::io::Error::other);
        }
    };
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| bad_gateway(&format!("failed building response: {e}")))
}

/// Tier-1 edge gate: hold until the daemon advertises a free slot via
/// `GET /v1/admit`, bounded by the health-wait budget. Fail-open on any probe
/// failure (older daemon, auth token, network) — the daemon's `429`/`Retry-After`
/// remains the backstop, so backpressure is an optimization, not a correctness
/// dependency.
async fn wait_for_window(state: &ProxyState, port: u16) {
    let deadline = Instant::now() + state.retry.health_wait;
    loop {
        match query_free_slots(&state.client, port).await {
            Some(0) => {
                if Instant::now() >= deadline {
                    return;
                }
                tokio::time::sleep(jitter(Duration::from_millis(100))).await;
            }
            _ => return, // free > 0, or unknown → fail open
        }
    }
}

/// `Some(free)` from the daemon's `/v1/admit`, or `None` if the probe didn't
/// yield a usable answer (caller fails open).
async fn query_free_slots(client: &reqwest::Client, port: u16) -> Option<usize> {
    let url = format!("http://127.0.0.1:{port}/v1/admit");
    let resp = client
        .get(&url)
        .timeout(Duration::from_secs(1))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("free").and_then(|f| f.as_u64()).map(|n| n as usize)
}

/// Back off (exp + jitter), then wait — bounded — for the daemon to answer health
/// again before the next replay, so we don't fire at a dead or still-warming
/// daemon and stampede it.
async fn wait_for_health(port: u16, attempt: u32, policy: &RetryPolicy) {
    tokio::time::sleep(policy.backoff(attempt)).await;
    let deadline = Instant::now() + policy.health_wait;
    while !crate::share::health_ok(port).await {
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Parse a `Retry-After: <seconds>` header (clamped to 30 s); `None` if absent or
/// not a plain integer (HTTP-date form is not used by our daemon).
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let secs = resp
        .headers()
        .get(header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    Some(Duration::from_secs(secs.min(30)))
}

/// Headers to send upstream: everything the agent sent except hop-by-hop and
/// framing headers that reqwest recomputes from the (re-)set body.
fn forward_request_headers(incoming: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in incoming.iter() {
        if is_hop_by_hop(name) || name == header::HOST || name == header::CONTENT_LENGTH {
            continue;
        }
        out.append(name.clone(), value.clone());
    }
    out
}

/// Copy the daemon's response headers back, dropping framing headers so hyper
/// re-frames the streamed body (an SSE stream has no fixed content-length).
fn copy_response_headers(from: &HeaderMap, to: &mut HeaderMap) {
    for (name, value) in from.iter() {
        if is_hop_by_hop(name)
            || name == header::CONTENT_LENGTH
            || name == header::TRANSFER_ENCODING
        {
            continue;
        }
        to.append(name.clone(), value.clone());
    }
}

fn is_hop_by_hop(name: &header::HeaderName) -> bool {
    matches!(
        *name,
        header::CONNECTION
            | header::PROXY_AUTHENTICATE
            | header::PROXY_AUTHORIZATION
            | header::TE
            | header::TRAILER
            | header::UPGRADE
    ) || name.as_str().eq_ignore_ascii_case("keep-alive")
}

/// Soft, retryable refusal for a poison-attributed request (HTTP 422). Never a
/// hard ban: the message tells the agent it can retry later, and the underlying
/// entry decays (TTL machine-wide, clean-success locally). `reason` explains why.
fn poisoned(fp: u64, reason: &str) -> Response {
    let msg =
        format!("request {fp:016x} {reason}; refused for now — retry later (advisory, expires)");
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            r#"{{"error":{{"type":"poison_refused","message":{}}}}}"#,
            json_string(&msg)
        ),
    )
        .into_response()
}

fn bad_gateway(msg: &str) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        [(header::CONTENT_TYPE, "application/json")],
        format!(
            r#"{{"error":{{"type":"upstream_error","message":{}}}}}"#,
            json_string(msg)
        ),
    )
        .into_response()
}

/// Minimal JSON string escaping for the error envelope (avoids pulling serde in
/// for a one-liner).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ProxyState {
        /// Test constructor with an explicit retry policy (so e2e tests don't
        /// inherit the slow production backoff / 60 s health wait or depend on
        /// process-global env).
        fn with_retry(daemon_port: u16, retry: RetryPolicy) -> Self {
            ProxyState {
                client: reqwest::Client::new(),
                daemon_port: Arc::new(AtomicU16::new(daemon_port)),
                retry,
                admit: AdmissionScheduler::new(proxy_admit_config()),
                poison: Arc::new(Mutex::new(HashMap::new())),
                poison_max: 3,
                poison_ttl_secs: 3600,
                lane: Arc::new(tokio::sync::RwLock::new(())),
            }
        }
    }

    #[test]
    fn cost_scales_with_body_size_for_fast_lane() {
        // A short follow-up is cheaper than a big context read, so the proxy's
        // fast lane (weight < 1024 tokens ≈ 4 KB) lets it overtake.
        let small = estimate_cost(b"{\"q\":\"hi\"}");
        let big = estimate_cost(&vec![b'x'; 40_000]);
        assert!(small.weight() < big.weight());
        assert!(small.weight() < 1024, "tiny body lands in the fast lane");
        assert!(big.weight() >= 1024, "big body does not");
    }

    /// One shot, no replay — for asserting the terminal 502.
    fn no_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 1,
            base: Duration::from_millis(1),
            cap: Duration::from_millis(1),
            health_wait: Duration::from_millis(0),
        }
    }

    /// Many quick attempts with short health polling — for the replay path.
    fn fast_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 30,
            base: Duration::from_millis(25),
            cap: Duration::from_millis(100),
            health_wait: Duration::from_secs(5),
        }
    }

    #[test]
    fn poison_count_bumps_and_clears() {
        let st = ProxyState::with_retry(1, no_retry());
        let fp = 0x42;
        assert_eq!(st.poison_count(fp), 0);
        assert_eq!(st.bump_poison(fp), 1);
        assert_eq!(st.bump_poison(fp), 2);
        assert_eq!(st.poison_count(fp), 2);
        st.clear_poison(fp);
        assert_eq!(st.poison_count(fp), 0, "a clean success decays the count");
    }

    /// Quick crash-attribution policy: many attempts, tiny backoff, short
    /// health-wait so the loop refuses fast once it hits `poison_max`.
    fn poison_retry() -> RetryPolicy {
        RetryPolicy {
            max_attempts: 10,
            base: Duration::from_millis(5),
            cap: Duration::from_millis(10),
            health_wait: Duration::from_millis(150),
        }
    }

    // A daemon that accepts the connection then drops it without responding —
    // i.e. it *crashes while handling the request* (an established-connection
    // failure, `is_connect() == false`). The proxy must attribute the crash to
    // this fingerprint and, after `poison_max` graduated attempts, refuse with a
    // soft, retryable 422 rather than retrying forever.
    #[tokio::test]
    async fn refuses_poison_request_after_repeated_crashes() {
        use tokio::io::AsyncReadExt as _;

        // Serialize the XDG_STATE_HOME mutation: a sole-in-flight crash confirms
        // the fingerprint to the shared poison.json, which we redirect to a temp
        // dir so we never write the real state dir (and never race another test).
        let _env = crate::share::POISON_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("rozum-proxy-poison-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: held under POISON_ENV_LOCK; no other thread reads XDG now.
        unsafe { std::env::set_var("XDG_STATE_HOME", &dir) };

        // Crasher upstream: accept, read the request, drop the socket.
        let up = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let up_port = up.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = up.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = sock.read(&mut buf).await;
                    drop(sock); // close without responding → connection reset
                });
            }
        });

        let px = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let px_port = px.local_addr().unwrap().port();
        tokio::spawn(async move {
            serve_with(px, ProxyState::with_retry(up_port, poison_retry()))
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{px_port}/v1/messages"))
            .body(r#"{"q":"crash me"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            422,
            "a repeatedly-crashing request is softly refused"
        );
        assert!(resp.text().await.unwrap().contains("poison_refused"));

        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: still under POISON_ENV_LOCK.
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
    }

    #[test]
    fn backoff_grows_and_caps_with_jitter() {
        let p = RetryPolicy {
            max_attempts: 10,
            base: Duration::from_millis(100),
            cap: Duration::from_secs(1),
            health_wait: Duration::from_secs(1),
        };
        // Jittered into [0.5,1.5)× the deterministic step.
        for attempt in 1..=3u32 {
            let step = 100u64 * (1u64 << (attempt - 1));
            let d = p.backoff(attempt).as_millis() as u64;
            assert!(
                d >= step / 2 && d < step * 3 / 2,
                "attempt {attempt}: {d}ms"
            );
        }
        // Far-out attempts are capped (jitter keeps it under 1.5× the cap).
        assert!(p.backoff(20).as_millis() < 1500);
    }

    #[test]
    fn drops_hop_by_hop_and_framing_request_headers() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "example".parse().unwrap());
        h.insert(header::CONTENT_LENGTH, "10".parse().unwrap());
        h.insert(header::CONNECTION, "close".parse().unwrap());
        h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        h.insert(header::AUTHORIZATION, "Bearer x".parse().unwrap());
        let out = forward_request_headers(&h);
        assert!(out.get(header::HOST).is_none());
        assert!(out.get(header::CONTENT_LENGTH).is_none());
        assert!(out.get(header::CONNECTION).is_none());
        assert_eq!(out.get(header::CONTENT_TYPE).unwrap(), "application/json");
        assert_eq!(out.get(header::AUTHORIZATION).unwrap(), "Bearer x");
    }

    #[test]
    fn drops_framing_response_headers() {
        let mut from = HeaderMap::new();
        from.insert(header::CONTENT_LENGTH, "42".parse().unwrap());
        from.insert(header::TRANSFER_ENCODING, "chunked".parse().unwrap());
        from.insert(header::CONTENT_TYPE, "text/event-stream".parse().unwrap());
        let mut to = HeaderMap::new();
        copy_response_headers(&from, &mut to);
        assert!(to.get(header::CONTENT_LENGTH).is_none());
        assert!(to.get(header::TRANSFER_ENCODING).is_none());
        assert_eq!(to.get(header::CONTENT_TYPE).unwrap(), "text/event-stream");
    }

    #[test]
    fn json_string_escapes() {
        assert_eq!(json_string("a\"b\\c\nd"), r#""a\"b\\c\nd""#);
    }

    // End-to-end: stand up a real upstream, proxy to it, and assert that method,
    // path+query, body, and a custom response header all pass through unchanged.
    #[tokio::test]
    async fn forwards_request_and_response_through_to_upstream() {
        use axum::{Router, extract::Request, routing::post};

        // Upstream daemon stand-in: echoes the body and the query, sets a custom
        // header + an SSE content-type.
        async fn echo(req: Request) -> Response {
            let q = req.uri().query().unwrap_or("").to_owned();
            let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                .await
                .unwrap();
            Response::builder()
                .status(201)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header("x-rozum-upstream", "1")
                .header("x-echo-query", q)
                .body(Body::from(body))
                .unwrap()
        }
        let up = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let up_port = up.local_addr().unwrap().port();
        tokio::spawn(async move {
            let app = Router::new().route("/v1/messages", post(echo));
            axum::serve(up, app).await.unwrap();
        });

        // Proxy in front of it.
        let px = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let px_port = px.local_addr().unwrap().port();
        tokio::spawn(async move {
            serve_with(px, ProxyState::with_retry(up_port, fast_retry()))
                .await
                .unwrap();
        });
        // Let both bind.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let client = reqwest::Client::new();
        let resp = client
            .post(format!(
                "http://127.0.0.1:{px_port}/v1/messages?stream=true"
            ))
            .header(header::CONTENT_TYPE, "application/json")
            .body(r#"{"hello":"world"}"#)
            .send()
            .await
            .unwrap();

        assert_eq!(resp.status(), 201);
        assert_eq!(resp.headers().get("x-rozum-upstream").unwrap(), "1");
        assert_eq!(resp.headers().get("x-echo-query").unwrap(), "stream=true");
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        assert_eq!(resp.text().await.unwrap(), r#"{"hello":"world"}"#);
    }

    // A dead daemon yields a clean 502 (the surface a later phase replaces with
    // replay-before-first-token).
    #[tokio::test]
    async fn dead_daemon_yields_502() {
        // Pick a port nothing is listening on.
        let dead = {
            let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };
        let px = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let px_port = px.local_addr().unwrap().port();
        tokio::spawn(async move {
            serve_with(px, ProxyState::with_retry(dead, no_retry()))
                .await
                .unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{px_port}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
        assert!(resp.text().await.unwrap().contains("upstream_error"));
    }

    // Replay-before-first-token: the daemon is down when the request arrives and
    // comes up shortly after (re-election). The proxy must wait + replay the
    // buffered body transparently — the client sees a slower 200, not an error.
    #[tokio::test]
    async fn replays_request_after_daemon_comes_back() {
        use axum::routing::{get, post};

        // Reserve then free a port: the daemon is "down" when the request lands.
        let port = {
            let l = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let p = l.local_addr().unwrap().port();
            drop(l);
            p
        };

        let px = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let px_port = px.local_addr().unwrap().port();
        tokio::spawn(async move {
            serve_with(px, ProxyState::with_retry(port, fast_retry()))
                .await
                .unwrap();
        });

        // Bring the daemon up on the same port a beat later (it answers the
        // health probe on /v1/models and echoes the body on /v1/messages).
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            async fn echo(req: Request) -> Response {
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (StatusCode::OK, body).into_response()
            }
            let up = tokio::net::TcpListener::bind(("127.0.0.1", port))
                .await
                .unwrap();
            let app = Router::new()
                .route("/v1/models", get(|| async { (StatusCode::OK, "{}") }))
                .route("/v1/messages", post(echo));
            axum::serve(up, app).await.unwrap();
        });

        // Fire immediately, while the daemon is still down.
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{px_port}/v1/messages"))
            .body(r#"{"q":"replay me"}"#)
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "request should replay through to upstream"
        );
        assert_eq!(resp.text().await.unwrap(), r#"{"q":"replay me"}"#);
    }

    // Tier-1 edge gate: the daemon reports no free slots at first, then opens up.
    // The proxy must poll `/v1/admit` and hold the request until there is room,
    // rather than firing into a full daemon.
    #[tokio::test]
    async fn holds_at_edge_until_daemon_advertises_room() {
        use axum::routing::{get, post};
        use std::sync::atomic::AtomicUsize;

        let probes = Arc::new(AtomicUsize::new(0));
        let up = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let up_port = up.local_addr().unwrap().port();
        let probes_for_handler = Arc::clone(&probes);
        tokio::spawn(async move {
            async fn echo(req: Request) -> Response {
                let body = axum::body::to_bytes(req.into_body(), 1 << 20)
                    .await
                    .unwrap();
                (StatusCode::OK, body).into_response()
            }
            let app = Router::new()
                .route(
                    "/v1/admit",
                    get(move || {
                        let p = Arc::clone(&probes_for_handler);
                        async move {
                            // Free only from the 3rd probe onward.
                            let free = usize::from(p.fetch_add(1, Ordering::SeqCst) >= 2);
                            axum::Json(serde_json::json!({ "free": free }))
                        }
                    }),
                )
                .route("/v1/messages", post(echo));
            axum::serve(up, app).await.unwrap();
        });

        let px = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let px_port = px.local_addr().unwrap().port();
        tokio::spawn(async move {
            serve_with(px, ProxyState::with_retry(up_port, fast_retry()))
                .await
                .unwrap();
        });
        tokio::time::sleep(Duration::from_millis(100)).await;

        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{px_port}/v1/messages"))
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert!(
            probes.load(Ordering::SeqCst) >= 3,
            "proxy should poll the window until the daemon opens it (got {})",
            probes.load(Ordering::SeqCst)
        );
    }
}
