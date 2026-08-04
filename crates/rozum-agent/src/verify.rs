//! The verify-repair gate: decide what "done" means, check it, and hand the real error back.
//!
//! An agent that reports success it has not observed is this project's most expensive failure
//! mode — a working RPN calculator whose own printout said `4 + 4 = 7`, a task "finished" on a
//! broken build. The gate closes that: **the ground truth is a command that either passes or
//! does not**, run after the agent stops, and its output — not the model's summary — decides.
//!
//! Two tiers, because tasks come in two kinds:
//!
//! 1. **Deterministic.** [`derive_check`] asks the model to FORMALIZE the task into structured
//!    data (`{"checkable":bool,"cargo_test":bool,"run":[{"arg","expect"}]}`), and *we* build the
//!    shell command from it — the model never writes shell, so it cannot inject any. When the
//!    task states an example, this becomes exactly the check that would have caught the wrong
//!    printout.
//! 2. **Semantic.** When the task has no machine-checkable criterion ("make the error message
//!    clearer"), [`judge`] asks an independent model whether the code accomplishes it. Its
//!    **Unknown is not a pass**: a bounded caller can escalate or report an honest unverified
//!    failure, but it may not claim correctness it has no evidence for.
//!
//! The guards are the part that took measurement to find, and they are why this is a module
//! rather than a prompt. `derive_check` refuses a task that is not about code at all — it once
//! invented `cargo run -- pong == gnop` for a chat task — and [`is_hallucinated_cargo_check`]
//! catches the same class one level down, when a cargo check appears for a workspace that has no
//! Cargo.toml and a task that never asked for Rust.
//!
//! Lives here, in the agent tier, because both consumers need exactly it and neither owns it:
//! `rozum launch` wraps it in a model-escalation chain, and `nadia` runs it around one model. Two
//! copies of a prompt this carefully worded is how they drift apart.

use std::path::Path;

use futures::StreamExt;

use crate::backend::{ChatBackend, ChatEvent, ChatRequest};

/// A semantic verdict. `Unknown` is deliberately distinct from `Fail`: "I could not tell" and
/// "it is wrong" call for different decisions from the caller, and collapsing them either
/// invents failures or hides them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail(String),
    Unknown(String),
}

/// One plain completion, no tools, collected to text. `None` when the backend fails — every
/// caller here treats that as "no opinion" rather than as an answer.
async fn ask(backend: &dyn ChatBackend, prompt: &str, max_tokens: u32) -> Option<String> {
    let mut req = ChatRequest::simple(prompt);
    req.sampling.temperature = Some(0.0);
    req.sampling.max_tokens = Some(max_tokens);
    let mut stream = backend.chat(req).await.ok()?;
    let mut text = String::new();
    while let Some(ev) = stream.next().await {
        match ev.ok()? {
            ChatEvent::TextDelta { text: t } => text.push_str(&t),
            ChatEvent::Done { .. } => break,
            _ => {}
        }
    }
    (!text.trim().is_empty()).then_some(text)
}

/// The first JSON object in a reply, tolerantly. Models wrap JSON in prose however they like.
fn first_json(text: &str) -> Option<serde_json::Value> {
    let (a, b) = (text.find('{')?, text.rfind('}')?);
    (a <= b).then(|| serde_json::from_str(&text[a..=b]).ok())?
}

/// Shell-quote a model-provided string so it is inert inside the derived command.
fn shquote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// A `cargo run` check that SAYS what it saw. A bare `[ "$(cargo run …)" = … ]` fails silently,
/// which leaves the repair round with an empty "real error" on exactly the mismatches that
/// matter most — the ones where the program runs and prints the wrong thing.
pub fn cargo_run_fragment(arg: &str, expect: &str) -> String {
    let (arg_q, exp_q) = (shquote(arg), shquote(expect));
    format!(
        "{{ out=$(cargo run -q -- {arg_q}) && [ \"$out\" = {exp_q} ] || \
         {{ printf 'cargo run -- %s printed <%s>; expected <%s>\\n' {arg_q} \"$out\" {exp_q} >&2; exit 1; }}; }}"
    )
}

/// Does the task actually ask for Rust? The cheap guard that lets a create-from-scratch cargo
/// check be legitimate before the project exists, while keeping a chat task off it.
pub fn task_mentions_cargo(task: &str) -> bool {
    let p = task.to_ascii_lowercase();
    ["cargo", "rust", "crate", "src/main.rs", "src/lib.rs", ".rs"].iter().any(|k| p.contains(k))
}

/// A cargo check for a workspace with no Cargo.toml and a task that never mentioned Rust is the
/// model having invented one. Measured: "reply with the word pong" became
/// `cargo run -- pong == gnop`, and the run then failed forever on a check nobody asked for.
pub fn is_hallucinated_cargo_check(check: &str, cwd: &Path, task: &str) -> bool {
    check.contains("cargo") && !cwd.join("Cargo.toml").exists() && !task_mentions_cargo(task)
}

/// The floor: what can be checked about this workspace without asking anybody. A cargo project
/// at least has to build, and has to pass its tests when it has any.
pub fn cargo_floor(cwd: &Path) -> Option<String> {
    if !cwd.join("Cargo.toml").is_file() {
        return None;
    }
    let has_tests = std::process::Command::new("sh")
        .arg("-c")
        .arg("grep -rqs '#\\[test\\]' src tests 2>/dev/null")
        .current_dir(cwd)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    Some(if has_tests { "cargo build -q && cargo test -q" } else { "cargo build -q" }.to_string())
}

/// Ask the model to formalize the task into a deterministic check, and build the command from
/// the structured answer. `None` when there is nothing machine-checkable to build — which is an
/// honest answer, not a failure: the caller falls back to the floor or to [`judge`].
pub async fn derive_check(backend: &dyn ChatBackend, task: &str) -> Option<String> {
    let prompt = format!(
        "Set up the acceptance CHECK for this coding task. Reply with ONLY a JSON object, no prose:\n\
         {{\"checkable\": <true|false>, \"cargo_test\": <true|false>, \"run\": [{{\"arg\": \"<argument VALUE only>\", \"expect\": \"<exact stdout>\"}}]}}\n\
         - \"run\": one entry per concrete example the task states as `cargo run -- X` printing `Y`.\n\
           `arg` is JUST the argument value X — e.g. for \"cargo run -- hello prints olleh\", arg is \
           \"hello\" (NOT \"cargo run -- hello\"); `expect` is JUST Y, e.g. \"olleh\". Omit if no example.\n\
         - cargo_test=true ONLY if the task explicitly requires a unit test to pass.\n\
         - checkable=false if the task has no machine-checkable build/run/test criterion. In particular, \
         if the task is NOT about a Rust program — it only asks for a chat reply, an explanation, or \
         plain text (e.g. \"reply with the word pong\") — return checkable=false. The acceptance is the \
         reply itself, which is NOT a build/run/test; do NOT invent a `cargo run` or an argument for it.\n\n\
         Task:\n{task}"
    );
    let j = first_json(&ask(backend, &prompt, 300).await?)?;
    if !j["checkable"].as_bool().unwrap_or(false) {
        return None;
    }
    let mut parts = vec!["cargo build -q".to_string()];
    if j["cargo_test"].as_bool().unwrap_or(false) {
        parts.push("cargo test -q".to_string());
    }
    for r in j["run"].as_array().into_iter().flatten() {
        if let (Some(arg), Some(exp)) = (r["arg"].as_str(), r["expect"].as_str()) {
            parts.push(cargo_run_fragment(arg, exp));
        }
    }
    Some(parts.join(" && "))
}

/// Run the check in `cwd`. Returns `(passed, the tail of what it actually printed)` — the second
/// half is the repair prompt, so cargo's progress noise is dropped and the diagnosis kept.
pub async fn run_check(cmd: &str, cwd: &Path) -> (bool, String) {
    let (cmd, cwd) = (cmd.to_string(), cwd.to_path_buf());
    let out = tokio::task::spawn_blocking(move || {
        std::process::Command::new("sh").arg("-c").arg(&cmd).current_dir(&cwd).output()
    })
    .await
    .ok()
    .and_then(|r| r.ok());
    match out {
        Some(o) => {
            let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
            s.push_str(&String::from_utf8_lossy(&o.stderr));
            let lines: Vec<&str> = s
                .lines()
                .filter(|l| {
                    let t = l.trim_start();
                    !t.starts_with("Compiling")
                        && !t.starts_with("Finished")
                        && !t.starts_with("Running")
                })
                .collect();
            (o.status.success(), lines[lines.len().saturating_sub(40)..].join("\n"))
        }
        None => (false, "verify command failed to run".to_string()),
    }
}

/// The workspace's own source, for a reader that cannot run anything.
pub fn source_snapshot(cwd: &Path, max_bytes: usize) -> Option<String> {
    let mut rels = vec!["Cargo.toml".to_string(), "src/main.rs".to_string(), "src/lib.rs".to_string()];
    if let Ok(rd) = std::fs::read_dir(cwd.join("tests")) {
        let mut tests: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".rs").then(|| format!("tests/{name}"))
            })
            .collect();
        tests.sort();
        rels.extend(tests.into_iter().take(4));
    }
    let sections: Vec<String> = rels
        .into_iter()
        .filter_map(|rel| {
            let body = std::fs::read_to_string(cwd.join(&rel)).ok()?;
            (body.len() <= max_bytes).then(|| format!("--- {rel} ---\n{body}"))
        })
        .collect();
    (!sections.is_empty()).then(|| sections.join("\n\n"))
}

/// Semantic judgement for a task with no deterministic check. Only ever consulted when the
/// alternative is a bare build floor that cannot see whether the task was done at all.
pub async fn judge(backend: &dyn ChatBackend, task: &str, cwd: &Path) -> Verdict {
    let code = source_snapshot(cwd, 8192).unwrap_or_else(|| "(no source found)".to_string());
    let prompt = format!(
        "You are a strict code reviewer judging whether the CODE accomplishes the TASK. Reply with ONLY \
         a JSON object, no prose: {{\"pass\": <true|false>, \"reason\": \"<one short sentence>\"}}.\n\
         Rule pass=false ONLY if the code clearly fails a STATED requirement of the task; if it plausibly \
         satisfies the task, pass=true. Do not invent requirements the task did not state.\n\n\
         TASK:\n{task}\n\nCODE:\n{code}"
    );
    match ask(backend, &prompt, 200).await {
        Some(text) => parse_verdict(&text),
        None => Verdict::Unknown("model-judge unavailable or timed out".to_string()),
    }
}

/// Parse a judge reply. A parseable explicit boolean is evidence; anything else is Unknown.
pub fn parse_verdict(text: &str) -> Verdict {
    match first_json(text) {
        Some(j) if j["pass"].as_bool() == Some(false) => {
            let reason = j["reason"].as_str().unwrap_or("task requirement not met").trim().to_string();
            Verdict::Fail(format!("model-judge ruled the task NOT accomplished: {reason}"))
        }
        Some(j) if j["pass"].as_bool() == Some(true) => Verdict::Pass,
        Some(_) => Verdict::Unknown("model-judge response has no boolean `pass` field".to_string()),
        None => Verdict::Unknown("model-judge response is not valid verdict JSON".to_string()),
    }
}

/// The message a failed check sends back into the conversation. It carries the command and what
/// it actually printed, because "it failed" is not something a model can act on and the output
/// is — this is the whole difference between a repair round and another guess.
pub fn repair_prompt(check: &str, output: &str) -> String {
    format!(
        "The acceptance check for this task FAILED. This is the real output, not a summary.\n\n\
         $ {check}\n{output}\n\n\
         Fix the cause and make the check pass. Do not explain the failure instead of fixing it, \
         and do not report success until you have run the check yourself and read what it printed."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_derived_run_check_reports_what_it_saw() {
        let frag = cargo_run_fragment("3 4 + 2 *", "14");
        // The expected value and the argument are both quoted — a model cannot inject shell.
        assert!(frag.contains("'3 4 + 2 *'") && frag.contains("'14'"), "{frag}");
        // And on a mismatch it PRINTS both, which is what the repair round reads.
        assert!(frag.contains("printed") && frag.contains("expected"), "{frag}");
        // A quote in the model's string stays inert.
        let nasty = cargo_run_fragment("'; rm -rf /; echo '", "x");
        assert!(!nasty.contains("; rm -rf /;") || nasty.contains("'\\''"), "{nasty}");
    }

    #[test]
    fn an_invented_cargo_check_is_recognized() {
        let d = tempfile::tempdir().unwrap();
        // No Cargo.toml, and a task that never asked for Rust: the check is invented.
        assert!(is_hallucinated_cargo_check("cargo run -q -- pong", d.path(), "reply with pong"));
        // The same check for a task that DOES ask for Rust is legitimate — the project is
        // about to be created.
        assert!(!is_hallucinated_cargo_check(
            "cargo build -q",
            d.path(),
            "write a Rust program that prints pong"
        ));
        // And once the project exists, nothing is invented.
        std::fs::write(d.path().join("Cargo.toml"), "[package]").unwrap();
        assert!(!is_hallucinated_cargo_check("cargo build -q", d.path(), "reply with pong"));
    }

    #[test]
    fn the_floor_is_build_plus_tests_only_when_there_are_tests() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(cargo_floor(d.path()), None, "not a cargo project → no floor");
        std::fs::write(d.path().join("Cargo.toml"), "[package]\nname='x'").unwrap();
        assert_eq!(cargo_floor(d.path()).as_deref(), Some("cargo build -q"));
        std::fs::create_dir_all(d.path().join("src")).unwrap();
        std::fs::write(d.path().join("src/lib.rs"), "#[test]\nfn t() {}").unwrap();
        assert_eq!(cargo_floor(d.path()).as_deref(), Some("cargo build -q && cargo test -q"));
    }

    #[test]
    fn a_verdict_without_evidence_is_unknown_not_a_pass() {
        assert_eq!(parse_verdict(r#"{"pass": true}"#), Verdict::Pass);
        assert!(matches!(parse_verdict(r#"{"pass": false, "reason": "no test"}"#), Verdict::Fail(_)));
        // The three shapes that must never read as success.
        assert!(matches!(parse_verdict("looks good to me!"), Verdict::Unknown(_)));
        assert!(matches!(parse_verdict(r#"{"verdict": "ok"}"#), Verdict::Unknown(_)));
        assert!(matches!(parse_verdict(""), Verdict::Unknown(_)));
    }

    #[tokio::test]
    async fn a_failing_check_comes_back_with_the_output() {
        let d = tempfile::tempdir().unwrap();
        let (ok, out) = run_check("echo hello; exit 1", d.path()).await;
        assert!(!ok);
        assert!(out.contains("hello"), "the repair round needs the real output: {out}");
        let (ok, _) = run_check("true", d.path()).await;
        assert!(ok);
    }

    #[test]
    fn the_repair_prompt_carries_the_command_and_the_output() {
        let p = repair_prompt("cargo test -q", "assertion failed: 4 + 4 = 7");
        assert!(p.contains("cargo test -q") && p.contains("4 + 4 = 7"), "{p}");
        assert!(p.contains("Do not explain"), "it must ask for a fix, not an explanation: {p}");
    }
}
