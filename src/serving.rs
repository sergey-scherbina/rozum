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
    parse_xml_function(body)
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
    None
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

#[cfg(test)]
mod tests {
    use super::*;

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
