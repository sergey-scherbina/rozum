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

#[derive(Clone)]
pub struct ProxyState {
    client: reqwest::Client,
    /// The shared daemon's stable port. Held in an atomic so a future phase can
    /// re-point the proxy at a respawned daemon without rebuilding the router.
    daemon_port: Arc<AtomicU16>,
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
        }
    }

    pub fn daemon_port(&self) -> u16 {
        self.daemon_port.load(Ordering::Relaxed)
    }
}

/// Serve the reverse proxy on `listener`, forwarding to `daemon_port`. Runs until
/// the process exits (it dies with the launch, like the in-process gateway did).
pub async fn serve(listener: tokio::net::TcpListener, daemon_port: u16) -> std::io::Result<()> {
    let state = ProxyState::new(daemon_port);
    let app = Router::new().fallback(forward).with_state(state);
    axum::serve(listener, app)
        .await
        .map_err(|e| std::io::Error::other(e.to_string()))
}

/// Forward one request to the daemon and stream the response back verbatim.
async fn forward(State(state): State<ProxyState>, req: Request) -> Response {
    let method = req.method().clone();
    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_owned())
        .unwrap_or_else(|| req.uri().path().to_owned());
    let req_headers = req.headers().clone();

    // Buffer the body (small JSON; also the seed for future replay).
    let body_bytes = match axum::body::to_bytes(req.into_body(), MAX_BODY_BYTES).await {
        Ok(b) => b,
        Err(e) => return bad_gateway(&format!("failed reading request body: {e}")),
    };

    let port = state.daemon_port();
    let url = format!("http://127.0.0.1:{port}{path_and_query}");

    let mut upstream = state.client.request(method, &url);
    upstream = upstream.headers(forward_request_headers(&req_headers));
    upstream = upstream.body(body_bytes);

    let resp = match upstream.send().await {
        Ok(r) => r,
        Err(e) => {
            // Daemon unreachable. A later phase replays here when no byte has been
            // forwarded yet; for now surface a clean 502 the agent can retry.
            return bad_gateway(&format!("shared gateway not reachable on :{port}: {e}"));
        }
    };

    let status = resp.status();
    let mut response = Response::builder().status(status);
    if let Some(headers) = response.headers_mut() {
        copy_response_headers(resp.headers(), headers);
    }

    // Stream the body through unchanged (SSE token streams included).
    let stream = resp
        .bytes_stream()
        .map(|chunk| chunk.map_err(std::io::Error::other));
    response
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| bad_gateway(&format!("failed building response: {e}")))
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
            serve(px, up_port).await.unwrap();
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
            serve(px, dead).await.unwrap();
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
}
