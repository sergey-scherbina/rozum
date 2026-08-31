# Code retrieval quality (P2)

## Overview

Make retrieval good enough at CODE that an agent doing ordinary work reaches for it. P0 served
the tool, P1 kept it fresh, `rag-index-scope` cleaned the corpus; all three left the ranking
untouched, and ranking is what decides whether the tool gets used.

The bar (from `rag-expose-to-agents`): an agent already has grep, glob and Read — exact, instant,
never stale. Retrieval earns a call only where those lose.

## The eval set

`crates/rozum-agent/tests/rag-eval.json` — 20 questions over this repository, each with the chunk
that answers it. Two rules make it honest:

- **Questions never contain the identifier.** "where does the code decide whether a model is
  allowed to become resident", not "acquire_residency". A question naming the symbol is one grep
  answers better, and including such questions would let a change look like an improvement while
  leaving the tool no more useful than `grep -rn`.
- **`grep_beats_it` is recorded per question**, so the set cannot drift into measuring only the
  cases retrieval happens to win.

Gate: `code_retrieval_meets_its_measured_floor`, `#[ignore]`d because it indexes the whole repo
(~22 s release, 107 s in the debug test profile — five times the entire unit suite). Run it when
touching chunking, ranking or selection:
`cargo test -p rozum-agent --lib code_retrieval_meets_its_measured_floor -- --ignored`.

## Results

```text
                                   top-1     top-5
  baseline (BM25 over the corpus)   3/20      9/20
  + identifier as a boosted field   4/20      9/20
  + code keeps most of k's slots    8/20      9/20
  + `use` blocks lose the boost     8/20     10/20
  (excluding self-referential hits) 8/20     11/20
  + stemming                        6/20     11/20   REJECTED, made top-1 worse
```

**top-1 went 3 → 8; top-5 did not move at all.** Both halves of that matter.

The gain came from one structural fact: a chunk's identifier — `fn detect_project`, a markdown
heading — was not indexed at ALL. The one part of a chunk that states what it *is* was invisible
to ranking. It is now a field boosted 3×, split on `snake_case`/`camelCase` so "project directory"
can match `detect_project`.

That alone bought almost nothing (+1), and the reason is the finding: **prose outranks the code it
describes.** A spec discussing a function mentions the query's words more often than the short
function implementing them, and BM25 has no notion of "describes" versus "is" — boosting
identifiers boosted the spec's headings too. Measured directly: ranking everything together finds
the answer first 4 times in 20; looking at code alone, 8. So `search_balanced` reserves most of `k`
for code chunks and does **not** re-sort the result by score — an earlier version did, which put
prose straight back on top and made the slots decoration. That mistake cost nothing but a
measurement, and it is why the "no re-sort" is written down rather than implied.

**CORRECTION (same day, before anyone acted on it).** The first version of this section said the
remaining answers "scored zero" and that "no amount of re-ranking reaches a chunk that scored
nothing". **That was wrong, and it was wrong in the direction that costs the most: it points the
next agent at embeddings when the actual lever is ranking.** I inferred "scored zero" from "absent
from top-5" without ever querying at a larger `k`.

Measured properly, at `k=200`, nine of the ten missing answers ARE retrieved:

```text
  rank   6   rag_lite.rs                          rank  36   fn refresh_in_background
  rank   7   rag_chunk.rs                         rank  58   fn spawn_index_warmup
  rank  15   fn forward                           rank  79   fn acquire_residency
  rank  24   fn structural_hint                   rank  95   fn git_project_files
  ABSENT     fn detect_project
```

So the ceiling is a RANKING problem — the answers are in the corpus and scoring — and exactly one
question out of twenty is a genuine vocabulary miss. Two consequences follow, both measured:

- **Stemming was tried and REJECTED.** It was filed as the cheap fix for the vocabulary story
  above, and with that story corrected it had little left to fix: a conservative suffix stripper
  (`residency`→`resident`, plurals, `-ing`/`-ed`) took **top-1 from 8/20 down to 6/20** while
  leaving top-5 at 11. It collapses distinct identifiers into one term, and in a corpus that is
  mostly code that loses more precision than the few word pairs are worth. Reverted; recorded so
  it is not re-attempted on the same reasoning.
- **An import block was ranking as if it were a named symbol.** `chunk_code` tiles a file, so its
  first chunk is `#use <first-import>` — short, identifier-dense, and carrying the module's `//!`
  doc — and the identifier boost was crediting `use std` as a symbol name. `store.rs#use` and
  `resident.rs#use` were beating the actual functions. Dropping the boost for `use` fragments
  (the chunk stays indexed) took **top-5 from 9/20 to 10/20**, and 11/20 once the eval set stopped
  measuring itself.

**The eval set now contaminates its own corpus.** `crates/rozum-agent/tests/rag-eval.json` and
this spec are indexed and quote the questions verbatim, so both rank #1 for them — worth +1 to +2
of apparent score. The numbers above exclude hits from those two files. This is
`rag-self-reference-contamination` arriving in the measurement rather than in the product, and it
is the clearest possible demonstration of it.

Current honest standing: **top-1 8/20, top-5 11/20**, with the remaining gap owned by ranking, not
vocabulary.
## Out of scope

- **Embeddings** (`rag-embeddings-backend`) — the justified next step, and it lands behind the
  existing `Retriever` seam. Cost is a resident model beside the frozen 4B, so it lands under the
  residency-admission rules rather than beside them. Judge it on this eval set.
- **Stemming** — measured and rejected, see the correction above. A curated SYNONYM list is a
  different bet and still open, but it now has to beat 8/20 top-1 rather than fix a vocabulary
  problem that turned out not to exist.
- `rag-self-reference-contamination` — a document quoting a query outranks the code for it. The
  code slots reduce the symptom without addressing the cause.
