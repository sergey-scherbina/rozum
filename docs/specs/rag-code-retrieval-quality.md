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

**top-5 stuck at 9/20 is the ceiling, and it is not a selection problem**: for the other eleven
questions the answer is not in the retrieved set at all. BM25 matches words, and these questions
do not share words with their answers — "resident" does not match "residency", "shortened" does
not match "fit the context window". No amount of re-ranking reaches a chunk that scored zero.

That is the measured case for embeddings, and the reason this spec does not attempt them: the
lever now has a set that can judge it, which it did not before.

## Out of scope

- **Embeddings** (`rag-embeddings-backend`) — the justified next step, and it lands behind the
  existing `Retriever` seam. Cost is a resident model beside the frozen 4B, so it lands under the
  residency-admission rules rather than beside them. Judge it on this eval set.
- **Stemming / a synonym list** — cheaper than embeddings and would close some of the eleven
  ("resident"/"residency"). Worth measuring first, precisely because it is cheap.
- `rag-self-reference-contamination` — a document quoting a query outranks the code for it. The
  code slots reduce the symptom without addressing the cause.
