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
/// **Raised 16 KB → 64 KB on 2026-08-31, and this time the corpus says yes.** The vendored
/// parser got ~13× faster over a series of fixes in scalascript's Rust backend (self-append →
/// `push`, self-extend → `extend`, ASCII fast paths in the string helpers, `String.toVector`,
/// read-only `Vec` captures by reference, and uniml scanning ref-defs by index instead of
/// copying the tail): 256 KB 173.4 s → 13.0 s.
///
/// Re-measured on this repo's actual `docs/specs`, the same way the earlier attempt to raise
/// this constant was measured — and rejected — before the speedup:
///
///     cap  16 KB   125 files    7.6 s     (was 41.5 s)
///     cap  32 KB   140 files   14.2 s     (was 91.0 s)
///     cap  64 KB   142 files   16.7 s     ← every file, syntactically
///
/// Full syntactic coverage now costs LESS THAN HALF what partial coverage cost before. That is
/// what pays for the cap; the earlier 64 KB attempt was reverted precisely because the number
/// did not, and the rule that caught it stands: against a quadratic cost, take the figure from
/// the corpus, not from a synthetic benchmark.
///
/// The cap still EXISTS because the parser is still O(bytes²) — ~13× is a constant, not a shape.
/// The remaining cost is not one bug: uniML is written in Scala's persistent-immutable idiom
/// where append/slice/share are O(1)–O(log n), and the backend lowers `Vector` to `Vec` where
/// each is an O(n) copy. Six profiling rounds found six instances of that one shape. The
/// architectural fix is `ssc-rust-persistent-vector` in BACKLOG.md; phase 2 (chunking CODE,
/// where files are routinely larger than any doc) is what needs it.
pub const MAX_MARKDOWN_TREE_BYTES: usize = 64 * 1024;

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
        let tree_path = ext == "md" && text.len() <= MAX_MARKDOWN_TREE_BYTES;
        progress(&rel, text.len(), tree_path);
        let file_chunks =
            if tree_path { chunk_markdown(&rel, &text) } else { chunk_text(&rel, &text) };
        if ext == "md" && !tree_path {
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
        assert_eq!(stats.chunks, 1 + 2 + 2, "{stats:?}");
        let hits = index.search("alpha body", 3);
        assert_eq!(hits[0].id, "docs/a.md#alpha");
    }

    // Behavior: a markdown file past the size cap is still INDEXED, just via the paragraph
    // path — the quadratic parser (see MAX_MARKDOWN_TREE_BYTES) must never make a big file
    // simply vanish from the index.
    #[test]
    fn oversized_markdown_degrades_to_text_but_is_indexed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let big = format!("# Huge\n\n{}\n", "lorem ipsum dolor ".repeat(8000));
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
