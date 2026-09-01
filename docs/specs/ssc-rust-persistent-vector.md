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

### Phase 1 — measured 2026-08-31 (load 3–5, quiet machine)

First, the **operation mix**, counted in the emitted parser rather than assumed:

| op | sites |
|---|---|
| `.clone()` | **1378** |
| `.iter()` | 119 |
| `.len()` | 80 |
| `.push()` | 95 |
| index read | 48 |
| `.extend()` | 11 |

That alone reframes the problem: the dominant operation is not append or slice, it is the
backend's DEFENSIVE CLONING (`cloneIfMoved` and the by-value calling convention). Which is
also what the parser profile showed — `String::clone`, `Vec::clone`, malloc, free.

**Accumulate + share** (`n` elements of a two-`String` struct; ×N = growth per doubling):

| clones/elem | `Vec` | `Rc<Vec>` CoW | `im::Vector` |
|---|---|---|---|
| 0 | ×1.7 | ×2.0 | ×2.1 |
| 1 | **×4.0** | ×2.0 | ×2.0 |
| 4 | **×4.0** | ×2.0 | ×2.0 |

`Vec` reproduces the parser's own ×3.9 curve exactly, from clone volume alone. Both
alternatives are linear, and neither costs anything when nothing is cloned.

**Adversarial — the clone is RETAINED across the next mutation** (8 live snapshots), which is
what a parser building a TREE does, since nodes hold their children:

| n | `Vec` | `Rc<Vec>` CoW | `im::Vector` |
|---|---|---|---|
| 4000 | ×3.8 | ×4.2 | ×2.0 |
| 16000 | 4.017 s ×4.1 | 4.062 s ×4.1 | **0.018 s ×1.9** |

**This overturned the spec's own a-priori favourite.** CoW is linear only while clones are
transient; the moment one outlives the next mutation `Rc::make_mut` must copy and CoW
degenerates to `Vec` exactly. `im::Vector` is the only candidate that stays linear — and is
200× faster here at n=16000.

**Read-heavy** (build once, 200 full iterations + strided indexing) — where persistence is
supposed to lose, and does:

| n | `Vec` | `im::Vector` | penalty |
|---|---|---|---|
| 4000 | 0.001 s | 0.008 s | 5.9× |
| 16000 | 0.003 s | 0.024 s | 9.0× |
| 64000 | 0.006 s | 0.050 s | 8.0× |

### What phase 1 concludes

`im::Vector` is the only representation that is linear under this workload, at a **6–9×
constant on the read path**. Against an O(n²) it wins overwhelmingly at parser scale, but the
tax is real and would be a regression for read-dominated programs — this lane compiles more
than uniML.

**Which surfaces a third option the spec did not consider, and it now looks better than
either:** make the backend **clone LESS**, rather than make cloning cheap. 1378 clone sites
against 95 pushes is not the source being profligate — it is `cloneIfMoved` being defensive
because it cannot prove the value is dead. Every clone it can prove unnecessary is removed
outright, with no representation change, no dependency, and no read-path tax. The two fixes
already shipped in this series (self-append → `push`, self-extend → `extend`) are exactly
that, and each was worth 2–4× on its own.

**Recommended order, revised by the measurement:**
1. **Reduce clone volume** (new item) — measure how many of the 1378 are provably dead, then
   remove those. Cheapest, no tax, and directly attacks the measured dominant cost.
2. Re-measure. If the curve is linear, this item CLOSES without a representation change.
3. Only if a quadratic remains: adopt `im::Vector`, accepting the read tax, ideally scoped to
   the accumulator shapes rather than every `Vector` in the language.

Phase 2 is therefore NOT started: the measurement says the cheaper option should be tried
first, which is what phase 1 existed to find out.

### Phase 2 pass — 2026-09-01 (the single-frame residual, two quadratics deep)

The re-measure confirmed the residual and then some: a single giant frame is not "mildly"
super-linear but a clean ×3.9/doubling, and holding bytes fixed while varying ONE variable
(the method that has now worked three times) proved cost ∝ (line count)²: 64 KB as 100 long
lines parses in 0.25 s, as 6400 short lines in **29.3 s**. `sample` named the leaves —
`UniNode::clone` + `UniEdge to_vec` under `TreeVm.step`.

**Quadratic #1 — per-token frame rebuild (FIXED, source-level).** `addTop` rebuilt the top
`VmFrame` (copying all its edges) on every token. The hot-top invariant moves the open frame's
edges into `VmState.topEdges` (`scalascript feature/treevm-top-edges-prestage10`, stage-10 twin
on `feature/treevm-top-edges`): the per-token path becomes a plain self-append → O(1) push in
the lowering; the O(frame) copies move to frame open/close. Result: worst case 29.3 s → 12.9 s,
the whole frame series ~1.7× faster, all uniml suites + 164 rozum-agent tests green.

**Quadratic #2 — per-token state-field deep-clone (DIAGNOSED, backend-level).** The curve stays
×3.9 because the generated `step` does `let mut topEdges = state.topEdges.clone()` on entry and
`topEdges: topEdges.clone()` in the returned `VmState` — two deep O(k) clones of the
accumulated edge list PER TOKEN, emitted for an owned parameter's field at what is provably its
last use. That is `cloneIfMoved` conservatism again, one level up: the fix belongs in the
backend (move an owned param's field at its last read), or — the spec's own fallback — a
scoped shared representation for exactly this accumulator field. Filed as the remaining work
of this item; the instrument (`mdbench` + the fixed-bytes/one-variable method) carries over.

Vendored crate regenerated from the pre-stage-10 base (stage-10 currently breaks the Rust lane
outright — 26 emitter refusals fixed on the stage-10 branch, ~86 rustc errors remain for its
owner; recorded in the rozum room 2026-09-01).
