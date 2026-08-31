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

### What actually occupies the agent's five slots

Chasing the competitors at ranks 6–95 turned up the real shape of the noise. Classifying every
hit across the 20 questions — 100 slots in total:

```text
                 implementation   tests   import blocks   prose
  before                     54      32               6       8
  after                      80      12               0       8
```

**Nearly a third of what an agent was shown for "where is this implemented" was TEST code.** Not a
scoring bug but a property of good tests: their names are English sentences
(`single_model_gate_is_identical_with_or_without_reservation`), which is exactly the shape of a
natural-language question, so they match one better than the terse function that does the work.
Import blocks took another 6 for the reason already known — short and identifier-dense.

`search_balanced` now apportions slots to IMPLEMENTATION chunks, with tests and prose filling the
rest; tests are demoted, never dropped, since sometimes the test is the answer.

**The hit rate did not move — 8/20 and 11/20 before and after — and that is worth stating plainly
rather than hiding.** For these twenty questions the answer was already in the retrieved set or
already out of reach, so the metric could not see the change. What changed is what the agent
READS: 80 of 100 slots are implementation instead of 54. Both numbers are true and they measure
different things; a set of questions whose answers sit at ranks 6–15 would show the difference in
hit rate too, and building one is the obvious next refinement of the eval.

A first attempt at this made things WORSE (top-1 8 → 6): detecting a test by
`text.contains("mod tests")` marks ordinary implementation chunks, because `chunk_code` tiles a
file and its last chunk usually carries the whole test module. The attribute has to OPEN the
chunk. Recorded because the loose version looks obviously correct.

### The eval set was blind, and now is not

The composition result above could not be seen by the metric, which is a defect in the metric.
Six questions were added whose answers sat at ranks 2–29 rather than at 1 or beyond reach, and the
enlarged set was then checked against the thing it must detect — the slot policy itself:

```text
                      top-1     top-5
  raw BM25 order       4/26      12/26
  with the slots       8/26      13/26
```

It registers. The original twenty scored 8 and 8 across that same change. A metric blind to the
change it exists to judge is worse than no metric, because it reads as evidence that nothing
happened — and I nearly reported exactly that.

### Length normalisation was ranking against the implementations

With a metric that could finally see ranking, the next question was what still beats the answers.
The obvious guess — long, vocabulary-rich chunks win — is WRONG. Across the misses, the chunk that
beat the answer had a median of **80 words against the answer's 207**, and was longer in only 5 of
11 cases. BM25's `b = 0.75` penalises long documents, and a Rust function that does real work is
long: the ranker was systematically biased against implementations, which is precisely the class
this whole item is about.

```text
  b = 0.75   top-1 8/26   top-5 13/26     (the textbook default)
  b = 0.50         9/26         15/26     <- chosen
  b = 0.30         7/26         15/26
  b = 0.00         3/26         11/26     (no normalisation at all: much worse)
```

`k1` was swept too and left at 1.2 (1.6 ties; 0.8 and 2.0 are worse). Two parameters tuned on 26
questions is a real overfitting risk; what makes this one trustworthy is that the DIRECTION was
predicted by the length measurement before the sweep, and that b = 0.0 is clearly worse — the
curve has an interior optimum rather than the metric just rewarding less normalisation.

### Where the remaining eleven actually sit — half ranking, half vocabulary

Re-measured at `k=300` after the slot policy and `b = 0.5`, so the answer is data rather than
another inference from a truncated view:

```text
  rank 13, 15, 17, 20, 31        five answers  — still reachable, still ranking
  rank 58, 59, 189               three         — deep
  absent (score zero)            three         — no shared vocabulary at all
```

The deep and absent six are a DIFFERENT problem from everything fixed so far, and the three
absent ones show it plainly — the doc comment says the same thing in other words:

```text
  "how does a room's transcript get written to disk"
      store.rs#fn append        "Append one message, assigning (date, n) from ts's local date"
  "where does the daemon record a room so it can be found again later"
      store.rs#fn register_room "Upsert a room (keyed by root) into <state_dir>/rooms.json"
  "how does an agent tell the room it is leaving"
      daemon_proxy.rs#fn announce_left  "Best-effort `left:` presence line, posted after the
                                         agent's stdio session ends"
```

`transcript`/`message`, `written to disk`/`append`, `record`/`upsert`, `leaving`/`left`. BM25
matches words; these pairs share none. **This is the measured case for embeddings** — and it is a
much better one than the version this spec carried at first, which claimed all eleven scored zero
and was wrong. Five of the eleven are still ordinary ranking and may yet yield to a lexical lever;
six will not.

### Indexing the doc comment as its own field: tried, measured, REJECTED

The obvious next lexical lever, and it looked strong from the data: a chunk is a byte-exact slice
of source, so most of a code chunk's words are syntax. In `detect_project`'s 55 words the meaning
lives in ONE line — `/// The agent's project: the nearest ancestor with a .git, else the cwd` —
and the rest is `unwrap_or`, `to_string_lossy` and a comment rule made of box-drawing characters.
The one sentence written for a human reader carried the same weight as a punctuation run.

So: extract the leading `///` / `//!` block per code chunk and boost it, exactly as the identifier
field is boosted. Swept:

```text
  DOC_BOOST = 0   top-1 9/26   top-5 15/26     (no doc field — what ships)
              1         7/26         16/26
              2         8/26         14/26
              3         7/26         14/26
              5         7/26         14/26
```

**Every non-zero weight makes top-1 worse.** The reason, in hindsight: those words are ALREADY in
the chunk text and already counted, so the boost adds no new signal — it multiplies existing
signal, and it does so for every code chunk equally, competitors included. Nothing is
discriminated; the whole field is scaled. The one +1 at top-5 does not pay for −2 at top-1, which
is the number an agent lives on because it reads the first hit.

Reverted. Recorded because "index the doc comment separately" is an obvious idea with a plausible
story behind it, and it will be proposed again.

Current honest standing over 26 questions: **top-1 9/26, top-5 15/26**, 80% implementation in the
slots, remaining gap owned by ranking, not vocabulary.
## Out of scope

- **Embeddings** (`rag-embeddings-backend`) — the justified next step, and it lands behind the
  existing `Retriever` seam. Cost is a resident model beside the frozen 4B, so it lands under the
  residency-admission rules rather than beside them. Judge it on this eval set.
- **Stemming** — measured and rejected, see the correction above. A curated SYNONYM list is a
  different bet and still open, but it now has to beat 8/20 top-1 rather than fix a vocabulary
  problem that turned out not to exist.
- `rag-self-reference-contamination` — a document quoting a query outranks the code for it. The
  code slots reduce the symptom without addressing the cause.
