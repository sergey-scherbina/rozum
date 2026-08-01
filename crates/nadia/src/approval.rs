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

/// Does this call need asking about? The six are decided by name; every MCP tool is guarded
/// whatever it is called (`nadia:SPEC.md` §2.1). An MCP server is an arbitrary program with its
/// own access to the machine — the path jail and the seatbelt confine nadia, not it — so treating
/// its tools as safer than `bash` because they have tidy names would be exactly backwards.
fn is_guarded(name: &str) -> bool {
    GUARDED.contains(&name) || crate::mcp::is_mcp_tool(name)
}

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
        let gated = is_guarded(name)
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
        println!("\n  {tool} {}", describe(tool, args));
        if tool == "edit_file" {
            print!("{}", edit_diff(args));
        }
        print!("  allow? [y]es / [N]o / [a]lways: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => decide(None),
            Ok(_) => decide(Some(&line)),
        }
    }
}

/// Turn what the operator typed into a decision. `None` is end-of-input.
///
/// **Anything that is not an explicit yes is a refusal**, and that includes both EOF and a
/// bare Enter. Measured the hard way: the first version read `""` as *allow*, on the
/// reasoning that Enter means "go ahead" — and `read_line` also returns an empty string at
/// EOF, so as soon as stdin was exhausted (a pipe, a closed terminal, a script) **every
/// subsequent write and command was approved automatically**. A gate whose failure mode is
/// "allow" is not a gate. The prompt spells the default as `[N]o` so this is visible
/// rather than surprising.
fn decide(input: Option<&str>) -> Decision {
    match input.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("y") | Some("yes") => Decision::Allow,
        Some("a") | Some("always") => Decision::AllowAlways,
        _ => Decision::Deny,
    }
}

/// What a call actually does, in one line. A raw JSON blob is not a decision aid — the
/// command, or the file and the size of the change, is. Shared with the live renderer so
/// a tool call reads the same whether it is being approved or merely reported.
pub fn describe(tool: &str, args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
    match tool {
        "bash" => s("command").to_string(),
        "write_file" => format!("{} ({} bytes)", s("path"), s("content").len()),
        "edit_file" => format!("{} — {}", s("path"), summarize_edit(s("old_string"), s("new_string"))),
        "read_file" | "list_dir" => s("path").to_string(),
        "grep" => s("pattern").to_string(),
        // An MCP call: which server, which of its tools, and the arguments clipped. The raw JSON
        // of an unknown schema is all we have, but a wall of it is not something anyone reads
        // before typing `y` — and that is the decision this line exists to inform.
        t if crate::mcp::is_mcp_tool(t) => {
            let rest = t.trim_start_matches("mcp__");
            let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
            format!("{server} · {tool} {}", clip(&args.to_string(), 120))
        }
        _ => args.to_string(),
    }
}

/// Clip to `max` chars, saying that it was clipped — silent truncation makes a reader
/// confidently wrong about what they approved.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!("{}…", s.chars().take(max).collect::<String>())
}

/// One line naming the shape of an edit, for the reported (non-approval) case.
fn summarize_edit(old: &str, new: &str) -> String {
    let (o, n) = (old.lines().count(), new.lines().count());
    match n as i64 - o as i64 {
        0 => format!("{o} line(s) rewritten"),
        d if d > 0 => format!("{o} line(s) -> {n} (+{d})"),
        d => format!("{o} line(s) -> {n} ({d})"),
    }
}

/// The before/after an approval decision actually needs. One line of context is not
/// enough to answer "should this run": what matters is which text is leaving the file and
/// which is replacing it, and a first-line summary hides exactly that.
fn edit_diff(args: &Value) -> String {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("");
    let mut out = String::new();
    for (marker, body) in [("-", s("old_string")), ("+", s("new_string"))] {
        let lines: Vec<&str> = body.lines().collect();
        for l in lines.iter().take(DIFF_LINES) {
            out.push_str(&format!("      {marker} {}\n", truncate(l, 72)));
        }
        if lines.len() > DIFF_LINES {
            out.push_str(&format!("      {marker} … {} more line(s)\n", lines.len() - DIFF_LINES));
        }
    }
    out
}

/// How much of each side of an edit to show before it stops being readable at a prompt.
const DIFF_LINES: usize = 6;

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
    fn describe_shows_the_decision_relevant_part() {
        assert_eq!(describe("bash", &json!({"command": "cargo test"})), "cargo test");
        assert_eq!(describe("write_file", &json!({"path": "a.rs", "content": "abc"})), "a.rs (3 bytes)");
        assert!(describe("edit_file", &json!({"path": "a.rs", "old_string": "a\nb", "new_string": "c"}))
            .contains("a.rs"));
    }

    #[test]
    fn nothing_but_an_explicit_yes_allows() {
        assert_eq!(decide(Some("y")), Decision::Allow);
        assert_eq!(decide(Some("YES\n")), Decision::Allow);
        assert_eq!(decide(Some("a")), Decision::AllowAlways);
        assert_eq!(decide(Some("n")), Decision::Deny);
        assert_eq!(decide(Some("wat")), Decision::Deny);
    }

    #[test]
    fn eof_and_a_bare_enter_refuse() {
        // The bug this pins: `""` used to mean allow, and read_line yields `""` at EOF —
        // so a piped or closed stdin auto-approved every write and every command.
        assert_eq!(decide(None), Decision::Deny, "EOF must fail closed");
        assert_eq!(decide(Some("")), Decision::Deny, "a bare Enter must not approve");
        assert_eq!(decide(Some("\n")), Decision::Deny);
    }

    #[test]
    fn an_edit_prompt_shows_both_sides() {
        // The decision is "is this the right replacement", which a first-line summary
        // cannot answer: both the outgoing and the incoming text have to be visible.
        let d = edit_diff(&json!({"old_string": "let x = 1;", "new_string": "let x = 2;"}));
        assert!(d.contains("- let x = 1;"), "{d}");
        assert!(d.contains("+ let x = 2;"), "{d}");
    }

    #[test]
    fn a_long_edit_is_bounded_and_says_so() {
        let long = (0..40).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let d = edit_diff(&json!({"old_string": long, "new_string": "x"}));
        assert!(d.contains("more line(s)"), "a 40-line edit must not flood the prompt: {d}");
    }
}
