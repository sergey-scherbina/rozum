# ssc-rust-lifted-def-return-types — nested defs join the return-type table

## Overview

Backlog entry, found twice in the corpus: return-type-driven lowerings do not resolve on a call
to a LIFTED LOCAL def, because its declared return type never reaches the module-level
`_returnTypes` table; both historical workarounds promoted the local def to a class method.
Verified against the code before fixing (the entry predated one partial patch): the **Option**
half was already covered locally (`isOptionExpr`'s `nestedLocalDefDecltpe` fallback, added for
`emailLocalBackscan.localTextOf`), but the **Vec** half refused outright (`val picked =
pick(xs); picked.nonEmpty` → "collection member, not a field") and the **String** half took
Rust's byte-`len` path (`tag.length` never routed to `_str_length`).

## Interface

Backend-internal:

- `_returnTypes` construction now also walks every module-level def's BODY, collecting nested
  defs' `(bare name → mapped declared return type)` into the SAME pool under the SAME collision
  discipline: a name resolving to more than one distinct type collapses to "no opinion" (empty
  string), exactly as module-level collisions always have. Bare-name keying stays scope-blind
  by design.
- `defReturnsString` (consulted by `collectLocalStrings`' `isStr`) gains an additive
  `_returnTypes` fallback — `_defBodies` is module-level only, so a lifted def's String return
  was invisible to it; the existing bare-name answer keeps deciding wherever it already did.

## Behavior

- [x] `val hit = findFirst(xs); hit.isDefined … hit.get` lowers (`hit.is_some()`) — was already
  green via the local fallback; pinned so it stays.
- [x] `val picked = pick(xs); picked.nonEmpty` lowers (`!picked.is_empty()`) instead of the
  "collection member, not a field" refusal.
- [x] `val tag = label(xs); tag.length` routes to `crate::runtime::_str_length(&tag)`.
- [x] All existing goldens stay green with zero expected-text churn (517/517, 1 new).
- [x] Regenerated vendored `uniml-md` builds; rozum-agent suite green.

## Out of scope

- `_ownedReturnTypes`/qualified-call resolution (nested defs are called bare by construction).
- Other `_defBodies`-only consumers (`defReturnsEither`, tuple destructure typing) — extend on
  the next concrete miss, with a repro first, per this item's own lesson.

## Decisions

- **Pool-level fix over more local fallbacks** — `isOptionExpr` already grew one local patch
  (`nestedLocalDefDecltpe`); a second and third per consumer would scatter the same fact across
  the file. Feeding the one table every consumer already reads fixes Vec/String now and every
  future return-type-driven lowering by default.
- **Same collision discipline, no scoping** — a nested def sharing a bare name with anything of
  a different type collapses to "no opinion", i.e. today's behavior; scoped resolution would be
  a new mechanism for a gap that collision-collapse already handles conservatively.

## Results

Landed as scalascript `726d07176` on `feature/treevm-top-edges-prestage10`.

- Repro golden written FIRST against the unfixed backend (per the verify-backlog-entries rule):
  the compile REFUSED at `picked.nonEmpty` — confirming the entry's Vec half live, its Option
  half stale (already patched around), and the String half silently wrong rather than refusing.
- Backend goldens **517/517** (1 new: Option/Vec/String lowerings on lifted-def-call vals),
  zero churn — the collision discipline kept every existing name's answer.
- Regenerated vendored `uniml-md` **byte-identical** (provenance SHA only): uniml's one
  historical site was long since promoted to a method, so this is purely enabling — the next
  `.ssc` source that calls a lifted local def and asks a type-driven question compiles instead
  of refusing.
- rozum-agent 164/164. Process note: the repro golden preceded the spec commit this time
  (verification-first collided with spec-first); recorded rather than hidden.
