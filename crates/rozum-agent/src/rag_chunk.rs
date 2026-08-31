//! Syntactic chunking for rag-lite (docs/specs/syntactic-rag.md, phase 1).
//!
//! Markdown files are split along their PARSE TREE — the vendored `uniml-md` crate (uniML
//! compiled via ssc→Rust, the operator's path-A decision: no JVM at build time or runtime) —
//! into heading-bounded, DISJOINT sections whose text is a byte-exact source slice. Everything
//! else falls back to blank-line paragraphs. Chunks feed [`crate::rag_lite::LexicalIndex`]
//! (BM25) behind the [`crate::rag_lite::Retriever`] seam; the persisted per-project index under
//! `.rozum/rag-index.json` is what lets `search_documents` serve an agent session that did not
//! just run the indexer.

use std::collections::BTreeSet;
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
/// (a tiny `maxBlocks` makes uniML halt with a diagnostic on an ordinary document; fabricating
/// a document that ordinary limits refuse would need megabytes).
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
/// Re-measured 2026-08-31 against the regenerated crate, on this repo's own largest documents and
/// on synthetic worst cases, best of 3:
///
/// ```text
///   SPRINT.md      503 KB   0.8% non-ASCII   3.244 s
///   CHANGELOG.md   451 KB   0.5% non-ASCII   1.096 s
///   BACKLOG.md     142 KB   0.5% non-ASCII   0.564 s
///
///   Cyrillic  83% non-ASCII      32 KB 0.087   64 KB 0.170   128 KB 0.340   512 KB 1.301
///   emoji     74% non-ASCII      32 KB 0.204   64 KB 0.402   128 KB 0.814   512 KB 3.193
/// ```
///
/// Every doubling costs ~2× (×1.93–2.02 across both non-ASCII series), so cost is now predictable
/// at roughly 6.5 s/MB in the worst case measured — emoji-dense text and, separately, ASCII
/// SPRINT.md, which land at the same rate for different reasons.
///
/// WHY A CAP AT ALL, STILL: not super-linearity any more — that is fixed — but LATENCY. A linear
/// parser still owes ~6.5 s for a 1 MB file, and [`MAX_FILE_BYTES`] admits documents up to 4 MB, so
/// without a cap one file could hold indexing for half a minute. 1 MB sits above every document
/// anyone here writes (the largest is SPRINT.md at 503 KB) while bounding a single file's tree
/// parse. Raise it only with a measurement, not an estimate: the last two raises on synthetic
/// reasoning both had to be reverted.
pub const MAX_MARKDOWN_TREE_BYTES: usize = 1024 * 1024;

/// Files larger than this are skipped outright — a multi-megabyte blob is generated output or
/// data, and one such file would dominate BM25's length statistics.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

/// Walk `root`, chunk every text file (`.md` syntactically, the rest by paragraphs), feed
/// `index`. Never fails the run on a bad file — that file is counted in `skipped`.
pub fn index_project(root: &Path, index: &mut LexicalIndex) -> IndexStats {
    let mut stats = IndexStats::default();
    let mut chunks: Vec<Chunk> = Vec::new();
    collect_project_chunks(root, root, &mut chunks, &mut stats, &mut |_, _, _| {});
    for c in &chunks {
        index.add(c.id.clone(), c.text.clone());
    }
    stats.chunks = chunks.len();
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
    let mut stats = IndexStats::default();
    let mut chunks: Vec<Chunk> = Vec::new();
    collect_project_chunks(root, root, &mut chunks, &mut stats, progress);
    stats.chunks = chunks.len();
    (chunks, stats)
}

fn collect_project_chunks(
    root: &Path,
    dir: &Path,
    out: &mut Vec<Chunk>,
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
                collect_project_chunks(root, &path, out, stats, progress);
            }
            continue;
        }
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
        if BINARY_EXT.contains(&ext.as_str()) || meta.len() > MAX_FILE_BYTES {
            stats.skipped += 1;
            continue;
        }
        let bytes = match fs::read(&path) {
            Ok(b) => b,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        let text = match String::from_utf8(bytes) {
            Ok(t) => t,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().to_string();
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
        if !file_chunks.is_empty() {
            stats.files += 1;
            out.extend(file_chunks);
        }
    }
}

// ─── Persistence ───────────────────────────────────────────────────────────────

/// On-disk shape of the per-project index: the CHUNKS, not the BM25 tables — rebuild on load
/// is cheap and the format stays independent of `LexicalIndex` internals (which the embedding
/// backend of phase 3 will replace anyway).
#[derive(Serialize, Deserialize)]
struct SavedIndex {
    version: u32,
    generated_utc: String,
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
    let (chunks, stats) = project_chunks_with_progress(root, progress);
    let file = index_path(root);
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir)?;
    }
    let saved = SavedIndex {
        version: 1,
        generated_utc: chrono::Utc::now().to_rfc3339(),
        chunks,
    };
    fs::write(&file, serde_json::to_vec(&saved)?)?;
    Ok((stats, file))
}

/// Load the persisted index for `root`, rebuilding the BM25 tables. `None` when no index has
/// been built (callers fall back to whatever they did before).
pub fn load_project_index(root: &Path) -> Option<LexicalIndex> {
    let bytes = fs::read(index_path(root)).ok()?;
    let saved: SavedIndex = serde_json::from_slice(&bytes).ok()?;
    let mut index = LexicalIndex::new();
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

    // End-to-end smoke over THIS repo's own specs: section-sized chunks make the
    // residency/admission docs the top hit for their own vocabulary.
    #[test]
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
