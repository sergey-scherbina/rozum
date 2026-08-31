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
- Embedding the corpus, **330 s** for this repo's 10,551 chunks — see the batching section below;
  the naive one-at-a-time version is 718 s. Still 15x the 22 s BM25 build, so the first build of a
  project is a five-and-a-half-minute background job sharing the GPU with the resident 4B. It
  belongs in the warmup that already exists, and incremental refresh MUST carry vectors forward
  for unchanged files exactly as it carries chunks — otherwise every refresh pays it again.
- Every search needs the query embedded, so the model must be loadable at query time or the search
  falls back.

**For +1 top-1 and +2 top-5 out of 26.** A real gain and a real price, close enough that this is a
judgement call rather than an obvious yes — which is why the spike stops at the measurement rather
than proceeding into the plumbing. The build cost was the loudest argument against and batching
halved it (718 s -> 330 s), with incremental refresh making it a once-per-project cost rather than
a recurring one; what remains against is the resident model itself, under admission rules, for a
gain of one and two questions out of twenty-six.

## Batching needs TWO limits, and neither works alone

The first measurement embedded one chunk per forward pass — batch size 1 on a GPU, the wrong
shape of work. Fixing it turned out to be a memory problem rather than a throughput one:

```text
  one chunk per pass                        718 s   completes
  fixed batch of 16 rows                      —     SIGKILL at chunk 3008 of 10551
  token budget 4096                           —     SIGKILL at chunk 6009
  token budget 4096 + MLX cache limit       330 s   completes      <- 2.2x
  token budget 16384 + MLX cache limit      363 s   completes      (worse: padding waste)
```

Both kills were deterministic, and both happened with **~16 GB of system memory free** — so the
activations themselves fit, and the naive reading ("embedding needs too much memory") is wrong.

- **Rows are the wrong unit.** Cost scales with rows x width, and chunks are sorted by length to
  minimise padding, so a fixed row count necessarily walks into the ceiling as the rows get
  longer. The budget has to be on the product.
- **The remainder was MLX's CACHE growing across batches**, which is exactly what
  `set_cache_limit` bounds and what `rozum-core`'s memory notes already say is the only bounded
  term (active memory — weights plus activations — has no cap at all). Bounding it is what made
  the run finish.

Quality is IDENTICAL batched and unbatched (7/26 and 15/26), which is the check that the padding
scheme is numerically sound: right padding with causal attention cannot reach a real token, so
pooling the last REAL token gives the unpadded answer. That is a property of this pooling choice,
not a general licence to pad.

On a machine whose hard invariant is no-OOM, this is not tuning. A background indexer that grows
without a ceiling beside a resident model is a jetsam waiting for a larger corpus.

## Why a second model — the resident 4B was tried, and cannot do this job

The obvious objection to a 336 MB embedding model is that a 4B is ALREADY resident: take its
hidden states and pool them, and the second model — with its disk, its admission and its memory —
disappears. The mechanism is there (`Model.model` is public in the qwen3 family; for `qwen3_5` its
`forward` is private, a one-word change in our own fork), so it was measured rather than argued.

```text
  Qwen3-Embedding-0.6B (purpose-built)   330 s    top-1 7/26   top-5 15/26
  resident Qwen3.5-4B (chat model)      2333 s    top-1 0/26   top-5  0/26
```

Zero on both, and the vectors are not degenerate — 15 distinct first hits across 26 questions —
they are simply meaningless: `.github/workflows/ci.yml`, CHANGELOG entries, `testdata`. What makes
this a result about the MODEL rather than about the spike is that the identical code path scores
7/26 and 15/26 with the 0.6B; only the checkpoint differs.

The reason is the ordinary one, now measured here: a causal LM's last-token hidden state is
trained to predict the NEXT TOKEN, not to represent the text it has read. Contrastive training is
what turns hidden states into a metric space, and a chat model has not had it.

And even had quality held, the cost runs the wrong way: **2333 s against 330 s** — 7x — on the very
model the operator is talking to, rather than on a 0.6B that can be loaded and dropped around it.
The second model is not overhead to be optimised away; it is the cheaper half of the trade.

**Two silent failures happened inside this one spike, and both looked like verdicts.** Mean pooling
instead of last-token scored 0/26 and read as "embeddings do not work here". Passing an EMPTY
`LayerCache` slice to `qwen3_5` ran ZERO layers — its forward is `layers.iter_mut().zip(cache
.iter_mut())`, so an empty slice yields no pairs — and returned the bare embedding lookup in 1.29 s
with, again, 0/26. Neither errored. A wrong call in this area produces a plausible number, not a
failure, so a zero is a reason to check the harness before it is a reason to conclude anything.

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
