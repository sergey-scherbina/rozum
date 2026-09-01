# Embeddings in production (build of the spiked design)

## Overview

Turns the measured spike (`docs/specs/rag-embeddings-backend.md`) into the shipped retrieval
path. Every parameter here was measured there, none is a guess: distilled chunk text, the
`implements` query instruction, last-token pooling with `<|endoftext|>`, a 255-token cap,
token-budget batching, and RRF fusion with BM25 as the junior partner. Target numbers on the
26-question eval set: embeddings alone 12/26 & 20/26, fused 11/26 & 21/26, against BM25's
9/26 & 15/26.

## Design — who runs the model

The MODEL runs in the GATEWAY, not in the proxy. The proxy is a thin stdio bridge spawned per
agent session; linking MLX into it would multiply a 336 MB model by the number of live agents
and drag the whole MLX C++ build into `rozum-meet`. The gateway is the one process that already
manages model residency, already sets the process-wide MLX cache limit (which is also why the
embedder must NOT set its own — the spike's 512 MB limit would throttle the chat model's cache
in-process), and already publishes its port in `share::active.json`, which is how the proxy
finds it.

- `rozum-core::embedding` — an `OnceLock` register-hook (the `obs.rs` pattern from the
  workspace split), so the gateway crate needs no edge to `rozum-mlx`. Unregistered → the
  endpoint answers 501 and callers fall back.
- `rozum-mlx::embedder` — the productionised spike: lazy-loads the embed model on ITS OWN
  dedicated thread (all its MLX ops stay on that thread; it never touches `apply_retain_env`,
  which is keyed to the CHAT model's family and process-wide). Batches by token budget.
- Gateway `/v1/embeddings` — OpenAI-shaped (`{"input": [...]}` → `{"data":[{"embedding":[..]}]}`)
  plus a `"query": true` extension that applies the instruction wrapper. Model spec from
  `ROZUM_EMBED_MODEL`, default `mlx-community/Qwen3-Embedding-0.6B-4bit-DWQ`.
- `rozum-agent::rag_embed` — the distilled text, the vector store
  (`.rozum/rag-vectors.bin`: magic + dim + `(chunk-id, f32-LE vector)` entries), cosine top-k
  over normalised vectors, and RRF fusion (k=10, BM25 weight 1, embedding weight 2).
- Proxy — the warmup embeds missing vectors after the incremental refresh (batches through the
  gateway; carries vectors forward for unchanged chunk ids, drops removed ones); `rag.search`
  embeds the query, fuses, and reports `"fused": true|false` so a BM25-only answer is visibly
  BM25-only. `ROZUM_RAG_EMBED=0` disables the whole path.

## Behavior

- [ ] With no gateway running (or no embedder registered), `rag.search` answers exactly as
      today — BM25 with `"fused": false` — and the warmup skips vectors silently.
- [ ] With vectors present and the gateway up, `rag.search` fuses and reports `"fused": true`.
- [ ] The vector store round-trips (write → read → same vectors), tolerates a missing file, and
      rejects a dimension mismatch by discarding (a model change invalidates vectors).
- [ ] An incremental refresh re-embeds ONLY chunks whose ids are new; unchanged ids keep their
      vectors; ids gone from the index are dropped from the store.
- [ ] The distilled text of a documented item is `path frag\ndoc sentence(s)`; of an
      undocumented one it falls back to the source (a bare name would lose every undocumented
      chunk).
- [ ] `/v1/embeddings` with no registered embedder answers 501, never 500.
- [ ] Query embedding failures (timeout, refused) degrade to BM25 within the same call — an
      agent never sees an error because an optional model was absent.

## Out of scope

- Residency-ledger accounting for the embed model's ~400 MB (footnoted: the gateway's admission
  preflight already refuses loads that overcommit actual-free RAM; folding the embedder into
  `update_own_footprint` is a follow-up).
- ANN indexing. 10.5k × 1024 dot products is ~10 ms; exact search wins until corpora are 100×.
- CLI `rozum rag index` embedding (agents get it via the warmup; the CLI stays model-free).
