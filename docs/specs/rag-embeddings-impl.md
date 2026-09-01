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

- [x] With no gateway running (or no embedder registered), `rag.search` answers exactly as
      today — BM25 with `"fused": false` — and the warmup skips vectors silently.
- [x] With vectors present and the gateway up, `rag.search` fuses and reports `"fused": true`.
- [x] The vector store round-trips (write → read → same vectors), tolerates a missing file, and
      rejects a dimension mismatch by discarding (a model change invalidates vectors).
- [x] An incremental refresh re-embeds ONLY chunks whose ids are new; unchanged ids keep their
      vectors; ids gone from the index are dropped from the store.
- [x] The distilled text of a documented item is `path frag\ndoc sentence(s)`; of an
      undocumented one it falls back to the source (a bare name would lose every undocumented
      chunk).
- [x] `/v1/embeddings` with no registered embedder answers 501, never 500.
- [x] Query embedding failures (timeout, refused) degrade to BM25 within the same call — an
      agent never sees an error because an optional model was absent.

## Results

Verified end to end 2026-09-01 with the real model through the real binaries, not only unit
tests: a fresh gateway (`gateway --port 8499 --model /tmp/qwen3-emb`) served `/v1/embeddings`
(dim 1024, unit norm, 0.4 s cold); a fresh project's proxy warmup built the index, embedded the
vectors through that gateway, and the FIRST `rag.search` of the next session answered
`fused: true` with `notes.md#storage` and `fn append` ranked first for "how does a room
transcript get written to disk" — the exact `transcript`↔`append` vocabulary gap that motivated
embeddings, closed through the production path.

Two deployment notes discovered live: the embed model needs its `chat_template.jinja` present
only if the SAME directory is also used as a chat model (the embedder itself never uses it);
and the warmup's embedding pass runs inside the proxy's async task, so it survives only as long
as the proxy — a session that exits immediately after one call leaves the vectors to the NEXT
session's warmup, which is correct but worth knowing when testing.

## The vector layer, measured (operator questions, 2026-09-01)

**Search** is exact cosine over every vector — no ANN, no index structure. Measured end to end
through the live proxy: warm fused `rag.search` answers in **42–193 ms** (first call ~1.7 s: the
42 MB store load plus the model's lazy load), and the dot-product sweep itself is ~10–20 ms in
Rust for 10.6k × 1024. An ANN index or an external vector DB buys nothing at this scale and costs
a resident service, a dependency, and consistency machinery — the wrong trade until corpora are
two orders larger. The decision is recorded WITH its numbers so the revisit threshold is visible:
when exact search is measurably slow (100k+ chunks), HNSW-in-process is the next step, not a
server.

**Storage** is `RZV2`: per-vector `i8 + f32 scale`, dequantised to f32 at load. Swept against the
eval before switching — f32 11/26 & 20/26, f16 11/26 & 20/26, i8 **12/26 & 20/26** (the +1 is
noise; the point is nothing is lost) — for a 4× smaller file and per-proxy resident copy
(42 MB → 11 MB, and every live agent session holds one). Legacy `RZV1` (f32) still loads and
upgrades on next save, so no project re-embeds.

**Freshness**: the mid-session gap is closed — `rag.search`'s incremental refresh now kicks a
background embed pass when it re-chunked or removed files (one in flight, non-blocking, per-batch
saves), so an edited file regains semantic retrieval within seconds instead of at the next proxy
start. Before this, BM25 found the edit and fusion half-missed it, silently.

**Discoverability**: the proxy's MCP instructions now name `rag.search` and when NOT to use it,
so agents learn the tool exists without any client configuration.

## Pluggable stores (operator direction, 2026-09-01)

`rag_embed::VectorIndex` is the seam an external store implements — Qdrant, LanceDB, a remote
service, or the in-process `VecStore`, interchangeably. Its shape encodes three decisions that
make external stores SAFE rather than merely possible:

- ids are chunk ids (`path#item`), vectors only — chunk text stays in the lexical index, never
  duplicated into a store;
- vectors are L2-normalised f32 at the boundary regardless of internal representation, so the
  contract "dot == cosine" is stated once;
- store state is always derivable from the chunk manifest — a cache by construction — so a dead
  or corrupt external store degrades to BM25, never to wrong answers.

Both consumers (the proxy's `rag.search` and the CLI) already call through the trait. The CLI
now FUSES too, through the in-process embedder hook this binary already carries — one policy,
two readers, no gateway required for `rozum rag search`. Top-k inside `VecStore` selects with
`select_nth_unstable` (O(n)) and sorts only the winners, rather than sorting all n scores for
the five the caller wants.

## The standalone MCP option (operator direction, 2026-09-01)

`rozum rag mcp` serves `rag.search` as its OWN stdio MCP server — retrieval without the meeting
room, for a config that wants only this:

```json
{ "rozum-rag": { "command": "rozum", "args": ["rag", "mcp"] } }
```

Fully self-contained in the engine binary: chunking, embedding (the in-process hook — no HTTP
anywhere) and serving in one process. No meeting daemon, no gateway. Verified live in a fresh
repo with neither running: the server built the index, embedded the vectors and answered
`fused: true` with the transcript↔append semantic pair. The meeting proxy's `rag.search` is
untouched — this is the meetings-free OPTION, not a replacement — and the gate pins that the
standalone server serves `rag.search` and NOTHING else, so meeting tools cannot quietly leak in.

## Out of scope

- ~~Residency-ledger accounting~~ — DONE in the follow-up commit: the embedder measures the MLX
  active-memory delta across its lazy load and bills it via `share::adjust_own_footprint`, so
  other gateways' admission math sees the sidecar. A process holding no reservation (bare test)
  skips the billing, which is a report, not an error.
- ANN indexing. 10.5k × 1024 dot products is ~10 ms; exact search wins until corpora are 100×.
- CLI `rozum rag index` embedding (agents get it via the warmup; the CLI stays model-free).
