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

use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::{Duration, Instant};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures::StreamExt as _;

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
        }
    }

    pub fn daemon_port(&self) -> u16 {
        self.daemon_port.load(Ordering::Relaxed)
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

    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        let pending = state
            .client
            .request(method.clone(), &url)
            .headers(forward_request_headers(&req_headers))
            .body(body_bytes.clone())
            .send()
            .await;
        match pending {
            // Daemon backpressure: hold the request at the proxy and retry rather
            // than bouncing a 429 back to the agent.
            Ok(resp)
                if resp.status() == StatusCode::TOO_MANY_REQUESTS
                    && attempt < policy.max_attempts =>
            {
                let wait = retry_after(&resp).unwrap_or_else(|| policy.backoff(attempt));
                tokio::time::sleep(wait).await;
            }
            // Headers received: from here on the response is committed to the
            // agent, so we stream it through and never replay.
            Ok(resp) => return stream_back(resp),
            // Connection failed before any byte reached the agent → safe to
            // replay. After the attempt cap, surface a clean 502.
            Err(e) => {
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

/// Stream a received upstream response back to the agent verbatim (SSE token
/// streams included), dropping framing headers so hyper re-frames the body.
fn stream_back(resp: reqwest::Response) -> Response {
    let status = resp.status();
    let mut response = Response::builder().status(status);
    if let Some(headers) = response.headers_mut() {
        copy_response_headers(resp.headers(), headers);
    }
    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| bad_gateway(&format!("failed building response: {e}")))
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
            }
        }
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
}
