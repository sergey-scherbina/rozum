# uniml-inline-tokenize-codeunits — the inline tokenizer indexes code units, not the string

## Overview

Contributor #6 (the last profiled one) of `uniml-single-frame-residual-superlinear`: a
blank-line-free document is ONE paragraph, and `MarkdownInlines.tokenize` plus its scanners walk
that whole content with `charAt`/`substring`/`indexOf`/`startsWith`/`regionMatches` — all O(i)
on the ssc→Rust lane, which emulates JVM code-unit indexing over a UTF-8 string by walking from
the start. At 25,600 lines, `_str_code_at` + `_str_substring` held 3277+1543 of ~5000 samples.
The fix is the precedent `MarkdownLexer.split` already documents in-source: convert the content
to a code-unit vector ONCE (`content.toVector`) and index that — O(1) per access, identical
semantics on both lanes (`Vector[Char]` is UTF-16 code units, exactly what `charAt` yields).

## Interface

Public API unchanged: `MarkdownInlines.parse(content: String, refs, profile)` keeps its String
signature and converts at the boundary (`tokenize(content.toVector, …)`). Internal:

- `tokenize` and every scanner that takes `(content, index)` — `delimiterRun`, `runLength`,
  `findBacktickClose`, `tryLink`, `tryReference`, `buildLink`, `buildRefLink`,
  `parseInlineDestination`, `matchBracket`, `scanAngle`, `scanAutolink`, `scanRawHtml`,
  `scanComment`, `scanClosingTag`, `scanOpenTag`, `scanEntity`, `scanExpression`,
  `isExtendedAutolinkStart`, `validAutolinkPredecessor`, `extendedAutolink`, `emailAutolink`,
  `domainAndPath` — now take `content: Vector[Char]`.
- String-only operations get small Vector-based helpers: `vecStartsWith`, `vecIndexOfChar`,
  `vecIndexOf`, `vecRegionMatchesIgnoreCase` (ASCII fold — the only patterns are schemes and
  `www.`), all O(pattern) at the probe position instead of O(position).
- Helpers that operate on SHORT extracted strings (`trimAutolinkTail`, `validEmailDomain`,
  `emailLocalBackscan`, `normalizeLabel`, `processEmphasis` lexeme math) stay String-based.

## Behavior

- [ ] `content.charAt(i)` → `content(i)`; `content.substring(a, b)` → `content.slice(a,
  b).mkString("")` — byte-identical output on the JVM (same code units, same joins), including
  surrogate pairs (two code units in, two out, concatenated in order).
- [ ] `indexOf`/`startsWith`/`regionMatches` sites replaced by the vec helpers with identical
  match semantics (case-insensitive comparison is ASCII-only, matching the call sites' use).
- [ ] JVM semantics suite green (unimlMarkdown), unchanged counts.
- [ ] Rust lane: regenerated vendored crate builds; rozum-agent suite green.
- [ ] The doubling curve on the blank-line-free worst case is at (or near) linear; no
  regression on ordinary long-line documents (same-window comparison).

## Out of scope

- `normalizeLabel` (labels are short) and the post-tokenize `processEmphasis` (operates on
  short delimiter lexemes).
- The backend's `_str_*` emulation itself (a general backend concern; this removes the hot
  callers instead).
- The `for`-loop outer-local clone gap and stage-10 (both filed separately).

## Decisions

- **Thread `Vector[Char]` through all scanners rather than keep a String+chars pair** — a mixed
  representation leaves every scanner's O(i) walk in place (each probe at position i still pays
  the from-zero walk); the vector must be the ONLY representation past the `parse` boundary.
- **ASCII-only case fold in `vecRegionMatchesIgnoreCase`** — the two call sites match URL
  schemes and `www.`; `regionMatches(true, …)` on the JVM folds more, but no scheme character
  is outside ASCII, so behavior is identical where it can ever be exercised.

## Results

_To be filled at verify time._
