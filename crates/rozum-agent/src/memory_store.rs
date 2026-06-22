//! Local memory store (`memory-store`): an **append-only** key→value log with retrieval by
//! exact key, persisted as JSONL. Last-write-wins for `get`; the full per-key history is kept
//! for `all`. Exposed to the reference agent runtime as `remember` / `recall` tools so a small
//! local agent has durable memory across turns/sessions.
//!
//! Deliberately simple: exact-key retrieval, no embeddings/ranking (that's `rag-lite`). One
//! JSONL file, one `{key, value, ts}` record per line; appends never rewrite history.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::agent::{CallbackToolSource, ToolError};
use crate::backend::ToolDef;

pub struct MemoryStore {
    /// Backing JSONL file; empty = in-memory only (no persistence).
    path: PathBuf,
    /// key → append-only history of values (oldest first).
    index: Mutex<BTreeMap<String, Vec<Value>>>,
}

impl MemoryStore {
    /// Open (or create) a store backed by a JSONL file, replaying any existing records.
    pub fn open(path: impl Into<PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        let mut index: BTreeMap<String, Vec<Value>> = BTreeMap::new();
        if path.exists() {
            for line in BufReader::new(File::open(&path)?).lines() {
                let line = line?;
                let t = line.trim();
                if t.is_empty() {
                    continue;
                }
                // Skip a corrupt line rather than failing the whole open.
                if let Ok(rec) = serde_json::from_str::<Value>(t) {
                    if let Some(key) = rec.get("key").and_then(Value::as_str) {
                        let value = rec.get("value").cloned().unwrap_or(Value::Null);
                        index.entry(key.to_string()).or_default().push(value);
                    }
                }
            }
        }
        Ok(Self { path, index: Mutex::new(index) })
    }

    /// An ephemeral store (no file). Handy for tests / scratch agents.
    pub fn in_memory() -> Self {
        Self { path: PathBuf::new(), index: Mutex::new(BTreeMap::new()) }
    }

    /// Append a fact `key → value`. Append-only: earlier values for `key` are kept.
    pub fn set(&self, key: &str, value: Value) -> std::io::Result<()> {
        let mut index = self.index.lock().unwrap();
        if !self.path.as_os_str().is_empty() {
            let rec = json!({ "key": key, "value": value, "ts": crate::share::now_unix() });
            let mut f = OpenOptions::new().create(true).append(true).open(&self.path)?;
            writeln!(f, "{rec}")?;
        }
        index.entry(key.to_string()).or_default().push(value);
        Ok(())
    }

    /// The most recent value for `key` (last write wins), or `None`.
    pub fn get(&self, key: &str) -> Option<Value> {
        self.index.lock().unwrap().get(key).and_then(|v| v.last().cloned())
    }

    /// Every value ever stored under `key`, oldest first (the append-only history).
    pub fn all(&self, key: &str) -> Vec<Value> {
        self.index.lock().unwrap().get(key).cloned().unwrap_or_default()
    }

    /// All known keys.
    pub fn keys(&self) -> Vec<String> {
        self.index.lock().unwrap().keys().cloned().collect()
    }
}

// ─── Agent tools ───────────────────────────────────────────────────────────────

/// Expose a shared [`MemoryStore`] as agent tools — `remember(key, value)` and `recall(key)`
/// — so the model can persist and retrieve facts across the loop.
pub fn memory_tools(store: Arc<MemoryStore>) -> CallbackToolSource {
    let on_remember = store.clone();
    let on_recall = store;
    CallbackToolSource::new()
        .with_tool(remember_def(), move |args| {
            let key = args["key"]
                .as_str()
                .ok_or_else(|| ToolError::new("`key` (string) is required"))?;
            let value = args.get("value").cloned().unwrap_or(Value::Null);
            on_remember
                .set(key, value)
                .map_err(|e| ToolError::new(format!("memory write failed: {e}")))?;
            Ok(json!({ "ok": true }))
        })
        .with_tool(recall_def(), move |args| {
            let key = args["key"]
                .as_str()
                .ok_or_else(|| ToolError::new("`key` (string) is required"))?;
            Ok(match on_recall.get(key) {
                Some(v) => json!({ "found": true, "value": v }),
                None => json!({ "found": false }),
            })
        })
}

fn remember_def() -> ToolDef {
    ToolDef {
        name: "remember".into(),
        description: "Store a fact under a key for later recall (persists across the conversation)."
            .into(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "key": {"type": "string"},
                "value": {"description": "any JSON value to remember"}
            },
            "required": ["key", "value"]
        }),
    }
}

fn recall_def() -> ToolDef {
    ToolDef {
        name: "recall".into(),
        description: "Retrieve the most recently remembered value for a key.".into(),
        input_schema: json!({
            "type": "object",
            "properties": {"key": {"type": "string"}},
            "required": ["key"]
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolSource;

    #[test]
    fn set_get_and_append_only_history() {
        let m = MemoryStore::in_memory();
        assert_eq!(m.get("x"), None);
        m.set("x", json!(1)).unwrap();
        m.set("x", json!(2)).unwrap();
        assert_eq!(m.get("x"), Some(json!(2)), "get returns the latest");
        assert_eq!(m.all("x"), vec![json!(1), json!(2)], "history is append-only");
        m.set("y", json!("hi")).unwrap();
        assert_eq!(m.keys(), vec!["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn persists_and_replays_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mem.jsonl");
        {
            let m = MemoryStore::open(&path).unwrap();
            m.set("name", json!("rozum")).unwrap();
            m.set("count", json!(3)).unwrap();
        }
        // Reopen: the records replay into the index.
        let m2 = MemoryStore::open(&path).unwrap();
        assert_eq!(m2.get("name"), Some(json!("rozum")));
        assert_eq!(m2.get("count"), Some(json!(3)));
    }

    #[tokio::test]
    async fn remember_and_recall_tools() {
        let store = Arc::new(MemoryStore::in_memory());
        let tools = memory_tools(store.clone());
        let names: Vec<String> = tools.tools().into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"remember".to_string()) && names.contains(&"recall".to_string()));

        let r = tools
            .dispatch("remember", json!({"key": "fav_color", "value": "blue"}))
            .await
            .unwrap();
        assert_eq!(r["ok"], true);
        // The store actually has it.
        assert_eq!(store.get("fav_color"), Some(json!("blue")));
        // recall finds it.
        let got = tools.dispatch("recall", json!({"key": "fav_color"})).await.unwrap();
        assert_eq!(got["found"], true);
        assert_eq!(got["value"], "blue");
        // Missing key → found:false (not an error).
        let miss = tools.dispatch("recall", json!({"key": "nope"})).await.unwrap();
        assert_eq!(miss["found"], false);
    }
}
