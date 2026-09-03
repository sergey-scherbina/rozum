# RAG exposed to agents (P0: the tool exists and nothing serves it)

## Overview

Make the project's syntactic RAG index reachable from an agent's ordinary code work. The
retrieval itself is built and shipped — `rag_chunk` chunks `.md` and `.rs` syntactically,
`rag_lite::LexicalIndex` ranks them, and `rozum rag index|search` drives both from the CLI.
What is missing is the last hop: **`rag_lite::retrieval_tools` (`rag_lite.rs:143`) builds a
`search_documents` tool and NOTHING registers it.** Its only callers are its own unit test
and the CLI. So no agent has ever been able to call it.

This spec adds one MCP tool, `rag.search`, to the stdio proxy every agent in this project
already runs, and registers the in-process twin on the agent loop's own `ToolSource`.

## The bar this has to clear

A coding agent already has grep, glob and Read. They are exact, instant, and never stale.
**Retrieval earns a call only where those lose**: the exact token is unknown (a concept, a
symptom, "where is admission decided"), the answer is spread across files that share no
literal string, or the agent is new to an area and needs shape before detail.

A tool that returns what `grep -rn` would have returned, slower and less precisely, will be
correctly ignored — and should be. Two consequences that are part of this spec, not of a
later one:

- the tool DESCRIPTION says what it is for and what it is not for, so a model can tell when
  not to reach for it;
- results carry enough provenance (`path#symbol`, score, index age) that the agent can
  decide to go read the file — retrieval points at code, it does not replace reading it.

## Interface

- **MCP tool `rag.search`** on `DaemonProxy` (`crates/rozum-meeting/src/meeting/daemon_proxy.rs`),
  alongside `rooms.*` / `meeting.*`. Params: `query: String`, `top_k: Option<u32>` (default 5,
  capped). Returns `{ results: [{ id, score, text }], index_age_secs, chunks, stale }`.
  - `id` is `rag_chunk`'s chunk id — `path#heading-slug` for markdown, `path#fn name` for Rust —
    so a hit names the file AND the item, and the agent can open it directly.
  - Every response reports the index's age and a `stale` flag. Freshness is
    `rag-index-freshness` (P1) and is NOT solved here; until it is, an answer out of a stale
    index must at least SAY so rather than be quietly wrong.
- **In-process** — `retrieval_tools(index)` registered on the agent loop's `MultiToolSource`
  next to `CallbackToolSource`, so the local Qwen assistant and the cascade get the same tool
  without MCP. This half is registration, not new code.
- **No gateway-side injection.** The gateway will NOT add this tool to every request:
  tool-schema bloat is measured here (~4.9K tokens of schema per request, which is why
  `rozum launch --lean` exists), and a tool the client did not ask for costs exactly that on
  every call. MCP is opt-in by construction — an agent that configured the rozum MCP server
  asked for these tools.

## Behavior

- [x] `rag.search` appears in the proxy's `tools/list` next to `rooms.list`, and an agent that
      already has the rozum MCP server configured gets it with no client-side change.
- [x] A query returns hits whose `id` names both file and item (`crates/…/rag_chunk.rs#fn
      chunk_code`), so the result is directly openable.
- [x] The index is loaded lazily, ONCE per proxy process, and reused across calls — the CLI's
      0.35 s per search is a 31 MB disk reload, not a search, and must not be paid per call.
- [x] No index → a clear, actionable answer naming `rozum rag index`, never an error or an
      empty list that reads as "nothing matches".
- [x] Every response carries `index_age_secs`; `stale` is true past a threshold, and the
      threshold is stated in the tool description so the model can weigh it.
- [x] The project is resolved with the proxy's existing `detect_project()` (nearest ancestor
      with `.git`, else cwd) — the same project the agent's room belongs to.
- [x] `top_k` is capped so one call cannot flood the context window it is meant to save.

## Design

The proxy, not the daemon. The proxy process is per-agent and per-project with the right cwd,
so one index serves exactly one project and dies with the session. The daemon is shared across
projects and rooms; hosting retrieval there would mean holding an index per project in one
long-lived process — 31 MB each today — against a machine where no-OOM is a hard invariant.

This adds the crate edge `rozum-meeting → rozum-agent`. Checked before accepting it:
`rozum-agent` depends only on `rozum-core`, `rozum-models`, `uniml-md`, tokio, rmcp, serde —
no MLX, no engine, and rozum-meeting already uses tokio/rmcp/serde. The edge costs build time,
not runtime weight. The alternative considered was the `OnceLock<fn>` register-hook IoC this
workspace already uses (`rozum-core::obs`), keeping the edge out; rejected for P0 because it
buys nothing here — the hook's only registrant would be the same binary — and it hides a
direct call behind indirection. Revisit if `rozum-meet` build time becomes a problem.

## Results

Verified 2026-08-31 through a real MCP handshake against `rozum-meet mcp-proxy` (not only unit
tests): `tools/list` returns `rag.search` alongside `rooms.*`/`meeting.*`, and a call answers
`chunks: 46733, index_age_secs: 0, stale: false` with three hits. The plumbing this spec is
about works.

**And the same probe measured the thing that decides whether it gets USED, so it is recorded
here rather than discovered again later.** Two code queries returned each other's answers:

```text
"the function that decides whether a model may become resident"
   35.82  crates/rozum-meeting/src/meeting/daemon_proxy.rs#fn forward      <- wrong
"how does the proxy forward a tool call to the daemon"
   21.51  docs/specs/cpu-uma-offload.md#risks-open-questions               <- wrong
   20.53  scripts/bench/results/_archive-…/…t5-code-easy.txt#p1            <- noise
```

BM25 is word overlap with no notion of meaning: "resident"/"model" matched `forward`'s prose,
while the answer to the second question was handed to the first. Against this spec's own bar —
retrieval must beat grep or be correctly ignored — **code retrieval does not clear it yet**.
That makes `rag-code-retrieval-quality` (P2) not polish but the item that makes this one pay,
and it gives that work a starting eval pair. Prose retrieval does clear the bar (`"residency
admission queue"` → the right spec, top hit), so the tool is useful today for docs.

Second finding from the same probe: archived benchmark output under
`scripts/bench/results/_archive-*/` is indexed and ranks. Retrieval scope is a cheap, separate
lever on the same problem — filed as `rag-index-scope`.

## Out of scope

- **Freshness / incremental reindex** — `rag-index-freshness` (P1). This spec only makes
  staleness VISIBLE.
- **Retrieval quality on code** — `rag-code-retrieval-quality` (P2). BM25 ranks prose well and
  code by word overlap; measured 2026-08-31, `"read-only parameter shared reference"` returns
  `struct SandboxPolicy` first. Both the embeddings lever and the free structure-aware lexical
  lever (the chunker knows a chunk is a `fn`/`impl`/`struct` and currently discards it) live
  there.
- A compact on-disk index format. 31 MB of JSON per project is a real cost once several agents
  each hold one; note it, do not fix it here.


## The third surface, and the policy that had drifted (2026-09-03)

This spec named two surfaces — MCP and the in-process agent loop — and both were wired. What
it did not say is that they were not serving the same retrieval, and one caller had been left
out entirely.

**The in-process tool was not the shipped policy.** `project_retrieval_tools` handed back the
generic `search_documents` over the raw index: BM25, no reserved slots for implementations, no
fusion, and a default `k` of 3. That is the configuration the eval measures at **4/26 top-1**,
while both MCP servers ran the fused, slot-balanced path measured at **22/25**. The caller was
`nadia` — the local agent whose model has the least context to fall back on, which is precisely
the argument this spec gives for why it needs retrieval most. It had the worst retrieval in the
system, and nothing said so, because the two paths were different code that merely shared a
name.

The cause is worth naming because it is structural, not a slip: the sequence *deep pool →
balance → fuse → fill texts → rebalance* existed as **three copies** (meeting proxy, standalone
MCP server, eval harness). Three copies of a policy are a policy that drifts, and the fourth
caller simply never received it. The fix is one function, `rag_embed::rank_fused`, with every
surface calling it — including the eval, so the instrument measures what the servers run.

The query VECTOR stays a parameter rather than something that function fetches, because how a
surface reaches an embedder is what legitimately differs: the meeting proxy must go through the
gateway (in-process embedding aborts the whole server at Metal init under the agent jail), the
eval carries no model at all, and the in-process tool tries the gateway first and falls back.
`None` is the first-class BM25-only state, and the returned `fused` flag is how a caller reports
which happened instead of guessing.

Verified behaviour-preserving: the eval scores **22/25 and 25/25 after the refactor, identical
to before**, with the same three questions top-5-only.

**The meeting-room participant now has retrieval too** — the gap this spec explicitly filed as
"not this item's unfinished half; file separately if the operator wants it there". The operator
did. `rozum meetings participant --rag-project <DIR>` (forwarded by `participant-pool` to every
room it supervises) offers the room's model a `rag_search` tool over that project's index.

Three decisions in it, each one load-bearing:

- **Two gates, and it needs both.** `--rag-project` is the operator's decision that the room may
  see that tree at all — retrieval reads a tree the sandbox does not confine, so a chat sandbox
  could otherwise widen silently into "can read the source". The per-user `read` grant from the
  ACL is the second: a user who may not read files has no business reading them through a search
  box. It does *not* require a sandbox — searching a project and having a working directory are
  independent grants, and the tool appears with neither, either, or both.
- **`rag_search`, not `rag.search`.** This surface is OpenAI function-calling, whose name grammar
  is `[A-Za-z0-9_-]{1,64}`; a dot is outside it, and the small local models these rooms run are
  the least forgiving consumers of a name their template did not expect. Same tool, same policy,
  spelled for the dialect carrying it — it dispatches under the canonical name internally.
- **The system prompt had to change, or the tool would never fire.** With only sandbox tools the
  prompt told the model, correctly, to answer project questions with TEXT and call no tool — a
  rule that becomes exactly wrong once the tool that answers project questions exists. That
  sentence is now conditional on retrieval being granted.

Results are rendered as a numbered list of `path#item` plus a clipped snippet rather than JSON:
a 4B model in a group chat reads that far better, and the room's context is small enough that
the braces are a real cost.
