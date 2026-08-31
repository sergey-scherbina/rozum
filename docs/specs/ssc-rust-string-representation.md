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

**~~Why this is a smaller change than it looks.~~ MEASURED 2026-08-31: it is BIGGER than it
looks, and this paragraph was wrong.** The original reasoning was that `SscChar(pub i64)` is
the same pattern, and that `Deref<Target = str>` makes `SscStr` transparent for reads, so the
work is "the type name at emission sites plus the trait impls". A probe disproved it: defining
a minimal `SscStr` and flipping `mapType`'s one `String` line produced **27 emitter refusals on
the markdown corpus alone, before a single line of Rust was compiled** — `def flatten reads
isEmpty without parentheses … it is a collection member, not a field`, and 26 more of that shape.

The reason is structural. The emitted type name `"String"` is not just an output; it is the
**inference key** the backend uses to decide what a receiver IS. `RustCodeWalk.scala` mentions
the literal `"String"` 57 times, 19 of them as a direct equality/`case` test driving decisions
like no-paren `.isEmpty`/`.nonEmpty` lowering, `isKnownStringField`, and string-vs-collection
dispatch. Changing what `mapType` returns silently rewires all of that.

So phase 1 is not "rename the type at emission sites". It is: introduce the newtype, AND migrate
every one of those inference sites to a predicate that knows both spellings, AND convert the
~40 `to_string()` / ~31 `format!` construction sites plus ~20 runtime signatures, AND iterate
the four corpora to green. That is a multi-session change with a real chance of subtle breakage,
and it should be planned as one — not started as a cleanup.

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

**Partial — the constant is fixed, the shape is not. Phase 1 is NOT done.**

Profiling before implementing (the lesson from the previous round, where the filed hypothesis
turned out to be 11% of its problem) moved the target: the dominant cost was **not** the index
emulation this spec was written about. `MdLine.split` was **90% of the whole markdown parse**,
and inside it the cost was `_str_substring` **materialising a `Vec<u16>` of the entire string on
every call** — while `split` calls it once *per character* (`text.substring(i, i+1)` to take one
char). An O(n) allocation per character, with the allocator in the inner loop.

Four sibling helpers had the same shape and are called once per scan position by the same kind
of scanner code: `_str_substring_from`, `_str_starts_with_at`, `_str_index_of_from`,
`_str_region_matches`. All five now take an ASCII fast path — when the relevant *prefix* is
ASCII a byte index IS the code-unit index, so the answer is a direct byte slice or byte
comparison, with no allocation and no walk of the whole string. General paths, and their panic
messages, are untouched. (scalascript `1498a5f39`.)

Measured, `uniml/markdown` parsing plain paragraphs, same harness:

| size | before | after | |
|---|---|---|---|
| 4 KB | 0.027 s | 0.012 s | |
| 8 KB | 0.092 s | 0.041 s | |
| 16 KB | 0.355 s | 0.136 s | |
| 32 KB | 1.359 s | 0.523 s | |
| 64 KB | 4.924 s | 2.153 s | |
| 256 KB | — | 32.079 s | |

**2.3× at every size**, on top of the earlier 2.8× — but still ~3.9× per doubling, i.e. still
O(n²). Semantics verified by RUNNING the emitted code, not by inspection: `"a😀b".length == 4`,
`charAt(1)`/`charAt(2)` = 55357/56832 (the surrogate halves), `substring(1,3)` = `"😀"`,
`"aébc".length == 4` / `substring(1,3)` = `"éb"`, `indexOf`/`startsWith` across a multi-byte
prefix, and the out-of-range `startsWith` contract (false, not a panic). 504/504
`backendRust/test`; all four corpora emit and `cargo build` clean; `v1-jit-size` PASS (runtime
template only).

**What still needs the newtype, and why nothing cheaper works.** `split`'s inner loop still
calls `_str_length` (in the loop CONDITION), `_str_char_at` and `_str_substring`, and each
re-scans the prefix to decide ASCII-ness — three O(n) SIMD scans per character. There is no
O(1) sufficient check without metadata stored on the value: `byte index == code-unit index`
holds only if nothing multi-byte precedes the index, and `is_char_boundary(i) && b[i] < 0x80`
is not sufficient (`"é" + "abc"`: byte 2 is `a`, ASCII and a boundary, but its code-unit index
is 1). So the conclusion of this spec stands — the flag must live on the value — but see the
corrected size estimate in Design.

`MAX_MARKDOWN_TREE_BYTES` in rozum therefore stays where it is; removing it is still gated on
the real phase 1.
