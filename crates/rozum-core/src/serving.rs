//! Engine-agnostic request-serving helpers shared across backends.
//!
//! Today this is tool-call parsing: turning a model's raw text output into
//! `(name, arguments_json)` pairs. Both the in-process MLX backend (whole-text,
//! at finalization) and the GGUF backend (streaming detector) used to carry their
//! own copy of the body-parsing logic; it lives here once now.
//! (extract-shared-serving-helpers — docs/specs/portability-and-the-backend-spi.md)

use serde_json::Value;

const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";

// DeepSeek-V2/V3 native tool-call markers. They are ADDED tokens with `special=false`, so the
// engine's skip-special decode KEEPS them (verified on the DeepSeek-Coder-V2-Lite tokenizer) — the
// markers reach the parser intact; only the parser was missing. The name sits OUTSIDE the JSON
// (after the sep), so the Qwen `<tool_call>{json}` loop and the loose-JSON fallback both miss it.
const DS_CALL_BEGIN: &str = "<｜tool▁call▁begin｜>";
const DS_CALL_END: &str = "<｜tool▁call▁end｜>";
const DS_SEP: &str = "<｜tool▁sep｜>";

/// Parse tool calls from a model's raw output into `(name, arguments_json)` pairs.
///
/// The primary, trained form is Qwen `<tool_call>{…}</tool_call>` (JSON or
/// XML/Hermes body). If the model emitted **no** `<tool_call>` envelope at all —
/// common for smaller models driven by a foreign (Claude/OpenAI) tool schema,
/// which instead emit a bare or ```json-fenced `{"name":…,"arguments":…}` — we
/// recover those too, so the agent loop still works. The fallback runs *only*
/// when there were no native blocks, so a legitimate ```json example inside an
/// ordinary answer is never mistaken for a tool call.
pub fn parse_tool_calls(text: &str) -> Vec<(String, String)> {
    // DeepSeek-V2/V3 native format: name is OUTSIDE the JSON (after `<｜tool▁sep｜>`), wrapped in
    // `<｜tool▁call▁begin｜>…<｜tool▁call▁end｜>` special-token markers → the `<tool_call>` loop and the
    // loose-JSON fallback both miss it. Parse it first when its markers are present.
    if text.contains(DS_CALL_BEGIN) {
        let ds = parse_deepseek_tool_calls(text);
        if !ds.is_empty() {
            return ds;
        }
    }
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find(TOOL_OPEN) {
        let after = &rest[open + TOOL_OPEN.len()..];
        // Tolerate a missing `</tool_call>` (model hit EOS right after a complete
        // JSON body): parse the trailing run instead of dropping it.
        let (body, next) = match after.find(TOOL_CLOSE) {
            Some(close) => (after[..close].trim(), Some(&after[close + TOOL_CLOSE.len()..])),
            None => (after.trim(), None),
        };
        if let Some(call) = parse_tool_call_body(body) {
            calls.push(call);
        }
        match next {
            Some(n) => rest = n,
            None => break,
        }
    }
    if calls.is_empty() {
        calls = parse_loose_tool_calls(text);
    }
    calls
}

/// DeepSeek-V2/V3 native tool calls:
/// `<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>NAME\n```json\n{args}\n```<｜tool▁call▁end｜>…`
/// (repeated per call). The name is the first line after `<｜tool▁sep｜>`; the args are the first
/// balanced JSON object after it (the ```json fence is incidental — the brace scan finds the object
/// whether fenced or bare). Tolerates a missing `<｜tool▁call▁end｜>` (EOS mid-call).
fn parse_deepseek_tool_calls(text: &str) -> Vec<(String, String)> {
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(b) = rest.find(DS_CALL_BEGIN) {
        let after = &rest[b + DS_CALL_BEGIN.len()..];
        let (body, next) = match after.find(DS_CALL_END) {
            Some(e) => (&after[..e], &after[e + DS_CALL_END.len()..]),
            None => (after, ""),
        };
        if let Some(sep) = body.find(DS_SEP) {
            let post = body[sep + DS_SEP.len()..].trim_start();
            let name = post.lines().next().unwrap_or("").trim();
            if !name.is_empty() {
                let args = balanced_json_objects(post)
                    .into_iter()
                    .find_map(|o| serde_json::from_str::<Value>(o).ok().filter(Value::is_object))
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                calls.push((name.to_string(), args));
            }
        }
        if next.is_empty() {
            break;
        }
        rest = next;
    }
    calls
}

/// Parse one tool-call body (the text inside `<tool_call>…</tool_call>`) into
/// `(name, arguments_json)`. Accepts either form Qwen3.6 emits nondeterministically:
///   - JSON:  `{"name":"f","arguments":{…}}`  (`"parameters"` accepted as an alias)
///   - XML:   `<function=f><parameter=k>v</parameter>…</function>`
pub fn parse_tool_call_body(body: &str) -> Option<(String, String)> {
    if let Ok(v) = serde_json::from_str::<Value>(body) {
        if let Some(call) = tool_call_from_value(&v, false) {
            return Some(call);
        }
    }
    parse_xml_function(body).or_else(|| parse_glm_arg_kv(body))
}

/// GLM-4.5/4.6/4.7 form inside `<tool_call>`: the function name is the leading run, then
/// `<arg_key>K</arg_key><arg_value>V</arg_value>` pairs — e.g.
/// `bash<arg_key>command</arg_key><arg_value>ls -la</arg_value>`. These tag tokens are SPECIAL in
/// the GLM tokenizer, so the engine must decode this run keeping special tokens (else they're
/// stripped and the body collapses to `bashcommandls -la`). Values are emitted raw (not tojson'd in
/// generation) → parse as JSON, fall back to a string, mirroring `parse_xml_function`.
fn parse_glm_arg_kv(body: &str) -> Option<(String, String)> {
    let kstart = body.find("<arg_key>")?;
    let name = body[..kstart].trim();
    if name.is_empty() {
        return None;
    }
    let mut args = serde_json::Map::new();
    let mut p = &body[kstart..];
    while let Some(ks) = p.find("<arg_key>") {
        let a2 = &p[ks + "<arg_key>".len()..];
        let Some(ke) = a2.find("</arg_key>") else { break };
        let key = a2[..ke].trim().to_string();
        let vrest = &a2[ke + "</arg_key>".len()..];
        let Some(vs) = vrest.find("<arg_value>") else { break };
        let v2 = &vrest[vs + "<arg_value>".len()..];
        let Some(ve) = v2.find("</arg_value>") else { break };
        let val = v2[..ve].trim();
        let jval =
            serde_json::from_str::<Value>(val).unwrap_or_else(|_| Value::String(val.to_string()));
        args.insert(key, jval);
        p = &v2[ve + "</arg_value>".len()..];
    }
    Some((name.to_string(), Value::Object(args).to_string()))
}

/// Extract just the tool name from a JSON body — the GGUF streaming detector only
/// needs the name (it forwards the raw JSON as the arguments delta).
pub fn tool_name(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body.trim())
        .ok()
        .as_ref()
        .and_then(|v| v.get("name"))
        .and_then(|n| n.as_str())
        .map(str::to_owned)
}

/// `(name, args)` from a JSON object with the tool-call shape. With `strict`, the
/// arguments must be a JSON object (used by the loose fallback to avoid eating
/// ordinary JSON content); otherwise arguments are optional (default `{}`).
fn tool_call_from_value(v: &Value, strict: bool) -> Option<(String, String)> {
    let name = v.get("name")?.as_str()?;
    if name.is_empty() {
        return None;
    }
    let args = v.get("arguments").or_else(|| v.get("parameters"));
    if strict {
        let a = args?;
        if !a.is_object() {
            return None;
        }
        return Some((name.to_string(), a.to_string()));
    }
    let args = args.map(|a| a.to_string()).unwrap_or_else(|| "{}".to_string());
    Some((name.to_string(), args))
}

/// XML / Hermes form: `<function=NAME> <parameter=KEY>VALUE</parameter> … </function>`.
fn parse_xml_function(body: &str) -> Option<(String, String)> {
    let fstart = body.find("<function=")?;
    let aft = &body[fstart + "<function=".len()..];
    let name_end = aft.find('>')?;
    let name = aft[..name_end].trim();
    if name.is_empty() {
        return None;
    }
    let mut args = serde_json::Map::new();
    let mut p = &aft[name_end + 1..];
    while let Some(ps) = p.find("<parameter=") {
        let a2 = &p[ps + "<parameter=".len()..];
        let Some(ke) = a2.find('>') else { break };
        let key = a2[..ke].trim().to_string();
        let vrest = &a2[ke + 1..];
        let Some(ve) = vrest.find("</parameter>") else { break };
        let val = vrest[..ve].trim();
        let jval = serde_json::from_str::<Value>(val)
            .unwrap_or_else(|_| Value::String(val.to_string()));
        args.insert(key, jval);
        p = &vrest[ve + "</parameter>".len()..];
    }
    Some((name.to_string(), Value::Object(args).to_string()))
}

/// Fallback for models that don't emit `<tool_call>`: tool-call-shaped JSON
/// objects anywhere in the text — bare or inside a ```json fence (the fence
/// markers aren't braces, so the object inside is found directly). Requires the
/// `{name:string, arguments|parameters:object}` signature.
fn parse_loose_tool_calls(text: &str) -> Vec<(String, String)> {
    // Strict path first: well-formed JSON (most models) — no false-positive risk.
    let mut calls = Vec::new();
    for obj in balanced_json_objects(text) {
        if let Ok(v) = serde_json::from_str::<Value>(obj) {
            if let Some(call) = tool_call_from_value(&v, true) {
                calls.push(call);
            }
        }
    }
    if !calls.is_empty() {
        return calls;
    }
    // Repair path: a MALFORMED `{"name":…}` — the classic LLM mistake of unescaped
    // quotes inside a string value (e.g. `"content":"…println!("{}", x)…"`) breaks
    // both `serde_json` AND the brace scanner. Find each `{"name"` and tolerantly
    // repair + parse it, so the call isn't dropped.
    let b = text.as_bytes();
    let mut i = 0;
    while let Some(start) = find_name_brace(b, i) {
        match repair_tool_object(b, start) {
            Some((repaired, end)) => {
                if let Ok(v) = serde_json::from_str::<Value>(&repaired) {
                    if let Some(call) = tool_call_from_value(&v, true) {
                        calls.push(call);
                    }
                }
                i = end.max(start + 1);
            }
            None => break,
        }
    }
    // Last resort: the GLM-4 form. GLM-4 emits a tool call as the WHOLE assistant output —
    // a bare function name on its own line, then the JSON arguments, terminated by the
    // `<|observation|>` stop token (already in GLM's config `eos_token_id`): e.g.
    // `get_weather\n{"city": "Paris"}`. No `<tool_call>` wrapper, and the name is OUTSIDE the
    // JSON, so the paths above miss it. Tight match (whole text = identifier + one JSON object)
    // keeps it from eating ordinary prose; fires only when nothing else parsed.
    if calls.is_empty() {
        if let Some(call) = parse_glm_tool_call(text) {
            calls.push(call);
        }
    }
    calls
}

/// GLM-4 tool-call form: a `<function_name>\n<json-object>` block (e.g. `get_weather\n{"city":
/// "Paris"}`). GLM emits it either as the WHOLE output (simple prompts) or — with a complex agent
/// tool set (Claude Code / Codex) — wrapped in a markdown ```fence``` amid prose (e.g.
/// ` ```bash\nRead\n{"file_path":"…"}\n``` `). Both are handled; the strict `name\n{object}` shape
/// (`glm_name_json`) keeps ordinary prose / code blocks from false-positiving.
fn parse_glm_tool_call(text: &str) -> Option<(String, String)> {
    if let Some(call) = glm_name_json(text.trim()) {
        return Some(call);
    }
    // Scan each ```…``` fenced block for a bare `name\n{json}` call.
    let mut rest = text;
    while let Some(open) = rest.find("```") {
        let after = &rest[open + 3..];
        let Some(nl) = after.find('\n') else { break }; // skip the fence/lang line (```bash)
        let body = &after[nl + 1..];
        let (inner, next) = match body.find("```") {
            Some(close) => (&body[..close], &body[close + 3..]),
            None => (body, ""),
        };
        if let Some(call) = glm_name_json(inner.trim()) {
            return Some(call);
        }
        if next.is_empty() {
            break;
        }
        rest = next;
    }
    // Embedded form: a lead-in prose line, then a bare `name\n{json}` with no fence — the
    // shape constrained decoding produces when GLM keeps a preamble ("Let me check…\nRead\n
    // {…}"). Neither the whole-text nor the fenced scan catches it; take the LAST such block.
    glm_embedded(text)
}

/// The LAST `{identifier}\n{balanced-json-object}` block in `text` (the call follows any
/// preamble), or `None`. Stricter than a substring scan: the name line must be a bare
/// identifier and the object must begin immediately after it — so prose with an inline `{…}`
/// won't match. Only reached as a last resort (nothing else parsed a call).
fn glm_embedded(text: &str) -> Option<(String, String)> {
    let mut line_start = 0usize;
    let mut best = None;
    for (rel, _) in text.match_indices('\n') {
        let name = text[line_start..rel].trim();
        if !name.is_empty()
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
        {
            let after = text[rel + 1..].trim_start();
            if after.starts_with('{') {
                if let Some(obj) = balanced_json_objects(after).into_iter().next() {
                    if after.starts_with(obj)
                        && serde_json::from_str::<Value>(obj).map(|v| v.is_object()).unwrap_or(false)
                    {
                        best = Some((name.to_string(), obj.to_string()));
                    }
                }
            }
        }
        line_start = rel + 1;
    }
    best
}

/// `<identifier>\n<json-object>` (trimmed) → `(name, args_json)`, else `None`. The bare-identifier
/// first line + single JSON object is GLM-4's tool-call shape and is strict enough that prose or a
/// code block (e.g. `fn main() {…}` — the name would contain spaces/parens) won't match.
fn glm_name_json(t: &str) -> Option<(String, String)> {
    let (first, rest) = t.split_once('\n')?;
    let name = first.trim();
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
    {
        return None;
    }
    let args = rest.trim();
    let v: Value = serde_json::from_str(args).ok()?;
    if !v.is_object() {
        return None;
    }
    Some((name.to_string(), args.to_string()))
}

/// Every top-level balanced `{…}` substring, tracking JSON string state so braces
/// inside a string value (e.g. code in a `"content"` argument) don't unbalance it.
fn balanced_json_objects(s: &str) -> Vec<&str> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'{' {
            let start = i;
            let (mut depth, mut in_str, mut esc) = (0i32, false, false);
            let mut j = i;
            while j < b.len() {
                let c = b[j];
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' => depth += 1,
                        b'}' => {
                            depth -= 1;
                            if depth == 0 {
                                out.push(&s[start..=j]);
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                j += 1;
            }
            i = j + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Index of a `{` whose first key is `name` (a tool-call opening), at or after `from`.
fn find_name_brace(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while let Some(rel) = b[i..].iter().position(|&c| c == b'{') {
        let pos = i + rel;
        let mut j = pos + 1;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b[j..].starts_with(b"\"name\"") {
            return Some(pos);
        }
        i = pos + 1;
    }
    None
}

/// Tolerantly scan a (possibly malformed) JSON object from `start` (a `{`), escaping
/// unescaped `"` and raw control chars inside string values, and return the repaired
/// JSON + the index just past the closing `}`. A `"` is a string CLOSE only if the
/// next non-space byte is `:` / `}` / `]` / end, or a `,` followed by the next key's
/// `"` — so a content quote (e.g. in `println!("{}", x)`) is escaped, not treated as
/// a close, and the braces inside that string never unbalance the object. `None` if
/// it never balances.
fn repair_tool_object(b: &[u8], start: usize) -> Option<(String, usize)> {
    let mut out: Vec<u8> = Vec::with_capacity(64);
    let (mut depth, mut in_str, mut esc) = (0i32, false, false);
    let mut i = start;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if esc {
                out.push(c);
                esc = false;
            } else if c == b'\\' {
                out.push(c);
                esc = true;
            } else if c == b'"' {
                let mut j = i + 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let closes = match b.get(j) {
                    None | Some(b':') | Some(b'}') | Some(b']') => true,
                    Some(b',') => {
                        let mut k = j + 1;
                        while k < b.len() && b[k].is_ascii_whitespace() {
                            k += 1;
                        }
                        b.get(k) == Some(&b'"')
                    }
                    _ => false,
                };
                if closes {
                    out.push(b'"');
                    in_str = false;
                } else {
                    out.extend_from_slice(b"\\\""); // escape a content quote
                }
            } else if c < 0x20 {
                match c {
                    b'\n' => out.extend_from_slice(b"\\n"),
                    b'\t' => out.extend_from_slice(b"\\t"),
                    b'\r' => out.extend_from_slice(b"\\r"),
                    _ => {} // drop other control chars
                }
            } else {
                out.push(c);
            }
        } else {
            out.push(c);
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return String::from_utf8(out).ok().map(|s| (s, i + 1));
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

/// Synthesize tool calls from a GLM **create-from-scratch artifact** — the failure mode where
/// GLM-4-0414, under a heavy agent prompt, shows the work instead of *naming* the tool, so
/// `parse_tool_calls` finds nothing and no file is written (the captured `turns=1 tools=0` cell).
/// Returns one `(tool_name, args_json)` per recovered call. Two REAL captured modes
/// (`docs/specs/glm-artifact-write-synth.md`), tried in order:
///
/// **Mode 2 (primary) — tool ARGS as JSON, no name.** GLM emits the offered tool's argument object in
/// a ```json fence (or bare): `{"file_path":…,"content":…}` (Write), `{"command":…}` (Bash). The name
/// is recovered by **matching the object's keys to an offered tool's `input_schema`**
/// ([`match_tool_by_args`]): the object's keys must be a subset of the tool's properties and include
/// all of its `required` — so `{file_path,content}` matches *only* Write, `{command,…}` *only* Bash.
/// An object that matches zero or ≥2 tools is skipped (no guess). Objects that already carry a `name`
/// are left to `parse_tool_calls`.
///
/// **Mode 1 (fallback) — raw content + prose filename.** Only if mode 2 found nothing: a ```toml/```rust
/// fence whose file path is named in the PRECEDING PROSE ("I'll create the `Cargo.toml` file:") becomes
/// a `Write`; fences with no recoverable filename (a ```bash `cargo run` command) are skipped.
///
/// **Mode 1b (last fallback) — complete program, no filename anywhere.** A ```rust fence that is a FULL
/// program (`fn main`) with no prose filename → a `Write` to the language default (`src/main.rs`). This
/// catches the weak-coder pattern (a small model narrates the whole, often correct solution in a fence
/// and never names Write) without firing on an incidental snippet (no entrypoint → skipped). **Gated to
/// the universal opt-in `ROZUM_ARTIFACT_SYNTH=1`** ([`fence_fallback_enabled`]) — OFF on the GLM-family
/// default path, which stays byte-identical. See [`default_path_for_full_program`].
///
/// **Returns EMPTY unless a guard holds** (a schema match, or a safe prose filename), so a chat answer
/// with an incidental code block is never written. The CALLER still gates on (GLM family) AND
/// (`parse_tool_calls` returned empty). `tools` is the request's offered tool set.
pub fn synth_glm_tool_calls(text: &str, tools: &[crate::backend::ToolDef]) -> Vec<(String, String)> {
    // ONE pass over the fenced blocks. A ```json fence (or any fence whose body is a JSON object) is
    // mode-2: tool ARGS without a name → recover the tool by matching keys to a schema. Any other
    // fence (```toml/```rust raw file content) is mode-1: filename from the preceding prose. Handling
    // both in one pass is what keeps mode-1 from grabbing a mode-2 JSON-args body as "file content"
    // (the bug that wrote `{"file_path":…}` INTO Cargo.toml).
    let write_tool = resolve_write_tool_name(tools);
    let mut out = Vec::new();
    let mut pos = 0usize; // scan cursor
    let mut prose_start = 0usize; // start of the prose preceding the current fence
    while let Some(rel) = text[pos..].find("```") {
        let open = pos + rel;
        let preceding = &text[prose_start..open];
        let after = &text[open + 3..];
        let Some(nl) = after.find('\n') else { break }; // unterminated fence header → stop
        let lang = after[..nl].trim().to_ascii_lowercase();
        let body_start = open + 3 + nl + 1;
        let (body_end, next) = match text[body_start..].find("```") {
            Some(c) => (body_start + c, body_start + c + 3),
            None => (text.len(), text.len()),
        };
        let body = text[body_start..body_end].trim();
        if lang == "json" || body.starts_with('{') {
            // Mode-2: bare tool-args JSON (GLM's common create form). Lenient parse tolerates GLM's
            // mismatched closing bracket (`]`/`)` for `}`); a `name` field means it's a full call →
            // parse_tool_calls's job, not ours.
            if let Some(m) = parse_tool_args_lenient(body, tools) {
                if !m.contains_key("name") {
                    if let Some(name) = match_tool_by_args(&m, tools) {
                        out.push((name, Value::Object(m).to_string()));
                    }
                }
            }
        } else if let (Some(path), Some(wt)) = (last_safe_filename(preceding), write_tool.as_deref()) {
            // Mode-1: raw file content; filename from the preceding prose.
            let mut m = serde_json::Map::new();
            m.insert("file_path".into(), Value::String(path));
            m.insert("content".into(), Value::String(format!("{}\n", body.trim_end_matches('\n'))));
            out.push((wt.to_string(), Value::Object(m).to_string()));
        } else if let (true, Some(path), Some(wt)) =
            (fence_fallback_enabled(), default_path_for_full_program(&lang, body), write_tool.as_deref())
        {
            // Mode-1b: a COMPLETE program in a known language with NO filename anywhere in the prose —
            // the weak-coder failure we kept scoring as "incapable": the model narrates the whole, often
            // CORRECT solution in a ```rust fence ("Here is the updated code:") and never names Write, so
            // nothing lands. Materialize it to the language's conventional entrypoint. The full-program
            // marker (an `fn main`) is the guard: an incidental snippet/example in a chat answer has none,
            // so it is left alone (preserving `synth_skips_chat_and_ambiguous`); a partial function-only
            // fence won't overwrite the whole file either. GATED to the universal opt-in (see
            // `fence_fallback_enabled`) so the GLM family default-on synth path is left byte-identical.
            let mut m = serde_json::Map::new();
            m.insert("file_path".into(), Value::String(path));
            m.insert("content".into(), Value::String(format!("{}\n", body.trim_end_matches('\n'))));
            out.push((wt.to_string(), Value::Object(m).to_string()));
        }
        prose_start = next;
        pos = next;
        if pos >= text.len() {
            break;
        }
    }
    // Also catch a bare (un-fenced) JSON args object, if no fence yielded anything.
    if out.is_empty() {
        for obj in balanced_json_objects(text) {
            if let Some(m) = parse_tool_args_lenient(obj, tools) {
                if !m.contains_key("name") {
                    if let Some(name) = match_tool_by_args(&m, tools) {
                        out.push((name, Value::Object(m).to_string()));
                    }
                }
            }
        }
    }
    out
}

/// Parse a tool-args JSON object, tolerating GLM's frequent mismatched closing bracket (it ends the
/// args object with `]` or `)` instead of `}` — the real captured malformation that made strict
/// `balanced_json_objects` miss it). Strict parse first; on failure, swap a single trailing
/// wrong-bracket for `}` and retry. `None` if it still isn't a JSON object.
fn parse_tool_args_lenient(body: &str, tools: &[crate::backend::ToolDef]) -> Option<serde_json::Map<String, Value>> {
    let t = body.trim();
    if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(t) {
        return Some(m);
    }
    // Tier 2: GLM closes the object with the wrong bracket (`]`/`)` for `}`).
    if t.starts_with('{') {
        if let Some(stripped) = t.strip_suffix(']').or_else(|| t.strip_suffix(')')) {
            let repaired = format!("{stripped}}}");
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&repaired) {
                return Some(m);
            }
        }
    }
    // Tier 3: GLM leaves UNESCAPED quotes inside a value (e.g. `println!("{}", x)`), which no bracket
    // fix can save. Extract key-by-key, driven by the OFFERED tool SCHEMAS (not hardcoded names).
    glm_kv_extract(t, tools)
}

/// Tolerant key-by-key extraction of a tool's args from MANGLED JSON, driven by the OFFERED tool
/// schemas (works for ANY agent's tool/arg names, not just claude's). For each tool whose required
/// params are all string-typed, read each required key from `t`: a **trailing** key — one with no
/// other property key after it (e.g. Write's `content`) — reads to the LAST quote in the object,
/// tolerating unescaped inner quotes in code; a non-trailing key (e.g. Bash's `command`, with
/// `description` after it) reads to its first quote. Returns the first tool whose required args fully
/// extract. `None` if none do.
fn glm_kv_extract(t: &str, tools: &[crate::backend::ToolDef]) -> Option<serde_json::Map<String, Value>> {
    for tool in tools {
        let Some(props) = tool.input_schema.get("properties").and_then(|p| p.as_object()) else {
            continue;
        };
        let required: Vec<&str> = tool
            .input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        // Only attempt tools whose required params are all string-typed (text args the synth recovers).
        if required.is_empty()
            || !required.iter().all(|k| {
                props.get(*k).and_then(|p| p.get("type")).and_then(|v| v.as_str()) == Some("string")
            })
        {
            continue;
        }
        let prop_keys: Vec<&str> = props.keys().map(String::as_str).collect();
        let mut m = serde_json::Map::new();
        let mut ok = true;
        for &k in &required {
            let Some(kpos) = t.find(&format!("\"{k}\"")) else {
                ok = false;
                break;
            };
            // "trailing" = no OTHER property key appears after this key → its value runs to the
            // object's last quote (so unescaped inner quotes are kept). Otherwise stop at the first.
            let after_key = &t[kpos + k.len() + 2..];
            let trailing = !prop_keys.iter().any(|o| *o != k && after_key.contains(&format!("\"{o}\"")));
            let val = if trailing { glm_str_to_last_quote(t, k) } else { glm_clean_str(t, k) };
            match val {
                Some(v) => {
                    m.insert(k.to_string(), Value::String(v));
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok && !m.is_empty() {
            return Some(m);
        }
    }
    None
}

/// The string after `"key": "`, read to the FIRST closing quote — for CLEAN values (paths, commands)
/// that don't contain quotes. JSON-unescaped.
fn glm_clean_str(t: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let after = &t[t.find(&pat)? + pat.len()..];
    let after = after[after.find(':')? + 1..].trim_start().strip_prefix('"')?;
    Some(glm_json_unescape(&after[..after.find('"')?]))
}

/// The value after `"key": "`, read to the LAST quote in the object — tolerant of GLM's unescaped
/// inner quotes (the value is code/text; the real terminator is the quote before the closing bracket).
/// Only safe for a TRAILING key (see [`glm_kv_extract`]).
fn glm_str_to_last_quote(t: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let after = &t[t.find(&pat)? + pat.len()..];
    let after = after[after.find(':')? + 1..].trim_start().strip_prefix('"')?;
    Some(glm_json_unescape(&after[..after.rfind('"')?]))
}

/// Decode the JSON string escapes GLM did emit (`\n \t \r \" \\ \/`); leave anything else verbatim.
fn glm_json_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('/') => out.push('/'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// The offered tool whose argument schema `args` fits — its keys ⊆ the tool's `properties` and all the
/// tool's `required` present — or `None` if zero or ≥2 tools match (ambiguous ⇒ never guess). This is
/// how mode-2 recovers the tool NAME GLM omitted. Empty `args` never matches.
fn match_tool_by_args(
    args: &serde_json::Map<String, Value>,
    tools: &[crate::backend::ToolDef],
) -> Option<String> {
    if args.is_empty() {
        return None;
    }
    let mut hit: Option<String> = None;
    for t in tools {
        let Some(props) = t.input_schema.get("properties").and_then(|p| p.as_object()) else {
            continue;
        };
        let keys_subset = args.keys().all(|k| props.contains_key(k));
        let required_present = t
            .input_schema
            .get("required")
            .and_then(|r| r.as_array())
            .map(|a| a.iter().filter_map(|v| v.as_str()).all(|r| args.contains_key(r)))
            .unwrap_or(true);
        if keys_subset && required_present {
            if hit.is_some() {
                return None; // ambiguous — matches more than one offered tool
            }
            hit = Some(t.name.clone());
        }
    }
    hit
}

/// The offered file-writing tool's name (for the mode-1 prose fallback): an exact `Write`, else a tool
/// whose schema is exactly a `{file_path|path, content}`-shaped writer.
fn resolve_write_tool_name(tools: &[crate::backend::ToolDef]) -> Option<String> {
    if let Some(t) = tools.iter().find(|t| t.name == "Write") {
        return Some(t.name.clone());
    }
    tools
        .iter()
        .find(|t| {
            t.input_schema
                .get("properties")
                .and_then(|p| p.as_object())
                .is_some_and(|p| p.contains_key("content") && (p.contains_key("file_path") || p.contains_key("path")))
        })
        .map(|t| t.name.clone())
}

/// Mode-1b is scoped to the UNIVERSAL artifact-synth opt-in (`ROZUM_ARTIFACT_SYNTH=1`), NOT the GLM
/// family default-on path. GLM's synth is separately validated (docs/specs/glm-artifact-write-synth.md)
/// and must stay byte-identical; materializing an unlabeled full-program fence is the deterministic-
/// delivery lever a small-model operator explicitly opts into. (Validated live: Coder-7B build 0/2→2/2
/// under this flag.)
fn fence_fallback_enabled() -> bool {
    matches!(std::env::var("ROZUM_ARTIFACT_SYNTH").ok().as_deref(), Some("1" | "true" | "on"))
}

/// Mode-1b: a fence body with NO filename in the prose. If it is a COMPLETE standalone program in a
/// recognized language (an entrypoint marker), return the language's conventional path so a model that
/// narrates the whole solution instead of naming `Write` still lands it. Conservative on purpose —
/// requires a full-program marker so an incidental snippet/example (no entrypoint) is NOT written, and
/// a partial function-only fence won't clobber the whole file. Only Rust binaries today (the agentic
/// create-from-scratch case); extend per measured need, not speculatively.
fn default_path_for_full_program(lang: &str, body: &str) -> Option<String> {
    match lang {
        "rust" | "rs" => body.contains("fn main").then(|| "src/main.rs".to_string()),
        _ => None,
    }
}

/// The LAST safe relative filename mentioned in `prose` (a fence's preceding text), or `None`. A token
/// is a filename if it is a known extensionless name (`Cargo.toml`, `Makefile`, …) or ends in a known
/// source/config extension; it must be a safe relative path ([`is_safe_relpath`]). "LAST" because GLM
/// names the file in the sentence right before its fence ("…create the `src/main.rs` file:"). Prose
/// with no such token (a command fence's "run `cargo run -- hello`") yields `None` → that fence is
/// skipped, which is the command-vs-file guard.
fn last_safe_filename(prose: &str) -> Option<String> {
    const KNOWN: &[&str] =
        &["Cargo.toml", "Cargo.lock", "Makefile", "Dockerfile", ".gitignore", "build.rs"];
    const EXTS: &[&str] = &[
        "rs", "toml", "md", "txt", "json", "yaml", "yml", "py", "cfg", "lock", "sh", "js", "ts",
        "tsx", "html", "css", "c", "h", "cpp", "hpp", "go", "java", "rb", "kt", "scala",
    ];
    let mut best = None;
    for raw in prose.split(|c: char| c.is_whitespace() || "`\"'(){}<>,;".contains(c)) {
        // strip surrounding quote/paren/sentence punctuation, but NOT a leading '.' (".gitignore")
        let tok = raw.trim_matches(|c: char| "`\"'():;,".contains(c));
        let tok = tok.strip_suffix('.').unwrap_or(tok); // trailing sentence period
        if tok.is_empty() {
            continue;
        }
        let is_file = KNOWN.contains(&tok)
            || tok.rsplit_once('.').is_some_and(|(stem, ext)| !stem.is_empty() && EXTS.contains(&ext));
        if is_file && is_safe_relpath(tok) {
            best = Some(tok.to_string()); // keep scanning; the LAST match wins
        }
    }
    best
}

/// A path is safe to synthesize a write for only if it is **relative** and stays in-tree: no absolute
/// (`/…`) or home (`~…`) root, no `..` segment, and a sane length. Refusing anything else keeps a
/// synthesized `Write` from escaping the workdir.
fn is_safe_relpath(p: &str) -> bool {
    !p.is_empty()
        && p.len() < 200
        && !p.starts_with('/')
        && !p.starts_with('~')
        && !p.split('/').any(|seg| seg == "..")
}

#[cfg(test)]
mod tests {
    use super::*;

    // The REAL captured GLM-4-32B create-from-scratch artifact (agentic.sh KEEP=1, turns=1 tools=0).
    const GLM_ARTIFACT: &str = include_str!("../tests/fixtures/glm_create_artifact.txt");
    const GLM_ARTIFACT_JSON: &str = include_str!("../tests/fixtures/glm_create_artifact_jsonmode.txt");

    // claude's real Write + Bash schemas, the matcher's input.
    fn claude_tools() -> Vec<crate::backend::ToolDef> {
        use crate::backend::ToolDef;
        vec![
            ToolDef {
                name: "Write".into(),
                description: "Write a file".into(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "properties":{"file_path":{"type":"string"},"content":{"type":"string"}},
                    "required":["file_path","content"]
                }),
            },
            ToolDef {
                name: "Bash".into(),
                description: "Run a command".into(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "properties":{"command":{"type":"string"},"timeout":{"type":"number"},
                        "description":{"type":"string"},"run_in_background":{"type":"boolean"}},
                    "required":["command"]
                }),
            },
        ]
    }

    #[test]
    fn synth_mode2_schema_matches_bare_json_args() {
        // The REAL captured format: tool ARGS as ```json fences, no name. Recover the name by schema.
        let calls = synth_glm_tool_calls(GLM_ARTIFACT_JSON, &claude_tools());
        // {file_path,content}->Write x2, {command,...}->Bash x1.
        assert_eq!(calls.len(), 3, "got: {calls:?}");
        assert_eq!(calls[0].0, "Write");
        assert_eq!(calls[1].0, "Write");
        assert_eq!(calls[2].0, "Bash");
        let a0: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert!(a0["file_path"].as_str().unwrap().ends_with("Cargo.toml"));
        assert!(a0["content"].as_str().unwrap().contains("[package]"));
        let a2: Value = serde_json::from_str(&calls[2].1).unwrap();
        assert!(a2["command"].as_str().unwrap().contains("cargo run"));
    }

    #[test]
    fn synth_repairs_malformed_json_and_writes_real_content() {
        // The REAL live failure: GLM closes the args object with `]` instead of `}`. The synth must
        // still recover Write{file_path, content=THE TOML/CODE} — NOT write the JSON wrapper into the
        // file (the bug that put `{"file_path":…}` into Cargo.toml).
        let malformed = include_str!("../tests/fixtures/glm_create_artifact_malformed_json.txt");
        let calls = synth_glm_tool_calls(malformed, &claude_tools());
        // 4 fences: Cargo.toml + src/main.rs (Write) then two `cargo run` (Bash).
        assert_eq!(calls.len(), 4, "got: {calls:?}");
        assert_eq!(calls[0].0, "Write");
        assert_eq!(calls[1].0, "Write");
        assert_eq!(calls[2].0, "Bash");
        let a0: Value = serde_json::from_str(&calls[0].1).unwrap();
        // file_path is GLM's full path, content is the ACTUAL toml — not the JSON wrapper.
        assert!(a0["file_path"].as_str().unwrap().ends_with("Cargo.toml"));
        let c0 = a0["content"].as_str().unwrap();
        assert!(c0.starts_with("[package]"), "content must be the toml, got: {c0:?}");
        assert!(!c0.contains("file_path"), "the JSON wrapper must NOT leak into the file content");
        // main.rs had UNESCAPED inner quotes (`println!(\"{}\")`) AND the `]` malformation — the
        // tolerant extractor must still recover the real code.
        let a1: Value = serde_json::from_str(&calls[1].1).unwrap();
        let c1 = a1["content"].as_str().unwrap();
        assert!(c1.contains("fn main"), "got: {c1:?}");
        assert!(c1.contains("println!(\"{}\""), "inner unescaped quotes preserved: {c1:?}");
        assert!(!c1.contains("file_path"), "no wrapper leak");
    }

    #[test]
    fn synth_generalizes_to_any_agent_tool_schema() {
        use crate::backend::ToolDef;
        // A NON-claude agent: tool "save_file" with args {filename, source} (source is the trailing
        // big-text field) and a shell tool "run" with {cmd, note} (cmd is first, note after).
        let tools = vec![
            ToolDef {
                name: "save_file".into(),
                description: "save".into(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "properties":{"filename":{"type":"string"},"source":{"type":"string"}},
                    "required":["filename","source"]
                }),
            },
            ToolDef {
                name: "run".into(),
                description: "run".into(),
                input_schema: serde_json::json!({
                    "type":"object",
                    "properties":{"cmd":{"type":"string"},"note":{"type":"string"}},
                    "required":["cmd"]
                }),
            },
        ];
        // MALFORMED (unescaped inner quotes + `]` bracket) with NON-claude key names → schema-driven
        // extraction must still recover the real source via the trailing-field detection.
        let art = "Make it:\n```json\n{\"filename\": \"hi.py\", \"source\": \"print(\"hi\")\nx = 1\"]\n```";
        let calls = synth_glm_tool_calls(art, &tools);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].0, "save_file");
        let a: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(a["filename"].as_str().unwrap(), "hi.py");
        let src = a["source"].as_str().unwrap();
        assert!(src.contains("print(\"hi\")") && src.contains("x = 1"), "source: {src:?}");
        // Non-trailing field: `cmd` is read to its FIRST quote, NOT over-reading into `note`.
        let bash = "```json\n{\"cmd\": \"ls -la\", \"note\": \"list\"}\n```";
        let c2 = synth_glm_tool_calls(bash, &tools);
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].0, "run");
        let a2: Value = serde_json::from_str(&c2[0].1).unwrap();
        assert_eq!(a2["cmd"].as_str().unwrap(), "ls -la"); // not "ls -la\", \"note\": \"list"
    }

    #[test]
    fn synth_mode1_prose_fallback_when_no_json_args() {
        // The other captured mode: raw content fences + filename in the preceding prose.
        let calls = synth_glm_tool_calls(GLM_ARTIFACT, &claude_tools());
        assert_eq!(calls.len(), 2, "got: {calls:?}");
        let paths: Vec<String> = calls
            .iter()
            .map(|(_, a)| serde_json::from_str::<Value>(a).unwrap()["file_path"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(paths, vec!["Cargo.toml", "src/main.rs"]);
        assert!(calls.iter().all(|(n, _)| n == "Write"));
    }

    #[test]
    fn synth_mode1b_universal_opt_in_and_guards() {
        // Mode-1b is the ONLY env-toggling synth test; every other synth test is robust to this flag's
        // value (their fences are labeled → Mode-1 precedence, or lack an `fn main` entrypoint), so the
        // process-global mutation here can't perturb a parallel test. Restored at the end regardless.
        let tools = claude_tools();
        // REAL Coder-7B pattern: the WHOLE program narrated in a ```rust fence with NO filename in the
        // prose ("Here is the updated code:"), never naming Write.
        let full = "Here is the updated code:\n\n```rust\nuse std::env;\nfn main() {\n    let a = env::args().nth(1).unwrap_or_default();\n    println!(\"{}\", a);\n}\n```\nThat should work.";

        // OFF by default → the GLM family default-on synth path stays byte-identical (no spurious Write).
        unsafe { std::env::remove_var("ROZUM_ARTIFACT_SYNTH") };
        assert!(synth_glm_tool_calls(full, &tools).is_empty(), "Mode-1b must be OFF without the opt-in");

        // ON under the universal opt-in → the unlabeled complete program lands at src/main.rs.
        unsafe { std::env::set_var("ROZUM_ARTIFACT_SYNTH", "1") };
        let calls = synth_glm_tool_calls(full, &tools);
        assert_eq!(calls.len(), 1, "got: {calls:?}");
        assert_eq!(calls[0].0, "Write");
        let v: Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(v["file_path"], "src/main.rs");
        assert!(v["content"].as_str().unwrap().contains("fn main"));
        // Guards (opt-in ON): a snippet with NO entrypoint is left alone...
        assert!(synth_glm_tool_calls("Here's a loop:\n```rust\nfor i in 0..3 { println!(\"{i}\"); }\n```", &tools).is_empty());
        // ...an explicit prose filename still wins (mode-1), not the src/main.rs default...
        let labeled: Value = serde_json::from_str(&synth_glm_tool_calls("I'll create src/bin/tool.rs:\n```rust\nfn main() {}\n```", &tools)[0].1).unwrap();
        assert_eq!(labeled["file_path"], "src/bin/tool.rs", "explicit filename wins over the default");
        // ...and nothing is synthesized when no Write tool is offered.
        assert!(synth_glm_tool_calls("Here is the code:\n```rust\nfn main(){}\n```", &[]).is_empty());

        unsafe { std::env::remove_var("ROZUM_ARTIFACT_SYNTH") }; // restore
    }

    #[test]
    fn synth_skips_chat_and_ambiguous() {
        let tools = claude_tools();
        // Chat example code: no JSON args, no "create the X file" prose → empty.
        let chat = "Here's how a Rust loop works:\n```rust\nfor i in 0..3 { println!(\"{i}\"); }\n```\nThat iterates three times.";
        assert!(synth_glm_tool_calls(chat, &tools).is_empty());
        // Pure prose → empty.
        assert!(synth_glm_tool_calls("I would create a Cargo.toml with the usual fields.", &tools).is_empty());
        // A bare JSON object matching NO offered tool's schema → empty (no guess).
        assert!(synth_glm_tool_calls("```json\n{\"unrelated_key\": 1}\n```", &tools).is_empty());
        // An object that already carries a name is left to parse_tool_calls (not synthesized here).
        assert!(synth_glm_tool_calls("```json\n{\"name\":\"Write\",\"file_path\":\"a\",\"content\":\"b\"}\n```", &tools).is_empty());
    }

    #[test]
    fn last_safe_filename_and_path_safety() {
        assert_eq!(last_safe_filename("First, I'll create the Cargo.toml file:").as_deref(), Some("Cargo.toml"));
        assert_eq!(last_safe_filename("the src/main.rs file with the code").as_deref(), Some("src/main.rs"));
        // Command prose → no filename.
        assert_eq!(last_safe_filename("run the program with `cargo run -- hello`"), None);
        // Path safety: reject escapes/absolute even if extension matches.
        assert!(is_safe_relpath("src/main.rs"));
        assert!(!is_safe_relpath("/etc/passwd.txt"));
        assert!(!is_safe_relpath("../escape.rs"));
        assert!(!is_safe_relpath("~/secret.toml"));
        assert_eq!(last_safe_filename("write to /abs/path/main.rs please"), None, "absolute path refused");
    }

    #[test]
    fn glm4_tool_call_form() {
        // GLM-4: bare name line + JSON object, terminated at gen time by <|observation|> (stripped).
        let calls = parse_tool_calls("get_weather\n{\"city\": \"Paris\"}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "get_weather");
        assert!(calls[0].1.contains("Paris"));
        // multi-line JSON args still parse (everything after the first newline is the object)
        let ml = parse_tool_calls("search_web\n{\n  \"q\": \"rust traits\",\n  \"n\": 3\n}");
        assert_eq!(ml.len(), 1);
        assert_eq!(ml[0].0, "search_web");
        // fenced form: GLM wraps the call in ```bash … ``` amid prose (the agent-context shape)
        let fenced = parse_tool_calls(
            "I'll read the file:\n\n```bash\nRead\n{\"file_path\": \"src/main.rs\"}\n```\n\nThen fix it.",
        );
        assert_eq!(fenced.len(), 1);
        assert_eq!(fenced[0].0, "Read");
        assert!(fenced[0].1.contains("src/main.rs"));
        // a fenced block that ISN'T a tool call (real code) must not false-positive
        assert!(parse_tool_calls("Here:\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```").is_empty());
        // does NOT eat ordinary prose, a non-object body, or an embedded object mid-sentence
        assert!(parse_tool_calls("Sure, here is the answer.\nIt is 42.").is_empty());
        assert!(parse_tool_calls("foo\n[1, 2, 3]").is_empty());
        assert!(parse_tool_calls("The result is\nsunny today").is_empty());
        // a real <tool_call> wrapper still takes precedence (GLM path is last-resort only)
        let wrapped = parse_tool_calls("<tool_call>{\"name\":\"x\",\"arguments\":{}}</tool_call>");
        assert_eq!(wrapped.len(), 1);
        assert_eq!(wrapped[0].0, "x");
    }

    #[test]
    fn glm4_embedded_after_prose() {
        // Constrained decoding forces clean args but GLM may keep a lead-in prose line and
        // drop the fence: `prose\nName\n{json}` — caught by the embedded last-resort scan.
        let c = parse_tool_calls(
            "Let me first check the contents of src/main.rs.\n\nRead\n{\"file_path\": \"src/main.rs\"}",
        );
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, "Read");
        assert!(c[0].1.contains("src/main.rs"));
        // multiple lines of preamble + trailing newline still resolve to the LAST call block
        let c2 = parse_tool_calls("Thinking…\nI'll edit.\nEdit\n{\"path\": \"a\", \"new\": \"b\"}\n");
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].0, "Edit");
        // prose with an inline object (object NOT immediately after a bare-identifier line)
        // must not false-positive
        assert!(parse_tool_calls("The config is set to {\"x\": 1} in the file.").is_empty());
        assert!(parse_tool_calls("Here is an example value\nfor the field {\"x\": 1}.").is_empty());
    }

    #[test]
    fn native_tool_call_blocks() {
        let text = "sure <tool_call>\n{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Paris\"}}\n</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "get_weather");
        assert!(calls[0].1.contains("Paris"));

        assert!(parse_tool_calls("plain answer, no tools").is_empty());

        let two = "<tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call>\
                   <tool_call>{\"name\":\"b\",\"arguments\":{\"x\":1}}</tool_call>";
        let calls = parse_tool_calls(two);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[1].0, "b");
    }

    #[test]
    fn glm_arg_kv_form() {
        // GLM-4.5/4.6/4.7 form: name then <arg_key>/<arg_value> pairs (tags kept by a
        // special-token-preserving decode). Two args, raw (un-quoted) values → strings.
        let body = "bash<arg_key>command</arg_key><arg_value>ls -la</arg_value>\
                    <arg_key>description</arg_key><arg_value>List files</arg_value>";
        let (name, args) = parse_tool_call_body(body).unwrap();
        assert_eq!(name, "bash");
        let v: Value = serde_json::from_str(&args).unwrap();
        assert_eq!(v["command"], "ls -la");
        assert_eq!(v["description"], "List files");
        // Whole-text parse with the `<tool_call>` envelope + leading prose.
        let text = "I'll list them.<tool_call>bash<arg_key>command</arg_key>\
                    <arg_value>ls</arg_value></tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "bash");
        assert!(calls[0].1.contains("ls"));
    }

    #[test]
    fn deepseek_v2_native_tool_call() {
        // DeepSeek-V2/V3 form: name after <｜tool▁sep｜>, args in a ```json fence, special-token markers.
        let text = "Sure.<｜tool▁calls▁begin｜><｜tool▁call▁begin｜>function<｜tool▁sep｜>bash\n\
                    ```json\n{\"command\": \"ls -la\"}\n```<｜tool▁call▁end｜><｜tool▁calls▁end｜>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "one deepseek call");
        assert_eq!(calls[0].0, "bash");
        assert!(calls[0].1.contains("ls -la"));
        // Two calls, unfenced args (robustness).
        let two = "<｜tool▁calls▁begin｜>\
                   <｜tool▁call▁begin｜>function<｜tool▁sep｜>read\n{\"path\":\"a\"}<｜tool▁call▁end｜>\
                   <｜tool▁call▁begin｜>function<｜tool▁sep｜>write\n{\"path\":\"b\"}<｜tool▁call▁end｜>\
                   <｜tool▁calls▁end｜>";
        let c2 = parse_tool_calls(two);
        assert_eq!(c2.len(), 2);
        assert_eq!(c2[0].0, "read");
        assert_eq!(c2[1].0, "write");
        // Missing close marker (EOS mid-call) still recovers the call.
        let cut = "<｜tool▁call▁begin｜>function<｜tool▁sep｜>bash\n{\"command\":\"pwd\"}";
        let cc = parse_tool_calls(cut);
        assert_eq!(cc.len(), 1);
        assert_eq!(cc[0].0, "bash");
    }

    #[test]
    fn close_tag_tolerance_and_nested_braces() {
        let p = parse_tool_calls;
        // Nested braces in a string arg (code) — the close tag delimits.
        let code = p("<tool_call>{\"name\":\"write_file\",\"arguments\":{\"content\":\"fn add()->i32{ a + b }\"}}</tool_call>");
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].0, "write_file");
        assert!(code[0].1.contains("a + b"));
        // MISSING close tag (model hit EOS after a complete body) — recovered.
        assert_eq!(p("<tool_call>{\"name\":\"g\",\"arguments\":{}}"), vec![("g".into(), "{}".into())]);
        // XML / Hermes form inside the envelope.
        let xml = p("<tool_call>\n<function=write_file>\n<parameter=path>\nadd.rs\n</parameter>\n</function>\n</tool_call>");
        assert_eq!(xml.len(), 1);
        assert_eq!(xml[0].0, "write_file");
        assert!(xml[0].1.contains("add.rs"));
    }

    #[test]
    fn xml_hermes_form() {
        let body = "<function=search><parameter=q>cats</parameter></function>";
        let (name, args) = parse_tool_call_body(body).unwrap();
        assert_eq!(name, "search");
        assert!(args.contains("cats"));
    }

    #[test]
    fn qwen_coder_create_from_scratch_xml_multiline_content() {
        // The REAL Qwen3-Coder create-from-scratch shape: a `<tool_call>` wrapping the Hermes
        // `<function=…>` form whose `content` parameter is a multi-line Rust file with braces,
        // quotes and `{}` interpolation. Isolation check (green-matrix-min-footprint): the parser
        // MUST recover name + the content intact (braces/newlines must not unbalance or truncate).
        let text = "<tool_call>\n<function=write_file>\n<parameter=path>src/main.rs</parameter>\n\
            <parameter=content>\nfn reverse(s: &str) -> String { s.chars().rev().collect() }\n\
            fn main() {\n    let a: Vec<String> = std::env::args().collect();\n    \
            println!(\"{}\", reverse(&a[1]));\n}\n</parameter>\n</function>\n</tool_call>";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "expected one tool call, got: {calls:?}");
        assert_eq!(calls[0].0, "write_file");
        // content must survive verbatim-ish (key tokens present, not mangled to JSON/truncated).
        assert!(calls[0].1.contains("src/main.rs"), "path lost: {}", calls[0].1);
        assert!(calls[0].1.contains("fn reverse"), "content head lost: {}", calls[0].1);
        assert!(calls[0].1.contains("reverse(&a[1])"), "content tail lost (truncated?): {}", calls[0].1);
        // and the arguments string must be valid JSON the agent can consume.
        assert!(serde_json::from_str::<serde_json::Value>(&calls[0].1).is_ok(), "args not JSON: {}", calls[0].1);
    }

    #[test]
    fn loose_markdown_json_fence_recovered() {
        // What weaker models emit when driven by a foreign tool schema: a fenced
        // ```json block with {name, arguments} and NO <tool_call> wrapper. The
        // "content" arg holds braces, which must not unbalance the scanner.
        let text = "I'll create the file.\n```json\n{\n  \"name\": \"Write\",\n  \"arguments\": {\n    \"file_path\": \"/tmp/Cargo.toml\",\n    \"content\": \"[package]\\nname = \\\"x\\\"\\n\"\n  }\n}\n```\n";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].0, "Write");
        assert!(calls[0].1.contains("Cargo.toml"));
    }

    #[test]
    fn loose_bare_json_recovered() {
        let calls = parse_tool_calls("{\"name\":\"ls\",\"arguments\":{\"path\":\".\"}}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "ls");
    }

    #[test]
    fn loose_requires_object_args_no_false_positive() {
        // A plain JSON answer that happens to have a "name" but no object args, or
        // a non-tool shape, must NOT be parsed as a tool call.
        assert!(parse_tool_calls("Here is data: {\"name\":\"Alice\",\"age\":30}").is_empty());
        assert!(parse_tool_calls("{\"name\":\"x\",\"arguments\":\"not an object\"}").is_empty());
    }

    #[test]
    fn native_blocks_suppress_loose_fallback() {
        // When the model used <tool_call>, a stray ```json example in the same
        // answer is NOT additionally parsed.
        let text = "call it <tool_call>{\"name\":\"a\",\"arguments\":{}}</tool_call> e.g. ```json\n{\"name\":\"b\",\"arguments\":{}}\n```";
        let calls = parse_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "a");
    }

    #[test]
    fn repair_unescaped_quotes_in_code_content() {
        // The classic weak-model malformation: unescaped quotes inside Rust code in a
        // "content" arg. Includes `"{}"` whose closing quote is followed by a comma —
        // a content quote that a naive parser mistakes for a value close.
        let malformed = concat!(
            "I'll write it.\n```json\n{\"name\":\"Write\",\"arguments\":{",
            "\"file_path\":\"/tmp/main.rs\",",
            "\"content\":\"fn main(){ println!(\"{}\", x); }\"",
            "}}\n```"
        );
        let calls = parse_tool_calls(malformed);
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].0, "Write");
        // The repaired arguments must be valid JSON with the code preserved.
        let v: serde_json::Value = serde_json::from_str(&calls[0].1).unwrap();
        assert_eq!(v["file_path"], "/tmp/main.rs");
        assert!(
            v["content"].as_str().unwrap().contains("println!(\"{}\", x)"),
            "content: {}",
            v["content"]
        );
    }

    #[test]
    fn repair_does_not_fire_for_wellformed_or_non_calls() {
        // Well-formed call → handled by the strict path (one call, not double-counted).
        let ok = "```json\n{\"name\":\"ls\",\"arguments\":{\"path\":\".\"}}\n```";
        assert_eq!(parse_tool_calls(ok).len(), 1);
        // A `{"name":…}` without object args is still not a tool call after repair.
        assert!(parse_tool_calls("{\"name\":\"Alice\",\"age\":30}").is_empty());
        assert!(parse_tool_calls("here is { some prose } with braces").is_empty());
    }

    #[test]
    fn parameters_alias() {
        let calls = parse_tool_calls("{\"name\":\"f\",\"parameters\":{\"k\":1}}");
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains("\"k\""));
    }
}
