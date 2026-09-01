# ssc-local-last-use-move — last-use MOVE of locals in the Rust backend

## Overview

Quadratic #3 of `uniml-single-frame-residual-superlinear` (BACKLOG): the generated `step`'s
returned constructor clones every local it hands back — `VmState { stack: stack.clone(),
topEdges: topEdges.clone(), … }` — at the local's provably LAST use, so each token pays two
O(document) copies that buy nothing. The backend (`v1` RustCodeWalk) gets a per-def liveness
table for LOCALS: a bare value read that is the textually last use of a local MOVES instead of
cloning. This generalizes `_ownedFieldMoves` (ssc-owned-field-move, param fields) to locals,
keyed by exact source position rather than by name.

## Interface

No language-surface change. Backend-internal:

- `_localLastUseMoves: Map[String, Map[String, Int]]` — defName → (localName → `pos.start` of
  the one occurrence allowed to move). Populated per def next to `_ownedFieldMoves`.
- `collectLocalLastUses(body: m.Term, params: Set[String]): Map[String, Int]` — the collector.
- One new case in `cloneIfMoved`, before the general `Term.Name` clone case: a bare
  `Term.Name(n)` whose `pos.start` equals the table entry renders WITHOUT `.clone()`, guarded by
  `!ctx.defParams(n) && !ctx.byRefMut(n) && !ctx.inClosure && !ctx.inWhileLoop` and a bare
  `rendered` (no parens).

## Behavior

- [x] A local `val`/`var` read once at the def's tail (returned constructor / call / bare return)
  is MOVED — no `.clone()` — when no later use exists.
- [x] The move is keyed by POSITION: earlier reads of the same local still clone; only the
  max-position occurrence moves.
- [x] Occurrences count as uses wherever they appear: value reads, field projections
  (`n.field`), method receivers (`n.len()`), assignment LHS/RHS. Only a bare value read can BE
  the move; anything else at max position simply means no move fires.
- [x] A local whose max-position occurrence sits inside a `while`/`for` body or a
  lambda/closure is NOT moved (may execute more than once / later than its position).
- [x] A local captured by a lifted local def counts a use at EVERY call site of that def
  (transitively through nested-def calls): a call after the last plain read keeps the clone.
  A lifted def referenced outside call position disqualifies its captures entirely.
- [x] A name declared more than once in the def (shadowing) is disqualified.
- [x] `lazy val`s and def params are excluded (params belong to `_ownedFieldMoves`).
- [x] All existing backend goldens stay green, modulo reviewed expected-text updates where a
  tail clone legitimately disappears — each such diff is a sound move, checked individually.
- [x] Vendored `uniml-md` regenerated from the fixed backend: the `step` exit constructor moves
  `stack`/`topEdges` (verbatim `stack: stack,` in the generated file), rozum tests green.

## Out of scope

- Moving locals whose last use is inside a loop body they are declared in (per-iteration
  freshness — the `loopExempt` shape); left cloning.
- Param-field moves (already shipped as ssc-owned-field-move) and whole-param moves.
- The `deadNames` mechanism: it is an ABSOLUTE clone-suppression with no loop/closure guard,
  so this pass does not piggyback on it; the new table re-checks `inClosure`/`inWhileLoop` at
  the use site.
- stage-10 lane (still broken by its owner's conversion; base remains
  `feature/treevm-top-edges-prestage10`).

## Design

Textual position order over-approximates execution order except in exactly three constructs:
loops (repeat), lambdas (deferred/repeated), and lifted local defs (deferred to call sites).
The collector therefore (a) flags every occurrence with in-loop/in-lambda context, (b) skips
nested-def bodies as direct uses and instead attributes each nested def's transitive capture
set to its call sites, and (c) admits a local only when its max-position occurrence is a plain,
unflagged use. `cloneIfMoved` then moves exactly that occurrence — position equality — and
re-checks the rendering context. Match arms and if/else branches need no special handling: an
occurrence at max textual position in a branch either executes last or not at all, and a move
in a branch not taken never happens.

## Decisions

- **Position-keyed table over extending `deadNames`** — `deadNames` suppression overrides even
  the `inClosure`/`inWhileLoop` clauses of `needs()`, so a name whose tail statement contains a
  loop reading it would be moved on iteration one and used on iteration two. The table gates one
  exact occurrence and re-checks context. Rejected: statement-level dead-set via
  `deadBeforeReassign` plumbing (unsound for compound tails).
- **Call-site attribution for lifted defs over wholesale disqualification** — `step`'s
  `closeFrame`/`pushFrame`/`attachClosed` all capture `stack`/`topEdges`; disqualifying any
  local referenced by a nested def would kill the exact win this pass exists for. Rejected:
  ignoring nested defs (unsound — a call after the last plain read reads a moved-out local).

## Results

**Three backend/source fixes landed under this slug (quadratics #3, #4, #5 of
`uniml-single-frame-residual-superlinear`), scalascript
`feature/treevm-top-edges-prestage10` `86449b44b` + `cf3a60ec8` + `3377ceb77`:**

1. **#3 — locals at last use** (`_localLastUseMoves`, this spec's original scope): the generated
   `step`'s exit constructor now MOVES `stack`/`topEdges`/`nodeCount`/`lastTokenId`/`roots`
   (verbatim `stack: stack,` at the tail). The captured-by-`record` smalls (`diags`, counters,
   `halted`) still clone — `record` is eta-mentioned (`foreach(record)`), which poisons its
   captures by design; they are O(1)-ish per token.
2. **#4 — loop-local fields** (`_localFieldMovePos`, position-keyed): `val stepped =
   vm.step(vmState, token); vmState = stepped.state` in BOTH driver loops of `UniML.parse` no
   longer deep-clones the whole accumulated VmState per token (`vmState = stepped.state;`).
   Name-keying would have seen the two sibling `stepped`s as shadowing — hence positions.
3. **#5 — the line lexer itself** (source-level, like hot-top): `MarkdownLexer.split`'s
   tail-recursive accumulator lowered to a REAL recursion concatenating the whole `lines` vector
   per line (85% of a 12,800-line parse). Imperative `while` + self-append push; JVM semantics
   suite 52/52 unchanged.

**Measured (64 KB as 6400 short lines, min-of-3, same machine window):
10.59 s → 0.232 s — ×45.6.** Ordinary long-line docs unchanged (0.216 s vs 0.211 s).
Cumulative for the campaign: 29.3 s (pre-hot-top) → 0.23 s — ×126.
Backend goldens 512/512 (5 new under this slug); rozum-agent 164/164 against the regenerated
vendored crate.

**The curve is STILL ~×3.7/doubling on this pathological shape** — contributor #6, profiled
exactly: a blank-line-free document is ONE paragraph, and `MarkdownInlines_parse` → `tokenize`
walks that whole content with `_str_code_at`/`_str_substring` (the O(i) JVM-code-unit emulation
over UTF-8; 3277+1543 of ~5000 samples at 25,600 lines). The fix is the one `split` already
documents — index a code-unit vector, not the string — applied to the inline tokenizer and its
helpers (`isExtendedAutolinkStart` et al. take the String by position). Filed in BACKLOG; real
documents have blank lines, so their paragraphs stay small and this tail does not dominate.

**Latent pre-existing gap found while pinning the negative golden:** the `for`-loop rendering
does not clone an OUTER local read by value inside the body (the `needs()` comment names the
gap for `while`, later widened via `loopExempt` — the `for` site never was): `for x <- xs do
val cur = St(boxed.items)` emits non-compiling Rust (E0382) with or without this pass. The
golden is spelled with `while` for exactly that reason.
