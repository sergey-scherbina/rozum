# Syntactic RAG (phase 1: uniML markdown chunker → rag-lite)

## Overview

Full-project RAG whose chunker is SYNTACTIC: documents are split along their parse-tree
structure, not by byte windows. The tree builder is **uniML compiled via ssc→Rust** (the
operator's binding "path A" decision: no JVM anywhere — not at rozum build time, not at
runtime; `syn` was proposed and rejected because uniML's one tree model covers code and
English/Russian prose alike). Phase 1 wires the already-working markdown dialect (plus a
plain-text fallback) into rozum's existing `rag_lite` (BM25 `LexicalIndex` behind the
`Retriever` trait, exposed as the `search_documents` agent tool): project docs — the
`*.md` mass of BUGS/specs/AGENTS in any repo — become heading-bounded syntactic chunks
that BM25 can actually rank, instead of being invisible or one giant blob.

Phases 2–3 (a Rust-ish uniML dialect for code; embeddings behind `Retriever`) are separate
BACKLOG items, out of scope here.

## Interface

- **Vendored crate `crates/uniml-md/`** — the `ssc-tools emit-rust` output of
  `uniml core+markup+markdown`, committed as generated Rust (package renamed from
  `ssc_program` to `uniml-md`), building with plain cargo. Public surface used by rozum:
  `Markdown_parse(SourceInput, MarkdownProfile, MarkdownLimits) -> ParseResult` and
  `Markdown_project(ParseResult, MarkdownProfile) -> MarkdownProjectionResult`
  (→ `MarkdownDocument { blocks: Vec<MarkdownBlock> }`).
- **`scripts/regen-uniml-md.sh`** — regenerates the crate from a scalascript checkout
  (`SCALASCRIPT_DIR`, default `../scalascript`): re-merges the uniML sources, runs
  `SSC_NO_BUILD_CHECK=1 ssc-tools emit-rust`, rewrites `crates/uniml-md/src/`, patches the
  crate name, and fails loudly if the result does not `cargo build` clean. Regeneration is
  an explicit dev action; rozum's own build never invokes ssc or a JVM.
- **`rozum_agent::rag_chunk` (new module)**:
  - `chunk_markdown(path: &str, text: &str) -> Vec<Chunk>` — parse with the Gfm profile,
    walk `MarkdownDocument.blocks`, emit one `Chunk` per heading-bounded section (the
    heading plus everything until the next heading of the same-or-higher level); a
    document with no headings is one chunk. `Chunk { id, text }` where `id` is
    `"<path>#<heading-slug-or-ordinal>"`.
  - `chunk_text(path: &str, text: &str) -> Vec<Chunk>` — the fallback for any non-`.md`
    file: paragraph-split (blank-line runs), same `Chunk` shape. (True uniML `Literal`
    lossless trees add nothing over this for retrieval; revisit only if a consumer needs
    offsets.)
  - `index_project(root: &Path, index: &mut LexicalIndex) -> IndexStats` — walk `root`
    honoring `.gitignore`-style basics (skip `.git`, `target`, `node_modules`,
    `.worktrees`, binaries by extension + a UTF-8 sniff), route `.md` →
    `chunk_markdown`, other text files → `chunk_text`, `LexicalIndex::add` each chunk.
- **CLI**: `rozum rag index [--root <dir>]` builds/refreshes the index for a project
  directory (default: cwd) and persists it under the project's state dir;
  `rozum rag search <query> [-k N]` queries it (thin wrapper over `Retriever::search`,
  same output the tool sees). The existing `search_documents` agent tool gains the
  persisted project index as its default backing when present.

## Behavior

- [ ] `crates/uniml-md` builds with plain `cargo build` — no ssc, no JVM, no network.
- [ ] `chunk_markdown` on a doc with `#`/`##` headings yields one chunk per section,
      each chunk's text containing its heading line and its body, none of the next
      section's; a headingless doc yields exactly one chunk.
- [ ] Fenced code blocks stay INSIDE their section's chunk (never split mid-fence), and
      a `#` inside a fence is not a section boundary — this is precisely what the
      syntactic tree buys over regex splitting.
- [ ] `chunk_text` splits on blank-line runs; CRLF input does not produce empty chunks.
- [ ] `index_project` over rozum's own repo indexes `*.md` + `*.rs` + plain text, skips
      `.git`/`target`/binaries, and reports counts (files, chunks) in `IndexStats`.
- [ ] `rozum rag index && rozum rag search "residency admission"` (in this repo) returns
      hits whose top result is a chunk from the residency/admission docs — an end-to-end
      smoke that ranking sees section-sized chunks.
- [ ] A malformed/hostile markdown file (uniML `diagnostics` non-empty or `document:
      None`) falls back to `chunk_text` for that file — indexing never fails the run.
- [ ] `regen-uniml-md.sh` run against the scalascript checkout reproduces the vendored
      crate byte-identically (modulo the recorded source SHA header) and `cargo build`s.

## Out of scope

- Chunking CODE by syntax (fn/impl boundaries) — phase 2 (`rag-syntactic-rust-dialect`).
- Embeddings/semantic ranking — phase 3 (`rag-embeddings-backend`); `Retriever` is the seam.
- Incremental/watch reindexing; index persistence format stability guarantees.
- xml/json/yaml dialect chunkers (compile clean already; wire when a consumer exists).

## Design

Vendor-generated-code, not build-time generation: the emitted crate is plain Rust with no
deps beyond std; committing it keeps rozum's build hermetic (path A's whole point), and
`regen-uniml-md.sh` + the byte-identity behavior check keep it honest against drift.
Chunk granularity is the heading section because that is the unit a human links to and
the size (102–103 tokens) BM25 ranks well at; block-level (paragraph) granularity loses
context, file-level defeats the purpose. `Chunk.id` doubles as a human-usable citation
(`path#heading`), which `search_documents` returns verbatim in `Hit.id`.

## Decisions

- **uniML over `syn`/`pulldown-cmark`** — operator decision, in-session, binding: one tree
  model for code AND prose, and the exercise itself hardens ssc→Rust (it already did:
  the whole backend-hardening campaign was this task's prerequisite). Rejected: `syn`
  (Rust-only, blind to docs), `pulldown-cmark` (second parser to trust, no path to the
  unified multi-dialect tree).
- **Vendored generated crate over ssc-at-build-time** — hermetic rozum build, no JVM
  (path A); regen script + byte-identity check instead of a build-time toolchain dep.
- **Heading-bounded sections over fixed windows** — the syntactic tree makes the correct
  boundary cheap; fences and nested structures stay intact by construction.
- **Persisted per-project index over in-memory-only** — `search_documents` must serve an
  agent session that did not just run the indexer; BM25 index serializes trivially.

## Results

(to fill at verify: chunk/file counts on rozum's own repo, index size, search smoke
output, regen byte-identity confirmation)
