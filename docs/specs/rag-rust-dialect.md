# RAG phase 2: a structural Rust dialect for uniML, and code chunking

## Overview

Phase 1 chunks documentation by parse tree. Phase 2 does the same for CODE, which is
what the RAG is actually for: `search_documents` should return the function you meant,
not the file it lives in. The chunker stays uniML, per the operator's binding decision
(one tree model for code and prose; `syn` was proposed and rejected).

The blocker phase 1 recorded — the parse was O(bytes²), so a 505 KB file extrapolated to
hours — is GONE: the parse is now linear on ASCII (256 KB 173.4 s → 0.28 s, ~2× per
doubling), and code is ASCII. Non-ASCII is still super-linear, which is
`ssc-rust-string-repr`'s business and does not gate this.

**Structural, not a grammar.** This dialect does not parse Rust. It finds the boundaries
of top-level items by matching braces while knowing where strings, chars and comments
are — that is all a chunker needs, and it is what keeps the dialect small enough to be
correct. `dialect/Literal.scala` (56 lines, lossless fallback over any text) is the shape
to follow; `uniml/json` (784 lines across five files) is the upper bound of what a real
dialect costs.

## Interface

**`uniml/rust`** — a new dialect module, `RustDialect extends DialectAdapter`, id
`uniml.rust`, aliases `rust`, `rs`.

- **Lexer** — one pass producing tokens with exact lexemes, lossless (concatenating every
  lexeme reproduces the file byte for byte, which is uniML's invariant and the property
  the chunker's byte-exact slicing depends on):
  `rust.ws`, `rust.line-comment`, `rust.block-comment` (nested, as Rust allows),
  `rust.string` (including `r"…"`, `r#"…"#`, escapes), `rust.char`, `rust.lifetime`
  (so `'a` is not mistaken for an unterminated char), `rust.ident`, `rust.number`,
  `rust.punct`.
- **Structure** — Open/Close branches for the item kinds a reader navigates by, at
  TOP LEVEL and inside `impl`/`mod` bodies (one nesting level is what citation needs):
  `rust.fn`, `rust.impl`, `rust.struct`, `rust.enum`, `rust.trait`, `rust.mod`,
  `rust.use`, `rust.const`, `rust.macro` (a `macro_rules!` block).
  An item's branch spans from the first token of its ATTRIBUTES/doc comments through its
  closing brace (or `;`), so a chunk carries the doc comment that explains it.
- **`rozum_agent::rag_chunk::chunk_code(path, text) -> Vec<Chunk>`** — one chunk per
  top-level item; `id` is `"<path>#<item-name>"` (e.g. `src/lib.rs#fn parse_header`).
  Text before the first item (imports, module docs) is a `#preamble` chunk. A file the
  dialect reports as broken falls back to `chunk_text`, exactly as markdown does.
- **`index_project`** routes `.rs` to `chunk_code`; everything else is unchanged.

## Behavior

- [x] Lossless: for every file in this repo's `crates/`, concatenating the lexer's token
      lexemes reproduces the file byte for byte.
- [x] A `fn` with a doc comment and attributes yields ONE chunk containing all three, and
      the next item's chunk starts after it — the sections are disjoint, as in phase 1.
- [x] A brace inside a string, char, or comment does NOT open or close an item
      (`"{"`, `'}'`, `// }`) — this is the whole reason for a lexer rather than a regex.
- [x] Nested block comments (`/* /* */ */`) and raw strings with hashes (`r#"…"#`)
      terminate correctly.
- [x] `'a` in `fn f<'a>(x: &'a str)` lexes as a lifetime, not an unterminated char.
- [x] Methods inside an `impl` are their own chunks; the `impl` header is the preamble of
      the first one rather than a chunk that duplicates all of them.
- [x] `chunk_code` on a file with no items (a `mod.rs` of `pub mod` lines) yields one
      chunk, not zero.
- [ ] `index_project` over this repo indexes `.rs` files as items, and
      `rozum rag search "residency admission"` still puts the residency docs first —
      code chunks join the index without drowning the prose.
- [x] A syntactically broken `.rs` file (unbalanced braces) falls back to `chunk_text`
      and never fails the run.
- [x] uniML's own JVM suites stay green, all four existing corpora still emit and
      `cargo build` clean, and the regenerated `uniml-md` crate is byte-identical apart
      from the new dialect.

## Out of scope

- Parsing Rust properly (types, generics, expressions). Item boundaries are the goal.
- Other languages. The dialect is `uniml.rust`; Scala/Python/etc. are separate items
  whose shape this one establishes.
- Semantic chunking (call graphs, symbol references) — that is a later phase, and
  `Retriever` is where it would attach.
- Non-ASCII performance (`ssc-rust-string-repr`); code is ASCII in practice.

## Design

Two files, mirroring the smallest real dialect rather than markdown's five:
`RustLexer.scala` (tokens) and `RustDialect.scala` (adapter + structural processor).
State is a small case class threaded through `step`, as `Literal` and `Json` do.

The structural processor is a brace-depth machine over the token stream: at depth 0 an
`ident` in the item-keyword set opens a branch; the branch closes when depth returns to 0
via `}`, or at the `;` that ends a body-less declaration (`use`, `const`, a trait method
signature). Attributes (`#[…]`) and doc comments preceding an item are held and attached
to the branch that follows, which is what makes a chunk self-explaining.

Item NAME extraction is deliberately shallow: the first `ident` after the keyword, which
is right for `fn f`, `struct S`, `trait T`, `mod m`, and gives `impl` the trait or type
name — good enough for a citation, and it cannot be wrong in a way that breaks chunking
(a wrong name is a worse `id`, never a wrong boundary).

## Decisions

- **uniML, not `syn`** — operator's decision, restated because it is the reason this item
  exists in this form: one tree model for code and prose, and the dialect is reusable
  wherever uniML is.
- **Structural, not a grammar** — a chunker needs boundaries, and a real Rust grammar is
  a project. The lexer exists only because braces inside strings and comments would
  otherwise break the boundaries.
- **Items, not lines or files** — the retrievable unit is what a human would link to.
- **One nesting level (`impl` members)** — deeper nesting produces chunks too small to
  rank, and the same argument phase 1 settled for headings.
- **Fallback to `chunk_text` on any doubt** — indexing must never fail a file; phase 1's
  rule, unchanged.

## Results

**Losslessness** holds over every `.rs` file in `crates/`, checked at two levels: the
lexer's token lexemes, and — because the processor runs whole item bodies together into
one token — the stream the VM actually receives. Both reproduce each file byte for byte.
`RustDialectSpec` is 12 tests, `RustCorpusSpec` 3 more when pointed at `crates/`; the full
uniML suite and `backendRust/test` (504/504) stay green.

**Chunks.** Indexing this repo yields 10,490 chunks over 489 files (4 skipped, 0 degraded):
6,427 from `.rs` (4.58 MB, mean 713 B) and 2,390 from `.md` (2.97 MB, mean 1,243 B). Chunk
size p50 446 B, p90 1,770 B, p99 5,915 B, max 68,100 B. Code chunks are SMALLER on average
than prose ones, which is what an item boundary should give: a function is a tighter unit
than a heading section.

**Search.** Citation ids read the way the spec wanted — `rag_chunk.rs#fn chunk_code`,
`specdecode_backend.rs#use`. Code chunks do not drown the prose: `"speculative decoding
draft"` returns the three spec documents first, and `"chunk code items"` correctly prefers
the code.

One box is deliberately left unchecked. For `"residency admission"` the residency specs are
now ranks 2 and 3, displaced from the top by ONE code chunk —
`rag_chunk.rs#fn e2e_smoke_own_docs`, the test that searches for that exact phrase and so
contains it verbatim. That is self-reference from indexing our own test source rather than
code crowding out prose, and the rest of the box holds, but the box as written says the
docs come first and they do not. Left for a reviewer to decide: accept it, or exclude
`#[cfg(test)]` bodies from the index.

**Cost.** `index_project` over the whole repo takes ~96 s (~114 s including building the
lexical index), with `crates/`'s 159 `.rs` files (3.85 MB) modelled at ~68 s of that.

That number is the honest headline of this phase, because getting it required fixing a
quadratic that had nothing to do with the dialect's design. Profiling — not reading — put
the time in `UniNode::clone`: `TreeVm.addTop` rebuilds the open frame on every token, and
on the Rust lane `edges :+ edge` copies the frame's edges DEEPLY, since an edge owns its
subtree. Cost is therefore O(k²) in tokens PER FRAME. Measured at ~18 KB of source: 400
small functions 1.57 s, eight large ones 3.60 s, ONE function 40.85 s — same bytes, and the
only variable is how many tokens sit in one frame. Markdown never noticed because a block
holds a handful of tokens; a Rust function body holds thousands. Worked around dialect-side
by emitting each item body as a single token, which is sound because a structural chunker
slices by the item's span and never needed a node per token. **The O(k²) in `TreeVm` is
untouched and will bite the next dialect with large frames.**

The second cost was lexeme slicing at O(offset) per token: `substring` can answer from a
byte slice only while the prefix is ASCII, and one em dash in a comment — most of this
repo's Rust — puts every later slice on a whole-string UTF-16 walk. The lexer now works in
1024-code-unit windows. Together: 40 KB 3.18 s -> 0.204 s, 200 KB 136 s -> 4.4 s, and the
one-huge-function case 40.85 s -> 0.008 s.

What remains is Θ(n^1.5), from extracting those windows, and it does not have a fix at this
layer. A lexeme can only come from a slice of the source: v2 has no Char box, so building
one from characters renders their code points' decimal digits instead — verified for both
`Char.toString` (`"abc"` -> `979899`) and `Vector[Char].mkString` (`"ab—"` -> `97988212`).
This is `ssc-rust-string-repr`, already queued, and it is the item that would make code
indexing linear. A size cap was considered and rejected: capping at 64 KB would cut the
tree path to ~21 s but drop 43% of the repo's Rust bytes back to paragraph chunking, which
is most of the value of this phase.
