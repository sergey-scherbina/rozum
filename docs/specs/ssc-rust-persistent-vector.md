# ScalaScript Rust backend: persistent `Vector` (removing the O(n²) class)

## Overview

Scala's `Vector` is a PERSISTENT structure: append, slice and "copy" all share
structure, costing O(1)–O(log n). Code written against it copies freely because copying
is not real. uniML is written exactly that way. The ScalaScript Rust backend lowers
`Vector` to `Vec<T>`, where every one of those operations is an O(n) copy — so idiomatic
Scala becomes quadratic, silently, with no wrong answer to point at.

This is not a hypothesis. Six rounds of profiling on `uniml/markdown`'s parse found six
distinct hot spots, and every one was the same shape:

| # | Source idiom | Scala cost | Emitted Rust cost | Fixed by |
|---|---|---|---|---|
| 1 | `xs = xs :+ x` | O(1) | whole-vector copy per append | `Vec::push` |
| 2–4 | `s.charAt(i)` / `s.substring(…)` in a scan | O(1) | O(i), and one case allocated a `Vec<u16>` of the whole string PER CALL | ASCII fast paths |
| 5 | `xs = xs ++ ys` in the per-token loop | O(1) | whole-vector copy per token | `Vec::extend` |
| 6 | `lines.drop(index)` per line | O(log n) | O(n) copy of the tail | scan by index |

Those fixes are real and shipped — cumulatively **~13×** (256 KB parse: 173.4 s → 13.0 s;
32 KB: 2.768 s → 0.230 s) — but the SHAPE never changed: still ~3.9× per doubling. Each
fix only revealed the next instance. Removing them one at a time is an unbounded tail;
the fix is to make the lowering match the semantics the source is written against.

## Interface

Phased, and phase 1 is deliberately the smallest thing that can be measured.

**Phase 1 — measure the candidate, on the real corpus.** Before any lowering changes,
benchmark `uniml/markdown`'s parse with `Vector` represented three ways, on the same
inputs and the same machine:
- today's `Vec<T>` (the baseline, already measured);
- a persistent vector crate (`im::Vector` / `rpds::Vector` — RRB-tree, O(1) amortised
  push, O(log n) slice and clone);
- copy-on-write `Rc<Vec<T>>` (O(1) clone; O(n) only on the first write after sharing).

Deliverable is a table, not a decision taken in advance: persistent structures have a
real constant-factor cost on ITERATION and INDEXING, which this parser does constantly,
and it is entirely possible that CoW wins or that neither beats `Vec` below some size.
**If no candidate makes the curve linear without losing more on constants than it saves,
that is a valid outcome and the item closes with the measurement.**

**Phase 2 — lower `Vector` onto the winner.** A type alias in the emitted runtime
(`pub type SscVec<T> = …`) plus the method mapping (`push`/`extend`/`slice`/index), so
the change is one lowering decision rather than a rewrite of every call site.
`Vec<T>` stays the lowering for `Array` (genuinely mutable, genuinely dense).

**Phase 3 — the value-semantics rule.** With cheap sharing available, the backend's
liberal `.clone()` (`cloneIfMoved`, the by-value capture convention) stops being a
performance problem and can be left alone. Confirm that, rather than assuming it.

## Behavior

- [ ] The phase-1 table exists: three representations × the size series, same machine,
      load recorded, ratios (not absolute seconds) as the reported signal.
- [ ] `uniml/markdown` parse is LINEAR in file size on ASCII input under the chosen
      representation — ~2× per doubling, sustained to 1 MB.
- [ ] Semantics unchanged: `backendRust/test` green, all four uniML corpora emit and
      `cargo build` clean, and uniML's own JVM test suites stay green (the source is not
      allowed to change to suit the backend).
- [ ] Iteration-heavy code does not regress: a benchmark that only READS a large
      `Vector` (fold/map/index) is no worse than the `Vec` baseline by more than the
      table's stated margin.
- [ ] `rozum rag index` over `docs/specs` drops proportionally, and
      `MAX_MARKDOWN_TREE_BYTES` can be REMOVED (its whole reason for existing is this
      defect).

## Out of scope

- Changing `Array`'s lowering (`Vec<T>` is correct for it).
- Changing uniML's source to avoid the idiom — that is precisely backwards: the point is
  that idiomatic Scala should not be quadratic. (Targeted source fixes already made where
  they were also algorithmically better, e.g. scanning by index, stay.)
- The string representation (`ssc-rust-string-repr`) — a separate, real, and now
  DEMOTED item: it addresses rounds 2–4, which were 2.8× of the 13×, while `Vector`
  accounts for the rest and for the remaining shape.

## Design

**Why this and not the string newtype.** The string work was picked first on the
assumption that UTF-16-over-UTF-8 indexing was the blocker. The measurements say
otherwise: string fixes bought 2.8×, `Vector` fixes bought the rest, and the two
remaining profile leaders after all of them are both `Vector` copies. The string newtype
also carries a risk this does not: `"String"` is the backend's INFERENCE KEY at 19 sites
(a probe produced 27 emitter refusals), whereas `Vector`'s lowering is a type alias plus
a method mapping and does not participate in inference.

**Why measure before choosing.** A persistent vector is not free: RRB indexing is
O(log n) with a worse constant than a flat `Vec`, and this parser indexes and iterates
heavily. CoW `Rc<Vec<T>>` keeps flat iteration and makes only SHARING cheap — which,
looking at the six findings, is most of what is actually needed (append and share, not
random-access slicing). That makes CoW the a-priori favourite and exactly the reason to
measure rather than reach for the more sophisticated structure.

## Decisions

- **Fix the lowering, not the source** — the defect is that idiomatic Scala is quadratic
  here; rewriting uniML to suit the backend would hide it and would have to be repeated
  for every program this lane compiles.
- **Measure three representations first** — the phase-1 table is the deliverable; picking
  a structure before measuring is how the string hypothesis cost a round.
- **Demote `ssc-rust-string-repr`** — real, but 2.8× of 13× and the riskier change.
- **`Array` keeps `Vec<T>`** — it is a mutable dense array and nothing here applies.

## Results

(to fill at phase 1: the three-way table with load context; at phase 2: the linearity
series to 1 MB, corpora + test status, and whether `MAX_MARKDOWN_TREE_BYTES` is gone.)
