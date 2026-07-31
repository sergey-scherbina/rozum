//! The six tools, and nothing else.
//!
//! The bar for a seventh: it must enable a task class that is *impossible* with these
//! six, not merely more convenient. Every tool costs schema tokens in every request of
//! every step, and dilutes selection for a small model — measured in this repo as
//! ~33 tools / ~4.9K schema tokens for stock Claude Code, which `--lean` cuts by 84%.
//!
//! Shape follows Contract 3 (`docs/specs/integration.md`): high-level, atomic,
//! deterministic, strict schemas, and errors written as instructions the model can act
//! on. A tool error is not an exception here — it is the next prompt.

use std::sync::Arc;

use rozum_agent::agent::{CallbackToolSource, ToolError, ToolSource};
use rozum_core::backend::ToolDef;
use serde_json::{json, Value};

use crate::sandbox::Sandbox;

/// Byte ceiling for any single tool result. A `cargo test` on a broken crate can emit
/// megabytes; feeding that back verbatim burns the context window in one step and the
/// agent loses the thread. Truncation is always *announced* — silent truncation makes
/// the model confidently wrong about what it saw.
const MAX_OUT: usize = 8_000;

fn clip(s: &str) -> (String, bool) {
    if s.len() <= MAX_OUT {
        return (s.to_string(), false);
    }
    let mut end = MAX_OUT;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_string(), true)
}

/// A required string argument, accepting any JSON scalar in its place.
///
/// A number, or a boolean, has exactly one obvious textual form, and a model that answers
/// `{"content": 4}` when asked to write the number four has not made a mistake worth failing
/// a task over. Refusing it was not strictness: the message claimed the argument was
/// *missing* when it had been supplied, so the model re-sent the identical call until the
/// repetition guard ended the run — on a task it had already solved. Observed against
/// Qwen3.5-4B; the twin implementation in the `nadia` repo carries the same fix and the
/// reasoning in full (`docs/tools.md`).
///
/// Objects and arrays are still refused. There is no single right way to render those as
/// text, and guessing one would put invented content into a file. `null` counts as missing
/// rather than as empty, so a lost value cannot quietly truncate a file.
fn str_arg(v: &Value, key: &str) -> Result<String, ToolError> {
    match v.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(Value::Number(n)) => Ok(n.to_string()),
        Some(Value::Bool(b)) => Ok(b.to_string()),
        None | Some(Value::Null) => Err(ToolError::new(format!(
            "missing required string argument `{key}`"
        ))),
        Some(Value::Object(_)) => Err(ToolError::new(format!(
            "argument `{key}` must be a string, but a JSON object was sent — pass the value as text"
        ))),
        Some(Value::Array(_)) => Err(ToolError::new(format!(
            "argument `{key}` must be a string, but a JSON array was sent — pass the value as text"
        ))),
    }
}

fn opt_usize(v: &Value, key: &str) -> Option<usize> {
    v.get(key).and_then(Value::as_u64).map(|n| n as usize)
}

fn def(name: &str, description: &str, properties: Value, required: &[&str]) -> ToolDef {
    ToolDef {
        name: name.to_string(),
        description: description.to_string(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

/// Build the tool source the agent loop dispatches through.
///
/// Handlers are synchronous by the `CallbackToolSource` contract, so a long `bash` call
/// blocks the task that runs the loop. That is correct for one agent per process (the
/// loop has nothing else to do while a build runs); when subagents land, each gets its
/// own task and the blocking moves behind `spawn_blocking`.
pub fn tool_source(sandbox: Arc<Sandbox>) -> impl ToolSource {
    let sb = sandbox;

    CallbackToolSource::new()
        .with_tool(
            def(
                "read_file",
                "Read a text file from the workspace. Returns the contents with line numbers. \
                 Read a file before editing it — `edit_file` needs `old_string` to match the \
                 file exactly.",
                json!({
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "start_line": {"type": "integer", "description": "First line to return (1-based, inclusive)."},
                    "end_line": {"type": "integer", "description": "Last line to return (1-based, inclusive)."},
                }),
                &["path"],
            ),
            {
                let sb = sb.clone();
                move |args: Value| {
                    let path = str_arg(&args, "path")?;
                    let full = sb.resolve(&path).map_err(ToolError::new)?;
                    let text = std::fs::read_to_string(&full)
                        .map_err(|e| ToolError::new(format!("read {path}: {e}")))?;
                    let start = opt_usize(&args, "start_line").unwrap_or(1).max(1);
                    let end = opt_usize(&args, "end_line").unwrap_or(usize::MAX);
                    let numbered: String = text
                        .lines()
                        .enumerate()
                        .filter(|(i, _)| i + 1 >= start && i + 1 <= end)
                        .map(|(i, l)| format!("{:>6}\t{l}\n", i + 1))
                        .collect();
                    let (body, truncated) = clip(&numbered);
                    Ok(json!({"path": path, "content": body, "truncated": truncated}))
                }
            },
        )
        .with_tool(
            def(
                "write_file",
                "Create a file, or replace its entire contents. Use `edit_file` to change \
                 part of an existing file — writing a whole file to change one line loses \
                 everything you did not repeat.",
                json!({
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "content": {"type": "string", "description": "The complete new contents of the file."},
                }),
                &["path", "content"],
            ),
            {
                let sb = sb.clone();
                move |args: Value| {
                    let path = str_arg(&args, "path")?;
                    let content = str_arg(&args, "content")?;
                    let full = sb.resolve(&path).map_err(ToolError::new)?;
                    if let Some(parent) = full.parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| ToolError::new(format!("create {}: {e}", parent.display())))?;
                    }
                    std::fs::write(&full, &content)
                        .map_err(|e| ToolError::new(format!("write {path}: {e}")))?;
                    Ok(json!({"written": path, "bytes": content.len()}))
                }
            },
        )
        .with_tool(
            def(
                "edit_file",
                "Replace one exact occurrence of `old_string` with `new_string`. `old_string` \
                 must appear EXACTLY ONCE — include enough surrounding context to make it \
                 unique. If it matches zero or several times the edit is refused and nothing \
                 changes; re-read the file and retry with more context.",
                json!({
                    "path": {"type": "string", "description": "Path relative to the workspace root."},
                    "old_string": {"type": "string", "description": "Text to replace, copied exactly from the file."},
                    "new_string": {"type": "string", "description": "Replacement text."},
                }),
                &["path", "old_string", "new_string"],
            ),
            {
                let sb = sb.clone();
                move |args: Value| {
                    let path = str_arg(&args, "path")?;
                    let old = str_arg(&args, "old_string")?;
                    let new = str_arg(&args, "new_string")?;
                    let full = sb.resolve(&path).map_err(ToolError::new)?;
                    let text = std::fs::read_to_string(&full)
                        .map_err(|e| ToolError::new(format!("read {path}: {e}")))?;
                    // The single-occurrence rule is the whole point of this tool. A tool that
                    // replaced the first match would let a model "fix" one of five identical
                    // lines and report success — the error below is what makes it retry with
                    // more context instead.
                    let hits = text.matches(&old).count();
                    if hits != 1 {
                        return Err(ToolError::new(format!(
                            "old_string matched {hits} times in {path} — it must match exactly once. \
                             {}",
                            if hits == 0 {
                                "Re-read the file: the text you quoted is not there (check whitespace and indentation)."
                            } else {
                                "Include more surrounding lines to make it unique."
                            }
                        )));
                    }
                    std::fs::write(&full, text.replacen(&old, &new, 1))
                        .map_err(|e| ToolError::new(format!("write {path}: {e}")))?;
                    Ok(json!({"path": path, "replaced": 1}))
                }
            },
        )
        .with_tool(
            def(
                "list_dir",
                "List the entries of a directory in the workspace.",
                json!({
                    "path": {"type": "string", "description": "Directory relative to the workspace root. Defaults to the root."},
                }),
                &[],
            ),
            {
                let sb = sb.clone();
                move |args: Value| {
                    let path = args.get("path").and_then(Value::as_str).unwrap_or(".").to_string();
                    let full = sb.resolve(&path).map_err(ToolError::new)?;
                    let mut entries: Vec<Value> = Vec::new();
                    let rd = std::fs::read_dir(&full)
                        .map_err(|e| ToolError::new(format!("list {path}: {e}")))?;
                    for e in rd.flatten() {
                        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                        entries.push(json!({
                            "name": e.file_name().to_string_lossy(),
                            "dir": is_dir,
                        }));
                    }
                    entries.sort_by_key(|v| v["name"].as_str().unwrap_or("").to_string());
                    Ok(json!({"path": path, "entries": entries}))
                }
            },
        )
        .with_tool(
            def(
                "grep",
                "Search the workspace with a regular expression. Returns `path:line:text` for \
                 each match. Use it to find where something is defined before reading a file.",
                json!({
                    "pattern": {"type": "string", "description": "Regular expression, e.g. `fn \\w+_handler` or `TODO`."},
                    "path": {"type": "string", "description": "File or directory to search. Defaults to the whole workspace."},
                }),
                &["pattern"],
            ),
            {
                let sb = sb.clone();
                move |args: Value| {
                    let pattern = str_arg(&args, "pattern")?;
                    // A bad pattern is the model's to fix, so the regex error goes back
                    // verbatim rather than being flattened into "no matches" — which is
                    // what a literal-substring fallback would silently look like.
                    let re = regex::Regex::new(&pattern).map_err(|e| {
                        ToolError::new(format!("`{pattern}` is not a valid regular expression: {e}"))
                    })?;
                    let path = args.get("path").and_then(Value::as_str).unwrap_or(".").to_string();
                    let root = sb.resolve(&path).map_err(ToolError::new)?;
                    let mut out = String::new();
                    let mut stack = vec![root.clone()];
                    while let Some(p) = stack.pop() {
                        if p.is_dir() {
                            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                            // Skipping build/VCS trees is not a nicety: `target/` alone can be
                            // hundreds of thousands of files, and the walk would dominate the step.
                            if matches!(name.as_str(), "target" | ".git" | "node_modules") {
                                continue;
                            }
                            if let Ok(rd) = std::fs::read_dir(&p) {
                                stack.extend(rd.flatten().map(|e| e.path()));
                            }
                            continue;
                        }
                        let Ok(text) = std::fs::read_to_string(&p) else { continue };
                        let rel = p.strip_prefix(sb.root()).unwrap_or(&p).display().to_string();
                        for (i, line) in text.lines().enumerate() {
                            if re.is_match(line) {
                                out.push_str(&format!("{rel}:{}:{}\n", i + 1, line.trim_end()));
                            }
                        }
                    }
                    let (body, truncated) = clip(&out);
                    Ok(json!({"pattern": pattern, "matches": body, "truncated": truncated}))
                }
            },
        )
        .with_tool(
            def(
                "bash",
                "Run a shell command in the workspace and return its output. This is how you \
                 build, test and run things (`cargo build`, `cargo test`, `cargo run`). The \
                 command runs with the workspace as its working directory. Read the output \
                 before claiming success — a command that failed did not do what you asked.",
                json!({
                    "command": {"type": "string", "description": "Shell command line to run."},
                    "timeout_ms": {"type": "integer", "description": "Kill the command after this many milliseconds."},
                }),
                &["command"],
            ),
            {
                let sb = sb.clone();
                move |args: Value| {
                    let command = str_arg(&args, "command")?;
                    let timeout = opt_usize(&args, "timeout_ms")
                        .map(|ms| std::time::Duration::from_millis(ms as u64));
                    let r = sb.exec(&command, timeout).map_err(ToolError::new)?;
                    let (stdout, out_trunc) = clip(&r.stdout);
                    let (stderr, err_trunc) = clip(&r.stderr);
                    Ok(json!({
                        "exit_code": r.exit_code,
                        "stdout": stdout,
                        "stderr": stderr,
                        "timed_out": r.timed_out,
                        "truncated": out_trunc || err_trunc,
                    }))
                }
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, Arc<Sandbox>) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "alpha\nbeta\nalpha\n").unwrap();
        let mut sb = Sandbox::new(dir.path()).unwrap();
        sb.confine = false;
        (dir, Arc::new(sb))
    }

    async fn call(src: &impl ToolSource, name: &str, args: Value) -> Result<Value, String> {
        src.dispatch(name, args).await.map_err(|e| e.0)
    }

    #[tokio::test]
    async fn exposes_exactly_six_tools() {
        let (_d, sb) = fixture();
        let names: Vec<String> = tool_source(sb).tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names.len(), 6, "tool set grew without a spec change: {names:?}");
        for expected in ["read_file", "write_file", "edit_file", "list_dir", "grep", "bash"] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
    }

    #[tokio::test]
    async fn edit_refuses_ambiguous_and_missing_matches() {
        let (_d, sb) = fixture();
        let src = tool_source(sb);

        // "alpha" appears twice — replacing the first would silently do the wrong thing.
        let err = call(&src, "edit_file", json!({"path": "a.txt", "old_string": "alpha", "new_string": "x"}))
            .await
            .unwrap_err();
        assert!(err.contains("matched 2 times"), "{err}");

        let err = call(&src, "edit_file", json!({"path": "a.txt", "old_string": "nope", "new_string": "x"}))
            .await
            .unwrap_err();
        assert!(err.contains("matched 0 times"), "{err}");

        // Unique context succeeds and changes exactly one line.
        call(&src, "edit_file", json!({"path": "a.txt", "old_string": "beta", "new_string": "BETA"}))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn path_arguments_cannot_escape_the_workspace() {
        let (_d, sb) = fixture();
        let src = tool_source(sb);
        for (tool, args) in [
            ("read_file", json!({"path": "../../etc/passwd"})),
            ("write_file", json!({"path": "/tmp/nadia-escape", "content": "x"})),
            ("list_dir", json!({"path": ".."})),
        ] {
            let err = call(&src, tool, args).await.unwrap_err();
            assert!(err.contains("outside") || err.contains("escapes"), "{tool}: {err}");
        }
    }

    #[tokio::test]
    async fn unknown_tool_is_a_recoverable_error() {
        let (_d, sb) = fixture();
        let err = call(&tool_source(sb), "delete_everything", json!({})).await.unwrap_err();
        assert!(err.contains("unknown tool"), "{err}");
    }

    #[tokio::test]
    async fn missing_argument_names_the_argument() {
        let (_d, sb) = fixture();
        let src = tool_source(sb);
        let err = call(&src, "read_file", json!({})).await.unwrap_err();
        assert!(err.contains("`path`"), "the model must be told which argument: {err}");
        // `null` is missing, not empty. Reading it as "" writes an empty file over a real one.
        let err = call(&src, "write_file", json!({"path": "n.txt", "content": null}))
            .await
            .unwrap_err();
        assert!(err.contains("`content`"), "{err}");
    }

    #[tokio::test]
    async fn a_number_where_a_string_was_asked_for_is_accepted() {
        // Observed against Qwen3.5-4B: asked to write a line count into a file, it sent
        // {"content": 4}. Answering "missing required string argument `content`" is false —
        // it was supplied — so the model re-sent the identical call until the repetition
        // guard killed a task it had already solved.
        let (d, sb) = fixture();
        let src = tool_source(sb);
        call(&src, "write_file", json!({"path": "count.txt", "content": 4}))
            .await
            .expect("a JSON number has one obvious textual form");
        assert_eq!(std::fs::read_to_string(d.path().join("count.txt")).unwrap(), "4");

        // An object has no single right rendering, and guessing would put invented content
        // into a file. Refused, and told what was actually sent.
        let err = call(&src, "write_file", json!({"path": "c.txt", "content": {"a": 1}}))
            .await
            .unwrap_err();
        assert!(
            err.contains("must be a string") && err.contains("JSON object"),
            "{err}"
        );
    }
}
