//! Built-in tools — a small registry of safe, generally-useful tools exposed as a
//! [`CallbackToolSource`] for the reference agent runtime (`tool-routing`). Read-only and
//! side-effect-free (`echo`, `current_time`, `list_models`): no filesystem or network writes,
//! so they're safe to hand any agent. An app composes these with its own domain tools.

use chrono::Utc;
use serde_json::{json, Value};

use crate::agent::{CallbackToolSource, ToolError};
use crate::backend::ToolDef;

/// A [`crate::agent::ToolSource`] of the safe built-in tools.
pub fn builtin_tools() -> CallbackToolSource {
    CallbackToolSource::new()
        .with_tool(echo_def(), echo)
        .with_tool(current_time_def(), current_time)
        .with_tool(list_models_def(), list_models)
}

fn tool(name: &str, description: &str, schema: Value) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), input_schema: schema }
}

fn echo_def() -> ToolDef {
    tool(
        "echo",
        "Echo back the given text. Useful for testing the tool loop.",
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        }),
    )
}

fn echo(args: Value) -> Result<Value, ToolError> {
    let text = args["text"]
        .as_str()
        .ok_or_else(|| ToolError::new("`text` (string) is required"))?;
    Ok(json!({ "echo": text }))
}

fn current_time_def() -> ToolDef {
    tool(
        "current_time",
        "The current UTC time as a unix timestamp and an ISO-8601 string.",
        json!({ "type": "object", "properties": {} }),
    )
}

fn current_time(_args: Value) -> Result<Value, ToolError> {
    let now = Utc::now();
    Ok(json!({ "unix": now.timestamp(), "iso8601": now.to_rfc3339() }))
}

fn list_models_def() -> ToolDef {
    tool(
        "list_models",
        "List the recommended model catalog and the models already installed locally.",
        json!({ "type": "object", "properties": {} }),
    )
}

fn list_models(_args: Value) -> Result<Value, ToolError> {
    let to_json = |m: &crate::models::RecommendedModel| {
        json!({
            "spec": m.spec,
            "name": m.display_name,
            "approx_size_gb": m.approx_size_gb,
            "notes": m.notes,
        })
    };
    let recommended: Vec<Value> = crate::models::RECOMMENDED.iter().map(to_json).collect();
    let extended: Vec<Value> = crate::models::EXTRA.iter().map(to_json).collect();
    let installed: Vec<Value> = crate::models::scan_all_installed()
        .iter()
        .map(|m| json!({ "spec": m.spec, "size_bytes": m.size_bytes }))
        .collect();
    Ok(json!({ "recommended": recommended, "extended": extended, "installed": installed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolSource;

    #[test]
    fn registry_exposes_the_three_tools() {
        let names: Vec<String> = builtin_tools().tools().into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"echo".to_string()));
        assert!(names.contains(&"current_time".to_string()));
        assert!(names.contains(&"list_models".to_string()));
    }

    #[tokio::test]
    async fn echo_round_trips_and_validates() {
        let tools = builtin_tools();
        let out = tools.dispatch("echo", json!({ "text": "hi" })).await.unwrap();
        assert_eq!(out["echo"], "hi");
        // Missing `text` → a recoverable ToolError, not a panic.
        let err = tools.dispatch("echo", json!({})).await.unwrap_err();
        assert!(err.0.contains("text"));
    }

    #[tokio::test]
    async fn current_time_returns_unix_and_iso() {
        let out = builtin_tools().dispatch("current_time", json!({})).await.unwrap();
        assert!(out["unix"].as_i64().unwrap() > 1_700_000_000); // after 2023
        assert!(out["iso8601"].as_str().unwrap().contains('T'));
    }

    #[tokio::test]
    async fn list_models_includes_the_catalog() {
        let out = builtin_tools().dispatch("list_models", json!({})).await.unwrap();
        let rec = out["recommended"].as_array().unwrap();
        assert!(!rec.is_empty(), "the recommended catalog is non-empty");
        assert!(rec.iter().all(|m| m["spec"].is_string()));
        assert!(out["installed"].is_array());
    }
}
