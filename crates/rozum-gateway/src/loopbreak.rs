//! Agentic stuck-loop detection (the server-side loop-breaker).
//!
//! Extracted verbatim from `gateway.rs` (gw-monolith-decompose): the pure detection logic —
//! `detect_stuck_loop` (identical-failing-call + edit-churn signatures) and its helpers. No
//! gateway-internal deps beyond the shared `backend` types. The async integration point
//! (`chat_or_loopbreak`) stays in `gateway.rs`; the regression tests stay in its test module and
//! reach these via `super::*`.
use crate::backend::{ChatEvent, ChatStream, ContentBlock, Message, Role, StopReason};
use serde_json::Value;

/// Number of identical, consecutively-failing tool calls that mark a stuck agent.
pub(crate) const STUCK_LOOP_THRESHOLD: usize = 3;

/// Edit-churn (signature 3): a single file edited this many times *with* a ping-pong
/// (an added line re-introduces a previously-removed one) marks a model going in circles.
pub(crate) const EDIT_CHURN_MIN: usize = 3;
/// Marker carried by the first churn message, so the second detection can see that the model has
/// already been told once. The escalation is read from the transcript, not held as state.
pub(crate) const CHURN_NUDGE_MARK: &str = "[rozum: you just undid your own edit]";
/// Backstop: a single file edited this many times is churning even without a strict ping-pong.
pub(crate) const EDIT_CHURN_BACKSTOP: usize = 6;
/// Signature 5: the same edit payload applied successfully to one file this many times. See the
/// signature for why three and not two — it is a measured number, not a chosen one.
const IDENTICAL_EDIT_THRESHOLD: usize = 3;
/// Signature 6: a weak model re-reading one target (varying offset/limit each time, so sigs 1/4
/// never see a byte-identical call) this many times without ever having made a single mutating
/// call, anywhere in the conversation. Higher than the edit thresholds: legitimately reading a
/// large file in several chunks before acting is normal, so this must not fire on ordinary
/// chunked reading — only on reading that never converges to an action.
const READ_CHURN_MIN: usize = 6;
/// Marker carried by the first read-churn nudge, mirrored on [`CHURN_NUDGE_MARK`].
pub(crate) const READ_NUDGE_MARK: &str = "[rozum: you're re-reading instead of acting]";
/// A ping-pong needs this share of the edit's substantive added lines to be lines a previous edit
/// removed. One shared line is not evidence of anything: see [`is_pingpong`].
const PINGPONG_MIN_SHARE: f64 = 0.5;

/// Is this line worth anything as evidence that the model is going in circles?
///
/// A line with no alphanumeric character in it — `}`, `{`, `});`, `);` — is punctuation that
/// closes whatever came before, and it recurs in every edit anyone has ever made to a braced
/// language. Matching on it made two ordinary, forward edits look like a circle.
fn is_substantive(line: &str) -> bool {
    line.chars().any(|c| c.is_alphanumeric())
}

/// Ping-pong: is this edit putting back what an earlier edit took out?
///
/// Two guards, because the single-shared-line test this replaces fired on a closing brace and
/// aborted three benchmark runs before the model reached `cargo test` (2026-08-16, the `duration`
/// cells — the model's implementation was correct and the harness stopped it anyway):
///
/// 1. Only substantive lines are evidence at all ([`is_substantive`]).
/// 2. The re-introduced lines must be at least [`PINGPONG_MIN_SHARE`] of what this edit ADDS. Real
///    churn re-applies a version of the same thing, so nearly everything it adds is something it
///    removed a moment ago; an edit that adds twenty new lines and happens to repeat one is doing
///    new work. The share, not a line count, is what tells those apart — a genuine one-line
///    toggle (`collect::<String>()` ⟷ `collect()`) is 1 of 1, and must still fire.
fn is_pingpong(added: &[String], seen: &std::collections::HashSet<String>) -> bool {
    let mut distinct: Vec<&String> = added.iter().filter(|l| is_substantive(l)).collect();
    distinct.sort();
    distinct.dedup();
    if distinct.is_empty() {
        return false;
    }
    let back = distinct.iter().filter(|l| seen.contains(**l)).count();
    back as f64 / distinct.len() as f64 >= PINGPONG_MIN_SHARE
}

/// From one tool-call input, extract `(file, removed_lines, added_lines)` if it carries a
/// patch/edit. Shape-agnostic: stringifies the input and scans for a V4A/unified envelope, so
/// it matches an `apply_patch` function call, a `{patch: …}` arg, or a rewritten `patch -p0`
/// heredoc alike. Lines are normalized (leading `+`/`-` and surrounding whitespace stripped)
/// for content comparison; `+++`/`---`/`@@`/`***` header lines are not change lines.
pub(crate) fn edit_target_and_lines(input: &Value) -> Option<(String, Vec<String>, Vec<String>)> {
    // Claude Code's structured edit tools (Edit / MultiEdit / Write) carry the target as a
    // `file_path` key with `old_string`/`new_string` (or `content`, or an `edits` array) — NOT a
    // diff/apply_patch body, so the codex-format scan below is blind to them and signature-3 churn
    // never fired for the CC harness (its 67-turn `fix` loops ran to timeout). Handle them directly.
    if let Some(file) = input.get("file_path").and_then(|v| v.as_str()) {
        fn lines(s: &str) -> Vec<String> {
            s.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect()
        }
        let mut removed = Vec::new();
        let mut added = Vec::new();
        // `file_path` alone is NOT an edit. `Read` carries one and nothing else, and counting it
        // put a third "edit" on every file the model had looked at before touching — which is the
        // normal way to work. Measured on the `duration` bench cells (2026-08-16): Read + two
        // ordinary Edits scored 3 against a threshold of 3, and all three runs were aborted before
        // the model ever reached `cargo test`. An edit is a call that carries an edit PAYLOAD.
        let mut is_edit = false;
        if let Some(s) = input.get("old_string").and_then(|v| v.as_str()) {
            is_edit = true;
            removed.extend(lines(s));
        }
        if let Some(s) = input.get("new_string").and_then(|v| v.as_str()) {
            is_edit = true;
            added.extend(lines(s));
        }
        // `Write` overwrites the whole file — count it as an edit (its content is the "added" side).
        if let Some(s) = input.get("content").and_then(|v| v.as_str()) {
            is_edit = true;
            added.extend(lines(s));
        }
        // `MultiEdit`: each entry is an old/new pair.
        if let Some(edits) = input.get("edits").and_then(|v| v.as_array()) {
            is_edit = true;
            for e in edits {
                if let Some(s) = e.get("old_string").and_then(|v| v.as_str()) {
                    removed.extend(lines(s));
                }
                if let Some(s) = e.get("new_string").and_then(|v| v.as_str()) {
                    added.extend(lines(s));
                }
            }
        }
        return is_edit.then(|| (file.to_string(), removed, added));
    }
    // Pull every string leaf out of the input so we see the patch body whatever key holds it.
    fn collect_strings(v: &Value, out: &mut String) {
        match v {
            Value::String(s) => {
                out.push_str(s);
                out.push('\n');
            }
            Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
            Value::Object(o) => o.values().for_each(|x| collect_strings(x, out)),
            _ => {}
        }
    }
    let mut text = String::new();
    collect_strings(input, &mut text);
    if !text.contains("*** Update File:") && !(text.contains("--- ") && text.contains("+++ ")) {
        return None;
    }
    let mut file: Option<String> = None;
    let mut removed = Vec::new();
    let mut added = Vec::new();
    for ln in text.lines() {
        if let Some(p) = ln.strip_prefix("*** Update File:") {
            file.get_or_insert_with(|| p.trim().to_string());
        } else if let Some(p) = ln.strip_prefix("+++ ") {
            let p = p.trim();
            file.get_or_insert_with(|| {
                p.strip_prefix("b/").or_else(|| p.strip_prefix("a/")).unwrap_or(p).to_string()
            });
        } else if let Some(p) = ln.strip_prefix("--- ") {
            let p = p.trim();
            file.get_or_insert_with(|| {
                p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p).to_string()
            });
        } else if ln.starts_with("@@") || ln.starts_with("*** ") {
            continue;
        } else if let Some(rest) = ln.strip_prefix('+') {
            let c = rest.trim();
            if !c.is_empty() {
                added.push(c.to_string());
            }
        } else if let Some(rest) = ln.strip_prefix('-') {
            let c = rest.trim();
            if !c.is_empty() {
                removed.push(c.to_string());
            }
        }
    }
    file.map(|f| (f, removed, added))
}

/// Tool names that only observe state. Used by signature 6 — re-reading one of these forever
/// without ever calling a [`MUTATING_TOOLS`] tool is the "stuck reading" shape, distinct from the
/// edit-churn signatures above (which need a mutating call to have happened at all).
const READ_ONLY_TOOLS: &[&str] = &["Read", "Grep", "Glob", "LS", "NotebookRead"];
/// Any of these anywhere in the conversation is evidence the model DID act, so signature 6 must
/// not fire — `Bash` is included even though many Bash calls are themselves read-only (`cat`,
/// `git status`) because treating it as "acted" only makes the signature more conservative, never
/// blind to a real loop: a model whose Bash calls are genuinely all inspection still has to reach
/// an Edit/Write eventually or another signature already covers it.
const MUTATING_TOOLS: &[&str] = &["Edit", "MultiEdit", "Write", "NotebookEdit", "Bash", "apply_patch"];

/// The file/path this read-only call targets, normalized so calls that differ only in
/// `offset`/`limit` (chunked reads of one file) still key together.
fn read_target(name: &str, input: &Value) -> Option<String> {
    if !READ_ONLY_TOOLS.contains(&name) {
        return None;
    }
    for key in ["file_path", "path", "notebook_path", "pattern"] {
        if let Some(s) = input.get(key).and_then(|v| v.as_str()) {
            return Some(format!("{name}:{s}"));
        }
    }
    None
}

/// Detect the agentic stuck-loop signature in the incoming conversation. A weak local
/// model that re-issues the same already-applied edit gets stuck retrying and runs to
/// `--max-turns` instead of stopping (root cause: SPRINT.md "agentic-loop-root-cause").
/// The gateway sees the whole conversation each turn, so it can short-circuit the next
/// doomed turn. Two signatures, because the loop surfaces differently per harness:
///
///  1. **Structured** (Codex / Responses, and CC when tool use completes): the last
///     `STUCK_LOOP_THRESHOLD` tool calls are byte-identical (same name + input) and each
///     got an **error** result — the model keeps re-sending an edit whose target text is
///     already gone (`String to replace not found`).
///  2. **Text-repeat** (Claude Code headless): CC *interrupts* the doomed tool use and
///     records the turn as a placeholder (`[Tool use interrupted]` / `(no content)`), so
///     the gateway never sees a structured call — only the same assistant text repeated.
///
/// Both are conservative: a healthy agent never re-sends a byte-identical failed call nor
/// repeats the same assistant text `STUCK_LOOP_THRESHOLD` times, so neither trips it.
pub(crate) fn detect_stuck_loop(messages: &[Message]) -> Option<String> {
    use std::collections::HashMap;
    // ── Signature 1: identical, consecutively-failing structured tool calls ──
    // Keep the result CONTENT, not just the error flag: signature 4 needs to tell a spin
    // apart from a verify loop, and that difference lives entirely in whether the output
    // changed between two identical calls.
    let mut results: HashMap<&str, (bool, &str)> = HashMap::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolResult { tool_use_id, is_error, content } = b {
                results.insert(tool_use_id.as_str(), (*is_error, content.as_str()));
            }
        }
    }
    let mut calls: Vec<(&str, &Value, bool, &str)> = Vec::new();
    for m in messages {
        for b in &m.content {
            if let ContentBlock::ToolUse { id, name, input } = b {
                let (err, out) = results.get(id.as_str()).copied().unwrap_or((false, ""));
                calls.push((name.as_str(), input, err, out));
            }
        }
    }
    if calls.len() >= STUCK_LOOP_THRESHOLD {
        let tail = &calls[calls.len() - STUCK_LOOP_THRESHOLD..];
        let (name0, input0, _, _) = tail[0];
        if tail.iter().all(|(n, i, e, _)| *e && *n == name0 && *i == input0) {
            return Some(format!(
                "The `{name0}` tool was called {STUCK_LOOP_THRESHOLD} times in a row with identical \
                 arguments and every call returned an error — the change has most likely already \
                 been applied. Stopping to avoid an infinite retry loop; verify and report."
            ));
        }
    }

    // ── Signature 2: no-progress repetition in the recent assistant turns ──
    // CC's interrupted-tool loop doesn't repeat one text *consecutively* — it ping-pongs
    // between a re-diagnosis ("The bug is in `reverse`…") and the `[Tool use interrupted]`
    // placeholder. So instead of "N identical in a row", fire when, within the recent
    // window, any single assistant text recurs `STUCK_LOOP_THRESHOLD` times — the model is
    // cycling the same outputs without making progress.
    let asst_texts: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .map(|m| {
            m.content
                .iter()
                .filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.trim()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .collect();
    const WINDOW: usize = 2 * STUCK_LOOP_THRESHOLD;
    let start = asst_texts.len().saturating_sub(WINDOW);
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for t in &asst_texts[start..] {
        if !t.is_empty() {
            *counts.entry(t.as_str()).or_default() += 1;
        }
    }
    if counts.values().any(|&c| c >= STUCK_LOOP_THRESHOLD) {
        return Some(
            "The last several assistant turns cycled the same outputs without progress (the tool \
             cycle is stuck — repeated re-attempts/interruptions). Stopping to avoid an infinite \
             loop; verify the current result and report it in one short line."
                .to_string(),
        );
    }

    // ── Signature 3: edit-churn / ping-pong ──
    // The model re-edits one file with *different, mostly-succeeding* patches, undoing and
    // redoing its own changes (toggling equivalent forms, re-anchoring on stale context). The
    // patches differ and don't error, so signatures 1 & 2 miss them; left running, fuzzy
    // re-applies corrupt the file (dup lines / unbalanced braces) and the run burns to timeout
    // with a broken file. Fire when one file is edited >=3 times AND a ping-pong occurred (an
    // added line re-introduces a previously-removed one), or >=6 times outright.
    let mut edits_per_file: HashMap<String, usize> = HashMap::new();
    let mut removed_seen: HashMap<String, std::collections::HashSet<String>> = HashMap::new();
    let mut pingpong_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut restored: HashMap<String, Vec<String>> = HashMap::new();
    // Only edits that LANDED. An edit whose tool call errored changed nothing, so it cannot be
    // evidence that the file's content is going in circles — the same rule signature 5 already
    // applies, and signature 3 not applying it was a second way to count a non-edit as an edit.
    //
    // Measured on today's stops (2026-08-16): two of six were triggered by a failed call, and in
    // both the shape was the same — the model re-sent an Edit whose `old_string` and `new_string`
    // were identical, the tool answered "No changes to make", and that non-event became the third
    // edit that tripped the threshold. One of the two was the 9B's only `duration` loss, where the
    // implementation on disk was already correct. Signature 1 is the signature for repeated FAILING
    // calls, and it needs three of them in a row; this one is about content churn.
    for (_, input, is_error, _) in &calls {
        if *is_error {
            continue;
        }
        if let Some((file, removed, added)) = edit_target_and_lines(input) {
            *edits_per_file.entry(file.clone()).or_default() += 1;
            let seen = removed_seen.entry(file.clone()).or_default();
            if is_pingpong(&added, seen) {
                // Keep the EVIDENCE, not just the verdict: the first thing said to the model
                // is which lines it put back, and a verdict alone cannot say that.
                let back: Vec<String> = added
                    .iter()
                    .filter(|l| is_substantive(l) && seen.contains(*l))
                    .take(4)
                    .cloned()
                    .collect();
                restored.insert(file.clone(), back);
                pingpong_files.insert(file.clone());
            }
            seen.extend(removed.into_iter().filter(|l| is_substantive(l)));
        }
    }
    let churn = edits_per_file.iter().find(|(f, c)| {
        let n = **c;
        n >= EDIT_CHURN_BACKSTOP || (n >= EDIT_CHURN_MIN && pingpong_files.contains(f.as_str()))
    });
    if let Some((file, n)) = churn {
        // HELP ONCE BEFORE STOPPING.
        //
        // With BUG-054/056/057/058 fixed, 8 of the 9 remaining benchmark failures are this
        // signature, and every one is genuine churn (55-83% of an edit's added lines restoring
        // lines an earlier edit removed). That makes this the single most common thing the gateway
        // ever says to a struggling model — and it was saying the wrong thing. It asserted "the fix
        // has most likely already been applied", which is false on `board` where the file is
        // broken, and it ended the run with nothing the model could act on.
        //
        // First detection: name the lines it put back, and ask for a DIFFERENT change. Second: the
        // hard stop, because a model that churns after being told is the corrupting loop this
        // signature was built for.
        //
        // The escalation reads the transcript instead of holding state — the nudge is IN the
        // conversation, so "have I said this already?" is a question the messages answer. That
        // keeps `detect_stuck_loop` a pure function of its input, which every test here relies on.
        let already_nudged = messages.iter().any(|m| {
            m.content.iter().any(|b| {
                matches!(b, ContentBlock::Text { text } if text.contains(CHURN_NUDGE_MARK))
            })
        });
        if !already_nudged {
            let lines = restored
                .get(file.as_str())
                .map(|ls| ls.iter().map(|l| format!("\n  {l}")).collect::<Vec<_>>().join(""))
                .unwrap_or_default();
            return Some(format!(
                "{CHURN_NUDGE_MARK} Your last edit to `{file}` put back lines that an earlier edit \
                 of yours had removed:{lines}\n\nThat undoes your own work, so the file is now closer \
                 to where it started than to a fix. Do NOT re-apply that edit. Read the file as it \
                 stands now, run the test command to see the CURRENT failure, and make a change you \
                 have not made before. If the file is already correct, say so in one line and stop."
            ));
        }
        return Some(format!(
            "The file `{file}` has been edited {n} times, re-doing and undoing the same change \
             without net progress, and this was pointed out once already. Stopping to avoid \
             corrupting the file in a churn loop; verify the current state and report it in one line."
        ));
    }

    // ── Signature 5: the same edit applied, successfully, three times ──
    // Re-applying an edit payload that ALREADY SUCCEEDED cannot be progress by construction: a
    // `Write` of identical content leaves the file exactly as it was, and an `Edit` cannot honestly
    // succeed twice on one `old_string` because the first application consumed it.
    //
    // This is the true positive signature 3 used to catch by ACCIDENT, through counting a `Read` as
    // an edit — a heuristic that cost 28 false stops before BUG-054 removed it. This one costs
    // none: "byte-identical, and it already worked" has no innocent reading.
    //
    // THREE, not two, and the number is measured rather than chosen. Replayed over all 335 kept
    // transcripts: 8 runs repeat an identical successful edit exactly twice, 2 do it three times,
    // 1 four times. Firing on the second would have added those 8 — and every one of them was
    // already a doomed run (a model inventing a `Cargo.toml` on `greet`, a task that needs no
    // files), so it would have bought wall-clock and nothing else, against the risk this whole bug
    // was about: ending a run that was going to succeed. Three identical successful applications is
    // unambiguous, and stopping there still catches the deepest loops.
    //
    // Errored calls are excluded: retrying a write that FAILED is exactly right, and signature 1
    // already covers a retry that never starts working.
    let mut applied: HashMap<(String, String), usize> = HashMap::new();
    for (_, input, is_error, _) in &calls {
        if *is_error {
            continue;
        }
        if let Some((file, removed, added)) = edit_target_and_lines(input) {
            let key = (file.clone(), format!("{}\u{1}{}", removed.join("\n"), added.join("\n")));
            let n = applied.entry(key).or_insert(0);
            *n += 1;
            if *n >= IDENTICAL_EDIT_THRESHOLD {
                return Some(format!(
                    "The identical edit to `{file}` has now been applied {n} times and succeeded \
                     every time, so it is changing nothing — the file already holds this content. \
                     Stopping to avoid a rewrite loop; verify the current state and report it in \
                     one short line."
                ));
            }
        }
    }

    // ── Signature 4: windowed identical tool-call recurrence ──
    // The no-stop-after-success loop (observed: Qwen3-Coder-30B burning 482 tool calls to
    // timeout on a task it had ALREADY passed): the model re-issues the SAME tool call — re-run
    // the same `cargo run`/test command, re-read the same file, re-write the same content — turn
    // after turn. The repeats aren't strictly consecutive and don't error (sig 1 misses), the
    // prose around them varies each turn (sig 2 misses), and the dominant repeated calls are
    // Bash/Read "verification" calls, not an edit-patch on one file (sig 3, edits-only, misses).
    // Fire when one byte-identical call recurs `TOOL_REPEAT_THRESHOLD` times within the recent
    // window AND ITS RESULT NEVER CHANGED. The threshold is 4 — one higher than the text/
    // structured signatures — so the healthy "build a few times while fixing a compile error,
    // then it passes" rhythm and the existing "3 identical successful calls are not a loop"
    // contract don't trip it.
    //
    // The result half is load-bearing, and matching without it was a measured defect. On the
    // 2026-07-31 matrix (Qwen3.5-4B) this signature cut 11 of nadia's 16 cells and 6 of codex's,
    // and claude's none — because an agent whose prompt tells it to VERIFY re-runs the same
    // `cargo test` on purpose. That is the verify half of fix -> test -> fix, not a spin: the
    // command is identical and the output is not, because the files changed underneath it. Only
    // when the output is identical too has nothing moved.
    const TOOL_WINDOW: usize = 12;
    const TOOL_REPEAT_THRESHOLD: usize = 4;
    {
        let win = &calls[calls.len().saturating_sub(TOOL_WINDOW)..];
        let mut tool_counts: HashMap<(&str, String, &str), usize> = HashMap::new();
        for &(name, input, _, out) in win {
            let c = tool_counts.entry((name, input.to_string(), out)).or_default();
            *c += 1;
            if *c >= TOOL_REPEAT_THRESHOLD {
                return Some(format!(
                    "The `{name}` tool was called {TOOL_REPEAT_THRESHOLD} times with identical \
                     arguments in the last {TOOL_WINDOW} tool calls AND returned the same result \
                     every time — nothing is changing, so repeating it will not help. Stopping to \
                     avoid an infinite loop; report the current result in one short line."
                ));
            }
        }
    }
    // ── Signature 6: read-without-progress ──
    // A weak model (observed: GLM-4-9B on a 1641-line BACKLOG.md) re-reads one target in chunks,
    // narrating "let me continue reading" each turn, and never reaches an edit — sigs 1/4 miss it
    // because the offset/limit differ each call, so no two calls are byte-identical; sig 2 misses
    // it because the assistant text is paraphrased, not repeated verbatim. Only fires when NO
    // mutating call has happened anywhere in the conversation — a model that read a file six times
    // and then edited something else entirely already made progress, and this must not stop it.
    if !calls.iter().any(|(name, _, _, _)| MUTATING_TOOLS.contains(name)) {
        let mut reads_per_target: HashMap<String, usize> = HashMap::new();
        for (name, input, _, _) in &calls {
            if let Some(target) = read_target(name, input) {
                *reads_per_target.entry(target).or_default() += 1;
            }
        }
        if let Some((target, n)) = reads_per_target.iter().find(|&(_, &c)| c >= READ_CHURN_MIN) {
            let already_nudged = messages.iter().any(|m| {
                m.content.iter().any(|b| {
                    matches!(b, ContentBlock::Text { text } if text.contains(READ_NUDGE_MARK))
                })
            });
            if !already_nudged {
                return Some(format!(
                    "{READ_NUDGE_MARK} You've read `{target}` {n} times and haven't made a single \
                     edit or run a command yet. Stop reading it from the start each time — grep for \
                     the section you need, or append/edit directly using what you already saw. If \
                     you don't yet know what to add, say so in one line and stop."
                ));
            }
            return Some(format!(
                "`{target}` has been read {n} times with no edit or command in between, and this \
                 was pointed out once already. Stopping to avoid an infinite read loop; report what \
                 you know in one short line."
            ));
        }
    }

    None
}

/// A one-shot `ChatStream` that emits `text` then `Done{EndTurn}` without touching the
/// model. Used by the loop-breaker: it slots in where `backend.chat` would, so every
/// existing per-protocol serializer (OpenAI / Responses / Anthropic, streaming or not)
/// renders it as an ordinary final assistant turn with `finish_reason: stop`.
pub(crate) fn synthetic_stop_stream(text: String) -> ChatStream {
    Box::pin(async_stream::stream! {
        yield Ok(ChatEvent::TextDelta { text });
        yield Ok(ChatEvent::Done { input_tokens: 0, output_tokens: 0, stop_reason: StopReason::EndTurn });
    })
}
