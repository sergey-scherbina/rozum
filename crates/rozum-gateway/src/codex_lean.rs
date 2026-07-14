//! codex-lean: trimming a load-sensitive model's codex tool set + system prompt.
//!
//! Extracted verbatim from `gateway.rs` (gw-monolith-decompose): the pure policy helpers deciding
//! WHICH codex tools/instructions/reasoning a small local model gets (the codex analog of claude
//! `--lean`). No gateway-internal deps. The request-handling call sites reach these via `super`'s
//! glob-import; the tests stay in `gateway.rs`.

/// codex-lean: codex hands a LOCAL model ~18 tools (most are meta-tool noise — plans, goals,
/// plugins, MCP listing, `request_user_input`, …) on top of a ~21 KB system prompt. A small model
/// drowns in it: it stalls after diagnosing, or grabs a meta-tool instead of editing
/// (`docs/matrix-failure-analysis.md` Findings 1a/3). Dropping the non-coding tools is the codex
/// analog of claude `--lean` (which lifts the same model to 5/5). Gated by `ROZUM_CODEX_LEAN`
/// (off → codex's full tool set, unchanged). The keep-set is the actual coding surface.
pub(crate) fn codex_lean_keep(name: &str) -> bool {
    // Shell + file I/O + patching: everything a coding agent needs. Anything containing these
    // stems survives (covers exec_command, write_stdin, apply_patch, shell, read/write/edit, …).
    const KEEP_STEMS: &[&str] = &[
        "exec", "shell", "command", "stdin", "apply_patch", "patch", "read_file", "write_file",
        "edit", "view_image",
    ];
    let n = name.to_ascii_lowercase();
    KEEP_STEMS.iter().any(|s| n.contains(s))
}

/// A short, focused replacement for codex's ~21 KB system prompt, for load-sensitive local
/// reasoning models. The load bisection (`docs/specs/constrained-gptoss-delivery.md`) proved
/// CONTEXT SIZE is the DOMINANT breaker of tool-call delivery on gpt-oss — more than the V4A
/// format or tool count: with the easy `write_file` tool, a 30 KB prompt drops it to 0/3 (it
/// emits empty content, no tool call), while a ~20-byte prompt is 3/3. `codex_lean_keep` trims
/// only TOOLS; this trims the INSTRUCTIONS too. The tool *schemas* (kept by lean) carry the
/// argument shapes, so a short prompt suffices.
pub(crate) const LEAN_CODING_PROMPT: &str = "You are a coding agent in a sandboxed shell, already in the \
project directory. Complete the WHOLE task before you stop — never stop after a single command; \
keep going until every file is written and the success check passes.\n\
\n\
Run shell with the exec_command tool. Every call's argument is a JSON object {\"cmd\": \"…\"} — \
NEVER put prose or reasoning there, only the shell command.\n\
\n\
To CREATE or EDIT files, call the apply_patch TOOL (do NOT write files with a shell heredoc — \
`cat <<EOF` mangles quotes/backslashes/newlines; the runtime writes apply_patch bodies byte-for-byte). \
One apply_patch call can carry several files.\n\
\n\
CREATE a new file — `*** Add File:` then every line of the file prefixed with a single `+`:\n\
*** Begin Patch\n\
*** Add File: Cargo.toml\n\
+[package]\n\
+name = \"x\"\n\
*** Add File: src/main.rs\n\
+fn main() {\n\
+    println!(\"hi\");\n\
+}\n\
*** End Patch\n\
\n\
EDIT an existing file — `*** Update File:` (leading space = context, `-` = removed, `+` = added):\n\
*** Begin Patch\n\
*** Update File: <relative/path>\n\
@@\n\
 <unchanged context line>\n\
-<old line>\n\
+<new line>\n\
*** End Patch\n\
\n\
Use exec_command only to RUN things (build/test/run), never to write file bodies.\n\
\n\
When the task's success condition is met, reply with one short confirmation line and stop. Do not \
ask for confirmation or permission.";

/// Models whose tool-calling collapses under a large context (so they get [`LEAN_CODING_PROMPT`]
/// instead of codex's full instructions). gpt-oss reasons 4-8× more than Qwen3.6-35B and emits no
/// tool call at all under codex's 21 KB+ prompt; the capable tier (35B) is fine with the full
/// instructions (4/5) and is deliberately excluded so it is never regressed.
pub(crate) fn model_is_load_sensitive(model_id: &str) -> bool {
    let m = model_id.to_ascii_lowercase();
    m.contains("gpt-oss") || m.contains("gpt_oss")
}

/// The instructions to actually send for a codex `/v1/responses` request: codex's own when the
/// trim doesn't apply, or [`LEAN_CODING_PROMPT`] when it does. Gated by `ROZUM_CODEX_LEAN` (shares
/// the tool-lean switch) AND `model_is_load_sensitive`; override with `ROZUM_CODEX_LEAN_PROMPT`
/// (`0`/`off` = never trim, anything else = always trim). Behaviour-preserving for non-gpt-oss
/// models (returns codex's instructions verbatim).
pub(crate) fn codex_effective_instructions(model_id: &str, original: Option<&str>) -> Option<String> {
    let force = std::env::var("ROZUM_CODEX_LEAN_PROMPT").ok();
    let lean_tools = std::env::var("ROZUM_CODEX_LEAN").map(|v| v != "0").unwrap_or(true);
    if lean_prompt_on(model_id, force.as_deref(), lean_tools) {
        Some(LEAN_CODING_PROMPT.to_string())
    } else {
        original.map(str::to_string)
    }
}

/// The reasoning effort to actually use for a codex request. A load-sensitive model (gpt-oss) on the
/// lean path is forced to **low**: gpt-oss reasons 4-8× more than the 35B and emits a long `analysis`
/// CoT before EVERY tool call, which across a multi-turn agentic loop accumulates into RUN_TIMEOUTs —
/// codex sends `medium` per-request, which (without this) overrides the intended low default and times
/// the model out. Same gate as the lean prompt; the requested effort passes through for every other
/// model. Validated: codex×gpt-oss×rpn timed out 3/6 at medium.
pub(crate) fn codex_effective_reasoning(model_id: &str, requested: Option<String>) -> Option<String> {
    let force = std::env::var("ROZUM_CODEX_LEAN_PROMPT").ok();
    let lean_tools = std::env::var("ROZUM_CODEX_LEAN").map(|v| v != "0").unwrap_or(true);
    if lean_prompt_on(model_id, force.as_deref(), lean_tools) {
        Some("low".to_string())
    } else {
        requested
    }
}

/// Pure decision for [`codex_effective_instructions`] (env split out so it is race-free to test).
/// `force` is `ROZUM_CODEX_LEAN_PROMPT` (`0`/`off` = never, any other value = always); when unset,
/// the trim follows the tool-lean switch AND model load-sensitivity.
pub(crate) fn lean_prompt_on(model_id: &str, force: Option<&str>, lean_tools: bool) -> bool {
    match force {
        Some("0" | "false" | "off") => false,
        Some(_) => true,
        None => lean_tools && model_is_load_sensitive(model_id),
    }
}
