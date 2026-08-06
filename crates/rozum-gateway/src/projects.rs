//! The projects the console can point an agent at: what is on disk, plus the operator's extras.
//!
//! Extracted from `control.rs` (`gw-monolith-decompose`). Eight items — read the project directory,
//! merge in the manually added ones, and the route that adds one.


use serde::Serialize;

use crate::errors::json_err;

pub(crate) fn parse_project_add_body(body: &str) -> Result<ProjectAddRequest, String> {
    let body = body.trim();
    if body.is_empty() {
        return Err("name required".into());
    }
    if body.starts_with('{') || body.starts_with('[') {
        let req: ProjectAddRequest =
            serde_json::from_str(body).map_err(|e| format!("invalid JSON body: {e}"))?;
        return Ok(req);
    }
    for (k, v) in url::form_urlencoded::parse(body.as_bytes()) {
        if k == "name" {
            return Ok(ProjectAddRequest { name: v.into_owned() });
        }
    }
    Ok(ProjectAddRequest { name: body.to_string() })
}

#[derive(Debug, Clone, Serialize)]
pub struct ProjectBrief {
    pub name: String,
    pub path: String,
}

/// List known project directories from `rooms.json` for the workdir picker. Rooms without a
/// project path, and test/worktree paths, are excluded.
pub(crate) fn ucc_config_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".rozum/ucc/config.json")
}

pub(crate) fn read_projects_dir() -> String {
    std::fs::read(ucc_config_path()).ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .and_then(|v| v.get("projects_dir").and_then(|d| d.as_str()).map(|s| s.to_string()))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join("work").to_string_lossy().to_string())
                .unwrap_or_else(|| "/tmp/projects".to_string())
        })
}

pub(crate) fn projects_extra_path() -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_default()
        .join(".rozum/ucc/projects.json")
}

pub(crate) fn list_projects() -> Vec<ProjectBrief> {
    let mut out: Vec<ProjectBrief> = Vec::new();

    // 1) rooms.json — project rooms from the meeting daemon
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")));
    if let Some(path) = base.map(|b| b.join("rozum/rooms.json")) {
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(rooms) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
                for r in &rooms {
                    let Some(name) = r.get("name").and_then(|v| v.as_str()) else { continue };
                    let Some(project) = r.get("project").and_then(|v| v.as_str()) else { continue };
                    if project.is_empty() || project.contains("/tmp/") || project.contains("/.worktrees/") {
                        continue;
                    }
                    if !out.iter().any(|p| p.path == project) {
                        out.push(ProjectBrief { name: name.to_string(), path: project.to_string() });
                    }
                }
            }
        }
    }

    // 2) ~/.rozum/ucc/projects.json — user-added projects via the UCC "создать" button
    if let Ok(bytes) = std::fs::read(projects_extra_path()) {
        if let Ok(extras) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
            for r in &extras {
                let Some(name) = r.get("name").and_then(|v| v.as_str()) else { continue };
                let Some(path) = r.get("path").and_then(|v| v.as_str()) else { continue };
                if !out.iter().any(|p| p.path == path) {
                    out.push(ProjectBrief { name: name.to_string(), path: path.to_string() });
                }
            }
        }
    }

    out
}

#[derive(serde::Deserialize)]
pub(crate) struct ProjectAddRequest {
    pub(crate) name: String,
}

pub(crate) async fn project_add_route(body: String) -> axum::response::Response {
    use axum::response::IntoResponse;
    let req = match parse_project_add_body(&body) {
        Ok(req) => req,
        Err(e) => return json_err(axum::http::StatusCode::BAD_REQUEST, &e),
    };
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "name required");
    }
    if name.contains('/') || name.contains("..") {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "name must not contain path separators");
    }
    let base = read_projects_dir();
    let path = format!("{}/{}", base.trim_end_matches('/'), name);
    let p = std::path::Path::new(&path);
    if !p.exists() {
        if let Err(e) = std::fs::create_dir_all(p) {
            return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("mkdir: {e}"));
        }
    } else if !p.is_dir() {
        return json_err(axum::http::StatusCode::BAD_REQUEST, "path exists but is not a directory");
    }
    let extra = projects_extra_path();
    let mut projects: Vec<serde_json::Value> = if extra.exists() {
        std::fs::read(&extra).ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if !projects.iter().any(|e| e.get("path").and_then(|v| v.as_str()) == Some(path.as_str())) {
        projects.push(serde_json::json!({"name": name, "path": path}));
        if let Some(parent) = extra.parent() { let _ = std::fs::create_dir_all(parent); }
        if let Err(e) = std::fs::write(&extra, serde_json::to_vec(&projects).unwrap_or_default()) {
            return json_err(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &format!("write: {e}"));
        }
    }
    axum::Json(serde_json::json!({"ok": true})).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ucc_project_add_accepts_json_form_and_plain_body() {
        assert_eq!(parse_project_add_body(r#"{"name":"demo"}"#).unwrap().name, "demo");
        assert_eq!(parse_project_add_body("name=demo").unwrap().name, "demo");
        assert_eq!(parse_project_add_body("demo").unwrap().name, "demo");
        assert!(parse_project_add_body(r#"{"missing":"name"}"#).is_err());
        assert!(parse_project_add_body("").is_err());
    }
}
