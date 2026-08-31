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

- [ ] Lossless: for every file in this repo's `crates/`, concatenating the lexer's token
      lexemes reproduces the file byte for byte.
- [ ] A `fn` with a doc comment and attributes yields ONE chunk containing all three, and
      the next item's chunk starts after it — the sections are disjoint, as in phase 1.
- [ ] A brace inside a string, char, or comment does NOT open or close an item
      (`"{"`, `'}'`, `// }`) — this is the whole reason for a lexer rather than a regex.
- [ ] Nested block comments (`/* /* */ */`) and raw strings with hashes (`r#"…"#`)
      terminate correctly.
- [ ] `'a` in `fn f<'a>(x: &'a str)` lexes as a lifetime, not an unterminated char.
- [ ] Methods inside an `impl` are their own chunks; the `impl` header is the preamble of
      the first one rather than a chunk that duplicates all of them.
- [ ] `chunk_code` on a file with no items (a `mod.rs` of `pub mod` lines) yields one
      chunk, not zero.
- [ ] `index_project` over this repo indexes `.rs` files as items, and
      `rozum rag search "residency admission"` still puts the residency docs first —
      code chunks join the index without drowning the prose.
- [ ] A syntactically broken `.rs` file (unbalanced braces) falls back to `chunk_text`
      and never fails the run.
- [ ] uniML's own JVM suites stay green, all four existing corpora still emit and
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

(to fill at verify: losslessness over `crates/`, chunk counts and sizes for the repo's
own Rust, search smoke output, and the `index_project` timing with `.rs` included.)
