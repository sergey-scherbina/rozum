//! Codex `apply_patch` / tool-argument rewriting.
//!
//! Extracted verbatim from `gateway.rs` (monolith-decompose, gw-monolith-decompose): the codex
//! delivery-normalization cluster — unified-diff→V4A translation, the JSON-wrapped / exec-array /
//! function-call `apply_patch` forms, file-write synthesis, unicode-escape + html-entity decode, and
//! `normalize_codex_tool_args`. Pure string/`serde_json::Value` transforms with no gateway-internal
//! deps (verified: zero `crate::`/`super::` references), so the move is behaviour-preserving. Their
//! regression corpus stays in `gateway.rs`'s test module (accesses them via `use crate::codex_patch::*`).
use serde_json::{json, Value};

/// Codex's `apply_patch` requires its bespoke envelope (`*** Update File: <path>` + bare `@@`
/// hunk markers). Local models routinely emit a **standard unified diff** inside the
/// `*** Begin Patch` wrapper (`--- /+++ /@@ -a,b +c,d @@`), which codex rejects with
/// "Invalid patch hunk". The change lines (` `/`-`/`+`) are identical in both dialects, so we
/// translate just the headers and the (already-correct) edit lands. See
/// `docs/matrix-failure-analysis.md` Finding 4. Returns the input unchanged unless it is exactly
/// this malformed hybrid (codex envelope + unified-diff headers).
pub(crate) fn rewrite_unified_diff_to_apply_patch(patch: &str) -> String {
    // Fire on EITHER unified malformation: a `--- ` file header, or a `@@ -a,b +c,d @@` hunk header
    // (the model sometimes emits the codex `*** Update File:` header itself but keeps unified `@@`).
    let has_unified = patch.starts_with("--- ") || patch.contains("\n--- ") || patch.contains("@@ -");
    if !patch.contains("*** Begin Patch") || !has_unified {
        return patch.to_string();
    }
    let strip = |p: &str| -> String {
        let p = p.trim();
        p.strip_prefix("a/")
            .or_else(|| p.strip_prefix("b/"))
            .unwrap_or(p)
            .to_string()
    };
    let mut out = String::with_capacity(patch.len() + 16);
    let mut lines = patch.lines().peekable();
    while let Some(line) = lines.next() {
        if let Some(path) = line.strip_prefix("--- ") {
            // `--- a/x` [`+++ b/x`] → `*** Update File: x` (prefer the +++ path; both name the file)
            let mut file = strip(path);
            if let Some(next) = lines.peek() {
                if let Some(p2) = next.strip_prefix("+++ ") {
                    if p2.trim() != "/dev/null" {
                        file = strip(p2);
                    }
                    lines.next();
                }
            }
            out.push_str("*** Update File: ");
            out.push_str(&file);
            out.push('\n');
        } else if line.starts_with("@@ -") || line.starts_with("@@-") {
            // unified hunk header (`@@ -a,b +c,d @@`) — DROP it. codex's V4A apply_patch locates the
            // change via the surrounding context lines; a literal `@@ -a,b...` is read as a context
            // string to find ("Failed to find context '-a,b...'") which never matches the file.
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Method B (the robust fix). codex's `apply_patch` uses a finicky proprietary V4A format that a
/// local model can't reliably hit (header dialect + strict context-matching — see
/// `docs/matrix-failure-analysis.md` Finding 4 and the mock-codex probe). But the model DOES emit a
/// correct unified diff (its `-` lines match the file verbatim). So instead of translating to V4A,
/// reconstruct a MINIMAL unified diff from the model's patch and rewrite the whole
/// `apply_patch "<patch>"` shell command into `patch --fuzz` of it — standard tooling codex runs
/// verbatim (codex only intercepts `apply_patch`, not `patch`). `patch --fuzz` locates the change by
/// context, tolerant of the line-number/whitespace drift that breaks V4A. Returns the new shell
/// command, or None when it isn't a reconstructable apply_patch (→ caller falls back to the V4A bridge).
pub(crate) fn rewrite_apply_patch_command(cmd: &str) -> Option<String> {
    if !cmd.contains("apply_patch") {
        return None;
    }
    // gpt-oss (OpenAI tool surface) wraps the patch in a JSON payload under a flag, e.g.
    //   apply_patch -patches '[{"content":"*** Begin Patch\n*** Add File: …*** End Patch"}]'
    // The V4A body is then a JSON-escaped STRING (`\n`, `\"`), so the raw shell-unescape below leaves
    // literal `\n` and `apply_patch_block_to_fuzz` can't see the `*** Add File:` line structure → the
    // rewrite fails, the ORIGINAL `apply_patch -patches '[…]'` runs against the real shim →
    // `apply_patch accepts exactly one argument` → nothing written (matrix rc11, the biggest
    // codex/opencode create-from-scratch failure bucket). JSON-decode each carried patch FIRST (→ real
    // newlines) and run each through the shared block parser.
    if let Some(out) = rewrite_json_wrapped_apply_patch(cmd) {
        return Some(out);
    }
    let begin = cmd.find("*** Begin Patch")?;
    let end_rel = cmd[begin..].find("*** End Patch")?;
    let end = begin + end_rel + "*** End Patch".len();
    // The patch lives inside a shell double-quoted string — undo the shell escaping.
    let block = cmd[begin..end]
        .replace("\\\"", "\"")
        .replace("\\$", "$")
        .replace("\\`", "`")
        .replace("\\\\", "\\");
    apply_patch_block_to_fuzz(&block)
}

/// gpt-oss/OpenAI form: the `apply_patch` argument is JSON that carries the V4A patch as a string —
/// `-patches '[{"content":"*** Begin Patch…"}]'`, a single `{"patch":"…"}`, or a bare JSON string.
/// Parse the FIRST self-delimiting JSON value embedded in the command (ignoring trailing shell text
/// like the closing quote), pull every string field that holds a patch, and turn each into shell
/// writes via the shared block parser. `None` when the command has no JSON-wrapped patch — a raw
/// `*** Begin Patch` shell string (whose body may itself contain a `{`) fails the JSON parse here and
/// the caller falls back to the shell-string path.
pub(crate) fn rewrite_json_wrapped_apply_patch(cmd: &str) -> Option<String> {
    let after = &cmd[cmd.find("apply_patch")?..];
    let jstart = after.find(['[', '{'])?;
    let val = serde_json::Deserializer::from_str(&after[jstart..])
        .into_iter::<serde_json::Value>()
        .next()?
        .ok()?;
    let mut patches = Vec::new();
    collect_patch_strings(&val, &mut patches);
    if patches.is_empty() {
        return None;
    }
    let out: String = patches.iter().filter_map(|p| apply_patch_block_to_fuzz(p)).collect();
    (!out.is_empty()).then_some(out)
}

/// Collect every string inside a JSON value that looks like a V4A patch (carries `*** Begin Patch`
/// or an `*** Add/Create/Update File:` directive), recursing through arrays/objects. Shape-agnostic:
/// models wrap the patch under different keys (`content`, `patch`, `input`), sometimes in an array.
pub(crate) fn collect_patch_strings(v: &serde_json::Value, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            if s.contains("*** Begin Patch")
                || s.contains("*** Add File:")
                || s.contains("*** Create File:")
                || s.contains("*** Update File:")
            {
                out.push(s.clone());
            }
        }
        serde_json::Value::Array(a) => a.iter().for_each(|e| collect_patch_strings(e, out)),
        serde_json::Value::Object(o) => o.values().for_each(|e| collect_patch_strings(e, out)),
        _ => {}
    }
}

/// Render a verbatim file-create as a shell command: write `content` to `path` ONLY if the path is
/// still absent (so a re-sent create is an idempotent no-op and never clobbers a real edit), with
/// `mkdir -p` of the parent for nested targets. Single-quoted heredoc → the body lands byte-for-byte
/// (no `$`/backtick/`\` expansion). Shared by the explicit `*** Add/Create File:` path and the
/// `*** Update File:`-against-an-absent-file fallback.
pub(crate) fn synth_create_command(path: &str, content: &str) -> String {
    format!(
        "[ -e '{path}' ] || {{ mkdir -p \"$(dirname '{path}')\" 2>/dev/null; \
         cat > '{path}' <<'ROZUM_CREATE_EOF'\n{content}\nROZUM_CREATE_EOF\n}}\n"
    )
}

/// Extract explicit file-creations from a V4A patch block: each `*** Add File: <path>` /
/// `*** Create File: <path>` directive plus the lines that follow it (the new file's content, bare
/// or `+`-prefixed) up to the next `*** ` directive. This is the canonical — and, for gpt-oss, the
/// dominant — create-from-scratch shape (`*** Create File:` is gpt-oss's variant of the standard
/// `*** Add File:`). codex serves `apply_patch` only as a shell command for local models, so these
/// reach the bare `apply_patch` (absent in the jail) and the file never lands; we turn each into a
/// real write instead. Returns (path, content) pairs; empty when the block has no create directive.
pub(crate) fn parse_create_directives(block: &str) -> Vec<(String, String)> {
    let mut files: Vec<(String, Vec<String>)> = Vec::new();
    let mut active = false;
    for ln in block.lines() {
        if let Some(p) = ln
            .strip_prefix("*** Add File:")
            .or_else(|| ln.strip_prefix("*** Create File:"))
        {
            files.push((p.trim().to_string(), Vec::new()));
            active = true;
        } else if ln.starts_with("*** ") {
            active = false; // Begin/End Patch or an Update File hunk ends the create body
        } else if active {
            if let Some((_, body)) = files.last_mut() {
                body.push(ln.strip_prefix('+').unwrap_or(ln).to_string());
            }
        }
    }
    files
        .into_iter()
        .filter(|(p, b)| !p.is_empty() && !b.is_empty())
        .map(|(p, b)| (p, b.join("\n")))
        .collect()
}

/// Detect a "whole file dumped as a fake patch": `*** Update File: <path>` whose body (after the
/// `@@`) is the file's RAW content with NO diff markers at all — gpt-oss does this for a brand-new
/// file (esp. a nested `src/main.rs`), often inside a broken `apply_patch <<'…'` heredoc that runs
/// bare and lands nothing. There is no diff to apply; the body verbatim IS the intended file, so we
/// create it (when absent). Returns None the moment a real `+`/`-` marker appears — a genuine diff
/// belongs to the patch path, untouched. Structural lines (`@@`, `+++ `, `--- `) are skipped.
pub(crate) fn parse_bare_file_block(block: &str) -> Option<(String, String)> {
    let mut path: Option<String> = None;
    let mut content: Vec<&str> = Vec::new();
    let mut started = false;
    for ln in block.lines() {
        if let Some(p) = ln.strip_prefix("*** Update File:") {
            path = Some(p.trim().to_string());
            started = true;
            content.clear();
        } else if ln.starts_with("*** ") {
            if started && !content.is_empty() {
                break; // End Patch / next directive closes this file's content
            }
            started = false;
        } else if started {
            if ln.starts_with("@@") || ln.starts_with("+++ ") || ln.starts_with("--- ") {
                continue; // structural, skip
            }
            if ln.starts_with('+') || ln.starts_with('-') {
                return None; // real diff markers → not bare content; leave it to the patch path
            }
            content.push(ln);
        }
    }
    let path = path?;
    if content.is_empty() {
        return None;
    }
    Some((path, content.join("\n")))
}

/// Convert an unescaped V4A patch block (`*** Begin Patch` … `*** End Patch`, or a bare
/// `*** Update File:` + hunk) into a `patch -p0 --fuzz=3 -N --forward` heredoc — a small
/// ±3-context match surface that standard `patch` applies reliably and *idempotently* (`-N`:
/// a re-submitted, already-applied patch is ignored, never reversed — see the note at the
/// `format!` below). Shared by the apply_patch *shell-command* bridge (Method B) and the
/// apply_patch-*function* re-route (gpt-oss). None when there are no change lines to anchor on.
pub(crate) fn apply_patch_block_to_fuzz(block: &str) -> Option<String> {
    // gpt-oss over-escapes patch text — it emits `>` as `>` (and `&`/`<` likewise) inside the
    // patch body, so a context line like `pub fn add(a,b) -> i32 {` no longer matches the file's
    // `-> i32 {` → `patch` fails or fuzz-corrupts (observed: debug edits silently not landing). The
    // function-call path already decodes; the `{patch}` / `apply_patch <<EOF` shell-heredoc forms
    // reach here undecoded, so decode here too (idempotent — a no-op once there is no `\u`).
    let decoded = decode_unicode_escapes(block);
    let block = decoded.as_str();
    // Explicit `*** Add File:` / `*** Create File:` directives → real file writes (the dominant
    // gpt-oss create-from-scratch shape). One directive can carry several files; write each.
    let creates = parse_create_directives(block);
    if !creates.is_empty() {
        return Some(creates.iter().map(|(p, c)| synth_create_command(p, c)).collect());
    }
    // A whole new file dumped as a fake `*** Update File:` patch (bare body, no diff markers) →
    // create it from the verbatim body. (A real diff bails out of parse_bare_file_block to None.)
    if let Some((p, content)) = parse_bare_file_block(block) {
        return Some(synth_create_command(&p, &content));
    }
    let mut path: Option<String> = None;
    let mut body: Vec<String> = Vec::new();
    for ln in block.lines() {
        if let Some(p) = ln.strip_prefix("*** Update File:") {
            path = Some(p.trim().to_string());
        } else if let Some(p) = ln.strip_prefix("--- ") {
            let p = p.trim();
            let p = p.strip_prefix("a/").or_else(|| p.strip_prefix("b/")).unwrap_or(p);
            path.get_or_insert_with(|| p.to_string());
        } else if ln.starts_with("+++ ") || ln.starts_with("@@") || ln.starts_with("*** ") {
            continue;
        } else if ln.is_empty() {
            body.push(" ".to_string()); // a blank context line in the diff
        } else if matches!(ln.as_bytes()[0], b' ' | b'+' | b'-') {
            body.push(ln.to_string());
        }
        // anything else is stray prose — skip it
    }
    let path = path?;
    let chg: Vec<usize> = body
        .iter()
        .enumerate()
        .filter(|(_, l)| l.starts_with('+') || l.starts_with('-'))
        .map(|(i, _)| i)
        .collect();
    let (&first, &last) = (chg.first()?, chg.last()?);
    // Trim to ±3 lines of context around the change → small, reliable match surface.
    let lo = first.saturating_sub(3);
    let hi = (last + 1 + 3).min(body.len());
    let hunk = &body[lo..hi];
    let old = hunk.iter().filter(|l| l.starts_with(' ') || l.starts_with('-')).count();
    let new = hunk.iter().filter(|l| l.starts_with(' ') || l.starts_with('+')).count();
    let mut diff = format!("--- {path}\n+++ {path}\n@@ -1,{old} +1,{new} @@\n");
    for l in hunk {
        diff.push_str(l);
        diff.push('\n');
    }
    // `-N --forward`: make re-application idempotent. A weak model (gpt-oss) flails — it
    // re-submits the SAME patch after it has already landed. Without `-N`, GNU/BSD `patch`
    // hits "Reversed (or previously applied) patch detected!  Assume -R? [y]" and, with no
    // tty, assumes yes → it REVERSES the already-applied fix, putting the bug back. The file
    // then oscillates fixed↔buggy across the model's retries and whichever state the timeout
    // freezes decides pass/fail (observed coin-flip pass=0/1). `-N` turns a redundant patch
    // into a no-op ("Ignoring previously applied patch") instead of a revert, so the fix is
    // sticky and the outcome is deterministic. A genuinely new patch still applies normally.
    // Whitespace-tolerant FALLBACK: gpt-oss often drops the leading indentation on changed lines
    // (`-s.to_string()` instead of `-    s.to_string()`), and BSD `patch` — even `--ignore-whitespace`
    // — refuses to match it, so the hunk fails to a `.rej` and the fix never lands (looks like the
    // model "reverting" itself; it never applied). When `patch` leaves a `.rej`, a tiny static python
    // helper reads that `.rej`, matches the removed block against the file by *trimmed* content
    // (ignoring leading whitespace and the line number), and re-applies it preserving the file's own
    // indentation. Fires only after `patch` already failed → zero effect on patches that apply.
    let fuzz = patch_fuzz();
    // gpt-oss creating a file FROM SCRATCH emits the new content as an `*** Update File:` /
    // unified-diff hunk whose "old" side is bogus (a lone `---`, empty context) because the target
    // does not exist yet. `patch` then can't update the absent file → `.rej`, nothing lands (the
    // codex×gpt-oss `build`/`test` create reds, matrix Finding 5). Detect it — additions present
    // but the removed/context side carries no real content — and CREATE the file from the `+` lines
    // instead, only if it's still absent (so a re-sent create is an idempotent no-op and never
    // clobbers a real edit). A genuine edit (real removed/context lines) is byte-identical to
    // before: it falls through to the patch path unchanged, so the `fix` task is unaffected.
    let has_real_old = body
        .iter()
        .filter(|l| l.starts_with(' ') || l.starts_with('-'))
        .any(|l| l[1..].trim().chars().any(|c| c.is_alphanumeric()));
    let added: Vec<&str> = body.iter().filter(|l| l.starts_with('+')).map(|l| &l[1..]).collect();
    if !added.is_empty() && !has_real_old {
        return Some(synth_create_command(&path, &added.join("\n")));
    }
    Some(format!(
        "patch -p0 --fuzz={fuzz} -N --forward <<'ROZUM_PATCH_EOF'\n{diff}ROZUM_PATCH_EOF\n\
         f={path}; if [ -f \"$f.rej\" ]; then python3 - \"$f\" <<'ROZUM_PY_EOF'\n{py}\nROZUM_PY_EOF\n\
         rm -f \"$f.rej\" \"$f.orig\"; fi\n",
        py = PATCH_WS_FALLBACK_PY,
    ))
}

/// Static python helper for the whitespace-tolerant apply fallback (see `apply_patch_block_to_fuzz`).
/// Reads `<file>.rej`, extracts the removed (`-`) and added (`+`) lines, finds the removed block in
/// the file by trimmed comparison, and replaces it with the added lines re-indented to the file's
/// own leading whitespace. Best-effort + single-block: only the file path is dynamic (argv), so the
/// script needs no escaping when embedded in the command heredoc.
pub(crate) const PATCH_WS_FALLBACK_PY: &str = r#"import sys
f=sys.argv[1]
old=[]; new=[]
for ln in open(f+".rej").read().split("\n"):
    if ln.startswith("---") or ln.startswith("+++"): continue
    if ln[:1]=="-": old.append(ln[1:])
    elif ln[:1]=="+": new.append(ln[1:])
no=[s.strip() for s in old]
if no:
    L=open(f).read().split("\n")
    h=next((i for i in range(len(L)-len(no)+1) if [L[i+j].strip() for j in range(len(no))]==no), -1)
    if h>=0:
        ind=L[h][:len(L[h])-len(L[h].lstrip())]
        L[h:h+len(no)]=[ind+n.strip() for n in new]
        open(f,"w").write("\n".join(L))
        sys.stderr.write("[rozum-apply] whitespace-tolerant fallback applied\n")"#;

/// The `--fuzz` context-slack `patch` is allowed when matching a hunk. Higher = more lenient
/// (lands a model's slightly-off-context patch, but can mis-apply a stale-anchored churn patch
/// at the wrong line and corrupt the file); lower = stricter (a misanchored patch fails to a
/// `.rej`, leaving the file intact, but the model's first imperfect patch may not land).
/// `ROZUM_PATCH_FUZZ` overrides the default (3); clamped to GNU/BSD patch's 0..=3.
pub(crate) fn patch_fuzz() -> u8 {
    std::env::var("ROZUM_PATCH_FUZZ")
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map(|n| n.min(3))
        .unwrap_or(3)
}

/// gpt-oss (trained on the OpenAI/codex tool surface) emits a native `apply_patch` *function*
/// call, but codex serves apply_patch only as a shell command for the rozum-backed local-model
/// config — so the function call is rejected (`unsupported call: apply_patch`) and the edit is
/// silently lost. Re-route it: convert the function args into an `exec_command` payload that
/// applies the patch with standard tooling (Method B `patch --fuzz`; failing that, a quote-safe
/// `apply_patch` heredoc so codex's own V4A applier still gets a shot). Returns the exec_command
/// args JSON, or None when there is no reconstructable patch (caller keeps the original args).
pub(crate) fn rewrite_apply_patch_function_args(args: &str) -> Option<String> {
    let v: Value = serde_json::from_str(args).ok()?;
    // B1 (universal seam): a multi-file CREATE can also arrive as the apply_patch FUNCTION call with a
    // whole-file array and NO patch string — Devstral emits `{"patches":[{"op":"Add","path":…,"content":…}]}`
    // (also `file_changes`/`files`, path key `path`/`file`/`filename`). The patch-string extraction below
    // then returns None and the create is lost. Reuse the same synthesizer the exec_command path uses so
    // every {path,content} entry lands, regardless of which path (exec vs function) or key the model chose.
    if let Some(o) = v.as_object() {
        if let Some(writes) = synthesize_writes_from_patches(o) {
            eprintln!("[apply_patch-fn] synthesized file writes from function-call array {{…:[{{path,content}}]}}");
            return Some(json!({ "cmd": writes, "login": true }).to_string());
        }
    }
    // The model passes the patch text in one of a few shapes:
    //   {"command":["apply_patch","<patch>"]}  (gpt-oss, observed) — the last array string is it
    //   {"input":"<patch>"} / {"patch":"<patch>"} / a bare string
    let patch = v
        .get("command")
        .and_then(|c| c.as_array())
        .and_then(|a| a.iter().rev().find_map(|x| x.as_str()))
        .or_else(|| v.get("input").and_then(|x| x.as_str()))
        .or_else(|| v.get("patch").and_then(|x| x.as_str()))
        .or_else(|| v.as_str())?;
    if !patch.contains("*** Begin Patch") && !patch.contains("*** Update File") {
        return None;
    }
    // Decode the `\uXXXX` escapes gpt-oss double-escapes into the body (`&`→&, `<`/`>`→
    // </>). A Rust fix is full of these (`&str`, `&arg`, `collect::<String>()`, `->`);
    // left literal they land verbatim and break compilation. The shell-command path
    // (normalize_codex_tool_args) already decodes — this FUNCTION-call path (the dominant gpt-oss
    // edit shape) did not, which is a major source of the codex×gpt-oss corruption.
    let patch = decode_unicode_escapes(patch);
    // Prefer Method B: codex runs `patch --fuzz` verbatim (it only intercepts `apply_patch`).
    let cmd = apply_patch_block_to_fuzz(&patch).unwrap_or_else(|| {
        // Fallback: hand codex the raw apply_patch via a quote-safe heredoc (its V4A applier).
        format!("apply_patch <<'ROZUM_AP_EOF'\n{patch}\nROZUM_AP_EOF\n")
    });
    eprintln!("[apply_patch-fn] re-routed apply_patch function call → exec_command (gpt-oss)");
    Some(json!({ "cmd": cmd, "login": true }).to_string())
}

/// gpt-oss, asked to CREATE a file from scratch, routes a write-INTENT through the codex shell
/// tool: `{cmd:"apply_patch", path:"Cargo.toml", content:"<whole file body>"}`. `content` is a full
/// file, NOT a patch (no `*** Begin Patch`), so the apply_patch fold finds nothing and codex runs
/// bare `apply_patch` → "Usage: apply_patch 'PATCH'" → the file never lands (build/test create-from-
/// scratch tasks time out, matrix Finding 5). The intent is unambiguous (a path + its full content),
/// so synthesize the real write codex can't perform from the malformed call. None unless there is a
/// non-empty `path` plus a `content` string that is NOT a patch (patches go through the fold above).
pub(crate) fn synthesize_write_from_obj(o: &serde_json::Map<String, Value>) -> Option<String> {
    let path = o
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|p| !p.is_empty())?;
    let content = o.get("content").and_then(Value::as_str)?;
    let content = decode_unicode_escapes(content);
    if content.contains("*** Begin Patch") || content.contains("*** Update File") {
        return None; // a patch body, not a file — leave it to the apply_patch path
    }
    Some(synthesize_file_write(path, &content))
}

/// codex's STRUCTURED multi-file apply_patch: `{"cmd":"apply_patch","patches":[{"path":…,"content":…}, …]}`
/// — each entry is a WHOLE file (raw content, no V4A `*** Add File:` markers). Neither the V4A fold nor
/// the single-file `synthesize_write_from_obj` handles this shape, so the bare shim gets JSON it can't
/// parse and nothing lands. Return one shell command that writes each well-formed `{path, content}` entry
/// via the shared heredoc (skipping malformed entries); None if there's no usable array.
/// Both the ARRAY key and the per-entry PATH key vary wildly across models (r3-cumulative capture,
/// 2026-07-05): the array is `patches` / `file_changes` / `files` / `changes`, and each entry's path is
/// `path` / `file` / `filename` (Devstral's dominant form is `patches:[{file,content}]`). Accept any.
pub(crate) fn synthesize_writes_from_patches(o: &serde_json::Map<String, Value>) -> Option<String> {
    let arr = ["patches", "file_changes", "files", "changes"]
        .iter()
        .find_map(|k| o.get(*k).and_then(Value::as_array))?;
    let mut cmd = String::new();
    for e in arr {
        let Some(eo) = e.as_object() else { continue };
        // The per-entry path key also varies by model: Devstral emits `file`, others `path`/`filename`.
        let Some(path) = ["path", "file", "filename"]
            .iter()
            .find_map(|k| eo.get(*k).and_then(Value::as_str))
            .map(str::trim)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        let Some(content) = eo.get("content").and_then(Value::as_str) else {
            continue;
        };
        cmd.push_str(&synthesize_file_write(path, &decode_unicode_escapes(content)));
    }
    (!cmd.is_empty()).then_some(cmd)
}

/// Render a verbatim file write as one shell command: `mkdir -p <dir>` (so a nested target like
/// `src/main.rs` into a fresh dir doesn't fail on a missing directory) then a *single-quoted* heredoc
/// `cat > <path>` so the body lands byte-for-byte — no `$`/backtick/`\` expansion. The path is
/// single-quoted to tolerate spaces; a literal `'` in a path is pathological and not handled.
pub(crate) fn synthesize_file_write(path: &str, content: &str) -> String {
    format!(
        "mkdir -p \"$(dirname '{path}')\" 2>/dev/null; cat > '{path}' <<'ROZUM_WRITE_EOF'\n\
         {content}\n\
         ROZUM_WRITE_EOF\n"
    )
}

/// Decode literal `\uXXXX` (4-hex) escapes that gpt-oss sometimes double-escapes *into* patch
/// content (`&` for `&`, `>` for `>`) — the literal 6-char sequence survives in the
/// string, so the patch's context/`-` lines no longer match the file and the apply fails. Only the
/// bare 4-hex form is touched (Rust's own escape is `\u{..}` with braces, so source code is safe).
pub(crate) fn decode_unicode_escapes(s: &str) -> String {
    if !s.contains("\\u") {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\'
            && i + 5 < chars.len()
            && chars[i + 1] == 'u'
            && chars[i + 2] != '{'
        {
            let hex: String = chars[i + 2..i + 6].iter().collect();
            if let Some(c) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
                out.push(c);
                i += 6;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Read-repair (translate a malformed `sed/head/tail` file-read → `cat <file>`) is ON by default.
/// Reading the file is the decisive success factor: a weak model (gpt-oss) that emits a broken read
/// (`sed -n "src/main.rs"` with no line range) never sees the code and so never fixes it — it retries
/// the same broken read and gives up. The repair is conservative and only fires on a *genuinely
/// broken* read (a `sed` whose script slot holds the filename or a range with no print command);
/// well-formed ranged reads and `head`/`tail` are left intact (see `repair_broken_read`).
/// `ROZUM_CODEX_READ_REPAIR=0` turns it off.
pub(crate) fn read_repair_enabled() -> bool {
    std::env::var("ROZUM_CODEX_READ_REPAIR")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Exec-arg unicode decode (gptoss-exec-decode-loopbreak (a)) is ON by default. gpt-oss sometimes emits
/// shell metacharacters JSON-double-escaped — `cat > file` instead of `cat > file` — so the
/// redirect (`>` = `>`, `<` = `<`, `|` = `|`, `&` = `&`) lands as a literal token and
/// the command silently does the wrong thing (`cat > file` becomes a no-op read → the file never lands).
/// Decoding the `\uXXXX` in the exec command restores the operator. Conservative: `decode_unicode_escapes`
/// only rewrites valid 4-hex `\uXXXX` and is a no-op otherwise. `ROZUM_CODEX_EXEC_DECODE=0` turns it off.
pub(crate) fn exec_decode_enabled() -> bool {
    std::env::var("ROZUM_CODEX_EXEC_DECODE")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// A token that looks like a source-file path the model wants to view (has a slash, or a known
/// code/text extension). Used to recognize a file-read intent in a malformed command.
pub(crate) fn is_source_path(w: &str) -> bool {
    (w.contains('/') && w.contains('.'))
        || w.rsplit('.').next().is_some_and(|e| {
            matches!(
                e,
                "rs" | "py" | "js" | "ts" | "go" | "toml" | "txt" | "md" | "json" | "c" | "cpp"
                    | "h" | "java" | "rb" | "yaml" | "yml" | "sh" | "lock"
            )
        })
}

/// Translate a *genuinely broken* file-READ command into a plain `cat <file>`. gpt-oss emits broken
/// `sed` reads (filename in the script slot, a range with no `p` command, scrambled args) that exit
/// non-zero, so it never sees the file. The intent — view a source file — is unambiguous from the
/// path, and reading is non-destructive. Only fires when the read would actually fail, so it is safe
/// to default ON: a WELL-FORMED ranged read (`sed -n '1,200p' f`) and any `head`/`tail` (which work
/// with a file) are left intact — never collapsed to a full `cat`. Edits (`s/…/`, `-i`) and
/// redirects (`>`) are left alone. None when it isn't a recognizable broken read.
pub(crate) fn repair_broken_read(cmd: &str) -> Option<String> {
    let t = cmd.trim();
    let tool = t.split_whitespace().next()?;
    if !matches!(tool, "sed" | "head" | "tail") {
        return None;
    }
    if t.contains("s/") || t.contains(" -i") || t.contains('>') {
        return None; // an intentional edit / transform / redirect — not a read
    }
    // Unquoted positional (non-flag) args.
    let args: Vec<String> = t
        .split_whitespace()
        .skip(1)
        .filter(|w| !w.starts_with('-'))
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"').to_string())
        .collect();
    let path = args.iter().find(|w| is_source_path(w))?.clone();
    // `head`/`tail` with a file are valid reads — leave them (respect a deliberate partial read).
    if tool != "sed" {
        return None;
    }
    // A well-formed `sed -n` read has a print-script arg (`1,200p`, `5p`, `$p`). If one is present
    // the read works as written → don't touch it. Broken only when the script slot holds the file
    // or a range with no print command.
    let has_print_script = args.iter().any(|a| {
        a != &path && a.ends_with('p') && a.chars().all(|c| c.is_ascii_digit() || ",$p".contains(c))
    });
    if has_print_script {
        return None;
    }
    Some(format!("cat {path}"))
}

/// Walk a tool-call `arguments` JSON string and rewrite any embedded malformed codex `apply_patch`
/// (Finding 4). The patch text is nested inside the shell tool's command (and JSON-escaped), so we
/// parse, recurse over every string value, and re-serialize — keeping escaping correct. A no-op for
/// non-codex agents (only the Responses path calls this) and for well-formed / non-patch args.
pub(crate) fn heredoc_redirect_enabled() -> bool {
    std::env::var("ROZUM_HEREDOC_REDIRECT_FIX")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// gpt-oss frequently emits `cat <path> <<'EOF' … EOF` *without* the `>` redirect when it means to
/// WRITE the file. Without `>`, `cat` takes `<path>` as a positional arg and **ignores stdin** (the
/// heredoc) → the file is merely read (or errors if absent) and the intended write is **silently
/// lost**. Live autopsy (codex×gpt-oss build, run OzUnnR): the model's correct *final* `main.rs`
/// (`input.chars().rev().collect()`) was sent this way → it never landed, the earlier broken version
/// stayed on disk → `cargo run` printed nothing → build red. `cat <path> <<DELIM` is **never** a
/// meaningful command (cat discards the heredoc when given a file arg), so the write-intent is
/// unambiguous and the repair is safe: insert the missing `>`. Heredoc-aware (tracks the delimiter so
/// body lines that happen to start with `cat …` are never rewritten); only the opener line is fixed.
/// Leaves well-formed `cat > x <<EOF`, plain reads (`cat x`), and stdout heredocs (`cat <<EOF`) alone.
pub(crate) fn repair_heredoc_write(cmd: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let mut changed = false;
    let mut in_heredoc: Option<String> = None;
    for line in cmd.lines() {
        if let Some(delim) = &in_heredoc {
            let done = line.trim() == delim.as_str();
            out.push(line.to_string());
            if done {
                in_heredoc = None;
            }
            continue;
        }
        if let Some(p) = line.find("<<") {
            // delimiter word after `<<` (strip an optional surrounding quote)
            let after = line[p + 2..].trim_start();
            let raw = after.trim_start_matches(['\'', '"']);
            let delim: String = raw
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let head = &line[..p];
            let t = head.trim_start();
            // botched `cat <path> <<` write: starts with `cat `, a real path arg, no `>` redirect
            if t.starts_with("cat ") && !head.contains('>') {
                let cat_at = head.find("cat ").unwrap();
                let path = head[cat_at + 4..].trim();
                if !path.is_empty() && !path.starts_with('-') && !path.starts_with('<') {
                    let ins = cat_at + 4;
                    out.push(format!("{}> {}", &line[..ins], &line[ins..]));
                    changed = true;
                    if !delim.is_empty() {
                        in_heredoc = Some(delim);
                    }
                    continue;
                }
            }
            if !delim.is_empty() {
                in_heredoc = Some(delim);
            }
        }
        out.push(line.to_string());
    }
    if changed {
        Some(out.join("\n"))
    } else {
        None
    }
}

pub(crate) fn normalize_codex_tool_args(args: &str) -> String {
    // gptoss-exec-decode-loopbreak (b): empty args cause "expected value at line 1 col 1" in codex's
    // tool router → codex retries → runaway loop. Return a no-op echo so codex can continue.
    if args.trim().is_empty() {
        eprintln!("[exec-decode] empty exec args from model → substituting no-op echo");
        return r#"{"cmd":"echo '[gateway: model emitted empty exec args]'"}"#.to_string();
    }
    let mut v: Value = match serde_json::from_str(args) {
        Ok(v) => v,
        Err(_) => return args.to_string(),
    };
    // gpt-oss often emits the patch in a field SIBLING to a bare `apply_patch` command
    // ({"cmd":"apply_patch","patch":"*** Begin Patch …"}); these keys carry it.
    const PATCH_KEYS: &[&str] = &["patch", "input", "stdin", "patch_text", "content", "text"];
    fn walk(v: &mut Value) {
        match v {
            Value::String(s) => {
                if s.contains("*** Begin Patch") {
                    // Decode any literal \uXXXX the model double-escaped into the patch body.
                    let s2 = decode_unicode_escapes(s);
                    if let Some(rw) = rewrite_apply_patch_command(&s2) {
                        eprintln!("[apply_patch-bridge] rewrote apply_patch → patch --fuzz (Method B)");
                        *s = rw;
                    } else {
                        let fixed = rewrite_unified_diff_to_apply_patch(&s2);
                        if fixed != *s {
                            eprintln!("[apply_patch-bridge] rewrote unified-diff headers → codex V4A (fallback)");
                            *s = fixed;
                        }
                    }
                }
            }
            Value::Array(a) => a.iter_mut().for_each(walk),
            Value::Object(o) => {
                // gptoss-exec-decode-loopbreak (a): decode `\uXXXX` in the exec command FIRST, so a
                // JSON-double-escaped shell operator (`cat > f` → `cat > f`) redirects instead of
                // landing as a literal token (a silent no-op read). Covers both the `cmd` string and the
                // `command` argv-array shapes. `decode_unicode_escapes` is a no-op without a valid `\uXXXX`.
                if exec_decode_enabled() {
                    if let Some(Value::String(c)) = o.get_mut("cmd") {
                        if c.contains("\\u") {
                            let d = decode_unicode_escapes(c);
                            if d != *c {
                                eprintln!("[exec-decode] decoded \\uXXXX in exec cmd → shell operators restored");
                                *c = d;
                            }
                        }
                    }
                    if let Some(Value::Array(a)) = o.get_mut("command") {
                        for el in a.iter_mut() {
                            if let Value::String(c) = el {
                                if c.contains("\\u") {
                                    *c = decode_unicode_escapes(c);
                                }
                            }
                        }
                    }
                }
                // The dominant gpt-oss edit-delivery shape: a bare `apply_patch` command with the
                // patch stranded in a sibling field. codex runs bare `apply_patch` (ignoring the
                // sibling) → "Usage: apply_patch 'PATCH'" and the edit is lost. Fold the sibling
                // patch INTO the command (Method B `patch --fuzz`, unicode-decoded) so it lands.
                let cmd_is_apply = o
                    .get("cmd")
                    .and_then(Value::as_str)
                    .map(|c| c.trim() == "apply_patch")
                    .unwrap_or(false);
                if cmd_is_apply {
                    let patch = PATCH_KEYS.iter().find_map(|k| {
                        o.get(*k)
                            .and_then(Value::as_str)
                            .filter(|p| {
                                p.contains("*** Begin Patch") || p.contains("*** Update File")
                            })
                            .map(|p| decode_unicode_escapes(p))
                    });
                    if let Some(fuzz) = patch.as_deref().and_then(apply_patch_block_to_fuzz) {
                        eprintln!(
                            "[apply_patch-bridge] folded {{cmd:apply_patch, patch sibling}} → patch --fuzz"
                        );
                        o.insert("cmd".into(), Value::String(fuzz));
                        for k in PATCH_KEYS {
                            o.remove(*k);
                        }
                    } else if let Some(write) = synthesize_write_from_obj(o) {
                        // Create-from-scratch (Finding 5): `content` is a whole file, not a patch, so
                        // the fold found nothing. Synthesize the real write codex can't perform from
                        // the malformed `{cmd:apply_patch, path, content}` so the file actually lands.
                        eprintln!(
                            "[apply_patch-bridge] synthesized file write from {{path, content}} (create-from-scratch, Finding 5)"
                        );
                        o.insert("cmd".into(), Value::String(write));
                        o.remove("path");
                        o.remove("content");
                    } else if let Some(writes) = synthesize_writes_from_patches(o) {
                        // codex's STRUCTURED multi-file form: {cmd:apply_patch, patches:[{path,content},…]}
                        // — each entry is a whole file (raw content, no V4A markers), so neither the V4A
                        // fold nor the single-file synth fires and the bare shim can't consume the JSON →
                        // nothing lands (the gpt-oss rpn create-delivery residual, R2.3). Synthesize one
                        // shell write per entry so every file lands.
                        eprintln!(
                            "[apply_patch-bridge] synthesized file writes from {{cmd:apply_patch, patches/file_changes:[…]}}"
                        );
                        o.insert("cmd".into(), Value::String(writes));
                        o.remove("patches");
                        o.remove("file_changes");
                        o.remove("changes");
                    }
                }
                // Read-repair: gpt-oss frequently emits broken file reads (`sed -n 'src/main.rs'`,
                // `sed -n '1' '1' f`) that fail, so it never sees the file and can't build a matching
                // patch — reading is the decisive success factor. Its intent is unambiguous (a source
                // path in a read tool) and reading is non-destructive, so translate it to `cat <file>`.
                if read_repair_enabled() {
                    if let Some(fixed) = o
                        .get("cmd")
                        .and_then(Value::as_str)
                        .and_then(repair_broken_read)
                    {
                        eprintln!("[read-repair] broken file-read → {fixed}");
                        o.insert("cmd".into(), Value::String(fixed));
                    }
                }
                // `cat <path> <<EOF` (missing `>`) → `cat > <path> <<EOF`: without the redirect the
                // heredoc write is a silent no-op read and the file never lands (build-red autopsy).
                if heredoc_redirect_enabled() {
                    if let Some(fixed) = o
                        .get("cmd")
                        .and_then(Value::as_str)
                        .and_then(repair_heredoc_write)
                    {
                        eprintln!("[heredoc-redirect] `cat PATH <<EOF` missing `>` → write (was a no-op read)");
                        o.insert("cmd".into(), Value::String(fixed));
                    }
                }
                o.values_mut().for_each(walk);
            }
            _ => {}
        }
    }
    walk(&mut v);
    v.to_string()
}
