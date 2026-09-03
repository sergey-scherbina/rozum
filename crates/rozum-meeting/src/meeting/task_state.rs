//! Durable per-project task state, offered over MCP (`state.get` / `state.update` /
//! `state.reset` on [`super::proxy::ProxyServer`]).
//!
//! The point this makes concrete: an agent's own conversation keeps growing and gets
//! compacted or cleared, but a task's STATE — a small structured fact, not the transcript
//! that produced it — does not have to live there at all. `TaskState` is one JSON object
//! (`Sigma`) per project, changed only by an RFC 7396 JSON Merge Patch and persisted to
//! `<project>/.rozum/state.json`, so `/clear` and a fresh session cannot lose it: call
//! `state.get` right after either to recover exactly where a task stood.
//!
//! No schema validates a patch here, on purpose. The tool is generic across whatever
//! project it runs in, so the state's actual shape is a convention between whoever calls
//! `state.update` and whoever reads the file back — the same trust boundary a hand-edited
//! JSON file already has. A caller wanting typed validation decodes the returned value
//! itself; this module's contract is only "valid JSON object in, RFC 7396 applied, same
//! object out."
//!
//! Twin of `okay`'s `Staged.json`-adjacent `Json.mergePatch` / `StateMcp` (Scala): same
//! algebra (RFC 7396), same three tools, same "the project boundary is the state boundary"
//! design — reimplemented here natively rather than shared, because the two codebases do
//! not share a runtime and the merge itself is thirty lines.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::Mutex;

/// RFC 7396 JSON Merge Patch, applied: an object PATCH recursively merges into TARGET
/// field by field (a target that is not itself an object is treated as `{}`, per the
/// RFC), a `null` field DELETES that key, and any other value replaces it wholesale — so
/// a scalar or array patch always replaces, never merges. Pure and total: never panics,
/// never fails, whatever the inputs.
///
/// Self-composing only up to the RFC's own caveat: `merge_patch(merge_patch(t, p1), p2)`
/// is not always the same value as `merge_patch(t, merge_patch(p1, p2))` when `p2` deletes
/// a key that `t` carried and `p1` never mentioned — the combined patch has nothing to
/// delete, because it was never told the key existed. Apply patches in order; do not
/// combine them first.
pub fn merge_patch(target: &Value, patch: &Value) -> Value {
    match patch {
        Value::Object(patch_map) => {
            let mut result = match target {
                Value::Object(m) => m.clone(),
                _ => serde_json::Map::new(),
            };
            for (k, v) in patch_map {
                if v.is_null() {
                    result.remove(k);
                } else {
                    let orig = result.get(k).cloned().unwrap_or(Value::Null);
                    result.insert(k.clone(), merge_patch(&orig, v));
                }
            }
            Value::Object(result)
        }
        other => other.clone(),
    }
}

/// One project's durable state: a JSON object, an in-memory cache backed by a file,
/// merged and persisted on every `update`.
pub struct TaskState {
    path: PathBuf,
    sigma: Mutex<Value>,
}

impl TaskState {
    /// Open (or create empty) the state file at an explicit path. A file that exists
    /// but does not parse as a JSON object starts empty rather than failing — the same
    /// totality the rest of this project's persistence favors (a damaged file is a
    /// fact to recover from, not a reason to refuse to start).
    pub fn open(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let sigma = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        Self {
            path,
            sigma: Mutex::new(sigma),
        }
    }

    /// The state file for an already-resolved project path — `DaemonProxy` resolves
    /// the project once (cwd detection for stdio, the HTTP transport's `?project=` pin
    /// otherwise) and passes it straight through here, so this module makes no
    /// resolution decision of its own.
    pub fn for_project_path(project: &str) -> Self {
        Self::open(project_state_path(Path::new(project)))
    }

    fn persist(&self, sigma: &Value) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(sigma).unwrap_or_else(|_| "{}".to_string());
        std::fs::write(&self.path, text)
    }

    /// The current state, as it stands.
    pub async fn get(&self) -> Value {
        self.sigma.lock().await.clone()
    }

    /// Merge `patch` in (RFC 7396) and persist. Refuses a non-object patch outright —
    /// the state stays untouched and the error names why — because a merge patch that
    /// is not an object cannot mean "change some fields", only "replace everything",
    /// which is never what an incremental fact-update tool call means to do.
    pub async fn update(&self, patch: &Value) -> Result<Value, String> {
        if !patch.is_object() {
            return Err(format!(
                "state.update expects a JSON object patch (RFC 7396), got: {patch}"
            ));
        }
        let mut guard = self.sigma.lock().await;
        let merged = merge_patch(&guard, patch);
        self.persist(&merged)
            .map_err(|e| format!("failed to persist state: {e}"))?;
        *guard = merged.clone();
        Ok(merged)
    }

    /// Clear back to `{}`, persisted.
    pub async fn reset(&self) -> Result<Value, String> {
        let empty = json!({});
        self.persist(&empty)
            .map_err(|e| format!("failed to persist state: {e}"))?;
        *self.sigma.lock().await = empty.clone();
        Ok(empty)
    }
}

pub fn project_state_path(project: &Path) -> PathBuf {
    project.join(".rozum").join("state.json")
}

/// Convenience for `DaemonProxy`: a project-scoped store, or `None` outside a project.
pub type SharedTaskState = Option<Arc<TaskState>>;

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(fs: &[(&str, Value)]) -> Value {
        Value::Object(fs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }

    // RFC 7396 §3, verbatim.
    #[test]
    fn rfc_examples() {
        let cases: &[(&str, &str, &str)] = &[
            (r#"{"a":"b"}"#, r#"{"a":"c"}"#, r#"{"a":"c"}"#),
            (r#"{"a":"b"}"#, r#"{"b":"c"}"#, r#"{"a":"b","b":"c"}"#),
            (r#"{"a":"b"}"#, r#"{"a":null}"#, r#"{}"#),
            (r#"{"a":"b","b":"c"}"#, r#"{"a":null}"#, r#"{"b":"c"}"#),
            (r#"{"a":["b"]}"#, r#"{"a":"c"}"#, r#"{"a":"c"}"#),
            (r#"{"a":"c"}"#, r#"{"a":["b"]}"#, r#"{"a":["b"]}"#),
            (
                r#"{"a":{"b":"c"}}"#,
                r#"{"a":{"b":"d","c":null}}"#,
                r#"{"a":{"b":"d"}}"#,
            ),
            (r#"{"a":[{"b":"c"}]}"#, r#"{"a":[1]}"#, r#"{"a":[1]}"#),
            (r#"["a","b"]"#, r#"["c","d"]"#, r#"["c","d"]"#),
            (r#"{"a":"b"}"#, r#"["c"]"#, r#"["c"]"#),
            (r#"{"a":"foo"}"#, "null", "null"),
            (r#"{"a":"foo"}"#, r#""bar""#, r#""bar""#),
            (r#"{"e":null}"#, r#"{"a":1}"#, r#"{"e":null,"a":1}"#),
            (r#"[1,2]"#, r#"{"a":"b","c":null}"#, r#"{"a":"b"}"#),
            (
                r#"{}"#,
                r#"{"a":{"bb":{"ccc":null}}}"#,
                r#"{"a":{"bb":{}}}"#,
            ),
        ];
        for (target, patch, expected) in cases {
            let t: Value = serde_json::from_str(target).unwrap();
            let p: Value = serde_json::from_str(patch).unwrap();
            let e: Value = serde_json::from_str(expected).unwrap();
            assert_eq!(merge_patch(&t, &p), e, "{target} <- {patch}");
        }
    }

    #[test]
    fn nested_merge_deletes_only_its_own_key() {
        let target = obj(&[
            ("x", obj(&[("a", json!(1)), ("b", json!(2))])),
            ("y", json!(3)),
        ]);
        let patch = obj(&[("x", obj(&[("a", Value::Null), ("c", json!(9))]))]);
        assert_eq!(
            merge_patch(&target, &patch),
            obj(&[
                ("x", obj(&[("b", json!(2)), ("c", json!(9))])),
                ("y", json!(3))
            ])
        );
    }

    #[test]
    fn deleting_an_absent_key_is_a_no_op() {
        let target = obj(&[("a", json!(1))]);
        assert_eq!(merge_patch(&target, &obj(&[("b", Value::Null)])), target);
    }

    #[test]
    fn the_documented_caveat_combine_then_apply_can_disagree() {
        // p2 deletes "b", which only the ORIGINAL target ever set — p1 never mentions
        // it, so a combined patch has nothing to delete.
        let target = obj(&[("x", obj(&[("a", json!(1)), ("b", json!(2))]))]);
        let p1 = obj(&[("x", obj(&[("a", json!(10))]))]);
        let p2 = obj(&[("x", obj(&[("b", Value::Null)]))]);

        let sequential = merge_patch(&merge_patch(&target, &p1), &p2);
        let combined_first = merge_patch(&target, &merge_patch(&p1, &p2));

        assert_eq!(sequential, obj(&[("x", obj(&[("a", json!(10))]))]));
        assert_eq!(
            combined_first,
            obj(&[("x", obj(&[("a", json!(10)), ("b", json!(2))]))])
        );
        assert_ne!(
            sequential, combined_first,
            "the caveat's own doc comment is stale"
        );
    }

    #[tokio::test]
    async fn get_starts_empty_update_merges_a_second_field_does_not_erase_the_first() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskState::open(dir.path().join("state.json"));
        assert_eq!(store.get().await, json!({}));

        store
            .update(&obj(&[("task", json!("pack-orders"))]))
            .await
            .unwrap();
        store.update(&obj(&[("picked", json!(3))])).await.unwrap();
        assert_eq!(
            store.get().await,
            obj(&[("task", json!("pack-orders")), ("picked", json!(3))])
        );
    }

    #[tokio::test]
    async fn a_null_field_deletes_it_a_repeated_key_is_the_last_write() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskState::open(dir.path().join("state.json"));
        store
            .update(&obj(&[("a", json!(1)), ("b", json!(2))]))
            .await
            .unwrap();
        store
            .update(&obj(&[("a", Value::Null), ("b", json!(9))]))
            .await
            .unwrap();
        assert_eq!(store.get().await, obj(&[("b", json!(9))]));
    }

    #[tokio::test]
    async fn a_non_object_patch_is_refused_and_sigma_is_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let store = TaskState::open(dir.path().join("state.json"));
        store.update(&obj(&[("kept", json!("yes"))])).await.unwrap();
        let err = store.update(&json!([1, 2, 3])).await.unwrap_err();
        assert!(err.contains("JSON object"), "{err}");
        assert_eq!(store.get().await, obj(&[("kept", json!("yes"))]));
    }

    #[tokio::test]
    async fn reset_clears_it_and_the_clear_is_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let store = TaskState::open(&path);
        store.update(&obj(&[("x", json!(1))])).await.unwrap();
        store.reset().await.unwrap();
        assert_eq!(store.get().await, json!({}));
        let on_disk: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(on_disk, json!({}));
    }

    #[tokio::test]
    async fn state_survives_a_restart_a_fresh_open_over_the_same_file_sees_the_last_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        TaskState::open(&path)
            .update(&obj(&[("survived", json!(true))]))
            .await
            .unwrap();

        // a NEW TaskState over the SAME file — no in-memory state shared, only the
        // file on disk crosses this line, which is what surviving a restart means
        let restarted = TaskState::open(&path);
        assert_eq!(restarted.get().await, obj(&[("survived", json!(true))]));
    }

    #[tokio::test]
    async fn a_damaged_state_file_starts_empty_rather_than_failing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(TaskState::open(&path).get().await, json!({}));
    }

    #[test]
    fn project_state_path_lives_under_dot_rozum() {
        assert_eq!(
            project_state_path(Path::new("/tmp/proj")),
            PathBuf::from("/tmp/proj/.rozum/state.json")
        );
    }
}
