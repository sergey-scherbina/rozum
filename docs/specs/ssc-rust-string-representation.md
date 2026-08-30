# ScalaScript Rust backend: string representation (O(1) code-unit indexing)

## Overview

ScalaScript guarantees JVM/JS string semantics: `length` and `charAt` count **UTF-16 code
units** (`"a😀b".length` is 4, not 3 — measured on both reference lanes, and uniML depends
on it: it has `isHighSurrogate`/`isLowSurrogate` and pairs them by hand). The Rust backend
represents a string as a Rust `String` — UTF-8 — and today emulates that indexing by
WALKING `encode_utf16()`: `_str_length` is O(n), `_str_code_at` is O(i).

So the ordinary way to write a scanner —

```scala
while i < s.length do
  val c = s.charAt(i)
```

— pays O(n) per iteration in BOTH calls, and is O(n²) over the string, entirely inside the
runtime. This is not hypothetical: it is what makes `uniml/markdown`'s parse quadratic
(rozum's `rag-uniml-parser-quadratic`), which caps rozum's RAG indexer at 32 KB per file
and BLOCKS phase 2 (chunking code, where files are routinely larger than docs).

The semantics are load-bearing and stay. The REPRESENTATION is what changes: carry enough
metadata on a string value to answer `length`/`charAt` in O(1), computed once per value
instead of once per call.

## Interface

Phased; phase 1 is the whole win on real input and is much the smaller change.

**Phase 1 — `SscStr` with a cached ASCII flag.** The backend emits `SscStr` where it emits
`String` today:

```rust
pub struct SscStr { s: String, ascii: bool }        // `ascii` computed once, at construction
impl std::ops::Deref for SscStr { type Target = str; /* -> &self.s */ }
impl From<String> for SscStr; impl From<&str> for SscStr;
impl Display, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize?/Deserialize?
```

- `_str_length(s)` → `if s.ascii { s.len() } else { s.encode_utf16().count() }` — O(1) on
  the ASCII path.
- `_str_code_at(s, i)` → `if s.ascii { s.as_bytes()[i] }` — O(1) on the ASCII path.
- Non-ASCII keeps today's walk. Correct, and no slower than now.

`Deref<Target = str>` is what keeps the blast radius small: `format!`, comparisons, `&str`
arguments, `.contains`, file I/O and every other `str` method keep working untouched.

**Phase 2 — lazy UTF-16 index for non-ASCII.** Add `utf16: OnceCell<Vec<u16>>` (or a
code-unit→byte offset table) filled on first indexed access, making non-ASCII strings O(1)
after one O(n) touch. Only worth doing if a real workload turns out to be non-ASCII-heavy;
phase 1 already covers source code and the overwhelming majority of documentation.

**Orthogonal, already filed separately** (`rag-uniml-hoist-pure-length`): hoist a pure
`_str_length(s)` out of a `while` CONDITION when `s` is not reassigned in the loop. Cheap,
needs no representation decision, and removes one of the two O(n)-per-iteration terms even
before any of this lands.

## Behavior

- [ ] `"a😀b".length == 4` and `"a😀b".charAt(1)`/`charAt(2)` return the two surrogate halves
      — the JVM/JS semantics are unchanged by the new representation (this is the test that
      would catch "optimised into UTF-8 semantics by accident").
- [ ] An all-ASCII string answers `length` and `charAt` in O(1): a scan of an N-byte ASCII
      string performs O(N) total work, asserted as a RATIO across sizes, not a wall-clock
      threshold (2×, 4×, 8× input ⇒ ~2×, 4×, 8× time, not 4×, 16×, 64×).
- [ ] A string that is ASCII except near its end still takes the fast path for every index
      before the non-ASCII byte (today's prefix check already behaves this way; the flag
      must not regress it into an all-or-nothing decision).
- [ ] `SscStr` round-trips through struct fields, enum variants, `HashMap` keys, `format!`,
      `println!`, file I/O and the `Value` boundary with no call-site changes beyond the
      type name.
- [ ] All four uniML corpora (markdown/xml/json/yaml) emit and `cargo build` clean, and
      `backendRust/test` stays green.
- [ ] `uniml/markdown` parse becomes LINEAR in file size on ASCII input — the acceptance
      test for the whole exercise, and the thing that unblocks rozum's phase 2. Recorded as
      a before/after table in Results.

## Out of scope

- Changing ScalaScript's string semantics (code units stay code units) — that is a language
  decision, not a backend one, and uniML's surrogate handling depends on the current answer.
- Storing strings as UTF-16 the way the JVM does: every `println`, file write, regex and
  Rust-library call would need a conversion, which is the wrong trade for a native backend.
- The JS/interpreter lanes — they have the JVM's own representation and none of this problem.
- Interning or deduplicating string values.

## Design

**Why a newtype and not a side cache.** The obvious cheap idea — memoise the UTF-16
expansion in a thread-local keyed by `(ptr, len)` — is UNSOUND and must not be attempted:
after a `String` is dropped, another can be allocated at the same address with the same
length (ABA), and the cache would serve one string's data for another. There is no hook to
invalidate on drop. Metadata has to live WITH the value, which means the value's type
changes.

**Why this is a smaller change than it looks.** The backend already has this exact pattern:
`SscChar(pub i64)` is a newtype over a primitive with its own `Display`, introduced for the
same reason (JVM char semantics do not match a Rust primitive). `Deref<Target = str>` makes
`SscStr` transparent for reads, so the mechanical work is the type name at emission sites
plus the trait impls, not a rewrite of string handling.

**Why ASCII-flag-first.** It is one bool, computed with a vectorised `is_ascii`, and it
covers source code and nearly all documentation. The lazy UTF-16 table is strictly more
general and strictly more code; splitting it out keeps phase 1 shippable and measurable on
its own.

## Decisions

- **Metadata on the value (newtype), not in a side cache** — soundness: a pointer-keyed
  cache cannot be invalidated when the string is freed.
- **ASCII flag before lazy UTF-16 table** — the flag is a fraction of the work and covers
  the real corpus; the table is phase 2, gated on a workload that needs it.
- **`Deref<Target = str>`** — keeps `format!`/`&str` call sites unchanged, which is what
  makes the migration mechanical rather than invasive.
- **Semantics unchanged** — code units stay code units; the acceptance test asserts
  `"a😀b".length == 4` precisely so a future optimisation cannot quietly redefine it.
- **Linearity asserted as a RATIO** — a wall-clock threshold would be a flaky test on a
  contended machine; the shape is what is under test, and the shape is machine-independent.

## Results

(to fill at verify: the `uniml/markdown` before/after size-vs-time table showing the
quadratic→linear shape change; `backendRust/test` count; all four corpora clean; the
`MAX_MARKDOWN_TREE_BYTES` value the fix allows in rozum — the cap exists only because of
this defect, and the goal is removing it.)
