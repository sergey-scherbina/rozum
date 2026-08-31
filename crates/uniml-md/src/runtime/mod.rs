//! ScalaScript runtime helpers (rust target).
//! Emitted verbatim by RustGen; do not edit by hand.

use crate::value::Value;
use std::fmt::Display;

#[allow(dead_code)]
pub fn _show(v: &Value) -> String {
    v.show()
}

#[allow(dead_code)]
pub fn _print<T: Display>(s: T) {
    use std::io::Write;
    print!("{}", s);
    let _ = std::io::stdout().flush();
}

#[allow(dead_code)]
pub fn _println<T: Display>(s: T) {
    println!("{}", s);
}

// ── R.3.1 — time + filesystem intrinsics (no extra crate deps) ──

/// `nowMillis` — current Unix time in milliseconds, signed i64.
/// Mirrors the JVM target's `java.lang.System.currentTimeMillis`.
#[allow(dead_code)]
pub fn _now_millis() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// `readFile(path)` — read a UTF-8 file to a String.  Panics on
/// I/O error to match the interpreter's fail-fast contract.
/// Takes the path by reference so the caller keeps ownership.
#[allow(dead_code)]
pub fn _read_file(path: &str) -> String {
    std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("readFile({}): {}", path, e))
}

/// `writeFile(path, contents)` — overwrite a file's bytes with
/// the given UTF-8 string.  Takes both args by reference so the
/// caller keeps ownership of its variables.
#[allow(dead_code)]
pub fn _write_file(path: &str, contents: &str) {
    std::fs::write(path, contents)
        .unwrap_or_else(|e| panic!("writeFile({}): {}", path, e))
}

// ── R.3.4 — process & env intrinsics (no extra crate deps) ──

/// `args()` — command-line arguments after the binary name.
/// Returns an empty Vec when invoked with no args.
#[allow(dead_code)]
pub fn _args() -> Vec<String> {
    std::env::args().skip(1).collect()
}

/// `env(key)` — value of an environment variable as `Option[String]`.
/// Returns `None` when the variable is unset or contains invalid UTF-8.
#[allow(dead_code)]
pub fn _env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `envOrElse(key, default)` — env var value or a fallback string.
#[allow(dead_code)]
pub fn _env_or_else(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// `exit(code)` — terminate the process immediately with the
/// given exit code.  Wraps `i64` from the SS surface to `i32`
/// for `std::process::exit`.
#[allow(dead_code)]
pub fn _exit(code: i64) -> ! {
    std::process::exit(code as i32)
}

// ── std.fs — full filesystem API (pure std::fs, no extra crates) ──

#[allow(dead_code)]
pub fn _append_file(path: &str, contents: &str) {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().create(true).append(true)
        .open(path)
        .unwrap_or_else(|e| panic!("appendFile({}): {}", path, e));
    f.write_all(contents.as_bytes())
        .unwrap_or_else(|e| panic!("appendFile({}): {}", path, e));
}

#[allow(dead_code)]
pub fn _read_bytes(path: &str) -> Vec<i64> {
    std::fs::read(path)
        .unwrap_or_else(|e| panic!("readBytes({}): {}", path, e))
        .into_iter().map(|b| b as i64).collect()
}

#[allow(dead_code)]
pub fn _write_bytes(path: &str, bytes: Vec<i64>) {
    let data: Vec<u8> = bytes.into_iter().map(|b| b as u8).collect();
    std::fs::write(path, data)
        .unwrap_or_else(|e| panic!("writeBytes({}): {}", path, e));
}

// `s.charAt(i)` returns an INT, not a char — the kernel is
// `case (StrV(s), "charAt", List(IntV(i))) => IntV(s.charAt(i.toInt).toLong)`, i.e. a UTF-16
// CODE UNIT. std/json-core relies on exactly that: it compares the result against 92 and 117
// (`\` and `u`) and stores whole strings as `List[Int]`. Indexing Rust's `chars()` instead
// would agree on ASCII and disagree on every astral character, which is the kind of
// difference that shows up as one wrong emoji in production and nowhere in a test.
// `toInt` where the receiver's type did not survive to the emitter — a lambda parameter, a
// field read. `s as i32` is not a cast Rust has for a String (E0605), and that is what such a
// receiver used to get. Implemented for every type `toInt` is legal on in ScalaScript, so the
// generator can emit it knowing nothing about the receiver.
//
// The i64 arm truncates through i32 and widens back, preserving the Int wraparound the direct
// lowering has: this helper must not quietly mean something different from `x as i32 as i64`.
#[allow(dead_code)]
// A NUMERIC CONVERSION THAT FAILS STOPS THE PROGRAM, on this lane as on the others. Both
// parses used to be `unwrap_or(0)` / `unwrap_or(0.0)`, so `"abc".toInt` was 0 here and fatal
// on `run` — the same source, the same input, two answers, no diagnostic. Reported from rozum
// (INBOX `toint-on-a-non-integer-diverges`), where the natural round-trip check
// `s.toInt.toString == s` worked on this lane and killed the program on the other.
//
// `run` is the oracle and it throws, matching Scala, so the quiet lane is the one that moved.
// The message names the operation AND the input, which the run lane's does not — a panic
// naming neither is how a 0 becomes a bug report three days later.
//
// THIS LEAVES NO TOTAL FORM ON THIS LANE, and that is filed rather than papered over:
// `toIntOption` exists on the run lane and is not lowered for Rust, so a program that wants
// to ASK whether a string is a number has nothing here yet.
#[allow(dead_code)]
fn ssc_parse_int(s: &str) -> i64 {
    s.trim().parse::<i64>()
        .unwrap_or_else(|_| panic!("String.toInt: invalid integer: {:?}", s))
}
#[allow(dead_code)]
fn ssc_parse_double(s: &str) -> f64 {
    s.trim().parse::<f64>()
        .unwrap_or_else(|_| panic!("String.toDouble: invalid number: {:?}", s))
}
pub trait SscToInt { fn ssc_to_int(self) -> i64; }
impl SscToInt for i64    { fn ssc_to_int(self) -> i64 { self as i32 as i64 } }
impl SscToInt for f64    { fn ssc_to_int(self) -> i64 { self as i32 as i64 } }
impl SscToInt for String { fn ssc_to_int(self) -> i64 { ssc_parse_int(&self) } }
impl SscToInt for &str   { fn ssc_to_int(self) -> i64 { ssc_parse_int(self) } }
// Reference arms so the CALLER can borrow. `_to_int(s)` took the String, and a body that read
// `s` again was E0382 — `s.toInt.toString == s` did not compile while the same check through
// two local copies did. Reported from rozum. Borrowing costs nothing and a clone would have
// allocated on every conversion.
impl SscToInt for &String { fn ssc_to_int(self) -> i64 { ssc_parse_int(self) } }
impl SscToInt for &i64    { fn ssc_to_int(self) -> i64 { *self as i32 as i64 } }
impl SscToInt for &f64    { fn ssc_to_int(self) -> i64 { *self as i32 as i64 } }
// A CHARACTER converts to its code point — `'a'.toInt` is 97 and `'9'.toInt` is 57, not 9.
// That is what `run` answers and what Scala means, so the two lanes now agree instead of one
// of them refusing to build. Both spellings are needed: `charAt` yields `SscChar` while
// `toList` yields plain `char`, and a program that mixes them hit whichever arm was missing.
// Reported from rozum, where `s.trim.toList.map(c => c.toInt).sum` — a colour hash over a
// name — made a whole PWA unbuildable on the Rust lane.
impl SscToInt for char     { fn ssc_to_int(self) -> i64 { self as u32 as i64 } }
impl SscToInt for &char    { fn ssc_to_int(self) -> i64 { *self as u32 as i64 } }
impl SscToInt for SscChar  { fn ssc_to_int(self) -> i64 { self.0 } }
impl SscToInt for &SscChar { fn ssc_to_int(self) -> i64 { self.0 } }
#[allow(dead_code)]
pub fn _to_int<T: SscToInt>(x: T) -> i64 { x.ssc_to_int() }

// `toDouble` / `toFloat`, the twin of the above and for the same reason: `s as f64` is not a
// cast Rust has for a String either (E0605), and that is what EVERY receiver got here — the
// String arm was never written, only the numeric one. Reported from rozum, whose CSV field
// test reached for `toDouble` and could not use it.
#[allow(dead_code)]
pub trait SscToDouble { fn ssc_to_double(self) -> f64; }
impl SscToDouble for f64    { fn ssc_to_double(self) -> f64 { self } }
impl SscToDouble for i64    { fn ssc_to_double(self) -> f64 { self as f64 } }
impl SscToDouble for String { fn ssc_to_double(self) -> f64 { ssc_parse_double(&self) } }
impl SscToDouble for &str   { fn ssc_to_double(self) -> f64 { ssc_parse_double(self) } }
impl SscToDouble for &String { fn ssc_to_double(self) -> f64 { ssc_parse_double(self) } }
impl SscToDouble for &i64    { fn ssc_to_double(self) -> f64 { *self as f64 } }
impl SscToDouble for &f64    { fn ssc_to_double(self) -> f64 { *self } }
#[allow(dead_code)]
pub fn _to_double<T: SscToDouble>(x: T) -> f64 { x.ssc_to_double() }


// A Char on the Rust lane. Rust has no inheritance, so the property `CharV extends IntV` gives
// the v2 kernel for free is spelled out here: Display prints the CHARACTER, and every numeric
// use — comparison against a code unit, arithmetic, coercion into an `Int` slot — degrades to
// i64 exactly as Scala's Char does. `charAt` returning a bare i64 printed `97` where every
// other lane printed `a`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SscChar(pub i64);
impl std::fmt::Display for SscChar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match char::from_u32(self.0 as u32) {
            Some(c) => write!(f, "{}", c),
            // A lone surrogate is not a `char`; printing the code unit is the honest fallback
            // and cannot be mistaken for a character.
            None    => write!(f, "{}", self.0),
        }
    }
}
impl PartialEq<i64> for SscChar { fn eq(&self, o: &i64) -> bool { self.0 == *o } }
impl PartialEq<SscChar> for i64 { fn eq(&self, o: &SscChar) -> bool { *self == o.0 } }
impl PartialOrd<i64> for SscChar { fn partial_cmp(&self, o: &i64) -> Option<std::cmp::Ordering> { self.0.partial_cmp(o) } }
impl std::ops::Add<i64> for SscChar { type Output = i64; fn add(self, o: i64) -> i64 { self.0 + o } }
impl std::ops::Sub<i64> for SscChar { type Output = i64; fn sub(self, o: i64) -> i64 { self.0 - o } }
impl From<SscChar> for i64 { fn from(c: SscChar) -> i64 { c.0 } }
impl From<SscChar> for crate::value::Value {
    fn from(c: SscChar) -> crate::value::Value { crate::value::Value::Str(c.to_string()) }
}
impl crate::value::SscInt for SscChar { fn ssc_int(self) -> i64 { self.0 } }

// `s.length` — UTF-16 CODE UNITS, the same basis `_str_char_at` and `_str_substring` below
// index in, and the same number both reference lanes answer. Rust's own `String::len()` is
// BYTES, and that is what this lane emitted for every receiver: `"aé·b".length` was 6 here and
// 4 on `run`, so the obvious `s.substring(i, s.length)` panicked on any non-ASCII string —
// reported from rozum, where one accented character in a served page killed the tokio worker
// and every later request answered `PoisonError`. An ASCII test page hides it completely.
//
// Code units rather than characters, measured rather than assumed: `"a😀b".length` is 4 on
// both reference lanes, not 3. `chars().count()` is the intuitive fix and would have put this
// lane wrong again on exactly the inputs nobody tests.
// ASCII FAST PATH, and it is a COMPLEXITY problem rather than a micro-optimisation.
// For an all-ASCII string a UTF-8 byte index IS the UTF-16 code-unit index, so both answers
// come straight out of the byte slice. `<[u8]>::is_ascii` is vectorised; `encode_utf16()`
// walks the string one `char` at a time through iterator machinery, so the general path
// below costs orders of magnitude more per byte examined.
//
// Why it matters: a scanner written the ordinary way — `while i < s.length do s.charAt(i)` —
// makes BOTH of these O(n) per iteration (the length recomputed in the loop CONDITION, the
// index walked from the start), i.e. O(n²) over the string, entirely inside the runtime.
// Measured on `uniml/markdown` parsing plain paragraphs: `codePointCount` + `_str_code_at`
// were 40% of a profile whose whole shape was quadratic (2 KB 0.014 s → 256 KB 173 s).
// This keeps the quadratic SHAPE (see `rag-uniml-parser-quadratic` in rozum's backlog for
// the asymptotic fix, which needs a representation decision) and removes the constant that
// made it hurt at kilobyte scale.
#[allow(dead_code)]
pub fn _str_length(s: &str) -> i64 {
    if s.is_ascii() { s.len() as i64 } else { s.encode_utf16().count() as i64 }
}

#[allow(dead_code)]
pub fn _str_char_at(s: &str, i: i64) -> SscChar { SscChar(_str_code_at(s, i)) }

#[allow(dead_code)]
pub fn _str_code_at(s: &str, i: i64) -> i64 {
    // Only the PREFIX has to be ASCII for byte index `i` to be code-unit index `i`: a
    // multi-byte character anywhere at or before `i` shifts the mapping, one after it
    // cannot. Checking the prefix rather than the whole string also keeps the fast path
    // available for a mostly-ASCII document with one accented word near the end.
    let b = s.as_bytes();
    let idx = i as usize;
    if i >= 0 && idx < b.len() && b[..=idx].is_ascii() {
        return b[idx] as i64;
    }
    s.encode_utf16()
        .nth(i as usize)
        .unwrap_or_else(|| panic!("charAt({}): index out of range for a string of {} code units", i, s.encode_utf16().count()))
        as i64
}

// `substring(from)` / `substring(from, until)` — same code-unit basis, and same panic as the
// JVM's StringIndexOutOfBounds rather than a silent clamp: a wrong slice that keeps running
// is how a parser produces plausible garbage.
// ASCII FAST PATH, and it is the single hottest thing in this runtime. When the prefix
// through `until` is ASCII, a byte index IS the code-unit index, so the answer is a direct
// byte slice: no allocation, no walk of the whole string.
//
// The general path below materialises a `Vec<u16>` of the ENTIRE string on EVERY call.
// `uniml/markdown`'s line splitter calls this once PER CHARACTER (`text.substring(i, i+1)`
// to take one char), so that is an O(n) allocation per character — O(n²) with the allocator
// in the inner loop. Measured: `split` was 90% of a whole markdown parse profile, and the
// parse curve was ~3.6× per doubling.
#[allow(dead_code)]
pub fn _str_substring(s: &str, from: i64, until: i64) -> String {
    if from >= 0 && until >= from {
        let bs = s.as_bytes();
        let (a, z) = (from as usize, until as usize);
        // Only the prefix through `until` need be ASCII — a multi-byte character AFTER the
        // slice cannot shift the mapping of indices inside it.
        if z <= bs.len() && bs[..z].is_ascii() {
            return s[a..z].to_string();
        }
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let (a, b) = (from as usize, until as usize);
    if a > b || b > units.len() {
        panic!("substring({}, {}): out of range for a string of {} code units", from, until, units.len());
    }
    String::from_utf16_lossy(&units[a..b])
}

#[allow(dead_code)]
pub fn _str_substring_from(s: &str, from: i64) -> String {
    // Same fast path: an all-ASCII string has byte length == code-unit length, so the tail
    // is a byte slice. Avoids collecting the whole string just to learn its length.
    if from >= 0 && s.is_ascii() {
        let a = from as usize;
        if a <= s.len() {
            return s[a..].to_string();
        }
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    _str_substring(s, from, units.len() as i64)
}

// `s.startsWith(prefix, toffset)` — the two-arg overload (`src.startsWith("<?xml", pos)`,
// `uniml/xml`'s `Doc.scala`'s hand-written XML scanner, checked from the CURRENT scan
// position throughout). Same UTF-16-code-unit basis as `_str_char_at`/`_str_substring` above
// — a byte-offset `&s[at..]` slice (the obvious one-liner) panics on any non-ASCII input at
// exactly the position this scanner cares about most, right after a multi-byte character.
// Unlike `_str_substring`, an out-of-range `at` returns `false` rather than panicking: that
// is the JVM's own `String.startsWith(prefix, toffset)` contract (negative or `> length`
// answers false, never throws), and a scanner probing "am I at a marker" one code unit past
// the end is ordinary, not exceptional.
#[allow(dead_code)]
pub fn _str_starts_with_at(s: &str, prefix: &str, at: i64) -> bool {
    if at < 0 { return false; }
    // ASCII fast path (see `_str_substring`): with an ASCII prefix through `at`, code unit
    // `at` sits at byte `at`, which is a char boundary — and comparing UTF-8 bytes from a
    // char boundary is equivalent to comparing the text, whatever `prefix` itself contains.
    // A scanner probing "am I at a marker" calls this once per position.
    {
        let bs = s.as_bytes();
        let start = at as usize;
        if start <= bs.len() && bs[..start].is_ascii() {
            return bs[start..].starts_with(prefix.as_bytes());
        }
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let start = at as usize;
    if start > units.len() { return false; }
    let prefix_units: Vec<u16> = prefix.encode_utf16().collect();
    let end = start + prefix_units.len();
    end <= units.len() && units[start..end] == prefix_units[..]
}

// `s.indexOf(needle, fromIndex)` — the two-arg overload (`input.indexOf("?>", index + 5)`,
// `input.indexOf(';', cursor + 1)`, `uniml/xml`'s `Doc.scala`'s hand-written XML scanner) —
// same UTF-16-code-unit basis as `_str_starts_with_at` above, and the same reason: a byte
// offset diverges from Scala's own answer on any non-ASCII prefix. Slices to the UTF-16
// boundary first (lossy — the same tradeoff `_str_substring` already makes) so the search
// itself can use `str::find` and the surrounding OFFSET stays exact.
#[allow(dead_code)]
pub fn _str_index_of_from(s: &str, needle: &str, from: i64) -> i64 {
    if from < 0 { return -1; }
    // ASCII fast path (see `_str_substring`): an all-ASCII string has byte offsets equal to
    // code-unit offsets throughout, so the search and the returned index need no conversion
    // and no `Vec<u16>` of the whole string.
    if s.is_ascii() {
        let start = (from as usize).min(s.len());
        return match s[start..].find(needle) {
            Some(byte_idx) => (start + byte_idx) as i64,
            None => -1,
        };
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let start = (from as usize).min(units.len());
    let suffix = String::from_utf16_lossy(&units[start..]);
    match suffix.find(needle) {
        Some(byte_idx) => (start + suffix[..byte_idx].encode_utf16().count()) as i64,
        None => -1,
    }
}

// `content.regionMatches(true, i, "www.", 0, 4)` (`uniml/markdown`'s `MarkdownInlines.scala`'s
// `autolinkAtWWW`/`autolinkScheme` — scanning for a URL scheme or `www.` at the current
// position, case-insensitively) — same UTF-16-code-unit basis as `_str_starts_with_at`/
// `_str_index_of_from` above, and the SAME "out of range answers false, never panics"
// contract Java's own `String.regionMatches` has (a negative offset or a region running past
// either string's end is not exceptional here, just a non-match). `ignoreCase`, when set,
// decodes each region back to a `String` and compares lowercased — the ordinary Rust idiom
// for case-insensitive text comparison, and exact enough for the ASCII scheme names
// (`http`/`https`/`mailto`/`www.`) this lane's own callers actually probe for.
#[allow(dead_code)]
pub fn _str_region_matches(s: &str, ignore_case: bool, toffset: i64, other: &str, ooffset: i64, len: i64) -> bool {
    if toffset < 0 || ooffset < 0 || len < 0 { return false; }
    // ASCII fast path (see `_str_substring`): both regions become plain byte slices, with
    // no `Vec<u16>` of either whole string. `autolinkAtWWW`/`autolinkScheme` call this once
    // per scan position.
    {
        let (sb, ob) = (s.as_bytes(), other.as_bytes());
        let (to, oo, l) = (toffset as usize, ooffset as usize, len as usize);
        if to.saturating_add(l) <= sb.len() && oo.saturating_add(l) <= ob.len()
            && sb[..to + l].is_ascii() && ob[..oo + l].is_ascii() {
            let (sa, ob2) = (&sb[to..to + l], &ob[oo..oo + l]);
            return if !ignore_case { sa == ob2 } else { sa.eq_ignore_ascii_case(ob2) };
        }
    }
    let s_units: Vec<u16> = s.encode_utf16().collect();
    let o_units: Vec<u16> = other.encode_utf16().collect();
    let (to, oo, l) = (toffset as usize, ooffset as usize, len as usize);
    if to + l > s_units.len() || oo + l > o_units.len() { return false; }
    if !ignore_case {
        s_units[to..to + l] == o_units[oo..oo + l]
    } else {
        let sa = String::from_utf16_lossy(&s_units[to..to + l]);
        let oa = String::from_utf16_lossy(&o_units[oo..oo + l]);
        sa.to_lowercase() == oa.to_lowercase()
    }
}

// `Character.toLowerCase(c)` (`uniml/markdown`'s `MarkdownLexer.scala`'s `foldCase` — the
// Unicode-data-only, LOCALE-INDEPENDENT fold used for every non-ASCII code unit, as opposed to
// `String.toLowerCase()`'s locale-sensitive one; see that def's own doc comment for why the two
// must stay distinct here). `c` is a UTF-16 code unit (this lane's `SscChar`/`i64` convention),
// so the round trip is `i64 -> char -> lowercase -> i64`; Rust's `char::to_lowercase()` is an
// iterator because a few scalars fold to MULTIPLE chars (German ß-like cases), but every
// caller here treats the result as a single code unit, so only the first is kept — same
// "answer the input unchanged rather than panic" contract as this file's other char/String
// helpers when the input can't be interpreted (an unpaired surrogate never round-trips through
// `char::from_u32`).
#[allow(dead_code)]
pub fn _char_to_lowercase(c: i64) -> i64 {
    match char::from_u32(c as u32) {
        Some(ch) => ch.to_lowercase().next().map(|lc| lc as i64).unwrap_or(c),
        None => c,
    }
}

#[allow(dead_code)]
pub fn _exists(path: &str) -> bool { std::path::Path::new(path).exists() }

#[allow(dead_code)]
pub fn _is_file(path: &str) -> bool { std::path::Path::new(path).is_file() }

#[allow(dead_code)]
pub fn _is_dir(path: &str) -> bool { std::path::Path::new(path).is_dir() }

#[allow(dead_code)]
pub fn _mkdir(path: &str) {
    // No exists-then-create TOCTOU: create directly, tolerate AlreadyExists.
    match std::fs::create_dir(std::path::Path::new(path)) {
        Ok(()) => {}
        Err(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => panic!("mkdir({}): {}", path, e),
    }
}

#[allow(dead_code)]
pub fn _mkdirs(path: &str) {
    std::fs::create_dir_all(path)
        .unwrap_or_else(|e| panic!("mkdirs({}): {}", path, e));
}

#[allow(dead_code)]
pub fn _list_dir(path: &str) -> Vec<String> {
    std::fs::read_dir(path)
        .unwrap_or_else(|e| panic!("listDir({}): {}", path, e))
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

#[allow(dead_code)]
pub fn _delete_file(path: &str) {
    // Single file only — matches JVM/JS/interpreter (which error on a directory).
    // Previously `remove_dir_all` on a dir → divergent + destructive recursive wipe.
    std::fs::remove_file(std::path::Path::new(path))
        .unwrap_or_else(|e| panic!("deleteFile({}): {}", path, e));
}

#[allow(dead_code)]
pub fn _copy_file(src: &str, dst: &str) {
    std::fs::copy(src, dst)
        .unwrap_or_else(|e| panic!("copyFile({}, {}): {}", src, dst, e));
}

#[allow(dead_code)]
pub fn _move_file(src: &str, dst: &str) {
    std::fs::rename(src, dst)
        .unwrap_or_else(|e| panic!("moveFile({}, {}): {}", src, dst, e));
}

// ── std.os — OS environment (pure std::env, std::path) ──

#[allow(dead_code)]
pub fn _cwd() -> String {
    std::env::current_dir()
        .unwrap_or_else(|e| panic!("cwd: {}", e))
        .to_string_lossy().into_owned()
}

#[allow(dead_code)]
pub fn _sep() -> String { std::path::MAIN_SEPARATOR.to_string() }

#[allow(dead_code)]
pub fn _path_join(parts: Vec<String>) -> String {
    parts.iter().fold(std::path::PathBuf::new(), |mut p, s| { p.push(s); p })
        .to_string_lossy().into_owned()
}

#[allow(dead_code)]
pub fn _path_dirname(path: &str) -> String {
    std::path::Path::new(path).parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn _path_basename(path: &str) -> String {
    std::path::Path::new(path).file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn _path_extname(path: &str) -> String {
    std::path::Path::new(path).extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn _path_resolve(path: &str) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| std::path::PathBuf::from(path))
        .to_string_lossy().into_owned()
}

#[allow(dead_code)]
pub fn _path_is_absolute(path: &str) -> bool {
    std::path::Path::new(path).is_absolute()
}

#[allow(dead_code)]
pub fn _temp_dir() -> String {
    std::env::temp_dir().to_string_lossy().into_owned()
}

#[allow(dead_code)]
pub fn _temp_file(prefix: &str, suffix: &str) -> String {
    // O_EXCL create with a high-entropy name (pid+nanos+counter): fails if the
    // path already exists, blocking a pre-planted symlink hijack (CWE-377).
    let dir = std::env::temp_dir();
    let pid = std::process::id();
    for i in 0..1000u64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos()).unwrap_or(0);
        let name = format!("{}{}_{}_{}{}", prefix, pid, nanos, i, suffix);
        let path = dir.join(&name);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return path.to_string_lossy().into_owned(),
            Err(ref e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => panic!("tempFile: {}", e),
        }
    }
    panic!("tempFile: could not create a unique temp file")
}

/// `platform` — always "Native" on the Rust target.
#[allow(dead_code)]
pub fn _platform() -> String { "Native".to_string() }

#[allow(dead_code)]
pub fn _homedir() -> String {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string())
}

#[allow(dead_code)]
pub fn _hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "localhost".to_string())
}

// ── std.process — exec via std::process::Command ──

#[derive(Debug, Clone)]
pub struct ProcessResult {
    pub stdout:   String,
    pub stderr:   String,
    pub exitCode: i64,
}

/// `spawn(cmd, args, opts): Child` — start a child and return its pid WITHOUT waiting.
///
/// Reported from rozum (`process-needs-a-detached-spawn`): `exec` waits by construction, so an
/// HTTP handler starting a five-minute job could hold the connection for five minutes or not
/// start it. Every launch route of their port stopped here.
///
/// RETURNS AN i64, NOT A `Child`. The `Child` struct is GENERATED into the crate from the
/// `case class`, so this template — which is emitted verbatim and knows no user types — cannot
/// name it. The call site wraps the pid, exactly as it wraps every other generated struct.
///
/// THE CHILD OUTLIVES THIS PROCESS, which on Unix is what `Command::spawn` already does: the
/// `Child` handle is DROPPED here without `wait`, and Rust's drop does not kill or reap it.
/// stdio is null rather than inherited so the child holds none of this process's descriptors —
/// a child writing to an inherited pipe blocks once nobody reads it.
///
/// `timeout` is the one option this cannot honour and does not pretend to: the call returns
/// before there is anything to time.
#[allow(dead_code, unused_variables)]
pub fn _spawn_pid<O: Into<crate::value::Value>>(cmd: String, args: Vec<String>, opts: O) -> i64 {
    use std::io::Write;
    let mut command = std::process::Command::new(&cmd);
    command.args(&args);
    let mut stdin_text: Option<String> = None;
    if let crate::value::Value::Obj("ProcessOptions", f) = opts.into() {
        if let Some(crate::value::Value::Obj("Some", inner)) = f.get(0) {
            if let Some(crate::value::Value::Str(dir)) = inner.get(0) { command.current_dir(dir); }
        }
        if let Some(crate::value::Value::Bool(false)) = f.get(3) { command.env_clear(); }
        if let Some(crate::value::Value::Map(entries)) = f.get(1) {
            for (k, v) in entries {
                if let crate::value::Value::Str(s) = v { command.env(k, s); }
            }
        }
        if let Some(crate::value::Value::Obj("Some", inner)) = f.get(4) {
            if let Some(crate::value::Value::Str(text)) = inner.get(0) {
                stdin_text = Some(text.clone());
            }
        }
    }
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());
    command.stdin(if stdin_text.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    let mut child = command.spawn().unwrap_or_else(|e| panic!("spawn({}): {}", cmd, e));
    if let Some(text) = stdin_text {
        // Written and then DROPPED, which is what closes the pipe and gives the child its EOF.
        // It matters more here than in `exec`: nobody comes back to this handle.
        if let Some(mut pipe) = child.stdin.take() {
            pipe.write_all(text.as_bytes())
                .unwrap_or_else(|e| panic!("spawn({}): writing stdin: {}", cmd, e));
        }
    }
    child.id() as i64
}

// `opts` is the std/process `ProcessOptions`, and until 2026-08-16 this function DROPPED IT.
// The signature took `_opts: O` and the comment here said "cwd/env/timeout aren't applied
// yet", which made it look like a known gap rather than what it was from the outside: a
// program that sets `cwd` or scrubs the environment got it obeyed under `run` and silently
// ignored under `build-rust`. Nothing failed, nothing warned, and the child ran in the wrong
// directory with the wrong environment.
//
// It is read through `Into<Value>` rather than by naming the struct, because the struct is
// GENERATED into the crate from the `case class` and this template cannot name it. Fields are
// positional, in declaration order — the same access the v2 os-plugin uses on the same type,
// and `Value::Obj`'s own comment says why it carries no names. A shape this does not
// recognise leaves every option unset, which is exactly the old behaviour, so an unrelated
// caller cannot be broken by this.
#[allow(dead_code, unused_variables)]
pub fn _exec<O: Into<crate::value::Value>>(cmd: String, args: Vec<String>, opts: O) -> ProcessResult {
    let mut command = std::process::Command::new(&cmd);
    command.args(&args);
    let mut stdin_text: Option<String> = None;
    let mut timeout_ms: Option<u64> = None;
    if let crate::value::Value::Obj("ProcessOptions", f) = opts.into() {
        // cwd — field 0, an Option lowered as `Obj("Some", [v])` / `Obj("None", [])`.
        if let Some(crate::value::Value::Obj("Some", inner)) = f.get(0) {
            if let Some(crate::value::Value::Str(dir)) = inner.get(0) { command.current_dir(dir); }
        }
        // inheritEnv — field 3. SCRUBBED BEFORE `env` IS APPLIED, not after: the point of
        // `inheritEnv = false` is that the child sees ONLY what the caller listed, and
        // clearing after would throw those away too.
        if let Some(crate::value::Value::Bool(false)) = f.get(3) { command.env_clear(); }
        // env — field 1.
        if let Some(crate::value::Value::Map(entries)) = f.get(1) {
            for (k, v) in entries {
                if let crate::value::Value::Str(s) = v { command.env(k, s); }
            }
        }
        // stdin — field 4. Read here, ACTED ON below, because the write needs a spawned
        // child and this block only configures the command.
        if let Some(crate::value::Value::Obj("Some", inner)) = f.get(4) {
            if let Some(crate::value::Value::Str(text)) = inner.get(0) {
                stdin_text = Some(text.clone());
            }
        }
        // timeout — field 2, in MILLISECONDS. Applied below, where there is a child to time.
        if let Some(crate::value::Value::Obj("Some", inner)) = f.get(2) {
            if let Some(crate::value::Value::Int(ms)) = inner.get(0) {
                if *ms > 0 { timeout_ms = Some(*ms as u64); }
            }
        }
    }
    // stdin — field 4, appended to the record so 0..3 above keep their meaning.
    //
    // `.output()` cannot be used when there is something to WRITE: it wires stdin to /dev/null
    // and gives no handle. So the stdin case spawns, writes, CLOSES (dropping the handle is
    // what sends EOF — a child reading to the end would otherwise block forever and `exec`
    // would hang), and then collects. The no-stdin case keeps `.output()` byte-for-byte, so
    // every existing caller is emitted and runs exactly as before.
    // (rozum `process-needs-a-stdin-pipe`: the alternative is argv, where `ps` shows a secret
    // to every local process.)
    // `stdin_text.is_some() && timeout_ms.is_none()` — NOT just `is_some()`. The timeout path
    // below writes stdin itself, and without this guard a call setting BOTH would take this
    // branch, return here, and silently lose its timeout: one cell of the matrix fixed and its
    // neighbour left, which is the shape this repository keeps paying for.
    if let (Some(text), None) = (stdin_text.clone(), timeout_ms) {
        use std::io::Write;
        command.stdin(std::process::Stdio::piped());
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        let mut child = command.spawn().unwrap_or_else(|e| panic!("exec({}): {}", cmd, e));
        {
            let mut pipe = child.stdin.take()
                .unwrap_or_else(|| panic!("exec({}): stdin pipe was not created", cmd));
            pipe.write_all(text.as_bytes())
                .unwrap_or_else(|e| panic!("exec({}): writing stdin: {}", cmd, e));
        }
        let out = child.wait_with_output()
            .unwrap_or_else(|e| panic!("exec({}): {}", cmd, e));
        return ProcessResult {
            stdout:   String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr:   String::from_utf8_lossy(&out.stderr).into_owned(),
            exitCode: out.status.code().unwrap_or(-1) as i64,
        };
    }
    // A TIMEOUT NEEDS A CHILD TO WATCH, so this path spawns instead of calling `.output()`,
    // which blocks until the child is done and can therefore never be interrupted. Measured
    // before the change, `exec("sleep 5", timeout = 600ms)` on this lane returned after the
    // FULL five seconds with exit code 0, while `run` killed at 600 ms and returned -1: not
    // merely unenforced, but reporting success for a call the caller had bounded.
    //
    // -1 ON EXPIRY IS READ OFF THE OTHER LANES, not invented — the v1 plugin and the jvm
    // runtime both answer -1 after `destroyForcibly`, and a fourth answer here would be a new
    // divergence closing an old one. Whatever the child managed to write is still returned;
    // partial output is what the other lanes hand back too.
    //
    // THE PIPES ARE DRAINED ON THREADS for the reason both JVM lanes carry in their own
    // comments: reading either stream to EOF on this thread blocks until the child exits,
    // which deadlocks on more than a pipe-buffer of output AND defeats the very timeout being
    // implemented — the read could not return before the process it is timing had finished.
    if let Some(limit) = timeout_ms {
        use std::io::Read;
        command.stdout(std::process::Stdio::piped());
        command.stderr(std::process::Stdio::piped());
        if stdin_text.is_some() { command.stdin(std::process::Stdio::piped()); }
        let mut child = command.spawn().unwrap_or_else(|e| panic!("exec({}): {}", cmd, e));
        if let Some(text) = stdin_text {
            use std::io::Write;
            if let Some(mut pipe) = child.stdin.take() {
                let _ = pipe.write_all(text.as_bytes());
            }
        }
        let mut co = child.stdout.take();
        let mut ce = child.stderr.take();
        let out_t = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(h) = co.as_mut() { let _ = h.read_to_end(&mut buf); }
            buf
        });
        let err_t = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(h) = ce.as_mut() { let _ = h.read_to_end(&mut buf); }
            buf
        });
        // Polling rather than a channel: no extra crate, and the granularity that matters is
        // "did it beat the deadline", not the exact millisecond it was killed on.
        let started = std::time::Instant::now();
        let mut code: i64 = -1;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => { code = status.code().unwrap_or(-1) as i64; break; }
                Ok(None) => {
                    if started.elapsed() >= std::time::Duration::from_millis(limit) {
                        let _ = child.kill();
                        let _ = child.wait();
                        code = -1;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => panic!("exec({}): {}", cmd, e),
            }
        }
        let ob = out_t.join().unwrap_or_default();
        let eb = err_t.join().unwrap_or_default();
        return ProcessResult {
            stdout:   String::from_utf8_lossy(&ob).into_owned(),
            stderr:   String::from_utf8_lossy(&eb).into_owned(),
            exitCode: code,
        };
    }
    let out = command
        .output()
        .unwrap_or_else(|e| panic!("exec({}): {}", cmd, e));
    ProcessResult {
        stdout:   String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr:   String::from_utf8_lossy(&out.stderr).into_owned(),
        exitCode: out.status.code().unwrap_or(-1) as i64,
    }
}
