//! Asking before doing.
//!
//! The sandbox decides what is *possible*; this decides what is *wanted*. They are not
//! the same question: writing to `src/main.rs` is inside the workspace and therefore
//! permitted, and still not something to do silently while someone is watching.
//!
//! Layered as a [`ToolSource`] wrapper rather than a check inside each handler, for two
//! reasons: the tools stay free of I/O that only makes sense in one front-end, and the
//! decision becomes injectable, so the policy is unit-testable without a terminal.
//!
//! A refusal is returned to the model as a tool error, not as a halt. "The user declined"
//! is information it can act on — propose something else, or explain why it needs this —
//! whereas killing the turn leaves it unable to respond at all.

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use rozum_agent::agent::{ToolError, ToolSource};
use rozum_core::backend::ToolDef;
use serde_json::Value;

/// Tools that change something. Reads (`read_file`, `list_dir`, `grep`) are never gated:
/// prompting for them trains the operator to hit `y` without reading, which is how a
/// prompt for the one call that mattered gets waved through too.
const GUARDED: &[&str] = &["write_file", "edit_file", "bash"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// Execute guarded tools without asking (batch runs, and `/approve auto`).
    Auto,
    /// Ask before each guarded tool.
    Ask,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Allow,
    /// Allow this, and stop asking about this tool for the rest of the session.
    AllowAlways,
    Deny,
}

/// How a decision is obtained. The terminal implementation is one of these; tests use a
/// scripted one.
pub trait Approver: Send + Sync {
    fn ask(&self, tool: &str, args: &Value) -> Decision;
}

/// Shared policy state, so a slash command can flip the mode on a gate the agent loop is
/// already holding.
pub struct Policy {
    mode: Mutex<Mode>,
    always: Mutex<HashSet<String>>,
}

impl Policy {
    pub fn new(mode: Mode) -> Arc<Self> {
        Arc::new(Self { mode: Mutex::new(mode), always: Mutex::new(HashSet::new()) })
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    pub fn set_mode(&self, mode: Mode) {
        *self.mode.lock().unwrap() = mode;
    }
}

pub struct ApprovalGate<T: ToolSource> {
    inner: T,
    policy: Arc<Policy>,
    approver: Box<dyn Approver>,
}

impl<T: ToolSource> ApprovalGate<T> {
    pub fn new(inner: T, policy: Arc<Policy>, approver: Box<dyn Approver>) -> Self {
        Self { inner, policy, approver }
    }
}

#[async_trait]
impl<T: ToolSource> ToolSource for ApprovalGate<T> {
    fn tools(&self) -> Vec<ToolDef> {
        self.inner.tools()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let gated = GUARDED.contains(&name)
            && self.policy.mode() == Mode::Ask
            && !self.policy.always.lock().unwrap().contains(name);
        if gated {
            match self.approver.ask(name, &args) {
                Decision::Allow => {}
                Decision::AllowAlways => {
                    self.policy.always.lock().unwrap().insert(name.to_string());
                }
                Decision::Deny => {
                    return Err(ToolError::new(format!(
                        "The user declined the `{name}` call. Do not retry it. Explain what you \
                         intended to do and ask what they would prefer, or continue with the part \
                         of the task that does not need it."
                    )));
                }
            }
        }
        self.inner.dispatch(name, args).await
    }
}

/// Reads the decision from the terminal. Safe to read stdin here: the agent loop is
/// blocked on this dispatch, so the REPL's own reader is not competing for the line.
pub struct TerminalApprover;

impl Approver for TerminalApprover {
    fn ask(&self, tool: &str, args: &Value) -> Decision {
        println!("\n  {tool} {}", preview(tool, args));
        print!("  allow? [y]es / [n]o / [a]lways: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return Decision::Deny;
        }
        match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" | "" => Decision::Allow,
            "a" | "always" => Decision::AllowAlways,
            _ => Decision::Deny,
        }
    }
}

/// What the operator is actually being asked to approve. A raw JSON blob is not a
/// decision aid — the command, or the file and the size of the change, is.
fn preview(tool: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "bash" => s("command").to_string(),
        "write_file" => format!("{} ({} bytes)", s("path"), s("content").len()),
        "edit_file" => {
            let old = s("old_string");
            let first = old.lines().next().unwrap_or("");
            format!("{} — replace `{}`", s("path"), truncate(first, 48))
        }
        _ => args.to_string(),
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rozum_agent::agent::CallbackToolSource;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts how often it was consulted, so a test can prove `always` stops the asking.
    struct Scripted {
        decision: Decision,
        asked: AtomicUsize,
    }

    impl Approver for Scripted {
        fn ask(&self, _tool: &str, _args: &Value) -> Decision {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.decision
        }
    }

    fn source() -> impl ToolSource {
        let mut s = CallbackToolSource::new();
        for name in ["bash", "read_file"] {
            s = s.with_tool(
                ToolDef {
                    name: name.into(),
                    description: "t".into(),
                    input_schema: json!({"type": "object", "properties": {}}),
                },
                |_| Ok(json!({"ran": true})),
            );
        }
        s
    }

    #[tokio::test]
    async fn denial_is_recoverable_and_does_not_run_the_tool() {
        let gate = ApprovalGate::new(
            source(),
            Policy::new(Mode::Ask),
            Box::new(Scripted { decision: Decision::Deny, asked: AtomicUsize::new(0) }),
        );
        let err = gate.dispatch("bash", json!({"command": "rm -rf ."})).await.unwrap_err();
        assert!(err.0.contains("declined"), "{}", err.0);
        assert!(err.0.contains("Do not retry"), "a bare refusal invites a retry loop: {}", err.0);
    }

    #[tokio::test]
    async fn reads_are_never_gated() {
        let approver = Box::new(Scripted { decision: Decision::Deny, asked: AtomicUsize::new(0) });
        let gate = ApprovalGate::new(source(), Policy::new(Mode::Ask), approver);
        // Denying everything must still let a read through — it is not in GUARDED.
        assert!(gate.dispatch("read_file", json!({"path": "x"})).await.is_ok());
    }

    #[tokio::test]
    async fn auto_mode_never_asks() {
        let gate = ApprovalGate::new(
            source(),
            Policy::new(Mode::Auto),
            Box::new(Scripted { decision: Decision::Deny, asked: AtomicUsize::new(0) }),
        );
        assert!(gate.dispatch("bash", json!({"command": "ls"})).await.is_ok());
    }

    #[tokio::test]
    async fn always_stops_asking_for_that_tool_only() {
        let policy = Policy::new(Mode::Ask);
        let gate = ApprovalGate::new(
            source(),
            policy.clone(),
            Box::new(Scripted { decision: Decision::AllowAlways, asked: AtomicUsize::new(0) }),
        );
        for _ in 0..3 {
            assert!(gate.dispatch("bash", json!({"command": "ls"})).await.is_ok());
        }
        assert!(policy.always.lock().unwrap().contains("bash"));
        assert!(!policy.always.lock().unwrap().contains("write_file"));
    }

    #[test]
    fn preview_shows_the_decision_relevant_part() {
        assert_eq!(preview("bash", &json!({"command": "cargo test"})), "cargo test");
        assert_eq!(preview("write_file", &json!({"path": "a.rs", "content": "abc"})), "a.rs (3 bytes)");
        assert!(preview("edit_file", &json!({"path": "a.rs", "old_string": "fn main() {\nx"}))
            .contains("fn main()"));
    }
}
