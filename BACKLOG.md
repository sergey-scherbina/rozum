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

## Rescued from the parked bucket (triage 2026-08-08)

Both were moved into *Deprioritised — the model is frozen* on 2026-08-04, and neither carries a
`Parked because` line, because neither depends on a model. Sorting by section rather than by item
is how that happens.

- [ ] **test-cell-repair-failfast** (LOW, from B) — when a repair attempt hits an Edit-before-Read churn
  loop it burns the whole RUN_TIMEOUT (rc124) without converging; `repair_tool_protocol_hint` fires one
  attempt too late (loop is in the FINAL attempt). Lever: detect the churn live and fail-fast, and/or grant
  ONE bonus repair attempt AFTER the protocol hint is first triggered so the hint can actually apply.
  Note: harness already feeds the whole-file `repair_benchmark_recipe` ("replace the file, don't use Edit")
  on repair — Devstral ignores it, so this is bounded by model compliance, not just harness logic.

  *(was under: Matrix improvement levers (found 2026-07-05 during the matri)*

  *(this entry existed TWICE — in “Matrix improvement levers” and again in “Deprioritised”, because 2026-08-04 copied instead of moving. Merged 2026-08-08.)*
## rozum-core::share tests read the real machine (found 2026-08-05)

- [ ] ~~**share-tests-isolate**~~ — `cargo test -p rozum-core share::` fails on master right now: 7
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

- [ ] **mtg-incident-repro** — capture the workdir/repro alongside an incident. Split out of
  `mtg-incident-context` deliberately: copying files out of a working tree needs a policy about what
  may leave it (secrets, size, .gitignored state), and inventing one inside an incident feature is how
  a support tool grows a data-export problem. Wants a spec of its own before any code.

- [ ] **rozum-json-surface** — `rozum models list` / `models info` have no `--json`, and they are
  what `/control/model/info` needs to move to .ssc (`docs/specs/ucc-ssc-data-seam.md`). Small, and
  useful to every script on this machine, not only the port. `rozum gateway status --json` already
  exists and is the model to copy.

## Model chain (verification-gated, `--model A,B,C`)

The CORE chain shipped on master (spec `docs/specs/pipeline-cascade.md`, SPRINT top item): target
derivation (single + multi-model), deterministic verify-gate + repair, escalation across links, role-aware
quality stats + auto-exclude, cloud-last by ordering, backend planner/executor/verifier roles. These are
the deferred follow-ups (operator-triaged 2026-06-24, none urgent):

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

- [ ] model-catalog-refresh - Expand and verify tiny model catalog.
  - Include current small Qwen/Gemma/Phi candidates with exact file sizes.
  - Record license and expected strengths.

- [ ] benchmark-baseline - Record latency, disk size, and smoke eval score for each backend/model pair.
  - Use the eval harness once available.

- [ ] distillation-plan - Design a later LoRA/QLoRA or distillation path.
  - Do not implement until evals provide a baseline.

# BLOCKED — on another repository

## The meeting PWA cannot be rebuilt from source (found 2026-08-08)

- [ ] **meeting-ssc-unbuildable** — `~/.local/bin/rozum-meeting-ssc` serves `:8405` (phone, over
  Tailscale) and dates from 2026-06-29, because `ssc-tools build-rust clients/meeting/meeting.ssc`
  fails today inside the STANDARD LIBRARY: `jsonCoreRenderFields extracts Cons which is not a known
  enum constructor`, `_normSegments uses unsupported infix operator ::`. **The running binary is the
  only artifact** — a service whose source no longer compiles cannot be fixed when it breaks, and
  that is the actual risk, not the staleness. Reported upstream by their procedure
  (`scalascript:INBOX.md` `build-rust-std-json-cons`, room post). Nothing to do here until it lands;
  `scripts/install-bins.sh` now tries, refuses loudly with the compiler's own words, and leaves the
  working binary alone.
  **2026-08-08 — still true, and now linked to two more reports.** Re-checked after rebuilding their
  toolchain twice today: the build still fails, and the FIRST error is
  `Term.ApplyUnary (!jsonCoreIsLowSurrogate(low))` — the same unsupported unary `!` on a call that
  blocked rozum's own `public-matrix.ssc` (`ucc-ssc-backend` slice 1). So this entry,
  `join-works-under-build-rust-not-run` and the two divergences found writing that file are ONE
  seam: `build-rust` does not lower real programs. Said so in their room rather than filing a fourth
  report, so the three do not get routed to three modules as separate work.

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

# PARKED — what would revive it

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

- [ ] **codex-opencode-create-delivery (original notes)** — see BACKLOG
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
- [ ] **mlx-glm4-moe** — port GLM-4 MoE to native MLX. **REPRIORITIZED → bigger than thought**
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

- [ ] **plugin-wireprotocol** — make the agent wire layer a real `WireProtocol` trait
  (Chat / Messages / Responses impls). Supersedes the arch-spi "map, not trait" call —
  full plugin-ization is the goal. **(See TRIAGE above — re-scope + re-decide before starting.)**

**Parked because:** Its own TRIAGE already decided this out-of-scope pending an operator override.

- [ ] **plugin-services** — services (gateway / web / meetings / bridges) behind a plugin
  registry instead of `Command` match arms. **(See TRIAGE — decided out-of-scope; needs override.)**

**Parked because:** its own TRIAGE decided it out of scope pending an operator override — NOT for
  the Vulkan/x86 reason that stood here until 2026-08-08, which belonged to a different item and was
  copied onto this one.

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

- [ ] **codex-create-delivery-on-qwen** — does the `apply_patch` bridge land codex's CREATE forms
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

- [ ] **shared-checkout-guard** — `AGENTS.md` forbids feature work in the shared checkout; nothing
  enforces it, and I broke the rule three times in one session after writing it up twice.
  **Evidence, all 2026-08-06..08:** (1) a `scripts/rust-item-spans.py` fix left uncommitted there was
  swept into a sibling's `claim:` commit and published; (2) earlier the same happened to a
  `rest_read.rs` change, which landed inside `beace56`; (3) a third time the cwd reset after a
  `cd /tmp` and half a change landed in the shared tree while the other half was in the worktree.
  Each time the rule was known, written down, and re-affirmed — the failure is mechanical, not a
  matter of remembering harder.
  **The precedent works and is next door.** scalascript's pre-commit refuses any staged path outside
  `.work/` in the shared main checkout, names the offending paths, and prints the worktree command.
  It stopped me the first time I tried to commit an `INBOX.md` entry there — the same class of
  mistake, caught instead of published.
  **How:** a `pre-commit` hook + `core.hooksPath` (this checkout has NONE configured today — that is
  why nothing catches anything). Allow `.work/**` plus the board files this repo treats as
  coordination — `SPRINT.md`, `BACKLOG.md`, `BUGS.md`, `CHANGELOG.md` — and refuse the rest with the
  `git worktree add` line spelled out, as theirs does. `--no-verify` stays the escape hatch.
  **Gotcha:** the hook must be installed for every worktree too, or it only guards the place that is
  already hardest to get wrong. `core.hooksPath` set at the repo level covers worktrees; a hook
  dropped into `.git/hooks` does not.
  **Done when:** a staged `src/**` change in the shared checkout is refused with the worktree
  command, a claim/board commit still goes through, and a worktree commit is unaffected.\n
- [ ] **doctor-deployment-drift** — `doctor --services` says every service is healthy while the
  installed binary is three days behind master, and nothing says so.
  **Why:** the deployed `~/.cargo/bin/rozum-gateway` fell behind `master` three times in two days
  (2026-08-07..08). Each time it was caught by hand, and once it meant a feature was "shipped" for a
  day while the running daemon had never heard of it. Every other kind of drift in this repo now has
  a check; this one — the gap between what is merged and what is RUNNING — has none.
  **How:** stamp the build (`git rev-parse HEAD` at compile time via a build script or an env var
  baked in) and have `doctor --services` compare it against the checkout's `HEAD`, reporting
  "deployed N commits behind" as a `warn`. `warn`, not `fail`: being behind is normal between a
  merge and a deploy, and a red that is usually red gets ignored.
  **Gotcha:** the check must compare against `origin/master`, not the local checkout, or a stale
  local clone reports itself perfectly up to date.
  **Done when:** a deliberately stale install is reported with its distance, a fresh one is silent,
  and the report survives a checkout that is itself behind.
