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
    let mut calls = Vec::new();
    for obj in balanced_json_objects(text) {
        if let Ok(v) = serde_json::from_str::<Value>(obj) {
            if let Some(call) = tool_call_from_value(&v, true) {
                calls.push(call);
            }
        }
    }
    calls
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parameters_alias() {
        let calls = parse_tool_calls("{\"name\":\"f\",\"parameters\":{\"k\":1}}");
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.contains("\"k\""));
    }
}
