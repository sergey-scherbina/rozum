//! Syntactic chunking for rag-lite (docs/specs/syntactic-rag.md, phase 1).
//!
//! Markdown files are split along their PARSE TREE — the vendored `uniml-md` crate (uniML
//! compiled via ssc→Rust, the operator's path-A decision: no JVM at build time or runtime) —
//! into heading-bounded, DISJOINT sections whose text is a byte-exact source slice. Everything
//! else falls back to blank-line paragraphs. Chunks feed [`crate::rag_lite::LexicalIndex`]
//! (BM25) behind the [`crate::rag_lite::Retriever`] seam; the persisted per-project index under
//! `.rozum/rag-index.json` is what lets `search_documents` serve an agent session that did not
//! just run the indexer.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::rag_lite::{retrieval_tools, LexicalIndex, Retriever};
use uniml_md::generated::ssc_program as u;

/// One retrievable unit. `id` doubles as a human-usable citation: `"<path>#<heading-slug>"`
/// for a markdown section, `"<path>#pN"` for a plain-text paragraph, `"<path>#doc"` for a
/// headingless markdown file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Chunk {
    pub id: String,
    pub text: String,
}

/// Counts from one [`index_project`] run.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IndexStats {
    /// Files whose chunks entered the index.
    pub files: usize,
    /// Files skipped (binary extension, non-UTF-8, oversized, unreadable).
    pub skipped: usize,
    /// Total chunks added.
    pub chunks: usize,
    /// Markdown files that took the paragraph path instead of the tree, because they exceed
    /// [`MAX_MARKDOWN_TREE_BYTES`] — indexed, but without heading-bounded sections.
    pub degraded: usize,
    /// Files whose chunks were REUSED from the previous index because their `mtime` and length
    /// were unchanged. Zero on a full build. This is the number that says whether an incremental
    /// pass did its job — `files` counts what is in the index, not what had to be re-parsed.
    pub reused: usize,
    /// Files re-parsed because they were new or had changed.
    pub rechunked: usize,
    /// Entries dropped because the file is gone from the tree. Deletions matter more than they
    /// look: a chunk for a file that no longer exists is the one failure mode retrieval cannot
    /// recover from, since the agent is handed code it will never find on disk.
    pub removed: usize,
}

/// The default limits the uniML markdown dialect itself uses (`MarkdownLimits_default` in the
/// generated code — a topval, so it is inlined at its use sites rather than exported; restated
/// here once, byte-for-byte the same values).
fn default_limits() -> u::MarkdownLimits {
    u::MarkdownLimits {
        core: u::Limits {
            maxDepth: 512,
            maxNodes: 10_000_000,
            maxTokenCodePoints: 16 * 1024 * 1024,
            maxDiagnostics: 10_000,
        },
        maxSourceCodePoints: 64 * 1024 * 1024,
        maxLineCodePoints: 1024 * 1024,
        maxDelimiterRun: 1024 * 1024,
        maxFenceCodePoints: 16 * 1024 * 1024,
        maxReferences: 1_000_000,
        maxBlocks: 10_000_000,
    }
}

/// Split a markdown file into heading-bounded, disjoint sections.
///
/// Boundaries come from the LOSSLESS tree's top-level `markdown.heading` branches — their
/// spans are exact source offsets, so each chunk is a byte-exact slice of `text`, and a `#`
/// inside a fenced code block can never be a boundary (it is a `markdown.code-block` token,
/// not a heading branch — the whole point of parsing over regex-splitting). A heading inside
/// a blockquote or list is NOT top-level and stays inside its section, which is what a reader
/// linking to the section expects.
///
/// A parse that reports Error/Fatal diagnostics (or an empty tree for non-empty input) falls
/// back to [`chunk_text`] — indexing never fails a file, it only degrades granularity.
pub fn chunk_markdown(path: &str, text: &str) -> Vec<Chunk> {
    chunk_markdown_with_limits(path, text, default_limits())
}

/// [`chunk_markdown`] with caller-supplied limits — exists so the fallback path is TESTABLE
/// (a tiny limit makes uniML halt with a diagnostic on an ordinary document; fabricating a
/// document that ordinary limits refuse would need megabytes). The example this comment used to
/// give was `maxBlocks`, which at the time was ACCEPTED AND NEVER READ — four of `MarkdownLimits`'
/// six fields were, so the comment named a limit that could not have halted anything. The test
/// below has always used `maxNodes`, which is enforced, so it was sound; the comment was not.
pub fn chunk_markdown_with_limits(path: &str, text: &str, limits: u::MarkdownLimits) -> Vec<Chunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let input = u::SourceInput {
        source: u::SourceId { value: path.to_string() },
        chunks: vec![u::SourceChunk { text: text.to_string() }],
    };
    let result = u::Markdown_parse(input, u::MarkdownProfile::Gfm, limits);
    // Three distinct "don't trust this tree" signals, and the STATUS one is the load-bearing
    // surprise: a limit hit (e.g. maxBlocks) HALTS the parse with a TRUNCATED tree and no
    // Error/Fatal diagnostic at all — sectioning a truncated tree would silently drop the
    // file's tail, while chunk_text keeps every byte.
    let broken = result
        .diagnostics
        .iter()
        .any(|d| matches!(d.severity, u::Severity::Error | u::Severity::Fatal))
        || !matches!(result.status, u::CompletionStatus::Complete)
        || result.roots.is_empty();
    if broken {
        return chunk_text(path, text);
    }

    // Top-level heading branches → (code-point offset, slug source text).
    let mut headings: Vec<(usize, String)> = Vec::new();
    for root in &result.roots {
        if let u::UniNode::Branch { kind, edges, span, .. } = root {
            if kind == "markdown.heading" {
                let title: String = edges
                    .iter()
                    .filter_map(|e| match &e.child {
                        // Content tokens only: the atx marker/close, indent and the trailing
                        // line-break all carry a role naming them; title text is role-less or
                        // "content" (setext), and inline emphasis arrives as nested branches.
                        u::UniNode::Token { value } => match e.role.as_deref() {
                            None | Some("content") => Some(value.lexeme.clone()),
                            _ => None,
                        },
                        u::UniNode::Branch { .. } => Some(collect_lexemes(&e.child)),
                    })
                    .collect();
                headings.push((span.start.offset.max(0) as usize, title));
            }
        }
    }
    if headings.is_empty() {
        return vec![Chunk { id: format!("{path}#doc"), text: text.to_string() }];
    }

    // uniML offsets count CODE POINTS; map them to byte offsets once.
    let cp_to_byte: Vec<usize> = {
        let mut v: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        v.push(text.len());
        v
    };
    let byte_at = |cp: usize| -> usize { *cp_to_byte.get(cp).unwrap_or(&text.len()) };

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut push = |id_frag: String, slice: &str| {
        if slice.trim().is_empty() {
            return;
        }
        let mut id = format!("{path}#{id_frag}");
        let mut n = 1;
        while !used.insert(id.clone()) {
            n += 1;
            id = format!("{path}#{id_frag}-{n}");
        }
        chunks.push(Chunk { id, text: slice.to_string() });
    };

    let first = byte_at(headings[0].0);
    push("preamble".to_string(), &text[..first]);
    for (i, (cp_start, title)) in headings.iter().enumerate() {
        let start = byte_at(*cp_start);
        let end = headings.get(i + 1).map(|(cp, _)| byte_at(*cp)).unwrap_or(text.len());
        let frag = match slugify(title) {
            Some(s) => s,
            None => format!("s{}", i + 1),
        };
        push(frag, &text[start..end]);
    }
    chunks
}

/// Split a Rust file into item-sized chunks: one per `fn`, `struct`, `impl` member, and so on.
///
/// Boundaries come from uniML's `uniml.rust` dialect — a STRUCTURAL dialect, not a Rust parser:
/// it matches braces while knowing where strings, chars and comments are, which is exactly what
/// a chunker needs and nothing more. The spans are exact source offsets, so every chunk is a
/// byte-exact slice of `text` and a `{` inside a string can never end an item early.
///
/// `impl` and `mod` members are chunked individually, with the `impl` HEADER folded into the
/// first member rather than emitted as a chunk of its own that repeats all of them.
///
/// A parse that reports Error/Fatal diagnostics, or does not complete, falls back to
/// [`chunk_text`] — indexing never fails a file, it only degrades granularity. Same rule as
/// markdown.
pub fn chunk_code(path: &str, text: &str) -> Vec<Chunk> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let input = u::SourceInput {
        source: u::SourceId { value: path.to_string() },
        chunks: vec![u::SourceChunk { text: text.to_string() }],
    };
    let result = u::UniML_parse(input, std::rc::Rc::new(u::RustDialect), core_limits());
    let broken = result
        .diagnostics
        .iter()
        .any(|d| matches!(d.severity, u::Severity::Error | u::Severity::Fatal))
        || !matches!(result.status, u::CompletionStatus::Complete);
    if broken {
        return chunk_text(path, text);
    }

    // uniML offsets count CODE POINTS; map them to byte offsets once, as chunk_markdown does.
    let cp_to_byte: Vec<usize> = {
        let mut v: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        v.push(text.len());
        v
    };
    let byte_at = |cp: i64| -> usize { *cp_to_byte.get(cp.max(0) as usize).unwrap_or(&text.len()) };

    // (start byte, id fragment) for every chunk this file yields, in source order. Only the
    // START is recorded: each chunk runs to the NEXT one's start and the last to end of file, so
    // the chunks TILE the source and nothing — a trailing comment after the last item, a blank
    // line between two — is ever dropped. Recording explicit ends lost exactly that, which an
    // existing index_project test caught.
    let mut items: Vec<(usize, String)> = Vec::new();
    for root in &result.roots {
        if let u::UniNode::Branch { kind, edges, span, .. } = root {
            if !kind.starts_with("rust.") {
                continue;
            }
            let outer_start = byte_at(span.start.offset);
            // An `impl`/`mod` body: chunk the MEMBERS, folding the header into the first, so a
            // method is retrievable on its own and the header is not duplicated across all of them.
            let members: Vec<&u::UniNode> = edges
                .iter()
                .map(|e| &e.child)
                .filter(|c| matches!(c, u::UniNode::Branch { kind, .. } if kind.starts_with("rust.")))
                .collect();
            if members.is_empty() {
                items.push((outer_start, item_frag(kind.as_str(), root)));
            } else {
                for (i, mnode) in members.iter().enumerate() {
                    if let u::UniNode::Branch { kind: mkind, span: mspan, .. } = mnode {
                        // The first member carries everything from the parent's start, which is
                        // the `impl Foo {` header (and its doc comment) the reader needs to make
                        // sense of the method.
                        let start =
                            if i == 0 { outer_start } else { byte_at(mspan.start.offset) };
                        items.push((start, item_frag(mkind.as_str(), mnode)));
                    }
                }
            }
        }
    }
    if items.is_empty() {
        return vec![Chunk { id: format!("{path}#file"), text: text.to_string() }];
    }

    let mut chunks: Vec<Chunk> = Vec::new();
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut push = |id_frag: String, slice: &str| {
        if slice.trim().is_empty() {
            return;
        }
        let mut id = format!("{path}#{id_frag}");
        let mut n = 1;
        while !used.insert(id.clone()) {
            n += 1;
            id = format!("{path}#{id_frag}-{n}");
        }
        chunks.push(Chunk { id, text: slice.to_string() });
    };

    // Everything before the first item — the module doc comment and the `use` block a reader
    // needs to resolve the names below.
    push("preamble".to_string(), &text[..items[0].0]);
    for i in 0..items.len() {
        let start = items[i].0;
        let end = items.get(i + 1).map(|(s, _)| *s).unwrap_or(text.len());
        push(items[i].1.clone(), &text[start..end.min(text.len())]);
    }
    chunks
}

/// The uniML core limits, restated once (the generated crate inlines its own `Limits.default`
/// as a topval rather than exporting it, exactly as `default_limits` explains for markdown).
fn core_limits() -> u::Limits {
    u::Limits {
        maxDepth: 512,
        maxNodes: 10_000_000,
        maxTokenCodePoints: 16 * 1024 * 1024,
        maxDiagnostics: 10_000,
    }
}

/// A citation fragment for one item: `"fn parse_header"`, `"struct Chunk"`.
///
/// The NAME is recovered from the branch's own tokens rather than carried on the instruction:
/// `Open`'s role lands on the edge attaching a closed frame to its PARENT, so a top-level item —
/// the case that matters most here — would lose it. Best-effort by design: the first identifier
/// after the item keyword, skipping a generic parameter list. A wrong name is a worse label,
/// never a wrong boundary.
fn item_frag(kind: &str, node: &u::UniNode) -> String {
    let short = kind.strip_prefix("rust.").unwrap_or(kind);
    let mut idents: Vec<String> = Vec::new();
    if let u::UniNode::Branch { edges, .. } = node {
        let mut depth = 0i32;
        let mut seen_keyword = false;
        for e in edges.iter() {
            if let u::UniNode::Token { value } = &e.child {
                let lx = value.lexeme.as_str();
                if value.kind == "rust.punct" {
                    // Step over `<…>` so `impl<T> Foo` names Foo, not T.
                    if lx == "<" {
                        depth += 1;
                    } else if lx == ">" {
                        depth -= 1;
                    }
                    continue;
                }
                if value.kind != "rust.ident" || depth > 0 {
                    continue;
                }
                if !seen_keyword {
                    if is_item_keyword(lx) {
                        seen_keyword = true;
                    }
                    continue;
                }
                idents.push(lx.to_string());
                break;
            }
        }
    }
    match idents.first() {
        Some(name) => format!("{short} {name}"),
        None => short.to_string(),
    }
}

fn is_item_keyword(w: &str) -> bool {
    matches!(
        w,
        "fn" | "struct" | "enum" | "trait" | "impl" | "mod" | "use" | "const" | "static"
            | "type" | "union" | "macro_rules"
    )
}

/// Every token lexeme under a node, in order (nested inline branches inside a heading title).
fn collect_lexemes(node: &u::UniNode) -> String {
    match node {
        u::UniNode::Token { value } => value.lexeme.clone(),
        u::UniNode::Branch { edges, .. } => edges.iter().map(|e| collect_lexemes(&e.child)).collect(),
    }
}

fn slugify(title: &str) -> Option<String> {
    let slug: String = title
        .to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if slug.is_empty() { None } else { Some(slug) }
}

/// The fallback for any non-markdown text file: blank-line-separated paragraphs. CRLF-safe —
/// a line of pure whitespace (including a bare `\r`) is a separator, never an empty chunk.
pub fn chunk_text(path: &str, text: &str) -> Vec<Chunk> {
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let mut flush = |para: &mut Vec<&str>, chunks: &mut Vec<Chunk>| {
        if !para.is_empty() {
            let body = para.join("\n");
            let n = chunks.len() + 1;
            chunks.push(Chunk { id: format!("{path}#p{n}"), text: body });
            para.clear();
        }
    };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut para, &mut chunks);
        } else {
            para.push(line.trim_end_matches('\r'));
        }
    }
    flush(&mut para, &mut chunks);
    chunks
}

/// Extensions that are never text — cheaper than sniffing their bytes.
const BINARY_EXT: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "ico", "pdf", "zip", "gz", "tgz", "tar", "xz", "zst",
    "so", "dylib", "a", "o", "rlib", "bin", "dat", "db", "sqlite", "jar", "class", "woff",
    "woff2", "ttf", "otf", "eot", "gguf", "safetensors", "npz", "mlx", "wasm", "exe", "dll",
    "mp3", "mp4", "mov", "avi", "heic", "lock",
];

/// Directories that are never source: VCS, build output, vendored trees, sibling worktrees,
/// and rozum's own per-project state (indexing the index would feed it back to itself).
const SKIP_DIRS: &[&str] =
    &[".git", "target", "node_modules", ".worktrees", ".rozum", ".vendor", ".venv", "__pycache__"];

/// The largest markdown file that takes the SYNTACTIC path. Above it, `chunk_text` — the file
/// is still indexed, just without heading-bounded sections.
///
/// This is a mitigation for a MEASURED defect in the vendored parser, not a taste call:
/// `Markdown_parse` is **quadratic in input bytes**, and it is 99.2% of chunking cost (measured
/// 7.42 s of 7.48 s on a 10 KB document; the chunker's own tree walk is the other 0.8%). The
/// curve is clean — every doubling of input costs ~4x:
///
/// ```text
///    1 KB  →   0.10 s        16 KB  →  20.9 s
///    2 KB  →   0.35 s        32 KB  →  85.5 s
///    4 KB  →   1.30 s       505 KB  →  ~6.9 h   (extrapolated: this repo's own SPRINT.md)
///    8 KB  →   5.29 s
/// ```
///
/// Cost tracks BYTES, not block count — 16 KB as 682 sections (25.5 s) and as 8 sections
/// (22.8 s) cost the same — so nothing about how a document is structured avoids it.
///
/// **Raised to 1 MB on 2026-08-31, when the parse became LINEAR — on non-ASCII too.** A series of
/// fixes in scalascript's Rust backend and uniML got there: self-append/self-extend lowered to
/// `push`/`extend`, read-only `Vec` parameters and captures passed by reference, `take`/`drop`/
/// `slice` borrowing instead of cloning their receiver, the index-based string scanners hoisting
/// one `toVector`, and finally `MdLine.split` slicing that code-unit vector rather than calling
/// `substring` on the whole document. 256 KB went 173.4 s → 0.108 s, roughly 1600×.
///
/// A second round then took read-only `Vec`/`String` parameters of EVERY generated def by shared
/// reference instead of only class methods'. `uniml/markdown`'s inline lexer had a match-arm guard
/// evaluated at every character of a line, holding the whole line by value — so it copied the line
/// per character. Re-measured 2026-08-31 against that crate, best of 3:
///
/// ```text
///   SPRINT.md      503 KB   0.8% non-ASCII   2.707 s   (5.26 -> 3.244 -> 2.707)
///   CHANGELOG.md   451 KB   0.5% non-ASCII   0.804 s   (2.82 -> 1.096 -> 0.804)
///   BACKLOG.md     142 KB   0.5% non-ASCII   0.450 s   (0.75 -> 0.564 -> 0.450)
///
///   Cyrillic 512 KB  83% non-ASCII   1.301 -> 0.263 s
///   emoji    512 KB  74% non-ASCII   3.193 -> 0.492 s
///
///   ONE 64 KB LINE (one frame, one token stream)   35.203 -> 0.220 s
///   the same 64 KB as many small blocks             0.171 ->  0.011 s
/// ```
///
/// The single-huge-line column is the one worth remembering: cost depends on document SHAPE, not
/// only size. 64 KB as many blocks and 64 KB as one line differed by 206× before this, and the
/// pathological shape is what a CODE dialect produces (one function body is one frame), which is
/// why it mattered beyond prose.
///
/// WHY A CAP AT ALL, STILL: not super-linearity — that is fixed for ordinary documents — but
/// LATENCY, and one honest gap. Worst case measured is now ~1 s/MB for dense non-ASCII prose, but
/// SPRINT.md still costs 2.707 s for 503 KB (~5.4 s/MB) on structure alone, and a single enormous
/// frame remains mildly super-linear (~×3.8 per doubling: 0.018 / 0.055 / 0.215 s at 16/32/64 KB),
/// just 160× cheaper than before. [`MAX_FILE_BYTES`] admits documents up to 4 MB, so without a cap
/// one adversarially-shaped file could still hold indexing for a long time. 1 MB sits above every
/// document anyone here writes (the largest is SPRINT.md at 503 KB) while bounding a single file's
/// tree parse. Raise it only with a measurement, not an estimate: the first two raises were argued
/// from a synthetic benchmark and both had to be reverted. `examples/mdbench.rs` is the instrument.
pub const MAX_MARKDOWN_TREE_BYTES: usize = 1024 * 1024;

/// Files larger than this are skipped outright — a multi-megabyte blob is generated output or
/// data, and one such file would dominate BM25's length statistics.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Walk `root`, chunk every text file (`.md` syntactically, the rest by paragraphs), feed
/// `index`. Never fails the run on a bad file — that file is counted in `skipped`.
pub fn index_project(root: &Path, index: &mut LexicalIndex) -> IndexStats {
    let (files, mut stats) = project_files_with_progress(root, &HashMap::new(), &mut |_, _, _| {});
    let mut chunks = 0usize;
    for f in &files {
        for c in &f.chunks {
            index.add(c.id.clone(), c.text.clone());
            chunks += 1;
        }
    }
    stats.chunks = chunks;
    stats
}

/// The chunk list behind [`index_project`], exposed so persistence can save exactly what was
/// indexed (the index itself is rebuilt from chunks on load — BM25 construction is cheap and
/// this keeps the on-disk format independent of `LexicalIndex` internals).
pub fn project_chunks(root: &Path) -> (Vec<Chunk>, IndexStats) {
    project_chunks_with_progress(root, &mut |_, _, _| {})
}

/// [`project_chunks`] reporting each file as it is read: `(relative path, bytes, took the
/// syntactic path)`. Exists because indexing a real repo takes minutes — see
/// [`MAX_MARKDOWN_TREE_BYTES`] — and a CLI that prints nothing for that long looks hung.
pub fn project_chunks_with_progress(
    root: &Path,
    progress: &mut dyn FnMut(&str, usize, bool),
) -> (Vec<Chunk>, IndexStats) {
    let (files, stats) = project_files_with_progress(root, &HashMap::new(), progress);
    (files.into_iter().flat_map(|f| f.chunks).collect(), stats)
}

/// The files-and-stats walk every indexing path shares. `prev` is the previous index keyed by
/// project-relative path; a file whose `mtime` and length both match its entry is REUSED and
/// never read or parsed. Pass an empty map for a full rebuild.
fn project_files_with_progress(
    root: &Path,
    prev: &HashMap<String, FileChunks>,
    progress: &mut dyn FnMut(&str, usize, bool),
) -> (Vec<FileChunks>, IndexStats) {
    let mut stats = IndexStats::default();
    let mut files: Vec<FileChunks> = Vec::new();
    match git_project_files(root) {
        Some(paths) => {
            for rel in paths {
                let path = root.join(&rel);
                let Ok(meta) = path.symlink_metadata() else { continue };
                if meta.file_type().is_symlink() || meta.is_dir() {
                    continue;
                }
                index_one_file(&path, rel, &meta, prev, &mut files, &mut stats, progress);
            }
        }
        None => collect_project_chunks(root, root, &mut files, prev, &mut stats, progress),
    }
    stats.chunks = files.iter().map(|f| f.chunks.len()).sum();
    // Anything in the previous index the walk did not reach is gone from the tree. Counting it
    // is not bookkeeping: these are the entries that would otherwise point an agent at code that
    // no longer exists, which is the one wrong answer retrieval cannot recover from.
    let seen: std::collections::HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    stats.removed = prev.keys().filter(|p| !seen.contains(p.as_str())).count();
    (files, stats)
}


/// The files git considers part of this project: tracked, plus untracked ones that are NOT
/// ignored. `None` when `root` is not a git repository (or git is unavailable), in which case
/// the caller falls back to walking directories with the `SKIP_DIRS` fences.
///
/// This replaces a hardcoded denylist with the project's OWN declaration of what is source, and
/// it was worth doing on a measurement rather than on taste: in this repo `scripts/bench/results/`
/// — gitignored benchmark RUN OUTPUT, not code — was **36,034 of 46,733 chunks, 77% of the index
/// and 15.9 MB of its 31 MB**. That is not merely wasted space: BM25 ranks on document-frequency
/// statistics computed over the whole corpus, so three quarters of it being machine-generated
/// transcripts skews every score, and one such file was measurably ranking top-3 for a question
/// about the proxy.
///
/// Untracked-but-not-ignored files are deliberately INCLUDED: a file the agent created moments
/// ago is exactly what it will ask about next, and "tracked only" would undo the freshness the
/// incremental refresh exists to provide.
fn git_project_files(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let paths: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .filter_map(|p| std::str::from_utf8(p).ok().map(str::to_string))
        .collect();
    // An empty list from a real repo is possible but indistinguishable here from a git that
    // answered nothing useful; walking is the safe reading, and costs one pass on an empty tree.
    if paths.is_empty() { None } else { Some(paths) }
}

/// Index ONE file: the shared body of both discovery paths (the git file list and the
/// directory walk), so a rule added to one can never quietly miss the other.
#[allow(clippy::too_many_arguments)]
fn index_one_file(
    path: &Path,
    rel: String,
    meta: &std::fs::Metadata,
    prev: &HashMap<String, FileChunks>,
    out: &mut Vec<FileChunks>,
    stats: &mut IndexStats,
    progress: &mut dyn FnMut(&str, usize, bool),
) {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    if BINARY_EXT.contains(&ext.as_str()) || meta.len() > MAX_FILE_BYTES {
        stats.skipped += 1;
        return;
    }
    let mtime_secs = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // The reuse decision is made HERE, before `fs::read` — that is the whole saving. Deciding
    // after reading would still pay the I/O on every file and leave only the parse to skip,
    // and on this corpus the parse is the cheaper half for everything except large markdown.
    if let Some(hit) = prev.get(&rel)
        && hit.mtime_secs == mtime_secs
        && hit.len == meta.len()
    {
        stats.reused += 1;
        if !hit.chunks.is_empty() {
            stats.files += 1;
        }
        out.push(hit.clone());
        return;
    }
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(_) => {
            stats.skipped += 1;
            return;
        }
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => {
            stats.skipped += 1;
            return;
        }
    };
    // `.rs` goes through the uniml.rust dialect (RAG phase 2). It shares markdown's size cap:
    // the cap exists because a non-ASCII document is still super-linear, and while code is
    // ASCII in practice a generated .rs file can be enormous, so the same bound applies.
    let tree_path = (ext == "md" || ext == "rs") && text.len() <= MAX_MARKDOWN_TREE_BYTES;
    progress(&rel, text.len(), tree_path);
    let file_chunks = if tree_path && ext == "md" {
        chunk_markdown(&rel, &text)
    } else if tree_path {
        chunk_code(&rel, &text)
    } else {
        chunk_text(&rel, &text)
    };
    if (ext == "md" || ext == "rs") && !tree_path {
        stats.degraded += 1;
    }
    stats.rechunked += 1;
    if !file_chunks.is_empty() {
        stats.files += 1;
    }
    // Recorded even when it produced NO chunks (an empty or whitespace-only file). Skipping
    // those here would leave them out of the manifest, so every later pass would re-read them
    // forever — a small cost that never converges, which is the wrong shape for a cache.
    out.push(FileChunks { path: rel, mtime_secs, len: meta.len(), chunks: file_chunks });
}

fn collect_project_chunks(
    root: &Path,
    dir: &Path,
    out: &mut Vec<FileChunks>,
    prev: &HashMap<String, FileChunks>,
    stats: &mut IndexStats,
    progress: &mut dyn FnMut(&str, usize, bool),
) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            stats.skipped += 1;
            return;
        }
    };
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        // Symlinks are skipped wholesale: a link out of the tree defeats the SKIP_DIRS fences.
        let meta = match path.symlink_metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if !SKIP_DIRS.contains(&name) {
                collect_project_chunks(root, &path, out, prev, stats, progress);
            }
            continue;
        }
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
        index_one_file(&path, rel, &meta, prev, out, stats, progress);
    }
}

// ─── Persistence ───────────────────────────────────────────────────────────────

/// On-disk shape of the per-project index: the CHUNKS, not the BM25 tables — rebuild on load
/// is cheap and the format stays independent of `LexicalIndex` internals (which the embedding
/// backend of phase 3 will replace anyway).
///
/// v2 groups chunks BY FILE and records the stat that produced them, which is what makes an
/// incremental pass possible. A flat chunk list would force reuse to be decided by scanning
/// every chunk id for a path prefix — O(all chunks) per file, 46k × 2648 here, slower than
/// re-parsing the tree it was meant to avoid.
#[derive(Serialize, Deserialize)]
struct SavedIndex {
    version: u32,
    generated_utc: String,
    /// v2. Empty when loading a v1 file, which is what makes the first pass after this change a
    /// full rebuild rather than a wrong answer.
    #[serde(default)]
    files: Vec<FileChunks>,
    /// v1's flat list. Still READ so an existing index keeps serving searches across the upgrade;
    /// never written again, and it carries no stat, so it cannot be reused incrementally.
    #[serde(default)]
    chunks: Vec<Chunk>,
}

/// One source file's chunks, with the stat they were produced from.
#[derive(Serialize, Deserialize, Clone)]
struct FileChunks {
    /// Project-relative, the same string that prefixes each chunk id.
    path: String,
    /// Seconds since the epoch. Second resolution is deliberate: it is what every filesystem
    /// here agrees on, and the pairing with `len` covers the case it misses.
    mtime_secs: u64,
    /// Length in bytes. Carried BECAUSE mtime alone is not sufficient — a write inside the same
    /// second (an agent editing a file it just wrote) leaves mtime unchanged, and a length
    /// change catches most of those. This pair is a cheap heuristic, not a content hash: the
    /// residual case is an edit that keeps the byte count within one second of the last write.
    len: u64,
    chunks: Vec<Chunk>,
}

/// Where a project's index lives: `<root>/.rozum/rag-index.json` — the same per-project
/// `.rozum/` state directory the meeting rooms already use.
pub fn index_path(root: &Path) -> PathBuf {
    root.join(".rozum").join("rag-index.json")
}

/// Index `root` and persist the result. Returns the stats and the file written.
pub fn index_and_save(root: &Path) -> std::io::Result<(IndexStats, PathBuf)> {
    index_and_save_with_progress(root, &mut |_, _, _| {})
}

/// [`index_and_save`] with the per-file progress callback of
/// [`project_chunks_with_progress`].
pub fn index_and_save_with_progress(
    root: &Path,
    progress: &mut dyn FnMut(&str, usize, bool),
) -> std::io::Result<(IndexStats, PathBuf)> {
    write_index(root, &HashMap::new(), progress)
}

/// Refresh `root`'s index, re-parsing ONLY files whose `mtime` or length changed since the last
/// build, and dropping entries whose files are gone.
///
/// This is what makes freshness affordable rather than aspirational: a full build of this repo is
/// ~33 s, which is far too slow to run after an edit, so before this the index was rebuilt rarely
/// and served stale in between — the failure mode `rag.search` could report but not fix.
///
/// Falls back to a full build when there is no previous index or it predates the per-file
/// manifest (v1), which is the honest thing to do: a v1 file carries no stat, so "unchanged"
/// cannot be established for any entry in it.
pub fn reindex_incremental(
    root: &Path,
    progress: &mut dyn FnMut(&str, usize, bool),
) -> std::io::Result<(IndexStats, PathBuf)> {
    let prev = load_manifest(root);
    write_index(root, &prev, progress)
}

/// Build-or-refresh the index behind a cross-process lock, for callers that run it eagerly in
/// the background rather than on demand.
///
/// The lock is the point. Several agents commonly start in one project at the same time, and
/// without it each would run the same 23.5 s full build, N× the CPU for one identical file — on
/// a machine where local models are already competing for it. `try_lock` and not a blocking one:
/// a second agent should skip the work, not queue behind it, because when the holder finishes
/// the answer is already on disk for everyone.
///
/// `Ok(None)` = someone else is doing it. Returning that rather than an error keeps the caller's
/// handling honest: nothing went wrong, and there is nothing to report.
pub fn refresh_in_background(
    root: &Path,
    progress: &mut dyn FnMut(&str, usize, bool),
) -> std::io::Result<Option<(IndexStats, PathBuf)>> {
    let dir = root.join(".rozum");
    fs::create_dir_all(&dir)?;
    ignore_own_dir(&dir);
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("rag-index.lock"))?;
    // WouldBlock and a real failure are DIFFERENT answers and must not share a branch. The first
    // version treated any `Err` as "a sibling is building", so an I/O error silently became a
    // skipped build with nothing logged — and it showed up as a flaky test (passing alone,
    // failing about one run in three under the full suite's parallelism) rather than as the
    // defect it is. A lock we could not even evaluate is an error to report, not a no-op.
    match lock.try_lock() {
        Ok(()) => {}
        Err(fs::TryLockError::WouldBlock) => return Ok(None),
        Err(fs::TryLockError::Error(e)) => return Err(e),
    }
    // Held until this returns; the file stays on disk, which is fine — the LOCK is the state,
    // not the file, so a crashed builder releases it with its fd and leaves nothing to reap.
    let out = reindex_incremental(root, progress)?;
    Ok(Some(out))
}

fn write_index(
    root: &Path,
    prev: &HashMap<String, FileChunks>,
    progress: &mut dyn FnMut(&str, usize, bool),
) -> std::io::Result<(IndexStats, PathBuf)> {
    let (files, stats) = project_files_with_progress(root, prev, progress);
    let file = index_path(root);
    // NOTHING CHANGED → do not rewrite the file. This is not an optimisation of the write, it is
    // what makes refresh-on-every-search viable: a reader (the MCP proxy) holds the index in
    // memory and reloads it when the file's mtime moves, so rewriting identical content on every
    // call would make it re-read 31 MB every time — turning the freshness check into the most
    // expensive part of a search. A no-op pass must leave the file untouched.
    if !prev.is_empty() && stats.rechunked == 0 && stats.removed == 0 && file.exists() {
        return Ok((stats, file));
    }
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
        ignore_own_dir(dir);
    }
    let saved = SavedIndex {
        version: 2,
        generated_utc: chrono::Utc::now().to_rfc3339(),
        files,
        chunks: Vec::new(),
    };
    fs::write(&file, serde_json::to_vec(&saved)?)?;
    Ok((stats, file))
}

/// Make `.rozum/` ignore itself, the convention the meeting store already established
/// (`store.rs::materialize`). Load-bearing now that the index is built AUTOMATICALLY in the
/// background: without it every project an agent visits grows a 31 MB untracked file that shows
/// up in `git status` and can be swept into a commit by a careless `git add -A`. Best-effort and
/// never overwritten — a project that wants different rules keeps them.
fn ignore_own_dir(dir: &Path) {
    let gi = dir.join(".gitignore");
    if !gi.exists() {
        let _ = fs::write(&gi, "*\n");
    }
}

/// Every chunk `(id, text)` in the persisted index — the corpus the embedding warmup walks.
/// v2 manifests only: a v1 index carries no per-file grouping and will be rebuilt on the next
/// refresh anyway, so vectors for it would be discarded within minutes.
pub fn saved_chunk_texts(root: &Path) -> Vec<(String, String)> {
    load_manifest(root)
        .into_values()
        .flat_map(|f| f.chunks.into_iter().map(|c| (c.id, c.text)))
        .collect()
}

/// The previous build's per-file manifest, keyed by project-relative path. Empty for a missing
/// index or a v1 one.
fn load_manifest(root: &Path) -> HashMap<String, FileChunks> {
    let Ok(bytes) = fs::read(index_path(root)) else {
        return HashMap::new();
    };
    let Ok(saved) = serde_json::from_slice::<SavedIndex>(&bytes) else {
        return HashMap::new();
    };
    saved.files.into_iter().map(|f| (f.path.clone(), f)).collect()
}

/// Load the persisted index for `root`, rebuilding the BM25 tables. `None` when no index has
/// been built (callers fall back to whatever they did before).
pub fn load_project_index(root: &Path) -> Option<LexicalIndex> {
    let bytes = fs::read(index_path(root)).ok()?;
    let saved: SavedIndex = serde_json::from_slice(&bytes).ok()?;
    let mut index = LexicalIndex::new();
    // v2 keeps chunks grouped by file; `chunks` is v1's flat list and is still read so an index
    // written before the manifest existed keeps answering searches until the next build.
    for f in saved.files {
        for c in f.chunks {
            index.add(c.id, c.text);
        }
    }
    for c in saved.chunks {
        index.add(c.id, c.text);
    }
    Some(index)
}

/// The existing `search_documents` agent tool, backed by the persisted project index when one
/// exists — this is how an agent session that never ran the indexer still gets retrieval.
pub fn project_retrieval_tools(root: &Path) -> Option<crate::agent::CallbackToolSource> {
    let index = load_project_index(root)?;
    Some(retrieval_tools(Arc::new(index) as Arc<dyn Retriever>))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ─── RAG phase 2: code chunking (docs/specs/rag-rust-dialect.md) ──────────────────

    // Behavior: a fn with a doc comment and attributes is ONE chunk containing all three, and
    // the next item's chunk starts after it — disjoint, as in phase 1.
    #[test]
    fn code_items_carry_their_docs_and_are_disjoint() {
        let src = "/// Doc line.\n#[inline]\npub fn f() {}\nfn g() {}\n";
        let cs = chunk_code("a.rs", src);
        let f = cs.iter().find(|c| c.id.contains("fn f")).expect(&format!("{cs:?}"));
        assert!(f.text.contains("/// Doc line."), "{}", f.text);
        assert!(f.text.contains("#[inline]"), "{}", f.text);
        assert!(!f.text.contains("fn g"), "sections must be disjoint: {}", f.text);
        assert!(cs.iter().any(|c| c.id.contains("fn g")), "{cs:?}");
    }

    // Behavior: a brace inside a string, a char or a comment does not open or close an item.
    // This is the whole reason the dialect lexes instead of matching braces with a regex.
    #[test]
    fn braces_hidden_in_literals_do_not_split_items() {
        let src = concat!(
            "fn f() {\n",
            "    let a = \"{\";\n",
            "    let b = '}';\n",
            "    // }\n",
            "    let c = r#\"} still raw {\"#;\n",
            "}\n",
            "fn g() {}\n",
        );
        let cs = chunk_code("a.rs", src);
        let f = cs.iter().find(|c| c.id.contains("fn f")).expect(&format!("{cs:?}"));
        assert!(f.text.contains("still raw"), "f must span its whole body: {}", f.text);
        assert!(cs.iter().any(|c| c.id.contains("fn g")), "{cs:?}");
    }

    // Behavior: methods inside an impl are their own chunks; the impl header is folded into the
    // first one rather than becoming a chunk that duplicates all of them.
    #[test]
    fn impl_members_are_their_own_chunks() {
        let src = "impl Foo {\n    pub fn a(&self) {}\n    fn b(&self) {}\n}\n";
        let cs = chunk_code("a.rs", src);
        let a = cs.iter().find(|c| c.id.contains("fn a")).expect(&format!("{cs:?}"));
        let b = cs.iter().find(|c| c.id.contains("fn b")).expect(&format!("{cs:?}"));
        assert!(a.text.contains("impl Foo"), "the header belongs to the first member: {}", a.text);
        assert!(!b.text.contains("impl Foo"), "and is not repeated: {}", b.text);
        assert!(!a.text.contains("fn b"), "members are disjoint: {}", a.text);
    }

    // Behavior: a file with no items still yields a chunk, not zero.
    #[test]
    fn a_file_with_no_items_still_yields_one_chunk() {
        let cs = chunk_code("a.rs", "// just a note\n/* and another */\n");
        assert_eq!(cs.len(), 1, "{cs:?}");
        assert!(cs[0].text.contains("just a note"), "{cs:?}");
    }

    // Behavior: a syntactically broken file falls back to chunk_text and never fails the run.
    #[test]
    fn a_broken_file_falls_back_to_text() {
        let src = "fn f() { let a = \"unterminated;\nfn g() {}\n";
        let cs = chunk_code("a.rs", src);
        assert!(!cs.is_empty(), "a broken file must still be indexed");
        // Whole content preserved either way — nothing is silently dropped.
        let joined: String = cs.iter().map(|c| c.text.as_str()).collect();
        assert!(joined.contains("unterminated"), "{cs:?}");
    }

    // Behavior: LOSSLESS over this repo's own crates/ — every chunk is a byte-exact slice, so
    // concatenating a file's chunks reproduces it apart from whitespace between items.
    #[test]
    fn code_chunks_are_byte_exact_slices_of_the_source() {
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        if !crates.is_dir() {
            return;
        }
        let mut files = 0usize;
        let mut chunks = 0usize;
        for entry in walk_rs(&crates).into_iter().take(60) {
            let text = match fs::read_to_string(&entry) {
                Ok(t) => t,
                Err(_) => continue,
            };
            if text.len() > MAX_MARKDOWN_TREE_BYTES {
                continue;
            }
            let cs = chunk_code("f.rs", &text);
            // Stronger than "each chunk is a substring": the chunks TILE the file, so
            // concatenating them in order reproduces it exactly.
            let joined: String = cs.iter().map(|c| c.text.as_str()).collect();
            assert_eq!(joined, text, "chunks do not tile {entry:?}");
            files += 1;
            chunks += cs.len();
        }
        assert!(files > 10, "expected to have checked real files, saw {files}");
        assert!(chunks > files, "expected several items per file, {chunks} over {files}");
    }

    fn walk_rs(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(d) = stack.pop() {
            let rd = match fs::read_dir(&d) {
                Ok(r) => r,
                Err(_) => continue,
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let name = p.file_name().unwrap_or_default().to_string_lossy().to_string();
                    if name != "target" && !name.starts_with('.') {
                        stack.push(p);
                    }
                } else if p.extension().map(|x| x == "rs").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }

    // Behavior: heading-bounded, disjoint sections; headingless doc = one chunk.
    #[test]
    fn heading_sections_are_disjoint() {
        let text = "intro\n\n# One\nbody one\n\n## Two\nbody two\n";
        let chunks = chunk_markdown("d.md", text);
        let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["d.md#preamble", "d.md#one", "d.md#two"], "{chunks:?}");
        assert!(chunks[1].text.contains("# One") && chunks[1].text.contains("body one"));
        assert!(!chunks[1].text.contains("body two"), "next section leaked: {:?}", chunks[1]);
        assert!(chunks[2].text.contains("## Two") && chunks[2].text.contains("body two"));
        // Disjoint + complete: the chunks concatenate back to the whole source.
        assert_eq!(chunks.iter().map(|c| c.text.as_str()).collect::<String>(), text);
    }

    #[test]
    fn headingless_doc_is_one_chunk() {
        let chunks = chunk_markdown("d.md", "just a paragraph\n\nand another\n");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].id, "d.md#doc");
    }

    // Behavior: a `#` inside a fence is not a boundary; fences never split mid-block.
    #[test]
    fn fence_is_opaque_to_sectioning() {
        let text = "# One\nbefore\n```sh\n# not a heading\necho hi\n```\nafter\n\n# Two\ntail\n";
        let chunks = chunk_markdown("d.md", text);
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert!(
            chunks[0].text.contains("# not a heading") && chunks[0].text.contains("after"),
            "fence split or misplaced: {:?}",
            chunks[0]
        );
        assert!(chunks[1].text.starts_with("# Two"));
    }

    // Behavior: setext headings bound sections too (the tree names them, no regex could).
    #[test]
    fn setext_headings_bound_sections() {
        let text = "Alpha\n=====\nbody a\n\nBeta\n-----\nbody b\n";
        let chunks = chunk_markdown("d.md", text);
        let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["d.md#alpha", "d.md#beta"], "{chunks:?}");
    }

    #[test]
    fn duplicate_heading_slugs_get_ordinals() {
        let text = "# Same\na\n\n# Same\nb\n";
        let chunks = chunk_markdown("d.md", text);
        let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["d.md#same", "d.md#same-2"]);
    }

    // Behavior: chunk_text splits on blank-line runs; CRLF yields no empty chunks.
    #[test]
    fn chunk_text_paragraphs_crlf_safe() {
        let chunks = chunk_text("t.txt", "alpha\r\n\r\nbeta\r\ngamma\r\n\r\n\r\n");
        assert_eq!(chunks.len(), 2, "{chunks:?}");
        assert_eq!(chunks[0].text, "alpha");
        assert_eq!(chunks[1].text, "beta\ngamma");
        assert!(chunks.iter().all(|c| !c.text.trim().is_empty()));
    }

    // Behavior: a parse the dialect refuses falls back to chunk_text — indexing never fails
    // a file. `maxNodes = 1` is the forcing lever, and WHICH lever is a measured finding, not
    // a guess: of the six limits, `maxNodes`/`maxDepth`/`maxSourceCodePoints`/
    // `maxTokenCodePoints` all report `status: Halted` with a Fatal diagnostic, while
    // `maxBlocks` and `maxLineCodePoints` are accepted and then silently NOT enforced — a
    // document exceeding either parses `Complete` with a full tree. See the spec's Decisions.
    #[test]
    fn broken_parse_falls_back_to_text() {
        let mut limits = default_limits();
        limits.core.maxNodes = 1;
        let text = "# One\na\n\n# Two\nb\n\n# Three\nc\n";
        let chunks = chunk_markdown_with_limits("d.md", text, limits);
        assert!(!chunks.is_empty());
        assert!(
            chunks.iter().all(|c| c.id.contains("#p")),
            "expected chunk_text ids, got {:?}",
            chunks.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
    }

    /// The two limits that `rag-uniml-unenforced-limits` found dead, asserted THROUGH the
    /// vendored Rust crate rather than only in scalascript's own suite — that is the whole point
    /// of the entry. A limit enforced in the Scala source but lost in the ssc→Rust lowering would
    /// be the same defect one layer down, and this is the layer rozum actually runs.
    #[test]
    fn markdown_line_and_block_limits_are_enforced_in_the_vendored_crate() {
        let mut line_limited = default_limits();
        line_limited.maxLineCodePoints = 4;
        let chunks = chunk_markdown_with_limits("d.md", "short\nthis line is far too long\n", line_limited);
        assert!(
            chunks.iter().all(|c| c.id.contains("#p")),
            "a line-limit halt must fall back to chunk_text, got {:?}",
            chunks.iter().map(|c| &c.id).collect::<Vec<_>>()
        );

        let mut block_limited = default_limits();
        block_limited.maxBlocks = 2;
        let doc: String = (1..=20).map(|i| format!("# Heading {i}\n\nbody {i}\n\n")).collect();
        let chunks = chunk_markdown_with_limits("d.md", &doc, block_limited);
        assert!(
            chunks.iter().all(|c| c.id.contains("#p")),
            "a block-limit halt must fall back to chunk_text, got {:?}",
            chunks.iter().map(|c| &c.id).collect::<Vec<_>>()
        );

        // And the defaults must not fire on an ordinary document: a limit that trips in normal use
        // silently costs every file its heading structure, which is exactly what the cap saga cost.
        let ok = chunk_markdown_with_limits("d.md", "# T\n\nbody\n\n## S\n\nmore\n", default_limits());
        assert!(ok.iter().any(|c| c.id.contains("#t")), "defaults must keep the tree: {ok:?}");
    }

    // Behavior: index_project walks a tree, skips VCS/build/binaries, counts right.
    #[test]
    fn index_project_walks_and_skips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::create_dir_all(root.join("target")).unwrap();
        fs::write(root.join("README.md"), "# Title\n\nreadme body\n").unwrap();
        fs::write(root.join("docs/a.md"), "# Alpha\nalpha body\n\n# Beta\nbeta body\n").unwrap();
        fs::write(root.join("main.rs"), "fn main() {}\n\n// trailing\n").unwrap();
        fs::write(root.join("logo.png"), [0x89u8, 0x50, 0x4e, 0x47]).unwrap();
        fs::write(root.join(".git/config"), "[core]\n").unwrap();
        fs::write(root.join("target/junk.txt"), "generated\n").unwrap();
        let mut index = LexicalIndex::new();
        let stats = index_project(root, &mut index);
        assert_eq!(stats.files, 3, "{stats:?}"); // README.md, docs/a.md, main.rs
        assert_eq!(stats.skipped, 1, "{stats:?}"); // logo.png
        // README.md 1 + docs/a.md 2 + main.rs 1. `main.rs` used to be split into two PARAGRAPH
        // chunks; it is now chunked by ITEM (RAG phase 2), and `fn main() {}` plus the trailing
        // comment is one item — which is the point of chunking code structurally.
        assert_eq!(stats.chunks, 1 + 2 + 1, "{stats:?}");
        let hits = index.search("alpha body", 3);
        assert_eq!(hits[0].id, "docs/a.md#alpha");
    }

    // Behavior: a markdown file past the size cap is still INDEXED, just via the paragraph
    // path — the cap (see MAX_MARKDOWN_TREE_BYTES) bounds one file's parse latency and must
    // never make a big file simply vanish from the index.
    #[test]
    fn oversized_markdown_degrades_to_text_but_is_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let big = format!("# Huge\n\n{}\n", "lorem ipsum dolor ".repeat(70000));
        assert!(big.len() > MAX_MARKDOWN_TREE_BYTES);
        fs::write(root.join("big.md"), &big).unwrap();
        fs::write(root.join("small.md"), "# Small\n\nbody\n").unwrap();
        let mut index = LexicalIndex::new();
        let stats = index_project(root, &mut index);
        assert_eq!(stats.files, 2, "{stats:?}");
        assert_eq!(stats.degraded, 1, "only the oversized file degrades: {stats:?}");
        let hits = index.search("lorem ipsum", 3);
        assert!(
            hits.iter().any(|h| h.id.starts_with("big.md#p")),
            "oversized file must still be searchable, via paragraph ids: {:?}",
            hits.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
    }

    // Persistence round-trip + the search_documents backing.
    #[test]
    fn persisted_index_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("n.md"), "# Notes\nresidency admission queue design\n").unwrap();
        let (stats, file) = index_and_save(root).unwrap();
        assert_eq!(stats.files, 1);
        assert!(file.exists());
        let index = load_project_index(root).expect("index loads");
        let hits = index.search("residency admission", 2);
        assert_eq!(hits[0].id, "n.md#notes");
        assert!(project_retrieval_tools(root).is_some());
        assert!(project_retrieval_tools(&root.join("nowhere")).is_none());
    }

    /// The correctness gate for the whole incremental path, and the one that matters most: an
    /// incremental pass must produce the SAME index a full build would. A cache that is merely
    /// fast is worthless; the risk here is that it is fast and quietly different.
    #[test]
    fn incremental_matches_a_full_build_after_edit_add_and_delete() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.md"), "# Alpha\nresidency admission queue\n").unwrap();
        fs::write(root.join("b.md"), "# Beta\nsomething else entirely\n").unwrap();
        fs::write(root.join("gone.md"), "# Gone\ntemporary content\n").unwrap();
        index_and_save(root).unwrap();

        // Edit one, add one, delete one — the three things that happen to a working tree.
        // mtime has SECOND resolution, so a same-second rewrite is exactly the case the `len`
        // half of the pair exists for; the edit here deliberately changes the length.
        fs::write(root.join("a.md"), "# Alpha\nresidency admission queue, revised and longer\n")
            .unwrap();
        fs::write(root.join("c.md"), "# Gamma\nbrand new file\n").unwrap();
        fs::remove_file(root.join("gone.md")).unwrap();

        let (inc, _) = reindex_incremental(root, &mut |_, _, _| {}).unwrap();
        assert_eq!(inc.removed, 1, "the deleted file's entry is dropped");
        assert!(inc.rechunked >= 2, "edited + added were re-parsed: {inc:?}");
        assert!(inc.reused >= 1, "the untouched file was NOT re-parsed: {inc:?}");

        let incremental: Vec<(String, String)> = {
            let ix = load_manifest(root);
            let mut v: Vec<(String, String)> = ix
                .values()
                .flat_map(|f| f.chunks.iter().map(|c| (c.id.clone(), c.text.clone())))
                .collect();
            v.sort();
            v
        };
        // Now the same tree from scratch.
        fs::remove_file(index_path(root)).unwrap();
        index_and_save(root).unwrap();
        let full: Vec<(String, String)> = {
            let ix = load_manifest(root);
            let mut v: Vec<(String, String)> = ix
                .values()
                .flat_map(|f| f.chunks.iter().map(|c| (c.id.clone(), c.text.clone())))
                .collect();
            v.sort();
            v
        };
        assert_eq!(incremental, full, "an incremental index must equal a full one");
        assert!(
            incremental.iter().all(|(id, _)| !id.starts_with("gone.md")),
            "no chunk may survive its file: {incremental:?}"
        );
        assert!(incremental.iter().any(|(id, _)| id.starts_with("c.md")), "the new file is in");
    }

    /// Several agents commonly start in one project at once. The build lock is what stops each
    /// of them running the same full build — N× the CPU for one identical file, on a machine
    /// already contended by local models. A blocked caller must SKIP, not queue: by the time the
    /// holder is done the answer is on disk for everyone.
    ///
    /// The two branches are asserted on SEPARATE trees rather than by releasing the lock and
    /// re-taking it. Released-then-reacquired was tried and is flaky here: under the suite's
    /// thread parallelism a fresh descriptor still saw `WouldBlock` on a unique tempdir path
    /// after the only holder had been dropped (about one run in three, always passing when run
    /// alone). The property that matters is cross-PROCESS and both halves of it are covered
    /// below; an in-process release/reacquire dance is not, and asserting it bought a flaky
    /// test instead of a stronger guarantee.
    #[test]
    fn a_concurrent_builder_skips_instead_of_repeating_the_work() {
        // Branch 1: someone else holds it → skip, and write nothing.
        let busy = tempfile::tempdir().unwrap();
        fs::write(busy.path().join("a.md"), "# Alpha\ncontent\n").unwrap();
        fs::create_dir_all(busy.path().join(".rozum")).unwrap();
        let held = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(busy.path().join(".rozum").join("rag-index.lock"))
            .unwrap();
        held.lock().unwrap();
        let out = refresh_in_background(busy.path(), &mut |_, _, _| {}).unwrap();
        assert!(out.is_none(), "a second builder must skip, not build");
        assert!(!index_path(busy.path()).exists(), "and must not have written anything");
        drop(held);

        // Branch 2: nothing holds it → build.
        let free = tempfile::tempdir().unwrap();
        fs::write(free.path().join("a.md"), "# Alpha\ncontent\n").unwrap();
        let out = refresh_in_background(free.path(), &mut |_, _, _| {}).unwrap();
        assert!(out.is_some(), "an unlocked project builds");
        assert!(index_path(free.path()).exists());
    }

    /// A no-change pass must not rewrite the file. The MCP proxy holds the index in memory and
    /// reloads when the mtime moves, so an idempotent-content rewrite would make every search
    /// re-read the whole index — the freshness check would cost more than the search.
    #[test]
    fn an_unchanged_tree_leaves_the_index_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.md"), "# Alpha\nstable content\n").unwrap();
        let (_, file) = index_and_save(root).unwrap();
        let before = fs::metadata(&file).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let (stats, _) = reindex_incremental(root, &mut |_, _, _| {}).unwrap();
        assert_eq!((stats.rechunked, stats.removed), (0, 0), "nothing changed: {stats:?}");
        let after = fs::metadata(&file).unwrap().modified().unwrap();
        assert_eq!(before, after, "a no-op pass must not touch the file");
    }

    /// A v1 index (flat chunk list, no per-file stat) must still SERVE searches, and must fall
    /// back to a full rebuild rather than treating "no manifest" as "nothing changed" — which
    /// would freeze the index at its v1 contents forever.
    #[test]
    fn a_v1_index_still_loads_and_upgrades_by_rebuilding() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join(".rozum")).unwrap();
        let v1 = serde_json::json!({
            "version": 1,
            "generated_utc": "2026-01-01T00:00:00Z",
            "chunks": [{ "id": "old.md#s", "text": "legacy chunk text" }]
        });
        fs::write(index_path(root), serde_json::to_vec(&v1).unwrap()).unwrap();
        let ix = load_project_index(root).expect("a v1 index still loads");
        assert_eq!(ix.search("legacy chunk", 1)[0].id, "old.md#s");

        fs::write(root.join("real.md"), "# Real\nactual project content\n").unwrap();
        let (stats, _) = reindex_incremental(root, &mut |_, _, _| {}).unwrap();
        assert_eq!(stats.reused, 0, "a v1 file carries no stat, so nothing may be reused");
        let after = load_project_index(root).unwrap();
        assert_eq!(after.search("actual project", 1)[0].id, "real.md#real");
    }

    // End-to-end smoke over THIS repo's own specs: section-sized chunks make the
    // residency/admission docs the top hit for their own vocabulary.
    #[test]
    /// The retrieval-quality floor, over `tests/rag-eval.json` — 20 questions phrased the way an
    /// agent asks when it does NOT know the symbol, each answered by a specific chunk of this
    /// repo.
    ///
    /// The set has TWO bands. The first twenty ask about answers that were either already top-5
    /// or beyond reach, and could therefore not see a ranking change at all — slot composition
    /// improving from 54 to 80 of 100 shown chunks registered as no movement whatever. The second
    /// six ask about answers that sat at ranks 2–29, and they do see it: over the full set, raw
    /// BM25 scores 4/26 top-1 against 8/26 with the slot policy. A metric blind to the change it
    /// exists to judge is worse than none, because it reads as evidence that nothing happened.
    ///
    /// A floor rather than an exact score: the corpus is this repository, so the number moves
    /// whenever anyone writes a file, and pinning it exactly would make every unrelated commit
    /// fail. What must not happen silently is REGRESSION — the two changes measured here (indexing
    /// the chunk identifier as a boosted field, and reserving most of `k` for code) took top-1
    /// from 3/20 to 8/20, and a later change that quietly undid them would otherwise show up as
    /// nothing at all.
    ///
    /// top-5 is deliberately NOT gated high — but NOT for the reason first written here. That
    /// version said the missing answers "score zero"; measured at k=200 they rank 6, 7, 15, 24,
    /// 36, 58, 79 and 95, and exactly one of twenty is genuinely absent. The ceiling is RANKING,
    /// not vocabulary, and stemming was tried on the wrong story and made top-1 worse (8 -> 6).
    /// See the correction in `docs/specs/rag-code-retrieval-quality.md`.
    ///
    /// Hits from the eval file itself and from that spec are EXCLUDED here: both are indexed and
    /// quote these questions verbatim, so both rank first for them. A gate that scores itself is
    /// not a gate.
    /// IGNORED BY DEFAULT, and that is a cost, not a preference: it indexes the whole repository,
    /// which is ~22 s in release and 107 s in the debug profile tests build with — five times the
    /// entire unit suite. Left enabled it would be disabled by whoever hit it next, and a gate
    /// somebody switched off is worse than one that announces its price. Run it when touching
    /// chunking, ranking or selection:
    ///
    /// ```text
    /// cargo test -p rozum-agent --lib code_retrieval_meets_its_measured_floor -- --ignored
    /// ```
    #[test]
    #[ignore = "indexes the whole repo: ~107 s in the debug test profile"]
    fn code_retrieval_meets_its_measured_floor() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let eval = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/rag-eval.json");
        if !root.join("crates").is_dir() || !eval.is_file() {
            return; // packaged build without the source tree
        }
        let questions: Vec<(String, String)> = {
            let v: serde_json::Value =
                serde_json::from_slice(&fs::read(&eval).unwrap()).expect("eval set parses");
            v["questions"]
                .as_array()
                .expect("questions array")
                .iter()
                .map(|q| {
                    (
                        q["q"].as_str().unwrap_or_default().to_string(),
                        q["answer"].as_str().unwrap_or_default().to_string(),
                    )
                })
                .collect()
        };
        assert!(questions.len() >= 26, "the set must not shrink silently");

        let mut index = LexicalIndex::new();
        index_project(&root, &mut index);
        let mut top1 = 0;
        let mut missed: Vec<&str> = Vec::new();
        // The set and its spec quote every question verbatim and are part of the corpus.
        let self_referential =
            |id: &str| id.contains("rag-eval.json") || id.contains("rag-code-retrieval-quality");
        for (q, answer) in &questions {
            let hits: Vec<_> = crate::rag_lite::search_balanced(&index, q, 12)
                .into_iter()
                .filter(|h| !self_referential(&h.id))
                .take(5)
                .collect();
            if hits.first().is_some_and(|h| h.id.contains(answer.as_str())) {
                top1 += 1;
            } else if !hits.iter().any(|h| h.id.contains(answer.as_str())) {
                missed.push(answer);
            }
        }
        eprintln!("rag eval: top-1 {top1}/{}, absent from top-5: {missed:?}", questions.len());
        assert!(
            top1 >= 6,
            "top-1 fell to {top1}/{} (8/26 when measured; raw BM25 over the same set is 4/26). \
             Something undid the identifier field or the implementation slots.",
            questions.len()
        );
    }

    fn e2e_smoke_own_docs() {
        let specs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/specs");
        if !specs.is_dir() {
            return; // packaged builds without the docs tree
        }
        let mut index = LexicalIndex::new();
        let t0 = std::time::Instant::now();
        let stats = index_project(&specs, &mut index);
        eprintln!("e2e: {stats:?} in {:?}", t0.elapsed());
        assert!(stats.files > 50, "{stats:?}");
        assert!(stats.chunks > stats.files, "sections, not whole files: {stats:?}");
        let hits = index.search("residency admission queue", 5);
        assert!(!hits.is_empty());
        // Top-3, not top-1: docs/specs shares this vocabulary widely (elastic-context,
        // concurrency-abstraction and the cascade all discuss admission), and pinning BM25's
        // exact tie-breaking to one file is a flaky assertion about the corpus, not the code.
        assert!(
            hits.iter().take(3).any(|h| h.id.contains("residency") || h.id.contains("admission")),
            "no residency/admission doc chunk in the top 3: {:?}",
            hits.iter().map(|h| &h.id).collect::<Vec<_>>()
        );
    }
}
