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
