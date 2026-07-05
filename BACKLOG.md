# Backlog

## opencode → gateway `/v1/messages` 500 (found 2026-07-05, r3-cumulative run)

- [x] **opencode-500-v1-messages** — RESOLVED (2026-07-05). NOT a rozum/gateway bug and NOT the tool-call
  fixes. ROOT CAUSE: opencode's OWN internal SQLite DB was on a stale schema after the v1.16.2 update —
  `opencode run "…"` failed even with NO gateway involved, and `~/.local/share/opencode/log/*.log` showed
  `ERROR service=server error=no such column: replacement_seq  cause=SQLiteError` at
  `SessionContextEpoch.requestReplacement → SessionPrompt.createUserMessage`. opencode's error wrapper
  ("UnknownError: Unexpected server error, ref=err_…") is opencode's INTERNAL server, not the gateway
  (r3 gateway.log was clean, no panic/500 logged). FIX: backed up the broken DB
  (`~/.local/share/opencode/opencode.db` → `.broken-replacement_seq.bak`, reversible — preserves the
  user's opencode history) so opencode recreates a fresh DB with the current schema; `opencode run`
  returns `ok` again. Lesson: rule out the DRIVER before suspecting the gateway — reproduce the failing
  driver in isolation first.

## Matrix improvement levers (found 2026-07-05 during the matrix-hygiene analysis; evidence in agentic-ucc-1783166880)

The honest read of the curated tier is claude 89% / codex 33% / opencode 47% (summarize_matrix.py now
shows this + fail-mode rollup). The two big NON-model levers, ranked:

- [x] **codex-opencode-create-delivery** — DONE (master `73b6d64`, `rewrite_json_wrapped_apply_patch`).
  E2E-verified: build delivery 0/3→3/3 land (0 rc11), bridge fired 8×, 1 pass, shim error gone. RESIDUAL
  follow-up: **rpn still emits 1 rc11** — capture the rpn `-patches` shape (kept workdir under
  `/tmp/rozum-agentic-*` from the verify-codex-create run) and cover the form the bridge misses (likely an
  `*** Update File:` against an absent file, or a non-`content` JSON key). Remaining build reds are rc10 =
  gpt-oss wrong CODE (model capability, separate from delivery). Original evidence:
- [ ] **codex-opencode-create-delivery (original evidence)** — a THIRD+ of
  codex/opencode curated-tier failures are `deliver` (rc11 = wrote NO project files) on create-from-scratch
  (`build`/`test`), NOT wrong code. Kept-workdir evidence: codex emits an `apply_patch` *Add File* that
  never lands in the jail (the file isn't created), codex then re-verifies a "change already applied",
  trips its OWN loop-breaker ("Stopping to avoid an infinite loop"), and exits with an empty workdir →
  rc11. The gateway already has Method B (apply_patch→`patch --fuzz`) + an Add-File→shell rewrite
  (gateway.rs ~2264) for the EDIT case; the CREATE case still leaks for the curated models. **EXACT root
  cause (kept workdir, codex×gpt-oss×build):** gpt-oss emits
  `/bin/zsh -lc "apply_patch -patches '[{\"content\":\"*** Begin Patch\\n*** Add File: Cargo.toml\\n+…\\n*** Add File: src/main.rs\\n+…*** End Patch\"}]'"`
  — the patch is wrapped in a JSON array under a `-patches` flag, so the V4A body is JSON-escaped
  (`\\n`, `\\\"`). `rewrite_apply_patch_command` (gateway.rs ~2232) locates `*** Begin Patch…*** End Patch`
  but only undoes SHELL double-quote escaping, not JSON escaping — so the extracted block keeps literal
  `\\n` (not real newlines), `apply_patch_block_to_fuzz` can't parse the `*** Add File:` directives, the
  rewrite returns None, the ORIGINAL `apply_patch -patches '[…]'` runs against the real shim →
  `Error: apply_patch accepts exactly one argument` → nothing written → codex re-verifies → its own
  loop-breaker → rc11. **Fix:** in `rewrite_apply_patch_command`, detect the `-patches '[{...}]'` /
  JSON-wrapped form, `serde_json`-decode each object's `content` (→ a real-newline V4A patch), then feed
  each through the existing `apply_patch_block_to_fuzz` / `synth_create_command` path. Verify:
  codex×gpt-oss×build (+ re-check codex×Devstral×build — its rc11 emission shape still TBD, capture while
  implementing). REQUIRES a gateway rebuild + GPU-free slot to test (do NOT run while a matrix holds the slot).
- [ ] **glm32b-codex-timeout** (MED) — GLM-4-32B under codex/opencode times out (rc124) on ~7 curated
  cells; it's a dense 32B that fits resident, so the cost is per-turn reload/slowness under those drivers,
  not OOM. Lever: EAGER co-residency / keep-resident for GLM-4-32B alone, or a driver-specific higher
  RUN_TIMEOUT. Cheap wall-clock win (each timeout burns the full ceiling).
- [ ] **test-cell-repair-failfast** (LOW, from B) — when a repair attempt hits an Edit-before-Read churn
  loop it burns the whole RUN_TIMEOUT (rc124) without converging; `repair_tool_protocol_hint` fires one
  attempt too late (loop is in the FINAL attempt). Lever: detect the churn live and fail-fast, and/or grant
  ONE bonus repair attempt AFTER the protocol hint is first triggered so the hint can actually apply.
  Note: harness already feeds the whole-file `repair_benchmark_recipe` ("replace the file, don't use Edit")
  on repair — Devstral ignores it, so this is bounded by model compliance, not just harness logic.

## scalascript language gap: theme page-background never reaches `serve(view, port)` (found 2026-07-03, ucc-theme-bg)

- [ ] **ssc-serve-extracss-or-theme-body** — `std/ui/primitives.ssc`'s `serve(tree: View, port: Int)`
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

## Native MLX model ports (matrix coverage, lower priority — operator 2026-06-27)

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

- [ ] **glm-artifact-write-synth** (idea, NOT committed — clean workaround exists) — let GLM-4-32B
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

## Context management (operator 2026-06-28 — "make it just work without limits")

- [x] **gateway-auto-context** — **DONE** (mechanisms complete; `context_length_exceeded` is NEVER returned
  on any of the 3 request paths). `fit_to_context()`: (1) **sliding-window turn trim** (`4310a63`); (2)
  **extractive rolling-summary breadcrumb** (`057bd48`+`a307401`) — a system note telling the model history
  was trimmed AND the topics it covered (first ~80 chars of each dropped turn, no model call); (3)
  **lazy-tools** (`23a330f`) — strip tool descriptions when a fat system+tools surface can't be fit by
  turn-dropping (every tool kept). Default ON (`ROZUM_AUTO_CONTEXT=0` = legacy error), unit-tested, obs
  `auto_context_trim{dropped,tools_compressed}`. (4) **ABSTRACTIVE LLM rolling-summary** (`89de0b9`, opt-in
  `ROZUM_AUTO_CONTEXT_SUMMARIZE=1`, default OFF) — `with_elision_note` generates a terse summary of the
  dropped turns via the resident model (the summary gen serializes before the real gen; falls back to the
  extractive note on any failure). Default OFF = production hot path untouched; opt-in adds a summarizer
  gen per overflowing request. **ONLY OPTIONAL REMAINING (LOW VALUE, NOT blocking):** true on-demand lazy
  tool LOADING (a meta-tool the model calls to fetch a schema) — superseded by the description-strip (weak
  models won't reliably request schemas anyway); + a summary CACHE so consecutive overflowing requests
  don't re-summarize the same dropped prefix (a perf nicety for the opt-in path). Original design below.
  A gateway-side context-management layer so a request that exceeds
  the model's `n_ctx` **never returns `context_length_exceeded`** to the client — it transparently
  fits the window instead, for ANY agent (claude/codex/opencode) against ANY model. This is North-Star
  ("intelligence on any model/hardware"): the agent shouldn't have to know the model's window.
  **Honest physics first:** a transformer attends over ≤ `n_ctx` tokens per forward — you CANNOT
  losslessly fit more in one pass. So "without limits" = **never-error + arbitrary length, with
  *managed* loss**, not infinite full-fidelity attention. Pick what to drop, deliberately, instead of
  erroring. Mechanisms, dispatched by WHAT overflows:
  (a) **conversation > window** (the agentic case): **rolling compaction** — keep system + last-K turns
      verbatim + a running summary of the evicted older turns (a cheap side-call summarizes the middle;
      "split into several prompts" literally). Unbounded session, graceful. This is what Claude Code
      itself does client-side — we do it server-side, for all agents.
  (b) **one giant input** (a huge file/message): **map-reduce** — chunk → process each → reduce. Literal
      multi-prompt. For understand/summarize-a-huge-thing.
  (c) **giant static prompt** (codex: system + 18 tool schemas > window): **slim** — codex-lean (have it)
      + **lazy tool-schemas** (offer a few; fetch the rest on demand via a meta-tool). Can't split an
      indivisible "use these tools" coherently — shrink the floor instead.
  (d) **knowledge too big**: RAG — index outside, retrieve top-K relevant into a bounded window.
  **Caveat that bounds the win:** the floor is the model's `n_ctx`; the *effective* context is unbounded
  but lossy. Note: `context_length_exceeded` was NOT a real prod issue for codex-on-Qwen3-4B (auto-ctx
  = 40960 fits its prompt; it only bit when I hand-set `--n-ctx 8192`). The real target is (a) long
  agentic sessions + (b)/(d) big inputs. **Open design Qs (operator to weigh):** how aggressive the trim
  (how many turns verbatim?), and summarize with the SAME model (cheap, lower quality) vs a small
  dedicated summarizer. Chunked-PREFILL (`Generate` edit, exists) is orthogonal — it cuts the memory
  spike of a long-but-FITTING prompt, it does NOT extend the window.

## Meetings → product-support / incident platform (STRATEGIC — operator 2026-06-28)

**Direction:** rozum meetings are not just agent chat — they are the substrate for **product support
with escalation + resolving + per-incident context collection**, where AI agents are first-class
participants (triage, gather context, escalate, resolve) alongside humans. Think Slack+Zendesk+PagerDuty,
agent-native. A room/thread IS an incident; context (logs, history, related messages, artifacts) accretes
to it; messages carry support metadata; agents drive it toward resolution. Big perspective tasks, built
on the existing meeting stack (`docs/specs/agent-meetings-daemon.md`, `meeting-identity-roster.md`,
`meeting-mention-inbox.md`, `meetings-rest-read.md`; daily disk-backed rooms, session-token identity,
single-writer daemon). Each item below is its own spec+build later — listed to set the trajectory.

- [x] **mtg-ssc-request-handlers** (operator 2026-06-29) — DONE + LIVE-PROVEN. The `.ssc` route handler
  surface is a path/body STRING (no `Request`), so cookies were unreachable. Instead of migrating the whole
  route layer to `Request`, added a narrow scalascript runtime capability **`requestCookie(name): String`**
  (thread-local snapshot of the current request's Cookie header, published in `handle_request` right before
  the sync handler — no `.await` between, so no cross-request leak; scalascript `feature/rust-request-cookie`
  `9db95f21d`). On the rozum side: new CLI **`rozum meetings token resolve <tok> [--room R]`** → `handle\trole`
  (the `.ssc`'s bridge), and `meeting.ssc` now reads the `rozum_token` cookie → resolves handle + per-room
  role, **gates the incident action forms** (observer = "только чтение"), **re-checks the role server-side**
  on POST `/do` (a hand-crafted POST can't bypass), **attributes actions** via `--as <handle>` (`incident --as`
  already existed), plus a **`/login`** page (sets the cookie client-side) + an actor chip. Policy is
  PERMISSIVE: no token = the original open behavior (graceful, zero regression even if the gateway lacks
  `token resolve`); observer = read-only; responder/admin = act. Live isolated test (`:8499`): observer →
  denied (incident stays open), responder → resolved + attributed `alice:`, per-room admin override resolves.
  FOLLOW-UP (optional): `ROZUM_MEETING_REQUIRE_TOKEN=1` strict mode (no token = observer).
- [x] **mtg-retention** — DONE (`7bec5a1`). `store::prune_old_days` deletes `<date>.jsonl` older than N
  days + rewrites `index.json`, NEVER pruning a day holding an open incident's messages; wired into daemon
  start, gated `ROZUM_MEETINGS_RETAIN_DAYS` (default off). Test.
- [x] **mtg-event-sourced-threads** — DONE (`d7095d8`+`5e3e0a4`+`c367167`). `store::rebuild_threads` +
  `repair_threads` + CLI `meetings repair-threads` reconstruct incidents from the message log; `thread_open`
  posts an `opened incident` audit message so even reply-less incidents recover. **EXACT (`c367167`):** the
  posted transitions (open/escalate/resolve/assign) carry a structured `MsgMeta.thread_op` (opened/title/kind/
  state/owner/severity/pin/unpin); `rebuild_threads` REPLAYS them exactly (prose parsing is the fallback for
  old logs). thread_op is skip-when-none → plain msgs byte-identical. **NOW EXACT FOR ALL TRANSITIONS
  (`ad2de1f`):** thread_set_state + thread_pin also post a structured `thread_op` audit message, so manual
  `incident state` and pin/unpin survive a rebuild too. Live-proven: wipe threads.json + .bak → repair
  recovered title/severity/state(=triaging)/owner AND pinned exactly. Fully complete.
- [x] **mtg-msg-link-react-edit** — DONE (LINK `5e3e0a4` + REDACT `4a5ba09`). LINK: `Thread.links` +
  `meeting.thread_link` + CLI `incident link|unlink` + REST + console 🔗; thread_context resolves links into
  a `linked` bundle. REDACT (edit/redact): `redactions.json` tombstone applied on READ in `read_day` — every
  surface shows `[redacted: reason]`, original bytes preserved, reversible, zero-cost when none;
  `meeting.redact` + CLI `meetings redact [--undo]` + REST + console ⊘. **REACT DONE (`07a030e`):**
  `reactions.json` (msg_id→emoji→[who]) + `store::set_reaction` + `meeting.react` + CLI `meetings react
  <id> <emoji> [--off]` + REST GET `/reactions`/POST `/react` + console emoji-count chips. mtg-message-ops
  is now FULLY complete (link + redact + react).

- [ ] **mtg-rich-rooms** — a richer room model beyond the daily-file chat: rooms with a **lifecycle**
  (a support queue / a per-product channel / a per-incident room), durable identity, membership/roles
  (reporter, assignee, on-call, observer), and a room **kind** (chat | queue | incident). Today: one
  flat daily room per project. Needs a room registry with typed metadata + a migration from the daily
  files. The hinge the rest hangs on.
- [ ] **mtg-message-metadata** — messages carry **structured metadata**: type (note | question | event |
  alert | resolution), severity, status, assignee, tags, links to artifacts/logs, and a stable message
  id. Today a message is handle+text+timestamp. Needs a versioned message schema (back-compat with the
  plain lines) + write/read paths that preserve it.
- [ ] **mtg-threads** — group related messages into a **thread = an incident/topic**: a thread id, a
  parent message, reply-chains, thread-level state (open/triaging/escalated/resolved/closed) + SLA/owner.
  This is what turns a stream into trackable incidents. Needs thread storage + a thread-aware reader/TUI.
- [~] **mtg-message-ops** — **working with messages**. **SEARCH DONE + LIVE-PROVEN (`c422764`):**
  `store::search_messages` (AND filter: text substring · kind · MIN severity · tag · thread · since;
  `Severity::rank` for `>=`) over a room's whole history, surfaced three ways — REST
  `GET /rooms/{n}/search?q=&kind=&severity=&tag=&thread=&since=&limit=` (bad kind/severity → 400), CLI
  `rozum meetings search [--kind --severity --tag --thread --since] <q>`, and the console filter box
  (now spans ALL history server-side, was today-only). Also fixed `resolve_room_root` (read/inbox/search
  `--room <name>` for SHARED rooms resolved to the wrong dir → now consults the registry root).
  **REPLY-CHAINS DONE (`d738f8c`):** `in_reply_to` wired through — daemon `meeting.submit` param, CLI
  `meetings post --reply-to <id>`, console reply affordance (↩ button + cancellable hint + `↩ <id>`
  indicator on replies). **ASSIGN DONE (`0f9007b`):** `meeting.thread_assign` (owner only, no state change —
  orthogonal to the lifecycle), CLI `meetings incident assign <id> --to <h>`, REST `/threads/{id}/assign`,
  console 'Assign' button. **PIN DONE (`21f0b1a`):** `Thread.pinned` + `store::set_pinned`;
  `meeting.thread_pin`, CLI `incident pin|unpin` + `show` pinned-first, REST `/threads/{id}/pin`, console 📌
  toggle. **SLA/STALENESS DONE (`57caa56`):** per-severity windows + `needs_attention` metric + ⚠ on stale
  cards/list; `open_thread` inherits the anchor alert's severity. **REMAINING:** link/reference
  (retroactively attach a message to a thread — append-only store, so a reference record, not a mutation),
  react, edit/redact. Resolve/close already shipped. Search scans day files; index only if rooms get large.
- [ ] **mtg-escalation** — **escalation**: route/escalate an incident by severity/tier/on-call (to a
  specific agent, a stronger model, or a human), with an escalation policy + an audit trail of who/when.
  Ties into the model-chain (escalate to a stronger model) + identity-roster (who's on-call). The
  "P" of PagerDuty.
- [ ] **mtg-resolving** — **resolving**: an incident state machine (open → triaging → escalated →
  resolved → closed), resolution records, reopen, and metrics (time-to-resolve, escalation rate). Turns
  threads into accountable units of work.
- [ ] **mtg-incident-context** — **per-incident context collection**: a thread auto-accretes the relevant
  context — attach logs/gateway.jsonl slices, link related messages/threads, capture the workdir/repro,
  snapshot the model/agent state — so an agent (or human) picking up an incident has the full picture in
  one place. The "gather everything about this incident" primitive; the highest agent-leverage piece
  (an agent can assemble the context bundle automatically). Builds on obs + the meeting store.

- [x] **mtg-frontend** (operator 2026-06-28 — separate task) — DONE across BOTH frontends. **The console
  (`rest_read.rs`) is the full support UI** (v1 dashboard + v2 interactive + filters/search/staleness/pin/
  link/redact). **The `.ssc` PWA convergence is now COMPLETE (`003994c`):** the production mobile web both
  shows incident awareness (severity/kind badges + 🛟 count) AND manages incidents — a `/incidents/<room>`
  page reads `threads.json` and renders severity-coloured cards with inline lifecycle actions (triage /
  escalate-with-to / resolve / reopen) that POST to `/do` → `exec rozum meetings incident …`. Live-proven
  (prod PWA temp-unloaded + restored): escalate→@dba + resolve applied via the PWA and persisted. So the
  production launchd web and the console converge on ONE incident model. Detail of the console below ↓
  **V1 DONE +
  LIVE-PROVEN (`7f79ce5`):** a self-contained incident dashboard served by the daemon's read-only REST
  server (`rest_read.rs`), reading the SAME disk rooms the production single-writer daemon backs — so every
  StoredTurn's metadata + `threads.json` surface with no new plumbing. New endpoints `GET /rooms`,
  `/rooms/{name}/threads` (+ metrics), `/threads/{id}` (whole-incident context bundle), `/metrics`, and
  `GET /` serves `console.html` — a dependency-free SPA: header metrics (incidents / open / escalated /
  resolved / MTTR), a left rail of incident lanes grouped by state + severity-coloured, a live today-feed
  with kind/severity/thread badges, click-through incident drill-down (thread record + all messages +
  participants + timespan). Dark-mode aware, polls 4s, behind the existing Basic-auth secret. Live smoke
  test: daemon spawned the REST server, `meetings post --kind/--severity/--tag` wrote metadata to disk, the
  console + all endpoints returned it (auth 401 without creds). 88/88 meeting lib tests green.
  **V2 DONE + LIVE-PROVEN (`6281d2e`):** the console is now INTERACTIVE — escalate / triage / resolve /
  reopen an incident, open an incident on any live-feed message, compose posts with kind + severity
  (auto-attached to the open incident). The REST server (in-process with the daemon) reaches the
  single-writer path by connecting to the daemon's own socket as an MCP client (reusing
  `tui_client::call_once`, the incident-CLI route), so writes go through identity + single-writer unchanged.
  New POST endpoints `/threads` (open), `/threads/{id}/escalate|resolve|state`, `/messages` (submit); the
  Basic-auth username = the console actor. Live test over HTTP: open→escalate→post→resolve wrote the whole
  lifecycle (4-msg context bundle), the dashboard reflected resolved/owner.
  **REMAINING (v3):** filter/search by metadata; reply-chain rendering; fold into the `.ssc`→Rust PWA
  (`project-rozum-meeting-ssc-pwa`) so the production launchd web + this console converge on one model.
  Pairs with **mtg-incident-cli** (DONE — human shell verbs drive the same lifecycle).

- [x] **mtg-incident-cli** — DONE + LIVE-PROVEN (`976bd83`). `rozum meetings incident open|escalate|resolve|
  state|list|show|metrics` — the human/script twin of the agent-native MCP thread verbs. Each drives the
  daemon over its socket (new `tui_client::call_once` + `MeetingClient::call`, mirroring `post_once`), calling
  the same `meeting.*` thread tools the agents use. A human now runs the WHOLE lifecycle from the shell
  (open on a message id → escalate to an on-call handle → resolve) and inspects it (list / show context
  bundle / metrics), no agent + no UI. Makes the `mtg-frontend` console populate in real use. Live test
  (isolated daemon): open→escalate→resolve wrote `threads.json`; REST + console reflected the resolved
  incident, owner, and the 3-message context bundle (alert→event→resolution).

- [x] **mtg-registry-dup-name** — DONE (`054e670`). Room names aren't unique (two project dirs can derive
  the same basename) and `rooms.json` is a global registry, so a stale registration (a deleted/moved project
  that once held a same-named room) could shadow the live room when a surface resolved by name (the REST
  console read the wrong/empty room). Two-sided fix: `register_room` prunes any other same-name entry whose
  root no longer exists on register; `rest_read::room_root` prefers a same-name match whose root still exists
  (falling back to the most-recent). Test `registry_prunes_stale_same_name_dupes`. NOTE for tooling: tests/
  demos that spawn a daemon should override `XDG_STATE_HOME` to avoid polluting the operator's global registry.

## Model chain (verification-gated, `--model A,B,C`)

The CORE chain shipped on master (spec `docs/specs/pipeline-cascade.md`, SPRINT top item): target
derivation (single + multi-model), deterministic verify-gate + repair, escalation across links, role-aware
quality stats + auto-exclude, cloud-last by ordering, backend planner/executor/verifier roles. These are
the deferred follow-ups (operator-triaged 2026-06-24, none urgent):

- [x] **chain-cache-when-fits** — DONE (`2fcc051`). The gateway's `/control/switch` (the chain's
  escalation path) is now **cache-when-fits** instead of always destroy+rebuild: PROMOTE a warm target
  with no rebuild (Arc swap; live ~22ms vs a full reload) + KEEP the old primary warm when the residency
  planner says both fit, so a switch-back / re-run / the matrix reuses the resident copy. Falls back to
  the destructive single-resident swap (clearing warm first) when the pair can't co-reside, or the swap
  isn't cacheable (multislot off / custom backend / different n_ctx). The chain inherits it transparently
  — no chain code change. Gated by the SAME memory planner the warm cache already used
  (`plan_residency`: host_ram_budget − committed_by_others, shared reserve once → reboot-safe), unblocked
  by the co-residency refutation (`d63c9e4`). The destructive path is byte-preserved as `swap_destructive`
  (used verbatim when multislot off → single-resident behavior unchanged). 4 new unit tests + 85/85
  gateway green + live smoke (0.6B↔4B real MLX: cached → promote(22ms) → promote(22ms), no reboot).
  **SAFETY held:** the planner stays the sole "does the 2nd fit" authority; an oversubscribed pair (big
  models) drops the old model (destructive) rather than co-residing → no overcommit. Idle warm residents
  are swept after `ROZUM_GATEWAY_UNLOAD_IDLE_SECS` (no permanent RAM hold). Off-switch: `ROZUM_MULTISLOT=0`.

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

- [x] **residency-gate-v2-ramledger** — DONE (`feature/gateway-residency-ram-ledger`, `sunny-civet`).
  Replaced the BUG-003 v1 binary single-flight with a **RAM ledger**: each gateway reserves its
  estimated footprint (`residents/<pid>` flock-held file) before loading; admit iff sole OR
  `in_use + footprint ≤ total_ram × ROZUM_GATEWAY_RAM_BUDGET_FRAC` (0.65). The v1 "racy / needs
  PID-reap" objections are answered: reservation is up-front **under a brief admit lock** (no
  free-RAM-read TOCTOU), and liveness uses **per-pid `flock` probe** (same death-safety as v1, no
  kill-reaper). Footprint estimated caller-side from the catalog (core stays model-free). Spec § v2;
  4 unit tests + real-binary smoke; core 91/91, default+no-default green.
- [ ] **residency-gate-cap-mlx-sibling-aware** (v3 hardening, NOT urgent) — the ledger refuses at
  *admission*; the per-process MLX cap (`crates/rozum-mlx/.../mlx_native_backend.rs:~363`) is still
  flat `total−8`. Make it sibling-aware (`total−8−committed_by_others` from the ledger) so even an
  escape-hatch / unknown-path 2nd MLX process can't claim near-total RAM. Secondary to the ledger.

## MCP (deferred — decide the use, then build)

- [ ] **mcp-use** — the MCP-client `ToolSource` (`McpToolSource`, `src/mcp_tool_source.rs`)
  is built + tested (5/0, in-memory duplex). Deferred per the user: decide the *use* before
  building more. Three shapes considered: **(A)** an embedded agent loop that consumes MCP
  tools; **(B)** a gateway **MCP-federation** — rozum federates N MCP servers + its meeting
  tools into one tool surface for external agents (claude/codex) — *my recommendation, most
  "rozum-shaped"*; **(C)** gateway tool-augmentation (inject MCP tools into external-agent
  requests). Pick the use, then spec + build. Spec so far: `docs/specs/mcp-toolsource.md`.

## Agentic-bench fix candidates (from matrix-failure-analysis)

- [ ] codex-reliability — **Candidate fixes for the codex matrix reds (most of codex's 10/20).** Root
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

## Optional Model Adapters

Model adapters are optional. They must not be required for the default build,
default CLI startup, meeting rooms, round-robin moderation, or manual moderation.

- [x] candle-backend - Implement a real Candle adapter behind `InferenceBackend`.
  - Prefer pure Rust and keep heavyweight features gated.
  - Compare output and latency against `llama-gguf`.

- [x] native-gguf-backend - **SUPERSEDED/DONE.** The in-process GGUF backend (`gguf` feature, llama-cpp-2) shipped.

- [x] llama-gguf-library-backend - **SUPERSEDED.** Covered by the in-process GGUF backend.

- [x] external-command-backend - **Superseded/WON'T DO.** The OpenAI-HTTP client backend covers the
  Ollama / LM Studio HTTP use case; no separate external-command engine needed.

- [x] mlx-native-backend - **DONE (shipped long ago — this was the planning stub).** Native MLX
  inference via `mlx-rs` is the **primary in-process engine** now (`src/mlx_native_backend.rs`,
  feature `mlx-native`, default): Qwen3 / Qwen3-MoE / Qwen3.6 hybrid / Llama / Mistral / Phi-3 /
  Gemma3 / Qwen2, continuous batched decode, prefix-KV reuse, constrained decoding. The original
  ">10% over llama-cpp-2" bar was cleared and then some. Specs under `docs/specs/mlx-native-*`.

- [x] candle-real-streaming - **WON'T DO (2026-06-15).** The Candle backend is no longer developed
  (native MLX is the primary in-process engine; GGUF the fallback; remotes via HTTP). Not worth the
  streaming work.
  - Low priority: Candle-Metal is slower than llama-cpp-2 on the target models.

## GLM model landscape (sizing + port path)

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

- [x] mlx-native-batched-decode — true parallel serving (multiple concurrent sessions).
  **DONE + e2e-validated 2026-06-14 — dense Qwen3 / Qwen3-MoE AND hybrid Qwen3.6 (both arches).**
  - **Worker scheduler SHIPPED (`mlx_batched_scheduler_two_concurrent`):** with `ROZUM_BATCH=2`
    two concurrent greedy requests on one backend batch into **one** `run_batch` call (asserted via
    `BATCH_RUN_COUNT`) and each row gets its OWN correct answer — `France="Paris." Japan="Tokyo"`,
    no cross-row contamination. `worker_main` drains up to `ROZUM_BATCH` (default 1 = serial) ready
    jobs within a `ROZUM_BATCH_WINDOW_MS` (default 10) window, partitions greedy (argmax) vs the
    rest, batches the greedy ≥2 via `run_batch`, runs the others (and any single job) serially on the
    proven prefix-KV path. `concurrency_capacity()=Some(batch_cap())` so `admit_wrap` admits B.
    (**ALL dense families now batch** since 2026-06-15 — Llama 3.x / Mistral / Phi-3 / SmolLM
    (`llama.rs`), Qwen2 / Qwen2.5 / Qwen2.5-Coder (`qwen2.rs`), AND Gemma 3 (`gemma3.rs`) got the
    same per-row-RoPE port; `dense_forward`+`is_batchable_arch` include all three. Gemma 3 also
    needed its per-layer LOCAL windowed mask threaded into the batched path (`build_window_keep`
    AND-ed with the pad mask — at decode all rows are right-aligned in the left-padded cache so the
    window is uniform across slots). Validated `mlx_llama_batched_two_concurrent` +
    `mlx_qwen2_batched_two_concurrent` + `mlx_gemma3_batched_two_concurrent`. **No dense family stays
    serial.**) Per-row streaming + EOS/max-tokens/runaway
    retirement via `BatchSeq` (`take_axis` row-slice shrink + re-assembled per-row pad mask & rope).
  - Probe: B=2 batched `forward` is byte-exact per sequence + **2 seqs at 126.3 vs 63.9 t/s =
    1.98×** (near-linear) — because decode is 92% CPU graph-build and batching does ONE build for
    B sequences, **amortizing the exact build cost `mlx-native-perf-compile` couldn't reduce** (the
    two perf threads converge here: batching IS what compile aimed for, and it works).
  - **Ragged dense forward validated (`mlx_batched_ragged_byte_exact`):** two
    different-length sequences, prefilled separately then assembled into one batched cache, decode
    together with per-row RoPE + a per-row left-pad mask. Row A (len 7) **byte-exact** vs serial;
    row B (len 4) byte-exact 8 tokens then a **1-bf16-ulp near-tie flip** (a valid greedy choice,
    same class as MoE float-reduction nondeterminism) — i.e. **correct to bf16 precision**. Fork
    (rev `65a33bab`): `RopeVariant::forward_dynamic`, `qwen3::set_batch_pad_offsets` (thread-local;
    Attention ropes at `cache.offset()−pad_i` per row when set; **OFF by default → B=1 path
    byte-identical, no regression**), `ConcatKeyValueCache::{kv_used, from_kv}` (assemble a batched
    cache from per-sequence KV — avoids pad-token/negative-rope artifacts).
  - **Hybrid Qwen3.6 batching SHIPPED 2026-06-14 (`run_batch_hybrid`).** The feared blocker — "the
    GatedDeltaNet recurrence can't be left-padded" — only bites if you prefill a PADDED batch; we
    prefill each sequence separately (as the dense path already does), so no pad token ever advances
    the recurrence. The GDN turned out to be **already batch-generic and row-independent** (kernel
    grid z spans `b*hv`, `b_idx=n/Hv`; conv+recurrent state is `[B,…]`) — proven byte-exact
    (`gated_delta_batches_row_independent`, synthetic, no model). So hybrid batched decode = the
    dense ragged path for the full-attention layers (left-pad+stack KV, per-row rope + key-pad mask,
    ported to `qwen3_5::Attention` via `set_batch_pad_offsets`/`set_batch_pad_mask`) **+ just STACK
    the fixed-size conv + recurrent state on the batch axis for the GatedDeltaNet layers** (no
    padding/rope/mask — fixed size regardless of length). `run_batch_hybrid` assembles the
    heterogeneous `qwen3_5::LayerCache` (`Full`→KV stack, `Linear`→state stack), shared by both the
    dense-hybrid `Qwen35` and MoE-hybrid `Qwen35Moe` (same Model API). Validated on the real
    Qwen3.6-27B: **byte-exact** per row vs serial (`mlx_hybrid_batched_ragged_byte_exact` — both
    rows exact, incl. the padded one), e2e two concurrent sessions batch into one call (`"Paris"` /
    `"Red"`, distinct — `mlx_hybrid_batched_scheduler_two_concurrent`), and **2.30× throughput** at
    B=2 (`mlx_hybrid_batched_decode_throughput`, test profile — higher than dense's 1.98× because
    hybrid decode has more per-token op launches to amortize). Fork rev `9a3b3949`.
  - **Continuous batching SHIPPED 2026-06-14** (both dense + hybrid). `run_batch`/`run_batch_hybrid`
    now take the job receiver and, while decoding, ADMIT queued greedy jobs into freed/spare slots
    (up to `cap`) instead of waiting for the whole batch to drain — so a finished short row's slot is
    refilled mid-decode rather than idling. The decode loop tracks the KV `width` + per-row pad
    explicitly (invariant `pad_i = width − len_i`, both grow by 1/step); admitting a row prefills it
    (B=1), grows the width + left-pads existing rows if the new prompt is longer, then stacks it on
    the batch axis (dense KV / heterogeneous `LayerCache` Full+Linear). Byte-exact by the same
    argument as the initial ragged assembly (front-pad masked, rope offset invariant). Non-greedy
    jobs pulled from the queue are returned to the worker to run serially; a lone greedy job still
    goes serial (keeps the prefix-KV LRU). Validated: `mlx_continuous_admit_three` — 3 concurrent
    requests, `ROZUM_BATCH=2`, the 3rd admitted into a freed slot mid-decode (one `run_batch` call,
    `BATCH_ADMIT_COUNT`+1), each correct + distinct (`Paris`/`Tokyo`/`Berlin`); all dense + hybrid
    byte-exact and scheduler tests still green.
  - **Batched SAMPLING SHIPPED 2026-06-14** — batching is no longer greedy-only. Fork
    `qwen3::sample_rows(logits[B,vocab], temp[B], top_k[B], top_p[B])` samples one token per row,
    each honoring its OWN temperature/top-k/top-p (a unified always-nucleus path; `top_k<=0`/`top_p>=1`
    keep all; `temp==0` → per-row argmax override), so one batch can MIX greedy + sampling requests.
    The batch gate relaxed from `is_greedy` to `is_batchable` (only repetition-penalty / explicit-seed
    rows stay serial — they need per-row history scatter / RNG keys). `run_batch`/`run_batch_hybrid`
    build per-row `[B]` param arrays from each row's `SamplingParams` and call `sample_rows` in place
    of argmax (decode step + admit + initial). Validated: fork `sample_rows_per_row_collapses_to_argmax`
    (mixed per-row configs each collapse to their own argmax, deterministic) + the greedy e2e tests now
    route through `sample_rows@temp0` and stay byte-exact (`Paris`/`Tokyo`/`Berlin`) +
    `mlx_batched_sampling_two_concurrent` (two `temp=0.7` requests batch — `run_batch calls=1` — and
    stream coherent output `Red`/`Dog`). Repetition-penalty + per-seed batching are the remaining
    follow-up (rare for coding agents; serial path covers them).

  **RAGGED is tractable — confirmed (`mlx_rope_per_row_probe`):** `mlx_rs::fast::rope_dynamic`
  accepts a **per-row `[B]` offset array** and ropes each row at its own position (byte-exact vs
  per-row scalar rope, diff 0.00e0). So a batch of different-length sequences can be rope'd
  correctly in one call — no per-row rope loop, no per-row cache.

  **Full de-risked design (dense):**
  - **Left-pad** the B prompts to `maxL` (`pad_i = maxL − len_i` per row); one shared
    `ConcatKeyValueCache` holds `[B,H,maxL+steps,D]`, all rows append at the shared offset.
  - **RoPE per-row offset = `cache.offset() − pad_i`** (a `[B]` array) via `rope_dynamic`. During
    prefill (`offset=0`) row i token t → position `t − pad_i` (real tokens `t≥pad_i` get `[0,len_i)`);
    during decode (`offset=maxL+s`) the new token → position `len_i + s`. Byte-exact vs serial.
  - **Mask** (additive, via the existing `AttentionInput.mask`): row i masks key slots `[0, pad_i)`
    (the left pad); prefill also causal. Built in rozum with Array ops.
  - **Fork change:** thread an optional `pad_offsets: Option<&Array>` through `ModelInput →
    AttentionInput → Attention::forward`; when `Some`, use `rope_dynamic(q/k, cache.offset() −
    pad_offsets)` instead of the scalar `rope(cache.offset())`. Existing (B=1, `None`) path
    unchanged. Then a rozum batched-decode path: serial-prefill or batched-left-pad-prefill,
    assemble offsets+mask, batched decode loop, per-row argmax, per-sequence detok/stream, retire
    a row on EOS/max-tokens (shrink the batch), admit queued jobs (continuous batching).
  - **Worker:** drain up to B ready jobs each cycle → batch; 1 job → existing serial path (keeps
    the prefix-KV LRU). Raise `concurrency_capacity()` to a memory-budgeted `B`.

  **Hybrid (Qwen3.6) — SHIPPED 2026-06-14 (see `run_batch_hybrid` above), turned out NOT harder.**
  The premise "the GatedDeltaNet recurrence can't be left-padded (padding pollutes the running
  state)" is true but irrelevant: we prefill each sequence SEPARATELY (no padding through the
  recurrence) and the GDN state is fixed-size per row, so it just stacks on the batch axis. The
  conv+recurrent state was already `[B,…]` and the kernel grid already spans batch — byte-exact per
  row with zero kernel changes (`gated_delta_batches_row_independent`). The only real work was
  porting the dense per-row rope/mask to `qwen3_5::Attention` + assembling the heterogeneous cache.
  TODAY: the native MLX backend is capacity-1 — one OS worker thread owns the `!Send` model
  and runs jobs strictly serially (`worker_main`'s `while blocking_recv { run_job }`);
  `concurrency_capacity()=Some(1)`, so `admit_wrap` admits 1 and queues the rest (bounded
  `ROZUM_ADMIT_QUEUE_MAX`=32, shortest-job-first + fast lane, HTTP 429 on overflow). That's
  fine for ONE active CC/Codex session; many simultaneous sessions serialize (queued, not
  parallel). To actually serve N in parallel, add **continuous/batched decode** to the
  native runtime: batch B sequences in one `forward` (MLX has the batch dim), a per-sequence
  KV cache stacked on the batch axis (extend `ConcatKeyValueCache` / the GatedDeltaNet conv
  + recurrent state to a batch axis), ragged prefill admission, and per-sequence
  EOS/stop/cancel + streaming. Then raise `concurrency_capacity()` to a memory-budgeted
  `budgeted_max_num_seqs` (the budget machinery already exists; mistralrs uses it). Big:
  touches `Generate`, every model's `forward`, all KV/conv/recurrent caches, and the
  admission wiring. Throughput win scales with B until memory/Metal-bandwidth bound;
  single-stream latency unchanged. Only pull when concurrent multi-session serving is a real
  requirement (today's queue+SJF+429 is a reasonable single-GPU answer). Hybrid (Qwen3.6)
  is the hard part — the gated_delta kernel + conv cache must batch correctly (byte-exact
  per sequence vs the B=1 path).

- [x] mlx-native-chunked-prefill - DONE. `Model::prefill` chunks the prompt
  (`ROZUM_MLX_PREFILL_CHUNK`, default 2048), bounding the full-attention
  `[chunk, ctx]` causal-mask + SDPA peak instead of `[T, T]`; caches advance and
  are eval'd between chunks to free activations. `lm_head` runs only on the final
  position (`Model::project`), dropping the per-chunk `[1,chunk,vocab]` ~600MB
  logits transient too. Byte-identical to single pass
  (test `mlx_qwen35_chunked_prefill_matches_single_pass`, Δ=0). See SPRINT.

- [x] mlx-native-mem-bound - DONE (preflight). `run_job` estimates the request's KV
  footprint (`kv_bytes_per_position * (prompt_len + max_tokens)`, full-attention
  layers only — GatedDeltaNet state is O(1)) and rejects with a clear "context too
  large … lower --n-ctx / max_tokens … fits ~N tokens" `ModelError` when it exceeds
  75% of `available_ram_bytes()` (vm_stat), instead of letting Metal OOM. Unit test
  `kv_bytes_per_position_estimate`. FOLLOW-UP: a bounded/rotating KV cache to cap
  resident KV for very long sessions (only if the preflight isn't enough). See SPRINT.

- [x] mlx-native-decode-bug - RESOLVED. The custom-kernel "needs a blocking eval
  per call" rule is a buffer-donation hazard: the kernel's lazy `state_out` gets
  donated/reused by the ~60 later layers before it materializes, corrupting the
  recurrent state (decode diverges at token 2). The per-call eval forces it
  concrete and fixes it. A/B benched: the eval is FREE (decode is op-launch-bound,
  not sync-bound — 12 vs 12 t/s with/without). NOT a path to faster decode, and the
  obvious fusion lever (`mlx-native-compile`) turned out a measured dead end — see
  below; decode is FFI/per-op-overhead bound. See SPRINT `mlx-native-perf`.

- [x] mlx-native-compile - `compile_with_state` is net-NEGATIVE (measured), but this
  only rules out ONE of mlx-rs's two compile APIs. Probe `mlx_compile_probe` (dense
  Qwen3-4B): T=1 0.51x (8.79->17.34ms), T=16 0.85x — because `compile_with_state`
  re-marshals + sorts all ~400 params per call. **Plain `compile` (`compile.rs:344`)
  marshals only the args and captures referenced weights into the trace** — the way
  Python `mlx_lm` reaches ~22 t/s vs our ~12 — and was never probed. See
  `mlx-native-perf-compile` below; the fixed-shape-cache prereq is NOT moot.

- [x] mlx-native-perf-pipeline - **DONE (merged).** Decode-speed root cause settled:
  it was PIPELINING, not compile/cache. `stream_generation` now `async_eval`s step n+1
  before blocking on step n (dense arches: Qwen3/Qwen3-MoE/Llama/Qwen2; hybrid stays
  serial). Qwen3-4B **114→128 t/s = 96.5% of Python**; byte-exact all arches. Compile
  probes (`mlx_compile_probe_plain`) showed plain `compile` is 0.69× — not the lever;
  the fixed-cache + compiled-decode redesign is shelved. Spec: mlx-native-runtime.md
  "Performance — decode parity".

- [x] mlx-native-perf-hybrid-mlxbump - **DONE + SUPERSEDED — all of it shipped, and the real
  win was 3× bigger than this item imagined.** The plan here (bump to MLX 0.31.2, drop the
  per-layer GatedDeltaNet eval, pipeline the hybrid) is **entirely landed**: mlx-c builds against
  `GIT_TAG v0.31.2` with the env-gated retained-command-buffer `PATCH_COMMAND` (mlx-c `85ee313`),
  `gated_delta.rs` skips the per-call eval when `ROZUM_MLX_RETAIN` is set, and the hybrid decode
  paths (`Qwen35`/`Qwen35Moe` in `src/mlx_native_backend.rs`) already pass `pipeline=true`. That
  combo alone was ~12 → 16-17 t/s. **Then the actual bottleneck turned out to be something this
  item never saw:** a bf16→f32 stream leak in `qwen3_5.rs`'s q/k delta-scaling (a strong f32
  0-dim multiplier promoted the whole stream to f32 → ~1000 spurious `AsType` casts/token feeding
  every QuantizedMatmul/RMSNorm). Fixed by casting the scale to q/k's dtype
  (`Array::from_f32(s).as_dtype(qn.dtype())`, lines 426/428) + a null-weight `rms_norm_no_weight`.
  **Result: MoE hybrid decode 33 → ~88 t/s (2.7×), dense 27B 16 → ~19.6 t/s** — ~90% of Python
  (97-110 MoE / 23 dense). Full diagnostic + numbers: `docs/mlx-gd-bug/LOG.md`. Single-stream
  hybrid decode is now effectively maxed: every per-token-cost lever (mlxbump, retain, bf16-leak,
  null-weight norm, pipelining, hand-fused kernels, mx.compile) is pulled or proven dead. **The
  ONE lever left is batching** — the probe (`mlx-hand-fused-gdn-kernels`) showed 92% of per-token
  time is CPU graph-build/FFI (`build=15.65 eval=1.31 ms/tok`), and batching amortizes exactly
  that across B sequences (dense already got 1.98×, `mlx-native-batched-decode`). So the remaining
  hybrid-decode speedup lives in **hybrid batched decode** (the hard counterpart — GatedDeltaNet
  recurrence can't be left-padded; needs per-row conv+recurrent state on the batch axis).

- [x] mlx-native-perf-compile - **CLOSED 2026-06-14: confirmed dead AND superseded by
  mlx-native-batched-decode.** The premise was that `mx.compile` (trace once + reuse) could
  recover the ~2× left on the table by the 92% CPU build/FFI cost. Two findings retire it:
  (1) **compile is net-negative in mlx-rs** — `mlx_compile_probe` re-probed plain `compile` on
  the dense Qwen3-4B forward (7× bigger build than the original 0.6B probe) at fixed shapes and
  it's STILL 0.64× (slower), so the lever doesn't exist on this stack regardless of the
  fixed-shape-cache prereq. (2) **batched decode already captures the win compile aimed for** —
  the cost compile targeted is the per-token graph build, and batching does ONE build for B
  sequences, so B=2 gets 1.98× on exactly that axis (`mlx_batched_decode_probe`). The two perf
  threads converged: batching IS the amortization, shipped and validated. Custom hand-fused
  kernels (the other ~no-gain lever) stay deferred per `mlx-native-perf` notes above.

### Native MLX runtime — catalog expansion (more architectures)

Each architecture port is now cheap: the AFQ-quant loader + the model-agnostic
sampler are shared (import from `qwen3.rs`), so a new dense model ≈ a copy of
`llama.rs`/`qwen2.rs` with the right attention/norm quirks + a `LoadedModel` arm
+ a byte-exact oracle sweep vs Python `mlx_lm`. (Quick near-free ones — Mistral
alias, Llama variants, fp16 — are in SPRINT.) Out-of-scope ones (DeepSeek/MLA,
vision) and why: `docs/specs/mlx-native-catalog-non-goals.md`.

- [x] mlx-native-gemma - **Gemma 3 (text) DONE 2026-06-14 — own fork file `gemma3.rs`.**
  Distinct from Llama: `(1 + weight)` RMSNorm convention (computed in f32), embedding scaled by
  `sqrt(hidden)`, per-head q/k RMSNorm, **GELU(tanh) MLP**, four norms per layer, **alternating
  local/global attention** (per-layer RoPE base — `rope_local_base_freq` local vs `rope_theta`
  global, every `sliding_window_pattern`-th layer global), scale `query_pre_attn_scalar^-0.5`. Own
  `Generate` (mirrors llama); `LoadedModel::Gemma3`; routes `"gemma3_text" | "gemma3"`. **VALIDATED**
  (`mlx_gemma3_chat`, gemma-3-1b-it-4bit): *"Paris is the capital of France."* (clean). Getting there
  surfaced THREE general fixes (not Gemma-only): (1) mlx-community 4bit ships a SEPARATE quantized
  `lm_head` even when tied → detect + use it; (2) chat templates that emit `{{ bos_token }}` (Gemma)
  got an empty BOS → thread `bos_token`/`eos_token` into the minijinja context (a BOS-sensitive model
  was garbage without it); (3) `<end_of_turn>` (106) wasn't in EOS (config eos is only `<eos>`) → add
  the tokenizer's turn-end token. Added to `models::RECOMMENDED`. **Sliding window SHIPPED
  2026-06-15:** local layers now additionally mask keys older than `sliding_window` (global layers
  stay full causal), via per-layer additive masks built over absolute positions (`build_gemma_masks`
  — correct at decode); a no-op when the context fits the window (short prompts unchanged). A
  deterministic unit test proves the banding + decode windowing; `mlx_gemma3_chat` still clean.
  **Deferred:** Gemma 2 (`attn_logit_softcapping`) and the multimodal vision tower are separate; the
  mask keeps the FULL KV (memory still O(context)) — a bounded windowed KV cache is a later memory
  optimization, not a correctness gap.

- [x] mlx-native-phi3 - **DONE 2026-06-14 — NO new model file.** Phi-3 (`model_type: "phi3"`) is
  the Llama arch with **fused `qkv_proj` + `gate_up_proj`**. Rather than a whole new file,
  `llama::load_phi3_model` SPLITS each fused tensor along the OUTPUT axis into the separate
  `q/k/v_proj` + `gate/up_proj` at load (the 4-bit AFQ packing is along the INPUT axis, so
  row-slicing weight/scales/biases is exact — no unpacking), then returns a `llama::Model` that
  runs on the existing Llama path (Generate, batched decode, sampling — all reused). Routed via
  `"phi3" => load_phi3_model → LoadedModel::Llama`; `supported_model_type` admits it; dense guard +
  `mistral_is_a_supported_model_type` updated. **VALIDATED end-to-end** (`mlx_phi3_chat`,
  Phi-3-mini-4k-instruct-4bit, first try): *"The capital of France is Paris."* Added to
  `models::RECOMMENDED`. (Phi-3-mini-4k = full RoPE; the 128k `su`/longrope variant needs
  `rope_scaling` threaded — a small follow-up. Phi-3.5-mini is the same arch → should work too.)

- [ ] mlx-native-mixtral - **LOW PRIORITY (2026-06-15): MoE need already covered; Mixtral largely
  superseded.** mlx-native already serves Qwen3-MoE and **Qwen3.6-35B-A3B** (a more modern + faster
  MoE — 3B active), so the sparse-MoE capability is there with better models. Mixtral 8x7B (~26 GB
  @4bit, borderline on 32 GB) was a late-2023 hit now mostly displaced by Qwen3.x / Llama3.x / Gemma3.
  A full new-arch port + real-weight parity for nichey value — skip unless a specific Mixtral need
  appears. Original note: Mixtral / Mistral-MoE (`model_type: "mixtral"`). Sparse MoE on the Mistral
  block — reuse the `qwen3_moe` SwitchGLU routing + Mistral attention. Validate vs oracle.

- [x] mlx-native-recommend-catalog - As architectures land, curate `models::RECOMMENDED`
  (the launch picker / `rozum models` list) with a few good defaults per family
  (coder, small, mid) so users get a sensible menu, not just whatever they type.
  **DONE 2026-06-15.** Tiers across the landed families: heavy (Qwen3.6 MoE/dense, Coder-32B),
  mid (Qwen2.5-Coder-7B, Gemma 3 4B, Mistral-7B), small/test (Qwen3-4B, SmolLM2-1.7B, Phi-3-mini,
  Gemma 3 1B). Every spec is loaded + answered before listing (caught the Gemma 4B wrapper-load
  failure → fixed in the same change). New entries validated via the e2e tests. While adding the
  Gemma 3 4B, discovered + fixed the multimodal-wrapper load path (4B/12B/27B), so the bigger —
  actually useful — Gemma sizes work now, not just the 1B test model.

### Native MLX runtime — domain fine-tuning (OFFLINE, exploratory)

All **offline** (train with `mlx_lm.lora`/`fuse`, serve the merged checkpoint — the
host stays inference-only). The full feasibility/memory/eval write-up is
`docs/specs/training-and-lora-exploration.md`. Reality check on size: QLoRA on
**0.5–4B is plenty for FORMAT / STYLE / narrow-domain PATTERNS** (the three items
below), but NOT for raw reasoning — that stays on a big/remote model. Step up to
7–14B (still QLoRA-able on a 32–64 GB Mac) only if a tune must also carry capability.
Every item is gated by a **held-out eval** (domain set + a general probe to catch
forgetting) — non-negotiable; without it you can't tell "improved" from "quietly
degraded".

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

- [x] meetings-rest-read — **DONE 2026-06-21.** Remote stateless read-by-day on the
  meeting daemon's own HTTP listener: `GET /rooms/{name}/days`, `GET
  /rooms/{name}/messages/YYYY-MM-DD?from=N&count=M`. Gated by `ROZUM_WEB_SECRET`,
  opt-in bind via `ROZUM_MEETINGS_REST_BIND`, and implemented in
  `crates/rozum-meeting/src/meeting/rest_read.rs`. The REST surface has since grown
  beyond read-by-day (inbox/roster, incidents, messages, reactions, SSE/search), but
  this original backlog item is closed.
- [x] meetings-model-as-participant — **DONE 2026-06-20.** `rozum meetings participant`
  joins a daemon room as a live model participant, calls the local gateway over HTTP,
  obeys `mention`/`always`/`manual` reply policy, supports persona text/files and peer
  loop guards, and submits replies through the daemon like any other client. Spec/results:
  `docs/specs/demo-conference.md`.
- [ ] meetings-bridges-on-daemon — daemon-backed human web is DONE as `rozum meetings web`
  (`src/meeting/web.rs`). Remaining bridge cleanup: port the legacy `src/web` escape hatch and
  the telegram/discord bridges (`src/telegram`, `src/discord`) off the legacy per-room socket
  onto `meeting.sock` (`rooms.join`), so the legacy in-process room can eventually be retired.

### Portability / hardware-agnostic core (keep the durable layer durable)

The hardware-agnostic abstraction already exists — the `ChatBackend` SPI and
everything above it (gateway, rooms, launch, orchestration, model infra). MLX is
one swappable leaf; GGUF/llama.cpp already carries non-Mac (Linux/Windows, CUDA/
ROCm/Vulkan/CPU). Full write-up: `docs/specs/portability-and-the-backend-spi.md`.
These items turn "portable in principle" into "portable by `cargo build`".

- [~] portability-platform-features - **Durable core DONE + CI-enforced 2026-06-15.** `cargo build
  --no-default-features` builds **and tests** the whole non-backend layer (SPI, gateway w/ HTTP
  backends, agent, cascade, concurrency, config, meeting room — 271 tests) with no native toolchain;
  a CI **`linux-core`** job (`ubuntu-latest`) runs exactly that on every push, so a Linux regression
  in the durable layer fails CI, not folklore. (Gated the one MLX-only test module on the feature so
  `--no-default-features` test-compiles.) **Remaining (needs a Linux box):** make *bare* `cargo
  build` first-class on Linux — the native backends are Apple-Metal-bound (mlx-sys; `llama-cpp-2 {
  features=["metal"] }`), so a target-conditional default (MLX only on macOS) + a gguf-CPU/CUDA path
  (non-`metal` llama-cpp-2) — entangled with the Metal feature flags, can't be validated from macOS.
  Tracked with `portability-cuda-gguf`.

- [~] model-sandbox - **Structural confinement for agentic model runs (core DONE; Linux native optional).**
  Both models run agentic loops that touch the filesystem + shell; confine them so they
  **cannot do anything harmful WITHOUT per-action approval prompts** — safety is an OS jail,
  not interactive confirmation. **A sandbox is a SET of `(path, mode)` rules** (rw / ro / deny,
  most-specific-wins, default-deny), NOT one directory — because builds need `~/.cargo` /
  `~/.rustup` / `target` / `$TMPDIR`. v1 profile `rust-coding`: workspace(s) rw, toolchain
  caches rw, system + (optional) repo ro, network loopback-to-gateway only, deny the rest.
  Agent runs `approval=never` (safe in the jail; also kills the Codex rejected-escalation
  stall, `matrix-failure-analysis.md` Finding 1a). Full write-up:
  `docs/specs/model-sandbox.md`. Sub-tasks:
  - [x] model-sandbox-seatbelt - **P1 (M4 primary). DONE 2026-06-19/20** (branch
    `feature/model-sandbox-seatbelt`). `src/sandbox.rs`: `SandboxPolicy` + `rust_coding`
    profile + `to_seatbelt_profile()` (validated-on-M4 SBPL) + `write_seatbelt_profile_temp`.
    `exec_agent` wraps the agent child in `sandbox-exec -f <profile>` when **`ROZUM_SANDBOX`**
    is set (`=1`→cwd, `=<dir>`→that dir); all later env/arg wiring appends to the jailed
    invocation. **Validated on M4:** writes confined / outside-write denied / secret-read
    denied / system-read OK / exec + bare-name PATH resolution OK / generated profile
    parses+runs under `sandbox-exec`; feature-free build green. **REMAINING:** (a) real-agent
    end-to-end **DONE 2026-06-19** — committed integration test `cargo_build_runs_in_jail_and_
    escape_denied` builds a real crate under the rozum-generated profile via `sandbox-exec`
    (cargo build + run the binary succeed in-jail → toolchain paths correct) and proves a
    `$HOME` write is denied; secret-read (`~/.ssh`) denied too. **All launch paths jailed**
    (exec_agent + exec_agent_anthropic via shared `sandboxed_command`). **Agent-state dirs now
    writable** (`~/.claude`/`~/.codex`/opencode under `~/.config`+`~/.local`+`~/.cache`) — a
    launched agent persists its session/history instead of crashing mid-task; live-verified
    (~/.claude write OK, `$HOME`-root write denied). **ON BY DEFAULT (macOS, 2026-06-19):**
    every `rozum launch` jails the agent to its cwd with no env; `ROZUM_SANDBOX=0` disables,
    `=1`/`=<dir>` override; off-macOS stays OFF (no Seatbelt) so launch isn't broken. Secrets
    now denied for **read AND write** (last-match-wins), safe even when cwd encompasses `$HOME`.
    **Live-validated end-to-end:** trial matrix cell claude × gpt-oss-20b × build ran jailed and
    PASSED (`olleh`, 46s); default-on/opt-out probes confirmed. **(b) `--no-sandbox` clap flag
    DONE 2026-06-20** (branch `feature/launch-no-sandbox-flag`): sugar over `ROZUM_SANDBOX=0`
    on `rozum launch` (env-set is the single decision point in `sandbox_workspace()`); hoisted by
    `reorder_launch_args` so it works after the program name, left for the child after `--`; help
    text + 2 reorder unit tests + CLI probes green. **(c) `rozum.toml [sandbox]` DONE 2026-06-20**
    (branch `feature/sandbox-config-table`): a `[sandbox]` table — `workspace` (extra rw, "."/"~/…"),
    `read_only` (Docker `:ro` mounts), `secret_deny` (extra denies), `network`, `backend` — parsed into
    `RuntimeConfig.sandbox` (`SandboxConfig`) and merged in `sandboxed_command` via `rust_coding_with`.
    Env overrides config (`ROZUM_SANDBOX_NETWORK`/`_BACKEND`/`=0` win). Resource limits stay env-only.
    Tests: config parse + `read_only`-→-`:ro` + `rust_coding_with`; live smoke (config→Docker+ro+secret;
    env beats config). **The model-sandbox-seatbelt item (a/b/c) is now fully complete.**
  - [ ] model-sandbox-linux-native - **P2 — a non-container Linux jail (Landlock / bubblewrap).**
    The Docker backend already jails on any OS, but it's heavy (a whole container + image). On Linux a
    *native* jail is lighter and the natural default there, mirroring what Seatbelt is on macOS. Same
    durable seam: render the existing `rust_coding` `(path,mode)` `SandboxPolicy` to a Linux mechanism,
    selected by `SandboxBackend` (add a `Landlock`/`Bwrap` variant + `from_env` mapping;
    `sandbox_workspace`'s macOS-only guard already lets non-Seatbelt backends run off macOS). Two
    candidate mechanisms, likely both (try Landlock, fall back to bubblewrap):
    • **Landlock** (kernel ≥ 5.13, best ≥ 6.x) — in-process LSM ruleset: a `path_beneath` rule per
      writable/ro path; the launcher applies it then `execve`s the agent (no helper binary). Closest
      analog to Seatbelt; degrades on old kernels (feature-probe the ABI, fall back).
    • **bubblewrap** (`bwrap`) — `--bind <p> <p>` (rw) / `--ro-bind` (ro) / `--dev`/`--proc`/`--tmpfs`
      for the rest; unshare net for `none`, slirp/none for `gateway-only`. A helper-process jail like
      `sandbox-exec`, so it slots into `sandboxed_command` the same way (wrap the program).
    Carry over the macOS lessons: allow-all-read **minus** the secret denylist (an allow-list breaks
    the loader), keep the agent-state + toolchain paths writable, map `NetPolicy` (none/gateway-only/
    full; strict-egress via the same iptables idea or a netns). Pin toolchain-path discovery
    (`CARGO_HOME`/`RUSTUP_HOME`/`$TMPDIR`/git) robustly across distros while here. *Done when:* on a
    Linux host `rozum launch` jails the agent natively (writes confined, secrets denied, `cargo build`
    succeeds in-jail, an out-of-workspace write denied) — the Seatbelt P1 gate, Linux edition. Not
    needed on the current M4 target; unblock when a Linux host is in play.
  - [x] model-sandbox-container - **P3. Docker backend DONE 2026-06-20** (branch
    `feature/sandbox-docker-backend`). `ROZUM_SANDBOX_BACKEND=docker` (alias `container`) renders the
    same `rust-coding` `(path,mode)` set to a `docker run`: writable→`-v :rw` binds (host path==
    container path), the rest of the host FS **absent** (stronger than deny), secrets under a mount
    masked with `--tmpfs`, gateway via `host.docker.internal` (single choke point — `exec_agent`'s
    base URL), env via an allowlist (`SANDBOX_FORWARD_ENV`, no host-env leak). Works on **any OS**
    with a docker daemon (off-macOS the jail now turns on for docker). Image operator-supplied
    (`ROZUM_SANDBOX_DOCKER_IMAGE`, default `rozum-agent:latest`; must have the agent CLI on PATH).
    **Validated on M4 (Docker 29.6):** 4 unit tests on the argv + a real `docker run busybox` e2e
    (in-workspace write round-trips / out-of-mount write denied / secret tmpfs-masked) +
    `host.docker.internal` reachability probe + full `rozum launch --no-model` container run (stdout
    surfaced, env allowlist forwarded `CLAUDE_CODE_*`, non-listed host var stayed empty). **`rozum-agent`
    image DONE 2026-06-20** (branch `feature/rozum-agent-image`): `docker/rozum-agent.Dockerfile`
    (Rust + git + Node 22 + claude/codex/opencode CLIs; `/etc/profile.d/rust.sh` so `cargo` is on the
    PATH of login shells too) + `scripts/build-agent-image.sh`; `rozum launch` prints a build hint if
    the image is missing (no silent pull). Validated: a real `cargo new` + `cargo build` + run executes
    **inside** the container jail via `rozum launch … docker` and the output round-trips to the host
    (ignored test `agent_image_builds_a_crate_in_the_docker_jail`). **Resource limits + network knob
    DONE 2026-06-20** (branch `feature/sandbox-limits-network`): `--memory`/`--cpus`/`--pids-limit` via
    `ROZUM_SANDBOX_DOCKER_{MEMORY,CPUS,PIDS}` (memory/cpus opt-in; pids default 2048 fork-bomb guard) —
    verified a 64 MB cap OOM-kills (rc 137) + pids cap fails forks; and `ROZUM_SANDBOX_NETWORK`
    (`none`|`gateway-only`|`full`, both backends) — verified `none`→gateway BLOCKED, `gateway-only`→
    REACHED. Also added `wget` to the image (had only `curl`). **strict-egress + opencode config DONE
    2026-06-20** (branch `feature/sandbox-strict-egress`): (a) **`gateway-strict`** (`NetPolicy::
    GatewayStrict`, `ROZUM_SANDBOX_NETWORK=gateway-strict`) — true gateway-only-no-internet. Earlier
    "no simple Docker mechanism" was right about flags, so we do it IN the container: `--cap-add=
    NET_ADMIN` + `ROZUM_EGRESS=strict`, and the `rozum-agent` entrypoint installs an iptables egress
    allowlist (lo + established + resolved host-gateway, DROP rest incl. all IPv6; fails loud exit 70 if
    unenforceable). Seatbelt = `gateway-only` (already loopback-only). **Verified on M4 via `rozum
    launch`:** gateway REACHED, internet BLOCKED. (b) **opencode config** — write it under canonical
    `/tmp` (a toolchain bind mount) instead of `$TMPDIR` (which Docker Desktop doesn't share) → visible
    in the container at `OPENCODE_CONFIG` (verified in the image). **no-approval autonomy DONE
    2026-06-20** (branch `feature/sandbox-no-approval`): `apply_sandbox_autonomy_flags` injects the
    agent's approval-bypass flag for HEADLESS launches when jailed — `claude -p` →
    `--dangerously-skip-permissions`, `codex exec` → `--dangerously-bypass-approvals-and-sandbox`,
    `opencode run` → `--dangerously-skip-permissions` — so sandboxed models run with no per-action
    prompts (the jail is the safety boundary; kills the codex reject-escalation loop, matrix Finding
    1a). Gated: only when jailed, only headless (interactive operators keep prompts), never overriding
    an explicit user policy. Pure helper `autonomy_flag_for` (2 unit tests) + e2e-verified the agent
    receives the flag. **Regression harness DONE 2026-06-20** (`tests/sandbox_regression.rs`):
    fast no-default checks + ignored Seatbelt/Docker e2e commands documented in
    `docs/specs/model-sandbox.md`. **The model-sandbox P3 track is complete** (only the
    optional P2 Linux Landlock/bubblewrap backend remains, off macOS).

  - [ ] sandbox-docker-e2e-rerun - **Deferred Docker confidence pass.** When Docker Desktop (or another
    docker daemon) is intentionally running and enough memory is available, rebuild/confirm
    `rozum-agent:latest` with `scripts/build-agent-image.sh`, then run the ignored Docker sandbox
    regression checks:
    `cargo test --test sandbox_regression docker_e2e_builds_simple_crate_inside_jail
    --no-default-features -- --ignored` and
    `cargo test --test sandbox_regression docker_e2e_gateway_strict_reaches_host_and_blocks_internet
    --no-default-features -- --ignored`. Keep this out of the active sprint while Docker is off to
    preserve RAM; the normal macOS Seatbelt path and no-default tests do not require Docker.

- [ ] windows-portability - **Make rozum a first-class Windows host (durable core + CI).**
  rozum-as-gateway/launcher already works on Windows today (HTTP backends are pure
  cross-platform Rust); these sub-tasks close the gap for the **local meeting daemon** and
  **in-process engines**. All hardware-independent except GPU validation. Spec:
  `docs/specs/portability-and-the-backend-spi.md` (§ "Platform-aware build (Linux *and*
  Windows)"). Engines on Windows are tracked elsewhere and need NO separate item: GGUF via
  `portability-cuda-gguf` (non-`metal` llama-cpp-2 — CPU/CUDA/Vulkan; builds with MSVC), and
  the native iGPU path via `x86-native-runtime` (Vulkan is cross-platform — the SAME L5 engine
  runs on Windows; `VK_EXT_external_memory_host` zero-copy works there too). Sub-tasks:
  - [x] windows-core-ci - **DONE 2026-06-20.** A `windows-core` CI job (`windows-latest`)
    now mirrors `linux-core`: `cargo build --no-default-features --lib --bin rozum` +
    `cargo test --no-default-features --lib` on every push/PR, so a Windows regression fails
    CI, not folklore. Remaining Windows work is the concrete seams below.
  - [ ] windows-daemon-ipc - Abstract the meeting daemon's client transport. Today it's a
    Unix-domain socket (`meeting_sock()` / `UnixListener`), and `std::os::unix::net` does not
    exist on Windows. Put it behind a small transport with a Windows impl — AF_UNIX (Win10
    1803+) via a crate (`interprocess`), a named pipe, or a loopback-TCP fallback. The
    single-writer + direct-read model and the `mcp-proxy` bridge stay; only the byte transport
    changes.
  - [ ] windows-service-install - Add a Windows arm to `src/service.rs` (today: launchd/systemd
    generation + `launchctl`/`systemctl`): install/uninstall a Windows Service (`sc.exe` / the
    `windows-service` crate) or a Task Scheduler entry. The module is already "pure generation +
    invoke", so the arm slots in beside the existing two.
  - [ ] windows-fs-locks - Route the `.rozum/room/` single-writer advisory lock through a
    cross-platform lock (`fs2` / `fd-lock`) instead of a raw `flock`, and confirm all room/cache
    path handling is `PathBuf`-based (no hardcoded `/`).

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

- [x] portability-new-backend-checklist - **DONE 2026-06-15.** The "add a new runtime/hardware
  backend" recipe is written down — a concrete *Add-a-backend checklist* in
  `docs/specs/portability-and-the-backend-spi.md` (the 2 required `ChatBackend` methods + the opt-in
  hooks `concurrency_capacity`/`count_tokens`/`label`; bring your own template/tokenizer/cache; slot
  into `main.rs` builder + `config.rs` `ACCEPTED_ENGINES`; test feature-free). Folklore → checklist.

- [ ] portability-cuda-gguf - Concrete non-Mac GPU path: expose `gguf-cuda` /
  `gguf-vulkan` features that pass the matching `llama-cpp-2` backend feature
  through, so a Linux/CUDA user gets GPU GGUF inference without editing Cargo.toml.
  (Cheapest real "runs on someone else's non-Mac hardware" deliverable.)

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

- [ ] extract-shared-serving-helpers - **L1. STARTED 2026-06-16** (`src/serving.rs`).
  Tool-call parsing is unified there: MLX's whole-text `parse_tool_calls` and GGUF's
  streaming `tool_name` both call it (the duplicated body-parsing is gone). It was also
  made **robust** — when a model emits no `<tool_call>` envelope (common for 4B–7B models
  driven by a foreign tool schema, which fall back to a bare or ```json-fenced
  `{name,arguments}`), the call is recovered via a string-aware balanced-brace scan with a
  strict `arguments`-is-object guard against false positives; native `<tool_call>` blocks
  suppress the fallback. Validated end-to-end: Coder-7B's lost tool calls now execute. Still
  to lift into `serving`: tool-history rendering (`message_text`), UTF-8-safe incremental
  detokenize, multi-EOS stop logic, the KV/RAM preflight.

- [x] serving-loose-json-repair - **DONE 2026-06-16** (`src/serving.rs`). `parse_loose_tool_calls`
  now repairs a **malformed** `{"name":…}` when the strict path finds nothing: `repair_tool_object`
  does a single tolerant scan that disambiguates a content `"` from a structural one by lookahead (a
  `"` closes the string only if the next non-ws byte is `:` / `}` / `]` / EOF, or a `,` followed by
  the next key's `"`), escaping content quotes + raw control chars — so `println!("{}", x)` (incl. the
  `"{}"`-then-comma case) is recovered, not dropped. Runs only when the strict parse failed (no
  false positives). Validated: Coder-7B `build` now passes **with `ROZUM_MLX_CONSTRAIN=0`** (was a
  fail — lost the call). Known limit: a literal `","` inside content still defeats the heuristic.

- [ ] mistral-system-fold — **WON'T DO (2026-06-16).** A restrictive chat template (Mistral-7B-v0.3:
  rejects the `system` role via `raise_exception` + needs strict user/assistant alternation) 500s on
  every Claude Code request (which sends a system message + tool results). Folding system→first-user
  when a template lacks system support would un-break it — but **only Mistral-v0.3 needed this**, and
  it's been deleted from the cache + benchmark; all kept models (Qwen2.5/Qwen3/Qwen3.6) support the
  `system` role natively. Not worth the message-rewriting complexity for a model we don't use. Reopen
  only if a future restrictive-template model we actually want shows up.

- [x] extract-shared-sampler - **L2. DONE 2026-06-15** (`src/sampler.rs`). The sampler
  (repeat-penalty → temperature → top-k → top-p → categorical) defined over a plain `&[f32]` logit
  slice + an `impl Rng`, engine-agnostic. `SamplerConfig::from_params`, `seeded_rng(seed)`,
  `repeat_window`, `sample(logits, cfg, recent, rng)`. 6 deterministic unit tests (greedy, repeat
  penalty, top-k=1 collapse, top-p nucleus, seeded determinism, window).
  - **GGUF now calls it** (`gguf.rs`), replacing its ad-hoc temp+softmax + buggy global-static LCG —
    a real upgrade (gains top-k/top-p/repeat-penalty/seed) AND the dedup. Compile-verified
    `--features gguf`.
  - The MLX hot path keeps its on-device `sample_with` (byte-exact oracle tests); `src/sampler.rs` is
    the canonical CPU definition it mirrors, and what CPU leaves (GGUF, and any future CUDA/CPU leaf)
    call. The per-token GPU→CPU copy of one vocab vector is negligible for op-launch-bound decode, so
    a leaf can adopt it whenever byte-exactness isn't required.

- [x] extract-model-reference-specs - **L3. DONE 2026-06-15.** Captured the model *knowledge* as
  engine-independent reference docs in `docs/specs/model-reference/`: a `README.md` of cross-cutting
  checkpoint conventions (AFQ `.weight↔.inner.weight`/`.bias↔.inner.bias` remap, RMSNorm `+1`, tied
  embeddings, safetensors stale-shard-index fallback, multimodal `text_config` unwrap, MLX↔PyTorch row
  order) + one file per family (`qwen3`, `qwen36-hybrid` incl. the f32 GatedDeltaNet scan, `llama-family`
  incl. the Phi-3 fused-projection split + Mistral `head_dim`/list-template, `qwen2` QKV-bias, `gemma3`
  incl. the multimodal-wrapper defaults table). The forward math + quirks per family, grounded in the
  fork's model files. Linked from `mlx-native-runtime.md`. The code stays per-tensor-lib; the spec lets a
  new leaf implement from fact instead of re-deriving from a checkpoint.

- [x] extract-metal-kernels - **L4. DONE 2026-06-15.** Factored the GatedDeltaNet fused delta-rule
  scan kernel's MSL out of the inline raw string in the fork's `models/gated_delta.rs` into a
  hardware-only module: `mlx-lm/src/kernels/gated_delta_step.metal` (the kernel body + its I/O
  contract) + `kernels/mod.rs` exposing it via `include_str!` as `GATED_DELTA_SOURCE`. The engine
  binding (`MetalKernel::new` buffers, dispatch, eval control) stays in `gated_delta.rs`. So a future
  Metal engine (candle-metal, mistralrs-metal) can bind the same `.metal` instead of re-deriving the
  math. **Pure move — kernel output byte-identical:** the fork's `gated_delta_kernel_matches_ops` +
  `gated_delta_matches_python` still pass (4/4), and rozum's full suite is green against the bumped fork
  rev `838a39ab`. 178/0. Future Metal kernels land in the same module.

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

- [x] rozum-gateway-tool-contract - **P0b (rozum). DONE 2026-06-15.** Stabilize + document the
  Contract-1 surface the SDK targets: `/v1/chat/completions` (+ `/v1/messages` + `/v1/responses`)
  with `tools` (JSON-Schema), `tool_choice`, `temperature`, `stream`; response `tool_calls`
  (id/name/arguments) vs text + `finish_reason`; SSE tool-call argument deltas.
  - Closed the one real gap: `tool_choice` was silently ignored on all three routes. Now parsed +
    normalized across dialects (`ToolChoice::{Auto,None,Required,Named}`; OpenAI string/object,
    Responses flat, Anthropic `auto`/`any`/`none`/`tool`) and honored by transforming the tool set
    (none→empty, named→restrict) — no SPI change. `required` is accepted but best-effort (not forced),
    documented as such.
  - Documented as a stable contract in `docs/specs/api-gateway.md` (Tool-use / Contract-1 section:
    request `tools`/`tool_choice` table, non-streaming + streaming response shapes, the
    `finish_reason`/`stop_reason` mapping, and the `ROZUM_MLX_CONSTRAIN` arg-reliability note).
  - Conformance unit tests: `tool_choice_parse_openai`/`_anthropic`, `tool_choice_apply_semantics`,
    `oai_collect_tool_call_shape`, `anthropic_collect_tool_use_shape` (mock tool stream → asserted
    response JSON). 146/0.
  - Follow-up: genuinely *forcing* `required`/named (mask the model to start a tool call) — pairs
    with the constrained-decoding opener; deferred.

- [~] rozum-distributed-readiness - **P0b/P1 (rozum). Core SHIPPED 2026-06-15.** The gateway
  as a deployable, horizontally-scalable, stateless service. Spec:
  `docs/specs/distributed-readiness.md`.
  - **Health/readiness endpoints**: `GET /health` (liveness — never touches the model) and
    `GET /ready` (readiness — 200 servable / 503 while draining; body `{ready, loaded,
    shutting_down, model}`). A transient swap-drain does NOT flip readiness (those park + succeed).
  - **Graceful shutdown** on SIGTERM/SIGINT (`with_graceful_shutdown`): flip `/ready`→503 +
    reject new chats (`enter()` 503 instead of parking), grace (`ROZUM_SHUTDOWN_GRACE_SECS`,
    default 3) so the LB deregisters, then axum drains in-flight streams and exits — rolling-deploy
    safe.
  - **Stateless** documented: prefix-KV is a per-instance optimization, not affinity → no sticky
    sessions; round-robin/least-conn is fine. Builds on the existing shared-gateway daemon +
    `concurrency::admit_wrap` + the launch proxy replay/retry.
  - Tests: `readiness_reflects_servability`, `shutdown_flips_readiness`,
    `enter_rejects_new_chats_while_shutting_down`. 149/0.
  - **Follow-ups** (out of scope here): a model pool/router serving multiple resident models with
    size-class routing (`shared-gateway-multislot` + `concurrency-multi-instance`); cross-instance
    admission coordination (`concurrency-cross-process`).

- [x] rozum-agent-runtime - **P0b (rozum, DUAL-PURPOSE). DONE 2026-06-15** (`src/agent.rs`).
  A Rust reference implementation of the agent loop (Contracts 2–3): `(backend, system, user,
  tool_source, budget)` → model call → `tool_use` → execute via tool source → feed result → repeat.
  Dual-purpose: the in-process **embedded mode** and the **executable spec** the scalascript SDK
  mirrors. See the implemented contracts in `docs/specs/integration.md`.
  - **Contract 3**: `ToolSource` trait (`tools()` + `async dispatch(name,args)->Result<Value,
    ToolError>`) with BOTH adapters: `CallbackToolSource` (direct in-process) and `McpToolSource`
    (external MCP server over `rmcp` — `connect_stdio` spawns it, caches `list_tools`, forwards
    `dispatch` as `tools/call`; needs the `transport-child-process` rmcp feature). `ToolError` =
    recoverable message fed back to the model.
  - **Contract 2**: `run_agent(...) -> AgentOutcome {text, stop, steps, operations, transcript}`,
    bounded by `Budget {max_steps, max_tokens, wall_time, temperature=0}`. Speaks only the
    `ChatBackend` SPI → runs against any backend.
  - Validated model-free (scripted MockBackend: full loop + result feedback, budget cap,
    unknown-tool + handler-error recovery; `McpToolSource` over an in-memory MCP duplex: list +
    dispatch) AND e2e on native MLX (`agent_loop_real_backend`: Qwen3-4B `add(3,5)`→`{sum:8}`→final
    text, constrained args). 157/0.
  - **Remaining (separate item)**: `rozum-embed-crate` (P2) — the stable public crate over this.

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

- [x] mlx-native-cancel-prefill - DONE (fork `fb263995` + rozum `b022dc4`). The
  hybrid `Generate` polls a `should_cancel` predicate between prefill chunks
  (`prefill_cancellable` -> `Ok(None)`); rozum wires it to `job.cancel`, so a
  cancel/disconnect on a long prompt is honored DURING prefill, closing the
  native-side analog of the mistralrs large-prompt stall. Test
  `mlx_qwen35_prefill_cancels_mid_prefill`.

- [x] mlx-native-sampling - DONE: top_p/top_k/seed (fork `f36c8c3a` + rozum
  `510c760`) AND repeat_penalty (fork `e970b23a` + rozum `3597abe`). `sample_with`
  ported from mlx_lm, threaded through all Generate; greedy stays argmax
  (byte-exact). repeat_penalty applies over a 256-token window (take/put_along_axis,
  O(window)); Generate keeps a token history only when penalty != 1.0. Unit test
  pins top_k=1/tiny-top_p == argmax + that a hard penalty moves the argmax.

- [x] mlx-native-tool-use - DONE (fork `1fc66029`/`e316dbf7` + rozum `09dfbcc`).
  `mlx-lm-utils` `ApplyChatTemplateArgs` gained a `tools` field -> minijinja context
  (+ enabled minijinja `json` feature for the `tojson` filter). Rozum: `Job` carries
  `req.tools`; `render_prompt` builds OpenAI-style schemas; `stream_generation`
  suppresses `<tool_call>` from text and parses it into `ToolUse*` events +
  `stop_reason=ToolUse`. E2E `mlx_tool_use_weather` (get_weather call) + unit
  `parse_tool_calls_extracts`.

- [x] mlx-native-tool-history - DONE (rozum-only, pin unchanged). `message_text`
  renders assistant `ToolUse` blocks back as `<tool_call>` markup (inverse of
  `parse_tool_calls`) instead of dropping them, so multi-turn tool loops carry the
  prior call in history. Unit `tool_use_round_trips_into_history`.

- [x] mlx-native-multi-eos - DONE (rozum `b022dc4`). `read_config` collects the full
  `eos_token_id` set; `stream_generation` stops on any (Qwen3: `<|im_end|>` 151645 +
  `<|endoftext|>` 151643).

- [x] gguf-tool-use-non-qwen - **WON'T DO (2026-06-15).** GGUF is the maintained fallback, not an
  area of active feature work; tool-use for Llama-3.1 / Mistral is already covered by the primary
  native-MLX engine (constrained decoding + the cascade). Not worth extending the GGUF parser.

- [~] ui-streaming-ws-tui - **NOT APPLICABLE to the current architecture** (2026-06-15). Propagate a
  `ChatEvent` token stream to the web WebSocket + TUI for partial rendering. After the meeting-room
  pivot there is no such stream to propagate: external agents (Claude Code, Codex) generate their own
  responses and submit **complete** messages via the MCP `meeting.submit` tool (atomic), and the web
  bridge broadcasts complete transcript entries over its WebSocket. The live "is responding" indicator
  already exists (`responding-indicator.md`). Token-level streaming would require streaming partial
  submits through the MCP meeting protocol — a protocol change, not a UI change. Revisit only if rozum
  itself renders a locally-generated model stream in the room UI.

- [x] openai-http-client-backend - **DONE.** `ChatBackend` that calls the OpenAI Chat Completions API
  (`src/openai_http.rs`): SSE text + tool-call deltas → `ChatEvent`, sends `tools`, finish/usage/cancel,
  works against any OpenAI-compatible server (Ollama, llama.cpp, vLLM, OpenAI). 2026-06-15: added
  `with_api_key` (Bearer) so authenticated remotes (OpenAI/OpenRouter) work, not just local servers.

- [x] anthropic-http-client-backend - **DONE 2026-06-15** (`src/anthropic_http.rs`). `ChatBackend`
  calling the Anthropic Messages API: folds system turns + tool-results into the Anthropic wire shape,
  POSTs `/v1/messages` with `x-api-key`/`anthropic-version`, and parses the Anthropic SSE
  (`content_block_start`/`_delta`/`_stop` + `message_delta`/`message_stop`) — text + `tool_use` blocks
  — back into `ChatEvent`s. Enables frontier-model escalation/fallback (per `integration.md`). Unit
  tests for the SSE parser (text + tool_use) and the message conversion. 160/0.
  - Shares SSE parsing logic with the gateway server side.
  - Complements / supersedes the `remote-api-backends` sprint task (which predates the new SPI).

## Runtime And UX

- [x] cascade-router - **DONE 2026-06-15** (`docs/specs/cascade-router.md`; see SPRINT `cascade-p1…p9`
  + follow-ups). Frugal/escalation routing, complete end to end: all 9 phases (cost-ordered cascade,
  transient health, self-signal + uncertainty affordance, L2 judge, difficulty classifier, parallel
  residency lanes, learned+persisted stats with the `learned` start-tier, execution-feedback
  escalation, adaptive per-model concurrency) + the gateway request-surface wiring (`model:
  "cascade[:name]"`, `[cascade.<name>]` in `rozum.toml`, env JSON) + the simple path (just list
  models — comma/repeatable `--model`, `--strategy`, multi-select picker with Anthropic+OpenAI) +
  native Anthropic tier. The P9 controller is fed by the full signal set (overload, throughput,
  latency, quality, headroom), reconciled with the circuit breaker. Only intentional non-goals remain
  open (e.g. proactive health-pattern deprioritization, cross-process fleet).

- [x] gateway-openai-responses-api — **DONE.** `POST /v1/responses` (the OpenAI Responses API)
  so the **Codex CLI** (≥ 0.137, which dropped `wire_api="chat"`) can use the gateway.
  `responses_handler` parses the Responses request (`instructions` → system; `input` items —
  messages / `function_call` / `function_call_output`; flat `tools`; `max_output_tokens`) into
  the internal `ChatBackend`, and streams back the typed Responses event protocol
  (`response.created` → `output_item.added`/`content_part.added` → `output_text.delta` →
  `output_text.done`/`content_part.done`/`output_item.done` → `function_call` items
  (`arguments.delta`/`.done`) → `response.completed`, each event with `type` +
  `sequence_number`); non-stream returns the final `response` object with `output[]` + `usage`.
  Reuses the same backend stream as `/v1/chat/completions` (our event order — text then whole
  tool calls then Done — maps onto a message item + function_call items). Stateless (Codex
  sends the full `input` each turn). Tests: input/tool conversion, response-object shape, SSE
  smoke. The e2e Codex runner (`scripts/e2e_codex_gateway.sh`) now connects via
  `wire_api="responses"` (Codex ignores `OPENAI_BASE_URL`, so it sets `-c model_provider`).

- [x] mlx-native-prefix-kv-cache — **DONE for dense arches.** Reuse KV across agentic turns:
  the cap-1 worker now persists the previous request's prompt ids + KV (`PrefixCache`), and
  when the next prompt strictly extends it (the append-only agentic-loop case) it truncates the
  cache to the shared prefix and prefills only the **new suffix** instead of re-prefilling the
  whole growing conversation. Byte-exact: the kept `[0,reuse)` KV is exactly what a fresh
  prefill computes, and `create_attention_mask` builds the causal mask from the cache offset
  (integration test `mlx_prefix_reuse_byte_exact` asserts reuse output == fresh prefill). New
  fork method `ConcatKeyValueCache::truncate` (mlx-rs fork rev `c8517814`). `ROZUM_PREFIX_CACHE=0`
  disables. Dense only — Qwen3 / Qwen3-MoE / Llama / Qwen2 (they own the KV cache externally).
  **Follow-up below for hybrid (Qwen3.6).**

- [x] mlx-native-prefix-kv-cache-hybrid — **DONE.** Prefix reuse for the **hybrid** Qwen3.6
  arches (Qwen35 + Qwen35Moe). The `Full(KV)` layers truncate to the shared prefix like dense;
  the `Linear` GatedDeltaNet layers carry a recurrent state that can't be truncated, so it's
  **deep-snapshotted** (`Array::deep_clone` → own buffer, survives decode buffer donation) at the
  **end of prefill** (offset == prompt len) and restored on the next reuse. Fork (rev
  `fd284599`): `LayerCache::{truncate, snapshot→LinearSnap, restore}`, `Generate::with_cache`
  (start from a pre-populated cache + suffix) snapshotting right after the prefill step, and
  `into_cache_and_snapshot()`. rozum: `stream_generation` returns the iterator so the hybrid arms
  reclaim cache+snapshot; the worker persists `HybridPrefix{ids, cache, snap}` and on reuse
  truncates Full + restores Linear + prefills only the suffix. **Byte-exact** vs a fresh prefill
  (integration test `mlx_prefix_reuse_byte_exact_hybrid` on the deterministic Qwen3.6-27B; the
  35B-A3B MoE shares the exact reuse logic). `ROZUM_PREFIX_CACHE=0` disables.

- [x] mlx-native-runaway-stop — **DONE.** Bound a single runaway generation so one greedy loop
  can't pin the cap-1 worker for minutes (the e2e `test` task hit a 600 s hang, `result=None`).
  Two guards in the backend: (a) `DEFAULT_OUTPUT_CEILING=8192` clamps the effective `max_tokens`
  regardless of the client value (`ROZUM_MAX_OUTPUT_TOKENS` overrides; 0 disables) — a backstop;
  (b) `is_runaway_loop` in `stream_generation` stops when the last 64 generated tokens are
  exactly periodic with period ≤16 (a short block repeated ≥4×) — the principled fix, catches a
  greedy loop in ~64 tokens with no false positives on real text (`ROZUM_REPEAT_GUARD=0`
  disables). Unit test `runaway_loop_detection`. `--max-turns` does NOT help (it bounds the
  agentic loop, not one generation's tokens) — this does.

- [x] rozum-native-channels-tier3 - DONE (`feature/piggyback-wakeup`). Tier-3
  gateway piggyback wakeup, keyed by project + agent name. mcp-proxy drops each
  room transcript delta to `$XDG_RUNTIME_DIR/rozum/piggyback/<project>/<agent>.log`;
  the launch-local HTTP proxy drains it into the next chat request as an
  out-of-band system note (Anthropic `system` / OpenAI `system` message; tool JSON
  + SSE untouched). Fallback rung: auto-off when Tier-1 channels are active, on
  otherwise; `--no-piggyback` forces off, `ROZUM_PIGGYBACK=1` forces on. New
  `src/meeting/piggyback.rs` +
  hooks in `src/meeting/proxy.rs` (writer) and `src/proxy.rs` (reader). Reaches
  agents that take neither Tier-1 channels nor a Tier-2 `wait_my_turn` loop. Spec:
  `docs/specs/rozum-native-channels.md`.

- [x] streaming-output - **DONE/OBSOLETE 2026-06-15.** Satisfied by the gateway: model output
  streams **token by token** on all three dialects — OpenAI `/v1/chat/completions` (`oai_sse_stream`,
  a chunk per `ChatEvent::TextDelta`), OpenAI `/v1/responses`, and Anthropic `/v1/messages`
  (`anthropic_sse_stream`). The "CLI eval" framing predates the gateway (there's no non-streaming CLI
  run/eval path to retrofit; the agent runtime collects programmatically by design). A multi-model
  cascade necessarily buffers (it must see the whole answer to judge it), but a single-model
  passthrough streams live.

- [x] structured-output - **DONE 2026-06-15.** JSON/schema-constrained output, exposed as a non-tool
  `response_format` request field. The gateway parses OpenAI `response_format`
  (`{"type":"json_object"}` → any object; `{"type":"json_schema","json_schema":{"schema":…}}` → that
  schema) onto `SamplingParams.response_schema`. The native MLX backend constrains the WHOLE response
  to it during decode (`ResponseConstraint` + a generic `ConstraintDriver`/`constrained_decode_loop`
  shared with the tool path) — always honored when present (no env flag), dense + hybrid arches.
  Validated: gateway parse unit test + e2e (`mlx_response_format_json_schema`, Qwen3-4B → pure
  `{"city":"Paris","country":"France"}`). 161/0.
  - Required for reliable tool routing.
  - Start with parse/repair/retry before grammar decoding.

- [x] tool-routing - **DONE 2026-06-15** (`src/builtin_tools.rs`). A small registry of safe,
  read-only built-in tools (`echo`, `current_time`, `list_models`) exposed as a `CallbackToolSource`,
  so the reference agent runtime (`run_agent`) lets the model select them. Side-effect-free (no
  filesystem/network writes); `list_models` surfaces the recommended catalog + locally-installed
  models. File lookup deliberately omitted (security). Unit-tested (registry shape + each tool's
  dispatch incl. the missing-arg `ToolError`). An app composes these with its own domain tools.

- [x] memory-store - **DONE 2026-06-15** (`src/memory_store.rs`). Append-only local memory: a
  key→value JSONL log with retrieval by exact key (`MemoryStore::{open, in_memory, set, get, all,
  keys}`; last-write-wins for `get`, full per-key history for `all`; appends never rewrite). Exposed
  to the agent runtime as `remember`/`recall` tools (`memory_tools(Arc<MemoryStore>)`) so a small
  local agent has durable memory across turns. Unit-tested (append-only history, disk persistence +
  replay, the tools). No embeddings/ranking — that's `rag-lite`.

- [x] rag-lite - **DONE 2026-06-15** (`src/rag_lite.rs`). Local retrieval over small text documents:
  `LexicalIndex` (BM25 — `add(id, text)` + `search(query, k) -> Vec<Hit>`), pure Rust, no model/network,
  deterministic. The `Retriever` trait keeps the API stable so an embedding backend can drop in later
  (the "configurable backend"). Exposed to the agent runtime as a `search_documents` tool
  (`retrieval_tools(Arc<dyn Retriever>)`). Unit-tested (BM25 ranking + no-match/empty/k=0 edges, idf,
  the tool). Lexical fallback is the starting point per the brief; embeddings are the follow-up.

### Concurrency & scheduling (follow-ups to `mistralrs-concurrency-scheduling`)

Stretch items deliberately out of scope of the initial A→B+C→D delivery. See
`docs/specs/mistralrs-concurrency-scheduling.md` (Out of scope).

- [ ] concurrency-engine-yield - **LOW PRIORITY (2026-06-15): mistralrs-only + non-default, and the
  default engine already does better.** This targets the **mistralrs fork** (`pipeline::step`), which
  is **not in the default build** (`default = ["mlx-native", "gguf"]`). The default **mlx-native**
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

- [x] concurrency-cost-tokenizer - **DONE 2026-06-15** (`src/concurrency.rs`, `src/backend.rs`).
  `RequestCost::estimate(req, count_tokens)` is now tokenizer-pluggable: a new
  `ChatBackend::count_tokens(text) -> Option<usize>` hook (default `None`) lets a backend supply
  exact counts; the `AdmittingBackend` passes `self.inner.count_tokens`. The fallback heuristic is
  fixed to count **characters** (`chars().count()`), not bytes — the old `str::len()/4` over-counted
  non-ASCII (e.g. Cyrillic) prompts ~2× — and now also sums tool-result + rendered tool-call blocks.
  3 tests (exact-via-hook, char-not-byte, sums-all-blocks). 270/0. *Follow-up*: the MLX/GGUF
  tokenizers live in `!Send` worker threads, so wiring their `count_tokens` needs a worker round-trip
  (or a cached token-count cell) — left `None` for now; remote backends have no local tokenizer.

- [x] concurrency-multi-instance - **DONE 2026-06-15** — "several models on one GPU, done smartly" is
  fully covered by three shipped mechanisms:
  1. **Shared cross-resident GPU gate** (`src/concurrency.rs::global_gpu_gate`): a process-wide
     semaphore (size = one GPU's concurrent-prefill sweet spot, `DEFAULT_SEQS_CEILING`;
     `ROZUM_GPU_GATE` overrides) every local `admit_wrap`-ped backend acquires *in addition to* its
     per-model slot, so prefills across **distinct residents** can't oversaturate one GPU. Acquired
     after the per-model admit (no priority inversion); no-op for a single resident. 2 tests.
  2. **Size-class routing** (small lane / big lane, non-blocking) = the cascade's Phase-6 `LaneSet` +
     difficulty classifier (simple→small, complex→big, parallel lanes).
  3. **Shared memory budget** across residents = `shared-gateway-multislot`'s `plan_residency`
     (memory-gated admission + utility eviction).
  Out-of-process coordination (several daemons on one GPU) stays in `concurrency-cross-process`
  (low-priority — the architecture avoids it).

- [ ] concurrency-cross-process - **LOW PRIORITY (2026-06-15): the architecture avoids the
  multi-process case.** The in-process shared GPU gate (`concurrency-multi-instance` core) + multislot
  (several models in ONE daemon) + the single-shared-daemon registry mean the typical setup is one
  process — so a host-wide budget only matters in niche layouts (`--dedicated` beside the shared
  daemon, or several independent `rozum gateway` processes on one GPU). Needs IPC (named semaphore /
  `flock` / a coordinator) + multi-process validation. Original note: coordinate the concurrency
  budget across several `rozum` processes sharing one GPU, instead of budgeting in isolation.

- [x] concurrency-observability - Expose queue depth, admission limit, fast-lane
  hits, and shed/429 counts so the scheduler is tunable from data. **DONE 2026-06-14.**
  `/stats` reports an `admission` block — instantaneous (limit / in-use / waiting / free) PLUS the
  cumulative scheduler counters (`admitted`, `fast_lane`, `shed`, `queued`) — a `batch` block (runs /
  rows / mid-decode admits / peak / avg occupancy), and `mlx_memory_mb` (active/peak/cache). The
  counters live in the `AdmissionScheduler` `State` (under its existing mutex, no extra atomics):
  `take()` bumps `admitted` (+`fast_lane` for a reserved-lane admit), the full-queue path bumps
  `shed` before returning 429, and a queued arrival bumps `queued`; a pumped-but-cancelled waiter
  decrements back. Surfaced via `AdmissionSnapshot` + `AdmissionScheduler::counters()`. Test
  `counters_track_admit_fastlane_queue_and_shed` walks the whole flow (admit → fast-lane → queue →
  shed → admit). (The numbers are in `/stats` JSON; a push into the `obs` event log is a trivial
  later add if a metrics pipeline wants it.)

- [~] shared-gateway-multislot - **Phase 1 (decision core) DONE 2026-06-15** (`src/resident.rs`).
  Allow more than one resident model behind the shared gateway when memory permits — **adaptively**:
  keep the most *useful* (frequency × recency) small models co-resident without thrashing, evict the
  least useful (idle only) to make room, and fall back to a swap for a model too big to co-reside
  (unavoidable thrash). `UsageStats` (persisted JSONL) learns per-model usefulness; `plan_residency`
  is the pure, fully-tested memory-gated/utility decision (greedy keep-highest-utility-that-fits,
  busy models never evicted, `oversubscribed` flags the swap case). 7 tests.
  **Phase 2 IMPLEMENTED 2026-06-15** (mock-tested; `src/gateway.rs` + `docs/specs/shared-gateway-
  multislot.md`) — an **additive warm cache** alongside the untouched single-resident core. `enter(req
  .model)` routes a *different*, warmable model (a known cached local that fits) to a warm secondary
  resident built via the existing builder; admit/evict goes through `plan_residency`; a warm entry has
  its own in-flight counter (decoupled from the primary drain) and is evicted (idle-only,
  `spawn_blocking` drop) under memory pressure. **On by default** (user's choice), `ROZUM_MULTISLOT=0`
  opts out, **strict no-op for single-model traffic**, falls back to the primary on any miss
  (unknown/remote model, won't fit, build fail). 4 tests (serve-second, fall-back, skip-unknown,
  evict-idle). Plus **idle-timeout warm eviction** (the watchdog `sweep_idle_warm` frees a warm
  model idle past `unload_idle_secs`) and **persisted `UsageStats`** (`$XDG_STATE_HOME/rozum/gateway/
  warm-usage.jsonl` → the warm set's usefulness survives a restart). 6 tests. 278/0. **Real-model
  validation pending** (two real models co-resident, eviction frees RAM — the user runs it). A shared
  cross-resident GPU gate already shipped (`concurrency-multi-instance` core); out-of-process
  coordination stays in `concurrency-cross-process`.

- [x] shared-gateway-service - **DONE 2026-06-15** (`src/service.rs` + `src/main.rs`;
  `docs/specs/shared-gateway-service.md`). `rozum service {install,uninstall,start,stop,status}`
  registers the gateway as an always-warm **user service** (launchd on macOS, `systemd --user` on
  Linux) instead of
  lazy spawn + idle-exit. `--model` (repeatable/cascade) + `--port/--n-ctx/--offline/--strategy`;
  `ROZUM_CASCADE`/`ROZUM_CONFIG` captured into the service env. The plist/unit generation is the
  library's pure, unit-tested `service` module (4 tests); the binary writes the file + drives
  `launchctl`/`systemctl` (operator-validated, touches the real service manager). 282/0.

## Model Quality

- [ ] model-catalog-refresh - Expand and verify tiny model catalog.
  - Include current small Qwen/Gemma/Phi candidates with exact file sizes.
  - Record license and expected strengths.

- [ ] benchmark-baseline - Record latency, disk size, and smoke eval score for each backend/model pair.
  - Use the eval harness once available.

- [x] prompt-policy - **DONE 2026-06-15** (decision, `docs/specs/prompt-policy.md`). The gateway is a
  **transparent provider**: it passes the client's own system prompt + messages through unchanged and
  does **not** inject per-model prompts (that would corrupt CC/Codex). Raw is the default and only
  mode; the lone shaping is the existing `--enable-thinking` toggle. Per-model style/persona lives in
  the caller (agent runtime's `system` arg / room etiquette), not the gateway. A per-model prompt
  registry is explicitly rejected — the transparent boundary is the feature.

- [ ] distillation-plan - Design a later LoRA/QLoRA or distillation path.
  - Do not implement until evals provide a baseline.

## Project Hygiene

- [x] commit-initial-project - **DONE/N-A (2026-06-15).** The project is a live git repo with full
  history (this very work merges to `master` daily); the "commit the initial state" task is moot.

- [x] ci-smoke - **DONE 2026-06-15** (`.github/workflows/ci.yml`). Build + feature-free `cargo test
  --lib` on `master` push/PR (macos-latest, cargo cache). No model downloads; the real-model smoke
  tests stay opt-in (feature-gated + `#[ignore]`).

- [x] docs-bootstrap - **DONE 2026-06-15** (`README.md`). Refreshed the README with the LLM gateway /
  `rozum launch` / model-cascade quickstart (it was meeting-room only). Clone/submodule/build + first
  room + MCP proxy were already covered.
