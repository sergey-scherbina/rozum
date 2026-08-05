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
//!    data (`{"checkable":bool,"cargo_test":bool,"run":[{"args":[..],"expect"}]}`), and *we* build the
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

/// Strip one symmetric pair of delimiting quotes.
///
/// A task writes `cargo run -- "3 4 + 2 *"`, and the model returns the argument the way the task
/// spelled it — quotes and all. The check then demands a program that accepts a quoted argument,
/// which nobody asked for and no correct implementation provides; measured 2026-08-04, it cost a
/// run both of its repair rounds. The quotes are how the task DELIMITED the value, not part of it.
///
/// Exactly one pair, and only when it is symmetric: `"a"` → `a`, but `"a` stays `"a` (unbalanced,
/// so probably real), and `he said "hi"` stays whole (the quotes are inside, not around).
/// `""` → `` — an empty expectation is a legitimate thing to check for.
fn unquote(s: &str) -> &str {
    let t = s.trim();
    for q in ['"', '\''] {
        if t.len() >= 2 && t.starts_with(q) && t.ends_with(q) {
            let inner = &t[1..t.len() - 1];
            // Only if the pair really wraps: an inner copy of the same quote means the string is
            // something like `"a" + "b"`, where stripping the ends would corrupt it.
            if !inner.contains(q) {
                return inner;
            }
        }
    }
    t
}

/// Split a command line the way a shell does — whitespace separates, quotes group.
///
/// Deliberately small: no escapes, no expansion. It reads the argument list a TASK wrote, and a
/// task that needs `\$` in an example is past the point where a derived one-line check helps.
pub fn shell_lex(s: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut has) = (Vec::new(), String::new(), None::<char>, false);
    for c in s.chars() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                has = true;
            }
            None if c.is_whitespace() => {
                if has || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            None => cur.push(c),
        }
    }
    if has || !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Does the task actually state this expected output?
///
/// Whitespace-insensitive on purpose: a task writes `prints `olleh`` and a model may answer
/// `olleh\n`, and a multi-line expectation may be reflowed. What it will not do is accept an
/// expectation the task never mentions — which is the whole point.
pub fn task_states(task: &str, expect: &str) -> bool {
    let squeeze = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
    let (t, e) = (squeeze(task), squeeze(expect));
    // An empty expectation is a real thing to check for ("prints nothing") and cannot be looked up.
    !e.is_empty() && t.contains(&e)
}

/// The arity of an example, taken from the TASK rather than from the model.
///
/// The model is good at "what should this print" and bad at shell lexing, and lexing is the one
/// part we can do exactly — the task already wrote the argument list, with its quotes. Measured
/// 2026-08-04, both directions in one afternoon: asked for a single string it merged
/// `cargo run -- 3 4` into one argument, and asked for a list it split `cargo run -- "3 4 + 2 *"`
/// into five. Each answer is a false negative against a program that does exactly what was asked.
///
/// So: lex what follows `cargo run --` in the task, and find the shortest prefix whose words are
/// the value the model reported. That prefix IS the argument list, quotes and all; the prose that
/// follows the example ("must print 14") never matches and never enters. `None` when the task
/// states no such example — then the model's own list stands, which is all we had before.
pub fn task_argv_for(task: &str, joined: &str) -> Option<Vec<String>> {
    let want: Vec<&str> = joined.split_whitespace().collect();
    if want.is_empty() {
        return None;
    }
    for (i, _) in task.match_indices("cargo run --") {
        let tail = &task[i + "cargo run --".len()..];
        let tail = tail.lines().next().unwrap_or(tail);
        let lexed = shell_lex(tail);
        for n in 1..=lexed.len() {
            let prefix = &lexed[..n];
            let flat: Vec<&str> = prefix.iter().flat_map(|w| w.split_whitespace()).collect();
            if flat == want {
                return Some(prefix.to_vec());
            }
        }
    }
    None
}

/// A `cargo run` check that SAYS what it saw. A bare `[ "$(cargo run …)" = … ]` fails silently,
/// which leaves the repair round with an empty "real error" on exactly the mismatches that
/// matter most — the ones where the program runs and prints the wrong thing.
pub fn cargo_run_fragment(arg: &str, expect: &str) -> String {
    cargo_run_fragment_args(&[arg.to_string()], expect)
}

/// The same check for a program invoked with SEVERAL arguments.
///
/// Arity is part of the criterion, and the single-string form could not express it. Measured
/// 2026-08-04 end-to-end: for "cargo run -- 3 4 must print 7" the model returned `arg = "3 4"`,
/// which quotes into ONE literal, so the check ran `cargo run -q -- '3 4'` against a program that
/// correctly takes two arguments — it exited 1, the gate spent both repair rounds and reported
/// FAILED on work that was right. `cargo run -- 3 4` prints `7`, by hand, in the same workspace.
///
/// A false negative is the expensive kind of gate defect: the operator is told correct work is
/// broken, and the model is sent to break it.
pub fn cargo_run_fragment_args(args: &[String], expect: &str) -> String {
    let quoted: Vec<String> = args.iter().map(|a| shquote(unquote(a))).collect();
    let (argv, exp_q) = (quoted.join(" "), shquote(unquote(expect)));
    format!(
        "{{ out=$(cargo run -q -- {argv}) && [ \"$out\" = {exp_q} ] || \
         {{ printf 'cargo run -- %s printed <%s>; expected <%s>\\n' {argv_msg} \"$out\" {exp_q} >&2; exit 1; }}; }}",
        argv_msg = shquote(&args.iter().map(|a| unquote(a)).collect::<Vec<_>>().join(" "))
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
         {{\"checkable\": <true|false>, \"cargo_test\": <true|false>, \"run\": [{{\"args\": [\"<one entry PER command-line argument>\"], \"expect\": \"<exact stdout>\"}}]}}\n\
         - \"run\": one entry per concrete example the task states as `cargo run -- X` printing `Y`.\n\
           `args` is the argument LIST X, one entry per argument — for \"cargo run -- hello prints \
           olleh\", args is [\"hello\"] (NOT [\"cargo run -- hello\"]); `expect` is JUST Y, e.g. \"olleh\".\n\
         - HOW MANY entries is decided by the task's spacing and quotes, and it matters: \
         `cargo run -- 3 4` printing 7 is TWO arguments, args [\"3\", \"4\"] — while \
         `cargo run -- \"3 4 + 2 *\"` printing 14 is ONE argument, args [\"3 4 + 2 *\"]. Quotes that \
         DELIMIT a value are not part of it: no surrounding quotes inside the entries or in expect.\n\
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
        let Some(exp) = r["expect"].as_str() else { continue };
        // `args` carries the arity the single string could not; `arg` stays valid as one argument.
        let argv: Vec<String> = match (r["args"].as_array(), r["arg"].as_str()) {
            (Some(a), _) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
            (None, Some(one)) => vec![one.to_string()],
            (None, None) => continue,
        };
        if argv.is_empty() {
            continue;
        }
        // The EXPECTATION has to come from the task too. A task that says what the program does
        // ("print the top 3 words") without saying what it prints has no run-check in it, and a
        // model asked for one invents it: measured 2026-08-05 on `wordcount`, where the task
        // states no output and the derived check demanded `a 3 / c 2 / d 2` — three lines that
        // appear nowhere, for an input file the model never read. A correct program fails that
        // check, which is the same false negative as BUG-018 arriving through the schema's other
        // field. `checkable: false` was the right answer and the prompt already asks for it; this
        // is the deterministic half, because the model does not always give it.
        if !task_states(task, exp) {
            continue;
        }
        // Arity comes from the task's own punctuation when the task states the example; the
        // model's grouping is only the fallback for a task that describes one in prose.
        let joined = argv.iter().map(|a| unquote(a)).collect::<Vec<_>>().join(" ");
        let argv = task_argv_for(task, &joined).unwrap_or(argv);
        parts.push(cargo_run_fragment_args(&argv, exp));
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

/// What the agent actually produced, for a reader that cannot run anything.
///
/// Rust sources first, because that is the common case — then, when there are none, whatever
/// small text files the workspace holds, and finally its listing. **A judge shown nothing rules
/// against you**: measured 2026-08-04, a task whose whole result was `x.txt` containing `hi` got
/// `(no source found)`, a verdict of "not accomplished", two repair rounds and a reported failure
/// — while the file sat on disk, correct. The gate's own rule is that unverified must not read as
/// failed, and the first version of this function broke it for every task that is not a cargo
/// project.
pub fn artifact_snapshot(cwd: &Path, max_bytes: usize) -> Option<String> {
    if let Some(rust) = source_snapshot(cwd, max_bytes) {
        return Some(rust);
    }
    // No Rust sources. Show the small text files instead — a task whose result is a text file, a
    // config or a script is not less checkable, it is just not Rust.
    let mut sections: Vec<String> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(cwd).ok()?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        names.push(name.clone());
        if sections.len() >= 5 || !entry.path().is_file() {
            continue;
        }
        if let Ok(body) = std::fs::read_to_string(entry.path()) {
            if body.len() <= max_bytes {
                sections.push(format!("--- {name} ---\n{body}"));
            }
        }
    }
    if names.is_empty() {
        return None; // genuinely empty: nothing to judge, and Unknown is the honest verdict
    }
    let listing = format!("--- files in the workspace ---\n{}", names.join("\n"));
    Some(if sections.is_empty() { listing } else { format!("{listing}\n\n{}", sections.join("\n\n")) })
}

/// The workspace's own Rust source, for a reader that cannot run anything.
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
pub async fn judge(backend: &dyn ChatBackend, task: &str, cwd: &Path, answer: &str) -> Verdict {
    let artifacts = artifact_snapshot(cwd, 8192);
    // Nothing on disk AND nothing said: there is no evidence either way, and a judge asked to
    // rule on nothing rules against you. Unknown is the honest verdict — the caller reports "not
    // checked", which is what actually happened.
    if artifacts.is_none() && answer.trim().is_empty() {
        return Verdict::Unknown("nothing to judge: no files and no answer".to_string());
    }
    let evidence = match artifacts {
        Some(a) => format!("WHAT IS IN THE WORKSPACE:\n{a}"),
        None => "WHAT IS IN THE WORKSPACE:\n(nothing — the task may not have been about files)".to_string(),
    };
    // The answer is evidence too, and for a task whose result IS the answer ("reply with the
    // word pong") it is the only evidence there is.
    let prompt = format!(
        "You are a strict reviewer judging whether the WORK accomplishes the TASK. Reply with ONLY \
         a JSON object, no prose: {{\"pass\": <true|false>, \"reason\": \"<one short sentence>\"}}.\n\
         Rule pass=false ONLY if the work clearly fails a STATED requirement of the task; if it \
         plausibly satisfies the task, pass=true. Do not invent requirements the task did not state. \
         If the task asked for a reply rather than for files, judge the reply.\n\n\
         TASK:\n{task}\n\n{evidence}\n\nWHAT THE AGENT REPLIED:\n{answer}"
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

/// Where the cargo project actually is, when it is not where the check runs.
///
/// `cargo new <name>` creates a SUBDIRECTORY, and a check that runs `cargo` at the workspace root
/// then cannot pass however good the code is. Measured 2026-08-04: both repair rounds of a run
/// went on rediscovering that instead of fixing the task.
///
/// Returns the name of the single immediate child holding a `Cargo.toml`, when the root holds
/// none. Ambiguity (several children with manifests) returns `None` — a hint that names the wrong
/// directory is worse than no hint.
///
/// This is a DIAGNOSTIC, deliberately not a relocation: the check is not moved into the
/// subdirectory, because that would turn work delivered somewhere nobody asked for into a passing
/// run — the exact failure the gate exists to remove (`docs/specs/verify-gate.md` §B).
pub fn misplaced_project(cwd: &Path) -> Option<String> {
    if cwd.join("Cargo.toml").is_file() {
        return None;
    }
    let mut found: Option<String> = None;
    for entry in std::fs::read_dir(cwd).ok()?.flatten() {
        if !entry.path().join("Cargo.toml").is_file() {
            continue;
        }
        if found.is_some() {
            return None; // two candidates: say nothing rather than the wrong thing
        }
        found = Some(entry.file_name().to_string_lossy().into_owned());
    }
    found
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

/// [`repair_prompt`] plus, when it applies, the one diagnosis a model cannot reach by reading the
/// error: the check runs at the workspace root and the project is one level down.
pub fn repair_prompt_in(check: &str, output: &str, cwd: &Path) -> String {
    let base = repair_prompt(check, output);
    match misplaced_project(cwd) {
        Some(dir) => format!(
            "{base}\n\n\
             NOTE: the check runs in the workspace ROOT, and there is no Cargo.toml there — the \
             project is in `{dir}/`. `cargo new {dir}` created a subdirectory; the project has to \
             be in the root itself. Move it up (`mv {dir}/* {dir}/.* . 2>/dev/null; rmdir {dir}`) \
             or recreate it with `cargo init` in the root, then run the check again."
        ),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_expectation_the_task_never_states_is_not_checked() {
        // The measured case: `wordcount` says what the program must DO and never what it prints,
        // and the derived check demanded three lines the task does not contain — for a data file
        // the model never opened. A correct program fails that check forever.
        let wordcount = "create a Rust binary that reads a text file, counts words \
                         case-insensitively and prints the top 3 as `word count`. \
                         Verify with `cargo run -- input.txt`.";
        assert!(!task_states(wordcount, "a 3\nc 2\nd 2"));

        // What the task DOES state is checkable, whitespace and case aside.
        let reverse = "fix the bug: `cargo run -- hello` must print exactly `olleh`";
        assert!(task_states(reverse, "olleh"));
        assert!(task_states(reverse, " OLLEH "));
        assert!(!task_states(reverse, "hello world"));
        // An empty expectation cannot be looked up, so it is not accepted by this route.
        assert!(!task_states(reverse, ""));
    }

    #[test]
    fn arity_comes_from_the_task_not_from_the_model() {
        // Both directions, both measured end-to-end on 2026-08-04 with the same 4B model.
        // Asked for one string it merged two arguments; asked for a list it split a quoted one
        // into five. The task said which it was, in both cases, in its own punctuation.
        let two = "print the sum of two arguments: cargo run -- 3 4 must print 7";
        assert_eq!(task_argv_for(two, "3 4"), Some(vec!["3".to_string(), "4".to_string()]));

        let one = r#"an RPN calculator: cargo run -- "3 4 + 2 *" must print 14"#;
        assert_eq!(task_argv_for(one, "3 4 + 2 *"), Some(vec!["3 4 + 2 *".to_string()]));
        // Even when the model reports it pre-split, which is exactly what it did.
        assert_eq!(task_argv_for(one, "3 4 + 2 *"), Some(vec!["3 4 + 2 *".to_string()]));

        // The prose after the example is not swept in: the match stops at the reported value,
        // so the check runs `3 4` and not the rest of the sentence.
        assert_eq!(task_argv_for(two, "3 4").unwrap().len(), 2);
        // A value the task never states leaves the model's grouping alone rather than inventing.
        assert_eq!(task_argv_for(two, "5 6"), None);
        // A task that states no example leaves the model's grouping alone.
        assert_eq!(task_argv_for("make the greeting friendlier", "hello"), None);
    }

    #[test]
    fn the_lexer_groups_what_the_quotes_group() {
        assert_eq!(shell_lex(r#" "3 4 + 2 *" must print 14"#), vec!["3 4 + 2 *", "must", "print", "14"]);
        assert_eq!(shell_lex("3 4 must print 7"), vec!["3", "4", "must", "print", "7"]);
        // An empty argument is a real one — `cargo run -- ""` is a thing a task can ask for.
        assert_eq!(shell_lex(r#"a "" b"#), vec!["a", "", "b"]);
    }

    #[test]
    fn two_arguments_stay_two_arguments() {
        // The end-to-end finding (2026-08-04, Scala 3 gate, task "cargo run -- 3 4 must print 7"):
        // the single-string form quoted BOTH numbers into one literal, the check ran
        // `cargo run -q -- '3 4'` against a correct two-argument program, and the gate reported
        // FAILED after spending both repair rounds. `cargo run -- 3 4` printed `7` by hand.
        let frag = cargo_run_fragment_args(&["3".into(), "4".into()], "7");
        assert!(frag.contains("cargo run -q -- '3' '4'"), "{frag}");
        // The command that RUNS must not merge them; the message may spell them as a command line.
        let cmd = frag.split("||").next().unwrap();
        assert!(!cmd.contains("'3 4'"), "the two arguments were merged again: {frag}");
    }

    #[test]
    fn one_argument_with_spaces_stays_one_argument() {
        // The other half of the same rule, and the reason arity cannot be guessed from whitespace:
        // this task really does pass a single argument that contains spaces.
        let frag = cargo_run_fragment_args(&["3 4 + 2 *".into()], "14");
        assert!(frag.contains("cargo run -q -- '3 4 + 2 *'"), "{frag}");
    }

    #[test]
    fn a_multi_argument_mismatch_says_the_whole_command_line() {
        // What the repair round reads. Printing only the first argument would describe a command
        // nobody ran.
        let frag = cargo_run_fragment_args(&["3".into(), "4".into()], "7");
        let msg = frag.split("printf").nth(1).unwrap();
        assert!(msg.contains("'3 4'"), "the message named only part of the command line: {frag}");
        assert!(frag.contains("printed") && frag.contains("expected"), "{frag}");
    }

    #[test]
    fn a_derived_run_check_reports_what_it_saw() {
        let frag = cargo_run_fragment("3 4 + 2 *", "14");
        // The expected value and the argument are both quoted — a model cannot inject shell.
        assert!(frag.contains("'3 4 + 2 *'") && frag.contains("'14'"), "{frag}");
        // And on a mismatch it PRINTS both, which is what the repair round reads.
        assert!(frag.contains("printed") && frag.contains("expected"), "{frag}");
    }

    #[tokio::test]
    async fn a_model_supplied_string_cannot_execute_anything() {
        // Proven by RUNNING it, not by looking at it. The previous version of this test asserted
        // on the shape of the escaping — which meant it started failing the moment the quoting
        // changed, while the fragment was still perfectly inert. A test that watches for the
        // effect survives the implementation changing under it.
        let d = tempfile::tempdir().unwrap();
        let sentinel = d.path().join("pwned");
        for payload in [
            "'; touch pwned; echo '",
            "\"; touch pwned; echo \"",
            "$(touch pwned)",
            "`touch pwned`",
            "x'; touch pwned; #",
        ] {
            let frag = cargo_run_fragment(payload, "expected");
            // The fragment runs `cargo run` in a directory with no project: it fails, which is
            // fine — what matters is that nothing else ran.
            let _ = run_check(&frag, d.path()).await;
            assert!(!sentinel.exists(), "payload executed: {payload}\n{frag}");
        }
    }

    #[test]
    fn a_delimiting_quote_is_not_part_of_the_argument() {
        // The measured case: the task wrote `cargo run -- "3 4 + 2 *"`, the model returned the
        // argument the way the task spelled it, and the check then demanded a program that
        // accepts a quoted argument. One symmetric pair is delimiting and comes off.
        let frag = cargo_run_fragment("\"3 4 + 2 *\"", "\"14\"");
        assert!(frag.contains("'3 4 + 2 *'"), "the quotes were the task's, not the value's: {frag}");
        assert!(frag.contains("'14'"), "{frag}");
        assert!(!frag.contains("\\\""), "no doubled quoting should survive: {frag}");

        // What must NOT be stripped, because there the quotes are data.
        assert_eq!(unquote("hello"), "hello");
        assert_eq!(unquote("\"unbalanced"), "\"unbalanced");
        assert_eq!(unquote("he said \"hi\""), "he said \"hi\"");
        assert_eq!(unquote("\"a\" + \"b\""), "\"a\" + \"b\"");
        // Exactly one pair, not all of them: a doubly-quoted value keeps its inner pair.
        assert_eq!(unquote("'\"x\"'"), "\"x\"");
        // Both quote characters, and an empty expectation is a real thing to check for.
        assert_eq!(unquote("'y'"), "y");
        assert_eq!(unquote("\"\""), "");
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
    fn the_judge_is_shown_what_was_actually_produced() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // The measured case: a task whose entire result is one text file. The first version
        // looked only for Cargo.toml/src/*.rs, showed the judge "(no source found)", and got a
        // verdict of NOT accomplished — for work that was sitting on disk, correct.
        std::fs::write(root.join("x.txt"), "hi").unwrap();
        let snap = artifact_snapshot(root, 8192).expect("a workspace with a file is not empty");
        assert!(snap.contains("x.txt"), "the file must be visible to the judge: {snap}");
        assert!(snap.contains("hi"), "and so must its content: {snap}");

        // Rust still wins when it is there — that is the common case and the richer evidence.
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        let snap = artifact_snapshot(root, 8192).unwrap();
        assert!(snap.contains("src/main.rs"), "{snap}");

        // A genuinely empty workspace is None — which is what lets `judge` answer Unknown
        // instead of asking a model to rule on nothing.
        let empty = tempfile::tempdir().unwrap();
        assert!(artifact_snapshot(empty.path(), 8192).is_none());
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
    fn a_project_one_level_down_is_named_rather_than_accommodated() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // Nothing there yet: no diagnosis to offer.
        assert_eq!(misplaced_project(root), None);

        // `cargo new rpn` — the measured case.
        std::fs::create_dir(root.join("rpn")).unwrap();
        std::fs::write(root.join("rpn/Cargo.toml"), "[package]").unwrap();
        assert_eq!(misplaced_project(root).as_deref(), Some("rpn"));
        let p = repair_prompt_in("cargo build -q", "error: could not find Cargo.toml", root);
        assert!(p.contains("`rpn/`"), "the hint must NAME the directory: {p}");
        assert!(p.contains("cargo init"), "and say how to fix it: {p}");

        // Two candidates: a hint that names the wrong one is worse than none.
        std::fs::create_dir(root.join("other")).unwrap();
        std::fs::write(root.join("other/Cargo.toml"), "[package]").unwrap();
        assert_eq!(misplaced_project(root), None);

        // A project IN the root is not misplaced, whatever else is lying around.
        std::fs::write(root.join("Cargo.toml"), "[package]").unwrap();
        assert_eq!(misplaced_project(root), None);
        let p = repair_prompt_in("cargo build -q", "error[E0308]", root);
        assert!(!p.contains("NOTE:"), "no hint when the project is where it belongs: {p}");
    }

    #[test]
    fn the_repair_prompt_carries_the_command_and_the_output() {
        let p = repair_prompt("cargo test -q", "assertion failed: 4 + 4 = 7");
        assert!(p.contains("cargo test -q") && p.contains("4 + 4 = 7"), "{p}");
        assert!(p.contains("Do not explain"), "it must ask for a fix, not an explanation: {p}");
    }
}
