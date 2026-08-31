# Embeddings behind `Retriever` (P2) — spike results and the decision they force

## What was measured, and why a spike came before any plumbing

The justification for embeddings was that six of the eval set's answers score ZERO under BM25
because the doc comment says the same thing in other words (`"transcript"` vs `fn append`'s
*"Append one message"*). A backend, an index format, residency integration and a fallback path are
all wasted work if embeddings do not actually fix those. So the first thing built was a throwaway
that answers only that question, on the same 26 questions.

Model: `mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ` — `model_type: qwen3`, so rozum's existing
MLX loader takes it unchanged. No fork change was needed to get hidden states either: `Model` has
a public `model: Qwen3Model` field whose `forward` returns the last hidden state BEFORE the LM
head.

## Results

```text
                              top-1     top-5
  BM25 (what ships, w/ slots)  9/26      15/26
  embeddings alone             7/26      15/26
  RRF fusion of the two       10/26      17/26
```

**Embeddings alone LOSE to BM25.** Fused by reciprocal rank they win by +1 top-1 and +2 top-5,
which is the real result: the two methods miss different questions, and that is the only argument
for carrying both.

### The pooling recipe is not a detail — it was the whole answer

The first version of this spike used mean pooling and scored **0/26 top-1, 6/26 top-5**, with
`docs/specs/*.md` taking almost every first place. That reads as a decisive verdict against
embeddings and it would have been recorded as one.

It was a bug in the spike. Qwen3-Embedding is trained for LAST-token pooling with `<|endoftext|>`
appended and queries wrapped in an instruction (`Instruct: …\nQuery: …`). Applying its own recipe
moved the same model, the same corpus and the same questions from 0/26 to 7/26 top-1 and 6/26 to
15/26 top-5. **A model measured off-recipe measures nothing**, and this one nearly produced a
confident, wrong "embeddings do not work here".

## The cost, stated so the trade is visible

- **336 MB** model on disk, and a resident model beside the frozen 4B — so it lands UNDER the
  residency-admission rules, not beside them, and must degrade to BM25 when admission is refused.
- **41 MB** of vectors for this repo's 10,551 chunks (1024 dims x f32), on top of the 9.3 MB index.
- Embedding the corpus is a per-chunk forward pass: **718 s — twelve minutes — for this repo's
  10,551 chunks**, measured, on the same GPU the resident 4B is using. That is 32x the 22 s full
  BM25 build, and it is the number that most changes the trade: the first build of any project is
  a twelve-minute background job competing with the model the operator is actually talking to.
  It belongs in the warmup that already exists, and incremental refresh MUST carry vectors forward
  for unchanged files exactly as it carries chunks — otherwise every refresh pays it again.
- Every search needs the query embedded, so the model must be loadable at query time or the search
  falls back.

**For +1 top-1 and +2 top-5 out of 26.** A real gain and a real price, close enough that this is a
judgement call rather than an obvious yes — which is why the spike stops here rather than
proceeding into the plumbing. The twelve-minute first build is the part that argues loudest
against, and it was measured after the quality numbers, so it is worth re-reading the trade with
it in hand rather than the "minutes" this section first said.

## If it is built

- Behind the existing `Retriever` seam, as `rag-embeddings-backend` always intended.
- **Fusion, not replacement**: BM25 stays the primary and the zero-model fallback; embeddings are
  a second ranker fused by RRF. Alone they are worse, so replacing would be a regression.
- Vectors live beside chunks in the v2 per-file manifest, so `reindex_incremental` reuses them on
  an unchanged file for free.
- The eval set is the judge: 10/26 and 17/26 is the number to beat or match.

## Out of scope

- Any larger embedding model. 0.6B was chosen because it fits the residency budget; a bigger one
  changes the trade above, and the trade is the point.
