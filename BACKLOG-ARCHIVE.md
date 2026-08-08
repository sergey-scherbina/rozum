# Backlog archive

Finished backlog items, moved here so `BACKLOG.md` holds only what is still open.
Nothing is deleted: 80 closed entries were sitting inline and made up 1206 of its 2072 lines,
which is most of the reason nobody read it. The story of each is in `CHANGELOG.md`; this file
keeps the entry as it was written, because some of them carry a finding the changelog does not.

## rozum-core::share tests read the real machine (found 2026-08-05)

- [x] **share-tests-isolate — DONE 2026-08-05** (moved to SPRINT and finished the same day).

## Matrix improvement levers (found 2026-07-05 during the matrix-hygiene analysis; evidence in agentic-ucc-1783166880)

- [x] **codex-opencode-create-delivery** — DONE (master `73b6d64`, `rewrite_json_wrapped_apply_patch`).
  E2E-verified: build delivery 0/3→3/3 land (0 rc11), bridge fired 8×, 1 pass, shim error gone. RESIDUAL
  follow-up: **rpn still emits 1 rc11** — capture the rpn `-patches` shape (kept workdir under
  `/tmp/rozum-agentic-*` from the verify-codex-create run) and cover the form the bridge misses (likely an
  `*** Update File:` against an absent file, or a non-`content` JSON key). Remaining build reds are rc10 =
  gpt-oss wrong CODE (model capability, separate from delivery). Original evidence:
- [x] **codex-opencode-create-delivery (original evidence) — SHIPPED**, `3d03a35`, `crates/rozum-gateway/src/codex_patch.rs:104`. The live question that survived it is `codex-create-delivery-on-qwen` below; this entry is the evidence, kept. — a THIRD+ of
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

- [x] **mtg-rich-rooms — DONE 2026-08-07** (spec: `docs/specs/mtg-rich-rooms.md`). R1 roles, R2 a
  persisted lifecycle with `Paused`, R3 the queue view — each on the daemon/CLI/REST trio.
  **The task shrank twice on measurement, and both shrinkings are the value.** (1) Room kind was
  already built and unreachable (P3 `RoomKind`/`Member`), so the question stopped being "how do we
  add it" and became "why keep it" — removed on the operator's call, along with the duplicate role
  mechanism I had just introduced by not checking first. (2) R2 turned out to be a BUG rather than a
  feature: a room's phase was never persisted, so `end()` was forgotten across a daemon restart.
  **Honest limit:** no room on this host has a single thread, so the queue is unit-tested and was
  exercised against a hand-written `threads.json`, never against organically created data.
  Original: **R1 (roles) DONE 2026-08-07.**
  The spec's finding is the useful part: the entry's premise ("today: one flat daily room per
  project") undersold what exists — rooms already had a lifecycle field, durable participant
  identity, and threads carrying id/kind/five-state machine/assignee/pinning/links/SLA. So the
  recommendation, accepted by the operator, is **NOT to store a room kind**: a queue is a VIEW over
  open threads, and a stored room type would be a second, weaker copy of a working concept with two
  places to set state.
  **R1 shipped:** `RosterEntry.roles: Vec<Role>` (reporter|assignee|on_call|observer|admin) —
  a vector because on-call AND assignee is a normal simultaneous state; `Roster::grant/revoke/
  with_role`; `DaemonRoom::grant_role/revoke_role`; `meeting.role`, `rozum meetings role`,
  `GET/POST /rooms/{n}/roles`. Migration proven BOTH directions against fixtures captured from the
  live daemon before the change (`crates/rozum-meeting/tests/fixtures/`, redacted — a real
  RosterEntry carries a reconnect token and the rozum room holds 418 of them).
  **REMAINING: R2** (Phase → Active|Paused|Resolved|Archived, with `Ended` as an alias) and **R3**
  (the queue view over open threads). Both described in the spec; neither touches a day file.
  Original: a richer room model beyond the daily-file chat: rooms with a **lifecycle**
  (a support queue / a per-product channel / a per-incident room), durable identity, membership/roles
  (reporter, assignee, on-call, observer), and a room **kind** (chat | queue | incident). Today: one
  flat daily room per project. Needs a room registry with typed metadata + a migration from the daily
  files. The hinge the rest hangs on.
- [x] **mtg-message-metadata — DONE 2026-08-08. Nothing was built; everything asked for already
  existed, including the test that guards it.** The five kinds are exactly the five named
  (note|question|event|alert|resolution), plus severity, status, assignee, tags, links, thread_id,
  in_reply_to and `thread_op`; the id is derived `<date>/<n>`; and
  `stored_turn_metadata_is_backward_compatible` pins the property the whole design rests on — a
  plain message serialises byte-identically to the v1 line, and a v1 line reads back with defaults.
  **The one unaddressed word is "versioned", and a version field is deliberately NOT added.** The
  format is already compatible in both directions BY CONSTRUCTION: absent fields default on read,
  empty fields are omitted on write, unknown fields are ignored. A version number nothing branches on
  is decoration, and the log is APPEND-ONLY, so a version-keyed migration cannot exist — a past line
  can never be rewritten, only read with defaults, which is what happens today.
  **What would earn one:** a field whose MEANING changes rather than a field being added. Defaults
  cannot rescue that, and a reader needs to know which meaning applies. Add it then, stamped from
  that day forward, with absent read as v1 — the same defaulting that works now. Adding it earlier
  just records "1" on every line forever.
  Original: messages carry **structured metadata**: type (note | question | event |
  alert | resolution), severity, status, assignee, tags, links to artifacts/logs, and a stable message
  id. Today a message is handle+text+timestamp. Needs a versioned message schema (back-compat with the
  plain lines) + write/read paths that preserve it.
- [x] **mtg-threads — DONE 2026-08-07.** Measured first, and almost all of it already existed:
  thread storage, the id, the anchor, reply-chains (`in_reply_to`), the five-state machine, owner,
  severity, SLA, pinning and links — across 8 daemon tools, 12 CLI incident verbs and 22 REST
  references. **The only real gap was the entry's last clause, "a thread-aware reader":** zero
  mentions of a thread in the TUI module, zero in the generated client. Threads were fully trackable
  and completely invisible to whoever was meant to track them.
  Closed by adding a queue pane to `clients/control/meetings.ssc` — the reader `rozum meetings
  attach` actually execs — over the `/rooms/{n}/queue` endpoint from `mtg-rich-rooms` R3.
  Original: group related messages into a **thread = an incident/topic**: a thread id, a
  parent message, reply-chains, thread-level state (open/triaging/escalated/resolved/closed) + SLA/owner.
  This is what turns a stream into trackable incidents. Needs thread storage + a thread-aware reader/TUI.
- [x] **mtg-message-ops** — **working with messages**. **SEARCH DONE + LIVE-PROVEN (`c422764`):**
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
  cards/list; `open_thread` inherits the anchor alert's severity.
  **DONE 2026-08-06 — the three that were listed as remaining are all shipped, on all three
  surfaces, and nobody ticked the box.** `meeting.thread_link` / `meeting.react` / `meeting.redact`
  in the daemon; `meetings incident link`, `meetings react`, `meetings redact` in the CLI; and
  `POST /rooms/{n}/threads/{id}/link`, `POST /rooms/{n}/react`, `GET /rooms/{n}/reactions`,
  `POST /rooms/{n}/redact` in REST. Verified by reading the routers, not the notes.
  **"Edit" is deliberately absent and should stay absent:** the store is an append-only log
  (`store.rs`), so redaction writes a TOMBSTONE that replaces the content rather than mutating the
  line. An edit that rewrote history in place would break the one property the log is for. If
  someone wants "fix a typo", that is a new message referencing the old one, not an edit.
  Search scans day files; index only if rooms get large.
- [x] **mtg-escalation — DONE 2026-08-07** (spec `docs/specs/mtg-escalation.md`). The headline was a
  BUG, not a feature: with no explicit target, `escalate` wrote the literal string "on-call" into the
  audit message and never assigned anyone, so the room recorded a page that never went out. On-call
  is now resolved from the roster (`DaemonRoom::on_call_pick`), the policy for several on call is
  least-open-work with ties by handle — read through `room_queue`, so it cannot disagree with what
  `meetings queue` shows — and with nobody on call it still escalates but says so plainly.
  The audit event records the RESOLVED owner rather than the requested one.
  **NOT built, deliberately:** "escalate to a stronger model" (needs the model-chain, and this host is
  frozen on one model), severity tiers (no consumer), and notification delivery (the messenger's job).
  Original: route/escalate an incident by severity/tier/on-call (to a
  specific agent, a stronger model, or a human), with an escalation policy + an audit trail of who/when.
  Ties into the model-chain (escalate to a stronger model) + identity-roster (who's on-call). The
  "P" of PagerDuty.
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


## Host safety

- [x] **residency-gate-v2-ramledger** — DONE (`feature/gateway-residency-ram-ledger`, `sunny-civet`).
  Replaced the BUG-003 v1 binary single-flight with a **RAM ledger**: each gateway reserves its
  estimated footprint (readable `residents/<pid>` metadata + lifetime-lock sidecar) before
  loading; admit iff sole OR
  `in_use + footprint ≤ total_ram × ROZUM_GATEWAY_RAM_BUDGET_FRAC` (0.65). The v1 "racy / needs
  PID-reap" objections are answered: reservation is up-front **under a brief admit lock** (no
  free-RAM-read TOCTOU), and liveness uses a **per-pid lifetime-lock probe** (same death-safety as
  v1, no kill-reaper). Footprint estimated caller-side from the catalog (core stays model-free). Spec § v2;
  4 unit tests + real-binary smoke; core 91/91, default+no-default green.
- [x] **admission-unknown-footprint-message** — DONE (2026-07-14, SPRINT `gw-spec-normalization`). The
  root cause was broader than cosmetic: the SLASH/`hf:` spec forms never matched the catalog's colon
  spec (exact `m.spec == spec` at 8 sites) → the sentinel fired for perfectly-valid specs. Fixed with
  `model_source::same_model` everywhere + consolidated the sentinel into `share::UNSIZEABLE_FOOTPRINT_*`
  and a short-circuit in `acquire_residency` (deny immediately, no 240s wait, no garbage "~N MB"; the
  CLI's "size UNKNOWN" message keyed on the same threshold does the talking).


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

- [x] prompt-policy - **DONE 2026-06-15** (decision, `docs/specs/prompt-policy.md`). The gateway is a
  **transparent provider**: it passes the client's own system prompt + messages through unchanged and
  does **not** inject per-model prompts (that would corrupt CC/Codex). Raw is the default and only
  mode; the lone shaping is the existing `--enable-thinking` toggle. Per-model style/persona lives in
  the caller (agent runtime's `system` arg / room etiquette), not the gateway. A per-model prompt
  registry is explicitly rejected — the transparent boundary is the feature.


## Project Hygiene

- [x] commit-initial-project - **DONE/N-A (2026-06-15).** The project is a live git repo with full
  history (this very work merges to `master` daily); the "commit the initial state" task is moot.

- [x] ci-smoke - **DONE 2026-06-15** (`.github/workflows/ci.yml`). Build + feature-free `cargo test
  --lib` on `master` push/PR (macos-latest, cargo cache). No model downloads; the real-model smoke
  tests stay opt-in (feature-gated + `#[ignore]`).

- [x] docs-bootstrap - **DONE 2026-06-15** (`README.md`). Refreshed the README with the LLM gateway /
  `rozum launch` / model-cascade quickstart (it was meeting-room only). Clone/submodule/build + first
  room + MCP proxy were already covered.


## Land the reactive-chat primitives in canonical scalascript (deferred "потом", 2026-07-22)

- [x] **chat-primitives-canonical — DONE 2026-07-23.** rozum deploy now rebuilds `chat.html` from
  `chat.ssc`. The SOURCE turned out to be already on scalascript `main` (`bade13ed5` fetchStreamSignal/
  intervalTick, `221c940f2` forJson — re-applied by another agent, so my cherry-pick came up empty). The
  real missing piece was STAGING: `bin/lib`'s bundled `signals.mjs` was the Jul-19 copy WITHOUT the
  fetchStream/forJson runtime, so `ssc-tools emit-spa` silently dropped the streaming wiring
  (`_mountFetchStream`/`data-ssc-fetch-stream-url` = 0). Patched the current `signals.mjs` into the staged
  `scalascript-backend-js` jar (bin/lib is gitignored/local) → canonical emit now reproduces the live
  working `chat.html` **byte-for-byte** (447071 B; JS clean; chat renders + `__chat` wired). rozum deploy
  `c8d0374` moves the `chat.ssc` emit into the always-run region so `UCC_SPA_ONLY` rebuilds it too;
  e2e 16/16 (3 env fails = sole resident model). scalascript claim released (`7849f33ef`).
  FOLLOW-UP — DONE 2026-07-23: ran a full `installBin` from `main` (`7849f33ef`) → bin/lib fully re-staged
  (ssc.jar + 129 runtime jars + compiler/native-front/providers, `[success]` 11s), replacing the signals.mjs
  hand-patch with the from-source runtime and fixing the JVM/interpreter path too. Re-verified: canonical
  emit of chat.ssc == live chat.html byte-for-byte; re-deployed index+chat, byte-identical (no regression);
  sbt server shut down after. Original plan kept below for reference.


## The meeting daemon has two owners — SEE `BUGS.md` BUG-025

- [x] **meeting-daemon-ownership — RESOLVED 2026-08-06.** BUG-025 is fixed in both halves (respawn loop `5ff78e7`, socket ownership `3bf39cd`); `doctor --services` reports `svc:meeting-daemon` as `ok` where it reported the split as `warn`. Left here as a pointer because the ninety-minutes-apart double filing is worth remembering. Original: filed here from `doctor --services` (launchd's copy down while
  `:8401` answered, because a bridge spawns its own on demand and wins the socket) about ninety
  minutes before a sibling agent filed the same thing, from the other end, as **BUG-025** (the job
  respawns every ~9 s forever because the daemon detaches, and two daemons can share one socket
  path). One problem, two symptoms: **BUG-025 is the record; this line is a pointer, not a second
  copy.** What this side adds is that `doctor --services` already reports the split state as `warn`
  with the reason, so whoever fixes it has a way to see it fixed.


## Parked worktree work — two patches rescued from worktrees that were about to be deleted (2026-08-04)

- [x] **parked-patches-decide — DECIDED 2026-08-05, both dropped** (see the verdict below; the checkbox was never ticked when it was). Original: during the 2026-08-04 worktree cleanup, two branches turned out to
  carry UNCOMMITTED work that would have died with their directories. Both were committed to their own
  branches and exported as patch files so they survive even a branch delete:
  `~/.rozum/attic/parked-patches/` (`0001-wip-gateway-the-codex-create-steer-*.patch`,
  `0001-wip-meeting-the-viewport-probe-*.patch`). **Decide what happens to each — that decision is the
  task; neither is in master.**
  1. **`feature/codex-gptoss-steer` `a1141f7` — codex create-steer** (78 lines in
     `src/gateway.rs`): `CODEX_CREATE_RECIPE` (append the `mkdir -p … && cat > PATH <<'EOF'` primitive
     to codex's prompt so a small model has ONE trivial file-creation shape instead of V4A, default-on
     via `ROZUM_CODEX_CREATE_STEER`) plus `CODEX_LEAN_PROMPT` (replace codex's ~21 KB `instructions`
     wholesale with a compact coding prompt, opt-in via `ROZUM_CODEX_LEAN_PROMPT`). Written against
     gpt-oss under codex — **both of which are now out of scope** (model not on disk, driver not in
     use), so the honest default is: leave it parked, and reconsider only if codex/opencode or a second
     model comes back. Counter-argument worth one read: the *lean-prompt* half is a general claim about
     prompt load collapsing small-model format adherence, and Qwen3.5-4B is a small model — if that
     generalizes, a lean prompt could help the model we DO run. Cheap check before deciding: does the
     claude path already send a small prompt? If yes, there is nothing to win and this closes.
  2. **`feature/ucc-web-live` `2b97edb` — the iOS viewport probe** (1 line in
     `clients/meeting/meeting.ssc`): `syncH()` overwrites the composer's placeholder with live
     `innerHeight`/`visualViewport`/`offsetTop`/keyboard numbers. **Not shippable as-is** — it eats the
     placeholder — and it is diagnostic scaffolding for the iOS keyboard-layout question, not a fix.
     Decide: either that question is settled (delete the branch, keep the patch in the attic as the
     record of how it was measured) or it is not (promote a real task that uses the probe, then removes
     it). Do NOT merge it as-is.
  **DECIDED 2026-08-05 — both DROPPED, branches deleted, patches kept in the attic.**
  1. **codex create-steer — dropped, and the interesting half turned out to be already shipped.**
     The create-recipe half is for gpt-oss under codex: neither the model nor the driver is in use.
     The lean-prompt half made a GENERAL claim worth checking — that prompt load collapses
     format-adherence in small models, and Qwen3.5-4B is small. Checked, and the lever already
     exists on the path we actually run: `rozum launch --lean` cuts the claude request from ~4.9K
     schema tokens to ~0.8K via `--disallowedTools` (`src/main.rs:332`). So there is nothing left
     to win here — the idea was right and was implemented before this patch existed.
  2. **iOS viewport probe — dropped.** Diagnostic scaffolding that overwrote the composer's
     placeholder with live viewport numbers. Nobody is chasing the iOS keyboard question, and it was
     never shippable. The patch in the attic is the record of how it was measured.
  Both patches remain at `~/.rozum/attic/parked-patches/` and apply cleanly; nothing is lost.


## Service liveness — we ship daemons with no health signal (found 2026-07-27, BUG-013)

- [x] **service-liveness-watch — PROMOTED to SPRINT 2026-08-05** after today produced three
  more instances of the same shape. Original text moved with it.


## CI: extend the portable-core gate to the whole workspace (2026-07-15)

- [x] **ci-workspace-portable-core — DONE 2026-07-16.** CI now builds every real workspace binary
  and tests every workspace library on macOS (shipped defaults) and Linux
  (`--no-default-features`). Windows builds the thin dispatcher and tests an explicit portable-core
  package allow-list; Unix daemon/control/service packages are named exclusions, not implied support.
  Real Actions run `29533946535` passed all three jobs. The Windows run also exposed and drove fixes
  for `.exe` dispatch, PID liveness, and locked residency queue/ledger metadata; those paths now have
  hosted behavioral coverage instead of a cross-compile-only claim. Spec: `docs/specs/ci-green-baseline.md`.


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


## Optional Model Adapters

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


## Native MLX runtime — performance (ports from the mistralrs work)

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
- [~] meetings-bridges-on-daemon — daemon-backed human web is DONE as `rozum meetings web`
  (`src/meeting/web.rs`). **Telegram/Discord DONE 2026-07-20:** the public thin commands now
  join existing rooms through `meeting.sock` as bridge clients, tail the canonical store without
  replaying history, suppress self-echo, validate external targets, enforce sender allowlists,
  and keep tokens process-scoped (`docs/specs/messenger-bridges-daemon.md`). Remaining cleanup:
  port the separate legacy `rozum web` escape hatch off the per-room socket so the old
  in-process room can eventually be retired.

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

- [x] portability-new-backend-checklist - **DONE 2026-06-15.** The "add a new runtime/hardware
  backend" recipe is written down — a concrete *Add-a-backend checklist* in
  `docs/specs/portability-and-the-backend-spi.md` (the 2 required `ChatBackend` methods + the opt-in
  hooks `concurrency_capacity`/`count_tokens`/`label`; bring your own template/tokenizer/cache; slot
  into `main.rs` builder + `config.rs` `ACCEPTED_ENGINES`; test feature-free). Folklore → checklist.

- [x] serving-loose-json-repair - **DONE 2026-06-16** (`src/serving.rs`). `parse_loose_tool_calls`
  now repairs a **malformed** `{"name":…}` when the strict path finds nothing: `repair_tool_object`
  does a single tolerant scan that disambiguates a content `"` from a structural one by lookahead (a
  `"` closes the string only if the next non-ws byte is `:` / `}` / `]` / EOF, or a `,` followed by
  the next key's `"`), escaping content quotes + raw control chars — so `println!("{}", x)` (incl. the
  `"{}"`-then-comma case) is recovered, not dropped. Runs only when the strict parse failed (no
  false positives). Validated: Coder-7B `build` now passes **with `ROZUM_MLX_CONSTRAIN=0`** (was a
  fail — lost the call). Known limit: a literal `","` inside content still defeats the heuristic.

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


## Deprioritised 2026-08-04 — the model is frozen on Qwen3.5-4B

- [x] **bugs-ledger-id-gate** — DONE 2026-08-05, `crates/rozum-core/src/bug_ledger.rs`. It is a LIB test, not a script: CI runs `cargo test --workspace --lib`, and a guard that does not run reads as coverage while providing none. Checks uniqueness, contiguity, newest-first order read from the FILE (not a sorted copy), and heading shape; the duplicate case is pinned in the exact shape it happened. While wiring it up: CI ran no BINARY tests at all — 64 of them, including the whole gateway bin suite — so `cargo test --workspace --bins` was added after verifying it green. Original entry:
  **Why:** two different bugs were both filed as `BUG-017` (nadia's jail, 2026-08-04; the meeting
  daemon's REST secret, 2026-08-05). Renumbering after the fact is the expensive kind of fix: the
  wrong number had already reached commit messages, a spec, the room and my own notes, and every one
  of those now points at somebody else's bug. `scripts/` has no doc gate and this checkout has no
  hooks configured (`core.hooksPath` unset), so the check has no home yet — that is the first thing
  to decide.
  **How:** a few lines over `BUGS.md` — ids unique, contiguous from 001, newest-first ordering
  intact, and every `## BUG-NNN` heading matching the expected shape. Cheapest home is the pre-push
  guard the worktrees install; the alternative is a `scripts/doc-gate.sh` called from `smoke-ci`,
  which also runs for people who never made a worktree.
  **Gotcha:** the ordering check must read the FILE order, not sort the ids — the whole convention is
  newest-first, so a sorted check would pass a file that was silently reordered.
  **Done when:** the gate fails on a deliberately duplicated id in a scratch copy, passes on `master`,
  and is wired somewhere that runs without being asked.

- [x] **matrix-queue-persist — DONE 2026-08-08.** The queue persists to `matrix-queue.json` beside
  the live panel, atomically, and loads on startup.
  **The decision, and it is the point of the change: a restart SETTLES unfinished jobs, it never
  resumes them.** `Queued`/`Paused` become `Stopped`, `Running` becomes `Failed`, terminal states are
  left as history. Restoring a queued job would start a matrix run nobody asked for, unattended,
  minutes after a reboot — and the matrix has taken this host down twice (BUG-001, BUG-003). So what
  persisting buys is that the panel stops lying after a restart, and that another process can read
  the queue; resumption is a separate feature with a separate risk and should be asked for.
  **Shape worth keeping:** mutation goes through `with_queue`, which takes the lock, mutates and
  persists in one breath. There were six mutation sites and I missed one on the first pass — a
  helper makes "mutate without saving" hard, where a convention only makes it discouraged.
  The policy is split into `settle_after_restart(jobs, now)` so it is testable without a clock or a
  file: a decision only exercised through I/O is one nobody re-checks.
  **Why:** `matrix_queue()` is a `OnceLock<Mutex<Vec<MatrixJob>>>` with no write path to disk
  (`crates/rozum-gateway/src/matrix.rs:170`), while its sibling `matrix_live()` IS file-backed
  (`persist_matrix_live` / `load_matrix_live_from_disk`). Two consequences, and the second is the
  one that bites: a gateway restart silently forgets every queued job, and no other process can
  read the queue — which is what stopped `/control/public/matrix` from moving to the .ssc server in
  `ucc-ssc-backend` slice 1.
  **How:** the same shape as `matrix_live` — persist on mutation, load on startup, with a stale
  guard. The live panel already carries `LIVE_STALE_SECS`, so the precedent for "ignore state older
  than X on startup" exists in the same file.
  **Gotcha:** a queued-but-never-run job resurrected after a restart would start a matrix run
  nobody asked for. Persisting the queue means deciding what a restart does to `queued` entries —
  probably drop them and keep only what was running, which is the conservative direction.
  **Done when:** a gateway restart preserves the queue, a second process can read it, and a
  deliberately stale queue file does not launch anything.
