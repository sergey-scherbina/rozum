//! HTTP error responses, in the shape each dialect expects.
//!
//! Extracted from `gateway.rs` (`gw-monolith-decompose`). These two functions were already used by
//! four modules — `oai_api`, `anthropic_api`, `responses_api` and `gateway` itself — so they were a
//! shared utility living inside the largest file in the crate, reachable only by importing that
//! file. `gateway` re-exports them, so every call site reads exactly as it did.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rozum_core::backend::ModelError;
use serde_json::json;

pub(crate) fn error_json(status: StatusCode, msg: &str, err_type: &str) -> Response {
    let body = json!({ "error": { "message": msg, "type": err_type } });
    (status, axum::Json(body)).into_response()
}

/// Map a backend `chat()` error to an HTTP response. Overload sheds with 429 +
/// `Retry-After` so clients back off; everything else is a 500 with the dialect's
/// own error type (`backend_error` for OpenAI, `api_error` for Anthropic).
pub(crate) fn chat_error_response(e: &ModelError, fallback_type: &str) -> Response {
    match e {
        ModelError::Overloaded(msg) => {
            let mut resp = error_json(StatusCode::TOO_MANY_REQUESTS, msg, "overloaded");
            resp.headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
            resp
        }
        ModelError::Timeout(msg) => error_json(StatusCode::GATEWAY_TIMEOUT, msg, fallback_type),
        _ => error_json(
            StatusCode::INTERNAL_SERVER_ERROR,
            &e.to_string(),
            fallback_type,
        ),
    }
}
