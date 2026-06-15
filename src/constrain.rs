//! Constrained JSON decoding for tool-call arguments.
//!
//! A JSON-Schema (a common subset) is compiled into a **prefix acceptor**: given
//! the partial JSON emitted so far, it answers whether that text is a valid
//! complete value, a valid-but-incomplete prefix, or already invalid. The decode
//! loop uses it to mask the sampler so a small model cannot emit a malformed or
//! schema-violating arguments object. See `docs/specs/constrained-tool-decoding.md`.
//!
//! The acceptor is **stateless** w.r.t. the input — it re-validates the whole
//! partial-JSON suffix each step. Tool arguments are short, so this is cheap, and
//! it is far easier to get right than an incremental state machine. It also lets
//! the caller swap the schema mid-stream (e.g. resolve `arguments` once the tool
//! `name` literal is known) for free.

use serde_json::Value;

/// Whether a partial JSON string is a valid (in)complete prefix of the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix {
    /// A complete value matched (only trailing whitespace may remain).
    Complete,
    /// A valid but incomplete prefix — more characters are needed.
    Partial,
    /// The text violates the schema and cannot be extended to match.
    Invalid,
}

/// The compiled subset of JSON Schema we constrain against. Anything outside the
/// subset compiles to [`Schema::Any`] (generic well-formed JSON) — we relax, never
/// over-reject.
#[derive(Debug, Clone, PartialEq)]
pub enum Schema {
    Bool,
    Integer,
    Number,
    /// A string; `allowed` (from `enum`/`const`) restricts it to fixed literals.
    Str { allowed: Option<Vec<String>> },
    Array { items: Box<Schema> },
    Object {
        props: Vec<(String, Schema)>,
        required: Vec<String>,
    },
    /// Unknown / unsupported subschema: accept any well-formed JSON value.
    Any,
}

/// Outcome of matching a complete value of a schema at the start of `s`.
enum M<'a> {
    /// A complete value matched; `rest` is the unconsumed remainder.
    Done(&'a str),
    /// `s` is a valid but incomplete prefix of such a value.
    More,
    /// `s` cannot begin a value of this schema.
    No,
}

impl Schema {
    /// Compile a JSON-Schema `Value` into the supported subset. Unsupported
    /// constructs fall back to [`Schema::Any`].
    pub fn parse(schema: &Value) -> Self {
        let obj = match schema.as_object() {
            Some(o) => o,
            None => return Schema::Any,
        };

        // `enum` / `const` of strings → a fixed literal set (most precise).
        if let Some(arr) = obj.get("enum").and_then(Value::as_array) {
            let lits: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if !lits.is_empty() && lits.len() == arr.len() {
                return Schema::Str { allowed: Some(lits) };
            }
            return Schema::Any;
        }
        if let Some(c) = obj.get("const").and_then(Value::as_str) {
            return Schema::Str {
                allowed: Some(vec![c.to_string()]),
            };
        }

        match obj.get("type").and_then(Value::as_str) {
            Some("boolean") => Schema::Bool,
            Some("integer") => Schema::Integer,
            Some("number") => Schema::Number,
            Some("string") => Schema::Str { allowed: None },
            Some("array") => {
                let items = obj
                    .get("items")
                    .map(Schema::parse)
                    .unwrap_or(Schema::Any);
                Schema::Array {
                    items: Box::new(items),
                }
            }
            Some("object") => {
                let props: Vec<(String, Schema)> = obj
                    .get("properties")
                    .and_then(Value::as_object)
                    .map(|p| p.iter().map(|(k, v)| (k.clone(), Schema::parse(v))).collect())
                    .unwrap_or_default();
                let required: Vec<String> = obj
                    .get("required")
                    .and_then(Value::as_array)
                    .map(|r| r.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                    .unwrap_or_default();
                Schema::Object { props, required }
            }
            // No (or unknown) `type` → accept any JSON value.
            _ => Schema::Any,
        }
    }

    /// Classify `s` (a partial JSON value) against this schema.
    pub fn prefix(&self, s: &str) -> Prefix {
        match match_value(self, s) {
            M::Done(rest) => {
                if json_ws(rest).is_empty() {
                    Prefix::Complete
                } else {
                    Prefix::Invalid // trailing junk after a complete value
                }
            }
            M::More => Prefix::Partial,
            M::No => Prefix::Invalid,
        }
    }

    /// `true` iff `s` is a complete value of this schema.
    pub fn is_complete(&self, s: &str) -> bool {
        self.prefix(s) == Prefix::Complete
    }
}

/// Strip leading JSON whitespace (ASCII subset only — not Unicode).
fn json_ws(s: &str) -> &str {
    s.trim_start_matches([' ', '\t', '\n', '\r'])
}

/// Match a single complete value of `schema` at the start of `s` (after ws).
fn match_value<'a>(schema: &Schema, s: &'a str) -> M<'a> {
    let s = json_ws(s);
    if s.is_empty() {
        return M::More;
    }
    match schema {
        Schema::Bool => match_keyword_set(s, &["true", "false"]),
        Schema::Integer => match_number(s, false),
        Schema::Number => match_number(s, true),
        Schema::Str { allowed } => match_string(s, allowed.as_deref()),
        Schema::Array { items } => match_array(items, s),
        Schema::Object { props, required } => match_object(props, required, s),
        Schema::Any => match_any(s),
    }
}

/// Match one of a fixed set of bare keywords (`true`/`false`/`null`).
fn match_keyword_set<'a>(s: &'a str, kws: &[&str]) -> M<'a> {
    let mut partial = false;
    for kw in kws {
        if let Some(rest) = s.strip_prefix(kw) {
            return M::Done(rest);
        }
        if kw.starts_with(s) {
            partial = true; // `s` is a proper prefix of this keyword
        }
    }
    if partial {
        M::More
    } else {
        M::No
    }
}

/// Match a JSON number prefix. `frac` allows `.`/exponent (number vs integer).
fn match_number<'a>(s: &'a str, frac: bool) -> M<'a> {
    let bytes = s.as_bytes();
    let mut i = 0;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    let int_start = i;
    while bytes.get(i).is_some_and(u8::is_ascii_digit) {
        i += 1;
    }
    let have_int_digits = i > int_start;
    if frac {
        if bytes.get(i) == Some(&b'.') {
            i += 1;
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
        if matches!(bytes.get(i), Some(&b'e' | &b'E')) {
            i += 1;
            if matches!(bytes.get(i), Some(&b'+' | &b'-')) {
                i += 1;
            }
            while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
        }
    }
    if i == bytes.len() {
        // Ran to the end: a number may continue (more digits/frac/exp).
        return M::More;
    }
    // A non-numeric char ends the number. Valid only if we consumed ≥1 digit.
    if have_int_digits {
        M::Done(&s[i..])
    } else {
        M::No
    }
}

/// Match a JSON string. `allowed` (if set) restricts the *whole* string to one of
/// the given literals (enum/const) — matched against their raw JSON encodings.
fn match_string<'a>(s: &'a str, allowed: Option<&[String]>) -> M<'a> {
    if let Some(lits) = allowed {
        let mut partial = false;
        for lit in lits {
            let raw = serde_json::to_string(lit).unwrap_or_default(); // e.g. "value"
            if let Some(rest) = s.strip_prefix(&raw) {
                return M::Done(rest);
            }
            if raw.starts_with(s) {
                partial = true;
            }
        }
        return if partial { M::More } else { M::No };
    }

    // Free string: opening quote, escaped contents, closing quote.
    let bytes = s.as_bytes();
    if bytes[0] != b'"' {
        return M::No;
    }
    let mut i = 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return M::Done(&s[i + 1..]),
            b'\\' => {
                let Some(&esc) = bytes.get(i + 1) else {
                    return M::More; // backslash at end — escape unfinished
                };
                match esc {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => i += 2,
                    b'u' => {
                        // \uXXXX — need four hex digits.
                        let hex = &bytes[i + 2..bytes.len().min(i + 6)];
                        if hex.iter().any(|c| !c.is_ascii_hexdigit()) {
                            if hex.len() < 4 {
                                return M::More; // hex digits still arriving
                            }
                            return M::No;
                        }
                        if hex.len() < 4 {
                            return M::More;
                        }
                        i += 6;
                    }
                    _ => return M::No, // invalid escape
                }
            }
            // Raw control chars are illegal in JSON strings.
            c if c < 0x20 => return M::No,
            _ => i += 1,
        }
    }
    M::More // ran out before the closing quote
}

/// Match a JSON array of `items`.
fn match_array<'a>(items: &Schema, s: &'a str) -> M<'a> {
    let bytes = s.as_bytes();
    if bytes[0] != b'[' {
        return M::No;
    }
    let mut rest = &s[1..];
    loop {
        let r = json_ws(rest);
        if r.is_empty() {
            return M::More;
        }
        if let Some(after) = r.strip_prefix(']') {
            return M::Done(after);
        }
        match match_value(items, r) {
            M::Done(after) => {
                let after = json_ws(after);
                if let Some(next) = after.strip_prefix(',') {
                    rest = next;
                } else if let Some(after) = after.strip_prefix(']') {
                    return M::Done(after);
                } else if after.is_empty() {
                    return M::More;
                } else {
                    return M::No;
                }
            }
            M::More => return M::More,
            M::No => return M::No,
        }
    }
}

/// Match a JSON object against declared `props` (keys restricted to them) and
/// `required` (all must be present before the closing brace).
fn match_object<'a>(props: &[(String, Schema)], required: &[String], s: &'a str) -> M<'a> {
    let bytes = s.as_bytes();
    if bytes[0] != b'{' {
        return M::No;
    }
    let mut rest = &s[1..];
    let mut seen: Vec<&str> = Vec::new();
    loop {
        let r = json_ws(rest);
        if r.is_empty() {
            return M::More;
        }
        if let Some(after) = r.strip_prefix('}') {
            // Can only close once every required key is present.
            if required.iter().all(|req| seen.iter().any(|k| k == req)) {
                return M::Done(after);
            }
            return M::No;
        }
        // Expect a key: one of the not-yet-seen declared property names.
        let remaining: Vec<&(String, Schema)> = props
            .iter()
            .filter(|(name, _)| !seen.iter().any(|k| k == name))
            .collect();
        let key_lits: Vec<String> = remaining.iter().map(|(n, _)| n.clone()).collect();
        let (key, after_key) = match match_string(r, Some(&key_lits)) {
            M::Done(after) => {
                // Recover which key matched by re-checking the raw prefixes.
                let matched = remaining.iter().find(|(n, _)| {
                    let raw = serde_json::to_string(n).unwrap_or_default();
                    r.strip_prefix(&raw)
                        .map(|rr| rr.len() == after.len())
                        .unwrap_or(false)
                });
                match matched {
                    Some((n, sch)) => (Some((n.as_str(), sch)), after),
                    None => return M::No,
                }
            }
            M::More => return M::More,
            M::No => return M::No,
        };
        let (kname, kschema) = key.expect("matched key");

        let after = json_ws(after_key);
        let after = match after.strip_prefix(':') {
            Some(a) => a,
            None if after.is_empty() => return M::More,
            None => return M::No,
        };

        match match_value(kschema, after) {
            M::Done(av) => {
                seen.push(kname);
                let av = json_ws(av);
                if let Some(next) = av.strip_prefix(',') {
                    rest = next;
                } else if av.starts_with('}') {
                    rest = av; // loop handles the close (checks required)
                } else if av.is_empty() {
                    return M::More;
                } else {
                    return M::No;
                }
            }
            M::More => return M::More,
            M::No => return M::No,
        }
    }
}

/// Match any well-formed JSON value (structure only — used for `Schema::Any` and
/// unknown subschemas).
fn match_any(s: &str) -> M<'_> {
    let s = json_ws(s);
    if s.is_empty() {
        return M::More;
    }
    match s.as_bytes()[0] {
        b'"' => match_string(s, None),
        b'{' => match_any_object(s),
        b'[' => match_array(&Schema::Any, s),
        b't' | b'f' => match_keyword_set(s, &["true", "false"]),
        b'n' => match_keyword_set(s, &["null"]),
        b'-' | b'0'..=b'9' => match_number(s, true),
        _ => M::No,
    }
}

/// Match a generic object (any string keys, any values).
fn match_any_object(s: &str) -> M<'_> {
    let mut rest = &s[1..];
    loop {
        let r = json_ws(rest);
        if r.is_empty() {
            return M::More;
        }
        if let Some(after) = r.strip_prefix('}') {
            return M::Done(after);
        }
        // key (any string)
        let after_key = match match_string(r, None) {
            M::Done(a) => a,
            M::More => return M::More,
            M::No => return M::No,
        };
        let after = json_ws(after_key);
        let after = match after.strip_prefix(':') {
            Some(a) => a,
            None if after.is_empty() => return M::More,
            None => return M::No,
        };
        match match_any(after) {
            M::Done(av) => {
                let av = json_ws(av);
                if let Some(next) = av.strip_prefix(',') {
                    rest = next;
                } else if av.starts_with('}') {
                    rest = av;
                } else if av.is_empty() {
                    return M::More;
                } else {
                    return M::No;
                }
            }
            M::More => return M::More,
            M::No => return M::No,
        }
    }
}

/// Build the Hermes tool-call envelope schema:
/// `{"name": <enum of tool names>, "arguments": <args schema>}`. When the chosen
/// tool is not yet known, pass `args = Schema::Any` and rebuild once `name` is read.
pub fn envelope(tool_names: &[String], args: Schema) -> Schema {
    Schema::Object {
        props: vec![
            (
                "name".to_string(),
                Schema::Str {
                    allowed: Some(tool_names.to_vec()),
                },
            ),
            ("arguments".to_string(), args),
        ],
        required: vec!["name".to_string(), "arguments".to_string()],
    }
}

// ─── XML tool format (Qwen3.6 / Qwen-Coder) ──────────────────────────────────
//
// Qwen3.6 emits tool calls as XML, not JSON:
//
//   <function=NAME>
//   <parameter=KEY>
//   VALUE
//   </parameter>
//   ...
//   </function>
//
// The body fed to the matcher starts at `<function=`. Structure (`<function=`,
// `<parameter=`, `</parameter>`, `</function>`) is fixed; NAME ∈ tool names, KEY ∈ that
// tool's properties (no dupes, all required before `</function>`), and an `enum`-typed
// VALUE is restricted to its literals. Non-enum values are free text up to `</parameter>`
// (strings are unconstrained anyway; typed scalars are a follow-up).

/// A compiled constraint for whichever tool-call envelope the model emits. The decode loop
/// picks the variant from the first non-space char after `<tool_call>` (`{` → JSON Hermes,
/// `<` → XML) and re-validates the whole body suffix each step.
#[derive(Debug, Clone)]
pub enum Constraint {
    /// JSON Hermes envelope (`{"name":…,"arguments":…}`) — possibly with `arguments`
    /// resolved to the chosen tool's schema.
    Json(Schema),
    /// XML `<function=…>` form, carrying every tool's `(name, args-schema)`.
    Xml(Vec<(String, Schema)>),
}

impl Constraint {
    pub fn prefix(&self, s: &str) -> Prefix {
        match self {
            Constraint::Json(schema) => schema.prefix(s),
            Constraint::Xml(tools) => xml_prefix(tools, s),
        }
    }
    pub fn is_complete(&self, s: &str) -> bool {
        self.prefix(s) == Prefix::Complete
    }
}

/// Classify a partial XML tool-call body (`<function=…>…`) against the tool set.
pub fn xml_prefix(tools: &[(String, Schema)], s: &str) -> Prefix {
    match match_xml(tools, s) {
        XM::Done(rest) => {
            if json_ws(rest).is_empty() {
                Prefix::Complete
            } else {
                Prefix::Invalid
            }
        }
        XM::More => Prefix::Partial,
        XM::No => Prefix::Invalid,
    }
}

enum XM<'a> {
    Done(&'a str),
    More,
    No,
}

/// Outcome of matching a fixed literal at the start of `s`.
enum Lit<'a> {
    Take(&'a str),
    Part,
    No,
}

fn lit<'a>(s: &'a str, kw: &str) -> Lit<'a> {
    if let Some(r) = s.strip_prefix(kw) {
        Lit::Take(r)
    } else if kw.starts_with(s) {
        Lit::Part // `s` is a proper prefix of the literal
    } else {
        Lit::No
    }
}

/// Match a `NAME>` token against an allowed set: returns the matched index + the text after
/// `>`. Names contain no `>`.
enum NameM<'a> {
    Done(usize, &'a str),
    More,
    No,
}

fn match_name<'a>(s: &'a str, allowed: &[&str]) -> NameM<'a> {
    if let Some(gt) = s.find('>') {
        let name = &s[..gt];
        match allowed.iter().position(|n| *n == name) {
            Some(i) => NameM::Done(i, &s[gt + 1..]),
            None => NameM::No,
        }
    } else if allowed.iter().any(|n| n.starts_with(s)) {
        NameM::More
    } else {
        NameM::No
    }
}

fn match_xml<'a>(tools: &[(String, Schema)], s: &'a str) -> XM<'a> {
    let s = json_ws(s);
    let after_fn = match lit(s, "<function=") {
        Lit::Take(r) => r,
        Lit::Part => return XM::More,
        Lit::No => return XM::No,
    };
    let names: Vec<&str> = tools.iter().map(|(n, _)| n.as_str()).collect();
    let (tidx, mut rest) = match match_name(after_fn, &names) {
        NameM::Done(i, r) => (i, r),
        NameM::More => return XM::More,
        NameM::No => return XM::No,
    };
    // The tool's args must be an object (the only shape the parameter form can carry).
    let (props, required): (&[(String, Schema)], &[String]) = match &tools[tidx].1 {
        Schema::Object { props, required } => (props, required),
        _ => (&[], &[]),
    };
    let mut seen: Vec<&str> = Vec::new();
    loop {
        let r = json_ws(rest);
        // Close: `</function>` (only once every required parameter is present).
        match lit(r, "</function>") {
            Lit::Take(after) => {
                if required.iter().all(|req| seen.iter().any(|k| k == req)) {
                    return XM::Done(after);
                }
                return XM::No;
            }
            Lit::Part => return XM::More,
            Lit::No => {}
        }
        // Otherwise a parameter: `<parameter=KEY>`.
        let after_open = match lit(r, "<parameter=") {
            Lit::Take(r) => r,
            Lit::Part => return XM::More,
            Lit::No => return XM::No,
        };
        let remaining: Vec<&str> = props
            .iter()
            .filter(|(n, _)| !seen.iter().any(|k| *k == n))
            .map(|(n, _)| n.as_str())
            .collect();
        let (key, after_key) = match match_name(after_open, &remaining) {
            NameM::Done(i, r) => (remaining[i], r),
            NameM::More => return XM::More,
            NameM::No => return XM::No,
        };
        let kschema = &props.iter().find(|(n, _)| n == key).expect("matched key").1;
        rest = match match_xml_value(kschema, after_key) {
            Lit::Take(r) => r,
            Lit::Part => return XM::More,
            Lit::No => return XM::No,
        };
        seen.push(key);
    }
}

/// Match a parameter VALUE and its `</parameter>` close. An `enum` value is restricted to
/// its literals (raw, unquoted); anything else is free text up to `</parameter>`.
fn match_xml_value<'a>(schema: &Schema, s: &'a str) -> Lit<'a> {
    if let Schema::Str { allowed: Some(lits) } = schema {
        let v = json_ws(s);
        let mut part = false;
        for litv in lits {
            if let Some(r) = v.strip_prefix(litv.as_str()) {
                // Literal matched; expect ws then `</parameter>`.
                let r2 = json_ws(r);
                match lit(r2, "</parameter>") {
                    Lit::Take(a) => return Lit::Take(a),
                    Lit::Part => return Lit::Part,
                    Lit::No => {
                        if r2.is_empty() {
                            return Lit::Part;
                        }
                        // this literal doesn't lead to a close — try the others
                    }
                }
            }
            if litv.starts_with(v) {
                part = true;
            }
        }
        if v.is_empty() || part {
            Lit::Part
        } else {
            Lit::No
        }
    } else {
        // Free value: consume up to the closing tag (unconstrained content).
        match s.find("</parameter>") {
            Some(end) => Lit::Take(&s[end + "</parameter>".len()..]),
            None => Lit::Part,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sc(v: serde_json::Value) -> Schema {
        Schema::parse(&v)
    }

    #[test]
    fn scalars_prefix_and_complete() {
        let b = Schema::Bool;
        assert_eq!(b.prefix(""), Prefix::Partial);
        assert_eq!(b.prefix("t"), Prefix::Partial);
        assert_eq!(b.prefix("tru"), Prefix::Partial);
        assert_eq!(b.prefix("true"), Prefix::Complete);
        assert_eq!(b.prefix("false"), Prefix::Complete);
        assert_eq!(b.prefix("x"), Prefix::Invalid);
        assert_eq!(b.prefix("truex"), Prefix::Invalid);

        let i = Schema::Integer;
        assert_eq!(i.prefix("-"), Prefix::Partial);
        assert_eq!(i.prefix("12"), Prefix::Partial); // could continue
        assert_eq!(i.prefix("12 "), Prefix::Complete);
        assert_eq!(i.prefix("abc"), Prefix::Invalid);

        let n = Schema::Number;
        assert_eq!(n.prefix("3."), Prefix::Partial);
        assert_eq!(n.prefix("3.14"), Prefix::Partial);
        assert_eq!(n.prefix("3.14 "), Prefix::Complete);
        assert_eq!(n.prefix("1e"), Prefix::Partial);
        assert_eq!(n.prefix("1e9 "), Prefix::Complete);
    }

    #[test]
    fn string_and_enum() {
        let s = Schema::Str { allowed: None };
        assert_eq!(s.prefix("\""), Prefix::Partial);
        assert_eq!(s.prefix("\"hi"), Prefix::Partial);
        assert_eq!(s.prefix("\"hi\""), Prefix::Complete);
        assert_eq!(s.prefix("\"a\\\"b\""), Prefix::Complete); // escaped quote
        assert_eq!(s.prefix("nope"), Prefix::Invalid);

        let e = sc(json!({"enum": ["celsius", "fahrenheit"]}));
        assert_eq!(e.prefix("\"c"), Prefix::Partial);
        assert_eq!(e.prefix("\"celsius\""), Prefix::Complete);
        assert_eq!(e.prefix("\"kelvin\""), Prefix::Invalid); // not in enum
        assert_eq!(e.prefix("\"f"), Prefix::Partial);
    }

    #[test]
    fn object_keys_required_and_types() {
        let o = sc(json!({
            "type": "object",
            "properties": {
                "city": {"type": "string"},
                "unit": {"enum": ["c", "f"]}
            },
            "required": ["city"]
        }));
        // Building up a valid object.
        assert_eq!(o.prefix("{"), Prefix::Partial);
        assert_eq!(o.prefix("{\"ci"), Prefix::Partial);
        assert_eq!(o.prefix("{\"city\": \"Paris\""), Prefix::Partial);
        assert_eq!(o.prefix("{\"city\": \"Paris\"}"), Prefix::Complete);
        assert_eq!(
            o.prefix("{\"city\": \"Paris\", \"unit\": \"c\"}"),
            Prefix::Complete
        );
        // Cannot close without the required key.
        assert_eq!(o.prefix("{}"), Prefix::Invalid);
        assert_eq!(o.prefix("{\"unit\": \"c\"}"), Prefix::Invalid);
        // Hallucinated key is rejected.
        assert_eq!(o.prefix("{\"weather"), Prefix::Invalid);
        // Wrong enum value is rejected.
        assert_eq!(o.prefix("{\"city\": \"Paris\", \"unit\": \"x"), Prefix::Invalid);
    }

    #[test]
    fn array_of_scalars() {
        let a = sc(json!({"type": "array", "items": {"type": "integer"}}));
        assert_eq!(a.prefix("["), Prefix::Partial);
        assert_eq!(a.prefix("[1"), Prefix::Partial);
        assert_eq!(a.prefix("[1,"), Prefix::Partial);
        assert_eq!(a.prefix("[1, 2, 3]"), Prefix::Complete);
        assert_eq!(a.prefix("[]"), Prefix::Complete);
        assert_eq!(a.prefix("[\"x\"]"), Prefix::Invalid); // string in an int array
    }

    #[test]
    fn unknown_schema_relaxes_to_any_json() {
        // No usable type info → Any → accept well-formed JSON of any shape.
        let any = sc(json!({"description": "freeform"}));
        assert_eq!(any.prefix("{\"a\": [1, {\"b\": true}]}"), Prefix::Complete);
        assert_eq!(any.prefix("{\"a\":"), Prefix::Partial);
        assert_eq!(any.prefix("{\"a\": tru"), Prefix::Partial);
        assert_eq!(any.prefix("nul"), Prefix::Partial);
        assert_eq!(any.prefix("null"), Prefix::Complete);
        assert_eq!(any.prefix("}"), Prefix::Invalid);
    }

    #[test]
    fn envelope_name_then_arguments() {
        let names = vec!["get_weather".to_string()];
        let args = sc(json!({
            "type": "object",
            "properties": {"city": {"type": "string"}},
            "required": ["city"]
        }));
        let env = envelope(&names, args);
        assert_eq!(env.prefix("{\"name\": \"get_weather\""), Prefix::Partial);
        assert_eq!(
            env.prefix("{\"name\": \"get_weather\", \"arguments\": {\"city\": \"Paris\"}}"),
            Prefix::Complete
        );
        // Wrong tool name can't be produced.
        assert_eq!(env.prefix("{\"name\": \"get_wea_x"), Prefix::Invalid);

        // Before the name is known, arguments=Any still accepts the structure.
        let open = envelope(&names, Schema::Any);
        assert_eq!(
            open.prefix("{\"name\": \"get_weather\", \"arguments\": {\"any"),
            Prefix::Partial
        );
    }

    fn weather_tools() -> Vec<(String, Schema)> {
        vec![(
            "get_weather".to_string(),
            sc(json!({
                "type": "object",
                "properties": {
                    "location": {"type": "string"},
                    "unit": {"enum": ["kelvin", "rankine"]}
                },
                "required": ["location", "unit"]
            })),
        )]
    }

    #[test]
    fn xml_tool_structure_and_enum() {
        let tools = weather_tools();
        let p = |s: &str| xml_prefix(&tools, s);
        // Building the skeleton up.
        assert_eq!(p("<function="), Prefix::Partial);
        assert_eq!(p("<function=get_weather>"), Prefix::Partial);
        assert_eq!(p("<function=get_weather>\n<parameter=location>\n"), Prefix::Partial);
        assert_eq!(
            p("<function=get_weather>\n<parameter=location>\nParis\n</parameter>\n"),
            Prefix::Partial
        );
        // Complete call.
        let full = "<function=get_weather>\n<parameter=location>\nParis\n</parameter>\n\
                    <parameter=unit>\nkelvin\n</parameter>\n</function>";
        assert_eq!(p(full), Prefix::Complete);
        // Wrong function name can't be produced.
        assert_eq!(p("<function=get_wea_x"), Prefix::Invalid);
        // Hallucinated parameter key rejected.
        assert_eq!(p("<function=get_weather>\n<parameter=zip"), Prefix::Invalid);
        // Cannot close before a required parameter is present.
        assert_eq!(
            p("<function=get_weather>\n<parameter=location>\nParis\n</parameter>\n</function>"),
            Prefix::Invalid
        );
    }

    #[test]
    fn xml_enum_value_is_masked() {
        let tools = weather_tools();
        let p = |s: &str| xml_prefix(&tools, s);
        let head = "<function=get_weather>\n<parameter=location>\nParis\n</parameter>\n\
                    <parameter=unit>\n";
        // A legal enum value (prefix) is accepted.
        assert_eq!(p(&format!("{head}kel")), Prefix::Partial);
        assert_eq!(p(&format!("{head}rankine\n</parameter>\n</function>")), Prefix::Complete);
        // The model's *preferred* (but illegal) value is rejected — the mask bites.
        assert_eq!(p(&format!("{head}celsius")), Prefix::Invalid);
        assert_eq!(p(&format!("{head}c")), Prefix::Invalid);
    }

    #[test]
    fn constraint_dispatch_json_vs_xml() {
        let tools = weather_tools();
        let json = Constraint::Json(envelope(
            &["get_weather".to_string()],
            tools[0].1.clone(),
        ));
        let xml = Constraint::Xml(tools);
        assert!(json.is_complete(
            "{\"name\":\"get_weather\",\"arguments\":{\"location\":\"Paris\",\"unit\":\"kelvin\"}}"
        ));
        assert!(xml.is_complete(
            "<function=get_weather>\n<parameter=location>\nParis\n</parameter>\n\
             <parameter=unit>\nkelvin\n</parameter>\n</function>"
        ));
    }
}
