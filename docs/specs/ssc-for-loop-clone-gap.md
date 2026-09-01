# ssc-for-loop-clone-gap — for-do and for-yield bodies get the loop/closure clone context

## Overview

Latent backend gap found while pinning `ssc-local-last-use-move`'s negative golden: the
`Term.For` rendering (`for x <- xs do …` → Rust `for x in xs { … }`) renders its body with a
PLAIN ctx — neither `inWhileLoop` nor `loopExempt` — so an OUTER local read by value inside the
body is moved on iteration one and gone on iteration two: `for x <- xs do val cur =
St(boxed.items)` emits non-compiling Rust (`error[E0382]`). The `while` site gained exactly this
machinery (`inWhileLoop` + `loopExempt`) long ago; the `for` site never did. `renderForYield`
has the same class of gap one step worse: its body renders into a `move |x| { … }` closure with
a plain ctx (no `enteringClosure`), so a captured non-Copy value is moved into a closure that
runs once per element. Every `.foreach` path already uses `enteringClosure` and is safe.

## Interface

No language-surface change. Backend-internal:

- `Term.For` case: body rendered with `ctx.copy(inWhileLoop = true, loopExempt =
  ctx.loopExempt ++ loopExemptNames(f.body) + <generator name>)` — the exact `while` mirror,
  plus the generator variable (fresh each pass, like a closure param).
- `renderForYield`: body rendered with `enteringClosure(ctx, Set(<generator name>))` — it IS a
  closure (`move |name| { … }`), so it takes the closure context, not the loop one.
- The generator RHS keeps the plain ctx in both (evaluated once).

## Behavior

- [x] A non-Copy outer local read by value inside a `for … do` body is cloned (the E0382 repro
  golden compiles and pins the clone).
- [x] Names the `for` body declares or reassigns, and the generator variable itself, stay
  exempt — no needless clones (the `loopExempt` semantics, verbatim).
- [x] A non-Copy capture read by value inside a `for … yield` body is cloned at the use.
- [x] All existing backend goldens stay green modulo reviewed diffs where a clone legitimately
  appears; the regenerated vendored `uniml-md` builds, rozum-agent suite green.
- [x] No performance regression: the parser doubling series and worst-case numbers hold
  (loop-local and reassigned names are exempt, so the hot paths gain no clones).

## Out of scope

- The `.foreach`/range-foreach paths (already `enteringClosure`) and `while` (already fixed).
- The reassigned-captured-`HashMap`-inside-nested-closure gap the foreach `elemType` comment
  documents (separate, measured-worse trade).

## Decisions

- **`inWhileLoop` for for-do, `enteringClosure` for for-yield** — for-do lowers to a Rust
  `for` LOOP (same "body runs again after its position has passed" fact as `while`), while
  for-yield lowers to a `move` CLOSURE (same "captured value consumed per invocation" fact as
  every other lambda); reusing each construct's existing, golden-covered machinery instead of
  inventing a third context.

## Results

Landed as scalascript `1e62d064e` on `feature/treevm-top-edges-prestage10`.

- Backend goldens **516/516** (3 new: the E0382 repro now clones; a loop-declared local gains
  NO clone; a for-yield non-Copy capture clones at the use) — and **zero churn** in existing
  expected texts: `loopExempt` (declared + reassigned + generator variable) kept every
  hot-path name clone-free.
- The regenerated vendored `uniml-md` is **byte-identical** to the previous build (only the
  provenance SHA lines changed): every for-body read in the parser was already exempt —
  declared/reassigned locals, borrows, or Copy values — so the fix is purely protective and
  the ×837 campaign numbers stand untouched. rozum-agent 164/164.
- What it protects: the first future `.ssc` source whose for-do body reads an outer non-Copy
  local by value (or whose for-yield captures one) now compiles instead of failing E0382 at
  crate build.
