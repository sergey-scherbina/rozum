# RAG index freshness (P1)

## Overview

Keep the project's RAG index current without anybody remembering to refresh it. P0 made the
index reachable and made staleness VISIBLE (`index_age_secs`, `stale`); it could not fix it.
For code that gap is not hygiene but correctness: a working tree changes under the agent on
every edit it makes itself, so a stale index answers confidently out of code that no longer
exists — and the agent cannot tell.

Three parts, in the order they depend on each other: make a refresh cheap (incremental), run
it before every search, and build it ahead of time in the background so nobody ever waits for
the first one.

## Interface

- **`reindex_incremental(root, progress)`** — re-parses only files whose `mtime` or length
  changed, drops entries whose files are gone, and REWRITES NOTHING when neither happened.
- **`refresh_in_background(root, progress)`** — the same, behind a cross-process `try_lock` on
  `<root>/.rozum/rag-index.lock`. `Ok(None)` means a sibling is already doing it.
- **`rozum rag index`** — incremental by default; `--full` re-parses everything, for when the
  CHUNKER changed (nothing on disk moves, so the mtime/length pair cannot see it). Prints
  `N reused, M re-parsed, K removed`.
- **On-disk format v2** — chunks grouped by file with the stat that produced them. A v1 index
  still LOADS (so searches keep working across the upgrade) but cannot be reused incrementally,
  so the first pass after upgrading is a full build.
- **`IndexStats`** gains `reused` / `rechunked` / `removed`.
- **Proxy warmup** — `rozum-meet mcp-proxy` starts a background build/refresh at startup, off
  the request path. `ROZUM_RAG_WARMUP=0` disables it.
- **`rag.search` refreshes before searching** when an index exists, and reports `building:
  true` when a first build is in flight.

## Behavior

- [x] An incremental index is byte-identical to a full one after an edit, an add and a delete.
- [x] A file deleted from the tree loses its chunks — nothing may outlive its source.
- [x] A no-change pass leaves the index file's mtime untouched.
- [x] A v1 index still answers searches, and upgrades by rebuilding rather than by treating
      "no manifest" as "nothing changed".
- [x] A second concurrent builder skips instead of repeating the work, and writes nothing.
- [x] A fresh checkout with no index serves its FIRST `rag.search` from a background-built
      index, with no manual command.
- [x] `rag.search` finds a file created moments earlier, with no manual reindex.
- [x] `.rozum/` ignores itself, so the automatic build never leaves an untracked 31 MB file.

## Results

Measured 2026-08-31 on this worktree (490 files, 10,518 chunks):

```text
  full build                    23.50 s
  incremental, nothing changed   0.02 s     1175x
  incremental, one file edited   0.51 s
```

Identical output either way — 490 files / 10,518 chunks from both paths, which is the gate that
matters: a cache that is merely fast is worthless, and the risk is that it is fast and quietly
different.

Two design points that the numbers forced, both of which the first version got wrong:

- **A no-op pass must not rewrite the file.** The MCP proxy holds the index in memory and
  reloads when the mtime moves, so rewriting identical content on every search would make each
  call re-read 31 MB — the freshness check would cost far more than the search it guards.
- **Auto-refresh must not perform the FIRST build.** Doing so put 23.5 s inside a tool call in
  every fresh checkout. Caught by P0's own `no_index` test, which started failing because the
  refresh had silently created the index it was asserting the absence of. The build moved to a
  background warmup at proxy startup instead — the operator's instruction, and the right shape:
  an agent should neither wait for a build nor be told to run a command.

One consequence worth naming, because it only appears once building is automatic: an index is
now created in every project an agent visits. `.rozum/` therefore writes the self-ignoring
`.gitignore` the meeting store already established — without it each visit leaves a 31 MB
untracked file in someone's `git status`, and one careless `git add -A` commits it.

## Out of scope

- **Content hashing.** `mtime` + length is a heuristic; the residual case is an edit that keeps
  the byte count within the same second as the last write. A hash would close it at the cost of
  reading every file on every pass — which is the 0.02 s pass's entire saving. Revisit only if
  the case is ever observed.
- **Watching the filesystem.** Refresh-before-search plus startup warmup covers the agent's own
  edits without a watcher, its platform differences, or a resident process.
- Retrieval quality (`rag-code-retrieval-quality`) and index scope (`rag-index-scope`).
