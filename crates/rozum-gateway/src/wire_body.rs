//! Reading the two request-body shapes the control routes accept.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). The console posts either JSON or an
//! HTML form body, and every route that takes an id or an action had to cope with both; these are
//! the two functions that do it in one place.

use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Deserialize)]
pub(crate) struct IdBody {
    pub(crate) id: String,
}

pub(crate) fn parse_id_body(body: &str) -> Result<String, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("id required".into());
    }
    if body.starts_with('{') || body.starts_with('[') {
        let req: IdBody =
            serde_json::from_str(body).map_err(|e| format!("invalid JSON body: {e}"))?;
        let id = req.id.trim().to_string();
        if id.is_empty() {
            return Err("id required".into());
        }
        return Ok(id);
    }
    for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
        if k == "id" {
            let id = v.trim().to_string();
            if id.is_empty() {
                return Err("id required".into());
            }
            return Ok(id);
        }
    }
    Ok(body.to_string())
}

pub(crate) fn parse_action_json<T: DeserializeOwned>(body: &str) -> Result<T, String> {
    serde_json::from_str::<T>(body.trim()).map_err(|e| format!("invalid JSON body: {e}"))
}

/// Parse a request body that may be JSON or form-urlencoded (the `.ssc` `formBody` posts the
/// latter) into flat string fields. JSON numbers/bools are stringified so `chat_id` works either
/// way — the SPA sends `"-1004378341901"`, a curl user sends `-1004378341901`.
pub(crate) fn parse_flat_body(body: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let body = body.trim();
    if body.starts_with('{') {
        if let Ok(serde_json::Value::Object(map)) = serde_json::from_str(body) {
            for (k, v) in map {
                let s = match v {
                    serde_json::Value::String(s) => s,
                    other => other.to_string(),
                };
                out.insert(k, s);
            }
            return out;
        }
    }
    for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
        out.insert(k.into_owned(), v.into_owned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;



    #[test]
    fn ucc_stop_id_accepts_json_form_and_legacy_plain_body() {
        assert_eq!(parse_id_body(r#"{"id":"claude-123"}"#).unwrap(), "claude-123");
        assert_eq!(parse_id_body("id=claude-123").unwrap(), "claude-123");
        assert_eq!(parse_id_body("claude-123").unwrap(), "claude-123");
        assert!(parse_id_body(r#"{"missing":"id"}"#).is_err());
        assert!(parse_id_body("").is_err());
    }

}
