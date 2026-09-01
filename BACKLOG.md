# Backlog

Three groups, and the group is the first thing to read:

- **LIVE** — actionable now: nothing external is in the way.
- **BLOCKED** — waiting on another repository; each says what unblocks it.
- **PARKED** — deliberately not now; each says what would revive it.

Closed items live in [`BACKLOG-ARCHIVE.md`](BACKLOG-ARCHIVE.md), not here. Keeping them inline is
how this file reached 2072 lines with 80 of its 136 entries already done — a list that long is
skimmed, not read, and the parked bucket had swallowed two items that depend on nothing (see LIVE
→ *Rescued from the parked bucket*). One more entry carried a `Parked because` line belonging to a
different item entirely, and TWO entries existed twice over — 2026-08-04 copied them into the parked
bucket instead of moving them, and the first pass of this triage moved both copies rather than
noticing. All three kinds of rot come from sorting by section instead of by item, including the one
committed while fixing the other two.

# LIVE

## Syntactic RAG (phases 2–3; phase 1 is on SPRINT as `rag-syntactic-md`)

Operator's design (2026-08, in-session, binding): RAG over project sources with a SYNTACTIC
chunker built on **uniML compiled via ssc→Rust** (path A — no JVM anywhere; `syn` proposed and
REJECTED — uniML's one tree covers code and English/Russian prose alike). The prerequisite
campaign — every uniML module emitting clean Rust (core 64→0, yaml 184→0, markdown 155→0 real
cargo errors) — landed in scalascript `main` 2026-08-30. Integration seam on the rozum side
already exists: `src/rag_lite.rs` (BM25 `LexicalIndex`, `Retriever` trait, `search_documents`
tool). Spec: `docs/specs/syntactic-rag.md` (written with phase 1).

- [x] **rag-uniml-parser-quadratic — DONE 2026-08-31.** `Markdown_parse` was O(bytes²); it is now
  linear in size for ordinary documents, and phase 2 (code) shipped on top of it. 256 KB went
  173.4 s → 0.108 s (~1600×) over four rounds, each of which killed the previous round's cause:
  eager `Vector` clones (real, 11%), the UTF-16-over-UTF-8 `charAt` emulation (real, 2.8× of 13×),
  `_str_substring` allocating per call plus `xs = xs ++ ys` copying the accumulator per token, and
  finally `MdLine.split` — introduced by round 3's own fix, because `substring` counts from the
  START of the string and so costs O(index) on this backend.
  Then `uniml-treevm-quadratic-frames` removed the last 160× on pathological SHAPE.
  **The cap is no longer the coverage limit it was.** This entry's central decision — keep
  `MAX_MARKDOWN_TREE_BYTES` at 16 KB because the 16–32 KB band cost more than all 125 smaller files
  combined — was correct against a quadratic parser and is obsolete against a linear one. The cap is
  1 MB now, above every document in the repo, and a full reindex reports **0 large .md on the text
  path**: coverage went 88% → 100%. `rozum rag index` = 2648 files → 46,733 chunks.
  The durable lesson is the one this entry already recorded twice and then a third time: cap
  arithmetic must come from the corpus, never from a synthetic benchmark.
- [ ] **ssc-rust-persistent-vector** — **the O(n²) class itself, and the successor to the two
  items below.** Spec: `docs/specs/ssc-rust-persistent-vector.md`. Scala's `Vector` is persistent
  (append/slice/share O(1)–O(log n)); the Rust backend lowers it to `Vec<T>`, where each is an
  O(n) copy — so idiomatic Scala is silently quadratic. Six profiling rounds on `uniml/markdown`
  found six hot spots and every one was that shape; the fixes shipped (~13× cumulative, 256 KB
  173.4 s → 13.0 s) but the curve never changed slope — each fix only revealed the next instance.
  **Phase 1 DONE 2026-08-31 — and it overturned this item's own favourite** (results in the spec): `Rc<Vec>` CoW degenerates to `Vec` exactly (×4.1) as soon as a clone outlives the next mutation, which is what a tree-building parser does; only `im::Vector` stays linear, at a 6–9× read-path tax. It also surfaced a cheaper third option now promoted above (`ssc-rust-reduce-clone-volume`), so PHASE 2 IS NOT STARTED pending that. Original framing: **a MEASUREMENT, not a change**: `Vec` vs a persistent RRB vector vs copy-on-write
  `Rc<Vec>`, on the real corpus, ratios reported. CoW is the a-priori favourite (this parser
  iterates and indexes constantly, which is where RRB pays its constant, while what the six
  findings actually needed was cheap APPEND and cheap SHARE). "No candidate wins" is a valid
  outcome that closes the item with the table. Removing `MAX_MARKDOWN_TREE_BYTES` and unblocking
  RAG phase 2 is what phase 2 of this buys.
- [ ] **ssc-rust-reduce-clone-volume** — **promoted ahead of the representation change by phase-1
  measurement** (`docs/specs/ssc-rust-persistent-vector.md` § Results). The emitted markdown
  parser has **1378 `.clone()` sites against 95 `.push()`** — the dominant cost is `cloneIfMoved`
  and the by-value calling convention being defensive because they cannot prove a value is dead,
  and a microbenchmark reproduces the parser's own ~×4 curve **from clone volume alone**. Measure
  how many of the 1378 are provably unnecessary (single-use, or the last read before the value
  goes out of scope), then remove those. Cheapest of the three options, needs no dependency, and
  — unlike adopting a persistent structure — costs nothing on the read path. The two fixes already
  shipped in this series (self-append → `push`, self-extend → `extend`) are exactly this shape and
  were worth 2–4× each. Re-measure after: if the curve goes linear,
  `ssc-rust-persistent-vector` closes without a representation change at all.

  **Update 2026-08-31 — the premise held, and the curve DID go linear.** The biggest single step
  was not proving clones dead but making them unnecessary: read-only `Vec`/`String` parameters of
  every def are now taken by shared reference (`uniml-treevm-quadratic-frames`), so the by-value
  calling convention this entry names as the cause no longer applies to them — 59 `&Vec` and 70
  `&String` parameters in the emitted crate today. Ordinary documents are now ~×2 per doubling on
  both ASCII and non-ASCII. Clone volume is still high in absolute terms (1414 sites against 101
  `push`), so the counting idea keeps its value — but it is no longer on the critical path, and
  the next measurement should establish whether the REMAINING clones cost anything before more
  machinery is built for them. `uniml-single-frame-residual-superlinear` is the one shape still
  above ×2 and is the honest place to look first.
- [x] **ssc-rust-lifted-def-return-types — DONE 2026-09-01, and the entry was one-third stale.**
  Verified against code first: the `.isDefined` symptom was ALREADY patched around locally
  (`isOptionExpr`'s `nestedLocalDefDecltpe`); the LIVE halves were Vec (`picked.nonEmpty` refused
  as "collection member, not a field") and String (`tag.length` silently took byte-len). Fix =
  nested defs join `_returnTypes` under the existing bare-name collision discipline +
  `defReturnsString` additive fallback (scalascript `726d07176`). 517/517 goldens (1 new), zero
  churn; vendored uniml-md byte-identical — purely enabling for future .ssc sources.
  Original entry: small backend gap, found twice now: `.isDefined` /
  `.get` (and any other return-type-driven lowering) do not resolve on a call to a LIFTED LOCAL
  def, because its declared return type never reaches the global `_returnTypes` table — the
  emitted code says `no field isDefined on type Option<T>`. Both times the workaround was to
  promote the local def to a class method. Fix: record a lifted def's own declared return type
  where the lifting already computes its parameter types.
- [x] **uniml-treevm-quadratic-frames — DONE 2026-08-31, and the filed cause was WRONG.** The
  symptom reproduced far more cleanly than this entry described — through plain markdown, no Rust
  dialect needed: the same 64 KB as many small blocks parsed in 0.171 s and as one block in
  35.203 s, **206× on document SHAPE alone**. But it was not `TreeVm.addTop` and not frame edges.
  Splitting the two variables apart settled it: the same bytes in ONE frame cost 2.076 s as a
  single long line and 0.068 s wrapped at 80 columns — 30× on LINE LENGTH, with the frame identical.
  `sample` then put 100% of frames under one leaf, `MarkdownInlines.tokenize`, in a memmove.
  The cause was a match-arm guard `isExtendedAutolinkStart(content, i, pending)` evaluated at every
  character, with `content` — the whole line — taken by value, so the call copied 48 KB per
  character. Fixed in the BACKEND, not the dialect (scalascript `37906bcab`): `_refParamPos` now
  takes read-only `Vec`/`String` parameters of every def by shared reference, not only class
  methods'. One 64 KB line 35.203 → 0.220 s (160×); Cyrillic 512 KB 1.301 → 0.263 s; emoji
  3.193 → 0.492 s; SPRINT.md 3.244 → 2.707 s. Getting 160 emitted-code errors to 0 took five
  separate fixes, each documented at its site — the sharpest being that the widened rewrite
  composed with `cloneIfMoved` into `&xs.clone()`, so the signature said `&Vec` while the call
  still copied and the optimisation read as applied while measuring as absent.
- [x] **uniml-single-frame-residual-superlinear — CLOSED 2026-09-01: the curve is LINEAR.**
  `uniml-inline-tokenize-codeunits` (docs/specs/uniml-inline-tokenize-codeunits.md) took #6, the
  last profiled contributor: the inline tokenizer + its scanners walked the single giant
  paragraph of a blank-line-free doc via O(i) charAt/substring/indexOf emulation; all of
  MarkdownInlines now indexes a code-unit Vector (scalascript `4aaeb385c`, backend charseq-param
  fix `f987fb16d`). Doubling ratio 1.84–1.99 at 3.2k→25.6k lines; 64 KB worst case
  **29.3 s (campaign start) → 0.035 s, ×837**; 25.6k lines 0.137 s; long-line docs 0.211 →
  0.012 s. Found on the way: backend printed a Vector[Char] PARAM's mkString as decimal code
  points (fixed, golden 513/513); rag_embed lock-test flake was the spawn fork→exec fd-window
  (test states the tolerance now). Six stacked superlinear contributors total, each found by
  profile-after-fix, none by theory.
  History of the campaign below.
  **(superseded) FIVE quadratics down, ONE left (tokenize) 2026-09-01.**
  `ssc-local-last-use-move` (docs/specs/ssc-local-last-use-move.md) landed #3+#4+#5 in one pass:
  #3 backend local-last-use moves (`_localLastUseMoves`, position-keyed) — step's exit
  constructor moves stack/topEdges/roots; #4 backend loop-local FIELD moves
  (`_localFieldMovePos`) — `vmState = stepped.state` no longer deep-clones the whole VmState per
  token in BOTH UniML.parse driver loops (position-keyed because the two sibling loops both name
  their local `stepped`); #5 source-level — MarkdownLexer.split's tail recursion was a REAL
  recursion on the Rust lane concatenating the whole lines vector per line; now imperative
  while + in-place push. Worst case 64 KB/6400 lines **10.59 s → 0.232 s (×45.6)** same-window;
  long-line docs unchanged; scalascript `86449b44b`+`cf3a60ec8`+`3377ceb77`, goldens 512/512,
  rozum-agent 164/164. REMAINING — contributor #6, profiled exactly at 25,600 lines: a
  blank-line-free doc is ONE paragraph and `MarkdownInlines_parse`→`tokenize` walks its whole
  content via `_str_code_at`/`_str_substring` (O(i) JVM-code-unit emulation; 3277+1543 of ~5000
  samples). Fix = the split precedent (index a code-unit Vector once), applied to tokenize and
  its String+index helpers (`isExtendedAutolinkStart` et al.) — a signatures refactor of the
  inline lexer, own claim. Curve till then ~×3.7/doubling on the pathological shape only; real
  docs have blank lines → small paragraphs. The for-loop clone gap
  filed here is FIXED (ssc-for-loop-clone-gap, scalascript `1e62d064e`): for-do mirrors
  while's inWhileLoop+loopExempt (generator var exempt), for-yield takes enteringClosure;
  516/516 goldens, zero churn, vendored crate byte-identical — purely protective.
  Original entry follows for the instrument and history.
  **(superseded) TWO of THREE quadratics fixed 2026-09-01.**
  Worst case 64 KB (6400 short lines): 29.3 s → 12.9 s (hot-top, #1) → **8.7 s**
  (ssc-owned-field-move, #2: the backend now MOVES a single-read field of an owned by-value
  param when every bare use precedes it — scalascript `7a5551b99` on
  feature/treevm-top-edges-prestage10; 507/507 backend goldens, incl. the position-ordered
  QName pin). The curve is STILL ~×3.9 because of contributor #3, visible verbatim in the
  generated `step`'s exit: `VmState { stack: stack.clone(), topEdges: topEdges.clone(), … }` —
  last-use clones of LOCALS in the returned constructor. That needs a general liveness pass
  (locals, not just param fields) — the next, bigger backend step. In-code note worth knowing:
  `Map.collect { case ((k1, k2), 1) => … }` silently dropped the FIRST entry (Scala 3 corner,
  verified against a plain filter) — spelled imperatively in `collectSingleReadOwnedFields`.
  Original entry follows for the instrument and history.
  It was TWO quadratics stacked (spec § Phase 2 pass). #1, the per-token `addTop` frame rebuild
  (the shape the old entry suspected!): FIXED at the source with the hot-top invariant
  (`VmState.topEdges`; scalascript `feature/treevm-top-edges-prestage10`) — worst case 64 KB
  29.3 s → 12.9 s, frame series ~1.7×. #2, still open and now EXACT: the generated `step`
  deep-clones `state.topEdges` on entry AND exit — two O(k) clones per token — for an owned
  parameter's field at its provably last use. Backend fix (field move at last use) or a scoped
  shared representation for that one accumulator field; until then the curve stays ×3.9 on
  single-frame documents while ordinary documents remain ~×2. NOTE: stage-10 currently breaks
  the whole Rust lane (~86 rustc errors, owner pinged); the vendored crate is regenerated from
  the pre-stage-10 base with the hot-top fix.
- [x] **constrained-path-prefix-reuse — SHIPPED 2026-09-01** (docs/specs/constrained-prefix-reuse.md; measured ×11: ttft 4.7 s → 410 ms on a 5.6k-token continuation turn, live service). Diagnosis trail below.
  Every agent turn on this machine re-prefills its whole conversation: ~1.2 ms/token, ttft 5–10 s
  per turn at 6–8k prompt tokens, growing with history — this is what made every RAG A/B run hit
  its 240 s timeout in both arms, and it taxes every claude/codex session all day. The diagnosis
  took four instrumented layers, each eliminating a hypothesis: NOT the batch path stealing jobs
  (agent turns have tools → `should_constrain` → they are serial); NOT the VL gate (no images →
  `vl_mm=None`); NOT the store or matcher (a lone /v1/messages pair reuses 6008/6030 tokens,
  ttft 357 ms — the machinery is byte-exact and LIVE for unconstrained requests). The cause is
  one early return: `run_job` routes tool-bearing jobs to `run_constrained_{dense,hybrid}` BEFORE
  the prefix-reuse block, and `prefill_job_{dense,hybrid}` build a FRESH cache and prefill the
  full prompt every time. Upstream-fork support already exists (`Generate.prefill_snapshot` at
  the conv boundary, `LayerCache::{truncate,snapshot,restore}`), so the fix is to give the
  constrained prefills the same take→truncate/restore→suffix-prefill→put cycle `run_job` has,
  threading `&mut PrefixStore` through. Expected ~8–15× on agent-turn ttft. Diagnostic tooling
  is in-tree behind `ROZUM_PREFIX_DEBUG` (store-miss print, BATCH_GATE decisions, path-entry
  markers). Also fixed on the way: `is_batchable`'s long-conversation gate under-counted (Text
  blocks only; an agent turn's volume is ToolResult/ToolUse) — now counted, though moot for
  tool-bearing turns which are serial anyway.
- [x] **rag-coexistence — DONE 2026-09-01.** The operator asked whether two servers on one store
  coordinate; the audit found two real gaps. Index writes were a plain overwrite — a reader
  during a sibling's refresh could see a torn file; now temp+rename like the vector store.
  Embedding ran OUTSIDE any lock (the build lock releases before the embed phase), so two
  servers both warming up meant double GPU work; both paths now share `rag-embed.lock`,
  try-and-skip, with a test at the lock. Registering both MCP servers with one agent is
  non-conflicting (clients namespace by server) but redundant — guidance is either/or.
- [x] **rag-standalone-mcp — DONE 2026-09-01.** `rozum rag mcp` = retrieval as its own stdio MCP
  server, meetings-free, for `{ "command": "rozum", "args": ["rag", "mcp"] }`. Self-contained in
  the engine binary via the in-process embedding hook — chunk, embed and serve in ONE process, no
  daemon, no gateway. Shares the proxy's whole contract (warmup off the request path, refresh
  before search, mid-session re-embed, no_index hint, `fused` honesty, stale reporting) through
  `rag_embed::gateway_less_warmup` + `rag_mcp::RagServer`. E2E-verified in a fresh repo with
  nothing else running: `fused: true`, transcript↔append found. A test pins that it serves
  `rag.search` and NOTHING else. The meeting proxy's tool is untouched.
- [x] **rag-store-seam — DONE 2026-09-01.** `VectorIndex` trait = the seam an external vector
  store (Qdrant/Lance/remote) implements; `VecStore` is its in-process impl and both consumers
  call through it. Three safety decisions in the shape: ids-only (text never duplicated into a
  store), L2-normalised f32 at the boundary whatever the store keeps inside, and store state
  always derivable from the manifest — a cache by construction, so a dead store degrades to BM25,
  never to wrong answers. Top-k via `select_nth_unstable` (O(n)) instead of full sort. CLI
  `rozum rag search` now FUSES through the in-process embedder hook (same binary, no gateway) —
  parity with the MCP tool verified live on the same query, same top hits.
- [x] **rag-vector-layer — DONE 2026-09-01** (operator's questions about search/storage/DBs,
  answered by measurement). Exact cosine stays: warm fused `rag.search` is 42–193 ms e2e, the
  sweep itself ~10–20 ms at 10.6k×1024 — ANN/external vector DBs buy nothing at this scale and
  cost a resident service; revisit threshold recorded (100k+ chunks → in-process HNSW, not a
  server). Storage now `RZV2` i8+scale, 4× smaller (42→11 MB per store AND per live proxy copy),
  quality swept before switching: f32 11 & 20, f16 11 & 20, i8 12 & 20 of 26 — nothing lost;
  legacy RZV1 still loads, upgrades on next save. Fixed the mid-session freshness gap: a
  refresh that re-chunks now kicks a background embed (one in flight), so an edited file regains
  semantic retrieval in seconds, not at next proxy start. Proxy MCP instructions now name
  rag.search and when NOT to use it.

### RAG: agents doing ORDINARY CODE WORK must use it (operator, 2026-08-31)

The operator set both the goal and the primary case: **RAG is first of all for agents working
on code day to day** — not the chat assistant, not meetings. Everything below is ordered by
that, not by how interesting the work is.

**The bar is not "RAG works", it is "RAG beats what the agent already has."** A coding agent
already has grep, glob and Read, and they are exact, instant and never stale. Retrieval earns a
call only where those lose: the exact token is unknown (a concept, a symptom, "where is
admission decided"), the answer is spread over files that share no literal string, or the agent
is new to an area and needs the shape before the detail. A tool that returns what `grep -rn`
would have returned, slower and less precisely, will be correctly ignored. Design and evaluate
against that bar explicitly — including a "should this have been grep?" check in the eval set.

Priority is set by one measured fact: **`search_documents` is written and registered NOWHERE.**
`crates/rozum-agent/src/rag_lite.rs:143` builds the tool; the only callers are its own unit test
and the CLI — no gateway, no MCP server, no agent loop. `rag-embeddings-backend` was the item
asked for and is P2 here because better ranking behind a tool nobody is served is worth nothing.

Baseline measured 2026-08-31: 2648 files → 46,733 chunks, 31.4 MB index, 33 s full build, 0.35 s
per search (re-reads the whole 31 MB from disk every call). Code retrieval quality is the known
weak spot and it is BM25's, not the chunker's: `rag search "read-only parameter shared reference"`
returns `struct SandboxPolicy` first — word overlap with no notion of meaning — while the same
index answers prose queries correctly (`"residency admission queue"` → the right spec, top hit).

- [ ] **rag-expose-to-agents (P0)** — the one that makes the rest matter. Two surfaces, both
  already built and both unconnected:
  1. **MCP.** `crates/rozum-meeting/src/meeting/mcp_server.rs` is the server every agent in this
     project is already attached to (`meeting.*`, `rooms.*`). One more `#[tool]` — `rag.search`
     — and every claude/codex/nadia session gets project retrieval with no client config change.
     Hold the index in memory in the server: 0.35 s per call is a disk reload, not a search.
  2. **In-process agent loop.** `MultiToolSource` already composes `CallbackToolSource` with
     `McpToolSource` (`docs/specs/mcp-toolsource.md`), so `rag_lite::…` needs registering, not
     writing. This is the path the local Qwen assistant and the cascade take.
  NOT via gateway-side injection into every request: `reference` tool-schema bloat is measured
  (~4.9K tokens of schema per request, which is why `--lean` exists), and a tool the client did
  not ask for is exactly that cost on every call. MCP is opt-in by construction.
- [x] **rag-index-freshness (P1) — DONE 2026-08-31.** Incremental reindex (mtime+length, chunks
  grouped per file in an on-disk v2 manifest), a refresh before every `rag.search`, and a
  background warmup at proxy startup so nobody waits for the first build. Measured on a 490-file
  tree: full 23.50 s, incremental-no-change **0.02 s** (1175×), one edited file 0.51 s, with
  byte-identical output either way. Two things the numbers forced, both wrong in the first cut:
  a no-op pass must NOT rewrite the file (the proxy reloads on mtime, so an idempotent rewrite
  would re-read 31 MB per search — the check costing more than the search), and auto-refresh must
  not perform the FIRST build (23.5 s inside a tool call in every fresh checkout; caught by P0's
  own `no_index` test, which began failing because the refresh had created the index it asserted
  the absence of). Building moved to a background warmup, guarded by a cross-process `try_lock`
  so N agents starting together do it once. Spec: `docs/specs/rag-index-freshness.md`.
- [x] **rag-index-scope — DONE 2026-08-31, and it was not small.** The index was **77% machine
  output**: `scripts/bench/results/` (gitignored benchmark transcripts) was 36,034 of 46,733
  chunks and 15.9 MB of 31.4 MB. Replaced the hardcoded `SKIP_DIRS` denylist with the project's
  OWN declaration of what is source — `git ls-files --cached --others --exclude-standard`, one
  subprocess, 0.017 s — falling back to the directory walk outside a repo. Untracked-but-not-
  ignored files stay IN, deliberately: a file the agent just created is what it will ask about
  next, and "tracked only" would undo the incremental freshness. Result: **46,733 → 10,530
  chunks, 31.4 → 9.3 MB**; the probe query that returned an archived bench transcript now returns
  `daemon_proxy.rs#fn forward` and `#fn forward_raw`. Build time barely moved (33 → 22 s) because
  the noise took the cheap paragraph path — the win is ranking and size, not speed.
  Fixed on the way: `refresh_in_background` treated ANY `try_lock` error as "a sibling is
  building", so an I/O error silently became a skipped build; `WouldBlock` and a real error are
  now separate branches. And a test I added in P1 was flaky (in-process release-then-reacquire of
  an flock; ~1 run in 3) — rewritten to assert both branches on separate trees.
- [x] **rag-self-reference-contamination — CLOSED 2026-08-31 by measurement, not by machinery.**
  Filed when `docs/specs/rag-expose-to-agents.md` took the top slot for two of three probe queries
  purely because it quoted them, and it was real. It is now absorbed by the implementation slots
  from `rag-code-retrieval-quality`: prose competes for ONE of five slots, so a document quoting a
  question can no longer displace an answer. Measured over the 26-question set with and without
  excluding the two self-referential files — **9/26 top-1 and 15/26 top-5, identical either way**.
  The gate keeps its exclusion anyway: a gate that can score itself is wrong on principle even
  while it happens to make no difference, and the day prose gets more slots it would matter again.
  Nothing to build; the levers this entry proposed (down-weighting `docs/specs/**`, preferring code
  for code-shaped queries) are NOT needed and would cost prose retrieval, which measures well.
- [x] **rag-code-retrieval-quality — MEASURED AND IMPROVED 2026-08-31; the ceiling is now named.**
  Built the eval set first (`crates/rozum-agent/tests/rag-eval.json`, 20 questions that never name
  the symbol they ask about) and it paid immediately: **top-1 3/20 → 8/20, top-5 9/20 → 9/20**.
  Two free levers, no model. (1) A chunk's identifier — `fn detect_project`, a heading — was not
  indexed at all; it is now a 3× field, split on snake/camel case. Worth +1 alone. (2) The finding
  that matters: **prose outranks the code it describes** — a spec mentions the query's words more
  often than the function implementing them — so `search_balanced` reserves most of `k` for code
  and does NOT re-sort by score (an earlier version did, which put prose back on top and made the
  slots decoration). top-5 not moving is the honest half: for 11 of 20 the answer scores ZERO,
  because BM25 matches words and these questions share none with their answers ("resident" ≠
  "residency"). Selection cannot reach a chunk that scored nothing. Spec:
  `docs/specs/rag-code-retrieval-quality.md`.
- [x] **rag-stemming-or-synonyms — MEASURED AND REJECTED 2026-08-31, same day it was filed.** It
  was filed against a diagnosis that turned out to be WRONG (see below), and once measured it made
  things worse: a conservative suffix stripper took **top-1 from 8/20 to 6/20**, top-5 unchanged.
  In a corpus that is mostly code, collapsing distinct identifiers into one term loses more
  precision than the few word pairs are worth. A curated SYNONYM list remains a different, open
  bet — but it now has to beat 8/20, not fix a vocabulary problem that does not exist.
- [x] **rag-ranking-competitors — DONE 2026-08-31.** Chased what outranks the answers at ranks
  6–95 instead of guessing. Classifying all 100 top-5 slots across the eval: **32 were TEST code**
  and 6 were import blocks — 38% of what an agent saw for "where is this implemented". Tests win
  because their names are English sentences, which is the shape of the question. `search_balanced`
  now reserves slots for IMPLEMENTATION, with tests and prose filling the rest (demoted, never
  dropped — sometimes the test is the answer). Result: **implementation 54 → 80 of 100 slots**,
  while top-1/top-5 stayed 8/20 and 11/20 — the hit-rate metric cannot see it because these
  answers were already in or already out. GOTCHA worth keeping: a first cut detected tests with
  `text.contains("mod tests")` and made top-1 8 → 6, because `chunk_code` tiles a file so the last
  chunk carries the test module; the attribute must OPEN the chunk.
- [x] **rag-rank-next — DONE 2026-08-31: BM25 was biased against implementations.** Guessed that
  long vocabulary-rich chunks were winning; MEASURED the opposite — the chunk beating the answer
  had a median 80 words against the answer's 207, longer in only 5 of 11 cases. `b = 0.75`
  penalises long documents, and a function that does real work is long. Swept: 0.75 → 8/26 & 13/26,
  **0.50 → 9/26 & 15/26**, 0.30 → 7/26 & 15/26, 0.00 → 3/26 & 11/26. Chose 0.5; `k1` left at 1.2
  (1.6 ties, extremes worse). The direction was predicted before the sweep and 0.0 is clearly
  worse, which is what makes it a finding rather than a fit to 26 questions.
- [x] **rag-eval-mid-rank-questions — DONE 2026-08-31.** Six questions added whose answers sat at
  ranks 2–29, then the enlarged set was VALIDATED against the change it must detect: raw BM25
  scores **4/26 top-1** against **8/26** with the implementation slots, where the original twenty
  scored 8 and 8 across that same change. Candidates were screened by measuring where each answer
  actually ranked and keeping the sensitive band — four proposals that already ranked #1 were
  discarded, since a question the ranker already wins measures nothing.
- [x] **rag-ranking-truth — a published diagnosis of mine was wrong, corrected 2026-08-31.** The
  `rag-code-retrieval-quality` spec said the unfound answers "score zero" and that "no re-ranking
  reaches a chunk that scored nothing", which pointed the next agent at embeddings. Measured at
  `k=200`, **nine of the ten rank 6, 7, 15, 24, 36, 58, 79, 95 — only ONE is truly absent.** The
  ceiling is RANKING, not vocabulary. I had inferred "scored zero" from "absent from top-5"
  without querying at a larger k. Two fixes came out of the correction: `#use` chunks were being
  credited with a symbol name (`chunk_code` tiles a file, so its first chunk is the import block —
  short, identifier-dense, and holding the module `//!` doc), which put `store.rs#use` above the
  actual functions — top-5 9/20 → 10/20; and the eval set now CONTAMINATES its own corpus, since
  it and its spec quote every question verbatim and rank first for them, so the gate and the
  runner both exclude those two files — 11/20 honest.
- [x] **rag-smoke-test-self-reference — CLOSED, subsumed.** The narrower ancestor of the entry
  above: `rag search "residency admission"` returned the test that contains that phrase. Both the
  implementation slots (a test now competes in the leftover slots, not the reserved ones) and the
  test demotion in `search_balanced` address it, and the umbrella entry carries the measurement.
- [x] **ssc-rust-string-repr — CLOSED 2026-08-31: its premise was measured and is FALSE.** The
  item said non-ASCII is quadratic because `charAt(i)` costs O(i). Instrumenting the helper says
  otherwise: charAt CALLS grow ×2.00 per input doubling, the slow-path walk work grows ×2.00, and
  **the longest string ever indexed on the slow path is 86 bytes and does not grow with the
  document**. charAt was never the asymptotic problem — it is a constant factor on short strings.
  The migration this justified was also bigger than the item claimed: not "58 emitter sites + 17
  helpers" but ~106 (58 `"String"` + 34 `"Vec<String>"` + 14 `"HashMap<String"`, all type-string
  matches driving inference) plus every construction site.

  The real cause was found with an allocation counter, by its signature — allocation COUNT growing
  ×2.00 (linear) while allocated BYTES grew ×3.74: the same number of allocations, each one
  bigger. `_str_substring`'s general path materialised a `Vec<u16>` of the WHOLE string on every
  call, which the ASCII fast path hid completely. Fixed in ~25 lines of runtime (walk
  `char_indices` to the two offsets and slice; a code-unit index inside a surrogate pair falls
  through to the old path). scalascript `c73b38565`: non-ASCII allocation bytes ×3.74 → **×2.00**,
  3,242 MB → **201 MB** at 256 KB, time 0.970 s → 0.483 s.

  If a future workload genuinely needs O(1) code-unit indexing, the design is still recorded in
  `docs/specs/ssc-rust-string-representation.md` — but nothing measured today asks for it.
- [x] **uniml-nonascii-residual-superlinear** — DONE 2026-08-31 (scalascript `60d824c9a`, vendored
  here). The residual term was `MdLine.split`, and it was introduced by an earlier commit in this
  same series: replacing per-character `substring` with one slice per line was a real improvement,
  but `substring` counts from the START of the string, so on a backend where that mapping is a walk
  it costs O(index) — growing with position in the file. Quadratic again with a smaller constant,
  invisible on ASCII because the fast path is a vectorised prefix check. `chars` is already the
  code-unit vector the loop indexes, so slicing THAT is O(line length) and needs no mapping at all.
  Found by the method this entry proposed — instrumenting every string helper at two input sizes:
  `substring` calls grew ×4.00 with the input while its walk distance grew ×15.20 and the longest
  string handed to it was the whole document, whereas `code_at`/`length`/`substring_from` were
  ×4.00 on both counts. Non-ASCII 256 KB 0.970 s → 0.108 s; both curves now ~2× per doubling
  (Cyrillic ×1.93–2.00, emoji ×1.97–2.02). `MAX_MARKDOWN_TREE_BYTES` re-justified as a LATENCY
  bound rather than a super-linearity guard.

- [x] **rag-uniml-unenforced-limits — DONE 2026-08-31, and it was FOUR fields, not two.** This
  entry named `maxBlocks` and `maxLineCodePoints`; grepping every field of `MarkdownLimits` found
  `maxDelimiterRun` and `maxFenceCodePoints` dead as well — only `maxSourceCodePoints` and
  `maxReferences` were ever read, against a struct whose own doc promises "finite bounds guarding
  every buffer, stack and delegated region". The two this entry named are now enforced
  (scalascript `3538a1c95`, vendored here): `maxLineCodePoints` once after the split against the
  longest line, `maxBlocks` counted at `track`, which sees every `Open` — so it counts block
  OPENINGS, one frame and one branch each. Both halt in the same shape as the source limit: no
  tokens plus a fatal diagnostic, since a truncated stream would sit in a tree that looks
  complete. Tested at BOTH layers — the Scala suite and, separately, through the vendored Rust
  crate, because a limit enforced in the source and lost in the ssc→Rust lowering is the same
  defect one layer down. Also corrected a comment in `rag_chunk.rs` that named `maxBlocks` as the
  example of a working limit while it was dead (the test it describes has always used `maxNodes`,
  which is enforced — the test was sound, the comment was not).
- [x] **uniml-remaining-dead-limits — DONE 2026-09-01** (scalascript `bf602f46e`, vendored here).
  `maxDelimiterRun` is checked in the BLOCK driver rather than threaded through the inline lexer:
  a delimiter run cannot cross a line ending, so the longest run in any line bounds what the
  inline lexer will ever walk — one O(line) scan where `limits` already lives.
  `maxFenceCodePoints` counts the OPEN fence's body, reset at each FenceOpen; per-fence and not
  cumulative is pinned by a test, because a cumulative counter would make the limit depend on how
  many code blocks a document has. All six `MarkdownLimits` fields are now enforced, tested at
  BOTH layers (Scala suite + the vendored Rust crate rozum actually runs).
- [x] **rag-syntactic-rust-dialect — DONE.** `uniml/rust` exists in scalascript (`RustLexer` +
  `RustDialect`, structural: keyword + brace-depth item finder, string/comment aware) and
  `rag_chunk::chunk_code` parses `.rs` through it, so code is chunked by item rather than by
  paragraph. Phase 2 of `docs/specs/syntactic-rag.md` is shipped.
- [x] **rag-doc-comment-field — TRIED AND REJECTED 2026-08-31.** A code chunk is a byte-exact
  source slice, so most of its words are syntax: `detect_project` is 55 words of which the meaning
  is one `///` line. Extracting the leading doc block as a boosted field (like the identifier one,
  which did help) was swept 0/1/2/3/5 and **every non-zero weight made top-1 worse**: 9/26 → 7–8/26,
  top-5 15/26 → 14–16/26. Those words are already in the chunk text and already counted, so the
  boost multiplies existing signal rather than adding new — for every code chunk equally,
  competitors included. Nothing is discriminated; the field is just scaled. Recorded because the
  idea is obvious and the story behind it is plausible, so it will be proposed again.
- [x] **rag-embeddings-backend — BUILT AND SHIPPED 2026-09-01** (operator decided build after the
  spike series took the trade from +1/+2 at 12 min to +3/+5-6 at 4.6 min). Design:
  `docs/specs/rag-embeddings-impl.md`. The MODEL runs in the GATEWAY (`rozum-mlx::embedder`, own
  thread, lazy; reached via new `/v1/embeddings`, OpenAI-shaped + `"query":true` extension),
  wired through a `rozum_core::embedding` OnceLock hook so no gateway→mlx crate edge; the proxy's
  warmup embeds missing vectors through it (batch 64, partial saves = interruptible) and
  `rag.search` fuses (RRF k=10, emb weight 2) with per-call fallback to BM25, reported as
  `"fused": true|false`. Vectors in `.rozum/rag-vectors.bin` (binary: JSON floats measured 128 MB
  vs ~45 MB), pruned+carried by chunk id, dimension change = model change = discard.
  E2E VERIFIED with the real model through the real binaries: fresh project → warmup builds
  index + vectors → next session's FIRST rag.search answers fused:true with `notes.md#storage`
  and `fn append` top for "how does a room transcript get written to disk" — the exact
  transcript↔append gap that justified the whole item. Two in-process rules that MUST hold:
  embedder never calls `apply_retain_env` (keyed to the CHAT family, process-wide) and never
  `set_cache_limit` (the gateway owns cache policy; the spike's own 512 MB limit would throttle
  the chat model). `ROZUM_EMBED_MODEL` overrides the checkpoint; `ROZUM_RAG_EMBED=0` disables.
  Follow-up (small): fold the embed model's ~400 MB into `update_own_footprint` ledger billing.
- [ ] **teach-collect** — phase 0, independently shippable and FIRST (SFT under ~100
  quality pairs overfits — the dataset must accumulate ahead of any trainer): teach-mode
  toggles, 👍/👎/correction affordances on Telegram (`/teach on|off`) + UCC + CLI/TUI, one
  shared JSONL dataset (`~/.rozum/teach/`, 0600), `rozum teach export|stats`.
- [ ] **teach-train-rust** — phase 1: LoRA training in the vendored mlx-rs stack. Hard
  prerequisite, named: mlx-c `value_and_grad` bindings + AdamW + gradient flow through
  LoRA branches over the frozen 4-bit qwen3_5 forward. Then `rozum teach train` →
  versioned adapters with manifests. Training obeys the residency-admission ledger.
- [ ] **teach-serve-adapters** — phase 2: load-time weight folding of an adapter into the
  resident model (zero hot-path cost), `rozum teach apply|rollback|list`, adapter carried
  on the model spec so panels show what serves, eval gate (`teach eval`) wired to the
  bench matrix — `apply` refuses on regression past threshold.
- [ ] **teach-dpo** — phase 3: when corrected pairs accumulate, preference training
  (correction ≻ original) over plain SFT; same trainer plumbing, different loss.

## Rescued from the parked bucket (triage 2026-08-08)

Both were moved into *Deprioritised — the model is frozen* on 2026-08-04, and neither carries a
`Parked because` line, because neither depends on a model. Sorting by section rather than by item
is how that happens.

- [x] **test-cell-repair-failfast — CLOSED 2026-08-09, not done: BOTH levers already shipped, and the
  obvious way to "finish" the second one is a documented regression.** Entry written 2026-07-05,
  before the loop-breaker existed. No code changed; the measurement is the deliverable.
  - *Lever 2 (bonus attempt)* — shipped, and nobody closed the entry. `scripts/bench/agentic.sh:940-949`
    (`bonus_used`, marked `R2.5`) grants exactly one extra attempt when `File has not been read yet`
    first appears on the final attempt. Its code comment is this entry's text, near-verbatim.
  - *Lever 1 (detect the churn live and fail-fast)* — shipped too, one layer DOWN, where it belongs:
    the gateway breaks the loop at the source for every agent and every harness, not just the bench.
    `chat_or_loopbreak` (`crates/rozum-gateway/src/serving.rs:117`) runs `detect_stuck_loop` on every
    chat, unconditionally — no env gate, no default-off.
  - **Live evidence, `~/.rozum/gateway.jsonl` (92,079 events): 879 `stuck_loop_broken`.** By signature:
    478 windowed-identical (sig 4), **229 edit-churn (sig 3 — precisely this entry's family)**, 172
    cycled-output (sig 1/2). The looping tools: bash 274, Write 64, exec_command 61, Edit 26, Read 10.
    The run no longer burns RUN_TIMEOUT because the loop is stopped before the timeout can fire.
  - **The trap, and the reason this is worth reading rather than just deleting.** The harness monitor
    `no_progress_monitor` (agentic.sh:183) has a deliberately NARROW predicate: the last `NP_REPEAT=5`
    tool signatures must be identical *consecutively*. The obvious "improvement" is to widen it to the
    gateway's proven shape (`TOOL_WINDOW=12`, `TOOL_REPEAT_THRESHOLD=4`). **Do not** — not as a
    straight port. The gateway's predicate has a second conjunct the bash monitor cannot see: *AND
    the result never changed*. `loopbreak.rs:248-253` records that matching without the result half
    was a MEASURED defect — on the 2026-07-31 matrix it cut 11 of nadia's 16 cells and 6 of codex's,
    because an agent instructed to VERIFY re-runs the same `cargo test` on purpose. Identical command
    with differing output is the healthy fix→test→fix rhythm; only identical output too means nothing
    moved. The monitor reads `tool_use` blocks only, so it has the command and not the result.
  - *Parked alternative, if this ever comes back:* teach the monitor to read `tool_result` blocks from
    the stream-json `user` messages, THEN widen the window. That is the only version that is safe, and
    it buys a second line of defence behind a source-level fix that already fires 879 times — which is
    why it is parked and not queued.

  *(was under: Matrix improvement levers (found 2026-07-05 during the matri)*

  *(this entry existed TWICE — in “Matrix improvement levers” and again in “Deprioritised”, because 2026-08-04 copied instead of moving. Merged 2026-08-08.)*
## rozum-core::share tests read the real machine (found 2026-08-05)

- [x] **share-tests-isolate — DONE, verified 2026-08-09.** The strikethrough was right and the checkbox
  was left open, so it still read as a live red on master. It is green: `cargo test -p rozum-core
  share::` → **20 passed, 0 failed** (7.6s, default parallelism). And structurally fixed, which matters
  more than one green run for a flake whose colour depended on the machine: the tests now point
  `XDG_STATE_HOME` at a temp dir (`share.rs:1451,1483`), INJECT the readings they used to take from the
  live host — `ROZUM_GATEWAY_AVAILABLE_RAM_BYTES`, `ROZUM_GATEWAY_RAM_BUDGET_BYTES`,
  `ROZUM_HOST_PRESSURE` (`:1484-1498`) — and serialize the env-mutating ones behind a lock (`:269`).
  The absurd "actual free RAM ~1099511627776 MB" in the original report was exactly that missing
  injection. Original text follows.
  *(original report, NOT a queue item)* — `cargo test -p rozum-core share::` failed on master: 7
  failures single-threaded, 8/7/10 across three parallel runs. The failure text shows the tests
  seeing a live ledger and an absurd "actual free RAM ~1099511627776 MB", i.e. they read process-wide
  state instead of a fixture. The same workspace was 850/0 twice earlier today, so the suite's colour
  depends on what the machine happens to be doing — which is the definition of a red nobody can act
  on. Point them at a temp state dir the way the other suites do.

## Matrix improvement levers (found 2026-07-05 during the matrix-hygiene analysis; evidence in agentic-ucc-1783166880)

The honest read of the curated tier is claude 89% / codex 33% / opencode 47% (summarize_matrix.py now
shows this + fail-mode rollup). The two big NON-model levers, ranked:

## Meetings → product-support / incident platform (STRATEGIC — operator 2026-06-28)

**Direction:** rozum meetings are not just agent chat — they are the substrate for **product support
with escalation + resolving + per-incident context collection**, where AI agents are first-class
participants (triage, gather context, escalate, resolve) alongside humans. Think Slack+Zendesk+PagerDuty,
agent-native. A room/thread IS an incident; context (logs, history, related messages, artifacts) accretes
to it; messages carry support metadata; agents drive it toward resolution. Big perspective tasks, built
on the existing meeting stack (`docs/specs/agent-meetings-daemon.md`, `meeting-identity-roster.md`,
`meeting-mention-inbox.md`, `meetings-rest-read.md`; daily disk-backed rooms, session-token identity,
single-writer daemon). Each item below is its own spec+build later — listed to set the trajectory.

- [x] **mtg-resolving — DONE 2026-08-08, mostly by finding it already built.** The state machine
  (open → triaging → escalated → resolved → closed), escalation with an assignee and a note, and
  resolution records all existed and were tested; the entry had aged past its own subject. What was
  missing was small, and one part of it was WRONG: time-to-resolve measured `updated_ts - created_ts`,
  and `updated_ts` moves on any later message, pin or owner change — the same incident reported 4
  minutes or 24.7 hours depending on whether somebody commented the next morning. Fixed with
  `resolved_ts`, plus `reopened`/`escalations` counters and an escalation RATE (the histogram only
  ever showed what is escalated right now). Spec: `docs/specs/incident-resolving.md`.

- [x] **mtg-incident-context — DONE 2026-08-08.** Most of it turned out to be built: `thread_context`
  already assembled the thread record, its messages, participants, timespan, operator-linked messages
  and auto-gathered related context. What was missing was the evidence from OUTSIDE the room — added
  as a gateway-log slice over the incident's own window (with a five-minute lead-in, capped, and
  reporting `matched` next to `shown`) and a machine snapshot written INTO the thread as a message at
  open time. Spec: `docs/specs/incident-evidence.md`.

- [x] **mtg-incident-repro — DONE 2026-08-09.** `rozum meetings incident repro <id>` attaches the
  commit, the TRACKED diff, the failing command and named env vars as one event message in the
  thread. Never untracked or ignored files; a secret in the diff refuses the whole capture (redaction
  is read-time, so the bytes would stay on disk); capped at 256 KB and truncated loudly; manual, never
  automatic — the operator's call. Spec: `docs/specs/incident-repro.md`.


- [x] **oai-seed-never-parsed — DONE 2026-08-14 as BUG-032.** `/v1/chat/completions` dropped the
  client's `seed`: OpenAI defines it, `SamplingParams` carries the field, and no wire dialect
  parsed it — so `apply_determinism`'s comment ("a caller that genuinely sent its own seed keeps
  it") described a branch no HTTP client could reach; the only caller that ever set one was that
  function's own unit test. It was filed separately from BUG-031 because it needed a decision, not
  four lines: honouring a client seed lets an OpenAI-dialect client override `ROZUM_SAMPLING_SEED`,
  the bench's determinism control. **The operator chose to wire it**, and the risk turned out to be
  none — checked rather than assumed: the matrix forces greedy, `temperature = 0` takes the argmax
  branch, and that branch never touches the RNG, so the bench's determinism never depended on a
  seed. The comment is now true. See BUG-032.

- [x] **wire-parameter-coverage — COMPLETE 2026-08-15.** Every sub-item is closed; the last two
  (`previous_response_id`, Anthropic `thinking`) were answered by CAPTURING a real client request
  against a fake endpoint rather than reading code, and both times the entry's assumption about what
  the client sends was wrong. The original text: the rest of the sweep the operator asked for after BUG-031/032
  (2026-08-14), recorded as one entry because it is one question: *which documented request
  parameters does this gateway drop, and which of them are wiring versus missing capability?*
  Measured against the three request structs, not from memory. The two that were pure wiring are
  already fixed (BUG-031 `top_p`/`top_k`, BUG-032 `seed`); the streaming-usage one is BUG-033. What
  is left is capability, and each needs a decision before code:
  - **stop sequences** — **DONE 2026-08-14 as BUG-037**, in the shared `consume_tokens` exactly as
    this entry guessed, with the mid-token case as its own test. Empty stays a strict no-op.
  - [x] **stop-reason-sequence — DONE 2026-08-14 as BUG-040.** The "83 references" that deferred it
    were mostly constructions: the compiler found ONE non-exhaustive match. The wildcards were the
    real question and reviewing them turned up a second defect — the upstream Anthropic shim was
    flattening `stop_sequence` into `end_turn` on the way in.
  - [x] **stop-sequence-which-one — DONE 2026-08-14 as BUG-041.** Losing `Copy` cost THREE compile
    errors, not the wide change the entry priced — and the field turned out to be absent from the
    non-streaming body altogether, not merely null.
  - **`frequency_penalty` / `presence_penalty`** — **DONE 2026-08-14 as BUG-042** for GGUF and x86,
    implemented properly (additive, count-based, generated-tokens-only, clamped to [-2,2]) rather
    than aliased onto `repeat_penalty`.
  - [x] **mlx-openai-penalties — DONE 2026-08-15.** Fork `sergey-scherbina/mlx-rs` 54acd697 (two
    fields, a count-based penalty, and `keeps_history()` replacing the inline predicate in eleven
    generators), pins bumped, threaded through the dense/hybrid/constrained paths. The old entry: The MLX
    backend samples inside the vendored `mlx-lm` graph (`SamplerOpts { temp, top_p, top_k,
    repeat_penalty }` in `sergey-scherbina/mlx-rs`), so honouring the pair there means two fields, a
    count-based penalty beside `apply_repeat_penalty`, a fork push and a rev bump in
    `Cargo.toml` — plus an MLX rebuild to verify. The fork already keeps the full token history
    (`self.history`), so the counts are available. Until then the backend logs `sampling_unsupported`
    once rather than ignoring the request in silence.
  - **`repeat_penalty` has the opposite problem** — **DONE 2026-08-14 as BUG-036**, wired as
    `repetition_penalty` on both OpenAI dialects. It went deeper than "unset": two of the three MLX
    batching-admission counters were reporting on paths no client could trigger.
  - **structured output on `/v1/responses`** — **DONE 2026-08-14 as BUG-034.** It was the closest to
    a real bug of the set, and it was one: `text.format` was unparsed while Chat's `response_format`
    worked, so the same capability existed on one OpenAI dialect and not the other. One parser now
    reads either nesting.
  - [x] **Anthropic `thinking` — MEASURED AND DONE 2026-08-15, and the entry named the wrong
    field.** Claude Code does send `thinking`, but as `{"type":"disabled"}` and
    `{"type":"adaptive","display":"omitted"}` — never `{type: enabled, budget_tokens}`. It is a
    switch, not a budget, and our only lever (`reasoning_effort`) is a LEVEL with no "off": `None`
    means unset and the chat template then applies `medium`, the opposite of `disabled`. So it is
    declared, read, and deliberately not mapped.
    **What the capture actually found was bigger.** Every request carries `output_config`, holding
    both `{"effort": "high"|"xhigh"}` and `{"format": {"type":"json_schema","schema":{…}}}` — so
    structured output existed on a THIRD dialect and was dropped, which is BUG-034's shape one
    dialect on, and the code comment asserting the Messages API "genuinely does not define" it was
    simply wrong. Neither half needed a new parser: `.format` is the nesting `parse_text_format`
    already reads, `.effort` is what `reasoning_effort` already carries.
    **And one gap that only a real capture would show:** the shared validator accepts
    `low|medium|high`, so `xhigh` — a client asking for MORE — parsed to `None` and the template
    fell back to `medium`, giving LESS. Clamped to `high`.
    Method as for `previous_response_id`: a fake endpoint, a real client, the bodies logged. The
    remaining unparsed fields on this dialect are `context_management` and `metadata`, both inert.
  - [x] **`previous_response_id` — MEASURED 2026-08-15, and the answer is that there is nothing to
    fix.** codex does not send it: not on the first turn, not on a continuation. It sends
    `store: false` and RESENDS the whole conversation in `input` — 3 items on the opening request,
    5 after a tool call (the `function_call` and its `function_call_output` appended). So no
    conversation state is dropped by ignoring a field nobody sends, and wiring it would be
    speculative code for a client that has told us, explicitly, to keep no state.
    **How, so nobody has to re-derive it:** a 30-line fake `/v1/responses` on 127.0.0.1 that logs
    each body and answers a valid SSE stream — a `function_call` on the first request so codex has
    to take a second turn, since a continuation is the only place the field could appear. `strings`
    on the codex binary was NOT enough and would have misled: it contains `previous_response_id` six
    times, all in rollout-trace replay code and in prompt text about the API, none in a request.
    **The census that fell out of it, for the one client that uses this endpoint.** codex sends 12
    top-level fields; `/v1/responses` declares 9 of them. The three it drops:
    - `client_metadata` — an installation id. Nothing to act on.
    - `include` — codex sends `[]`, asking for no extra output parts. Nothing to act on.
    - `prompt_cache_key` — a stable per-conversation key, and `ChatRequest.session_id` exists for
      exactly this. **Still not worth wiring**: `session_id` is set to `None` at every one of its
      three producers and read by nobody, and MLX prefix reuse keys on the token ids themselves
      with an LRU of slots, so a key would duplicate a match the content already makes. It becomes
      worth doing the day something needs a conversation identity the content cannot supply.

  The general lesson, which is why this entry exists rather than six scattered ones: **serde drops an
  undeclared field in silence**, so "the client asked for X and X did nothing" produces no error and
  no log line here. Both fixed bugs were found by putting the three dialects' parsed requests side by
  side in one file, not by reading any one of them.

- [x] **nadia-linux-confinement — DONE 2026-08-15 (Landlock).** Writes confined to root/CARGO_HOME/
  TMPDIR//dev sinks, `no_new_privs`, best-effort ABI; degrades LOUDLY when the kernel has no
  Landlock, and the child fails closed if a built ruleset then enforces nothing. The deletion test
  is un-gated so CI proves it on Linux. The old entry, for the reasoning:
- [~] **nadia-linux-confinement (original)** — nadia's exec sandbox was macOS-only. The mechanism is
  `sandbox-exec` (seatbelt), so `confine` defaults to false everywhere else and an agent on Linux
  runs unconfined in its workspace: BUG-017 ("the jail let the agent delete its own workspace") is
  unfixed there. Surfaced while attributing BUG-044's red CI. `exec` now says so once on stderr
  rather than running unconfined in silence, and the test that assumed otherwise is gated to macOS.
  A real fix is Landlock (kernel ≥ 5.13, no privileges needed, and it confines writes by path —
  the same shape as the seatbelt profile) or bubblewrap; both want a Linux box to develop against,
  and writing either from a mac is how the assertion this replaces got platform-shaped in the first
  place.

- [ ] **chain-per-model-executor-tools** (marginal, not urgent) — per-MODEL executor tool curation in the
  chain: a weaker link gets a smaller tool set than a strong one. Today the real levers are already pulled —
  `--lean` cuts the executor surface 33→4 tools and backend planner/verifier tiers run `tools=[]`
  (`cfdefbf`). **Why marginal:** the executor needs the core coding tools regardless of model; trimming
  further risks removing a tool the model needs. **Build only if** a specific weak link is shown to derail
  on a specific tool (e.g. a model that misuses `apply_patch`) → drop that one tool for that one model.
  Needs a per-(model) tool-allow map threaded into the launch/exec path (`src/main.rs` exec_agent) +
  evidence from the matrix that a named model+tool pairing regresses.

- [ ] **chain-target-interactive-confirm** (not urgent) — when `rozum launch` DERIVES a target from the
  prompt (no explicit `ROZUM_VERIFY`) and is UNSURE, confirm it with the operator before running the chain
  against it instead of silently proceeding. Today: the derived target is logged ("derived target — `…`
  (override with ROZUM_VERIFY)") and overridable, which covers the confident case. **Build:** have
  `derive_target` emit a confidence/ambiguity signal (e.g. the model couldn't pin a deterministic check, or
  produced a judge-only criterion) → in an interactive TTY, prompt "use this target? [y/edit/skip]"; in
  non-interactive/autonomous runs, fall through to the logged default (never block a headless run). Gate the
  prompt behind a TTY check so the matrix/cron paths are unaffected.

- [ ] **chain-noncommand-target-kinds** (MUST do eventually, not urgent) — generalize the target beyond the
  cargo-COMMAND kind (`cargo build && [ "$(cargo run -- arg)" = expect ]`). The spec (§ Target) defines four:
  (1) command/script exit-0 ✅ done; (2) **predicate** (a check over the result/filesystem — file exists,
  output matches a regex, a value is in range); (3) **Q&A known-answer** (the prompt has a checkable factual
  answer → compare); (4) **Q&A open → judgment** (no deterministic check → a judge model scores, the weakest
  acceptance, use only when nothing deterministic exists). **Build:** extend `derive_target`'s schema +
  `resolve_verify_cmd`/`run_verify` (`src/main.rs`) to carry a tagged target kind and dispatch per kind;
  keep the precedence deterministic-first (prefer a command/predicate over a judge). Judge-target is the
  escape hatch, not the default — record per-kind so the quality stats don't trust a judge's PASS as much
  as a deterministic one.

## Host safety

- [x] **residency-gate-cap-mlx-sibling-aware — CLOSED 2026-08-09, not done: the entry is stale in
  both halves, and one half asks for something weaker than what shipped.** No code changed; this is
  the reading that closes it.
  - *"still flat `total−8`"* — it is not, and the named line (`~363`) has long since moved.
    `select_mlx_mem_limit_bytes` (`crates/rozum-mlx/src/mlx_native_backend.rs:5117`) resolves
    explicit `ROZUM_MLX_MEM_GB` → **the per-process residency share** → `total−8` only as the
    *lone-gateway fallback*. `src/main.rs:9264` sets that share to `estimate_model_footprint_bytes`
    — the SAME footprint the residency gate reserved — before the worker loads.
  - The proposed formula is *looser* than what is already there. `total−8−committed_by_others`
    still lets one process take everything no sibling has claimed; capping at the model's own
    declared footprint ties it to its own share. Implementing the entry would be a regression.
  - The stated purpose is unreachable by this lever at ANY value: `set_memory_limit` is **soft**
    (evict/wait, then the allocation proceeds anyway — source-proven, memory
    `reference-mlx-memory-cap-semantics`). An "escape-hatch / unknown-path 2nd MLX process" is by
    definition one that never went through admission, so shrinking *our* hint does nothing to it.
    Admission plus `set_cache_limit` are the structural levers, which is what the code's own doc
    comments now say.
  - Lesson, the sixth time this session: an entry's text ages, the code does not. Re-read the code
    the entry names before scheduling the work — the line number moving is the cheap tell.
## MCP (deferred — decide the use, then build)

- [x] **mcp-use — CLOSED 2026-08-09: nothing left to decide or build.** (A) shipped in nadia;
  `mcp-toolsource-dedup` landed (`31df590`); (B) and (C) withdrawn with the measurement below. The
  client SPI stays where it is, used by nadia. Reopen only under the revival condition stated in (B).
  Full reasoning retained.
  *(detail, NOT a queue item)* — **REWRITTEN 2026-08-09 after reading the code: shape (A) already SHIPPED, so the
  open decision is only (B) vs (C).** The path in the old text (`src/mcp_tool_source.rs`) is stale —
  the workspace split moved it to `crates/rozum-agent/src/mcp_tool_source.rs`.
  - **(A) an embedded agent loop that consumes MCP tools — DONE, in production.** `nadia` connects
    operator-configured MCP servers as extra tools: `crates/nadia/src/mcp.rs` (config, selection,
    naming, failure policy) over `rozum_agent::mcp_tool_source::McpToolSource` (transport, handshake,
    call plumbing), wired at `crates/nadia/src/main.rs:186` `connect_mcp`. Its three rules are worth
    reusing for whatever gets built next, because each is a decision, not a detail: **opt-in per run**
    (a config file that merely exists adds nothing — six tools already cost ~1.5–2k schema tokens and
    one server can add a dozen, which dilutes selection for a 4B model); **a named server that will
    not start is a hard error before the loop begins** (a run that silently lost half its tools
    produces a confidently wrong answer); **the jail does not extend to a server** (separate process,
    own access to the machine — the seatbelt confines nadia, not it, and startup says so).
  - **(B) federation and (C) tool-augmentation — RECOMMENDATION WITHDRAWN 2026-08-09. Do not build
    either without a named beneficiary; I could not find one.** I had called (B) "most rozum-shaped";
    that was written before checking what this stack actually optimises for.
    **The evidence against:** rozum's own shipped, default-on lever for local models is `--lean`
    (`src/main.rs:329-338`), and what it does is *remove* MCP tools from the request — it strips the
    meeting-room MCP surface because Claude Code otherwise ships **~33 tool schemas (~4.9K tokens)
    every request**, and cutting to four (~0.8K) is what makes a small model work. With
    channel-wakeup off it adds `--strict-mcp-config` so ALL ambient MCP servers are dropped. nadia's
    MCP module says the same from the other side: six tools already cost ~1.5–2k schema tokens and
    one server can add a dozen, each diluting selection for a 4B model.
    Federation is a tool-MULTIPLIER aimed at a stack whose measured constraint is tool SCARCITY.
    **Who would it serve?** Only someone whose model is big enough that 30+ schemas don't hurt (this
    host is frozen on 4B), whose agent cannot configure its own MCP servers (claude and codex both
    can), and who wants one config point for many agents (`rozum mcp install` already gives that).
    None hold here. **Revive if** the host runs a model where tool-schema cost stops mattering, or a
    consumer appears that speaks MCP to rozum and cannot bring its own servers.
    The half worth keeping is the opposite one — CURATION, not aggregation — and `--lean` is already
    that. See [[reference-cc-tool-schema-bloat]] in memory for the measurement.
  - **`mcp-toolsource-dedup` — DONE 2026-08-09 (`31df590`).** `McpToolSource` existed TWICE in
    `rozum-agent`, both `pub`, both `impl ToolSource` — `mcp_tool_source.rs:40` (LIVE, nadia imports
    it) and `agent.rs:158` (no importer). No compiler warning guarded it: `pub` items are not
    dead-code-linted. The agent.rs copy and its three tests are gone; the module's four tests are a
    strict superset. 134 passed / 0 failed, workspace check clean.
    **One capability was dropped deliberately** — the deleted `call_result_to_value` parsed a text
    result as JSON when it could (`{"sum":9}` -> an object); the surviving `call_result_value` always
    wraps text as a JSON string. No consumer depended on it, so nothing changed behaviourally, but
    many MCP servers return JSON in a text block with no `structured_content` — so if (B) or (C) is
    ever built, this is a two-line addition to `call_result_value`, not a rediscovery.
  - Spec so far: `docs/specs/mcp-toolsource.md`.

## `com.rozum.ucc-ssc` has no plist in the repo (found 2026-08-16 during slice 4)

- [x] ~~**ucc-ssc-plist-not-in-repo**~~ — DONE 2026-08-16, and it was five definitions rather than
  one: the check written to catch it found `meeting-daemon`, `meeting-ssc` and `mcp-http` in the
  same state, plus `ROZUM_UCC_SSC_ORIGIN` missing from a template that DID exist. `svc:plists` now
  reports the drift.
- [x] ~~superseded — original text:~~ **ucc-ssc-plist-not-in-repo** — seven jobs keep their launchd definition under
  `clients/control/launchd/`; this one exists only in `~/Library/LaunchAgents`. It carries
  `SSC_HTTP_BIND=127.0.0.1` (without it the ScalaScript runtime binds 0.0.0.0 — measured), the
  working directory the cell route resolves `scripts/bench/results` against, and now `ROZUM_BIN`
  for the messenger routes' `exec`. A service whose definition lives on one disk is one reinstall
  from being gone, and BUG-030 is the same story about a binary.

## ScalaScript rust-lane divergences found by the UCC port (2026-08-16)

TWO, not the three first written down — the third did not survive being reduced to a minimal case,
which is the argument for reducing before filing. Both SILENT — wrong answers or a dead server, never a compile error. Measured with probes
that ran the same source on `ssc run` and `ssc build-rust`; the interpreter is right in all three.
Details and reproductions in `docs/specs/ucc-ssc-backend.md` § Slice 3.

- [x] ~~**ssc-serve-dies-permanently-after-one-handler-panic**~~ — HALF FIXED UPSTREAM 2026-08-16
  (`b876ca0d8`), confirmed: a handler panic no longer takes the server down (`/boom` → 500, `/ok`
  keeps answering). `jsonParse` still ABORTS its thread on bad input, so the pre-parse guard in
  `public-matrix.ssc` is still required — that was the report's second half and it did not land.
- [x] ~~superseded — original text:~~ **ssc-serve-dies-permanently-after-one-handler-panic** — one `jsonParse` on a blank line
  panics a worker, the http runtime's `unwrap()` on its own mutex then fails for every LATER
  request, and the process stays up answering nothing. Filed upstream 2026-08-16 with a repro that
  shows `/ok` alive → `/boom` panic → `/ok` silent forever. Note the interpreter REFUSES cleanly
  (`ssc: invalid JSON`), so the report is about the server's survival, not about the two lanes
  disagreeing.
- [x] ~~**ssc-type-pattern-on-a-local-val-matches-anything**~~ — FIXED UPSTREAM 2026-08-16
  (`scalascript` `0f8482f54`), confirmed by re-running the repro. Our workaround stays until the
  shared toolchain is past that commit: source relying on the fix would build correctly here and
  silently produce the old answer on the next rebuild from stale staging.
- [x] ~~superseded — original text:~~ **ssc-type-pattern-on-a-local-val-matches-anything** — `case m: Map[String, Any]` takes a
  JSON ARRAY when the scrutinee is a local `val`; the same match on a PARAMETER is correct. Filed
  upstream 2026-08-16 (`scalascript` INBOX, repro
  `examples/reported/rust-type-pattern-on-a-local-val-matches-anything.ssc`).
- [x] ~~**ssc-take-after-map-empty**~~ — **WITHDRAWN, not a defect.** Reducing it for the upstream
  report showed `take` correct on both lanes; the list began with an empty string, so `take(1)`
  returned what it should have. The empty answer came from the entry above.

## Agentic-bench fix candidates (from matrix-failure-analysis)

- [x] ~~**launch-connect-to-a-named-gateway**~~ — DONE 2026-08-16. `--gateway-url` connects instead
  of resolving; the named gateway is deliberately NOT managed (no failover, no lease, no takeover)
  and the launch-local proxy is kept because that is where the decode policy is stamped. Loopback
  only. `agentic.sh` steers instead of refusing. Original entry:
- [x] ~~superseded:~~ **launch-connect-to-a-named-gateway** — `rozum launch` cannot be TOLD which gateway to use.
  `ensure_shared_gateway` (`src/main.rs:3653`) reads the active-gateway registry and reuses whatever
  it names; `--port` only says where to spawn one if none is running. So `BENCH_GATEWAY_URL`, and
  any other "measure THIS gateway" intent, cannot reach the agent — measured 2026-08-15 with a
  recording proxy (the harness announced :8199, the agent talked to :8089, and nothing recorded the
  difference). `agentic.sh` now refuses that run rather than mislabelling it, which closes the
  reporting hazard but not the capability: there is still no way to A/B two gateway builds on one
  host, which is exactly what "did my change move the matrix?" needs. The work is a design call,
  not a flag: what should the failover watchdog, the client lease and the idle-takeover path do
  when the target gateway is one this launch does not manage? Answer that first.

- [x] **codex-reliability — CLOSED 2026-08-09: all four listed levers shipped, and today's run is the
  A/B the entry asked for.** The entry lists them as "Levers to A/B (NOT yet concluded)"; each is in
  the tree:
  - *(edit) bridge unified-diff → codex apply_patch format* — `rewrite_unified_diff_to_apply_patch`
    (`crates/rozum-gateway/src/codex_patch.rs:18`), the "most concrete lever", done.
  - *(create) get the model onto structured write instead of `echo > file`* — `LEAN_CODING_PROMPT`
    (`codex_lean.rs:32`) instructs it in so many words: "call the apply_patch TOOL (do NOT write files
    with a shell heredoc — `cat <<EOF` mangles quotes/backslashes/newlines)".
  - *trim codex's meta-tools (a codex analog of `--lean`)* — `codex_lean_keep` (`codex_lean.rs:14`).
  - *speed / reasoning* — `codex_effective_reasoning` (`codex_lean.rs:98`) pins the level codex would
    otherwise override per-request.
  Its own UPDATE 2026-06-21 already recorded the edit reds as largely resolved (five gateway fixes,
  matrix 22/30 → 27/30). **Validation on the model actually in use, today:** codex × Qwen3.5-4B ×
  `rpn` (create-from-scratch), 3 reps → 2/3, **zero rc11 and zero `toolcall_parse_miss`** — neither
  the patch-format mismatch nor the shell-echo corruption this entry is about. The one failure was a
  `write_stdin` spin stopped by the loop-breaker. See `codex-create-delivery-on-qwen`.
  Original text follows.
  *(original, NOT a queue item)* — **Candidate fixes for the codex matrix reds (most of codex's 10/20).** Root
  cause is NOT a single bug (reproduced, `docs/matrix-failure-analysis.md` Findings 1a/1b): codex fails
  to land code two ways depending on the model — (1a) it stalls in the approval/meta-tool layer
  (`request_user_input`, gratuitous escalation rejected under `approval=never`) and falls back to
  `cargo new <name>` (subdir); (1b) it writes code via `echo "…" > file` and **zsh escaping corrupts
  it** (`println!("{}",rev)` → `println!({},rev)`). Plus codex is slow → times out before recovering.
  And edit-existing (`fix`/`debug`, Finding 4): the model emits a **standard unified diff**, but codex
  `apply_patch` wants its bespoke `*** Update File:` format → `Invalid patch hunk` → the (correctly
  diagnosed) edit never lands.
  Levers to A/B (NOT yet concluded), highest-leverage first:
  - **(edit) bridge unified-diff → codex apply_patch format** in the gateway/wrapper — the model
    already produces a correct unified diff; translating it would land the fix. Most concrete lever.
  - (create) get the model onto codex's structured write (apply_patch raw content) instead of
    `echo > file`, which zsh-escaping corrupts; investigate why it prefers `echo`.
  - trim codex's meta-tools (a codex analog of claude `--lean`) for the 1a approval-stall.
  - speed (already capped to `medium` reasoning) — fewer timeouts = more recovery turns.
  Validate via A/B re-run of the codex `build`/`fix`/`debug` reds. NB: **replaces** the earlier
  (mock-derived) `structured-edit-MCP-for-codex` idea; the real-CLI repro shows it's a patch-format
  mismatch (edit) + shell-echo corruption (create), not a missing edit tool.
  - **UPDATE 2026-06-21 — largely RESOLVED for the `fix`/`debug` (edit) reds.** Five gateway fixes
    shipped (codex×gpt-oss; matrix 22/30 → 27/30, 35B 15/15 no regression), all in `src/gateway.rs`:
    (1) `-N --forward` re-send idempotency (`f63d583`), (2) loop-breaker sig-3 edit-churn (`c134334`),
    (3) `\uXXXX` decode in the apply_patch FUNCTION-call reroute (`14fe6c8`), (4) read-repair default-on
    + refined (`14fe6c8`), (5) whitespace-tolerant `.rej` fallback — gpt-oss drops indent, BSD patch
    can't match (`6f2bed9`). codex×gpt-oss×fix ~1-2/5 → 5/6. Method = `isolate` skill; full writeup
    [[project-gateway-patch-revert]] + specs `apply-patch-*`. **STILL OPEN:** the `build`/`test`
    (create-from-scratch) reds — codex×gpt-oss can't scaffold a project: `patch` can't create a
    missing file (`No file found → Oops.rej`), model flails between `cat`/`tee`/`apply_patch` stacking
    duplicate `[package]`, never reaches `src/main.rs`. Candidate fix `apply_patch` create-if-missing
    being A/B'd (branch `feature/apply-patch-create`); claude (Write tool) drives the same gpt-oss to
    pass, so it's a codex-create-workflow limit, not the model.

## Runtime And UX

- [ ] concurrency-engine-yield - **LOW PRIORITY (2026-06-15): mistralrs-only + non-default, and the
  default engine already does better.** This targets the **mistralrs fork** (`pipeline::step`), which
  is **not in the default build** (`default = ["mlx-native", "all-models"]`). The default **mlx-native**
  engine already does **continuous batched decode** — new requests are admitted into a *live* decode
  batch mid-flight (`src/mlx_native_backend.rs`), which is the interleaving this was reaching for and
  more than mistralrs's admission-only fast lane. (A very long *prefill* in mlx-native still runs as a
  block, not chunk-interleaved — a narrow residual.) Original note: ↓
  Make the fork yield between prefill chunks so a
  long prefill does not monopolise an engine step. Today chunking is internal to
  `pipeline::step` (commit `698bccf1f`) — memory-bounded but not preemptible — so
  the Phase B+C fast lane only reorders *admission*, not in-flight progress.
  Moving the chunk loop up to the scheduler (re-queue the seq as a running prompt
  after each chunk) would let an admitted fast request interleave with a big
  prefill. Upstreamable into `mistralrs-chunked-prefill`.

- [~] concurrency-preemption - **LOW PRIORITY / mostly moot (2026-06-15).** It needs **mistralrs**
  engine support (non-default, not developed). The primary **mlx-native** engine already does
  continuous batched decode (new requests join a live batch mid-flight), which covers most of the
  tail-latency goal; SJF + fast lane + the GPU gate handle admission. Revisit only with a concrete
  tail-latency problem on the default engine.

- [ ] concurrency-cross-process - **LOW PRIORITY (2026-06-15): the architecture avoids the
  multi-process case.** The in-process shared GPU gate (`concurrency-multi-instance` core) + multislot
  (several models in ONE daemon) + the single-shared-daemon registry mean the typical setup is one
  process — so a host-wide budget only matters in niche layouts (`--dedicated` beside the shared
  daemon, or several independent `rozum gateway` processes on one GPU). Needs IPC (named semaphore /
  `flock` / a coordinator) + multi-process validation. Original note: coordinate the concurrency
  budget across several `rozum` processes sharing one GPU, instead of budgeting in isolation.

## Model Quality

- [x] **resident-model-upgrade — MEASURED 2026-08-15, and the answer is to stay on the 4B.**
  Qwen3.5-9B runs (a loader fix was needed and is in master), and on our tasks it buys nothing:

  | | 4B | 9B |
  |---|---|---|
  | 8 tasks × 3 reps, identical conditions | **24/24** | **24/24** |
  | total wall time for those cells | 16 min | 30 min (**1.84×**) |
  | per-cell spread | 6–71 s | 12–262 s |

  Equal where both cope, twice as slow, and a long tail the 4B does not have. No task was found on
  which the 9B is better. The 4B stays; the 9B is verified and rejected, which is a result and not
  a non-result. Weights are on disk (5.98 GB) if anyone wants to re-check.

  **Three attempts at a discriminating task, all three missed, and the reason is the same each
  time: difficulty was set by the SHAPE of the task instead of by what the models lack.**
  - `leapday` — defect two calls below the failing test, no signpost. 4B: **3/3**. Too easy: the
    Gregorian rule is known to every model, the task only asked where to apply it.
  - `board` — four interacting rules, nothing to recall. Recorded as 4B **0/3**, 9B **0/3**,
    314–637 s per cell. ⚠️ **VOID: all six cells were ended by the loop-breaker, not by the
    models** (BUG-054, replayed 2026-08-16 over the kept transcripts). The compile errors quoted
    below were real, but whether either model would have worked past them is unknown, because
    neither was allowed to keep going.
  - ~~The evidence says why: 4B dies on `expected &str, found String`, 9B on `cannot borrow as
    mutable because it is also borrowed as immutable`. **Neither reaches the rules.** The ceiling on
    this stack is Rust's type and borrow system, not reasoning — so any "write something non-trivial
    from scratch" task hits that wall first and hides the difference behind it.~~ **Withdrawn.**
    The two errors were observed, the conclusion drawn from them was not earned: those runs were
    aborted at the point the error appeared, so "never reaches the rules" describes what we did to
    them. `duration` was designed against this claim and turned out well anyway, which is luck and
    should be read as such.

  **Attempts four and five, measured 2026-08-16.** The fourth (`apportion`) was the third one
  again in different arithmetic — compiling skeleton, failing test, plausible half-fix — and the
  operator said so before it had finished running. The repeat was the smaller half of the mistake.
  **The agent has `cargo test` in a loop, so difficulty that lives in a red test is difficulty the
  loop hands back for free**: the feedback shows the model exactly what a trap is built to hide.
  All three of the first attempts were therefore measuring how long that loop takes.

  | task | 4B × 3 | what the evidence says |
  |---|---|---|
  | `leapday` | 3/3 | rule is common knowledge; only "where" was asked. Passing cells, so unaffected |
  | `board` | ~~0/3~~ → **0/3** | re-measured after the fix. Still zero, now for the model's own reasons: 3 of 3 churn (edits putting back what earlier edits removed) or a false success. 9B not re-run |
  | `apportion` | ~~0/3~~ → **1/3** | re-measured after the fix. One clean pass; the two failures are genuine churn — edit 3 restored 17 of 31 substantive lines edits 1-2 had deleted |
  | `duration` | 0/3 → **1/3** | the 0/3 was also aborted; re-run after the fix, and the failures are now the models' own |

  **How much of the record this touched, measured rather than assumed.** The fixed detector was
  replayed over every kept transcript: 36 runs carry a loop-breaker message and **28 of them are
  false stops** — including all six `board` cells and all three `apportion` cells. The other 8 are
  genuine churn and would still fire today. The contaminated cells also span the historical matrix
  (GLM-4-9B `fix`/`debug`, GLM-4-32B+gpt-oss `fix`, Qwen3-4B `greet`, several codex cells), so any
  conclusion drawn from a RED cell in those runs deserves the same check before it is quoted.
  `scripts/bench/agentic_triage.py` now files such a run as `stopped_by_loopbreaker` ahead of every
  other class, because the "false success" it used to report was the model obeying our own
  injected instruction to stop and report in one line. The class names the FACT, not the fault:
  the re-runs show a legitimate stop looks identical from outside, so the row says which signature
  to go and read.

  **Re-measured 2026-08-16 with the fix in (4B × 3 each): `apportion` 1/3, `board` 0/3.** Both
  numbers are now the model's. The loop-breaker still ended four of the six cells, and inspection
  says it was right to: on `apportion` the model rewrote the function back toward a version it had
  already replaced, 17 of 31 substantive lines restored. So the 4B does churn on these two tasks —
  which is a real property, and a different statement from the one the void cells supported. What
  is still NOT established is `board` being unpassable: it has never passed, and it has also never
  had a run that was allowed to finish on the 9B.

  `duration` moves the difficulty out of the feedback loop: `cargo test` is green on arrival and
  stays green, the seeded tests cover only part of the spec, and days plus the all-zero case live
  in the prompt and in no test. The verifier runs the program on eight values.

  **And it produced the first result on this thread worth reading.** Twice out of three the 4B
  wrote a CORRECT implementation — all eight values right, including the days and the all-zero
  case no test mentions — and then rewrote a seeded expectation from `"1h 1m 1s"` to
  `"1d 1h 1m 1s"`. 3661 seconds has no day in it. It left the suite red and reported success. The
  third run did not compile. So the axis that separates is not "can it reason" but **"does it keep
  an invariant it was told to keep while changing something else"** — and that is visible per-run
  rather than as a bare 0/3.

  **The 9B was then run on it, twice, and both runs HUNG — a gateway defect, not the model.**
  ⚠️ My first reading of these two runs was wrong; the table is the raw record, the correction is
  underneath it:

  | run | limit | turns | tool calls | files changed |
  |---|---|---|---|---|
  | 1 | 1200 s, ctx auto (79872) | 6 | 6 | none |
  | 2 | 1800 s, ctx 32768 (matched to the 4B) | 9 | 8 | none |

  Both ended `rc=124 RUN_TIMEOUT` with `src/lib.rs` byte-identical to the seed, so the four
  "failures" printed in each row are the untouched skeleton's own and say nothing about the model.
  The first run's conditions were also wrong and mine to fix: a 79872-token context against the
  4B's 32768, which is slower prefill rather than an advantage.

  **What I then wrote from it, and why it was wrong.** I recorded "eight tool calls in thirty
  minutes is ~3.7 minutes per round trip" and "on a task needing eight to ten round trips the 9B
  does not finish on this host". Dividing the total by the call count is what produced that. The
  per-turn gaps say something else entirely:

  ```
  run 1:  1.7 12.9 12.4 12.6 13.3 15.0 … then 1115.9
  run 2:  1.6 13.3 13.1 12.9 14.1 15.4 15.4 21.8 2.0 … then 1673.9
  ```

  The 9B answers in **13–22 s per turn, consistently**. Then one turn never returns. That is
  BUG-055: nothing bounded the wait for the backend to produce a stream, so a wedged prefill sat
  there until the harness's own RUN_TIMEOUT — never the 120 s generation timeout, which was set and
  working the whole time. A gateway defect, recorded against the model. A mean over a bimodal
  distribution described neither mode.

  **Re-run against the BUG-055 fix, 2026-08-16, three reps each — and this is the first
  measurement on this whole thread that separates the two models.**

  | task | 4B × 3 | 9B × 3 | 9B seconds |
  |---|---|---|---|
  | `apportion` | 1/3 | **3/3** | 137 / 146 / 212 |
  | `duration` | 1/3 | **2/3** | 289 / 246 / 197 |
  | `board` | 0/3 | 0/1 | 262 |

  Five months of "the matrix cannot tell two models apart" ends here: on `apportion` the 9B is
  perfect where the 4B managed one in three, and it is also FASTER per cell (137-212 s against
  120-197 s at a comparable spread, having done the work rather than churned). `duration` is 2/3
  against 1/3.

  **Both models fail the same way when they fail, which is the finding worth keeping.** Every
  loss on both sides is either a self-authored wrong test or genuine edit churn, checked
  individually rather than assumed: the 9B's one `duration` loss wrote a CORRECT implementation
  (all eight verifier values green) and then added its own `assert_eq!(format(3661), "0d 1h 1m
  1s")` — a zero day component printed, which the stated rule forbids — exactly the 4B's failure
  shape at a lower rate. Its `board` loss and the 4B's are both real churn (60% of the added lines
  restoring lines an earlier edit removed). So the axis that separates is not reasoning; it is
  **how often the model keeps the invariants it was given while changing something else.**

  **What this does NOT overturn:** staying on the 4B. That rests on the eight-task 24/24 at 1.84×,
  which these three hard tasks do not touch, and the 9B needs ~10-12 GiB against the 4B's ~7.4 on
  a 36 GiB host that is routinely down to 8 GiB free. What it does overturn is "the 9B buys
  nothing" — on the hard end of the range it plainly buys something. Conditions, stated so the
  numbers are not read as better than they are: the 9B ran with the MLX cache adaptively reduced
  to 1 GiB against the 4B's 2 GiB, both at n_ctx 32768. The 9B's are the tighter conditions.

  ⚠️ Three reps is three reps. This is a signal, not a pass rate to quote as settled.

  **Then the losses were looked into (operator: "may be it can be fixed"), and part of them was
  ours again — BUG-056.** The 9B's only `duration` loss ended on an Edit whose `old_string` and
  `new_string` were identical; the tool refused it, and signature 3 counted the refusal as the
  third edit. Two of the day's six stops had that shape. With it fixed, `duration` re-run on the
  9B passes again (334.5 s, 19 turns, 13 tool calls).

  **Final, three reps each on both models, both fixes in:**

  | task | 4B × 3 | 9B × 3 |
  |---|---|---|
  | `apportion` | 1/3 | **3/3** |
  | `duration` | 1/3 | **3/3** |
  | `board` | 0/3 | 0/3 |

  The 9B is 6/6 on the two tasks the 4B takes one in three, and 0/3 on the third — where the 4B is
  also 0/3. So the bench now separates the models cleanly at one difficulty level and finds them
  equal above it.

  **`board` is not "unpassable" — it measures a specific limit, and both models hit it the same
  way.** All six cells (three per model) end on genuine edit churn, each verified individually
  rather than assumed: the third edit restores 60%, 67% and 78% of the lines earlier edits removed.
  The task states four rules that interact; a model satisfies one, breaks another, and rolls back.
  That is a real ceiling on holding several constraints at once, and it is the same ceiling for
  both sizes — which is itself the useful result, since it says the 9B's advantage is not general.

  One caveat inside the 9B's `duration` 3/3: the third cell passed on correctness at
  `900.1s (RUN_TIMEOUT)` — the work was right and the model did not stop, running to the harness
  limit. The verifier checks the files, not the conduct, so it scores as a pass; 900 s against the
  other two cells' 296 s and 334 s is a different behaviour, not noise. Signature 4 (the same call
  ≥4× in a window of 12) is the one meant to catch it and did not.

  **What is left is the model's, and there is nothing to fix on our side for it.** Both models lose
  the same way: they write a test of their own that contradicts the stated rule — the 9B's was
  `assert_eq!(format(3661), "0d 1h 1m 1s")`, a zero day component the rule forbids, and it defended
  the mistake in its own reasoning before making it. Suppressing that by telling the model not to
  add tests would hide the weakness the task exists to measure, not fix it.

  **Cost to be aware of before running more:** a 9B bench and the operator's chat do not both fit
  on this host. The 4B gateway spent 240 s refused and then gave up while a bench held the RAM —
  visible in `~/.rozum-gateway.log` as "9284 MB already reserved by [pid … 9B]". The chat recovers
  by itself afterwards (measured 0.9 s), but during a 9B run it is down. Two of the runs were also
  killed after their first cell for reasons not established; completed cells survive a kill, so
  small batches lose less.

  Worth keeping for whoever runs the rest: `gateway --dry-run` gives the admission verdict with the
  real load-path math and loads nothing, so headroom can be checked before committing an hour.

  `duration` stays as the bench's discriminating task: the only one of the five that produced a
  readable failure instead of a bare zero, and it grades a single model without needing a second
  one to compare against.

  **What the next attempt must look like**, now derived rather than guessed: a COMPILING skeleton
  where the change is to logic, not to ownership structure — the shape of `debug`, with real
  difficulty in the logic and no comment pointing at the line. Both new tasks are committed and
  usable (`leapday` is a fair ninth task; `board` is currently unpassable and is NOT in the default
  list), so nobody rebuilds them from scratch.

- [ ] **resident-model-upgrade (original entry, kept for the reasoning)** — **operator 2026-08-15: "попробуем".** The resident model has been
  frozen on `mlx-community:Qwen3.5-4B-MLX-4bit` since 2026-08-04. This is the survey of what has
  shipped since, measured off the model cards and configs rather than recalled, and the staged plan
  the operator approved. **Do the steps in order; step 1 is cheap and may end the item.**

  **The landscape, so nobody re-derives it.** Qwen released nothing small after 3.5: Qwen3.6 is 27B
  and 35B-A3B, Qwen3.8 is 27B and 2.4T-A95B, and **Qwen3.7 does not exist** — everything found under
  that name is a community merge. Within 3.5 the sizes are 0.8B / 2B / 4B / 9B / 27B / 35B-A3B and
  up. Gemma 4 shipped and is the only genuinely new small family (below). Nothing else small was
  trending in MLX 4-bit.

  ### Step 1 — `mlx-community/Qwen3.5-9B-MLX-4bit`. No port, measure it.

  It is not a similar model, it is **the same architecture, wider** — so our runtime loads it with
  no new code, and it already serves both the 4B and the 27B of this family:

  | | resident 4B | 9B |
  |---|---|---|
  | `model_type` | `qwen3_5` / `qwen3_5_text` | identical |
  | layers / hidden | 32 / 2560 | 32 / **4096** |
  | KV heads / `head_dim` | 4 / 256 | **4 / 256** |
  | `full_attention_interval`, `attn_output_gate` | 4, true | 4, true |
  | vocab / max ctx | 248320 / 262144 | identical |
  | quantisation | 4-bit, group 64, affine | identical |
  | vision | yes | yes |
  | on disk | 2.9 GB | **5.98 GB** (2 shards) |

  **The KV cost does not change at all**, which is the part worth not re-deriving: both have 8
  full-attention layers of 32 with 4 KV heads at `head_dim` 256 → **32,768 bytes per position** for
  each. Long context costs the same; only the weights grow.

  | | weights | footprint @ `n_ctx 8192` | admission needs |
  |---|---|---|---|
  | resident 4B | 2.83 GiB | 8.6 GiB | 10.6 GiB available |
  | 9B | 5.57 GiB | **11.3 GiB** | **13.3 GiB available** |

  (footprint = weights + KV×n_ctx + the 5.5 GiB cache/prefill reserve; admission adds `min_free` 2 GiB.)

  **Expect it to be ~2× slower per token** — same depth, wider. The 4B runs ~100 t/s, so plan for
  50–60. For agentic work latency matters more than a few points of quality, so this is the number
  to watch alongside the score.

  **Done when:** the 9B has run the same 88 matrix cells the resident model has, and we know three
  things — it is not WORSE, it fits at a usable `n_ctx`, and what it costs in tokens/second.

  ### Step 2 — only if step 1 disappoints: `mlx-community/gemma-4-E4B-it-qat-4bit`.

  The one genuinely new small family. `google/gemma-4-E4B-it` is ~8B, and the mlx-community build is
  **QAT** — quantisation-aware trained, which holds quality far closer to bf16 than a post-hoc 4-bit.
  Its KV is CHEAPER than ours: 42 layers of which only 7 are full attention, 2 KV heads, `head_dim`
  256, and the other 35 are sliding-window at 512 positions — a bounded, fixed-size state.

  **But it is a port.** `model_type: gemma4` against our `gemma3`, `Gemma4ForConditionalGeneration`,
  a sliding/full hybrid, and — per its config — an AUDIO tower beside the vision one, a modality this
  runtime has never handled. Do not start it before step 1 has a number.

  ### The measurement problem, which is the real risk to this item

  **Our matrix cannot show an improvement, because it is saturated.** The resident 4B scores
  **51/52** on it (the one red is a single `wordcount` rep in a run archived as BUG-014 evidence,
  where the same agent passed the same task on another rep). A better model has nowhere to go. So
  step 1 answers "not worse, fits, costs this much" and nothing more; demonstrating that 9B is
  BETTER needs harder tasks than the matrix has, and inventing them is its own item — do not smuggle
  it into this one. Read the result with `summarize_matrix.py`, which since 2026-08-15 excludes
  cells that did not measure the model.

  ### Gotchas before running anything

  - **Take the model slot properly.** Announce in the rozum room, check no resident gateway holds
    `residency.lock`, and never start a second model load (SPRINT's reboot-safety protocol; BUG-003
    was a kernel-watchdog reboot).
  - **Tidy the host first.** Measured 2026-08-15: 24.2 GiB of 36 was anonymous memory, mostly sbt
    daemons from scalascript toolchain builds, leaving **5.1 GiB available** — not enough even for
    the model we already run. `available` in the gate's own sense is
    `total − (wired + anonymous + compressor)`, so file cache does not count against you and JVMs do.
  - **Do not delete the 4B until the 9B has a score.** `models-cleanup` deleted the bf16 build on the
    strength of an equal score; that was right, but only because the score existed first.

- [ ] model-catalog-refresh - Expand and verify tiny model catalog.
  - Include current small Qwen/Gemma/Phi candidates with exact file sizes.
  - Record license and expected strengths.

- [ ] benchmark-baseline - Record latency, disk size, and smoke eval score for each backend/model pair.
  - Use the eval harness once available.

- [ ] distillation-plan - Design a later LoRA/QLoRA or distillation path.
  - Do not implement until evals provide a baseline.

- [x] **elastic-context-on-demand — CLOSED 2026-08-28 (v1: grow + idle shrink-back).** A resident
  `mlx-native` model raises its own served `n_ctx` live (no drain, no reload) the moment a request
  needs more than it currently serves, checked against a real RAM/ledger admission
  (`crates/rozum-mlx/src/mlx_native_backend.rs::grow_context`, wired in `gateway.rs` before
  `fit_to_context`), and releases the difference back down after `ROZUM_ELASTIC_CTX_SHRINK_IDLE_SECS`
  (default 120s) of idleness — piggybacked on the gateway's PRE-EXISTING idle-watchdog tick (the same
  loop idle-unload/pressure-shed already use), not a new timer. `ROZUM_ELASTIC_CTX=0` opts out.
  Spec + Results: `docs/specs/elastic-context-on-demand.md`. Reused two already-existing primitives
  (`ResidencyGuard::update_footprint`, `dry_run_admission`) rather than the new ledger op this entry
  originally called for — a smaller change than planned.

- [ ] **elastic-context-preempt-on-denial** (follow-up, split out of `elastic-context-on-demand`
  2026-08-28; renamed 2026-08-28 — the shrink half of the original split shipped same-day) — when a
  grow doesn't fit on its own, preempt an idle lower-priority sibling via the existing
  `residency-admission-queue.md` P4 protocol instead of just falling through to trim. **Build only
  if** v1's fallback (trim) turns out to fire often enough in practice to matter — not measured yet;
  `grow_context` eprintln's every successful grow to the gateway service log, so a quiet log over
  real usage vs. a chatty one is the signal to watch before building this.

# BLOCKED — on another repository

## The meeting PWA cannot be rebuilt from source (found 2026-08-08)

- [x] **meeting-ssc-unbuildable — DONE 2026-08-08.** The blocker was two import lines. On the Rust
  backend `route`/`serve`/`requestCookie`/`readFile`/`listDir`/`isDir` are intrinsics when used
  UNIMPORTED; importing `std/http.ssc` (19 lowering errors) and `std/fs.ssc` (1) is what pulled in
  the `::`/`Cons`/`Nil` code the backend cannot lower. Dropping them took the file from 20 errors to
  0; spelling out `ProcessOptions(…, true)` at 23 call sites got the emitted Rust to compile. The
  binary is rebuilt, published and serving :8405. Record: `CHANGELOG.md`.
  **CORRECTION 2026-08-09, measured, and it matters because a workaround was written against it.**
  The 20 errors were a STALE TOOLCHAIN, not the import lines. `bin/ssc-tools` here had been built
  from `194f9c43d` while their tree was at `32f298e77`, and it says so in a `STALE BUILD` banner.
  After `./install.sh --dev` (now `6d3fce2c7`): `[route, serve](std/http.ssc)` **with the functions
  called** emits **0** lowering errors, `[readFile, exists](std/fs.ssc)` **0**, and the ORIGINAL
  `clients/meeting/meeting.ssc` — imports intact — builds to a 1.7 MB binary with no errors at all.
  Their triage of `build-rust-std-imports-unlowerable` reached the same numbers first by quoting the
  SHA the BANNER names rather than the one `git log` shows, and asked us to check exactly that.
  So the intrinsics-when-unimported workaround and the 23 hand-spelled `ProcessOptions(…, true)` are
  changes made to route around a bug that does not exist on a current toolchain. They are harmless
  and they are also unnecessary; nobody should extend that pattern to new files.
  **The rule, and it has now cost two agents a build cycle each: quote the SHA from the BANNER.**
  `git log` describes the tree; the banner describes the binary that is actually measuring.
  **What survives, measured 2026-08-09 on a current toolchain, and it is narrow.** `std/http` and
  `std/fs` are fine. `std/json` is not — and the boundary is exact: it LOWERS cleanly (0 errors) and
  its emitted Rust then fails to compile, **155 rustc errors from a SIX-LINE program**, mostly
  `charAt` / `substring` emitted as `String` methods that do not exist. Filed with them as
  `json-core-emitted-rust-does-not-compile` (landed in their INBOX), deliberately separate from
  `build-rust-std-json-cons`, which is about lowering REFUSALS — this one gets past lowering.
  **`ucc-ssc-backend` slice 1 waits on that, and waiting is the decision.** The only thing the slice
  needs `std/json` for is checking a view token against a JSON file. Hand-rolling that with string
  matching would buy a green build and a token check that says yes because the token appears
  SOMEWHERE in the file — a worse outcome than a blocked port.

## Land the reactive-chat primitives in canonical scalascript (deferred "потом", 2026-07-22)

- [ ] ~~get scalascript's `fetchStreamSignal` + `intervalTick` + `forJson`~~
  toolkit primitives into canonical `main` so `deploy-ucc-web.sh` rebuilds `chat.html` FROM SOURCE
  (`chat.ssc`) and the fail-safe is retired. Operator explicitly wants this finished later.
  - **Why it's blocked today:** the primitives live ONLY on `origin/feature/ui-stream-chat` = exactly 2
    commits (`44d378ef8` fetchStreamSignal+intervalTick, `3814f4c08` forJson). The canonical `bin/ssc-tools`
    emit-spa lacks them → `deploy-ucc-web.sh` (lines ~437-452) emits nothing for chat.ssc and KEEPS the live
    (locally-emitted) `chat.html`. Live reactive chat works; it just isn't rebuilt from source.
  - **Why it's a cherry-pick, NOT a merge:** `feature/ui-stream-chat` is badly stale — `git diff main
    origin/feature/ui-stream-chat` = ~460 files / ~21k lines main-ahead. Merging would revert huge swaths of
    main. Must **cherry-pick the 2 commits** onto current `main` and resolve (they're additive: new `std/ui`
    defs in primitives.ssc/reactive.ssc + JS runtime `signals.mjs` + emit-spa/FrontendBridge lowering + tests).
  - **Steps:** scalascript is at `../scalascript` (REPOS.md), branch `main`. Follow the contribution flow
    (claim in `.work/active` → `scripts/new-worktree` → cherry-pick → conformance [emit-spa lanes are INT+JS,
    JVM lane fails pre-existing in fresh worktrees] → `sbt cli/assembly && installBin` → push branch:main →
    `sbt installBin` in the MAIN checkout to refresh `bin/lib`). THEN a rozum `deploy-ucc-web.sh` auto-ships
    the reactive chat (the fail-safe branch is skipped once emit-spa yields a valid `<!doctype>` chat.html).
  - **Bake in the mount-fire fix at the source** while porting `fetchStreamSignal`: make it NOT POST at mount
    (fire only when the trigger tick increments past its seed). That eliminates the empty `/control/chat/stream`
    request entirely — complementing the server-side no-op already shipped (rozum `c95235e`), which currently
    turns the mount-fire into a harmless 200.
  - **Caveat:** coordination-sensitive — dozens of scalascript agents depend on the shared `bin/lib`; announce
    in the room and land cleanly. Effort: medium, cross-repo. Ref: memory `project-chat-baseline-config`.

## UCC backend on .ssc→Rust (strategic, 2026-07-07)

- [~] **ucc-ssc-backend** (spec: `docs/specs/ucc-ssc-backend.md`) — **slice 1 SPEC'd 2026-08-08, and
  the measurement moved the whole plan.** 63 routes: 19 read, 23 action, 5 terminal, 16 auth; only
  **4 are public** (`/view/{token}` + the three `/control/public/matrix*`), the other 59 sit behind
  seven permission layers.
  **The critical path is none of the gaps the entry lists.** "Can a .ssc program serve HTTP" is not
  one — `rozum-meeting-ssc` has served `:8405` for weeks. WebAuthn is HALF present: 41 lines of
  browser passkey actions in `std/ui/webauthn.ssc`, no server ceremony. What actually blocks
  everything is: **how does a .ssc server participate in a session it does not own?** — and porting
  read routes one at a time would discover that same question 19 times.
  **Slice 1 is the four PUBLIC routes**, which need no session at all and answer the only question
  worth answering first: can a .ssc server stand beside the Rust one and serve real traffic. Then
  decide the session question as its own spec, then the 19 read routes, then the 23 action routes
  (which do need the spawn/registry primitives). Terminal and auth stay Rust, as the entry says.
  Original: express the UCC server half in ScalaScript, like the meeting web
  (`rozum-meeting-ssc` is already a pure .ssc→Rust server). Motivation: the async-job pattern now
  exists twice — `std/ui/patterns.ssc jobPanel` (client, toolkit expression) + `control.rs
  spawn_launch_task` (server, Rust) — a .ssc server would let the SERVER half be a scalascript
  function too (`route` + actor `spawn*` + a status registry), one language end-to-end, dogfooding
  the toolkit per the North Star. What the toolkit is MISSING for this today: WebAuthn/passkeys,
  PTY↔WebSocket bridging (the tmux terminal), process spawn/kill + registry primitives, launchd
  deployment story, and access to rozum's residency/admission API (would need an FFI seam or a
  sidecar). Path: start with the read-only status/dashboard routes as .ssc behind the same origin,
  migrate action routes once spawn/registry primitives exist, keep terminal+auth in Rust longest.
  Effort: large (weeks, cross-repo). Value: single-language UCC, the toolkit gains the server-side
  job pattern as a first-class function.


*(no open items — kept for the reasoning above.)*

## scalascript language gap: theme page-background never reaches `serve(view, port)` (found 2026-07-03, ucc-theme-bg)

- [x] **ssc-serve-extracss-or-theme-body — CLOSED 2026-08-09: fixed UPSTREAM, first of the two options
  the entry offered.** `scalascript` `origin/main`, `v1/runtime/std/ui/primitives.ssc:170`, now reads
  `extern def serve(tree: View, port: Int, extraCss: String = ""): Unit` — the `.ssc` surface reaches
  the third arg the JS runtime always accepted, with a default so existing call sites keep compiling.
  Their comment (lines 165-167) says it is appended to the base template LAST so it wins over the
  `body{margin:0;padding:0;background:#fff;…}` in `browserpatch.mjs:151` / `signals.mjs:1655` — which
  is precisely the white-canvas-under-dark-cards failure this entry described.
  **And rozum has already adopted it — nothing left to do.** `clients/control/deploy-ucc-web.sh:225-226`:
  *"Page canvas background: now set at the language level via `serve(view, port, extraCss)`
  (center.ssc passes `body{background:#111827}`), so the old post-emit sed of `body{#fff}` is gone."*
  The workaround this entry complained about is out of the tree.
  (My first draft of this closure told the next agent to go remove that `sed` — written from the
  entry's text instead of the script, and wrong within the hour. Same trap this whole sweep is about;
  corrected on re-reading the file. See [[feedback-verify-backlog-entry-against-code]] in memory.)
  Original entry follows.
  *(original entry, NOT a queue item)* — `std/ui/primitives.ssc`'s `serve(tree: View, port: Int)`
  extern def has no way to set the document/body background from `.ssc`, even though the JS-side
  `_ssc_ui_serve(tree, port, extraCss)` already accepts a third `extraCss` param — nothing in the
  `.ssc` language surface can reach it. `lower(view, theme)` correctly themes every widget it has a
  hook for (surface/onSurface/etc.), but the emitted base template hardcodes
  `body{background:#fff}`, so a themed app (e.g. `darkTheme`) renders correctly-dark cards on a
  white page canvas. Currently patched around in `rozum`'s `deploy-ucc-web.sh` with a `sed` on the
  emitted HTML — a rozum-only workaround, not a real fix. Real fix (either works): expose `extraCss`
  on the `.ssc` `serve` extern def, or have `emit-spa`/`_ssc_ui_serve` derive the base body
  background from the theme passed to `lower` automatically. Lives in `scalascript`, not `rozum` —
  belongs in that repo's own spec/BACKLOG when picked up.

# PARKED — what would revive it

## ONE GATE, several entries: the weights are not on disk (checked 2026-08-09)

`rozum models list` returns **exactly one** model: `mlx-community:Qwen3.5-4B-MLX-4bit`, 3.06 GB.
Every other model this board names — Qwen3-Coder-30B, GLM-4-32B, GLM-4.7-Flash, Devstral, gpt-oss —
is absent. So the entries below that read "GPU-gated" or "slot-gated" are mis-labelled: they are
**download-gated first**, and a GPU window is the second cost, not the first.

Affected, and what each would need before it can even start:
- `qwen-coder-edit-toolarg-decode` REMAINING (the live verify) — Qwen3-Coder-30B, ~17 GB.
- `glm32b-codex-timeout` — GLM-4-32B, ~18 GB.
- `B2 — one authoritative full matrix` — the whole curated tier, several models.
- `gptoss-codex-cascade` — gpt-oss (gone from disk) plus a second model; a cascade needs two.

**DECIDED 2026-08-09 — the operator has settled on the model already on disk and wants nothing else
downloaded for now.** So all four are parked on that decision, not on anyone's time: do NOT re-propose
them as "ready to run", and do not download weights to unblock one. They revive if and when the
operator asks for a second model. Measured cost of the largest, so the decision can be revisited with
a real number rather than a guess: `mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit` is **16.02 GB**
across 4 safetensors shards (HF API, 17 files) — and at ~16 GB of weights it does not co-reside with
the 4B on a 29 GB budget, so it also costs the resident model for the duration.

This is one operator decision (disk + bandwidth + a GPU window), not four separate ones. Recorded
here so it is made once. Note the contrast: `codex-create-delivery-on-qwen` was runnable TODAY with
no download and no eviction precisely because it names the resident model — an entry that specifies
a model already in use costs nothing to answer, which is worth copying when writing the next one.

## Native MLX model ports (matrix coverage, lower priority — operator 2026-06-27)

**Revives when:** one of these models is on disk. `~/.cache/huggingface` holds Qwen3.5-4B and
nothing else, so each of these is a download away from being real work.

- [ ] **mlx-port-granite4** — IBM `granite-4.0-h-small` (`granitemoehybrid`, 4bit 18.1 GB):
  Mamba2-SSM + MoE hybrid, tool-use-tuned. Medium-high effort (SSM ≠ the GDN hybrid we already
  did for Qwen3.6; new state-space layer). Consider only if the matrix wants an IBM/tool-tuned family.
- [ ] **mlx-port-seed-oss** — ByteDance `Seed-OSS-36B-Instruct` (`seed_oss`, 4bit 20.3 GB):
  own arch, long context; 20 GB is borderline on 36 GiB. Payoff unclear vs Qwen3-Coder/GLM-MoE.
- [ ] **mlx-mla-attention** (DeepSeek-V2-Lite only — GLM-4.7-Flash DONE) — **absorbed-MLA for
  GLM-4.7-Flash (`glm4_moe_lite`) is SHIPPED (e8c060a, 2026-07-03).** Remaining work: full
  DeepSeek-V2-style MLA (non-absorbed: `q_a/q_b` low-rank, `kv_a_proj_with_mqa`, decoupled
  nope/rope head dims) for `DeepSeek-Coder-V2-Lite` (`deepseek_v2`). Low priority given we now have
  3 model families (Qwen, GLM, Devstral) covering all tasks. DeepSeek-Coder-V2-Lite ≈17 GB but
  previously scored 2/5 (edit tasks only). Revisit only if a 4th diverse family is needed.

## Agentic drivers

**Revives when:** a GLM model is back on disk — this is a GLM-specific workaround, and a clean
one already exists.

- [x] **glm-artifact-write-synth — CLOSED 2026-08-09: shipped, DEFAULT-ON, and it outgrew the entry.**
  The entry still reads *"idea, NOT committed"*; the code has been on by default since 2026-06-23.
  `mlx_native_backend.rs:5161` `glm_artifact_synth_enabled()` is opt-**out** (`ROZUM_GLM_ARTIFACT_SYNTH=0`),
  flipped on after a live A/B: **create 0/3 → 3/3, no regression on edit** (synth doesn't fire there),
  chat false-writes guarded (`synth_skips_chat_and_ambiguous`). The spec the entry wanted exists:
  `docs/specs/glm-artifact-write-synth.md`. The "why hard" worries — recovering the PATH from an
  unstructured label — were solved, not dodged (`glm_kv_extract`, `match_tool_by_args`).
  **It also generalised past its own premise**, which is the part worth carrying forward:
  `artifact_synth_universal()` (`ROZUM_ARTIFACT_SYNTH=1`, default OFF) applies the same recovery to
  ANY model, because the synth turned out to be model-agnostic — Mode-2 matches a bare tool-args
  object against an offered tool's `input_schema`. See the new entry `artifact-synth-universal-measure`.
  Original entry follows.
  *(original entry, retained — NOT a queue item)* — let GLM-4-32B
  drive CREATE-from-scratch agentic flows by synthesizing a `Write` tool call when GLM emits a
  labeled file artifact instead of naming the tool. Today GLM names tools cleanly for edit/debug
  (logit-constraint `99c6081`) but on create-from-scratch shows `Cargo.toml`/`main.rs` content in
  fenced blocks — a GLM-4-0414 model decision property, proven NOT prompt-induced (claude's captured
  prompt has zero framing; glm4-bringup § ROOT CAUSE). Precedent: codex's `synthesize_write_from_obj`
  (gateway.rs ~1982) does this from a structured `{path,content}`. **Why only an idea / why hard:**
  (1) GLM's artifact is UNSTRUCTURED free text — the synth must recover the file PATH from the label
  ("Cargo.toml:", a `// src/main.rs` first-line comment, or a ```rust:path info-string); needs REAL
  GLM output samples to build against (slot-gated — do NOT build blind, that's the framing-strawman
  mistake). (2) FALSE-POSITIVE RISK: a GLM CHAT answer with an example code block + a filename mention
  would get wrongly written to disk — needs tight guards (only when a Write tool is offered AND no
  tool call parsed AND the turn is clearly a create request). (3) It INVENTS a call the model didn't
  make (unlike codex's case, which had explicit `{path,content}` intent). **Decision:** the clean
  answer "use Qwen3.6-35B for create-from-scratch, GLM-4-32B for edit/debug/chat" already covers the
  need, so this stays a backlog idea. If pursued: capture real GLM create output via a KEEP=1 probe
  (slot-claimed), build+unit-test the path-extractor offline, gate default-OFF, live-A/B before on.
  Integration point: `serving::parse_tool_calls` returns empty → synth at the mlx call site
  (mlx_native_backend.rs ~2115); needs tool-names + GLM-family threaded into scope (not there today).

## Optional Model Adapters

**Revives when:** a model that needs one of these adapters is on disk.

Model adapters are optional. They must not be required for the default build,
default CLI startup, meeting rooms, round-robin moderation, or manual moderation.


*(no open items — kept for the reasoning above.)*

## GLM model landscape (sizing + port path)

**Revives when:** GLM is back in the catalogue. Sizing notes age fast; re-measure rather than
trust these numbers.

- [ ] glm-model-landscape — **Recorded 2026-06-21.** Which GLM (Zhipu/Z.ai) models are worth
  running in rozum, and how. **Verified facts:** the MLX-native crate (`.vendor/mlx-lm`) has NO
  GLM (`unsupported model_type`); the vendored **mistral.rs DOES** — `glm4.rs` / `glm4_moe.rs` /
  `glm4_moe_lite.rs`, registered as `Glm4ForCausalLM` / `Glm4MoeForCausalLM` / `Glm4MoeLiteForCausalLM`.
  - **Fits 36 GB (do these):** GLM-4-9B and **GLM-4-32B-0414** (both DENSE → `glm4` loader; 4-bit
    ~6 GB / ~18–20 GB). Actionable port task: SPRINT `glm4-bringup` (MLX-native `glm4.rs`, the
    fast first-class path; partial-RoPE/qkv-bias/post-norm building blocks already in the crate).
  - **Quick validation today:** `ROZUM_FORCE_MISTRALRS=1 rozum launch --model <glm-4-9b>` (arch
    already in the fork; candle/Metal, slow — `ROZUM_FORCE_MISTRALRS` / lower `--n-ctx` for the
    RAM preflight, per [[project-qwen36-mistralrs]]).
  - **Too big for 36 GB (NOT targets):** GLM-4.5-Air (106B-A12B), GLM-4.5 (355B), **GLM-5 / GLM-5.1**
    (744B total / 44B active MoE, 256 experts·8 active, DeepSeek sparse attention, 200K ctx, released
    2026-02-11; ≈ 370 GB at 4-bit — cluster-scale). DeepSeek-style arch; mistral.rs has `deepseek2/3`
    but no `glm5`. Revisit only if a much larger box (512 GB Mac Studio is still marginal) is in play.

## Native MLX runtime — performance (ports from the mistralrs work)

**Revives when:** it depends on the item, and that is the point — this section is three different
things under one heading. The model ports need another model on disk; the `tune-*` experiments need
a model to fine-tune; `windows-portability` needs a Windows host and no model at all. Split it the
next time anyone works here.

The native MLX runtime (`docs/specs/mlx-native-runtime.md`) shipped correctness +
the GatedDeltaNet prefill kernel. These carry over optimizations proven in the
mistralrs backend that the native runtime does NOT yet have. (Concurrency,
admission, backpressure and the OOM circuit breaker already apply generically
through `concurrency::admit_wrap`, so they are not relisted.)

- [~] mlx-hand-fused-gdn-kernels — **PROBED 2026-06-14: low reward, deferred.** Re-measured
  the MoE hybrid decode (`mlx_qwen35_moe_decode_bench`, 35B-A3B — the e2e model): **~59-60 t/s**,
  serial==pipe (pipelining gives only 1.02× — see why below), and the SPLIT timing is
  **`build=15.65ms/tok, eval=1.31ms/tok` → 92% of per-token time is CPU graph-build / FFI**,
  only 8% GPU. Dumped the decode-step graph (`ROZUM_DUMP_DOT`): **122 primitive nodes**, and
  the hot elementwise ops are **already auto-fused by MLX** at eval — the gate sigmoid·multiply
  shows up as `CompiledSigmoidBroadcastBroadcastMultiply` (5×), `RMSNorm` is fused (7×), and
  there are **no stray `AsType`** (the bf16-stream fix held). So the original premise — that
  `compute_g`/gate are *unfused* and need hand-written `metal_kernel`s — no longer holds; MLX's
  automatic elementwise fusion already collapses them. Custom kernels would duplicate MLX and
  carry the hybrid byte-exactness risk for ~no gain. **The bottleneck is the 92% build/FFI
  cost** (≈0.13 ms × 122 op-launches/token of Rust→C→C++), which pipelining can't hide (build ≫
  eval). The obvious lever for that is `mx.compile` (trace once + reuse) — **but it's confirmed
  dead in mlx-rs (see `mlx-native-perf-compile` below): re-probed plain `compile` on Qwen3-4B
  (7× bigger build than the original 0.6B probe) and it's STILL net-negative (0.64×); mlx-rs's
  `compile` adds more overhead than the per-token build it saves, independent of model size.**
  So the build cost isn't reducible via the available APIs (MLX already auto-fuses the
  elementwise ops; mlx-rs compile doesn't deliver the Python `mx.compile` win). Decode at
  ~59 t/s is already fast and the dominant agentic latency (prefill) is solved by prefix-KV
  reuse. **Don't pull hand kernels; don't pull compile.** (Probe was the MoE; the dense 27B
  hybrid runs all params per token and is slower — re-probe it separately if it becomes the
  primary model.) Diagnostics:
  `ROZUM_DUMP_DOT=/tmp/d.dot … mlx_qwen35_moe_decode_bench` + a DOT label histogram.

- [ ] mlx-native-mixtral - **LOW PRIORITY (2026-06-15): MoE need already covered; Mixtral largely
  superseded.** mlx-native already serves Qwen3-MoE and **Qwen3.6-35B-A3B** (a more modern + faster
  MoE — 3B active), so the sparse-MoE capability is there with better models. Mixtral 8x7B (~26 GB
  @4bit, borderline on 32 GB) was a late-2023 hit now mostly displaced by Qwen3.x / Llama3.x / Gemma3.
  A full new-arch port + real-weight parity for nichey value — skip unless a specific Mixtral need
  appears. Original note: Mixtral / Mistral-MoE (`model_type: "mixtral"`). Sparse MoE on the Mistral
  block — reuse the `qwen3_moe` SwitchGLU routing + Mistral attention. Validate vs oracle.

- [ ] tune-toolcall-format - **Highest value/effort.** SFT/QLoRA a small model
  (0.5–1.5B) on correct `<tool_call>{…}</tool_call>` traces to raise tool-call
  format adherence (small models sometimes botch the JSON). Narrow, low-risk,
  trivially measurable (format-valid rate on a held-out set). Pure format learning —
  a tiny model is enough.

- [ ] tune-domain-coder - QLoRA `Qwen2.5-Coder-1.5B/7B` on this repo's conventions
  (FIM / signature+docstring→body / diff→commit-message) for fast, private, on-device
  **autocomplete + boilerplate** in our style. NOT a replacement for the agent model
  — it's the "small local handles the rote 80%, big/remote handles the hard 20%"
  tier (rozum's multi-backend routing already fits this). 1.5–4B for completion;
  7B if it should also carry a bit of domain reasoning.

- [ ] tune-room-agent-style - Light QLoRA for a consistent room-agent voice/format
  (tone, structure of replies, meeting etiquette). Style/persona is exactly what a
  small model picks up; 0.5–4B is enough.

- [ ] tune-minimal-experiment - **The one-day proof.** Offline QLoRA
  `mlx-community/Qwen2.5-Coder-1.5B-Instruct-4bit`: ~1–5k `(prompt, completion)` pairs
  from the repo (10% held out), rank 16, target `q/k/v/o + gate/up/down`, LR 1e-4,
  2 epochs, seq 2048, batch 1 + grad checkpointing → `mlx_lm.fuse` → `rozum launch
  --model <merged-dir>`. Fits in 16–32 GB, ~an afternoon. Eval: held-out
  exact-match/edit-distance + a small general probe. Decides yes/no on "helped my
  domain without breaking general use" before investing in the items above. Spec §6.

### Agent meetings daemon — follow-ups (spec: `docs/specs/agent-meetings-daemon.md`)

Shipped on `feature/meetings-impl`: the daemon (`rozum meetings`), disk-backed
multi-room store (daily files, per-day `n`), session-lifetime identity, agent
proxy (`rozum mcp-proxy` → `meeting.sock`), user-service install, human TUI
client (`rozum` / `rozum meetings attach`) with picker + day-scoped render, and
polish (graceful drain, idle-evict, content-off-daemon, per-room `catch_unwind`,
second poll-connection, bare-`rozum` cutover with `--legacy-room` escape hatch).
Remaining:

- [ ] windows-portability - **Make rozum a first-class Windows host (durable core + CI).**
  The HTTP/backend abstractions and the package allow-list below are cross-platform, but the full
  gateway/launcher package still compiles Unix control/PTTY/service seams and is not claimed as a
  native-Windows host. These sub-tasks close that gap for the **local meeting daemon**, gateway
  host, and **in-process engines**. All hardware-independent except GPU validation. Spec:
  `docs/specs/portability-and-the-backend-spi.md` (§ "Platform-aware build (Linux *and*
  Windows)"). Engines on Windows are tracked elsewhere and need NO separate item: GGUF via
  `portability-cuda-gguf` (non-`metal` llama-cpp-2 — CPU/CUDA/Vulkan; builds with MSVC), and
  the native iGPU path via `x86-native-runtime` (Vulkan is cross-platform — the SAME L5 engine
  runs on Windows; `VK_EXT_external_memory_host` zero-copy works there too). Sub-tasks:
  - [x] windows-core-ci - **RESTORED + VERIFIED 2026-07-16.** The old 2026-06-20 root-only
    command became dead after the binary/workspace split. `windows-latest` now builds package
    `rozum-cli`'s real `rozum.exe` dispatcher and tests the declared portable packages
    (`rozum-core`, models, agent, and feature-off engine interfaces). Run `29533946535` is green.
    Full meeting/gateway host support remains the concrete Unix-seam work below.
  - [x] windows-daemon-ipc — **DONE 2026-08-09 (compiles for Windows; never run there).** The
    transport is behind `meeting::ipc`: unix socket unchanged, Windows named pipe (not loopback TCP —
    a pipe carries an ACL and this endpoint speaks MCP as whoever joined). `rozum-meeting` went from
    11 Windows errors to 0, as did rozum-agent / rozum-meet / rozum-cli. The daemon prints an
    UNVERIFIED notice on Windows. Spec: `docs/specs/windows-daemon-ipc.md`.
  - [x] windows-openssl-webauthn — **DONE 2026-08-09 by making the console optional.** `ucc` (default
    ON) gates `control` + `auth` + `webauthn-rs`; the machine snapshot moved to `status.rs` and is
    never gated. `--no-default-features` has no OpenSSL in the Windows dependency graph at all.
    Measured: `webauthn-rs-core` depends on openssl unconditionally, so "webauthn on rustls" is a
    fork of someone else's crate, not a switch — `openssl/vendored` remains the escape hatch for
    anyone who wants the console on Windows. Spec: `docs/specs/ucc-optional.md`.
  - [x] windows-spawn-seams — **DONE 2026-08-14 (compiles for Windows; never run there).** All of
    it is behind `crates/rozum-gateway/src/procctl.rs`: own-process-group, pid/group liveness,
    stop/suspend/resume, the executable-file test, parent pid, and replace-self. `cargo check
    --workspace --no-default-features --target x86_64-pc-windows-gnu` is now **0 errors** — the
    gateway's 26 plus two more the entry did not know about (`tokio::signal::unix` in the
    participant pool). **The entry named three files and there were five**: `gateway.rs` and
    `agents.rs` were missing, because the count came from the compiler and the file list came from
    memory. Windows' Ctrl+Break replaces SIGTERM; SIGSTOP has no equivalent at all and the routes
    now answer with a `why` instead of a bare `false`. `pid_alive` de-duplicated against
    `rozum-core::share`. Spec: `docs/specs/windows-spawn-seams.md`.
    - Runtime gaps that remain even with it compiling, and are NOT compile errors: terminal
      sessions shell out to `tmux` and the matrix to `bash`, neither of which is on a Windows box.
      Whoever takes `windows-service-install` should decide whether those get a seam or a refusal.
      ANSWERED 2026-08-14: a refusal — `windows-tmux-bash-refusal` below carries the reasoning.
  - [x] windows-service-install — **DONE 2026-08-14, and the entry understated it: there was not a
    missing arm, there was a WRONG one.** The split was `macos` / `not(macos)`, so Windows took the
    systemd path — `rozum service install` wrote a systemd unit into `%APPDATA%` and then failed to
    spawn `systemctl`, after the file was already on disk. It compiled, and Windows CI was green,
    because the wrongness was entirely in what it did. `not(macos)` is now `all(unix, not(macos))`.
    Task Scheduler and not `sc.exe`: both existing arms install a PER-USER thing, `sc.exe` installs a
    machine service under `LocalSystem`, and the SCM kills any binary that does not speak the service
    control protocol — that is a change to the BINARY, not an arm in a file generator. The trade-off
    is recorded rather than dismissed: a task runs only while someone is logged on. Two files,
    because task XML has no element for environment variables and all three services pass one. A
    double quote is REFUSED rather than escaped (`cmd.exe` has no total escape, and the failure mode
    is a service silently started with different arguments). Proven the cross-check really compiles
    the arm by making it fail on purpose. Spec: `docs/specs/windows-service-install.md`.
  - [x] windows-tmux-bash-refusal — **DONE 2026-08-15.** Both routes refuse at the door with 501 and
    a sentence naming the missing tool: terminal sessions need `tmux` (new-session / send-keys /
    capture-pane / a PTY bridge — on Windows that is ConPTY plus a session manager, not a shim), the
    matrix needs `bash` for `scripts/bench/agentic.sh`. Previously both surfaced as the OS's own
    "program not found" from inside `Command::new`, after a registry entry or a queued job, a result
    directory and a `running` row already existed. Only `session_launch_route` is guarded —
    stop/send/output/attach take a session id and already answer "no such session", which is true.
    **The check that matters for any `#[cfg(windows)]` written on a Mac:** a deliberate undefined
    name inside the Windows body must make the cross-check FAIL. It did (two references), and
    removing it returned it to 0 — otherwise the arm compiles nowhere and says nothing.
  - [x] windows-fs-locks — **DONE 2026-08-14, and two thirds of what this entry asked for did not
    exist.** Measured before writing anything: every advisory lock in the workspace is already
    `std::fs::File::try_lock` (std, both platforms — no `fs2`/`fd-lock` needed, and the code around
    them already carries Windows-aware comments), and path handling is already `PathBuf`-based —
    four `format!("{}/…")` hits in the two crates holding room and cache paths, none of them a
    filesystem path. What WAS broken is one line the entry does not mention: **`HOME` is not a
    Windows variable**, so every path took the fallback, and `PathBuf::from("/tmp")` on Windows is
    `\tmp` on the current drive — shared by every account. `share::gateway_dir()` holds the
    residency ledger (BUG-003), and a ledger two users share is not a ledger. Fixed by
    `crates/rozum-paths` (leaf crate, no deps): `home_dir`/`state_dir`/`config_dir`/`temp_dir`, one
    rule for the six crates that had four copies of it. Spec:
    `docs/specs/windows-user-paths.md`.

- [~] portability-shared-model-source - **STARTED 2026-06-18** (branch
  `feature/native-engine-spi-a2-a3`). Step 1 DONE: created `src/model_source.rs` — an
  engine-agnostic module holding `spec_to_hf_repo` / `resolve_model_dir` /
  `config_model_type` / `ensure_model_dir`, lifted out of the MLX leaf, with the per-engine
  "can I load this `model_type`?" decision passed in as a **`gate` callback** (so a new leaf
  reuses one fetch/cache/resolve path). The MLX leaf keeps its catalog
  (`supported_model_type`/`model_type_gate`) and re-exports for zero caller churn.
  **REMAINING:** the RAM/KV **preflight** is still MLX-leaf-bound (lift when a real 2nd
  in-process consumer shapes it); wire `mistralrs`/GGUF to call `model_source` as that 2nd
  consumer (today they have their own resolution). Auto-download + hf_hub/ModelScope cache
  (`src/hf_hub.rs`, `src/modelscope.rs`) were already separate modules; `model_source` is now
  the shared front door to them.

- [x] portability-cuda-gguf — **DONE 2026-08-09.** `gguf-cuda` / `gguf-vulkan` / `gguf-rocm` pass the
  matching llama-cpp-2 backend through, and `metal` follows the TARGET instead of the request, so a
  non-Mac user never edits a Cargo.toml. Two further blockers found by RUNNING it: the admission gate
  refused every GGUF (a `.gguf` path never matched the HF catalog, so its size read as "UNKNOWN"),
  and both RAM probes were macOS-only, so on Linux the OOM gate measured nothing and failed open.
  Real inference verified on CPU through the gateway. CUDA/Vulkan/ROCm compilation itself still needs
  the hardware. Spec: `docs/specs/gguf-portability.md`.

- [ ] native-engine-spi - **Architecture: lift the reusable layer up, isolate hardware
  down (prerequisite of `x86-native-runtime`).** The decode-control loop is copy-pasted
  per engine (MLX `stream_generation`, GGUF's own loop); x86 would be a third. Define a
  tiny `LocalEngine` trait + one shared engine-agnostic `drive` loop above it (render +
  tokenize + detok→`ChatEvent` + tool-call parse incl. harmony + EOS/cancel/max-tokens +
  sampling glue), so an engine only provides `load`/`meta`/`generate` (forward + sampling
  + kernels). Token-level seam, NOT a per-op tensor abstraction — the engine keeps whole-
  graph ownership, so no `mistralrs-mlx-direct` perf floor. Hardware-independent; A1 define
  seam → A2 MLX adopts (tests/matrix/throughput unchanged) → A3 GGUF adopts + lift render/
  EOS/harmony/model-source. Net: a new engine = "implement `LocalEngine` + kernels". Full
  write-up: `docs/specs/native-engine-spi.md`. Effort: MEDIUM (behavior-preserving refactor).
  - **Progress 2026-06-18** (branch `feature/native-engine-spi-a2-a3`): A1 seam + A2a
    `consume_tokens` + A2b MLX-rewire done; `model_source` extracted (incl. the KV preflight
    estimator); **`drive` implemented + unit-tested** (generate→`consume_tokens`; render/detok
    caller-side). Remaining = the deferred-risky sub-item below + A3 GGUF/render lift (shape
    with the real x86 consumer; don't downgrade GGUF's *streaming* tool parser).

- [ ] native-engine-spi-mlx-reclaim-seam - **DEFERRED / RISKY (user: leave for later, 2026-06-18).**
  Route the **MLX** leaf formally through `LocalEngine`/`drive`. **Blocker (found in code):** the
  hybrid arches (`qwen3_5`/`qwen3_5_moe`) reclaim the generator's internal KV/conv cache *after* a
  run via `into_cache_and_snapshot()` (for next-turn prefix reuse), but the trait's
  `generate() -> Box<dyn Iterator>` return **erases** that concrete state — so forcing hybrid MLX
  through `drive` would break shipped prefix reuse. Needs a **trait cache-reclaim seam** (an
  associated `GenerationState` / `into_generation_state` hook, or engine-owned cache) — a real
  refactor of the hybrid `Generate`. Also relax `LocalEngine: Send` (MLX model is `!Send`; the seam
  is single-threaded on the worker). **Gate (mandatory):** the full agentic matrix + a
  before/after decode-throughput check on a clean machine — byte-exact greedy unchanged AND no
  prefix-reuse regression. Best shaped *with* the real x86 engine (no hybrid-reclaim quirk), per
  the spec's 2026-06-18 note. Dense MLX has no reclaim and could adopt `drive` first as a
  lower-risk warm-up. Spec: `docs/specs/native-engine-spi.md` (A2 risk section).

- [ ] x86-native-runtime - **The MLX recipe on commodity x86: a native iGPU engine.**
  Bring MLX's architectural advantage — compute on the **integrated GPU**, **unified
  memory** (no host↔device copy), **zero-copy `mmap` of the weight file** — to x86 as
  a new `ChatBackend` leaf on **cross-vendor Vulkan compute** (Intel Xe/Arc + AMD APU).
  Distinct from `portability-cuda-gguf` (that's llama.cpp's engine; this is OUR graph,
  day-one models from `model-reference/`, shared AFQ/MXFP4 quant) and from MLX-CUDA
  (discrete VRAM + copies — the opposite of the UMA thesis). Zero-copy via
  `VK_EXT_external_memory_host`; weights live once in shared RAM, model size bounded
  by total RAM like a Mac. **Reuses L1–L4** (chat template, `parse_tool_calls`, the
  harmony adapter, the CPU sampler, the `model-reference/` forward math); writes only
  **L5** (Vulkan tensors + memory + quant/attention kernels + the decode loop).
  Feature-gated `--features x86-native` (off by default). Honest caveat: own kernels
  ⇒ won't match MLX speed day one — bank correctness + day-one models + zero-copy
  memory first, perf is a separate tuning track (bar = llama.cpp-Vulkan on the same
  iGPU). Phased: **P0** probe (Vulkan device + zero-copy import on both vendors) → **P1**
  MVP dense forward (Qwen3-4B, greedy parity vs MLX) → **P2** AFQ quant kernels
  (zero-copy) → **P3** MoE + gpt-oss (gather-qmm, MXFP4, sinks, sliding, YaRN, harmony)
  → **P4** perf → **P5** catalog + ship. Decisions locked 2026-06-17: Vulkan + own
  kernels, cross-vendor iGPU. Full write-up: `docs/specs/x86-native-runtime.md`.
  Effort: LARGE (a forward engine + GPU kernels from a blank page).
- [ ] portability-heterogeneous-devices - Utilize a commodity x86 box's
  **discrete NVIDIA GPU + integrated GPU (UMA) + CPU concurrently**. NOT by
  splitting one model across them (a trap: the throughput gap + PCIe/UMA
  interconnect makes heterogeneous tensor/pipeline parallelism net-negative), but
  by **device-pinned multi-instance**: a fast worker model on the dGPU
  (`gguf-cuda`), a small utility/draft/router/embeddings model on the iGPU
  (`gguf-vulkan`, tapping the big DRAM via UMA), overflow on CPU — routed by the
  cascade/multislot by size-class **+ device**. The one genuine single-stream
  co-use is **speculative decoding**: draft on iGPU/CPU, target verifies on the
  dGPU (rozum already has a spec-decode draft track). Generalizes
  `shared-gateway-multislot` (one-GPU co-residency) to N heterogeneous devices +
  per-device budgets. Prereqs: `portability-cuda-gguf`; a device-pinning notion
  in the backend builder; `resident::plan_residency` extended across devices.
  Note: the native-MLX perf work does NOT port (Apple/Metal-only) — the x86 path
  is the `ChatBackend` SPI + GGUF/CUDA-Vulkan + HTTP backends. See the 2026-06-17
  discussion.

#### Extractions — pull leaf-bound work into modules keyed by their *true* dependency

The taxonomy + rationale is in `docs/specs/portability-and-the-backend-spi.md`
("Taxonomy by dependency" / "What to extract"). Each item below pulls something out
of the MLX leaf into a module that depends only on hardware, or only on the model,
or on nothing — so any engine can reuse it.

- [x] extract-shared-serving-helpers - **L1. STARTED 2026-06-16** (`src/serving.rs`).
  Tool-call parsing is unified there: MLX's whole-text `parse_tool_calls` and GGUF's
  streaming `tool_name` both call it (the duplicated body-parsing is gone). It was also
  made **robust** — when a model emits no `<tool_call>` envelope (common for 4B–7B models
  driven by a foreign tool schema, which fall back to a bare or ```json-fenced
  `{name,arguments}`), the call is recovered via a string-aware balanced-brace scan with a
  strict `arguments`-is-object guard against false positives; native `<tool_call>` blocks
  suppress the fallback. Validated end-to-end: Coder-7B's lost tool calls now execute.

  **CLOSED 2026-08-14 — each of the four remaining items measured against the code first, and
  three of them no longer existed.**
  - *UTF-8-safe incremental detokenize* and *multi-EOS stop* — **already lifted**, into
    `rozum_core::engine::consume_tokens` (`crates/rozum-core/src/engine.rs:277`) by the
    `native-engine-spi` A2a work: it streams the decoded suffix with a `\u{FFFD}` trim and stops on
    `meta.eos.contains(&id)` — a SET, which is the multi-EOS half. Both engines drive it.
  - *KV/RAM preflight* — **was genuinely duplicated, and the two copies had diverged.**
    `src/main.rs` had its own KV math beside `model_source::kv_bytes_per_position`, and only the
    `main.rs` copy read `layer_types`. Since the shared one feeds the RESIDENCY GATE, a config
    carrying the list without `full_attention_interval` would have been counted as all-layers —
    for the shape of the model that runs here, a **4× over-estimate of the KV cache**, i.e. the
    gate refusing a load that fits. Fixed: one implementation, `layer_types` first, plus the
    multi-head (`num_attention_heads`) fallback the other copy had. The surrounding footprint is
    deliberately NOT shared — candle's ~5% scratch and MLX's cache+prefill reserve (~5.5 GiB,
    smmr-D) describe two runtimes, and merging them would refuse loads that fit.
  - *tool-history rendering (`message_text`)* — **should not be lifted, and the entry's premise was
    wrong.** The three functions of that name are three different jobs: `auto_context`'s flattens
    Text+ToolResult with spaces for summarization; `mistralrs_backend`'s takes Text only because
    tool calls go structurally through `message_tool_calls`; the MLX one re-renders `ToolUse` into
    Qwen `<tool_call>` markup plus image placeholders. Unifying them would either impose Qwen
    markup on mistralrs or strip it from MLX. Same name, three subjects.

- [ ] mistral-system-fold — **WON'T DO (2026-06-16).** A restrictive chat template (Mistral-7B-v0.3:
  rejects the `system` role via `raise_exception` + needs strict user/assistant alternation) 500s on
  every Claude Code request (which sends a system message + tool results). Folding system→first-user
  when a template lacks system support would un-break it — but **only Mistral-v0.3 needed this**, and
  it's been deleted from the cache + benchmark; all kept models (Qwen2.5/Qwen3/Qwen3.6) support the
  `system` role natively. Not worth the message-rewriting complexity for a model we don't use. Reopen
  only if a future restrictive-template model we actually want shows up.

- [ ] extract-l5-track-upstream - **L5 (no extraction — discipline only).** Engine
  -binding fixes (RoPE reshape, zero-buffer, buffer-donation/`eval`, `mx.compile`
  finding, the `metal_kernel` mlx-c binding) are irreducibly engine-specific. Keep
  pushing them upstream so the *ecosystem* carries them (done: 4 mistralrs PRs + the
  mlx-rs fork fixes); this item is just the standing reminder to upstream, not vendor.

### Agent integration (busi) — DISTRIBUTED-FIRST

**busi is the agent; rozum is a stateless model service it calls over HTTP.** The
orchestration/session state lives in busi (so rozum scales + fails over for free);
the agent loop + the generic plumbing live in a **scalascript "agent SDK"** (generic,
reusable by any app), and the accounting tools/prompts/eval are busi on top. Design +
the three contracts (model-call API / agent loop / tool) + the generic-vs-domain
layering: `docs/specs/integration.md`. The rozum items here are
just the model-service side; the SDK + tools are owned by the scalascript/busi side.

- [ ] rozum-embed-crate - **P2 (rozum, optional). DEFERRED — not needed for now** (2026-06-15,
  user's call). Stable minimal public crate (`rozum-embed`) for the in-process embedded mode (Rust
  busi component + small model): build a backend, run the reference agent-runtime, pick a tool source.
  The runtime itself (`src/agent.rs`) already exists; this is only the packaging-as-a-crate, which is
  not currently wanted. Revisit if an external Rust embedder appears.

- [~] structured-output-for-tools - **P2 (rozum). v1 SHIPPED 2026-06-15.** Constrained
  decoding that enforces a tool call's arguments against the tool's JSON schema *during*
  decode, so a small local model cannot emit an invalid argument object. Spec:
  `docs/specs/constrained-tool-decoding.md`.
  - **Engine** (`src/constrain.rs`): a JSON-Schema subset → incremental **prefix
    acceptor** (`Schema::prefix` → Complete/Partial/Invalid). Subset = object
    (properties/required, additional props forbidden → keys restricted), string (+enum/
    const), integer, number, boolean, array-of-scalar, nested object; anything else
    relaxes to generic well-formed JSON (never over-rejects). Stateless re-parse of the
    whole suffix each step. 6 model-free unit tests.
  - **Sampler mask** (`mlx_native_backend.rs`): a generic B=1 decode loop
    (`constrained_decode_loop<C>`) that masks the logits to the top-K candidates whose
    decoded piece keeps the body a valid prefix (widen 256→4096→full, argmax fallback), then
    runs the normal sampler. Runs on BOTH the dense KV path (`run_constrained_dense`, every
    dense arch) and the Qwen3.6 **hybrid** `LayerCache` path (`run_constrained_hybrid`).
    Behind `ROZUM_MLX_CONSTRAIN` (OFF by default → free path byte-identical).
  - **Two formats** (2026-06-15): picks the envelope from the first body char after
    `<tool_call>` — JSON Hermes `{…}` (Qwen3) or XML `<function=…>` (Qwen3.6/Coder), via
    `Constraint::{Json, Xml}` + `xml_prefix`. The JSON path resolves `arguments` once `name`
    is read; the XML path constrains `NAME`/`KEY`/required + `enum` `VALUE`s.
  - **Validated** on both: `mlx_constrained_tool_call_conforms` (Qwen3-4B, JSON) and
    `mlx_constrained_tool_call_hybrid` (Qwen3.6-35B-A3B, hybrid+XML). Discriminating enum
    `["kelvin","rankine"]` vs a "celsius" prompt → output `unit:"kelvin"` on both, proving
    the mask bites. 141/0.
  - **Follow-ups**: full JSON-Schema (`oneOf`/`$ref`/patterns); typed (number/bool) XML
    values (only `enum` is strict there today); a general `response_format: json_schema`
    request field reusing the engine; expose over Contract-1 so the SDK just passes schemas.

- [ ] busi-eval-and-tune - **P1→P3 (busi-side; rozum hooks only).** busi/scalascript
  build the eval harness (20–50 real flows + task-success metric) to pick the smallest
  model that clears the bar; then QLoRA a small model on collected `(prompt →
  tool-call)` traces (offline; see `tune-toolcall-format`) → a fast, private,
  on-device busi model. rozum side: serve the merged checkpoint (already works) +
  decode determinism (`temperature:0`) for reproducible eval.

  NOTE: the **generic scalascript agent SDK** (model HTTP/SSE client, agent loop, tool
  framework, schema derivation, endpoint pool/retry — the "build once, reuse in any
  app" layer) is owned by the scalascript/busi side, not rozum — full design + public
  API in `docs/specs/agent-sdk.md`. rozum provides the gateway contract +
  the optional Rust reference runtime as its executable twin.

### Native MLX runtime — backend feature parity (vs mistralrs)

Audit 2026-06-11 (`docs/specs/mlx-native-runtime.md` "Backend feature parity"):
features the mistralrs backend shipped that the native backend does NOT yet have.

## Deprioritised 2026-08-04 — the model is frozen on Qwen3.5-4B

**Revives when:** stated per item below. Two entries that carried no condition at all were moved
to LIVE on 2026-08-08 — they depended on nothing.

The operator settled on a single model: **`mlx-community:Qwen3.5-4B-MLX-4bit`**, the one model
actually on disk (every other tier — gpt-oss, GLM, Devstral, Qwen3-Coder, 35B — is no longer in
`~/.cache/huggingface`). Everything below was moved out of `SPRINT.md` on 2026-08-04 because it
only pays off for a model, a driver, or a hardware target that is not in use. Each entry is kept
VERBATIM so it can be promoted back unchanged; the **Parked because** line says what would
revive it.

**Parked because:** Operator froze the model on Qwen3.5-4B (2026-08-04): there is no routing decision left to make, and the other tiers are not even on disk any more (only Qwen3.5-4B-MLX-4bit is cached). Revive only if a second model is adopted.

- [ ] **B2 (original) — one authoritative full matrix** (the real baseline + the data for routing) — now all 3
  drivers work + all fixes in: `claude+codex+opencode × curated-tier × all tasks`, `RUN_TIMEOUT=900`,
  REPS≥1, capture on. Produces (a) the authoritative honest number, (b) the `model × driver` capability
  table that B3 needs. Slot-gated, ~2h — run in background.

**Parked because:** gpt-oss under codex/opencode. Neither the model (not cached) nor the driver is in use — the live setup is Qwen3.5-4B under the `claude` harness. Detailed root-cause notes kept below; the BACKLOG entry of the same slug holds the rest.

- [x] **codex-opencode-create-delivery — CLOSED 2026-08-09: the pinned root cause was fixed and the
  fix is in the tree.** The entry is the ORIGINAL NOTES, kept as evidence, but it sat as an open
  checkbox describing a live defect. `rewrite_json_wrapped_apply_patch`
  (`crates/rozum-gateway/src/codex_patch.rs:104`) undoes the JSON wrapping this entry pinned, and
  the sibling entry `codex-create-delivery-on-qwen` records the result: build delivery 0/3-land →
  3/3-land on codex × gpt-oss. One residual (`rpn`) is tracked there — a question, not this defect.
  Notes retained below.
  *(original notes, NOT a queue item)* — see BACKLOG
  `codex-opencode-create-delivery` for the full evidence. ROOT CAUSE PINNED: gpt-oss (via codex) emits
  `apply_patch -patches '[{"content":"*** Begin Patch\n*** Add File: …*** End Patch"}]'` (patch wrapped in
  a JSON array under `-patches`, body JSON-escaped `\n`/`\"`). `rewrite_apply_patch_command`
  (crates/rozum-gateway/src/gateway.rs ~2232) only undoes SHELL double-quote escaping, not JSON, so the
  block keeps literal `\n`, `apply_patch_block_to_fuzz` can't parse the `*** Add File:` directives, the
  rewrite returns None, the original runs against the real shim → `apply_patch accepts exactly one
  argument` → no files → codex loop-breaker → rc11. On the ucc run this is `deliver 12` (codex) + `deliver
  13` (opencode) of the curated-tier failures.
  EXACT STEPS: (1) in `rewrite_apply_patch_command`, before the shell-unescape, detect the JSON-wrapped
  form — an `apply_patch` arg that is (or contains) a JSON array/object with a `content` field; when so,
  `serde_json`-decode each object's `content` into a real-newline V4A patch string and run each through the
  existing `apply_patch_block_to_fuzz` (which already yields `synth_create_command` `cat > <path>` heredocs
  for `*** Add File:`), concatenating the results. Keep the existing raw/heredoc path for the non-JSON form.
  (2) Also capture codex×Devstral×build's rc11 emission shape (kept workdir `/tmp/rozum-agentic-Rf1YJM`
  showed nothing on the first grep — re-inspect) and cover it if different. (3) `cargo build -p rozum`
  (builds the gateway bin — NOT target/release/rozum; see [[reference-rozum-binary-split]]).
  VERIFY (GPU-gated, slot must be free): `AGENTIC_MODELS="mlx-community:gpt-oss-20b-MXFP4-Q4" AGENTS=codex
  TASKS=build REPS=3 REPAIR=1 KEEP=1 BENCH_BIN=./target/release/rozum-gateway bash scripts/bench/agentic.sh`
  — expect build to go from 0/3 → passing, and inspect a kept workdir to confirm Cargo.toml + src/main.rs
  actually land (no "accepts exactly one argument"). Do it on a `feature/codex-create-delivery` worktree
  off origin/master; do not push until verified.

**Parked because:** Qwen3-Coder-30B — a DIFFERENT model from the frozen Qwen3.5-4B, and not on disk. Parts (a) and (b) already shipped; only the GPU-gated live verify remains, and it cannot run without the model.

- [ ] **qwen-coder-edit-toolarg-decode** (HIGH; board updated 2026-07-08 — the entry was stale) —
  Qwen3-Coder edit-path (fix/test) corruption: XML-entity escaping in `<parameter>` values.
  (a) DONE `3005e3f` (R2.1, 2026-07-05): html-entity decode (`&quot;` `&lt;` `&gt;` `&apos;` `&amp;`)
  in the `<parameter>` string fallback, unit-tested. The board previously said "no decoding exists" —
  that predated R2.1.
  (b) newline loss: hypothesis — same escaping mode encodes line breaks NUMERICALLY (`&#10;`), so a
  multiline file arrives as one line. DONE (this commit): `&#10;`/`&#13;`/`&#9;` decode + unit test
  reproducing the exact one-line-doc-comment failure shape. Additive/safe: numeric whitespace
  entities never legitimately appear in intended file content.
  REMAINING (GPU-gated, queued in the RAM window behind the B2-GLM matrix): live verify
  `AGENTIC_MODELS=Qwen3-Coder-30B AGENTS=claude TASKS="fix test" REPS=3 REPAIR=1 KEEP=1
  ROZUM_RAW_DUMP=1` → expect fix/test pass + a kept workdir with real multiline src/main.rs; RAW_DUMP
  settles the hypothesis if cells still fail (then the collapse is model-side and needs a different
  lever). Original kept workdirs are gone (/tmp cleaned) — RAW_DUMP recaptures evidence.

**Parked because:** GLM-4-32B under codex/opencode — model not cached, driver not in use.

- [ ] **glm32b-codex-timeout** (MED, cheap wall-clock) — GLM-4-32B under codex/opencode times out (rc124)
  on ~7 curated cells; dense 32B fits resident, so cost is per-turn reload/slowness, not OOM. Lever: keep
  GLM-4-32B resident (EAGER) for the run, or a driver-specific higher RUN_TIMEOUT. See BACKLOG.

**Parked because:** Bench-harness polish whose failing cell is Devstral (not cached). With the full matrix parked this has no reader.

  *(also listed under “Matrix improvement levers” until 2026-08-08 — the same copy-not-move. It belongs HERE: it is a GLM-under-codex timeout and neither is on this machine.)*
- [x] **mlx-glm4-moe — CLOSED 2026-08-09: the half that fits this machine SHIPPED; the other half is
  hardware-blocked, not effort-blocked.** `glm4_moe_lite` / GLM-4.7-Flash — the member this entry
  calls "the only one that FITS" — was ported with absorbed-MLA (`e8c060a`) and is live:
  `crates/rozum-gateway/src/control.rs:748` carries it as *"15/15 full matrix, in DEFAULT_MODELS"*,
  and `mlx_native_backend.rs` routes the `glm4_moe_lite` model_type. So "a NEW attention we don't
  have → HIGH effort" is spent, and the sibling entry `mlx-mla-attention` already records it.
  What genuinely remains is `glm4_moe` (GLM-4.5-Air/4.6) — **easy GQA, no new attention, blocked
  only by 36 GiB of RAM.** That is a hardware gate, so it is not backlog work here; it becomes a
  half-day port the day rozum runs on a bigger box, which is exactly the North Star case. Kept
  below verbatim for that day: the MoE-side shapes were measured against the checkpoint and are
  still the load-bearing detail. Fork scaffold parked at `feature/glm4-moe`.
  Original entry follows.
  *(original entry, retained for the measured shapes — NOT a queue item)* — port GLM-4
  MoE to native MLX. **REPRIORITIZED → bigger than thought**
  (checkpoint inspection 2026-06-27, spec `docs/specs/glm4-moe-native.md`): the family splits by
  attention and it's adversarial — `glm4_moe` (GLM-4.5-Air/4.6) is easy GQA but **too big for 36 GiB**;
  `glm4_moe_lite` (**GLM-4.7-Flash**, 16.9 GB, the only one that FITS) uses **MLA** (DeepSeek-V2-style
  latent attention: q_a/q_b + kv_a, q_lora 768 / kv_lora 512 / qk_nope 192 / qk_rope 64 / v 256) —
  a NEW attention we don't have → **HIGH effort, same work as `mlx-port-deepseek-v2` (do together)**.
  The "reuse glm4.rs attention" plan was WRONG (verified before writing code — discipline paid off).
  MoE side IS adaptable (sigmoid + correction-bias + flat top-k(4, n_group=1) + shared expert +
  routed_scaling 1.8; naming `mlp.switch_mlp.*`/`mlp.shared_experts.*`/`mlp.gate.e_score_correction_bias`;
  first_k_dense_layers=1 ⇒ mixed dense/MoE, which the fork doesn't yet handle). **Decision: defer the
  MLA port; the matrix win is `matrix-add-coders` (Qwen3-Coder, zero port) — run that first.** Fork
  scaffold parked: `feature/glm4-moe` (`.vendor/mlx-lm/.../models/glm4_moe.rs`, NOT in mod.rs).

**Parked because:** A cascade needs at least two models; there is one. Revive together with any second-model decision.

- [ ] **gptoss-codex-cascade** (stretch, now ALSO the GLM lever) — gpt-oss/GLM for speed, auto-fall-back
  to 35B on a failed cell (the `CascadeBackend` exists). Best-of-both: fast when the small model succeeds,
  35B-reliable when it doesn't. The matrix proved 35B is the agentic driver (14/15) and GLM is not (4/15,
  multi-layered tool-use non-robustness per `glm-shell-delivery-fix` above) → cascade is the highest-
  leverage RELIABILITY lever for the weaker models, without fighting their nature.

**Parked because:** Its own TRIAGE already says it needs an operator decision first and rewrites the matrix-critical request/SSE path — highest risk, no payoff for the single frozen model.

- [x] **plugin-wireprotocol — DONE 2026-08-14 on the operator's override, in the form the
  re-measurement justified: the SPINE, not the extractors.** The Stage-3 rejection was two-thirds
  right and both live thirds are honoured — each dialect keeps its own typed extractor (request
  validation untouched) and its own SSE sequence (bytes untouched). What that investigation did not
  weigh is what sits BETWEEN parse and serialize: lease, auto-context fit, elision note, token
  estimate, `ChatRequest`, loop-breaker, metering, generation timeout, stream/collect branch — ~45
  lines written three times on the path the whole matrix runs through, and already drifted
  (`/v1/messages` accepts no `top_p`/`top_k`, both in its own API; left unfixed here on purpose and
  recorded as data). Now `trait WireDialect` + `serve_wire`. **Costs 44 lines MORE code than it
  replaced** — the win is that the spine cannot drift again, not brevity. Gate: a byte-level golden
  (`crates/rozum-gateway/src/testdata/wire-golden.txt`) frozen in its own commit BEFORE any handler
  moved, byte-identical after, covering both the response bytes and the request that reached the
  backend. NOT re-run against the agentic matrix — that needs the model slot. Specs:
  `docs/specs/wire-dialect-seam.md`, and `architecture-spi.md` records the supersession beside the
  original call.

- [x] **plugin-services — DONE 2026-08-09, in the narrow form the operator approved: the DECLARATION
  layer only.** `src/services.rs` declares each service once (label, binary, probe, owner, shape);
  `doctor` reads it, `rozum-gateway services --json` exposes it, `install-bins.sh` consults it
  instead of its own hardcoded map, and `service.rs` takes the gateway label from it. How services
  START was deliberately NOT touched — that machinery has sharp edges (see the same day's
  publish-restart and daemon-ownership work) and the payoff here is maintenance, not capability.
  Two new findings fall out: an installed job nothing declares, and a plist running a binary other
  than the declared one. `docs/specs/service-liveness.md` § Where the list lives.

- [ ] **plugin-x86-engine** — the reserved `rozum-x86` engine slot → a real engine plugin
  behind `LocalEngine` / `ChatBackend` (the North-Star multi-device frontier).
  (Already plugin-ized: `ChatBackend`, `ToolSource` + MCP client, `ToolDialect`.)
  **(See TRIAGE — already structurally a plugin; remaining work is Vulkan kernels → needs x86 HW.)**

**Parked because:** Device detect + placement for other hardware (North Star). Nothing to place while one model runs on one Mac.

- [ ] **Phase 4 — `rozum-hardware`** (device detect + placement; North Star). Separate spec —
  reserved as a crate slot here, designed later (it is new work, not a move).

**Parked because:** Explicitly the ARCHITECTURE PREREQUISITE of `x86-native-runtime`; phases A1/A2a already landed. The rest pays off only when a second engine/hardware target exists.

- [ ] native-engine-spi - **ARCHITECTURE FIRST (prerequisite of `x86-native-runtime`).**
  Draw the internal seam every in-process engine shares so a new engine is "implement
  a tiny trait + its kernels", not "re-implement the leaf". Lift the engine-agnostic
  decode/serving logic UP into one shared `drive` loop behind a `LocalEngine` trait;
  push hardware/kernels DOWN into small isolated components. The decode-control loop
  is currently copy-pasted (MLX `stream_generation`, GGUF's own loop) — x86 would be
  a third. Hardware-independent; validated on MLX+GGUF on a Mac. Phases: **A1 [x]**
  define the seam (`src/engine.rs`: `LocalEngine`/`EngineMeta`/`drive`) → **A2a [x]**
  extract the engine-agnostic consumption loop `consume_tokens` (detok→`ChatEvent`,
  harmony + `<tool_call>` parse, EOS/max-tokens/runaway-guard, finalize) +
  `is_runaway_loop`/`next_tool_call_id`, unit-tested hardware-free → **A2b [x]** rewire
  the MLX leaf: `stream_generation` now only PRODUCES token ids (`PipelinedIds`, keeps
  the `async_eval` pipelining; lazy serial fetch so hybrid prefix-reuse stays in sync)
  and delegates to `consume_tokens` (the ~200-line copy deleted). Validated: 314 lib
  tests; gpt-oss chat+tool+~90 tok/s; Qwen3.6-27B hybrid multi-turn prefix-reuse. (A
  formal `impl LocalEngine` wrapping load/meta/generate is the remaining tidy-up.) →
  helpers consolidated to one source. **Core done — the shared layer the x86 leaf
  needs is ready** (`consume_tokens`, `sampler`, `serving`/`harmony`, model-reference).
  **A3 [IN PROGRESS — user-authorized full hardware-independent push, 2026-06-18]**
  (branch `feature/native-engine-spi-a2-a3`). Step 1 DONE: **`portability-shared-
  model-source` extracted** — `spec_to_hf_repo`/`resolve_model_dir`/
  `config_model_type`/`ensure_model_dir` lifted out of the MLX leaf into a new
  engine-agnostic `src/model_source.rs`, with the per-engine "can I load this
  `model_type`?" decision passed in as a **`gate` callback** (so mistralrs / a
  future leaf reuse one fetch/cache/resolve path); the MLX leaf keeps its catalog
  (`supported_model_type`/`model_type_gate`) and re-exports for zero caller churn.
  Verified: feature-free build green, `model_source` unit tests pass, `mlx-native
  --tests` compiles. Step 2 DONE: **`drive` implemented** (was `unimplemented!()`) —
  runs `LocalEngine::generate` over a rendered prompt → `consume_tokens`, render/detok
  stay caller-side (engine tokenizer is borrowed separately from its forward graph);
  unit-tested end-to-end via a minimal in-memory `FakeEngine`. **FINDING (blocks the
  formal MLX `impl LocalEngine`):** the MLX **hybrid** arches (Qwen3.6) reclaim the
  generator's internal KV/conv cache *after* a run (`into_cache_and_snapshot`, for
  prefix reuse), which a `generate()->Box<dyn Iterator>` return ERASES — so routing
  hybrid MLX through `drive` would break shipped prefix reuse. The trait needs a
  cache-reclaim seam, deferred to be shaped against the real x86 engine (dense MLX +
  the x86 leaf have no such reclaim and can adopt `drive` directly). NEXT: A3 GGUF
  adoption (caveat retained: don't downgrade GGUF's *streaming* tool parser;
  render/preflight lift) — also best shaped by the x86 consumer.
  Token-level seam,
  NOT a per-op tensor abstraction (avoids the `mistralrs-mlx-direct` perf dead-end).
  Spec: `docs/specs/native-engine-spi.md`.
  - [x] **engine-spi-a3-gguf — DONE 2026-06-20** (branch `feature/gguf-consume-tokens`). GGUF's
        `generate_blocking` now drives `crate::engine::consume_tokens` via a token iterator
        (`std::iter::from_fn` over llama.cpp sample→advance) + a per-token detok closure; deleted GGUF's
        private ~150-line decode loop + the streaming `ToolUseParser`/`ToolParseEvent`. SPI now proven by
        **two real engines** (MLX + GGUF). `consume_tokens` has no `Send` bound, so the `!Send`
        `LlamaContext` works on the blocking thread (couldn't use `drive()`). The "streaming→finalize"
        tool-call change is cosmetic (clients coalesce). **Surfaced + fixed a pre-existing GGUF bug:**
        `get_logits_ith(n_cur-1)` used the absolute position, but it indexes the last decoded batch
        (1-token decode batch → index 0) → after the first token it read garbage → an end token →
        generation stopped after ~1 token. GGUF was effectively broken in rozum. Fixed (track the right
        index). **Validated e2e** on `ollama:qwen2.5-coder:7b`: before — count→`"1"`, tool→`{"`; after —
        full `"1 2 … 20"` + correct `get_weather` tool_call with a cross-turn-safe id. (Step 1, the
        `next_tool_call_id` fix on `feature/gguf-toolcall-id`, was superseded here — `consume_tokens`
        already uses it.)
  - [x] **engine-spi-dense-mlx-drive — DONE 2026-06-21** (branch `feature/dense-mlx-drive`). Two parts:
        (1) **Send-relaxation** (prereq) — dropped `Send` from `LocalEngine` + `generate()`'s return; a
        feasibility map proved this (not the reclaim seam) was the real blocker, since the MLX engine
        state is irreducibly `!Send`. `drive` runs the engine synchronously on its own thread, so `Send`
        was unneeded. Proven by `drive_accepts_a_not_send_engine` (Rc-holding `!Send` engine — would not
        compile before). (2) **The adoption** — a `DenseMlxEngine` (`impl LocalEngine`) whose `generate`
        dispatches per dense arch (Qwen3/Qwen3Moe/GptOss/Llama/Qwen2/Gemma3), built from the prepared
        prefill + borrowed model+cache (split-borrow); `run_job` now routes the 6 dense arches through
        `engine::drive`, while the 2 hybrid arms stay on `stream_generation` (they reclaim via
        `into_cache_and_snapshot`, which `Box<dyn Iterator>` would erase). `drive` now has its first
        production caller; the SPI is exercised by a real engine. **Validation:** (a) byte-identical by
        construction — same per-arch generator + same `consume_tokens` with identical
        meta/prompt_len/seed/repeat_guard/decode/emit; (b) functional — the branch produced correct
        coherent greedy output on cached gpt-oss-20b (analysis-channel prime list `2, 3, 5, 7, 11, …`);
        (c) engine unit tests green. The empirical master-vs-branch raw A/B was attempted but blocked by
        RAM-starvation from accumulated 11 GB model loads (an environment limit, not the code; and
        risky to force given the GPU-memory history) — the by-construction proof + functional run stand.
        Dense path is byte-identical; no runtime change (the value is the SPI proof / x86 de-risking).
  - [~] **engine-spi-reclaim-seam — DRAFT DONE 2026-06-21** (branch `feature/engine-spi-reclaim-draft`).
        The hybrid cache-reclaim seam is now sketched + compile/FakeHybrid-validated in `src/engine.rs`:
        a `ReclaimStream` trait (`Iterator<Item=Result<u32,String>>` + `type State` +
        `into_state(self: Box<Self>) -> State`, mirroring MLX's `generator.into_cache_and_snapshot()`)
        and `drive_reclaiming(...) -> (StopReason, State)` that drains the stream through the SAME shared
        `consume_tokens` (borrowed so it survives) then reclaims its state. Two tests: `FakeHybridStream`
        round-trips a pretend KV cache through the loop (`drive_reclaiming_returns_post_run_state`) and
        through a `Box<dyn ReclaimStream>` (`..._works_through_a_trait_object`). **Deliberately unwired**
        — not used by MLX; the FINAL shape (fold into `LocalEngine`? exact `State` bounds? engine-side
        production) is to be decided against the real x86 engine. No MLX hybrid rewire until x86 is in
        play. The Send-relaxation half is DONE (above). Spec: `docs/specs/native-engine-spi.md`.

**Parked because:** Needs an Intel Xe/Arc and an AMD APU to run at all.

- [ ] x86-native-p0-probe - **P0 of `x86-native-runtime`** (after `native-engine-spi`) (the MLX recipe — iGPU +
  unified memory + zero-copy `mmap` — on commodity x86 via cross-vendor Vulkan).
  Stand up a Vulkan compute device from Rust (`ash`/`vulkano`); on BOTH an Intel
  Xe/Arc and an AMD APU confirm a `HOST_VISIBLE | DEVICE_LOCAL` heap and
  `VK_EXT_external_memory_host`, then `mmap` a safetensors file → import the host
  pointer as device memory → read a tensor back GPU-side (zero-copy). Decide the
  Rust Vulkan binding and whether to lean on a kernel lib for plumbing. Acceptance:
  zero-copy import demonstrated on both vendors + a short decision record appended
  to the spec. **Needs an x86 iGPU box** (can't be validated from macOS). Spec:
  `docs/specs/x86-native-runtime.md`; epic + phases P1–P5 in `BACKLOG.md`
  (`x86-native-runtime`).

- [x] **codex-create-delivery-on-qwen — CLOSED 2026-08-09 by running it: the bridge lands. The
  residual was gpt-oss-specific.** ANSWERED, not deferred.
  Run: `TASKS=rpn REPS=3 AGENTS=codex BENCH_PORT_BASE=8320 NCTX=32768` against
  `mlx-community:Qwen3.5-4B-MLX-4bit`, results in `scripts/bench/results/agentic-20260809-160333`.
  **2/3 pass (524.2s ✓, 416.8s ✗, 320.1s ✓) — and ZERO rc11 in three reps**, which is the answer:
  every rep delivered its files, so the create form the bridge could not land on gpt-oss does not
  reproduce on the frozen model. Cross-checked in `~/.rozum/gateway.jsonl` for the run window
  (16:03–16:29): **zero `toolcall_parse_miss`** — nothing was emitted-but-lost either.
  The single failure was rc10 and is explained: at 16:19:16 the loop-breaker fired —
  `write_stdin` called 4× with identical args and an identical result — inside that rep. The agent
  spun on stdin, was stopped, and finished with `Cargo.toml` written but no `src/*.rs`. Model
  behaviour, correctly classified, and the breaker saved ~180s of the 600s timeout.
  **Two things worth keeping from the run:**
  (1) The entry's cost line ("a GPU window that evicts the operator's resident model; ask first")
  was stale — the harness borrowed the running gateway (`sharing the running gateway on :8089`),
  resident pid unchanged, nothing evicted. An entry naming the RESIDENT model costs nothing to answer.
  (2) **The rc10/rc11 discriminator is coarser than this entry assumed.** rc11 fires only when
  `Cargo.toml` is absent, so a run that writes the manifest and loses the source reads as rc10
  ("the model's fault") even if the source write had been lost by us. It wasn't, this time — the
  obs log settled it. But do not treat rc10 alone as proof of a model-side failure; check
  `toolcall_parse_miss` for the window. FIXED 2026-08-14 — that shape is `rc=12`, and the seeded
  tasks, where presence could not answer it at all, are `rc=13`. See `CHANGELOG.md`
  `bench-rc-partial-delivery` and `bench-rc-seeded-nondelivery`.
  *(original question, retained)* — does the `apply_patch` bridge land codex's CREATE forms
  when the driver is the frozen model?
  **Why:** `codex-opencode-create-delivery` shipped `rewrite_json_wrapped_apply_patch`
  (`crates/rozum-gateway/src/codex_patch.rs:104`) and proved it on codex × gpt-oss: build delivery
  went 0/3-land → 3/3-land. One residual never closed — `rpn` still threw a single rc11, a create
  form the bridge does not fully land. That evidence path is gone (gpt-oss is off disk), but codex
  is still installed and still a driver for Qwen3.5-4B, so the question survives its evidence.
  **How:** `TASKS=rpn REPS=3 AGENTS=codex BENCH_PORT_BASE=8320 NCTX=32768
  ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 scripts/bench/agentic.sh`. rc11 = patch delivery (ours), rc10 =
  the model wrote wrong code (not ours) — the distinction is the whole point of running it.
  **Cost:** a GPU window that evicts the operator's resident model; ask first.
  **Gotcha:** the bench opens with `gateway stop --force`; launchd brings `com.rozum.gateway` back.
  **Done when:** either an rc11 is captured with its `-patches` shape (then it is a real bridge gap
  and becomes a BUGS entry), or three reps come back clean and this closes as gpt-oss-specific.

- [ ] **gpt-oss-20b (closed on the sprint 2026-08-05 — pointer only)** — the model is not on disk and
  `models list` shows one. Kept as a line so the name resolves: the sprint entry holds the findings,
  and the five gateway delivery bugs it drove out are shipped and independent of it. Reopen only if
  gpt-oss is downloaded again, and re-measure rather than trusting the old numbers.

- [x] **shared-checkout-guard — ALREADY DONE, verified 2026-08-08.** Landed the same day as the
  entry (`f0c6dd2`): `scripts/githooks/pre-commit` + `install.sh` (repo-level `core.hooksPath`, so
  it covers worktrees) + a nine-case test, documented in `AGENTS.md`. The entry claimed "nothing
  enforces it", which had stopped being true hours earlier. Verified rather than believed: the test
  passes 9/9, and a real `src/doctor.rs` edit staged in the shared checkout was refused live. The
  sweep case is covered too — a sibling's `git add -A` in the shared checkout stages the stray file
  and gets the same refusal. Known limit, documented in the hook: a CLEAN `git revert` on master is
  refused because git writes no marker for one.

- [x] **doctor-deployment-drift — DONE 2026-08-08.** Binaries carry the commit they were built from
  (`crates/rozum-stamp`, read from the FILE, never by running the service); `doctor --services`
  counts the distance to `origin/master` and warns. Unstamped is warned about too — its age is
  unknown, and unknown reported as silence is the failure being fixed. Proven live in both
  directions: freshly deployed rows silent, `svc:mcp-http` reporting itself 1 commit behind.
  Spec: `docs/specs/deployment-drift.md`.
