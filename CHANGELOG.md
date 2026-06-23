# Changelog

## models/gateway — a control-API snapshot (UCC data layer, no scalascript dep)
Completed: 2026-06-23

The models/gateway half of the UCC control-API (the meetings half shipped earlier as
`rozum-meeting::client`). New `rozum-gateway::control::status()` aggregates a coherent snapshot — the
active shared gateway (model/port/pid/uptime/clients/health), host residency (RAM budget / committed /
available / the resident set), and the installed model catalog — into one `Serialize` `ControlStatus`.
Surfaced as `rozum gateway status --json` (the machine/dashboard contract; live-verified). Adds a
read-only `share::list_residents()`. This is the data layer the future UCC dashboard binds to (and the
same snapshot can be served over the gateway's HTTP surface) — buildable now, parallel to and
independent of the scalascript `frontend/tui` backend. 117 core + 81 gateway tests green. Spec
docs/specs/services-and-clients.md.

## meetings — REST parity: inbox + roster over HTTP
Completed: 2026-06-23

The daemon's `rest_read` axum surface gains `GET /rooms/{name}/inbox/{handle}` (turns addressing a
handle, via `client::inbox`) and `GET /roster` (the live agent principals, via `client::roster`) — so a
remote/web client (and the future UCC web target) can fetch the same operations as JSON instead of
shelling out or reading the disk format. End-to-end test for the inbox endpoint. (POST-over-HTTP is
deferred — the socket submit path + auth is a separate write task.) Spec docs/specs/services-and-clients.md.

## meetings — client API write-side (identity/status through one contract)
Completed: 2026-06-23

Second increment of the meetings client API. `rozum-meeting::client` now also owns the write/identity
operations: `post_identity` (the single agent-vs-human posting rule, was inline in the bin), `whoami`
(an `Identity` enum), `establish` (hello), and `daemon_status`. The `rozum` binary's post/whoami/hello/
status handlers are thin presentation over them — no identity/daemon logic left in the bin. 80 meeting
tests green; live whoami/status verified. With the read side (prior commit), the CLI is now fully thin
over the client contract — the seam web/TUI/UCC consume next. Spec docs/specs/services-and-clients.md.

## admission — lower the keep-free margin 3→2 GiB (validated)
Completed: 2026-06-23

The default `min_free` keep-free margin (the headroom the residency gate preserves above a model's
footprint) is lowered 3→2 GiB. It used to be the *leading* buffer for the prefill spike; that role is now
covered structurally: improvement A folds each model's REAL measured peak (incl. the spike) into the
footprint estimate, and improvement B refuses at admission under kernel memory-pressure. The host-wide
ledger — not this margin — is what blocks concurrent overcommit (the 2026-06-22 reboot was a ~25 GiB
overcommit no keep-free size would have gated). So keep-free is now just the single-load external-growth
cushion, and 2 GiB suffices. Net: ~1 GiB less required per model, so the weight-bound 35B / GLM-32B fit.

Live-validated and the footprint cache seeded in one pass: gpt-oss, then GLM-4-32B (free → 7.5 GiB) and
Qwen3.6-35B (free → 8.8 GiB) each loaded at keep-free=2 with a heavy prefill request — kernel pressure
stayed Normal throughout, 0 reboot (a pressure auto-abort-on-critical guard was armed and never fired).
The three real peaks now seed improvement A: dry-run estimates drop gpt-oss 21.94→17.44, GLM 24.49→19.99,
35B 24.94→23.46 GiB. Caveat: validation never drove free to the exact 2 GiB floor (smallest was 7.5), so
the boundary rests on the A+B+ledger reasoning; override with `ROZUM_GATEWAY_MIN_FREE_RAM_BYTES`.

## meetings — a client API (clients stop knowing the storage format)
Completed: 2026-06-23

First increment of the services-and-clients architecture (operator: failure isolation + one unified
client + cleanliness). New `rozum-meeting::client` module is the single contract for room operations —
`resolve_room_root`, `read`, `inbox` (+ the `InboxCursor`), `roster` — so a client consumes *operations*
rather than parsing the on-disk jsonl / principal / cursor files. The `rozum` binary's `read`/`inbox`/
`who` handlers are now thin presentation over it (the inline disk parsing is gone from the bin); the
storage format is internal to the crate. Behavior-preserving (80 meeting tests green; live read/inbox/who
verified). This is the seam the web `.ssc` + the Rust TUI + the future UCC client will consume, and the
same operations can be served over HTTP (the daemon's existing `rest_read` axum surface) for remote/web.
Models↔meetings were already separate services (separate crates, no code dep, coupling only via the
gateway HTTP API) — kept as-is. Spec `docs/specs/services-and-clients.md`.

## admission — tighten the footprint estimate toward the measured real peak
Completed: 2026-06-23

The residency admission estimate was conservative by design — weights + KV + a fixed activation reserve —
so it OVER-refused models that would actually fit. It now tightens toward each model's REAL measured peak:
the MLX backend's `Drop` records `get_peak_memory()+get_cache_memory()` (a process-global high-water mark)
into a small running-MAX cache (`~/.local/state/rozum/gateway/footprint-peaks.json`, keyed by model), and
`estimate_model_footprint_bytes` returns `min(conservative, max(weights+KV+1GiB, peak+margin))`.

Safe by construction (this is why it shipped only after live validation): a peak measured under a LIGHT
request (short prompt → little KV) does NOT bound a future full-context load, so the estimate is floored at
the TARGET n_ctx's full weights+KV plus a fixed 1 GiB scratch reserve — a light measurement can never
under-provision; the measured peak can only push the estimate UP toward the conservative upper bound,
never below the safe floor. keep-free and the kernel pressure-guard backstop the remainder. Opt-out
`ROZUM_GATEWAY_MEASURED_FOOTPRINT=0`; surfaced in `gateway --dry-run` ("measured peak: …").

Live-validated on gpt-oss (real load + request + graceful stop recorded an 11.16 GiB peak): the dry-run
estimate dropped 17.53 → 16.03 GiB (−1.5) with the lever vs without, and the floor correctly ignored the
unrepresentative light peak (kept full KV). Two bugs caught in validation and fixed: a key mismatch
(backend `model_id` slash vs CLI spec colon) and an n_ctx-exact key that adaptive loading's drifting n_ctx
never hit (now keyed by model only). 3 footprint unit tests + 117 rozum-core tests green.

## admission — kernel memory-pressure guard (third no-reboot lever)
Completed: 2026-06-23

The residency admission gate now consults the kernel's OWN memory-pressure level
(`kern.memorystatus_vm_pressure_level`, via the existing `shed::read_host_pressure()`) as a third lever
beside the cross-process ledger and the free-RAM check: a load is refused if the host is already at
warn/critical, independent of the byte arithmetic (which can read "fits" moments before pressure spikes —
the kernel computes availability better than page math, and the `shed` runtime watchdog already keys on
this same signal). Safety-only: it can only ADD refusals (never rescues a byte-over-budget load) and
fail-safes to Normal on an unreadable level, so it never blocks spuriously.

This is the safe outcome of investigating "should we count reclaimable-active memory in `available`?":
measuring the vm_stat page classes proved our `available` (free+inactive+speculative+purgeable) already
captures all reclaimable-without-swap memory — the uncounted "active" pool is ~100% anonymous (reclaiming
it needs swap/compression = the jetsam→reboot path), so excluding it is correct. We do NOT loosen
`available`; we add the kernel's pressure signal. `admits()` gains a `pressure` param (kept pure),
wired into `acquire_residency` + `dry_run_admission` (+ `AdmissionReport.pressure`), and surfaced in
`gateway --dry-run` ("host pressure: normal/warn/critical"). Unit test
`admits_refuses_under_elevated_pressure`; 115 rozum-core tests green. (The complementary
`footprint-estimate-accuracy` lever — tighten the over-conservative estimate via measured peaks — is
filed in SPRINT as validation-gated, since under-estimating risks the reboot it must prevent.)

## meetings — show identity names (drop "· animal") + auto-hello instruction
Completed: 2026-06-23

The two follow-ups to the Human/Agent identity work. (1) The transcript author label is now the
participant's **identity name** — `Sergiy`, `sunny-civet` — not `Sergiy · plucky-fox`:
`identity::display_name` (used by `room.rs::display_for`) returns the base identity, keeping the minted
handle internal (uniqueness) and falling back to it only for an un-named client. So a human never looks
like an agent, and an agent shows its own name. Takes effect on the next daemon restart; the roster
stores base+handle separately and recomputes the label live, so existing participants get clean labels
too. (2) `AGENTS.md` now instructs every agent to run `rozum meetings hello <your-handle>` first thing in
a session, so it posts as itself (not the operator) by default — the operator does nothing. 80 meeting
tests green.

## gateway — `--dry-run`: show the adaptive-load fit + admission verdict without loading
Completed: 2026-06-23

`rozum gateway --model <m> --dry-run` reports how a model WOULD load at the current free RAM — the
adaptive `n_ctx`/cache shrink it would pick and the host-RAM admission verdict (would-load / would-load-
reduced / would-refuse, and by how many GiB) — then exits WITHOUT loading anything. It reuses the EXACT
load-path math (`fit_model_params` + `estimate_model_footprint_bytes` + a new non-mutating
`share::dry_run_admission` that mirrors `admits` over the same ledger/RAM inputs without taking the admit
lock), so a real run does exactly what the dry-run reports. Purpose: plan a matrix run (which models fit,
at what n_ctx, how much RAM to free) with zero load risk, and make the no-reboot guarantee legible — the
output spells out that a refusal is a clean exit before any weights load (a matrix FAIL, never a reboot).

Example (36 GB host, ~25 GiB free): `Qwen3.6-35B-A3B-4bit` → adaptive ↓ n_ctx 262144→35840, cache 1 GiB,
footprint 22.21 GiB → ✅ would load; `gpt-oss-20b` → ✓ full n_ctx 131072, cache 4 GiB, 21.94 GiB → ✅.
Confirms adaptive loading is ON by default (opt-out `ROZUM_GATEWAY_ADAPTIVE_LOAD=0`). rozum-core admits +
rozum-models fit tests green; release builds clean.

## meetings — clean Human vs Agent identities + a `who` roster
Completed: 2026-06-23

Puts the identity model in order. Before: agents called `meetings post` without identifying, so they
inherited the ONE machine-local identity (`$USER · <animal>`) — the operator — and every agent showed up
as the same `Sergiy · plucky-fox`, with the real handle only in free-text. Now there are two distinct
principals that never mix: the **human** is the account/login identity, and each **agent** has its OWN
name, assigned ONCE at session start.

New `meeting::agent_identity`: a per-session Agent principal keyed by `$CLAUDE_CODE_SESSION_ID` (the
stable env key — there's no tty, and shell env doesn't persist between calls, so it lives on disk).
`run_meetings_post` resolves identity as `--as`/`$ROZUM_MEETING_AS` → the session's Agent principal →
the human — so an agent always posts as itself and a bare shell always as the operator. New commands:
`rozum meetings hello [<name>]` establishes the identity once (idempotent; mints a stable name if none
given; emits a terminal-title escape), `meetings whoami` reports agent-vs-human, and `meetings who` is a
roster mapping each live handle to a findable session (worktree/cwd, age, liveness) — so a meeting
mention maps to "which window is that". Realizes the Agent side of the `Principal` model from
`docs/specs/agent-meeting-coordination.md`; spec `docs/specs/meeting-identity-roster.md`. 80 meeting
tests green. Follow-ups: drop the human's `· <animal>` mashup; auto-`hello` at session start.

## meetings — wakeup push flags "for you" mentions (Tier-1/3)
Completed: 2026-06-23

Completes the mention-inbox work on the PUSH side. The `daemon_proxy` wakeup pusher now checks whether
each new room delta (from someone else) addresses the proxy's own handle (`@you` / `-> you`, via
`mention::addresses`) and, when it does, sets `mentioned`/`your_turn` on the `notifications/claude/channel`
event and prefixes the Tier-3 piggyback injection with `‹for you›`. The proxy instructions teach the agent
that `mentioned="true"` means the message addresses it (prioritize), and point at
`rozum meetings inbox --as <handle>`. So a connected agent is now told "this one is for you" instead of
treating every room turn as equal chatter; the durable pull inbox covers the offline case. 78 meeting
tests green.

## meetings — `inbox`: durable "messages that address you" (mention detection)
Completed: 2026-06-23

Addressing a sibling in the room (`-> plucky-fox`, `@nimble-raven`) was convention, not delivery — the
target learned it only by re-reading the room, and the push ladder is dormant when no proxy is connected
(measured: posts landing in 0 piggyback drops). New `rozum meetings inbox --as <handle>` makes it a
durable, offline-surviving pull: a view over the room transcript filtered to turns that address your
handle, past a per-handle seen-cursor on disk (`<room>/.inbox/<handle>.json`) — so even a CLI-only agent
with no live proxy sees "addressed to me, unread". Reading advances the cursor; `--peek` doesn't, `--all`
ignores it. The room stays the single durable record (a read mention is never lost — re-findable with
`--all`).

Detection is a new pure `meeting::mention` module (`addresses(content, handle)` for `@h`/`-> h`,
boundary-checked so `-> plucky-foxtrot` ≠ `plucky-fox`; `known_handles`/`mentions` as a secondary helper).
Live finding baked into the design: `display_name` is not a reliable handle source here (agents post under
a shared local identity and self-identify in content), so the inbox trusts the agent's own `--as <handle>`
rather than gating on display-name-derived handles. Spec `docs/specs/meeting-mention-inbox.md`. 7 mention
unit tests + 78 meeting tests green; `meetings read` refactored onto a shared `resolve_room_root`. The
push-side `mentioned` flag (Tier-1/3 wakeup) is the remaining follow-up (`wakeup-mentioned-flag`).

## gateway — repair `cat PATH <<EOF` missing-`>` (gpt-oss lost-write build red)
Completed: 2026-06-23

Live autopsy of the codex×gpt-oss `build` reds (KEEP=1, run OzUnnR): gpt-oss writes the CORRECT final
`src/main.rs` but delivers it as `cat src/main.rs <<'EOF' … EOF` **without the `>` redirect**. Without
`>`, `cat` takes the path as a positional arg and **ignores stdin** (the heredoc) — the write is a
silent no-op read, the file never lands, the earlier broken version stays on disk, `cargo run` prints
nothing → build red. Since the matrix grades `build` by FINAL FILE STATE (it runs cargo itself),
landing the correct code makes the cell pass even when the model never re-ran cargo.

Fix: `repair_heredoc_write` in `crates/rozum-gateway/src/gateway.rs` rewrites `cat <path> <<DELIM` →
`cat > <path> <<DELIM` when there is a real path arg, a heredoc body, and no existing redirect —
`cat <path> <<DELIM` is never a meaningful command (cat discards the heredoc given a file arg), so the
write-intent is unambiguous. Heredoc-aware (tracks the delimiter so body lines starting with `cat …`
are never rewritten); spares `cat > x`, plain `cat x` reads, and stdout `cat <<EOF`. Wired into
`normalize_codex_tool_args` (the codex tool-call path), gated `ROZUM_HEREDOC_REDIRECT_FIX` (default on).

Unit-proven on the exact OzUnnR input + 5 negatives (`heredoc_redirect_repairs_missing_gt_and_spares_valid_forms`),
80/80 gateway tests green. Live-validated codex×gpt-oss (REPS=4): the repair fires 2× in the real flow,
`test` 3/3→4/4 (no regression), the lost-write failure mode is eliminated — the residual build reds are
now a SEPARATE class (model emits non-compiling final code and declares done without re-running cargo;
filed as `gptoss-verify-before-done` in SPRINT).

## meeting client — unread badges in the room-switcher
Completed: 2026-06-23

The `.ssc` meeting web now shows unread counts per inactive room in the room-switcher `<select>`:
each option renders as `name (N)` when that room has messages you haven't seen. A new `GET /u` route
returns `name|count` lines (per-room message count via `readRoom`, counting `"content":"` lines — the
same source the active-author chips use). The client polls `/u` every 5 s, keeps a per-room last-seen
map in `localStorage` (`rozumSeen`), badges the unread delta, always marks the current room read, and
treats the very first load as all-read so it doesn't show everything as unread. Additive: no server or
schema change, the existing chat/poll path is untouched.

Built with the current `ssc` toolchain (the `5408689` `.map`/list-index lowering fix on
scalascript `origin/main`) and validated live on :8405 — `/u` returns the counts, the page still
renders 200, the room-switcher shows the badges.

## perf-baseline — correct two lever calls (verify-before-build on the spec itself)
Completed: 2026-06-23

Git-history check corrected two claims in the just-filed perf-baseline spec/tasks, so the next agent
doesn't burn a slot-session on a wrong premise:
- **perf-compiled-decode is NOT the open structural lever — it's on-ice.** Commit `f6b20a3`
  (2026-06-22) already ran `mlx_compile_probe_plain` on Qwen3-0.6B and found compiled decode SLOWER
  (T=1 0.69×, T=16 0.58×), matching the `compile_with_state` net-negative; decision was "don't build
  Stages 1/2 — batching was the real lever". Only the 27B / fixed-shape-cache caveats remain
  (low-confidence). Don't re-run the answered 0.6B probe.
- **perf-batch-default-on is not a free flip.** With `ROZUM_BATCH>1` a *lone* request waits the full
  `batch_window_ms` (10) in the gather loop before discovering it's alone → a single-agent TTFT tax
  with no benefit. Added a prereq task **perf-batch-gather-shortcircuit** (skip the window when no 2nd
  job is queued/admitted) that must land — and its scheduler tests pass — before flipping the default.
  Batched==serial correctness itself is already well-covered.

## perf-baseline — code-grounded micro-perf lever audit (prep; run is slot-gated)
Completed: 2026-06-23

Analysis half of `#3 Micro-perf → perf-baseline`, done without the host model slot (held by the
matrix). New spec `docs/specs/perf-baseline.md` + concrete `perf-<lever>` tasks in `SPRINT.md`.

Key finding from a code audit: 2 of the 4 candidate levers are **already realized** — cross-turn
prefix-cache reuse is DONE for the mainline serving path (LRU `PrefixStore`, longest-prefix
truncate + suffix-only prefill, default-ON, byte-exact) and the KV cache layout is DONE
(pre-allocated 256-block in-place cache, no per-step O(context) concat). async_eval pipelining +
retained command buffers also done. So the open levers are not a from-scratch build:
**perf-batch-default-on** (continuous concurrent-request batching is built + wired but ships off —
`ROZUM_BATCH` default 1; benches prove ~1.98× at B=2 → validate-and-flip), **perf-compiled-decode**
(decode is ~92% CPU graph-build; the structural fix is a compiled fixed-shape decode graph, go/no-go
via the existing plain-`compile` probe), plus batch arch-coverage (GLM-4/gpt-oss), prefix-reuse for
the plookup/spec-decode fast-paths, non-batchable-row batching, and a KV ctx-sweep verification. The
measurement tooling already exists (`scripts/bench/run.sh` + the in-code `#[ignore]` throughput
benches); the spec gives the run plan + per-model targets for when the slot frees.

## meeting .ssc client — rebuild against current Rust backend
Completed: 2026-06-23

Rebuilt and reloaded the live launchd meeting client from current source. The rebuild exposed
two ScalaScript/Rust backend edge cases in the client source: filtering room status lines through
`roomLineName` passed a borrowed `String`, and static `sw.js`/`icon.svg` route closures moved
captured strings out of `Fn`. The client now uses a small recursive room-line lookup and returns
static assets through helper functions. Live smoke on `:8405` passes for `/`, `/manage`,
`/r/rozum`, `/m/rozum`, `/mp/rozum`, `/manifest.webmanifest`, `/sw.js`, and `/icon.svg`.

## gateway — restore the apply_patch (edit) protocol in the lean prompt (fix codex×gpt-oss debug)
Completed: 2026-06-23

A full codex×gpt-oss measurement showed the aggressive instruction-trim — great for CREATE
(build/test 0/3 → ~2/3 via `cat >`) — had REGRESSED the EDIT cell: `debug` (fix a file via
apply_patch) went 1/3 → 0/3 with a 1.3 GB runaway loop, because the trim dropped codex's V4A
`apply_patch` format spec so the model couldn't form a valid edit patch. Added a concise V4A
reminder to `LEAN_CODING_PROMPT` (`*** Begin Patch / *** Update File / @@ / -/+ / *** End Patch`,
~0.3 KB — far below the load threshold). Validated: `debug` **0/3 → 3/3** (63/113/62 s, no loops);
`build` 1/1 (no regression). Net codex×gpt-oss ≈ **1/9 → ~7/9** (build ~2/3, test ~2/3, debug 3/3),
2-5× faster. Lesson: a single lean prompt must cover BOTH create (shell) and edit (apply_patch).
Spec `docs/specs/constrained-gptoss-delivery.md`.

## residency — shared-reserve admission billing (in-process multislot admits more)
Completed: 2026-06-23

Completes nimble-raven's shared-reserve handoff on the admission-mechanism side. The in-process
`plan_residency` planner no longer sums a full per-model activation reserve (~5.5 GiB) for every
co-resident — the MLX buffer cache is a single process-global pool and prefill serializes, so only
ONE reserve is physically real per process. `ResidentRequest` gains a `process_reserve_bytes` input;
the planner bills each model's `weight − reserve` (its genuine `runtime_active_bytes`, the caller
still passing full footprints) against `budget − one reserve`. On a 36 GiB host (~27 GiB budget)
this is the difference between admitting a 2nd small co-resident or needlessly refusing it.

Reboot-safety preserved: the call site still passes full `runtime_footprint_bytes`, so the values
flowing to the cross-process `published_reservation` ledger keep their reserve — only the in-process
admit decision is relaxed. The reserve is injectable (`WarmConfig.reserve`: production
`process_reserve_bytes(0)`, reserve-less test stubs `0`), keeping it consistent with the weight
model. Provably single-model-identical (`req − reserve ≤ budget − reserve` ⇔ `req ≤ budget`).
Tests: two `resident::tests` units + an end-to-end `warm_admits_a_co_resident_by_counting_reserve_once`.
Spec: `docs/specs/safe-multi-model-residency.md` § Shared-reserve accounting.

## gateway/mlx — honour the client's reasoning.effort (codex can now control gpt-oss reasoning)
Completed: 2026-06-23

Previously the gateway dropped codex's reasoning entirely (`RespReq` had no field), so gpt-oss
always ran at the engine default. Now the OpenAI Responses `reasoning.effort` is honoured:
parsed (`reasoning_effort_of`), carried on `SamplingParams.reasoning_effort` (a Default field — no
constructor churn), and applied at the gpt-oss harmony render via a per-job thread-local
(`REQ_REASONING`, set at `run_job`; gpt-oss tool jobs are constrained → serial, so it's exact).

- **Precedence: request `reasoning.effort` > `ROZUM_GPTOSS_REASONING` env > `low` default.** Other
  models ignore it. `reasoning_effort_of` + the parse/precedence are unit-tested; gateway 77/77,
  `rozum-mlx --features mlx-native` compiles, `--no-default-features` green.
- Validated: (1) honouring works — a direct `/v1/responses` probe with `effort:high` reasons more
  (slower) than `effort:low`; (2) NO regression — a real codex×build ran fast (37 s) with the env
  unset, i.e. codex does NOT send `reasoning:medium`, so the `low` default still holds. An agent can
  now raise the effort explicitly while the coding default stays fast. Spec
  `docs/specs/constrained-gptoss-delivery.md`.

## meeting .ssc client — harden room paths and status docs
Completed: 2026-06-23

The shipped ScalaScript/Rust meeting web no longer assumes the operator's absolute
`~/.local/state` path for room transcripts. It resolves project rooms from
`rozum meetings status` to `<project>/.rozum/room` and resolves global/ad-hoc
rooms through `$XDG_STATE_HOME/rozum/rooms` (falling back to `~/.local/state`).

The meeting client docs and sprint tracker now match the implementation:
dynamic room selector/prefix routes, `/manage` room cleanup, model list/rm,
gateway status/switch/stop/unload, and per-room model-participant start/stop
are marked shipped. Generic interactive `rozum launch` from the web remains
deferred until there is a non-TTY supervisor contract.

## mlx — gpt-oss reasoning effort default `low` (the residual-timeout cause)
Completed: 2026-06-23

WHY gpt-oss reasons long (the residual ~1/3 codex×gpt-oss timeouts): its harmony chat template
defaults `reasoning_effort = "medium"` *"if not defined"*, and `ApplyChatTemplateArgs` never passes
it → gpt-oss always gets **`Reasoning: medium`** and emits a substantial `analysis` chain-of-thought
before EVERY tool call; the multi-turn loop accumulates into RUN_TIMEOUTs. (`enable_thinking=false`
is unrelated — it governs how *prior* messages' `thinking` render.) The reasoning is productive, not
a loop — it scaled 3-5× with context (the instruction-trim), which a loop wouldn't.

- `apply_reasoning_level` (in `sanitize_chat_template`) rewrites the template's `reasoning_effort =
  "medium"` default to `ROZUM_GPTOSS_REASONING` (`low`|`medium`|`high`, **default `low`** — rozum runs
  gpt-oss for agentic coding where medium CoT is wasted on simple tasks). String-substitution on the
  template (no fork rev-bump); `medium` is a no-op; non-harmony templates untouched. Pure
  `apply_reasoning_level` unit-tested; `--features mlx-native` compiles + the existing suite green.

## gateway — codex instruction-lean for load-sensitive models (help gpt-oss deliver)
Completed: 2026-06-22

A controlled load bisection (direct to gpt-oss) isolated WHY a capable model can solve a task
but not deliver it under codex: the dominant breaker is **context SIZE, not the V4A format**.
With the easy `write_file` tool, a 30 KB system prompt drops gpt-oss to **0/3** (it emits empty
content, no tool call); a ~20-byte prompt is **3/3**. The V4A format (1/3) and tool count (2/3)
are secondary. Codex's real request: **instructions = 20.9 KB + input = 13 KB + 18 tools** — and
`codex_lean_keep` trimmed only the TOOLS, never the 20.9 KB instructions (the dominant load).

- `codex_effective_instructions(model_id, original)` in `responses_handler` replaces codex's
  instructions with a short focused `LEAN_CODING_PROMPT` **only for load-sensitive models**
  (`model_is_load_sensitive` = gpt-oss). The capable tier (Qwen3.6-35B, fine with the full 21 KB)
  keeps codex's instructions **verbatim → no regression by construction**. The kept tool schemas
  carry the arg shapes, so a short prompt suffices.
- Gated by `ROZUM_CODEX_LEAN` (shares the tool-lean switch); override with `ROZUM_CODEX_LEAN_PROMPT`
  (`0` = never trim, `1` = always). Pure decision `lean_prompt_on` unit-tested (race-free);
  gateway suite 75/75; `--no-default-features` bin build green.
- Refutes the alternative (constrained-decode for gpt-oss): that A/B-broke gpt-oss (0/4) and is
  unneeded — gpt-oss is format-competent unconstrained (4/4 clean); the issue was always LOAD.
  Spec `docs/specs/constrained-gptoss-delivery.md`.

## gateway — memory-pressure watchdog: graceful shedding before the jetsam reboot (BUG-003 runtime half)
Completed: 2026-06-22

The residency gate stops overcommit at *load* time; this handles *runtime* drift
(KV growth on long contexts, the cap-unenforced gguf/mistralrs paths) that can still
creep toward the OS jetsam ladder that rebooted the Mac. New `rozum-core::shed`: reads
the OS memory-pressure level (macOS `kern.memorystatus_vm_pressure_level` — the jetsam
signal) and a pure `should_shed(pressure, inflight, idle)` decision. The gateway
lifecycle watchdog now unloads its OWN idle model under genuine host pressure (it lazily
reloads on the next request) — a reboot becomes graceful degradation. Conservative by
default: never interrupts in-flight work, only an idle model (`ROZUM_GATEWAY_SHED_MIN_IDLE_SECS`,
30s), and Critical-pressure only (`ROZUM_GATEWAY_SHED_ON_WARN=1` for earlier); the sysctl
probe runs only when already idle (no hot-path cost). `ROZUM_GATEWAY_SHED=0` disables.
7 unit tests (decision matrix + reader); gateway 74/74, core 107/107.

## gateway — BUG-003 v2: RAM-ledger residency (admit a fitting 2nd model, not just refuse)
Completed: 2026-06-22

v1's host-residency gate was a hard single-flight — one resident model per host, any
2nd refused. v2 makes it a **RAM budget**: each gateway reserves its estimated
footprint (`residents/<pid>` flock-held file) before loading, and a load is admitted
iff it is the sole resident OR `in_use + footprint ≤ total_ram × RAM_BUDGET_FRAC`
(0.65). A genuinely-small 2nd model co-resides; the case that reboots (two big models
⇒ overcommit) still refuses with a clear, holder-named message. The v1 reasons for
rejecting a ledger are answered: reservation is up-front **under a brief admit lock**
(no free-RAM-read race) and liveness is a **per-pid flock probe** (same death-safety
as v1, no kill-reaper). Footprint is estimated caller-side from the model catalog
(`estimate_model_footprint_bytes`, rozum-core stays model-free); an unknown model gets
a huge estimate so it loads only when the host is empty (conservative). Knobs:
`ROZUM_GATEWAY_RAM_BUDGET_FRAC`/`_BYTES`, `ROZUM_GATEWAY_FOOTPRINT_INFLATE`/`_BASE_MB`.
4 unit tests + real-binary smoke; core 91/91; spec § v2; BUGS.md BUG-003.

## gateway — sampling reproducibility instrument (matrix non-determinism root cause + fix)
Completed: 2026-06-22

The agentic matrix had cells that flipped pass↔fail on a **byte-identical config** —
noise that undermines every reading. Root cause, proven from code **and** live: the
gateway is a faithful pass-through of `temperature`/`top_p`/`top_k` but **never threads
`SamplingParams.seed`**, so the sampler + MLX RNG seed from entropy. Any `temperature>0`
request (Claude Code's main loop sends 1.0; gpt-oss reasoning ~1.0) therefore produces a
different token stream every run → high-entropy trajectories (reasoning, free phrasing,
tool-arg values) flip. A canonical-output prompt stays stable (peaked distribution
collapses temp=1 to argmax), which is why only some cells flip.

Live probe (GLM-4-9B, dense, under the BUG-003 residency guard, N=5): temp=1.0 unseeded
→ **5/5 distinct**; temp=1.0 + `ROZUM_SAMPLING_SEED=42` → **1/5 (deterministic — fixed)**;
temp=0 greedy → 1/5 (dense argmax baseline).

- `apply_determinism_env()` at all three gateway handler sites (`oai_chat`/`responses`/
  `anthropic`), pure unit-tested core `apply_determinism(s, force_greedy, seed)` — both
  knobs **default OFF, byte-for-byte unchanged** unless set:
  - `ROZUM_SAMPLING_SEED=<u64>` — pins the RNG (fills only an unset seed) so a temp>0 run
    replays identically **without** changing temperature (right knob for a stable matrix).
  - `ROZUM_FORCE_GREEDY=1` — forces temp 0/argmax (isolation control; distorts reasoning
    models, so not for the bench).
- `scripts/bench/agentic.sh` exports `ROZUM_SAMPLING_SEED="${ROZUM_SAMPLING_SEED-1234}"`
  → the matrix is reproducible by default (override, or set empty for free sampling).
- `scripts/bench/nondeterminism-probe.sh` — read-only N-identical-POST byte-compare (never
  starts a gateway). Spec `docs/specs/matrix-nondeterminism.md`. 3 tests; 74/74 gateway
  tests green on default + `--no-default-features`.

## gateway — host-wide model-residency gate (stop concurrent-gateway reboots, BUG-003)
Completed: 2026-06-22

The Mac rebooted (2026-06-22 13:41) from a **watchdog kernel panic** — not the
BUG-001 GPU double-free, but whole-system memory overcommit: two matrix runs
overlapped, ~3 model-loaded `rozum gateway` processes ≈61.6 GB on a 36 GiB box →
`vm-compressor-space-shortage` jetsam cascade → `watchdogd` starved 92s → panic.
A dedicated `rozum gateway --port 8300+` (what `agentic.sh` starts) bypasses the
shared-gateway port singleton (8089/`active.json`), so nothing stopped the second
resident model.

Fix: a **host-wide model-residency admission gate** in `crates/rozum-core/src/share.rs`
(`acquire_residency`). Every model-loaded gateway takes an advisory `flock` on
`gateway_dir()/residency.lock` before bringing weights resident and holds it for its
process lifetime (wired into `run_gateway` + `run_launch_dedicated`). Independent of
port/run/worktree, so it catches the dedicated-bench path. A second loader waits up
to `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` (default 240s, past the matrix teardown
window) then refuses with a clear message naming the holder — a host reboot becomes
a recoverable error. `flock` releases on process death (no stale-lock risk). Escape
hatch `ROZUM_ALLOW_CONCURRENT_RESIDENT=1`. Unit tests + real-binary smoke (held →
refuse+exit 1 before load; free → passes through). BUGS.md BUG-003,
memory `[[project-reboot-watchdog-oom]]`.

## gateway — codex create: handle "whole file as a fake Update-File patch"
Completed: 2026-06-22

One more gpt-oss create shape: a brand-new file (esp. nested `src/main.rs`)
dumped as `*** Update File: <path>` whose body after `@@` is the file's RAW
content with NO diff markers (often inside a broken `apply_patch <<'…'` heredoc
that runs bare → nothing lands). `parse_bare_file_block` detects it (Update File
+ body, zero `+`/`-` markers) and `apply_patch_block_to_fuzz` creates the file
from the verbatim body via the shared `synth_create_command` (absence-guarded,
indentation preserved). A genuine diff bails out to None → patch path unchanged.

Honest scope note: this is one more catch in a long tail. gpt-oss emits the
nested-file create as an OPEN-ENDED variety of malformations (bare-Update-File,
nested-heredoc `cat <<'EOF'…cat>…<<'EOF'` with delimiter collision, chain-of-
thought leaked into tool args, bare `apply_patch`). The gateway can catch them
one by one, but reliable codex×gpt-oss create-from-scratch is not reachable by
gateway translation alone — it's the model wrestling codex's V4A protocol (the
content it produces is valid Rust; the *delivery* is what degrades, far more than
under claude's trivial `Write({path,content})`). See docs/matrix-failure-analysis.md
Finding 5.

## gateway — codex create-from-scratch: handle patch-based create shapes
Completed: 2026-06-22

Extended the create-from-scratch handling beyond the explicit `{path, content}`
write-intent (below) to the two PATCH-based create shapes gpt-oss actually emits
most often, both in `apply_patch_block_to_fuzz` so all three delivery paths
(string command, sibling patch, function call) are covered:

1. **`*** Add File:` / `*** Create File:` directives** (the dominant, canonical
   create shape — `*** Create File:` is gpt-oss's variant of the standard V4A
   `*** Add File:`). The lines that follow are the new file's content (bare or
   `+`-prefixed); each becomes a real write. Multi-file patches handled.
2. **`*** Update File:` against an absent file** — the model labels a brand-new
   file an "Update" with a bogus `---` old-side; `patch` can't update a missing
   file. Detected (additions present, no real removed/context content) and
   written from the `+` lines.

Both go through a shared `synth_create_command`: `[ -e path ] || { mkdir -p …;
cat > path <<'ROZUM_CREATE_EOF' … }` — written verbatim, only when absent (a
re-sent create is an idempotent no-op, never clobbers a real edit). Genuine edits
(real removed/context lines) stay byte-identical on the `patch --fuzz` path, so
the `fix` task is unaffected. Validated: 3 unit tests + shell e2e (`cargo run →
olleh`) + live matrix — codex × gpt-oss × `test` flipped 0→1 (first create-from-
scratch green); pairs best with `ROZUM_GPTOSS_TOP_P=0.95` (clips the junk-token
tail that otherwise makes the model emit unparseable shapes). Residual `build`
flake is gpt-oss run-to-run variance, not a gateway gap.

## gateway — codex create-from-scratch: synthesize a real write
Completed: 2026-06-22

Fixed the last codex × gpt-oss residual (matrix Finding 5). Asked to create a
file from scratch, gpt-oss routes a write-intent through the codex shell tool as
`{cmd:"apply_patch", path, content}` where `content` is a whole file (not a
patch); codex runs bare `apply_patch`, drops `path`/`content`, and the file never
lands (`build` rc=143 timeout, `test` pass=0). `normalize_codex_tool_args` now
detects this shape — bare `apply_patch` + a non-patch `{path, content}` — and
synthesizes the real write: `mkdir -p "$(dirname '<path>')"; cat > '<path>'
<<'ROZUM_WRITE_EOF' … ROZUM_WRITE_EOF` (single-quoted heredoc → body verbatim,
no shell expansion). Patch-content still folds to `patch --fuzz`; path-only calls
are left untouched. Unit + shell-e2e validated; full matrix cell deferred behind
a concurrent GLM-4-32B run holding RAM. Spec:
`docs/specs/codex-create-write-synth.md`.

## gateway — opt-in raw Codex tool-call capture
Completed: 2026-06-21

Added `ROZUM_CODEX_TOOL_CAPTURE=1` for the Codex `/v1/responses` gateway path.
When enabled, the existing gateway JSONL log records `codex_tool_inventory` and
`codex_tool_call` events, including raw tool names/arguments and the final
names/arguments returned to Codex after apply-patch reroutes or argument
normalization. The trace is off by default, supports
`ROZUM_CODEX_TOOL_CAPTURE_MAX_BYTES`, and covers both streaming and
non-streaming responses.

## meetings web — model-participant controls
Completed: 2026-06-21

Added a compact model control panel to `rozum meetings web` plus authenticated
`/api/model/status`, `/api/model/start`, and `/api/model/stop` endpoints. The web
process supervises one managed `rozum meetings participant` child for the current
room, passing through model, handle, reply policy, gateway URL, peers, and
persona options to the existing CLI. Status reports running/stopped/exited state,
pid, config, and a best-effort gateway probe; stop kills only the child started
by this web process. Verified with focused web tests, no-default build, and an
isolated live smoke.

## meetings — read-only REST transcript API on the daemon
Completed: 2026-06-21

Added an opt-in read-only HTTP listener to the meeting daemon, enabled by
`ROZUM_WEB_SECRET` and bound by `ROZUM_MEETINGS_REST_BIND` (default
`127.0.0.1:8401`). It exposes `GET /rooms/{name}/days` and
`GET /rooms/{name}/messages/{date}?from=N&count=M`, reading only the daemon
registry, `index.json`, and daily JSONL files. Auth matches `rozum meetings web`
HTTP Basic password gating; no submit, SSE, room creation, model, or UI path is
added. Verified with tempdir HTTP tests, daemon tests, no-default build, and a
temporary-daemon curl smoke.

## engine-spi — dense MLX routes through `drive` (first real production caller)
Completed: 2026-06-21

The dense MLX path now goes through the shared engine-SPI driver `engine::drive` instead of calling
`stream_generation` directly — giving `drive` its first production caller and proving `LocalEngine` on
a real, perf-tuned engine (it had only `FakeEngine` + a stub before). A new `DenseMlxEngine`
(`impl LocalEngine`) carries the already-prefilled state (`run_job` does prefix-reuse prefill outside
the generator) + the borrowed model & KV cache (split-borrow) + sampler params, and its `generate`
dispatches per dense arch (Qwen3 / Qwen3Moe / gpt-oss / Llama / Qwen2 / Gemma3) to build the same
per-arch `Generate` + `PipelinedIds` the old arms did. `run_job` now splits on `is_hybrid_arch`: the 6
dense arches → `drive`; the 2 hybrid arms (Qwen3.6) stay on `stream_generation` because they reclaim
their internal KV/conv cache via `into_cache_and_snapshot` for prefix reuse, which `drive`'s
`Box<dyn Iterator>` return would erase (that's the still-deferred reclaim seam). Builds on the
just-shipped `Send`-relaxation (MLX engine state is `!Send`). **No runtime change** — the value is the
architectural proof + x86 de-risking. Validation: (1) byte-identical by construction — the dense path
builds the identical generator and runs the same `consume_tokens` with identical
meta/prompt_len/seed/repeat_guard/decode/emit; (2) functional — the branch produced correct coherent
greedy output on cached gpt-oss-20b (the analysis channel correctly listing primes `2, 3, 5, 7, 11, …`
before the token cap, exactly as deterministic greedy should); (3) engine unit tests green. The
empirical master-vs-branch raw A/B was attempted but blocked by RAM-starvation from accumulated 11 GB
model loads (an environment limit, not the code), so it rests on the by-construction proof + the
functional run.

## engine-spi — draft the cache-reclaim seam (prefix-reuse engines through `drive`)
Completed: 2026-06-21

A compile- and FakeHybrid-validated DRAFT of the last conceptual gap in the engine SPI: how a
prefix-reuse engine (the MLX hybrid arch — Qwen3.6) would route through the shared `drive` loop
without losing its post-run cache. `LocalEngine::generate`'s `Box<dyn Iterator>` is dropped at
end-of-run, which erases the generator's reclaimable KV/conv cache (MLX reclaims it via
`generator.into_cache_and_snapshot()` → `store.put_hybrid` for next-turn prefix reuse). The draft adds
a `ReclaimStream` trait (`Iterator<Item=Result<u32,String>>` + `type State` + `into_state(self:
Box<Self>) -> State`, mirroring `into_cache_and_snapshot`) and `drive_reclaiming(stream, …) ->
(StopReason, State)`, which drives the stream through the SAME shared `consume_tokens` (borrowed so the
stream survives) then reclaims its state. Two tests prove the cache round-trips through the loop —
directly (`FakeHybridStream`) and through a `Box<dyn ReclaimStream>` trait object. **Deliberately
unwired:** MLX is untouched (hybrid still calls `consume_tokens` directly); the FINAL shape (whether it
folds into `LocalEngine`, the exact `State` bounds, how an engine produces a `ReclaimStream`) is to be
decided against the real x86 engine — the second prefix-reuse-capable engine — which doesn't exist on
M4 yet, so no API is committed. This closes the engine-SPI's design questions on paper and de-risks the
eventual MLX-hybrid + x86 adoption. Spec + the now-`Send`-free trait updated in
`docs/specs/native-engine-spi.md`.

## engine-spi — relax the `LocalEngine` `Send` bound (unblocks `!Send` in-process engines)
Completed: 2026-06-20

Dropped the `Send` requirement from the `LocalEngine` trait and from `generate()`'s returned iterator.
A feasibility map established that this — not the (deferred) cache-reclaim seam — is the real blocker
to routing the MLX path through the shared `engine::drive`: the MLX engine state (model + `Array`s +
KV cache on one Metal stream) is irreducibly `!Send`, pinned to a worker thread for life (the same
reason the GGUF/llama.cpp path calls `consume_tokens` directly). `drive` runs the engine
**synchronously on its own thread** and never moves it across threads, so the `Send` bound only locked
these engines out of the seam for no benefit; engines that *are* `Send` still are. Proven by a new
`drive_accepts_a_not_send_engine` test — an engine holding an `Rc` (so `!Send`) that now implements
`LocalEngine` and runs end-to-end through `drive`, which would not have compiled before. This unblocks
a future `impl LocalEngine` for dense MLX / llama.cpp; the remaining dense-MLX `drive` adoption is a
hot-path `run_job` restructuring of mostly-architectural value (MLX already shares the decode loop via
`consume_tokens`), best done alongside the x86 engine — tracked in SPRINT `engine-spi-dense-mlx-drive`.

## gguf — route through the shared engine loop + fix a 1-token generation bug
Completed: 2026-06-20

Two things. **(1) engine-SPI A3:** the GGUF backend's `generate_blocking` now drives the shared
`crate::engine::consume_tokens` (the engine-SPI decode loop, also used by MLX) via a token iterator
(`std::iter::from_fn` over llama.cpp sample→advance) + a per-token detokenize closure — deleting
GGUF's private ~150-line decode loop and its streaming `ToolUseParser`/`ToolParseEvent`. The SPI is
now proven by **two real engines, not just MLX**. Tool calls + cross-turn-unique ids + the runaway
guard come from the shared loop (no `Send` bound, so the `!Send` `LlamaContext` is fine running
synchronously on the blocking thread — unlike `drive()`).

**(2) Fixed a pre-existing GGUF generation bug surfaced by the e2e:** `get_logits_ith(n_cur - 1)`
used the *absolute* sequence position, but `get_logits_ith` indexes the **last decoded batch** — and
every single-token decode batch holds its token at index 0. So after the first generated token, every
subsequent sample read past the 1-token batch → garbage logits → an end token → **generation stopped
after ~1 token**. GGUF output in rozum was effectively broken (the focus had been MLX). Now the index
is tracked correctly (`n_prompt-1` for the prefill batch, `0` for each decode batch). **Validated
e2e** against `ollama:qwen2.5-coder:7b` (ollama's own runtime confirmed the model is fine): before —
"count to 20" → `"1"`, a tool request → `{"`; after — full `"1 2 3 … 20"` and a correct
`tool_calls: [{name: get_weather, arguments: {"city":"Kyiv"}}]` with a cross-turn-safe id, through
the refactored path. Engine + serving + gguf unit tests green; A/B confirmed the pre-fix behavior was
identical on master (not a regression).

## gguf — cross-turn-unique tool-call ids (fixes Claude Code dropping turns)
Completed: 2026-06-20

The GGUF backend's streaming `ToolUseParser` minted tool-call ids from a per-response counter
(`call_1`, `call_2`, …) that **reset every response**, so ids collided across turns — and Claude Code,
unable to pair a `tool_result` back to a reused id, **drops the turn**. It now mints ids via the shared
`crate::engine::next_tool_call_id()` (a process-monotonic counter, the same one the MLX path uses), so
every tool call across a conversation is unique. This is also the first concrete step of the
engine-SPI's GGUF adoption (sharing an engine helper). Test
`tool_call_ids_are_unique_across_calls_and_consistent_within` (ids are unique across calls, consistent
within a call's Start/Delta/End). The fuller GGUF→`consume_tokens` adoption remains a tracked follow-up
(SPRINT `engine-spi-a3-gguf`).

## sandbox — a `[sandbox]` table in `rozum.toml` (persistent policy beyond env)
Completed: 2026-06-20

The sandbox was env-only (`ROZUM_SANDBOX*`), which can't express path lists. Added a `[sandbox]`
table to `rozum.toml` (`SandboxConfig` on `RuntimeConfig`): `workspace` (extra writable paths beyond
the launch cwd; `"."` = cwd, `"~/…"` = `$HOME`), `read_only` (reference paths — Docker `:ro` mounts;
under Seatbelt a non-writable path is already read-only), `secret_deny` (extra secret dirs appended to
the built-in denylist), `network`, and `backend`. It's loaded in `sandboxed_command` (the unsandboxed
launcher) and merged into the policy via a new `SandboxPolicy::rust_coding_with`. **Env overrides
config** — `ROZUM_SANDBOX_NETWORK`/`ROZUM_SANDBOX_BACKEND` win over the config values, and
`ROZUM_SANDBOX=0` still disables the jail; the config is the persistent default, env the per-launch
override. `SandboxPolicy` gained a `read_only` field rendered as `:ro` binds under Docker (skipping
any path already covered by a writable mount). A missing/malformed `rozum.toml` falls back to env-only
behavior (never breaks the jail). Resource limits stay env-only for now. Verified: config-parse +
read-only-mount + `rust_coding_with` unit tests, and a live smoke (`[sandbox] backend=docker` +
read_only + extra workspace/secret drove a real launch; `ROZUM_SANDBOX_BACKEND=seatbelt` overrode the
config). Completes the model-sandbox config surface — the last (c) item of the sandbox track.

## cascade — `rozum gateway --model cascade[:name]` works from a cold start
Completed: 2026-06-20

The cascade request-surface (`model: "cascade"` / `"cascade:<name>"` / a comma-separated model list
→ a `CascadeBackend`, with named configs from `[cascade.<name>]` in `rozum.toml`) was wired into the
gateway's reload `BackendBuilder` but NOT its initial **startup** build — so `rozum gateway --model
cascade:fast` launched fresh tried to load a literal model named "cascade:fast" and failed with "no
backend"; it only worked after a lazy reload/switch. Fixed by extracting the cascade detection into a
shared `try_cascade_backend` chokepoint that both the startup build (`run_gateway`) and the reload
builder now call, with identical semantics (a `cascade[:name]` spec never falls back to a literal
model; a comma list that fails to build does fall back). A cascade spec takes precedence over
speculative decoding (a draft pairs with one model, not a cascade). Verified: a startup-routing
integration test (cascade:name / bare cascade / comma-list / plain-model all route correctly, using
no-key OpenAI remote tiers that build without I/O) and a live smoke — `rozum gateway --model
cascade:test` with a `[cascade.test]` rozum.toml now boots serving the cascade (startup banner, not
"no backend"). Completes the cascade-router request-surface (the last deferred piece of the 9-phase
cascade work).

## sandbox — no-approval autonomy for jailed headless agents (the "no-noise" principle)
Completed: 2026-06-20

Realizes the model-sandbox "No-noise principle": a sandboxed model should be free to act inside its
allowed paths **without a stream of per-action approval prompts** — the structural jail, not
interactive confirmation, is the safety boundary. `rozum launch` now injects the agent's
approval-bypass flag for HEADLESS invocations when the jail is active: `claude -p` →
`--dangerously-skip-permissions`, `codex exec` → `--dangerously-bypass-approvals-and-sandbox` (whose
own help says it's "intended solely for environments that are externally sandboxed" — exactly this
jail), `opencode run` → `--dangerously-skip-permissions`. This also kills the Codex reject-escalation
retry loop (matrix Finding 1a), where prompts that can't be answered headlessly made the model spin to
the turn cap. Gated three ways for safety: only when the jail is on (never grant no-prompt autonomy
unsandboxed), only for headless invocations (an interactive operator can answer prompts, so those
sessions are untouched), and never overriding an explicit user policy (`--permission-mode`, codex
`-a`/`-s`/`--sandbox`/`--full-auto`, or the flag already present). Previously only the agentic bench
passed these flags by hand; now any `rozum launch` of a sandboxed headless agent gets them. The
decision lives in a pure `autonomy_flag_for` helper (2 unit tests covering jailed/headless/interactive/
explicit-policy/idempotent/basename matching); verified end-to-end that the launched agent actually
receives the flag, and that interactive and `ROZUM_SANDBOX=0` launches do not. Completes the
model-sandbox P3 track.

## sandbox — strict gateway-only egress (no internet) + opencode-under-Docker fix
Completed: 2026-06-20

Closes the last two Docker-backend gaps. **`gateway-strict` egress** (`NetPolicy::GatewayStrict`,
`ROZUM_SANDBOX_NETWORK=gateway-strict`) gives a true egress allowlist: the container reaches the
local model gateway and **nothing else** — no internet, so a misbehaving model can't exfiltrate the
repo or fetch untrusted code. Docker has no native egress-allowlist flag (`--internal` blocks the
host gateway too), so it's enforced IN the container: `to_docker_run_args` adds `--cap-add=NET_ADMIN`
+ `ROZUM_EGRESS=strict`, and the `rozum-agent` entrypoint installs an iptables allowlist (ACCEPT
loopback + established + the resolved host-gateway IP, DROP everything else incl. all IPv6) before
exec'ing the agent. If it can't enforce (missing cap/iptables) it fails loud (exit 70) rather than
run unprotected. On Seatbelt it's identical to `gateway-only` (the SBPL rule is already loopback-
only). Verified on M4 via `rozum launch`: gateway REACHED, `1.1.1.1` BLOCKED (and `gateway-only`
control reaches both). **opencode-under-Docker** now works: its generated `OPENCODE_CONFIG` file is
written under canonical `/tmp` (a toolchain bind mount) instead of `$TMPDIR` — Docker Desktop doesn't
reliably share `/private/var/folders`, which left the config invisible in the container; `/tmp` (host
path == container path) is exposed by the existing mount (verified in the `rozum-agent` image). The
image gained `iptables` + a `/usr/local/bin/rozum-entrypoint.sh` (transparent passthrough unless
strict). 3 new tests (`docker_args_strict_egress_adds_cap_and_marker`, the net-policy parse, and
`opencode_config_lives_under_tmp_so_docker_mounts_it`); the cargo-build-in-jail e2e still passes
through the new entrypoint.

## sandbox — Docker resource limits + a network-policy knob (DoS containment + egress control)
Completed: 2026-06-20

Two hardening levers for the sandbox plus two honest findings. **Resource limits:** the Docker render
now takes `--memory`/`--cpus`/`--pids-limit` (`ROZUM_SANDBOX_DOCKER_{MEMORY,CPUS,PIDS}`) so a runaway
model can't exhaust host RAM/CPU or fork-bomb — memory/cpus are opt-in (heavy builds aren't throttled),
and `--pids-limit` defaults to 2048 as a cheap fork-bomb guard. Verified on M4: a 64 MB cap OOM-kills a
256 MB allocation (rc 137) and a pids cap fails forks past the limit. **Network knob:**
`ROZUM_SANDBOX_NETWORK` (`none` | `gateway-only` (default) | `full`) is now honored by BOTH backends
(`sandboxed_command` previously hard-coded `GatewayOnly`); verified via `rozum launch`: `none` → the
container can't reach the gateway (true zero-egress), `gateway-only` → it can. Also added `wget` to the
`rozum-agent` image (it shipped only `curl`). **Findings recorded (not faked):** (1) there is no simple
Docker flag for true gateway-only-but-no-internet — `--internal` blocks the host gateway too, and Docker
Desktop has no native egress allowlist, so strict containment needs a host/VM firewall or proxy sidecar
(use `network=none` for guaranteed zero-egress meanwhile); (2) opencode's `$TMPDIR` config file isn't
reliably shared into the container by Docker Desktop, so opencode-under-Docker needs an exec_agent
refactor to mount it (claude/codex are env/flag-driven and work). 2 new unit tests
(`docker_args_render_resource_limits_only_when_set`, `net_policy_parse_maps_aliases_and_defaults`).

## sandbox — the `rozum-agent` container image (makes the Docker backend runnable)
Completed: 2026-06-20

The Docker sandbox backend renders correct `docker run` commands, but `default_docker_image()`
pointed at a `rozum-agent:latest` that didn't exist — so a real `rozum launch <agent> … docker`
had nothing to run in. This adds the image: `docker/rozum-agent.Dockerfile` (a `rust` slim base +
git + Node 22 + the three agent CLIs — claude/codex/opencode — on PATH) and `scripts/build-agent-
image.sh` to build it. The agent talks only to the host gateway via `host.docker.internal` (rozum
wires that env at launch), so the image needs no creds and the container is fully ephemeral. One
sharp edge fixed: the `rust` base exposes cargo via `ENV PATH`, but agents commonly build through a
*login* shell (`bash -lc`) that resets PATH and dropped cargo — the image adds `/etc/profile.d/
rust.sh` so `cargo` is found no matter how the agent spawns it. `rozum launch` now also prints a
build hint when the configured image is missing locally, instead of confusingly trying to pull the
unpublished default. **Validated end-to-end on M4 (Docker 29.6):** built the image, then a real
`rozum launch --no-model … docker` ran `cargo new` + `cargo build` + executed the binary *inside* the
container (`Hello, world!`), and the build output round-tripped to the host workspace — the Docker
analog of the Seatbelt P1 "cargo build succeeds" gate (committed as the ignored test
`agent_image_builds_a_crate_in_the_docker_jail`). Known gaps unchanged (BACKLOG): bridge egress under
`gateway-only`, opencode's unmounted config file, no resource limits yet.

## sandbox — Docker container backend (`ROZUM_SANDBOX_BACKEND=docker`)
Completed: 2026-06-20

A second enforcement backend for the agent jail (model-sandbox P3), opt-in alongside the default
macOS Seatbelt. The same `rust-coding` `(path, mode)` policy is now also renderable to a `docker run`
(`SandboxPolicy::to_docker_run_args`): writable paths become `-v <p>:<p>:rw` binds (host path ==
container path so the workspace/cwd line up), the rest of the host filesystem is simply **absent**
(no mount = unreachable, stronger than a Seatbelt deny), secrets that sit under a mounted workspace
are shadowed by an empty `--tmpfs`, and network maps to `--network=none` / `--add-host
host.docker.internal:host-gateway`. The container reaches the host gateway/MLX via
`host.docker.internal` — wired as a **single choke point** (`sandbox_gateway_host()` feeds
`exec_agent`'s `base`, so every Anthropic/OpenAI/codex URL is container-correct with no other change).
Only an allowlist (`SANDBOX_FORWARD_ENV`) is forwarded into the container via `-e NAME`, so host env
doesn't leak. Selected with `ROZUM_SANDBOX_BACKEND=docker` (alias `container`); the image is
operator-supplied (`ROZUM_SANDBOX_DOCKER_IMAGE`, default `rozum-agent:latest`, must carry the agent
CLI on PATH). Unlike Seatbelt (macOS-only), the Docker backend turns the jail on for any OS with a
docker daemon. **Validated on M4 (Docker 29.6):** 4 unit tests on the rendered argv + a real `docker
run busybox` e2e (in-workspace write round-trips to the host; an out-of-mount write does not; a secret
under the mount reads back empty) + a container→host `host.docker.internal` reachability probe + a
full `rozum launch --no-model` container run (the container's stdout surfaced; the env allowlist
forwarded `CLAUDE_CODE_*` while a non-listed host var stayed empty). Known gaps (tracked in BACKLOG):
`gateway-only` still permits bridge egress (use `none` for strict isolation), opencode's config file
isn't mounted yet, and no resource limits. Seatbelt default + behavior unchanged.

## launch — `--no-sandbox` flag (opt out of the agent jail per-launch)
Completed: 2026-06-20

The agent sandbox is ON by default on macOS; the only way to opt out was the `ROZUM_SANDBOX=0`
env var. Added `rozum launch --no-sandbox` as CLI sugar for it: the flag sets `ROZUM_SANDBOX=0`
so `sandbox_workspace()` stays the single place the jail decision lives (no second code path to
drift). It's hoisted by `reorder_launch_args` like the other launch flags, so it works after the
program name too (`rozum launch claude --no-sandbox`); a `--no-sandbox` placed after a `--`
separator is still passed through to the child program unchanged. Help text + 2 reorder unit tests
+ CLI probes (default jailed / `--no-sandbox` unjailed / after-program hoist / after-`--` passthrough)
green. BACKLOG `model-sandbox-seatbelt` item (b) closed; only `rozum.toml [sandbox]` config (c) left.

## gateway/harmony — gpt-oss agentic delivery: recover garbled tool calls + repair broken reads
Completed: 2026-06-19

Deep dissection (RESOLUTION 3 in `docs/matrix-failure-analysis.md`) showed codex × gpt-oss failures
are mostly us DROPPING the model's delivery, not the model failing. Two more fixes on top of the
delivery bridges: (1) **harmony recovery** (`infer_tool_from_body`, default-on) — gpt-oss sometimes
garbles the harmony envelope (drops / detaches the `to=functions.NAME` recipient), so `parse_harmony`
dropped a real tool call and the agent stalled; we now recover the function from the args shape
(`cmd`→exec_command, `patch`/`*** Begin Patch`→apply_patch), with a negative test so prose is never
misrecovered. (2) **read-repair** (`repair_broken_read`, `ROZUM_CODEX_READ_REPAIR`, env-gated) —
reading the file is the decisive success factor, but gpt-oss emits broken `sed` reads that never see
the file; we translate a malformed sed/head/tail read → `cat <file>`. Net full gpt-oss matrix with all
fixes: 12/15 (was 8/15), 0 panics. Refuted (kept as negative results): injecting an apply_patch tool
(conflicts with codex's instruction format → worse) and `ROZUM_GPTOSS_TOP_P` (within noise).

## models — focus the catalog on two models; older ones move to an opt-in fallback
Completed: 2026-06-19

The `RECOMMENDED` catalog (the launch picker + `rozum models list --remote` + the `list_models`
builtin) now surfaces only the two models we actively run: **Qwen3.6-35B-A3B-4bit** (strongest local
agentic coder) and **gpt-oss-20b-MXFP4-Q4** (OpenAI reasoning MoE). The seven older / niche models
(Qwen3-30B-A3B, Qwen3.6-27B, 35B-A3B-DWQ, Qwen3-Coder-30B, Qwen2.5-Coder-32B/7B, Qwen3-4B) moved to a
new `EXTRA` fallback list — shown with `rozum models list --all` and still launchable any time via
`--model <spec>` (the catalog is a curated picker list, not a whitelist). The agentic bench default
(`agentic.sh DEFAULT_MODELS`) is now these two. Separately, the on-disk weights for everything except
the two kept models were removed (HF cache + Ollama), freeing ~131 GB (163 → 32 GB).

## gateway — re-route gpt-oss's `apply_patch` *function* call to exec_command (codex)
Completed: 2026-06-18

gpt-oss (trained on OpenAI's tool surface) emits `apply_patch` as a **function** call, but codex —
for the rozum-served local-model config — offers apply_patch only as a **shell command**, so codex
rejected it (`error=unsupported call: apply_patch`) and the edit was silently lost. This was the last
clean gateway barrier behind `codex × gpt-oss`'s edit failures. `src/gateway.rs` now re-routes it
(`rewrite_apply_patch_function_args` + the Responses streaming/collect paths): when the model calls
`apply_patch` as a function **and codex didn't offer apply_patch as a tool** (`apply_patch_is_tool`,
read from the request), the item is renamed to `exec_command` and the args become `{"cmd":"<patch
--fuzz heredoc>","login":true}` (reusing Method B's `apply_patch_block_to_fuzz`; raw-`apply_patch`
heredoc fallback). The gate leaves a genuine codex-with-apply_patch config — and Qwen's
apply_patch-via-shell (Method B) path — untouched. Validated: `unsupported call` eliminated, the
re-route fires + a `codex × gpt-oss fix` run passes because of it; unit test
`apply_patch_function_reroutes_to_exec_command`, gateway suite 50/50. The remaining `codex × gpt-oss`
reds are now proven model-level (malformed shell, temp-1.0 looping, `cargo new` subdir), documented in
`docs/matrix-failure-analysis.md` (RESOLUTION 2).

## meeting — presence emitted by the mcp-proxy (supersedes the Claude Code settings.json hooks)
Completed: 2026-06-18

The mcp-proxy now posts a `joined:` line on its first join and a `left:` line when the agent's
session ends, **over the agent's own session** — so the presence line carries the agent's handle
(unified with its messages, fixing the dual-handle wart), works for **every** agent (not just
Claude Code), and edits no user config. This replaces the earlier `rozum mcp install` Claude Code
`SessionStart`/`SessionEnd` hooks (which would double-post, were CC-only, and edited
`~/.claude/settings.json`): the hook-merge code + the `--no-hooks` flag are removed, so `rozum mcp
install` now just registers the MCP server. Posted once per proxy lifetime (not on reconnects);
`left:` is best-effort after the stdio session ends. (serde_json `preserve_order` is kept — harmless.)

## meeting — stable local identity (`rozum identity`); the human is one handle across launches
Completed: 2026-06-18

The local-default `Principal` (agent-meeting-coordination P1.6). The TUI + `rozum meetings post`
used to mint a fresh random session token each launch, so the operator showed up as a new
adjective-animal every time. Now `src/meeting/local_identity.rs` persists a stable `{token, display}`
in `~/.config/rozum/identity.json`; the human's clients (`MeetingClient::connect_as`, `meetings post`
without `--as`) present it, so the operator is **one participant across launches/clients**.
`rozum identity whoami` shows it; `rozum identity set-name <name>` sets the display (keeping the
token). Verified live (two posts → same `Sergiy · mellow-marten`) + a path-injected unit test.
First zero-config rung of the Principal model; auth / multiple-and-remote humans / cross-client
unification are later resolvers on the same seam.

## meeting — shared coordination room via `ROZUM_MEETING_ROOM`
Completed: 2026-06-18

Lets the operator route all agents into one shared room (e.g. `commons`) for a single overview,
instead of per-project rooms (agent-meeting-coordination P1.2). When `ROZUM_MEETING_ROOM=<name>` is
set, the mcp-proxy's auto-join uses `rooms.new` (create-or-open) for that named room instead of the
project room, and `rozum meetings post` honors the same room (precedence: `--room` >
`ROZUM_MEETING_ROOM` > the cwd project) so the presence-hook posts land where the agents are. New
`MeetingClient::enter_or_create` + `PostTarget::Shared` (create-or-open, unlike `Named` which opens
an existing room only). Verified live (post created + routed to `commons`; `meetings status` lists
it) + a unit test. Deferred (a daemon single-room→multi-room change, best shaped by dogfooding):
being in the project room AND `commons` at once; and a `rozum.toml [meeting]` config (env-only now).

## meeting — Claude Code presence hooks + coordination instructions
Completed: 2026-06-18

Continues the agent-meeting-coordination epic (P1.3 hooks + P1.4):

- `rozum mcp install` now also installs Claude Code **presence hooks** — `SessionStart`→ post
  `joined:` and `SessionEnd`→ post `left:` (calling `rozum meetings post --as claude`), so the room
  reflects agents arriving/leaving without depending on the model remembering. Merged into
  `~/.claude/settings.json` **non-destructively**: every existing key + hook (e.g. your `PreToolUse`)
  is preserved, it's idempotent, and `rozum mcp uninstall` reverts to a **byte-identical** file. Uses
  the correct session-lifecycle events (not per-turn `Stop`). `--no-hooks` skips them. Enabled
  serde_json `preserve_order` so editing a user's JSON config keeps their key order.
- Rewrote the mcp-proxy `instructions` (every connecting agent sees them) into a **coordination
  contract**: announce `working:` when starting, check the room before clashing on files/`responding`,
  ask when blocked, post `done:`/`blocked:` on finish, treat the human's messages as priority — on the
  agent's own judgement, not every step. Strengthened AGENTS.md "Meeting-room coordination".

380 lib + 15 bin tests green (4 mcp/hook unit tests). Verified live + reversibly on the real
settings.json. Next: global `commons` room + auto-join, TUI multi-room overview, the Principal layer.

## meeting — `rozum mcp install/uninstall` (bare agents auto-join meetings)
Completed: 2026-06-18

`rozum mcp install [--agent claude|codex|opencode|all]` registers the meeting `rozum mcp-proxy` in
an agent's MCP config, so a **bare `claude`/`codex` run** (no `rozum launch`) gets the `meeting.*`
tools + the channel and auto-joins its project's room. Uses each agent's **own `mcp add`/`mcp
remove`** so their config stays valid (no hand-edited JSON/TOML); idempotent (remove-then-add).
`rozum mcp uninstall` reverts. Verified live and reversibly: claude (user scope, ✔ Connected) and
codex both register then cleanly remove. opencode's `mcp add` is interactive → guidance-only for
now (a config-write is a follow-up). Pure `mcp_add_spec`/`mcp_remove_spec`/`expand_mcp_agents`
(3 unit tests). Part of the agent-meeting-coordination epic (P1.3); CC SessionStart/Stop presence
hooks are the next piece.

## meeting — `rozum meetings post` (one-shot post transport) + author display in the transcript
Completed: 2026-06-18

First increment of the **agent-meeting-coordination** epic (the meeting room as the collaboration
system — `docs/specs/agent-meeting-coordination.md`):

- `rozum meetings post <text> [--room <name>] [--as <display>]` — one-shot connect → join (the cwd
  project's room by default, or a named room) → submit → exit; **auto-spawns the daemon** if it
  isn't running. This is the transport the upcoming SessionStart/Stop coordination hooks call, and
  a handy human/script post. Core is `meeting::tui_client::post_once` (unit-tested: lands in the
  room; unknown room errors cleanly).
- **Author is now visible in the transcript:** `room.rs::submit` writes `base_name · handle`
  (e.g. `claude · spry-wren`) via the existing `identity::display_name`, instead of the bare
  minted handle — so readers see WHO posted (the agent name / `--as` value / `$USER`), which the
  coordination use case needs. De-dup stays by participant id, so this is cosmetic + safe.

Verified live (auto-spawn → post → on disk → author shows the name). 380 fast tests green. Fuller
Principal-based identity/display, the global room, auto-join, and the hooks are the next increments.

## x86 — scaffold the native x86 (Vulkan iGPU) engine slot, ready to fill without rework
Completed: 2026-06-18

Prepares the ground for the cross-vendor Vulkan iGPU engine (`docs/specs/x86-native-runtime.md`)
so the real engine drops into a named, compiling slot with no structural rework — no hardware
needed yet. New `src/x86/`:

- `X86NativeOptions`, `X86NativeBackend` (`impl ChatBackend` — errors with a self-documenting
  `NOT_IMPLEMENTED` message), `try_build_x86_backend` (logs + falls through until built), and
  `X86Engine` (`impl crate::engine::LocalEngine`) — a **second `LocalEngine` implementor** that
  proves the token-level seam fits a non-MLX engine (the native-engine-spi validation, sans GPU).
- The five compact components the spec decomposes the engine into are pre-shaped stub files with
  their contract + the test to write: `device`, `memory` (zero-copy `mmap` import), `tensor`,
  `kernels` (quant matmul / sdpa / rmsnorm / rope / swiglu …), `model` (per-family forward).
- Reachable + self-documenting: `engine = "x86-native"` (aliases `x86`/`vulkan`) is in
  `config.rs::ACCEPTED_ENGINES` with a `main.rs::build_choice` arm; NOT in the default auto-chain.
  `Cargo.toml` reserves the `x86-native` feature for the future Vulkan binding; the stub needs no
  deps, so the default CI keeps the contract honest (3 slot tests; 379 fast tests green).

To fill the slot: add the Vulkan dep under the feature, implement the component bodies (P0–P5), and
wire `chat` through the shared `engine::drive` (native-engine-spi A3, shaped against this consumer).

## cli — `rozum commit-msg` (small-first commit messages from the staged diff)
Completed: 2026-06-18

New subcommand `rozum commit-msg [--model <spec[,spec2]>] [--n-ctx N]`: reads `git diff --cached`,
builds the gate-shaped `commit_message_request`, runs it through a local model, and prints the
message. A single `--model` generates directly; a `small,big` comma-list builds the
`cascade::small_task_config(CommitMessage, …)` cascade — the small model answers and the
`CommitMessageGate` escalates to the big model only when the cheap answer is unusable. Model
defaults to `[runtime].model` from `rozum.toml`; errors cleanly when nothing is staged. This wires
the previously library-only small-model-cascade to a real CLI. `staged_diff_in` is split out and
unit-tested in a temp git repo; the model-call path is manual. Spec: `docs/specs/small-model-cascade.md`.

## bench — graceful gateway teardown (stop the agentic matrix from kernel-panicking the Mac)
Completed: 2026-06-18

The agentic matrix (`scripts/bench/agentic.sh`) could **reboot the host via a GPU kernel panic** —
not a RAM OOM. It tore each model's shared gateway down with `kill -INT` → 60s → an **unconditional
`kill -KILL`**; a SIGKILL landing while the MLX worker is inside a wedged Metal eval corrupts the
IOGPU driver's buffer accounting (`IOGPUGroupMemory::remove_memory_object() not found`) → panic →
reboot. Now the teardown is graceful: SIGINT → wait `TEARDOWN_GRACE` (180s, env-overridable) for a
clean exit → SIGKILL only as a loudly-flagged last resort → a `GPU_SETTLE` (8s) pause so the kernel
finishes async IOGPU reclamation before the next gateway allocates on the same Metal device. Also
adds `ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0` to the gateway launch so the shared gateway isn't self-exited
(`clients_gone`) between the claude/codex phases. Harness-only; `bash -n` clean. Tracked in BUGS.md
BUG-001. The deeper rozum-side bounded-teardown (a Metal-eval timeout so Drop's join can't block
forever) is a deliberate tracked follow-up — it can't be validated without risking a reboot.

## meeting — channel-wakeup ported to the default daemon proxy
Completed: 2026-06-18

Channel-wakeup (push room activity into an idle Claude Code session as `<channel>` events) was
built in the **legacy** `proxy.rs`, but the P4 daemon refactor made `daemon_proxy.rs` the default
`rozum mcp-proxy` — stranding the whole feature (Tier-1 capability/pusher AND the Tier-3 piggyback
writer) on the now-unused legacy path. Ported the mechanism into `daemon_proxy.rs`:

- `initialize` captures the session peer and advertises `experimental:{"claude/channel":{}}`;
  instructions teach the agent to treat `<channel source="rozum" …>` events as a wakeup and fetch
  the authoritative delta via `meeting.wait_my_turn`.
- A per-session background task **disk-tails** the joined room (`store::read_since` from the
  proxy's `room_root` — no second daemon connection, no ghost participant; fits the daemon's
  "clients read disk directly" contract) and pushes `notifications/claude/channel` deltas
  fire-and-forget. It skips the agent's own entries (`participant_id`), primes past the backlog on
  join, and re-primes on a room switch; `read_since` is inclusive of `n`, so the cursor tracks
  next-n. Also carries the Tier-3 piggyback append (likewise previously legacy-only).
- `rooms.join` now also re-points the proxy's disk-read `room_root` (fixes a latent stale-room bug
  for `wait_my_turn` after a switch); `leave` idles the task.

4 new unit tests (render skip-own/format/seq, all-own→none, capability+instructions declared,
transcript_head primes-past-backlog + delivers-fresh). 387 fast tests green. Spec updated with the
daemon-proxy implementation note.

## cascade — task-typed small-first cascade (commit-message gate)
Completed: 2026-06-18

`src/cascade/tasks.rs` adds the **small-model-cascade**: bounded single-shot tasks run small-first
behind a cheap, task-specific validator gate that ACCEPTs a good cheap answer or ESCALATEs to the
big model. A thin preset over the existing cascade (one `AcceptanceCheck` + a two-tier config
builder + a prompt helper), not a new engine — health/backoff/budget/stats/lanes are inherited.

- `SmallTask::CommitMessage` + `small_task_config(task, small, big)` → a two-tier `CascadeConfig`
  (small=tier 0, big=tier 1, `AlwaysCheapest`; acceptance = `[StructuralCheck, <task gate>]`;
  self-signal + escalation affordance off — the task has a concrete validator, not a self-report).
- `CommitMessageGate` (free, deterministic): extracts the subject (first non-empty line, wrapping
  fence/quote/heading stripped) and ESCALATEs on empty / over-72-char / refusal / chatter-preamble /
  `<placeholder>`; otherwise ACCEPT. False-positive-safe (a legit "Fix commit message parser crash"
  accepts). `commit_message_request(diff)` builds the tight gate-shaped prompt.

10 fast tests (gate accept/escalate cases + e2e over a real `CascadeBackend` with mock backends:
good cheap answer accepts at tier 0 with the big tier never called, junk escalates once, and a
small-tier hit-rate batch where 3/4 cheap answers pass → big called exactly once). 383 fast tests
green. Spec: `docs/specs/small-model-cascade.md`. Deferred: process-gated task types (one-line-fix
via `cargo check`) and CLI wiring (`rozum commit-msg`).

## router — small-model RAG worker (rerank + grounded summarize); closes small-model-router
Completed: 2026-06-18

`src/router.rs` gains `RagWorker` — the P2 RAG counterpart to `ModelRouter`, same shape (tight
prompt, `temp 0`, `snap_to_label`, **never errors**) applied to retrieval post-processing:
- `rerank(query, hits)` judges each `rag_lite::Hit` `relevant`/`related`/`irrelevant`, **drops** the
  irrelevant ones, and reorders relevant-first with a **stable** sort — so a small model's coarse
  3-way verdict *refines* BM25 recall without scrambling the lexical order within a grade. A model
  failure / off-set reply keeps the hit as `related` (a conservative keep — never silently drop on a
  fumble).
- `summarize(query, hits)` condenses the survivors into an answer grounded **only** in their text;
  a blank/failed generation falls back to the top hit's snippet (capped on a char boundary).
- `rerank_and_summarize` / `grounded_answer(retriever, query, k)` compose the two with `rag_lite`
  recall into the end-to-end grounded step → `GroundedAnswer { hits, summary }`.

7 hardware-free unit tests over a scripted backend + an `#[ignore]` M4 eval `rag_worker_eval`
(Qwen3-4B drops a lexical decoy, ranks the answering doc first, grounds the summary). 373 fast tests
green. This closes the **small-model-router** track (classifier P1, cascade wiring P2, RAG worker P2).

## launch — rozum-launched codex now defaults to `medium` reasoning (was the user's global `xhigh`)
Completed: 2026-06-17

`rozum launch codex` now appends `-c model_reasoning_effort=medium` by default (skipped if the
operator passes their own). Codex inherits the user's global `model_reasoning_effort` — often
`xhigh`, which suits a frontier cloud model but on a LOCAL model burns minutes of reasoning for
little gain (measured codex on Qwen3-30B-A3B at 7+ min/task; fix 433 s, test hit the 600 s wall).
This is unconditional (not gated on `--lean`) and leaves the user's `~/.codex/config.toml` intact —
it's a launch-time override, like the provider flags. Supersedes the opt-in `--lean` codex cap;
`--lean` is once again a no-op for codex. The agentic benchmark's codex now runs at medium
automatically (it launches via `rozum launch`).

## launch + bench — `rozum launch opencode`; agentic benchmark adds opencode, defaults to 35B
Completed: 2026-06-17

`rozum launch opencode …` now works like claude/codex: it writes a temp opencode config defining an
OpenAI-compatible `rozum` provider pointed at the local gateway (`OPENCODE_CONFIG`) and defaults the
model to `-m rozum/local`. opencode's tools (edit/bash/read/…) are built in, so a provider-only config
suffices. Verified: `opencode run` fixes the reverse-cli bug on Qwen3.6-35B-A3B (PASS).

`scripts/bench/agentic.sh` now compares **three** agents — claude, codex, AND opencode (all via
`rozum launch`, reusing the resident shared gateway) — and both bench scripts default to just
`Qwen3.6-35B-A3B-4bit` (the standardized local model; override with `AGENTIC_MODELS=…` / args). Unit
test for the opencode config writer; bench scripts syntax-checked.

## launch — `--lean` now also caps codex reasoning effort to `medium`
Completed: 2026-06-17

`rozum launch codex --lean` now appends `-c model_reasoning_effort=medium`. Codex inherits the
user's global `model_reasoning_effort` (often `xhigh`), which on a LOCAL model burns long reasoning
chains for little gain — measured codex on Qwen3-30B-A3B taking 7+ min/task (fix 433 s, test hit the
600 s wall). A mock-codex study also showed the meta-tools/heavy-reasoning path traps weaker models
(see the codex patch-barrier investigation). Capping to `medium` roughly halves the wasted
generation. Opt-in (only with `--lean`); skipped if the operator passes their own
`model_reasoning_effort`. Extends the existing claude `--lean`; 5 unit tests.

## models — Qwen3.6-35B-A3B is now the top recommended local model
Completed: 2026-06-16

Reordered `RECOMMENDED` (`src/models.rs`) to lead with `Qwen3.6-35B-A3B-4bit`: the only model to
score a perfect 10/10 (claude 5/5 + codex 5/5) in the 2026-06-16 agentic matrix once this session's
gateway fixes landed (unique tool-call ids, constrained-decode default, name-first envelope,
tool-role, clients_gone). Dropped the stale OOM caveat from its notes — chunked prefill + the MLX
memory cap removed the prefill-OOM, and the full matrix ran it cleanly at ~25 GB peak on a 36 GB Mac.
30B-A3B is now framed as the lighter/more-headroom alternative. All `RECOMMENDED` consumers (the
launch picker, `rozum list`, builtin tools) iterate in order, so the new ordering surfaces everywhere.

## mlx — globally-unique tool-call ids (fixes claude `debug` read-loop on 35B)
Completed: 2026-06-16

The mlx backend minted each `tool_use` id as `call_{i}` where `i` is the index **within one
response** — so the counter RESET to `call_0` every turn and the same id recurred across the
conversation. The Anthropic/OpenAI contract requires `tool_use` ids unique within a conversation so
the client pairs each `tool_result` back to its call. Claude Code, receiving a `call_0` it had already
seen in a prior turn, could not pair the result and dropped the turn as `[Tool use interrupted]` /
`(no content)`. The model therefore **never saw the output of its own tool call** (e.g. the file it
just Read) and re-issued it forever — the 35B `debug` read-loop.

Captured the real CC requests (temporary `ROZUM_DUMP_REQ`) and proved it: across all 5 requests CC
sent for the 35B run, the `src/lib.rs` content was ABSENT — every Read turn was `[Tool use
interrupted]`. The gateway stream itself was textbook-clean (single `tool_use`, `stop_reason=tool_use`,
TTFT 1.5 s, no timeout), and the model edits correctly when the Read result is supplied — so the only
defect was the colliding id. Why it bit 35B and not 30B-A3B: 35B opens with TWO tool calls
(`call_0`+`call_1`), so its turn-2 `call_0` collides with an id already in the conversation; 30B-A3B
emits one call per turn and dodged it.

Fix: `next_tool_call_id()` — a process-monotonic counter, so every id is fresh across all turns and
requests. Verified: 35B claude went **4/5 → 5/5** (`debug` now PASSes, and fewer turns — no more
loop), 30B-A3B stays **5/5** (no regression). Unit test asserts uniqueness.

## constrain — tool-call envelope forces `name` before `arguments`
Completed: 2026-06-16

Direct-probe root-cause of why Qwen3.6-35B-A3B's tool calls vanish in a rich agentic context:
given a faithful Claude-Code-style history (ls → cargo test → line-numbered Read), the model opens
the tool call with `arguments` FIRST and never emits the required `name` — `{"arguments":{…}}`. Free
decode then closes it name-less (→ `parse_tool_calls` drops it, no `name`); constrained decode
correctly blocks the close (name required) but JSON permits whitespace, so the weak model degenerates
into `\r` spam instead of recovering to `,"name":…`. Either way the call is dropped and the agent
loops. NOT a model-capability limit: in a clean single-turn context the same model emits a correct
`{"name":"Edit",…}`.

Fix: the tool-call `envelope` schema is now an ORDERED object — `name` must precede `arguments`
(added `ordered` to `Schema::Object`, enforced in `match_object`; plain JSON-schema / structured-output
objects stay unordered). Probe-verified: the exact reproduction now yields a clean
`tool_use` Edit (was dropped text). 2 unit tests.

Honest scope: this fixes the malformed-tool-call class, but does NOT by itself fix the claude `debug`
e2e on 35B — in the FULL Claude Code context (large system prompt + tool set) the model read-loops and
never emits the Edit at all (a separate tool-SELECTION issue the minimal probes don't reproduce).

## gateway — Anthropic tool results now render under the `tool` role (was `user`)
Completed: 2026-06-16

Anthropic has no `tool` role: a tool result arrives inside a `user` message as a `tool_result`
block. `anthropic_messages_to_internal` mapped that message straight to `Role::User`, so the
backend rendered the result as a plain user turn — contradicting the code's own documented
contract (`mlx_native_backend.rs:455`: "rendered under the `tool` role") and diverging from the
OpenAI and Responses paths, which both emit `Role::Tool`. Under Qwen3.6's chat template
(`chat_template.jinja`), only the `tool` role gets the trained `<tool_response>…</tool_response>`
wrapper; under `user` the model sees a bare paste. Fixed: split each `tool_result` out of the
`user` message into its own `Role::Tool` message, preserving block order (two unit tests).

Honest scope: this was investigated as the cause of claude `debug` read-looping on Qwen3.6-35B-A3B
(the model re-reads a file instead of editing). It is NOT — an e2e re-run with the fix shows the
SAME read-loop and FAIL: the model simply never emits an `Edit` (0 Edit/Write across the run),
independent of tool-result formatting. That failure is a model/agentic-capability limit, not a
gateway bug. This change ships only as the correctness fix it is.

## mlx — constrained tool-call decoding ON by default (fixes Codex tool delivery)
Completed: 2026-06-16

`ROZUM_MLX_CONSTRAIN` was opt-in: the fast (unconstrained) decode relied on `serving`'s JSON-repair
to recover malformed tool calls. But a local model driven under a foreign (Codex/Claude) tool schema
emits a structurally-broken Qwen3.6 XML form — e.g. `<tool_call>{"function=exec_command">{…}}` — that
repair can't recover (it's neither valid JSON nor the `<function=…>` XML the parser knows), so
`parse_tool_calls` returns nothing, the agent silently drops the call, and loops.

Measured on Qwen3.6-35B-A3B (KEEP=1 transcripts): with constraints OFF, **Codex `fix` and `debug` both
fail** — the model knows the fix (`s.chars().rev().collect()`, `a + b`) but every apply path emits an
unparseable `<tool_call>` that Codex never executes, so the file is never edited. With constraints ON,
the masked sampler forces a valid `{"name":…,"arguments":…}` body the moment `<tool_call>` opens, and
**both pass** (final files correctly edited, zero malformed `function=` markers in the transcript).

Flipped `constrain_enabled()` to default ON; opt out with `ROZUM_MLX_CONSTRAIN=0`. Cost: the B=1 masked
path is ~2-3× slower per token and disables batching, but only for tool-bearing requests on dense/hybrid
models — exactly the agentic coding path where a dropped tool call wastes the whole turn. (Does NOT fix
the separate claude `debug` failure, which is a read-only stall in the loop-breaker, not a tool-call
format problem.)

## gateway — a manual shared gateway no longer self-exits on `clients_gone`
Completed: 2026-06-16

The agentic benchmark (`scripts/bench/agentic.sh`) loads each model once as a shared `rozum gateway`
and reuses it across tasks via `rozum launch` (no `--model`). In practice only the first 2-4 claude
tasks per model ran; the rest — and **every** codex task — returned `rc=2` (`no gateway running`),
instantly (0.0 s, 0 turns). Not the 35B prefill OOM `rc=2`: reproduced on tiny Qwen3-4B, where OOM is
impossible.

Root cause: the lifecycle watchdog spawns whenever `idle_exit || unload_on_idle || launch_managed`.
The mlx-native backend `can_reload()`, so `unload_on_idle` is true and the watchdog runs even for a
plain `rozum gateway`. Its lifecycle-exit branch `if seen_lease { "clients_gone" }` was **not** gated
behind `launch_managed`, so the moment the first `rozum launch` client's lease dropped in the gap
between two invocations (`in_flight==0 && live_leases==0`), the gateway `process::exit(0)`'d. The
`/usr/bin/time` rusage showed a ~22 s gateway lifetime instead of the full run. `ROZUM_GATEWAY_IDLE_SECS=0`
did not help — it only disables the `idle` exit reason, not `clients_gone`.

Fix: gate `seen_lease -> clients_gone` behind `launch_managed`, matching the documented contract — a
manual gateway exits only via `idle_secs`; a launch-managed daemon still frees everything when its last
client leaves. Verified end-to-end: the full agentic matrix (5 models × claude+codex × 5 tasks) now runs
with **0 `rc=2`** (was ~30), the shared gateway survives the whole per-model run, and codex — previously a
silent victim of the dead gateway — executes normally. This also re-measured the chunked-prefill relief:
Qwen3.6-35B-A3B served all 10 real agentic tasks at ~25.8 GB peak (under the cap), no OOM.

## mlx — chunked prefill for the dense paths + lower default chunk (35B memory headroom)
Completed: 2026-06-16

The final matrix's only infra failure was the 35B-A3B gateway OOMing during a big agent prompt's prefill.
Root cause: the **dense** `qwen3` / `qwen3_moe` Generate prefilled the whole prompt in **one forward**, so
the activation spike (attention scores `[T, ctx]` + MLP + `lm_head` over all T positions) was unbounded;
the **hybrid** Qwen3.6 path already chunked, but its 2048-token default still spiked.

Fork (`9fa852f4`): both dense Prefill states now process the prompt in chunks of `prefill_chunk_size()`,
advancing the KV cache across chunks and eval'ing **only the cache** between them — so MLX's lazy graph
skips `lm_head` on the discarded intermediate chunks (needed only for the final position) and the peak is
bounded to one chunk. Added `KeyValueCache::collect_eval` (default no-op; `ConcatKeyValueCache` pushes its
state arrays) so the generic dense Generate can force the per-chunk cache. **Byte-exact verified on
Qwen3-4B** (chunk=64 vs single-shot → identical greedy output). Also lowered `PREFILL_CHUNK_DEFAULT`
2048→1024 (the 2048 spike + ~25 GB resident + KV + cache topped the MLX cap on a 36 GB Mac → process-fatal
Metal OOM); prefix-reuse makes per-turn prefill incremental, so it only adds a few eval syncs on turn 1.
Env-tunable via `ROZUM_MLX_PREFILL_CHUNK`. `Cargo.toml` bumps the fork rev. (35B OOM relief reasoned, not
yet re-measured — needs RAM headroom; pair with `ROZUM_MLX_CACHE_GB=1`.)

## launch/mlx — coherent context window: auto = model max, `--n-ctx` honored, one number
Completed: 2026-06-16

The context window was reported inconsistently for mlx-native (the default backend): `resolve_n_ctx`
printed `context window: 32768 (auto)` (a fixed fallback — `auto_n_ctx` never read the config for the
non-mistralrs build), `register_n_ctx` advertised that 32768 in `/v1/models`, yet the backend actually
loaded the model's true max (`ready (context 40960)`) and the overflow guard used that. Three different
numbers, and `--n-ctx` was silently ignored by mlx (the constructor never received it).

Now there's one source of truth. `auto_n_ctx` reads the model's `max_position_embeddings` for mlx too
(via the now-ungated `cached_config_json` + new `model_max_ctx`); mlx grows its KV cache lazily per token
and RAM-preflights each request, so the full max is the safe default (no upfront cost — that's why no cap,
unlike the mistralrs path which still caps at `N_CTX_AUTO_CAP` because it pre-allocates the pool).
`MlxNativeBackend::new` now takes `max_ctx: Option<u32>`: `Some(n)` from a user `--n-ctx` caps the window
to `min(model_max, n)` (so `--n-ctx` finally works on mlx), `None` (tests) uses the full max. So
`resolve_n_ctx`'s printed value, `register_n_ctx`, `context_window()`, and the backend's "ready" line now
all agree. Unit test pins `auto_n_ctx("Qwen3-4B") == 40960`. `src/main.rs`, `src/mlx_native_backend.rs`.

## launch — `--lean` also stabilizes the system-prompt prefix for KV-cache reuse
Completed: 2026-06-16

`--lean` now adds `--exclude-dynamic-system-prompt-sections` in addition to stripping non-coding tools.
CC embeds per-machine bits — cwd, env, **git status**, memory paths — in the system prompt; git status
changes every time the agent edits a file, so the ~1.4K-token system+tools prefix changes every turn,
busting the prefix-KV cache and forcing a full re-prefill each turn. The flag relocates those sections
into the first user message, keeping the static prefix byte-identical → cached across turns. It's safe
(relocates, removes nothing — unlike stripping the system prompt, which is load-bearing and breaks the
agent). Each lever is independent: tool-strip is skipped if you manage tools yourself, exclude-dynamic
is skipped if you set `--system-prompt`. Verified: `fix` 4/5 (no regression; the miss is the usual
weak-model give-up), and successful runs dropped from ~10 turns to a consistent 6. `apply_lean_tools`
renamed to `apply_lean_flags`; 4 unit tests. `src/main.rs`.

## gateway — count tool schemas + tool results in the prompt-token estimate (accurate overflow guard)
Completed: 2026-06-16

`estimate_prompt_tokens` replaces the old `total_message_text` + `estimate_tokens` pair at all three
handlers (OpenAI / Responses / Anthropic). The old estimate counted only `Text` blocks, so it **ignored
the parts that dominate an agentic request**: prior tool-call args, **tool results** (file dumps /
command output — often the largest blocks), and the **tool schemas** (which the chat template renders
into the prompt — ~5K tokens for Claude Code's ~33 tools). It under-counted a real coding turn
several-fold, so the context-overflow preflight (`est > ctx_win`) could wave through a prompt that
actually blows the model's window. Discovered while measuring `--lean`: the estimate stayed flat (1732)
even as the tool count swung 27→35. The new estimate sums all block types + each tool's
name+description+schema. Unit test covers the tool-result and tool-schema contributions. `src/gateway.rs`.

**Investigated, NOT shipped — CC system-prompt stripping.** The other half of the prompt overhead is
Claude Code's system prompt (~1,400 tokens). Measured the CLI levers: `--bare` cuts it to ~27 tokens
(est −71%) and `--system-prompt <minimal>` replaces it (est −48%). But both **break the agent** on local
models — `--bare`: 0/3 on the simple `fix` task (the model runs but can't complete the tool loop) and
flaky on `build`; `--system-prompt` minimal: 0/3 on `build`. Unlike the tool schemas (pure overhead,
safely stripped by `--lean`), the system prompt is **load-bearing** — it carries the operating
instructions the weak local model depends on — so it is left intact. (`--exclude-dynamic-system-prompt-
sections` only *relocates* per-machine sections for cache reuse; it's not a size win.)

## launch — `rozum launch --lean`: strip non-coding tools from Claude Code (smaller local-model prompt)
Completed: 2026-06-16

Claude Code ships its full tool set on **every** request to the model. Measured on a real
`rozum launch claude` (Qwen3-4B): **33 tools = ~4,878 tool-schema tokens** of fixed overhead — and
most are non-coding (7 `mcp__rozum__*` meeting-room tools, `Cron*`, `Task*`, `Workflow`,
`Enter/ExitPlanMode`, `Enter/ExitWorktree`, `Skill`, `Agent`, `ScheduleWakeup`, `LSP`, `Web*`,
`NotebookEdit`). On a local quantized model with a 16–40K context that bloats the prompt + KV cache,
slows prefill, and gives a weak model more ways to derail.

`--lean` injects `--disallowedTools` with the non-coding list (+ `mcp__rozum` server-wildcard) →
**4 tools / ~761 tokens (−84%)**, leaving the Read/Write/Edit/Bash core. Important correction the
measurement forced: **`--allowedTools` is a permission whitelist, not a request shaper** — it left the
tool count unchanged (even +2); `--disallowedTools` is the flag that actually removes schemas from the
request. `apply_lean_tools` (`src/main.rs`) injects at the end of the program vector (the flag is
variadic), is a no-op for non-`claude` programs, and is skipped if the user already manages tools.
`--lean` is in `KNOWN_BOOL_FLAGS` so it hoists when placed after the program name. The agentic
benchmark (`scripts/bench/agentic.sh`) now passes `--lean` (it subsumes the old `--disallowedTools
AskUserQuestion`). CC's fixed system prompt isn't strippable via CLI, so `--lean` targets the tool
schemas (the bigger, variable half). 2 unit tests + verified e2e. `src/main.rs`, `scripts/bench/agentic.sh`.

## gateway — break agentic stuck-loops server-side (agents that can't stop when done)
Completed: 2026-06-16

**Why.** Weak local models driving `rozum launch claude/codex` often don't STOP after finishing a
task — they run to `--max-turns`. A real `rozum launch claude` transcript (Qwen3-4B, `fix` task) showed
the mechanism: the model applies the correct fix on its **first** `Edit`, then re-issues the
byte-identical edit; each retry fails with `String to replace not found` (the target text is gone), and
the model reads the error as "retry" rather than "already done". It also skips the verification step, so
it never gets the positive closure signal. Tool-call format is **not** at fault — it's a small-model
state-tracking limit (0.5–4B loop hard, Llama-1B `fix` = 442 turns; Qwen3-30B-A3B doesn't).

**What.** The gateway sees the whole conversation each turn, so it now detects the stuck signature and
short-circuits the next doomed turn with a synthetic stop. `detect_stuck_loop` + `chat_or_loopbreak`
(all three protocol handlers — OpenAI / Responses / Anthropic — route through it) + `synthetic_stop_stream`
(a one-shot `TextDelta` + `Done{EndTurn}` that every per-protocol serializer renders as an ordinary
`finish_reason: stop`). Two signatures, because the loop surfaces differently per harness:
1. **structured** — the same tool call (name+input) repeated ≥3× with **error** results (Codex /
   Responses, and CC when tool use completes);
2. **text-repeat** — CC headless *interrupts* its own doomed tool use and records the turn as a text
   placeholder (`[Tool use interrupted]` / `(no content)`), so the gateway never sees structured tool
   blocks. CC ping-pongs between a re-diagnosis and the interruption, so "N-in-a-row" misses it; instead
   we fire when any single assistant text recurs ≥3× within the recent 6-turn window.

**Verified e2e** (Qwen3-4B `fix`): the gateway logged `stuck_loop_broken`, the synthetic stop became
CC's final assistant turn, and the run concluded `subtype=success` at 7 turns instead of looping.
Thresholds are conservative (a healthy agent never re-sends a byte-identical failed call nor repeats the
same assistant text) — 5 unit tests cover both signatures and the distinct-progress non-firing case.
Also shipped as band-aids: `scripts/bench/agentic.sh` `MAX_TURNS` 30→15 and per-task
"you-are-DONE→STOP" prompt nudges (fix/debug also say "String to replace not found = already applied,
don't retry"). `src/gateway.rs`, `scripts/bench/agentic.sh`, `SPRINT.md`.

## mlx-native — cap MLX unified memory (no more 28 GB hoard / rc=2 cascade) + constrain back to opt-in
Completed: 2026-06-16

MLX kept freed Metal buffers cached and grew its footprint to a RAM fraction (~28 GB) regardless of
model size, which starved agent processes (the agentic-benchmark rc=2 cascade), pushed big models
near-OOM, and forced a per-task gateway reload. The vendored fork now exposes the mlx-c memory
setters (`mlx_rs::memory::set_cache_limit` / `set_memory_limit` / `set_wired_limit`, fork
`693f89ab`); rozum's `cap_mlx_memory()` (called at model load) caps them — `ROZUM_MLX_CACHE_GB`
(default 4) and `ROZUM_MLX_MEM_GB` (default total RAM − 8). The cache cap is the lever:
**a Qwen3-4B gateway serving 12 requests now peaks at 2.5 GB (was ~28 GB)** — the cache no longer
accumulates, so the cascade is gone and a shared per-model "load once" gateway is viable again.

Also, with the loose-JSON repair shipped, `ROZUM_MLX_CONSTRAIN` is back to **opt-in** (`=1`): the
repair recovers the common malformations on the fast path (Coder-7B agentic 2→4/5, constrain-off), so
the B=1 masked decode is only worth its cost for the rare case repair can't disambiguate. And the
agentic benchmark default model set is now just the 5 that actually do agentic coding (4B–35B) — the
weak (Qwen2.5-0.5B / Qwen3-0.6B / Llama-3.2-1B, `greet`-only even with repair) and template-less
(gemma, Phi-3, SmolLM2, Mistral-v0.3) models were deleted from cache + benchmark. `Cargo.toml`,
`src/mlx_native_backend.rs`, fork `mlx-rs/src/memory.rs`, `scripts/bench/agentic.sh`, `SPRINT.md`.

## serving — repair malformed tool-call JSON (recover calls with constrain OFF) + bigger timeouts
Completed: 2026-06-16

`serving::parse_loose_tool_calls` now **repairs** a malformed `{"name":…}` instead of dropping it.
The classic weak-model mistake is unescaped quotes inside a string value (`"content":"…println!("{}",
x)…"`), which breaks both `serde_json` and the brace scanner. When the strict parse finds nothing,
`repair_tool_object` does one tolerant scan that disambiguates a content `"` from a structural one by
lookahead — a `"` closes the string only if the next non-ws byte is `:` / `}` / `]` / EOF, or a `,`
followed by the next key's `"` — and escapes content quotes + raw control chars (so `println!("{}", x)`,
including the tricky `"{}"`-then-comma, is recovered). Runs only after a failed strict parse → no false
positives. **This makes weaker models reliable with `ROZUM_MLX_CONSTRAIN=0` (the fast path)** instead
of needing the B=1 masked decode: Coder-7B `build` now passes constrain-off (was a fail — lost the
call). Surfaced by the agentic benchmark. Also: `ROZUM_GEN_TIMEOUT_SECS` default 180 → **300** (more
headroom for slow/big quantized models); the bench `agentic.sh` drops the template-incompatible
`Mistral-7B-v0.3` from its default set and raises its run/gen timeouts. `src/serving.rs` (12 tests),
`src/gateway.rs`, `scripts/bench/agentic.sh`.

## mlx-native — stop streaming loose tool calls as text (fixes agentic re-emit loop) + constrain on by default
Completed: 2026-06-16

Two changes that, together with the earlier constrained-decode + mask fixes, make a small model run a
full agentic task **cleanly to completion**:

- **Don't leak a loose tool call as text.** The streamer suppressed only the `<tool_call>` envelope, so
  a loose ```json / `{"name":…}` tool call was emitted BOTH as raw text AND (at finalize) as a
  `tool_use`. The model then saw its own call twice in the next turn and kept re-emitting it — an
  infinite agentic loop that burned the whole timeout on an already-passing task. `BatchSeq` now
  suppresses loose tool markup too (`tool_markup_at` + a held-back trailing ``` fence in tool requests,
  flushed at finalize if it wasn't a call). Validated: Qwen2.5-Coder-7B `build` now finishes in 4 turns,
  `rc=0`, no loop (was: looped to the 500 s timeout) — clean `tool_use` blocks, `cargo run` → `olleh`.
- **`ROZUM_MLX_CONSTRAIN` is on by default** (`=0` to disable). The masked decode forces valid,
  schema-conforming tool-call JSON even from weaker models; the perf cost (B=1) is worth the
  reliability for the typical single-stream agentic use. `src/mlx_native_backend.rs`, 2 tests.

## mlx-native — constrained decoding also catches loose (markdown / bare) JSON tool calls
Completed: 2026-06-16

`ROZUM_MLX_CONSTRAIN` (the opt-in masked tool-decode) now activates not only on the trained Qwen
`<tool_call>` envelope but also on a **loose** `{"name":…,"arguments":…}` — bare or in a ```json fence
— that smaller models emit when driven by a foreign (Claude/OpenAI) tool schema. The constraint masks
the sampler to valid schema-conforming JSON, so the model **cannot** emit the malformed JSON it
otherwise tends to (e.g. unescaped quotes in a `"content"` arg of Rust code), which the parser had to
drop. `json_region` gains a fallback: when there is no `<tool_call>`, `find_loose_tool_json` locates a
`{` whose first key is `name` (so a `{` in prose / a code example isn't mistaken for a call) and
constrains from there. Validated: Qwen2.5-Coder-7B `build` went from `tool_uses=1` (2nd call lost to
bad JSON) to **9 valid tool calls** — it now writes both `Cargo.toml` and a correct `src/main.rs`.
(The masked path is B=1, hence still opt-in for the perf cost.) `src/mlx_native_backend.rs`, 1 test.

## mlx fork — fix multi-turn crash on qwen2 / llama prefix reuse (mask offset)
Completed: 2026-06-16

The agentic benchmark surfaced a real backend crash, not just model weakness: a 2nd-turn agentic
request to a **qwen2** (Qwen2.5 / Coder) or **llama** (Llama-3.2 / Mistral / Phi-3) model died with
`[broadcast_shapes] Shapes (T,T) and (1,H,T,N+T) cannot be broadcast` → HTTP 500, breaking the tool
loop. Cause: those two model forwards built the causal mask with `create_additive_causal_mask(T)` —
a `(T,T)` mask that **ignores the KV-cache offset**. With prefix reuse a continuation prefills `T>1`
new tokens against an `N`-token cache, so attention is `(B,H,T,N+T)` and the `(T,T)` mask can't
broadcast. `qwen3` / `qwen3_moe` already used the cache-aware `create_attention_mask(&h, cache, …)`
(hence 30B-A3B never crashed but Coder-7B did). Ported the same to qwen2 + llama in the vendored
mlx-rs fork (`838a39ab..bddd6feb`, branch `rozum-hybrid-decode`); `Cargo.toml` bumped to the new rev.
The no-cache single-turn path is byte-identical (offset 0 → same `(T,T)` causal mask), so no
regression. Validated: Coder-7B `build` no longer crashes on turn 3 (was rc=1 + 500, now rc=0, no
`broadcast_shapes`). Note: the crash and a small model's weak multi-step agentic ability are
independent — Coder-7B still only manages one tool call, but the backend no longer falls over.

## serving — robust tool-call parsing, unified in a shared module
Completed: 2026-06-16

New `src/serving.rs` holds the engine-agnostic tool-call parser; the MLX backend's whole-text
`parse_tool_calls` and the GGUF streaming detector's `tool_name` both call it now (the duplicated
body-parsing is gone — first slice of `extract-shared-serving-helpers`, L1). The parser is also
**robust to models that don't emit the trained `<tool_call>` envelope**: smaller models (4B–7B)
driven by Claude Code's / Codex's foreign tool schema fall back to a bare or ```json-fenced
`{"name":…,"arguments":…}`, which the old parser silently dropped — the agent then executed nothing.
A fallback now recovers those via a string-aware balanced-brace scan (so braces inside a `"content"`
arg don't unbalance it) with a strict `arguments`-is-object guard, and it runs **only** when there were
no native `<tool_call>` blocks, so a legitimate ```json example in a normal answer is never mistaken
for a call. The agentic benchmark proved it end-to-end: Qwen2.5-Coder-7B `build` went from `tool_uses=0`
(nothing created) to actually executing its `Write` call. `src/serving.rs` (8 unit tests),
`src/mlx_native_backend.rs`, `src/gguf.rs`, `src/lib.rs`.

## bench — agentic end-to-end benchmark (real `rozum launch` Claude Code / Codex)
Completed: 2026-06-16

`scripts/bench/agentic.sh`: drives a **real `rozum launch claude` / `rozum launch codex`** against a
local MLX model — the whole stack as a user runs it. Each run is a private in-process model
(`--dedicated`), so launch applies its Claude-Code prompt trimming and Codex provider config; every
agent flag is passed on the command line. For each `agent × model × task` it gives a real coding task
(trivial → hard, with tool use: `greet` / `build` / `fix` / `test` / `debug`, reusing the e2e tasks),
**verifies the result independently** of the agent (files exist, `cargo test` green, `cargo run --
hello` == `olleh`), and measures wall time, the whole process-tree peak RAM + CPU%, and the model's
resident footprint (`/usr/bin/time -l` on the rozum process). **Two independent timeouts** per the
intended design: `ROZUM_GEN_TIMEOUT_SECS` (engine, default 180) bounds a single model request; a
generous `RUN_TIMEOUT` (default 1200) bounds the whole task (many model calls + cargo builds, which
don't depend on any one request). Context defaults to the model max (auto, no `--n-ctx` cap).

Findings: verification is independent of the agent's exit (a run that exits non-zero, or is killed by
its timeout, still PASSes if the artifact is correct — Qwen3-30B-A3B `build` exited rc=1 but passed
with 31 tool calls). `rozum launch` already trims the Claude-Code prompt
(`CLAUDE_CODE_DISABLE_BUNDLED_SKILLS` / `_GIT_INSTRUCTIONS` / `_CLAUDE_MDS` / `_NONESSENTIAL_TRAFFIC`,
`DISABLE_NON_ESSENTIAL_MODEL_CALLS`), cutting footprint ~16.8 → ~13.4 GB for Qwen3-4B even at 2× the
context — **but the trim has a cost: weak 4B–7B models then emit tool calls as markdown JSON the gateway
can't parse and fail the agentic loop, while a strong MoE (30B-A3B) drives the full read/edit/run
loop.** CPU% is GPU-bound-low for the model (MLX runs on Metal) and mainly reflects the agent + cargo.
`results/` gitignored.

## gateway — generation inactivity timeout + local-model benchmark harness
Completed: 2026-06-16

A wedged in-process generation can no longer hang a client forever. A 13-model benchmark
(`scripts/bench`) surfaced it: under memory pressure the 35B-A3B's longest task stalled for ~4.5 h —
a single Metal eval thrashing swap blocks inside one FFI call, so the decode loop's per-token
`is_cancelled()` check never runs. The gateway now wraps **every backend stream** (all dialects,
streaming + non-streaming) in an inactivity timeout: if no event arrives within
`ROZUM_GEN_TIMEOUT_SECS` (default 180; `0` disables), it cancels the job and ends the stream with
`ModelError::Timeout` → HTTP 504 instead of hanging. The cancel lets the worker abandon the job the
moment its eval unblocks. Engine-agnostic (mlx-native, gguf, remote). `src/gateway.rs`,
`src/backend.rs`; 3 unit tests.

The **benchmark harness** (`scripts/bench/run.sh` + `tasks.jsonl`): 8 tasks of increasing difficulty,
per model — load time, peak physical-memory footprint (`/usr/bin/time -l`), TTFT (first content
token), pure decode rate (tok/s excluding prefill), and a heuristic answer-key PASS/FAIL. A per-model
warm-up request removes the cold-start blip; a per-request `curl --max-time` ceiling is the
harness-side backstop. Headline: MoE breaks the size→speed curve (Qwen3-30B-A3B 16 GB ≈104 tok/s,
faster than every dense 7B; dense-27B ≈15 tok/s); a cold hybrid/MoE first token costs up to ~33 s
(Metal kernel JIT + weight page-in). `results/` is gitignored (reproducible).

## portability — the durable core is Linux-buildable + CI-enforced
Completed: 2026-06-15

The "durable layer" portability thesis is now **verified, not folklore** (`portability-platform-
features`, durable-core part). `cargo build --no-default-features` builds **and tests** the whole
non-backend layer — the `ChatBackend` SPI, the gateway (with HTTP/remote backends), the agent
runtime, the cascade router, the concurrency layer, config, and the meeting room — with **no native
toolchain** (no Metal/Xcode), 271 tests. A new CI **`linux-core`** job (`ubuntu-latest`) runs exactly
that on every push, so a Linux regression in the durable layer fails CI. (One MLX-only test module
was gated on the `mlx-native` feature so `--no-default-features` test-compiles.) The macOS CI job now
also runs the tests.

Bare `cargo build` on Linux is **not** yet first-class — the native backends are Apple-Metal-bound
(mlx-sys; `llama-cpp-2 { features = ["metal"] }`), so a target-conditional default + a gguf-CPU/CUDA
path is a larger effort that can't be validated from macOS (tracked with `portability-cuda-gguf`).

Also backlog hygiene: closed several no-longer-developed items — `candle-real-streaming`,
`gguf-tool-use-non-qwen` (won't do; native MLX covers tool-use), `concurrency-preemption` (mistralrs
fork, mostly moot — mlx-native does continuous batched decode), and the superseded gguf-adapter
stubs. `.github/workflows/ci.yml`, `Cargo.toml`, docs. 282 (default) / 271 (no-default).

## gateway — install as an always-warm user service (+ closed streaming-output)
Completed: 2026-06-15

`rozum service {install,uninstall,start,stop,status}` registers the local gateway as a **user
service** — launchd on macOS, `systemd --user` on Linux — so it starts at login and is kept alive,
instead of the lazy-spawn + idle-exit default (`shared-gateway-service`). `start`/`stop` toggle the
running state (launchd `load`/`unload`, `systemctl --user start`/`stop`) without removing the
installed file; `install`/`uninstall` write/remove it. `--model` is repeatable / comma
(a cascade), with `--port/--n-ctx/--offline/--strategy`; `ROZUM_CASCADE`/`ROZUM_CONFIG` from the
installing shell are captured into the service environment so a named/JSON cascade keeps working.

The plist / unit **generation** is a pure, unit-tested library module (`src/service.rs` —
`launchd_plist`, `systemd_unit`, the install paths; XML-escaped values, `RunAtLoad`+`KeepAlive` /
`Restart=on-failure`). The binary writes the file and drives `launchctl` / `systemctl` (operator
runs it — it touches the real service manager). `docs/specs/shared-gateway-service.md`; 4 tests.

Also re-closed `streaming-output` (a backlog doc-edit lost in an earlier branch shuffle): the gateway
already streams token-by-token on all three dialects. 282/0.

## shared-gateway-multislot — warm idle eviction + persisted usefulness
Completed: 2026-06-15

The two finishing touches on the warm cache. **Idle-timeout eviction**: the gateway's lifecycle
watchdog now also sweeps warm secondary residents — `sweep_idle_warm` drops a warm model that's been
idle (no in-flight) past `unload_idle_secs`, freeing its RAM on a blocking thread (joins the `!Send`
worker), just like the primary's idle-unload. Each warm entry tracks its own last-activity (set on
request start, refreshed on lease drop) so a busy model is never swept. **Persisted usefulness**: the
per-model `UsageStats` is now opened at `$XDG_STATE_HOME/rozum/gateway/warm-usage.jsonl`, so the
frequency×recency ranking that decides which models stay warm survives a daemon restart (tests stay
in-memory).

`src/gateway.rs`; 2 new tests (sweep evicts a long-idle model, keeps a busy one). 278/0.

## shared-gateway-multislot — Phase 2: the warm cache (on by default)
Completed: 2026-06-15

The shared gateway can now keep **more than one model resident** so two clients hitting two
different local models don't thrash a single slot (`shared-gateway-multislot` Phase 2). It's an
**additive warm cache** layered on the untouched single-resident core, **on by default** (opt out
with `ROZUM_MULTISLOT=0`) and a **strict no-op for single-model traffic** — so the common
Claude-Code/Codex case is byte-for-byte unchanged.

`enter(req.model)` routes a request for a *different*, warmable model (a known cached local that
fits the memory budget) to a **warm secondary resident**, built through the existing backend builder;
admission and eviction run through the Phase-1 `resident::plan_residency` planner (keep the most
useful that fit, evict the least-useful *idle* ones, fall back to a primary swap when a model's too
big). A warm entry carries its **own in-flight counter**, decoupled from the primary `generating`,
so warm traffic can never hold up a primary swap/unload drain; eviction is idle-only and drops the
backend on a blocking thread (joins the `!Send` worker) like the existing unload. Any miss
(unknown/remote model, won't fit, build failure) cleanly falls back to the primary path.

`src/gateway.rs`; 4 tests via the mock-builder harness (serve-second-model, fall-back-when-too-big,
skip-unknown/remote, evict-idle-to-make-room). 276/0. **Real-model validation** (two real models
co-resident; eviction frees RAM) is the operator's to run — the spec lists the checklist. Deferred:
idle-*timeout* warm eviction (today freed only under pressure) and persisting `UsageStats`.

## concurrency — shared cross-resident GPU gate
Completed: 2026-06-15

The missing primitive for running more than one model on one GPU without oversaturating it
(`concurrency-multi-instance`, the core). Per-model admission bounds each backend's own concurrency,
but two *distinct* resident models each admitting up to their cap could together run 2× the GPU's
sweet-spot of concurrent prefills. A **process-wide GPU gate** — a semaphore sized to one GPU's
concurrent-prefill sweet spot (`DEFAULT_SEQS_CEILING`; `ROZUM_GPU_GATE` overrides, `0` disables) —
is now shared by every local (`admit_wrap`-ped) backend: each request acquires it *in addition to*
its per-model slot, so total concurrent local prefills across all residents stay bounded.

It's acquired **after** the per-model admit (so a request parked for its own slot never holds a
scarce GPU permit — no priority inversion), held for the request, and released on
completion/disconnect. It composes with the cascade residency lanes and the per-model adaptive
ceiling (just another `min()`), and is a **no-op for a single resident** (the gate ≥ the per-model
cap, so it never binds) — hence safe to leave on by default.

`src/concurrency.rs` (`global_gpu_gate`, `AdmittingBackend::with_gpu_gate`); 2 tests
(shared-across-two-backends, no-bind-below-size). 272/0.

## shared-gateway-multislot — Phase 2 design (live-daemon wiring)
Completed: 2026-06-15

The Phase-2 design for wiring the residency core into the live `Switchboard`
(`docs/specs/shared-gateway-multislot.md`). The approach is deliberately **additive + opt-in**: keep
the single-resident core (swap/drain/unload/idle) untouched and add a **warm cache** of secondary
residents gated by `ROZUM_MULTISLOT` (default off ⇒ byte-for-byte today's behavior). `enter(req.model)`
routes a known cached-local request to the warm cache; a warm entry has its own in-flight counter
(decoupled from the primary drain, so it can't deadlock a swap); admission/eviction goes through
`resident::plan_residency`; eviction is idle-only with the existing `spawn_blocking` drop care. The
spec calls out injectable weight/budget seams so the logic is mock-testable, and the exact
**real-model validation checklist** (two small models co-resident, eviction frees RAM, big-model
swap, flag-off regression).

Implementation is **deferred on purpose**: it changes the live serving path that backs Claude
Code / Codex, and its memory / `!Send`-worker-drop behavior can only be confirmed on the target
machine — so it's best written as small, individually-validated steps rather than one unvalidatable
big-bang. Phase 1 (the tested decision core, `src/resident.rs`) already shipped.

## docs hygiene — add-a-backend checklist + prompt policy
Completed: 2026-06-15

Two documentation items, no code. **Add-a-backend checklist** (`portability-new-backend-checklist`):
the recipe for a new runtime/hardware leaf is now written down in
`docs/specs/portability-and-the-backend-spi.md` — the 2 required `ChatBackend` methods (`chat`,
`context_window`), the opt-in hooks (`concurrency_capacity`, `count_tokens`, `label`), bring your own
template/tokenizer/cache, register it in `main.rs` + `config.rs::ACCEPTED_ENGINES`, test feature-free.

**Prompt policy** (`prompt-policy`, `docs/specs/prompt-policy.md`): a decision, not a feature. The
gateway is a transparent provider — it passes the client's own system prompt through unchanged and
does *not* inject per-model prompts (that would corrupt CC/Codex); raw is the default and only mode,
the lone shaping being the existing `--enable-thinking` toggle. Per-model style/persona belongs to
the caller (the agent runtime's `system` arg, room etiquette), not the gateway passthrough.

## concurrency — tokenizer-pluggable request cost (+ a char-vs-byte heuristic fix)
Completed: 2026-06-15

The admission cost estimate is now **tokenizer-accurate when a backend can provide it**, and its
fallback heuristic is fixed (`concurrency-cost-tokenizer`). `RequestCost::estimate(req,
count_tokens)` uses a new `ChatBackend::count_tokens(text) -> Option<usize>` hook (default `None`)
per text block, summed over the prompt (text, tool results, and rendered tool calls); the
`AdmittingBackend` passes its backend's `count_tokens`.

The fallback heuristic had a real bug: it estimated tokens from `str::len()` (**bytes**), so a
non-ASCII prompt — e.g. Cyrillic, where each char is ~2 UTF-8 bytes — was costed ~2× too high,
skewing the shortest-job-first admission order. It now counts **characters** (`chars().count() / 4`).

`src/concurrency.rs` + `src/backend.rs`; 3 tests. 270/0. The MLX/GGUF tokenizers live in `!Send`
worker threads, so wiring their exact `count_tokens` (a worker round-trip / cached cell) is a
follow-up; remote backends have no local tokenizer, so they stay on the heuristic.

## shared-gateway-multislot — adaptive residency, Phase 1 (decision core)
Completed: 2026-06-15

Toward serving more than one model behind the shared gateway **without thrashing**, the adaptive
decision core (`src/resident.rs`). The policy the user asked for: small requested models that fit
*and are statistically useful* stay co-resident; the least-useful idle model is evicted to make
room; a model too big to co-reside falls back to a swap (thrash is unavoidable for big models) — so
the gateway keeps the **best arrangement possible under the memory budget**.

`UsageStats` is a persisted (JSONL, replay-on-open) per-model request history; `ModelUsage::utility`
= request count × an exponential recency decay (1 h half-life), so frequent+recent models rank high
and stale ones decay out of the warm set. `plan_residency` is the pure, fully-tested planner: greedy
*keep the highest-utility models that fit* (always including the just-requested one), evict the rest
(idle only — a busy model is never dropped mid-stream), and flag `oversubscribed` when the request
can't co-reside (the caller swaps).

It's the **pure core** — reasons over `(model, weight, busy, utility)` only, no backends/daemon — so
it's fully unit-tested here (7 tests). Phase 2 (wiring it into the live `Switchboard`: a model-keyed
resident set, routing by `req.model`, per-model generating/idle-unload) is a separate step that
needs real-model daemon validation. `src/resident.rs`; 267/0.

## quick wins — CI smoke gate, README refresh, and `--offline`
Completed: 2026-06-15

Three small high-value items.

**CI** (`ci-smoke`): there was no CI at all. A GitHub Actions workflow now gates `master` push/PR —
`cargo build --lib --bin rozum` + `cargo test --lib` (feature-free, no Xcode/Metal) on `macos-latest`,
with cargo caching. Protects the pure-Rust core (SPI, gateway, agent runtime, cascade router,
concurrency, config — 260 tests) on every change.

**README** (`docs-bootstrap`): the README documented only the meeting-room half. Added a "Local LLM
gateway & model cascade" quickstart (the gateway, `rozum launch`, the picker, the cascade
model-list + `--strategy`), refreshed the project layout and the dev/test instructions.

**`--offline`** (`cascade-offline`): a new flag on `launch`/`gateway` that disables all remote/cloud
cascade tiers — use only local models. It sets `ROZUM_OFFLINE` (the spawned daemon inherits it);
`build_remote_tier` then skips every remote tier (dropped like any unbuildable tier — locals survive,
an all-remote cascade errors), and the launch picker hides the Anthropic + OpenAI entries.

`.github/workflows/ci.yml`, `README.md`, `src/main.rs`. 260/0.

## cascade — rename the `alwaysCheapest` strategy to `cheapest`
Completed: 2026-06-15

The user-facing strategy name is now just **`cheapest`** (config, `--strategy`, JSON/TOML) instead of
`alwaysCheapest`. `StrategyName::AlwaysCheapest` → `StrategyName::Cheapest`; serde serializes it as
`"cheapest"` and accepts the old `"alwaysCheapest"` via a `#[serde(alias)]` (so existing configs keep
working). `parse_cli` takes `cheapest`/`cheap`/`alwaysCheapest`. The internal runtime enum
`RoutingStrategy::AlwaysCheapest` is unchanged (descriptive, not user-facing). Docs/help updated. 1
new test. 260/0.

## cascade — repeatable --model + a --strategy flag
Completed: 2026-06-15

Two CLI ergonomics on top of the simple model-list path (`cascade-cli-ergonomics`).

`--model` is now **repeatable** on `launch` and `gateway`: `--model qwen3-4b --model claude-haiku-4-5
--model gpt-4o` builds the same cascade as the comma form `--model "qwen3-4b,claude-haiku-4-5,gpt-4o"`
(each value may itself be a comma list — `join_models` flattens and re-joins, then the auto-cascade
path orders them).

A new **`--strategy`** flag picks the cascade start-tier strategy — `classify` (default), `learned`,
or `alwaysCheapest` — without writing a full spec. It flows through `ROZUM_CASCADE_STRATEGY` (so a
spawned shared-gateway daemon inherits it) and overrides the strategy of whatever spec is built
(list, TOML, or env JSON). `StrategyName::parse_cli` parses it case- and separator-insensitively.

`src/main.rs` + `src/cascade/spec.rs`; 1 new test. 259/0.

## cascade — the simple path: just list models, rozum builds the cascade
Completed: 2026-06-15

Configuring a cascade no longer needs a full spec — **just name the models** and rozum auto-orders
and auto-policies them (`cascade-model-list`). `from_model_list(names)` classifies each name
(`classify_model_name`: `claude…` → native Anthropic, `gpt…`/`o1…`/`o3…` → OpenAI, everything else →
a local model) and orders them cheapest→most-capable — locals first (free, on-device) by parameter
size (MoE ranked by *active* params, the `Nbit` quant suffix ignored), then remotes by provider tier
(haiku/mini < sonnet/4o < opus/o1). The strategy defaults to `classify`, so simple requests start at
the cheapest tier and hard ones start higher.

Two ways in:
- **A comma-separated model string** — `--model "qwen3-4b,claude-haiku-4-5,gpt-4o"`, or a request's
  `"model"` — builds an auto-ordered cascade. One name = a plain model (not a cascade).
- **The launch picker** now lists hosted **Anthropic + OpenAI** models alongside local ones, and
  **multi-select** (e.g. `2 9 4`) forms a cascade from the chosen models.

`build_remote_tier` defaults the OpenAI endpoint (`https://api.openai.com/v1`) so a bare `gpt-4o`
resolves; `models::RECOMMENDED_REMOTE` is the picker's hosted-model catalog (you can also type any
exact model id). `src/cascade/spec.rs` + `src/models.rs` + `src/main.rs`; 2 new tests. 258/0.

## cascade — named configs in rozum.toml ([cascade.<name>] tables)
Completed: 2026-06-15

Cascade configs can now live in `rozum.toml` instead of an env var (`cascade-toml-config`), so a
named cascade survives a restart without re-exporting JSON. A `[cascade.<name>]` table is a
`CascadeSpec` — `strategy`, `max_escalations`, and `[[cascade.<name>.tiers]]` entries:

```toml
[cascade.default]
strategy = "classify"
max_escalations = 1
  [[cascade.default.tiers]]
  model = "mlx-community:Qwen3-4B-4bit"
  [[cascade.default.tiers]]
  model = "claude-haiku-4-5"
  location = "remote"
  api = "anthropic"
```

`model: "cascade"` selects `default`; `model: "cascade:<name>"` selects `<name>`.
`RuntimeConfig` gained a `cascades` map + `cascade_spec(name)`; the gateway's `load_cascade_spec`
checks the TOML table first and **falls back to the env JSON** (`ROZUM_CASCADE` /
`ROZUM_CASCADE_<NAME>`), so both paths work. `TierSpec`/`CascadeSpec`/`StrategyName` gained
`PartialEq`/`Eq` (so they can embed in the `Eq` `RuntimeConfig`).

`src/config.rs` + `src/main.rs`; 1 new test. 256/0. This closes the cascade-router's open follow-ups
— the feature is done end to end.

## cascade — Phase 7 follow-ups: adaptive judge threshold + persisted health
Completed: 2026-06-15

The two remaining Phase-7 pieces, both driven by the learned stats (`cascade-p7-adaptive`).

**Adaptive judge threshold.** A `(task-class, model)` whose historical accept-rate has earned trust
now gets a *more lenient* L2 judge — `StatsStore::is_trusted(…)` →
`CascadeBackend::effective_judge_threshold` lowers the configured threshold by `judge_trust_discount`
(default 0.1). So once a cheap model has proven itself on a class, we stop burning escalations
second-guessing it on a borderline judge score; a model with no track record keeps the strict base
threshold. Off with `judge_trust_discount = 0.0`.

**Health-pattern persistence.** Cooldowns now survive a restart. `HealthRegistry::open(path)` replays
a JSONL of health transitions (`HealthEvent` = a failure with its wall-clock cooldown deadline, or a
recovery; latest event per model wins). A cooldown still in the future is restored as an active
`Unavailable` entry — the `Instant` rebuilt from the persisted unix deadline — carrying its `fails`
count, so a remote whose hourly quota is exhausted stays parked across a daemon restart instead of
being re-probed immediately, and exponential backoff keeps escalating. Opt-in via
`CascadeConfig.health_path` (`None` = in-memory, unchanged).

`src/cascade/{stats,health,mod}.rs`; 4 new tests. 255/0. With this the cascade-router's learned track
is complete; the only open follow-up is a `rozum.toml [cascade]` config schema.

## cascade — Anthropic-native remote tier (Claude as the strong tier)
Completed: 2026-06-15

A cascade can now use **Claude natively** as a remote tier over Anthropic's `/v1/messages`, not just
through an OpenAI-compatible proxy (`cascade-anthropic-tier`). `TierSpec` gained `api: RemoteApi`
(`openai` default, `anthropic`). When a remote tier is `anthropic`, the gateway builds the native
`AnthropicHttpBackend` — endpoint defaults to `https://api.anthropic.com`, the key comes from
`ANTHROPIC_API_KEY` (required; the tier is skipped if it's absent, like any unbuildable tier);
OpenAI-compatible tiers default their key to `OPENAI_API_KEY`. `api_key_env` and `endpoint` override
the defaults either way.

So a frugal "local-first, Claude-on-escalation" cascade is now a one-liner:

```
ROZUM_CASCADE='{"tiers":[{"model":"mlx-community:Qwen3-4B-4bit"},
  {"model":"claude-haiku-4-5","location":"remote","api":"anthropic"}],"strategy":"classify"}'
```

`src/cascade/spec.rs` + `src/main.rs`; 2 new tests. 251/0.

## cascade — resource headroom: back off before the OOM (P9 signal set complete)
Completed: 2026-06-15

The last unfed Phase-9 signal is now live (`cascade-adaptive-headroom`): **free-memory headroom**, so
the adaptive loop backs concurrency off *before* an OOM instead of recovering from one.

`system_memory_headroom()` is a std-only, cached (~1s) probe — macOS `vm_stat`
(free+inactive+speculative+purgeable pages) over `sysctl hw.memsize` → a free-RAM fraction `[0,1]`.
On Apple Silicon's unified memory the GPU shares system RAM, so one system-wide figure applies to
every local backend; the probe lives in the feature-free concurrency layer (no MLX coupling) and
returns `None` off macOS (the signal is simply skipped). The `AdmittingBackend` feeds it on every
completed request; below the controller's `min_headroom` (0.15) it's a hard red and the admission
ceiling backs off.

The probe is **injectable** (`AdmittingBackend::with_headroom_probe`) — a clean seam for a
GPU-specific probe later and for deterministic tests. `src/concurrency.rs`; 2 new tests. 250/0.

With this, `ConcurrencySample` is **fully fed** — the per-model AIMD controller now reacts to
overload, throughput (success), latency (a per-token baseline → ratio), answer quality
(`report_quality`), and resource headroom. The cascade-router and its adaptive concurrency loop are
feature-complete.

## cascade — richer adaptive signals: latency + quality into the live loop
Completed: 2026-06-15

The adaptive concurrency loop (Phase 9) now reacts to two more signals beyond raw overload/success
(`cascade-adaptive-signals`), so it finds each model's sweet spot by *both* throughput and quality.

**Latency.** The `AdmittingBackend` times every request and tracks a low-concurrency `ms/token`
baseline (EWMA — the model's *unsaturated* speed). Under concurrency, a request's `per_token /
baseline` becomes the `latency_ratio`; a ratio over the controller's ceiling (a latency cliff —
GPU contention) backs the admission ceiling off. It's cost-normalized (per output token) with a
min-token floor, so prefill-dominated tiny responses don't add noise, and the baseline only moves on
unsaturated requests (which can't be "too slow").

**Quality.** A new `ChatBackend::report_quality(ok)` (default no-op; the `AdmittingBackend` overrides
it) lets a higher layer feed the grounded answer-quality verdict into the per-model controller. The
cascade calls it after each acceptance verdict — so a model whose answers get *rejected while running
concurrently* backs its ceiling off ("quality drops under load" closed into the live loop). No
cross-layer controller registry is needed: the backend already owns its controller, and a rejection
is fed as a quality-red `ConcurrencySample` (an accepted answer is a no-op — the throughput path
already rewards success).

`src/concurrency.rs` + `src/backend.rs` + `src/cascade/mod.rs`; 4 new tests. 248/0. The one remaining
`ConcurrencySample` field still unfed is local resource headroom (free mem/CPU), which needs
MLX-specific probing.

## cascade — adaptive concurrency goes live (AIMD ⇄ circuit-breaker reconciliation)
Completed: 2026-06-15

The Phase-9 AIMD controller now drives **real admission limits** — and it no longer fights the
circuit breaker (`cascade-adaptive-live`). Both used to move the same admission `limit`: the breaker
trips it down on an OOM and recovers it; the controller tunes the steady state. Run together they'd
stomp each other.

The fix is a clean split of roles. The controller owns the **ceiling**: `AdmissionScheduler::
set_ceiling` sets both the recovery `capacity` and the live `limit`. The breaker (`trip` /
`recover_step`) then operates as a *fast inner loop within `[1, ceiling]`* — an acute OOM still drops
the live limit instantly and drains in-flight work, but recovery can't climb back above what the
controller has learned the model sustains. The breaker handles sub-update transients; the controller
sets the steady state; neither overrides the other.

`AdmittingBackend` gained an opt-in `AdaptiveConcurrency` (`with_adaptive`): every completed request
feeds a `ConcurrencySample` and the controller drives `set_ceiling` — a clean run probes the ceiling
up, an overload/error backs it off. Adaptive backends **start serial** (limit 1) and open up only as
healthy traffic accumulates — measured, not assumed. Turned on with `ROZUM_ADAPTIVE_CONCURRENCY=1`
in `admit_wrap` (default off, so the static budgeted limit is unchanged).

v1 drives the loop from the overload + success signals; the richer signals already modeled in
`ConcurrencySample` (resource headroom, latency baseline, judge/exec-feedback quality) are the next
refinement. `src/concurrency.rs`; 4 new tests. 245/0.

## cascade — gateway request-surface wiring (`model: "cascade[:name]"`)
Completed: 2026-06-15

The cascade is now reachable from outside: a request with `model: "cascade"` (or
`"cascade:<name>"`) builds and runs a `CascadeBackend` through the normal gateway path
(`cascade-gateway-wiring`).

A serializable `CascadeSpec` describes a cascade — cost-ordered `TierSpec`s (`{model, location,
pool?, endpoint?, api_key_env?}`) plus `max_escalations` and a `strategy` name. `build_cascade(spec,
resolver)` turns it into a backend by handing each tier's model to a caller-supplied async resolver,
so the cascade module stays decoupled from how backends are constructed. The `main.rs` hook resolves
locals through this binary's normal build chain and remotes through the OpenAI-compatible HTTP
backend with the env-named API key. A tier that can't be built (a remote with a missing key or
endpoint) is **skipped**, not fatal — a partial config still runs; only an all-empty cascade errors.

Named specs load from the environment as JSON: `ROZUM_CASCADE` for the default `model: "cascade"`,
`ROZUM_CASCADE_<NAME>` for `model: "cascade:<name>"`. `parse_cascade_model` routes the model string;
`Location` gained serde. `src/cascade/spec.rs` + the `build_cascade_backend` hook in `src/main.rs`;
6 new tests. 241/0.

Follow-ups: an Anthropic-native remote tier (v1 is OpenAI-compatible only), a `rozum.toml [cascade]`
schema (v1 is env JSON), and the **live P9 feed** — feeding per-request `ConcurrencySample`s and
applying per-model `set_limit`, which first needs reconciling the AIMD controller with the existing
circuit breaker (both move the admission `limit`).

## cascade — Phase 9: adaptive per-model concurrency (the cascade-router is complete)
Completed: 2026-06-15

The right concurrency level is **different for every model and can't be assumed** — a small local
takes more concurrent prefills than a big one; a generous remote more than a metered one. So
`cascade-p9-adaptive-concurrency` *measures* it. `AdaptiveConcurrency` is a per-model **AIMD**
controller (the TCP congestion-control idea, applied to request admission): probe the limit up by one
after a run of healthy requests (additive increase), and the moment the model shows load —
multiplicatively back off. It starts serial and opens each model up only as evidence accumulates,
oscillating around the demonstrated sweet spot.

`ConcurrencySample { overload, headroom, latency_ratio, ok }` carries exactly the signals the user
called for: a load failure (429 / quota / OOM), thin local resource headroom (back off *before* the
OOM, not after), a latency cliff, and **answer quality as a function of concurrency** (a failed
answer counts as red only *above* the floor — at serial it isn't a concurrency problem). `record(model,
sample)` returns the new target; a caller pushes it onto the already-resizable
`AdmissionScheduler::set_limit` (the actuator that existed since the circuit-breaker work).

It composes with the Phase-6 residency lanes for free — the effective live width of a model is
`min(adaptive limit, lane residency share)`, two independent gates already in the pipeline. Live
feeding (classify each request's `FailReason` / resource snapshot / exec-feedback into a sample and
apply per model) lands with the gateway request-surface wiring.

`src/concurrency.rs`; 6 new tests. 235/0.

**This closes the Cascade Router** (`docs/specs/cascade-router.md`): all 9 phases shipped — frugal
cheapest-first routing, transient health/availability, self-signal + the uncertainty affordance, an
L2 judge, a difficulty classifier, parallel residency lanes, a learned/persisted stats store with the
`Learned` start-tier, execution-feedback escalation, and now adaptive per-model concurrency.

## cascade — Phase 8: execution-feedback escalation (agent loop)
Completed: 2026-06-15

The agent loop can now escalate the **backend itself** on the most reliable quality signal there is
— whether the model's tool calls actually *worked* (`cascade-p8-exec-feedback`). A judge guesses; a
`ToolError` is ground truth that the answer was wrong. This can't drive the bare per-response cascade
(the response returns before the tools run), so it lives at the agent level.

`run_agent_escalating(tiers, system, user, tools, budget, policy)` drives a **cost-ordered list of
backends**: when the current model keeps producing failing tool calls, the next tier takes over,
inheriting the full transcript — errors included — so the stronger model sees exactly what went wrong
and fixes it. `ExecFeedbackPolicy { escalate_after_error_steps }` (default 2): that many
**consecutive** all-errored steps escalates; any progress (a call that works, or a final answer)
resets the counter. `tiers[0]` is the cheapest; one backend = plain `run_agent` (now a one-line
wrapper that never escalates, so all prior behavior is unchanged).

`ToolInvocation` gained `tier` (which cost-tier produced the call); `AgentOutcome` gained `final_tier`
and `tool_error_rate_by_tier() -> {tier: (errors, total)}` — the bridge a caller maps tier→model to
feed the Phase-7 learned stats with the per-model, per-task-class tool-error rate.

`src/agent.rs`; 3 new tests (escalate-on-persistent-errors, no-escalation-on-recovery,
single-backend-stays-tier-0). 229/0.

## cascade — Phase 7: learned stats store → the Learned start-tier
Completed: 2026-06-15

The cascade now **learns from its own history** and **carries it across restarts**
(`cascade-p7-learned`). Every model attempt is recorded per `(task-class, model)`; the first thing
that data buys is the `Learned` routing strategy — start at the cheapest tier that has *demonstrably*
been good enough for this class, instead of always re-discovering at tier 0 that the cheap model
escalates on hard prompts.

`TaskClass` = `{Freeform, Structured, ToolUse} × {Easy, Medium, Hard}` (shape from the request,
difficulty bucketed from the classifier score). `AttemptRecord` per attempt: accepted/escalated,
latency, tokens, judge-score, `FailReason` — **plus the concurrency level and a `ResourceSnapshot`**
(local mem/CPU headroom; remote rate-limit/latency reaction) so quality/latency/failures are
attributable to the concurrency they happened at, which is the curve the Phase-9 adaptive controller
will consume. `StatsStore` is an append-only JSONL log (the `memory_store` pattern) replayed on open,
with in-memory aggregates (EWMA latency/score, accept-rate) per `(task-class, model)`.

`RoutingStrategy::Learned`: `start_index` enters at the cheapest tier whose historical accept-rate
clears `learned_accept_threshold` (0.6) with at least `learned_min_attempts` (5) of evidence,
falling back to `ClassifyThenStart` when the evidence is thin. The cascade records every attempt when
`config.stats` is set (opt-in; default off, so all prior paths are unchanged).

`FailReason` gained `Serialize`/`Deserialize` for the log. `src/cascade/stats.rs`; 8 new tests (5
unit, 3 e2e). 226/0. Deferred within the learned track (non-blocking): adaptive judge thresholds and
health-pattern persistence feeding the backoff.

## cascade — Phase 6: parallel residency lanes
Completed: 2026-06-15

The cascade now **parallelizes by difficulty without blocking** (`cascade-p6-scheduler`). Concurrent
requests already ran as independent futures; what they must not do is contend for the same scarce
local memory. A simple request on the small fast model must never be stuck behind a complex request
on the big one, and remote (HTTP) requests should parallelize freely.

A **lane** is a residency group. `Lane { Pool(name), Free }` per `ModelCard`; `Lane::default_for`
puts every local in one shared `"local"` pool (one GPU, mutually exclusive) and every remote in
`Free`. `LaneSet` holds one semaphore per pool. The cascade `enter`s a model's lane before each
attempt and holds the permit **only for that attempt** (freed on escalation), so co-residents
serialize (single-resident = 1 slot) while a request in a *different* lane — or any remote — runs in
parallel. Multi-resident (co-residency / multi-GPU) is the same code with `residency_slots[pool] > 1`.

The scheduler sits **above** per-backend admission (`concurrency::admit_wrap`): it owns lane
assignment and delegates within-lane concurrency to the backend. The default (one 1-slot local pool)
is safe on a single-GPU box and leaves every existing single-request path unchanged.

`src/cascade/scheduler.rs`; 6 new tests (5 `LaneSet` unit incl. distinct-pools-parallel,
same-pool-serialize-then-free, multi-resident slots; 1 e2e where a simple + a hard request on
distinct lanes meet at a shared barrier — proving they ran concurrently). 218/0.

## cascade — Phase 5: difficulty classifier → ClassifyThenStart
Completed: 2026-06-15

The cascade can now **start partway up** instead of always at the cheapest tier
(`cascade-p5-classifier`). `AlwaysCheapest` wastes a round-trip on every obviously hard request
(long code, formal reasoning) that the cheap tier will predictably punt. `ClassifyThenStart` scores
the request's difficulty once, up front, and enters the cascade at a proportional tier.

`Classifier` trait (`difficulty(req) -> 0.0..1.0`) + `HeuristicClassifier` — a free, deterministic
score over surface features: prompt length, code markers (```` ``` ````, `fn `, `def `, …), math/
reasoning cues (`prove`, `integral`, …), multi-step asks (`step by step`, `refactor`, `analyze`, …),
the number of offered tools, and conversation depth. It reads **only the user/assistant turns**, so a
big system prompt never inflates the score.

`RoutingStrategy { AlwaysCheapest (default), ClassifyThenStart }` on `CascadeConfig`; `start_index`
maps difficulty onto `0..n-1` (round to nearest). The candidate order is **start-and-up** (the
natural escalation path) then the **cheaper tiers below as availability fallbacks**
(`(start..n).chain((0..start).rev())`), so a parked entry tier still degrades to something rather than
failing. Classification moves only the *entry point*, never the ceiling — escalation works unchanged
from there, and `AlwaysCheapest` reproduces the old order byte-for-byte (all Phase 1–4 tests stay
green). Opt-in (default `AlwaysCheapest`, classifier `None` → the built-in heuristic).

`src/cascade/classifier.rs`; 9 new tests (6 heuristic scoring, 3 e2e: trivial→cheapest,
hard→skip-cheap, hard-prompt-with-entry-down→fall-back-below). 212/0.

## cascade — Phase 4: the L2 judge (pluggable: heuristic or model)
Completed: 2026-06-15
Adds the L2 judge to the Cascade Router (`cascade-p4-judge`), consulted **only when L0/L1 are
inconclusive** (a free-form answer with no structural requirement and no self-signal), so most
requests are still settled by the free checks. A pluggable `Judge` trait (`async score(req, answer)
-> 0..1`); below the configured `threshold` → escalate. Two implementations: `HeuristicJudge` (free,
deterministic — an empty answer or an explicit non-answer like "I don't know" scores low) and
`ModelJudge` (a small model rates the answer 0–10, parsed and normalized; a judge error yields a
neutral 0.5 so a flaky judge never blocks the cascade). `pipeline_verdict` now returns
`Option<Verdict>` — `None` (all inconclusive) routes to the async judge if configured, else accepts.
Opt-in (default no judge). `src/cascade/judge.rs`; 5 new tests. 203/0.

Also noted (sprint `cascade-p8-exec-feedback`, a user idea): the most reliable quality signal is
**execution feedback** — whether the model's tool calls actually *worked*. The agent runtime already
records it (`AgentOutcome.operations[].output: Result`), so a later phase lets `run_agent` over a
cascade escalate when tool calls keep failing, and feeds the tool-error rate into the learned stats.
The bare per-response cascade can't see it (the answer returns before tools run), so it lives at the
agent level. Spec updated (`docs/specs/cascade-router.md`).

## cascade — Phase 3: self-signal escalation + the "admit uncertainty" affordance
Completed: 2026-06-15
The cheap model can now **defer instead of guessing** (`cascade-p3-self-signal`). Rather than guessing
whether a model "sounds unsure", we give it the skill: an `EscalationAffordance` injects a system-prompt
instruction into every non-top tier — *"if you are not confident, do not guess; reply `[[ESCALATE:
reason]]` and a stronger model takes over — admitting it beats being confidently wrong"* — and L1
`SelfSignalCheck` escalates on that marker, on an `escalate`/`consult_stronger` tool call, or on an
opt-in refusal pattern (off by default; we rely on the taught signal, not heuristics). The marker is
stripped from any fallback answer so it never leaks to the client. `escalation_tools()` exposes
`consult_stronger` as a `ToolSource` for agent mode (composes with `run_agent` / `MultiToolSource`). The
default cascade pipeline is now `[L0 structural, L1 self-signal]` with the affordance on; the top tier
gets no affordance (nothing above it). 7 new tests (marker / tool / refusal detection, affordance
injection, marker strip, tool ack; e2e: marker → escalate to the strong model, marker stripped from a
fallback). 198/0.

## cascade — Phase 2: transient availability/health-aware routing
Completed: 2026-06-15
Makes the Cascade Router **resilient** (`cascade-p2-health`): a model's availability is transient — a
remote hits its quota / gets rate-limited / goes down / the network drops, a big local OOMs — so it's
tracked as live runtime health and the cascade routes around a model that's failing *right now* to the
best **available** alternative, recovering automatically.
- `HealthRegistry` (`src/cascade/health.rs`): per-model `HealthState {Healthy, Degraded(half-open),
  Unavailable}` + `FailReason {RateLimited, QuotaExhausted, Down, Network, OutOfMemory, Unknown}`.
  `classify(err)` maps a backend error string to a reason; `record_failure` parks the model with an
  exponential-backoff + jitter cooldown (longer for quota, short for rate-limit); `is_available` goes
  half-open once the cooldown elapses (one probe); `record_success` → Healthy.
- The cascade loop now **skips models in cooldown** (best-available routing, which may be sideways or
  *down* — a failing remote → a local, a big-local OOM → the smaller model's best-so-far), classifies
  an attempt error → parks the model, and a `Network` failure parks **every** `Location::Remote` model
  at once (the internet is gone). Graceful degradation: return the best usable answer, hard-fail only
  when nothing is available or usable. `ModelCard` gained `location: Local | Remote`.
- 6 new tests: error classification, park → half-open → recover, backoff; and deterministic e2e —
  a parked model is skipped on the next request, a remote network failure parks all remotes and
  degrades to local, a big-local OOM falls back to the smaller model. 191/0.

## cascade — frugal/escalation model routing, Phase 1 (the deterministic core)
Completed: 2026-06-15
First phase of the Cascade Router (`cascade-router`; spec `docs/specs/cascade-router.md`): a
`CascadeBackend: ChatBackend` (`src/cascade/`) that arbitrates over a caller-supplied, cost-ordered
list of models — try the cheapest first, escalate only when the cheap answer isn't good enough, stop
at the first acceptable. Cheaper than a single frontier call on average (the opposite of a parallel
ensemble). Phase 1 is the deterministic, model-free core:
- `ModelCard {id, backend, tier}` + `CascadeConfig {models, acceptance, budget}`; one model → live
  passthrough (no arbitration).
- The loop: drain each attempt to a `TurnOutcome`, run the acceptance pipeline, `Accept` →
  short-circuit and replay that model's output, `Escalate` → next tier, errored attempt skipped;
  budget (`max_escalations` / `wall_time`) or list exhaustion → the best usable answer so far, only a
  hard error if no model produced one.
- L0 `StructuralCheck` (reusing the `constrain` engine): a backend error escalates; a
  `response_format` schema or tool-call args that don't conform escalate; free-form text is
  inconclusive (→ accept). `AcceptanceCheck` trait + `Verdict {Accept, Escalate, Inconclusive}` +
  `pipeline_verdict` (first decisive check wins; all-inconclusive → Accept).
- 7 model-free e2e tests (escalate-on-structural-fail → strong wins; accept-cheap skips strong;
  passthrough; error-escalate; budget → best-so-far; free-form → cheapest; all-error → error). 185/0.
Next phases (sprint): availability/health-aware routing, self-signal+escalate-tool, cheap judge,
difficulty classifier, parallel lanes, learned stats. (Gateway request-surface wiring deferred.)

## kernels — extract the GatedDeltaNet Metal kernel into a standalone .metal (L4)
Completed: 2026-06-15
Factored the GatedDeltaNet fused delta-rule scan kernel's MSL source out of the inline raw string in
the fork's `models/gated_delta.rs` into a **hardware-only** module
(`mlx-lm/src/kernels/gated_delta_step.metal` + `kernels/mod.rs` exposing it via `include_str!` as
`GATED_DELTA_SOURCE`). The kernel math now lives once, as an actual `.metal` source with its I/O
contract documented; the engine *binding* (`MetalKernel::new` buffer wiring, dispatch, eval control)
stays with the model leaf. So a future Metal engine (a candle-metal path, mistralrs-metal) can bind the
same `.metal` instead of re-deriving the recurrence — the last piece of the durable-layer split (L4).
Pure move — kernel output is byte-identical: the fork's `gated_delta_kernel_matches_ops` (kernel vs ops
reference) and `gated_delta_matches_python` still pass (4/4), and rozum's full suite is green against
the bumped fork rev `838a39ab`. 178/0.

## sampler — shared engine-agnostic token sampler (extract-shared-sampler, L2)
Completed: 2026-06-15
The sampler (repeat-penalty → temperature → top-k → top-p → categorical) now lives in one
engine-agnostic module (`src/sampler.rs`, L2 of the durable-layer split), defined over a plain
`&[f32]` logit slice + an `impl Rng` so every leaf can materialize its final-position logits and call
it instead of re-implementing sampling. `SamplerConfig::from_params`, `seeded_rng(seed)`,
`repeat_window`, `sample(logits, cfg, recent, rng)`; 6 deterministic unit tests.
- **GGUF migrated to it** (`gguf.rs`): its ad-hoc temperature+softmax with a buggy *global-static* LCG
  is gone, replaced by the shared sampler — so GGUF now honors `top_k` / `top_p` / `repeat_penalty` /
  `seed` (which it silently ignored before) and shares one definition. Compile-verified
  `--features gguf`.
- The MLX hot path keeps its on-device `sample_with` (the byte-exact oracle tests pin it); the shared
  module is the canonical CPU definition it mirrors, available to any CPU leaf — the per-token GPU→CPU
  copy of one vocab vector is negligible for op-launch-bound decode. 178/0.

## agent — `MultiToolSource` combinator: compose the local-agent toolkit
Completed: 2026-06-15
`run_agent` takes a single `ToolSource`, but an app wants several at once — the built-in tools
(`tool-routing`), the memory store (`memory-store`), a retrieval index (`rag-lite`), and its own
domain tools. `MultiToolSource` composes them into one: the union of their tools (first-added wins on a
name clash) with each call routed to the source that declares it. Builder API
(`MultiToolSource::new().with(a).with(b)`). Unit-tested (union + clash precedence + routing + unknown
→ `ToolError`). This is the capstone that ties the recent local-agent toolkit together. 172/0.

Also marked `ui-streaming-ws-tui` **not-applicable**: after the meeting-room pivot there's no
`ChatEvent` token stream feeding the web/TUI — external agents submit *complete* messages via the
atomic MCP `meeting.submit`, and the web bridge broadcasts complete transcript entries. The live
responding-indicator already covers "who's typing".

## agent — lexical retrieval (rag-lite): search a local corpus
Completed: 2026-06-15
A lightweight local retrieval layer (`src/rag_lite.rs`, `rag-lite`): index small text documents and
pull the top-K most relevant to a query. v1 is **lexical** — a `LexicalIndex` implementing **BM25**
(`add(id, text)` + `search(query, k) -> Vec<Hit>`), pure Rust, no model/network, fully deterministic.
A `Retriever` trait keeps the retrieval API stable so an **embedding** backend can be dropped in later
(the configurable backend the brief asks for) without touching callers. Exposed to the reference agent
runtime as a `search_documents` tool (`retrieval_tools(Arc<dyn Retriever>)`), so a small local agent
can ground answers in a local corpus. Unit-tested (BM25 ranks the relevant doc first; no-match / empty
index / `k=0` are safe; idf; the tool). Pairs with `memory-store` (exact key) and `tool-routing` as the
local-agent toolkit. 171/0.

## agent — local memory store (memory-store): durable remember/recall
Completed: 2026-06-15
An append-only local memory (`src/memory_store.rs`, `memory-store`): a key→value JSONL log with
retrieval by exact key. `MemoryStore::{open, in_memory, set, get, all, keys}` — `get` is
last-write-wins, `all` returns the full per-key history, appends never rewrite earlier records, and
`open` replays the JSONL into the index so memory survives restarts. Exposed to the reference agent
runtime as `remember(key, value)` / `recall(key)` tools (`memory_tools(Arc<MemoryStore>)`), so a small
local agent has durable memory across turns and sessions. Deliberately exact-key only — no
embeddings/ranking (that's `rag-lite`). Unit-tested (append-only history, disk persistence + replay,
the tools incl. missing-key → `found:false`). 168/0.

## agent — built-in tools registry (tool-routing)
Completed: 2026-06-15
A small registry of safe, read-only built-in tools (`src/builtin_tools.rs`, `tool-routing`) exposed as
a `CallbackToolSource`, so the reference agent runtime lets a model select them with zero app wiring:
- `echo(text)` — round-trips text (handy for exercising the tool loop).
- `current_time()` — UTC unix timestamp + ISO-8601.
- `list_models()` — the recommended catalog (`models::RECOMMENDED`) + the locally-installed models
  (`scan_all_installed`), so an agent can introspect what it can run.
Side-effect-free (no filesystem/network writes); file lookup deliberately omitted (security). Composes
with an app's own domain tools. Unit-tested (registry shape + each tool's dispatch, including the
missing-arg `ToolError`). 165/0.

## docs — model reference specs (engine-independent): implement a new leaf from fact
Completed: 2026-06-15
Captured the model *knowledge* we reverse-engineered porting each family into the native MLX runtime,
as engine-independent reference docs in `docs/specs/model-reference/` — so a new leaf (a different
tensor library, a CUDA path) implements from fact instead of re-deriving from a checkpoint, which is
where the real time went (`extract-model-reference-specs`, L3 of the durable-layer split).
- `README.md` — the cross-cutting checkpoint conventions: the AFQ `.weight→.inner.weight` /
  `.bias→.inner.bias` load-time remap (detected by a sibling `.scales`), the RMSNorm `+1` convention
  (who uses it — Gemma 3 + Qwen3.6, NOT Qwen3/Qwen2/Llama — and computed in f32), tied-embedding
  detection by key presence, the safetensors stale-shard-index fallback, the multimodal `text_config`
  unwrap (`language_model.` prefix + skip vision), and the MLX↔PyTorch row-order caveat.
- One file per family: `qwen3` (per-head q/k norm, SwitchGLU MoE), `qwen36-hybrid` (the
  GatedDeltaNet f32 delta-scan, heterogeneous per-layer cache, +1-at-load, the 8-bit router/shared-gate
  outside `nn::quantize`), `llama-family` (the Phi-3 fused-projection split at load, Mistral's optional
  `head_dim` + list chat-template, SmolLM bf16), `qwen2` (the QKV-bias quirk), `gemma3` (four norms,
  `query_pre_attn_scalar` scale, sliding-window local/global, linear RoPE scaling, and the
  multimodal-wrapper config-defaults table for 4B/12B/27B).
Grounded in the fork's `models/*.rs`; linked from `mlx-native-runtime.md`. Docs only — no code change.

## structured output — `response_format: json_schema` constrains the whole reply
Completed: 2026-06-15
Exposes the `constrain.rs` engine as **non-tool structured output**: a client asks for a JSON shape and
the native MLX backend guarantees the entire response parses + conforms (`structured-output`). The
gateway parses OpenAI `response_format` (`{"type":"json_object"}` → any object;
`{"type":"json_schema","json_schema":{"schema":…}}` → that schema; `text`/absent → free) onto a new
`SamplingParams.response_schema`, which flows to the backend with the rest of sampling (zero new
threading).
- **Generalized the constrained decode** rather than duplicating it: the masked B=1 loop is now generic
  over a `ConstraintDriver`. `ToolConstraint` (the existing tool path, waits for `<tool_call>`) and the
  new `ResponseConstraint` (constrains the WHOLE output to a fixed schema from the first token, releases
  on completion) both drive the same loop on both dense and hybrid arches.
- **Always honored** when a `response_schema` is present — unlike the tool constraint it is NOT gated by
  `ROZUM_MLX_CONSTRAIN`, because it's an explicit client correctness request, not an opt-in reliability
  tweak.
- Validated: a gateway parse unit test (`response_format_parsing`) and an e2e
  (`mlx_response_format_json_schema`, Qwen3-4B): "Return the city Paris and its country as JSON" →
  `{"city":"Paris","country":"France"}` — pure, schema-conforming JSON constrained from token 0. 161/0.

## backends — Anthropic Messages HTTP client backend (+ OpenAI client API-key auth)
Completed: 2026-06-15
Adds the client side of the Anthropic dialect so rozum can call a remote Anthropic (or
Anthropic-compatible) model as a `ChatBackend` — the mirror of the gateway's server-side Anthropic
support, and the path for **frontier-model escalation/fallback** when a local model can't handle a
task (`integration.md`). `src/anthropic_http.rs`:
- `messages_to_anthropic` folds system turns into the top-level `system` and carries tool-results as
  user-role `tool_result` blocks (the Anthropic wire shape).
- `POST /v1/messages` with `x-api-key` + `anthropic-version`, `stream: true`, `tools` as
  `{name, description, input_schema}`.
- Parses the Anthropic SSE — `content_block_start`/`_delta`/`_stop` (text + `tool_use` blocks, via an
  index→tool-id map) and `message_delta`/`message_stop` — into `ChatEvent`s, mapping `stop_reason`
  (`tool_use`→ToolUse, `max_tokens`→MaxTokens, else EndTurn). Unit-tested (text stream, tool_use
  block, message conversion).
- **OpenAI client** (`openai_http.rs`) gained `with_api_key` (Bearer) so it works against
  authenticated remotes (OpenAI/OpenRouter), not just local servers — closing
  `openai-http-client-backend` too. 160/0.

Both client backends speak the `ChatBackend` SPI, so they plug into the orchestrator and the reference
agent runtime exactly like the in-process backends.

## agent — MCP-client ToolSource adapter (the runtime can use external MCP tools)
Completed: 2026-06-15
Adds the second `ToolSource` adapter to the reference agent runtime: `McpToolSource`, backed by an
external MCP server over `rmcp`. `connect_stdio(program, args)` spawns the server (stdio child
process), runs the MCP handshake, and caches its `list_tools`; `dispatch` forwards each call as
`tools/call` and flattens the `CallToolResult` (preferring structured content, else the text parts).
So `run_agent` can now drive a model against tools served by any MCP server, not just in-process
callbacks — the same loop, a different tool backend. Completes `rozum-agent-runtime`.
- Added the `transport-child-process` rmcp feature (for the stdio transport).
- `McpToolSource::from_service` wraps any already-connected client service, which the test uses over
  an in-memory duplex: a minimal `#[tool]` MCP server exposing `add` → `list_tools` surfaces it →
  `dispatch("add", {a:3,b:5})` returns `{sum:8}`. Plus model-free unit tests for the two conversions
  (`Tool`→`ToolDef`, `CallToolResult`→`Value`). 157/0.
- Remaining nearby: `rozum-embed` (P2), the stable public crate over `run_agent` + the adapters.

## agent — reference agent runtime (Contracts 2–3): the tool loop, in Rust
Completed: 2026-06-15
A Rust reference implementation of the agentic loop (`rozum-agent-runtime`, P0b), in `src/agent.rs`.
Dual purpose: it powers the in-process embedded mode (small local model, no network) and is the
executable spec the scalascript agent SDK mirrors. It speaks only the `ChatBackend` SPI, so it runs
against any backend (native MLX, GGUF, a remote HTTP client backend). Completes the P0b contract
trio (Contract 1 gateway ✓, the tool contract ✓, now the agent loop).
- **Contract 3 — Tool** (`ToolSource` trait): `fn tools() -> Vec<ToolDef>` (the schemas advertised to
  the model) + `async fn dispatch(name, args) -> Result<Value, ToolError>`. `CallbackToolSource` is
  the direct in-process adapter — register `(ToolDef, handler)` pairs; a `ToolError` is a recoverable
  message handed back to the model as the tool result so it can self-correct.
- **Contract 2 — the loop** (`run_agent(backend, system, user, tools, budget) -> AgentOutcome`):
  `[system, user] → model → (tool calls → dispatch → append results)* → final text`, bounded by
  `Budget {max_steps, max_tokens, wall_time, temperature}` (temperature 0 by default for reproducible
  runs). `AgentOutcome` carries `{text, stop, steps, operations, transcript}` — the full audit trail
  of executed side effects + the conversation. `AgentStop ∈ {Done, BudgetSteps, BudgetTime, Error}`.
- **Validated model-free** (a scripted `MockBackend`): the full tool loop with the result fed back on
  the next step, the budget capping a runaway tool-calling loop, and recovery from both an
  unknown-tool call and a handler validation error (the `ToolError` reaches the model). **And e2e
  against native MLX** (`agent_loop_real_backend`, Qwen3-4B): the model calls `add(3,5)`, gets
  `{sum:8}`, and answers "The result of 3 + 5 is 8." — with `ROZUM_MLX_CONSTRAIN` guaranteeing the
  args are valid. 154/0.
- **Follow-up**: an MCP-client `ToolSource` adapter (over `rmcp`) so the runtime can use tools from an
  external MCP server (the trait is ready), and the `rozum-embed` public crate.

## gateway — distributed readiness: /health + /ready + graceful shutdown (run rozum as a service)
Completed: 2026-06-15
Makes the gateway safe to run as N identical instances behind a load balancer with zero-downtime
rolling deploys (`rozum-distributed-readiness`, P0b/P1). Spec: `docs/specs/distributed-readiness.md`.
- **`GET /health`** (liveness — 200 while the process serves HTTP, never touches the model) and
  **`GET /ready`** (readiness — 200 when servable, 503 while draining; body `{ready, loaded,
  shutting_down, model}`). The split is the standard one: health → restart decisions, ready → routing
  decisions. A transient model **swap**-drain (`/control/switch`) does NOT flip readiness — those
  requests park for the brief swap and still succeed, so the instance stays in rotation; only a
  shutdown (or an unloaded `--dedicated` model that can't rebuild) reads not-ready.
- **Graceful shutdown** on SIGTERM/SIGINT via `axum::serve(...).with_graceful_shutdown(...)`: flip the
  instance to not-ready and reject new chats (`enter()` returns 503 `shutting_down` instead of
  parking), wait `ROZUM_SHUTDOWN_GRACE_SECS` (default 3) so the LB deregisters, then axum stops
  accepting and drains the in-flight streams to completion before exit. Rolling deploys bleed an old
  instance out cleanly while new ones absorb traffic.
- **Stateless** is now documented as a property: the prefix-KV cache is a per-instance latency
  optimization, not session affinity — any instance serves any request, so no sticky sessions are
  needed (round-robin / least-connections is fine).
- Tests: `readiness_reflects_servability`, `shutdown_flips_readiness`,
  `enter_rejects_new_chats_while_shutting_down` (no leaked `generating` token). 149/0. Follow-ups
  (noted): a multi-model pool/router and cross-instance admission coordination.

## gateway — tool contract (Contract-1) hardened + documented; `tool_choice` honored
Completed: 2026-06-15
The HTTP tool surface the scalascript agent SDK builds against (`rozum-gateway-tool-contract`, P0b) is
now a stable, documented, conformance-tested contract. The tool-use machinery mostly existed
(`tools` → `tool_calls`/`finish_reason`/SSE deltas across `/v1/chat/completions`, `/v1/messages`,
`/v1/responses`); the one real gap was `tool_choice` — parsed nowhere, silently ignored.
- **`tool_choice` now parsed + honored** on all three routes, normalized across dialects into
  `ToolChoice::{Auto, None, Required, Named}` (OpenAI string/object, Responses flat `{type,name}`,
  Anthropic `{type: auto|any|none|tool}`). Honored by transforming the tool set the backend sees — no
  SPI change: `none` empties the tools (text-only), `named` restricts to that one tool, `auto` passes
  through. `required` is accepted but best-effort (the model isn't *forced* to start a call) and
  documented as such rather than silently dropped.
- **Documented** as a stable contract in `docs/specs/api-gateway.md` — a Tool-use/Contract-1 section
  with the `tool_choice` cross-dialect table, the non-streaming + streaming response shapes, the
  `finish_reason`/`stop_reason` mapping, and the `ROZUM_MLX_CONSTRAIN` arg-reliability note (the
  constrained decoding is transparent to the contract — the SDK just gets conformant `arguments`).
- **Conformance tests** (model-free, mock streams): `tool_choice` parsing per dialect +
  `apply_tool_choice` semantics, and the actual tool-call response JSON for both dialects
  (`oai_collect_tool_call_shape`: `tool_calls[].{id,type,function}` + `finish_reason:"tool_calls"`;
  `anthropic_collect_tool_use_shape`: `tool_use` block + `stop_reason:"tool_use"`). 146/0.

## mlx-native — constrained tool decoding reaches Qwen3.6 (hybrid path + XML tool format)
Completed: 2026-06-15
The constrained-decoding v1 only covered dense arches and the JSON Hermes tool format — so it didn't
actually help the user's primary model: Qwen3.6 is a **hybrid** (GatedDeltaNet) arch AND it emits tool
calls as **XML**, not JSON. Both gaps are now closed.
- **Hybrid path**: extracted the masked decode into a generic `constrained_decode_loop<C>` and added
  `run_constrained_hybrid` over the heterogeneous `LayerCache` (mirror of the dense
  `run_constrained_dense`). `should_constrain` now routes both dense and hybrid arches.
- **XML tool format**: Qwen3.6 emits `<function=NAME><parameter=KEY>\nVALUE\n</parameter>…</function>`
  rather than `{"name":…,"arguments":…}`. Added an XML prefix-acceptor (`xml_prefix`) and a unified
  `Constraint::{Json, Xml}` enum; the decode loop picks the format from the first body char after
  `<tool_call>` (`{` → JSON, `<` → XML) and constrains accordingly — `NAME` ∈ tool names, `KEY` ∈ the
  tool's properties (no dupes, all required before `</function>`), `enum` `VALUE`s restricted to their
  literals. 3 new model-free unit tests.
- **Validated on the real model** (`mlx_constrained_tool_call_hybrid`, Qwen3.6-35B-A3B): the prompt
  asks for "celsius" but the schema enum is `["kelvin","rankine"]` → output
  `{"location":"Paris","unit":"kelvin"}`, i.e. the mask bit on the hybrid XML path exactly as on the
  dense JSON path. The discovery that surfaced this (the dense JSON e2e passed but the hybrid one came
  back `unit:"celsius"`) is itself why each addition is run, not assumed. 141/0.

## mlx-native — constrained tool-argument decoding (small models can't emit an invalid tool call)
Completed: 2026-06-15
Tool-use was post-hoc: render `tools` into the prompt, generate freely, then parse
`<tool_call>{json}</tool_call>` after the fact — so a small model could emit malformed JSON, a
hallucinated key, a wrong type, or a missing required arg, and the parse would fail or yield garbage.
Now, behind `ROZUM_MLX_CONSTRAIN`, the sampler is **masked to the tool's JSON schema during decode**,
so the arguments object physically cannot violate it. v1 of `structured-output-for-tools`; spec at
`docs/specs/constrained-tool-decoding.md`.

- **Engine** (`src/constrain.rs`, pure Rust, no MLX): a JSON-Schema subset compiles to an incremental
  **prefix acceptor** — `Schema::prefix(s)` returns Complete / Partial / Invalid for the partial JSON
  so far. Subset: object (properties + required, keys restricted to declared props), string (+`enum`/
  `const`), integer, number, boolean, array-of-scalar, nested object; anything it can't model relaxes
  to generic well-formed JSON (it never over-rejects). It's stateless — re-validates the whole suffix
  each step (args are short), which also lets the caller swap the schema mid-stream for free. 6
  model-free unit tests cover required keys, enums, types, completion, and the relax path.
- **Sampler mask** (`mlx_native_backend.rs`): a B=1 dense decode loop (`run_constrained_dense`) that,
  once the model opens a `<tool_call>{`, keeps only the top-K candidate tokens whose decoded piece
  leaves the JSON a valid prefix (widen 256→4096→full vocab, argmax fallback), forbids the rest (−∞),
  then runs the existing `sample_with` among the allowed (temp/top-p/top-k/penalty still apply). The
  Hermes envelope `{"name": <enum tool names>, "arguments": <schema>}` is enforced; `arguments`
  resolves to the chosen tool's schema as soon as the `name` literal is read; the constraint releases
  when the object closes. Covers every dense arch (Qwen3/Qwen2/Llama/Gemma 3) via `dense_forward`.
- **OFF by default** → the free-decode + post-hoc-parse path is byte-identical; constrained jobs are
  also kept out of the batched path (they need the B=1 masked loop).
- **Validated** with a discriminating e2e (`mlx_constrained_tool_call_conforms`, Qwen3-4B): the prompt
  asks for "celsius" but the schema's `unit` enum is `["kelvin","rankine"]` — the output is
  `{"location":"Paris","unit":"kelvin"}`, i.e. the mask redirected the model off its *preferred but
  invalid* token onto a legal enum literal. Proves the constraint actually bites, not that the model
  happened to comply. 138/0.

Follow-ups (BACKLOG): hybrid Qwen3.6 constrained decode (v1 is dense; hybrid falls back to post-hoc),
full JSON-Schema (`oneOf`/`$ref`/patterns), and a general `response_format: json_schema` request field
reusing the same engine.

## mlx-native — the bigger Gemma 3 sizes load (4B/12B/27B multimodal wrapper) + catalog mid-tier
Completed: 2026-06-15
Only the tiny text-only Gemma 3 1B (`model_type: "gemma3_text"`) loaded before; the genuinely useful
4B/12B/27B ship as the **multimodal wrapper** (`model_type: "gemma3"`) and failed at load. The 4B was
added to the catalog, validation caught the failure (exactly why each addition is run, not assumed),
and the loader was fixed — four general changes in `gemma3.rs`, all validated end-to-end on
gemma-3-4b-it-4bit (answers correctly, `mlx_gemma3_wrapper_chat`):
- **Config nesting:** the text model lives under `text_config` (with `quantization` at the top level,
  grafted on). The wrapper omits most head fields, so `ModelArgs` now carries serde defaults matching
  HF `Gemma3TextConfig` (heads 8, kv 4, head_dim 256, sliding_window_pattern 6, query_pre_attn_scalar
  256, rope_theta 1e6, rope_local 1e4, vocab 262208) — verified to reconstruct 4B (heads→8), 12B
  (head_dim→256) and 27B exactly. Model-free unit test `wrapper_text_config_fills_gemma3_defaults`.
- **Weight prefix:** strip `language_model.` and skip `vision_tower.*` / `multi_modal_projector.*` so
  the text params line up; no materialized lm_head → tied embeddings (already handled).
- **RoPE scaling:** the wrapper sets linear scaling `{factor 8}`, which Gemma 3 applies to the GLOBAL
  layers only (local sliding-window layers stay unscaled). Threaded through `Attention::new` /
  `DecoderLayer::new`.
- **Stale index (general robustness):** some mlx-community uploads ship a `model.safetensors.index.json`
  that names sharded files (`model-0000N-of-…`) after the weights were consolidated into a single
  `model.safetensors`. Trust the index only when every shard it names exists; otherwise load every
  `*.safetensors` actually present. Fixes the load for any repo with a stale index, not just Gemma.

Catalog (`mlx-native-recommend-catalog`): with all the architectures now landed, `models::RECOMMENDED`
gained mid-tier entries so the picker has coder/general at a fits-16GB size, not just tiny + heavy —
**Qwen2.5-Coder 7B** (mid coder, same family as the 32B) and **Gemma 3 4B** (mid general). Both
validated by actually loading + answering. Fork rev `8c18fd23`.

## mlx-native — Gemma 3 batches too → EVERY dense family now serves concurrently
Completed: 2026-06-15
The last serial dense arch joins the batched path: two concurrent Gemma 3 sessions now share one
forward instead of queueing. Gemma 3 was the hold-out because its LOCAL layers attend only to the
last `sliding_window` (512) keys, and that per-layer windowed mask had to be threaded through the
batched decode (the per-row RoPE port — `BATCH_PAD_OFFSETS` + `set_batch_pad_offsets` in `gemma3.rs`
— is the same mechanism as Llama/Qwen2). The window plumbing is cheap because of the cache geometry:
at decode every row is right-aligned in the left-padded KV cache, so "keep the last `window` keys"
is a single uniform key-axis mask (`build_window_keep`: `kpos ≥ total − window`) AND-ed with the
per-row pad mask — no per-row window math. `dense_forward` gained a `Gemma3` arm, `is_batchable_arch`
includes it, and `run_batch` sets `gemma3::set_batch_pad_offsets` alongside the others (each model
reads only its own thread-local — harmless no-ops). OFF by default → B=1 serial path byte-identical
(`mlx_gemma3_chat` unchanged; windowing math still covered by `sliding_window_mask_bands_local_attention`).
Validated end-to-end (`mlx_gemma3_batched_two_concurrent`, cached gemma-3-1b-it-4bit): two concurrent
requests land in ONE `run_batch` call with distinct correct answers (`Paris` / `Tokyo`). **So now
EVERY dense family batches** — Qwen3 / Qwen3-MoE, Qwen2/2.5, Llama / Mistral / Phi-3 / SmolLM, and
Gemma 3 — plus the Qwen3.6 hybrid via its own `run_batch_hybrid`. No dense arch is left serial.
131/0. Fork rev `06fd0421`.

## mlx-native — the Llama family batches too (Mistral / Phi-3 / SmolLM / Llama-3.x)
Completed: 2026-06-15
Batched decode used to be Qwen-only — every dense Llama-family model (Llama 3.x, Mistral, Phi-3,
SmolLM, all of which load into `LoadedModel::Llama`) ran strictly serially, so concurrent sessions on
them queued instead of sharing a forward. Now they batch: ported qwen3's per-row-RoPE mechanism into
`llama.rs` (a `BATCH_PAD_OFFSETS` thread-local + `set_batch_pad_offsets`; `Attention` ropes q/k at
`cache.offset() − pad_i` per row via `forward_dynamic` when set, else the normal scalar offset). The
key-pad mask was already threaded through `AttentionInput.mask`, so it needed no change. `dense_forward`
gained a `Llama` arm and `is_batchable_arch` includes it; `run_batch` sets both arches' thread-locals
(only the loaded model reads its own — the extra setter is a harmless no-op). OFF by default → the B=1
serial path is byte-identical (`mlx_llama_chat` unchanged). Validated end-to-end
(`mlx_llama_batched_two_concurrent`, Llama-3.2-1B): two concurrent requests land in ONE `run_batch`
call with distinct correct answers (`Paris` / `Tokyo`). **Qwen2 / Qwen2.5 / Qwen2.5-Coder got the same
treatment** (identical per-row-RoPE port in `qwen2.rs`; `mlx_qwen2_batched_two_concurrent` on a cached
Qwen2.5-0.5B). So EVERY dense family now serves concurrent sessions in parallel with continuous
batching + per-row sampling — Qwen3 / Qwen3-MoE, Qwen2/2.5, Llama, Mistral, Phi-3, SmolLM (plus the
Qwen3.6 hybrid via its own path). 131/0. Fork rev `341ebb2c`. (Only Gemma 3 — its per-layer
local/global windowed masks need threading into the batched path — remains serial; a follow-up.)

## mlx-native — Gemma 3 sliding-window attention (local layers window correctly at long context)
Completed: 2026-06-15
Finishes the one deferred Gemma 3 gap. Its LOCAL (non-global) layers are supposed to attend only to
the last `sliding_window` (512) keys, but the initial port approximated them with full attention —
exact for short prompts, but diverging on long contexts (which coding agents hit). Now each layer
gets the right additive mask: GLOBAL layers stay full causal, LOCAL layers additionally drop keys
older than the window. Both masks are built over ABSOLUTE positions (`build_gemma_masks`) so they're
correct at decode (`offset > 0`), and they coincide whenever the whole context fits the window — so
short prompts are byte-unchanged (`mlx_gemma3_chat` still clean). A deterministic unit test proves
the local mask bands the causal mask to the window (prefill) and windows correctly at decode (no
model needed). The mask keeps the full KV cache, so memory is still O(context) — a bounded windowed
KV cache is a later optimization, not a correctness gap. Fork rev `f3e66904`.

## mlx-native — Gemma 3 (text) support — own model file + three general template/EOS/lm_head fixes
Completed: 2026-06-14
Opens Google's Gemma 3 family (`model_type: "gemma3_text"` / multimodal-wrapper `gemma3`). Unlike
Mistral/Phi-3 this is a genuine port (`gemma3.rs`): the `(1 + weight)` RMSNorm convention (f32),
embedding `sqrt(hidden)` scaling, per-head q/k RMSNorm, GELU(tanh) MLP, four norms per layer,
alternating local/global attention (per-layer RoPE base `rope_local_base_freq` vs `rope_theta`,
every `sliding_window_pattern`-th layer global), and `query_pre_attn_scalar^-0.5` scale; own
`Generate`, `LoadedModel::Gemma3`. Validated end-to-end (`mlx_gemma3_chat`, gemma-3-1b-it-4bit):
*"Paris is the capital of France."* (clean). Getting there required three fixes that are GENERAL
improvements, not Gemma-specific: (1) mlx-community 4-bit conversions ship a SEPARATE quantized
`lm_head` even for tied models (its quant params differ from the embedding's) — detect an `lm_head.*`
key and use it; (2) chat templates that emit `{{ bos_token }}` themselves (Gemma) got an empty BOS
because the minijinja context lacked it — `bos_token`/`eos_token` are now threaded into the render
context (a BOS-sensitive model was pure garbage without the leading `<bos>`); (3) the assistant
turn-end token `<end_of_turn>` (106) wasn't in EOS (config eos is only `<eos>`=1), so the model
over-ran into garbage past its answer — the worker now adds the tokenizer's turn-end token to EOS.
Added to `models::RECOMMENDED`; 131/0. Deferred: the 512 sliding window is approximated by full
attention (exact within the window); Gemma 2's logit soft-cap and the vision tower are separate.
Fork rev `9b4c844f`.

## mlx-native — Phi-3 support (fused-projection split into the Llama path, no new model file)
Completed: 2026-06-14
Opens Microsoft's Phi-3 family (`model_type: "phi3"`). Phi-3 is the Llama architecture but ships
FUSED projections — one `qkv_proj` and one `gate_up_proj` per layer instead of separate
`q/k/v_proj` + `gate/up_proj`. Instead of a whole new model file, `llama::load_phi3_model` splits
each fused tensor along the OUTPUT axis into the separate weights the Llama structure expects, then
returns a `llama::Model`. The 4-bit AFQ packing is along the INPUT axis, so row-slicing the
weight/scales/biases is exact (no unpacking). Phi-3 then runs on the existing Llama path — Generate,
batched decode, sampling, everything — with no new runtime variant (`"phi3" => load_phi3_model →
LoadedModel::Llama`). Validated end-to-end first try (`mlx_phi3_chat`, Phi-3-mini-4k-instruct-4bit):
*"The capital of France is Paris."* Added to `models::RECOMMENDED`; `supported_model_type` + the
dense-classification guard updated; 131/0. (Phi-3-mini-4k uses full RoPE; the 128k su/longrope
variant would need `rope_scaling` threaded — a small follow-up. Phi-3.5-mini is the same arch.)
Fork rev `0bfa4bd6`.

## mlx-native — verified Llama-family aliases + non-quantized (bf16) load (SmolLM2)
Completed: 2026-06-14
Closes two "quick & cheap" catalog verifies with one model. `mlx-community/SmolLM2-1.7B-Instruct` is
a non-Llama-3 `model_type: "llama"` checkpoint AND a non-quantized bf16 model, so running it
(`mlx_smollm_chat` → *"The capital of France is Paris."*) confirms both `mlx-native-llama-aliases`
(the wider Llama family runs on the shared llama path) and `mlx-native-fp16-verify` (the AFQ loader's
`quantization = None` branch loads full-precision MLX checkpoints, just using more RAM). The recent
`head_dim` config-tolerance fix held for SmolLM2 too. Added to `models::RECOMMENDED` as a tiny,
light, non-Qwen option.

## gateway — admission counters in /stats (fast-lane hits, shed/429, queued, admitted)
Completed: 2026-06-14
Finishes `concurrency-observability`. The `/stats` `admission` block now carries, alongside the
instantaneous window (limit / in-use / waiting / free), four cumulative scheduler counters:
`admitted` (total requests that got a slot), `fast_lane` (the subset that took a reserved fast-lane
slot), `shed` (rejected with HTTP 429 because the queue was full), and `queued` (had to wait for a
slot). They live in the `AdmissionScheduler`'s `State` under its existing mutex (no new atomics):
`take()` bumps `admitted` (+`fast_lane`), the full-queue path bumps `shed` before returning
`Overloaded`, a queued arrival bumps `queued`, and a pumped-but-cancelled waiter decrements back so
the count stays exact. Exposed through `AdmissionSnapshot` + a new `AdmissionScheduler::counters()`.
Test `counters_track_admit_fastlane_queue_and_shed` walks the full flow. The admission policy
(limit, fast-lane reservation, queue depth) is now tunable from data.

## mlx-native — Mistral / Mistral-Nemo support (Llama-path alias) — VALIDATED + 2 config fixes
Completed: 2026-06-14
Opens the Mistral family (`model_type: "mistral"`) at near-zero cost and validated it end to end
(*"Paris is the capital of France."* on Mistral-7B-Instruct-v0.3-4bit). Mistral / Mistral-Nemo are
architecturally Llama and upstream `mlx_lm` serves them with the *llama* class, so `LoadedModel::load`
routes `"llama" | "mistral" => llama::load_llama_model` and `supported_model_type` admits `"mistral"`
— no new fork model file. (The one delta, Mistral's 4096 sliding-window attention, is approximated by
the llama path's full attention: identical except beyond the window — fine for agents, bounded by the
KV preflight.) Running it surfaced two GENERAL config quirks (not the alias itself), both fixed in the
fork so the whole Llama family is more config-tolerant: (1) Mistral's `config.json` omits `head_dim`
→ `llama::ModelArgs.head_dim` is now `Option`, default `hidden_size/num_attention_heads`; (2) Mistral
ships `chat_template` as the older list-of-`{name,template}` form → `load_model_chat_template_from_str`
parses both the string and list forms (picking the `"default"` entry), with a unit test (and the
pre-existing broken `mlx-lm-utils` tests fixed along the way). Added to `models::RECOMMENDED`; fast
guards + `mlx_mistral_chat` network test; 131/0. Fork rev `3f230b2a`.

## mlx-native — idle-unload proven to reclaim memory (100%) + non-blocking unload + memory in /stats
Completed: 2026-06-14
Follow-through on the worker-join `Drop`: proves it actually frees the model's RAM and stops it
blocking the runtime. (1) **Proof it reclaims memory.** MLX weights live in unified-memory Metal
buffers that process RSS does NOT capture (an `ps -o rss` probe saw the load add only ~600 MB and free
~0). Added a fork `mlx_rs::memory` module wrapping mlx-c's `mlx_get_active_memory` / peak / cache, and
a test (`mlx_drop_reclaims_memory`) using MLX's own counter: load a model, chat, drop the backend —
`active before=0MB → after_load=2197MB → after_drop=0MB`, i.e. **100% of the model's Metal memory is
reclaimed** on drop. So idle-unload genuinely returns the RAM. (2) **Non-blocking unload.** The
`Drop` now joins the worker (blocks until buffers free), so `unload()` doing `*backend = None` inline
under the `backend` RwLock would stall every concurrent `current()` reader and block a tokio thread.
Fixed: take the backend out of the lock first (so `current()` reports unloaded immediately), then free
it on `spawn_blocking`. (3) **Observability.** `/stats` now reports `mlx_memory_mb` (active / peak /
cache) — watch the model's footprint, and watch `active` drop to ~0 after an idle-unload.

## mlx-native — verified batching fires through the admission layer + batch observability
Completed: 2026-06-14
Closes the loop on the batched-decode work: confirmed that concurrent load actually batches through
the real production path, and exposed it operationally. The gateway serves every request via
`concurrency::admit_wrap(backend)` with `limit = concurrency_capacity() = batch_cap()`, so a new test
(`mlx_admit_wrap_batches_e2e`) wraps the REAL MLX backend the same way, asserts the admission limit is
2, and fires two concurrent requests — admission lets BOTH reach the worker and they land in ONE
`run_batch` (`Paris`/`Tokyo`, `run_batch calls=1`). So the batching isn't serialized by admission; it
fires end-to-end. Observability: promoted the batched-decode counters to real metrics
(`mlx_native_backend::batch_stats()` → runs / rows / mid-decode admits / peak batch size) updated in
`run_batch`/`run_batch_hybrid` + on each continuous admit, and surfaced them in the gateway `/stats`
JSON alongside a new `admission` block (limit / in-use / waiting / free). `/stats` now answers "how
many concurrent requests actually share a forward" — `batch.avg_occupancy = rows/runs`, `batch.max`
the high-water size, `batch.admits` the continuous admissions. Verified live: after a 2-row batch,
`batch_stats()` reports `runs=1 rows=2 admits=0 max=2`. Default (no-feature) build returns `None`.

## mlx-native — join the worker thread on drop (deterministic unload + model swap)
Completed: 2026-06-14
`MlxNativeBackend` spawned its `!Send` model-owning worker thread **detached** (the `JoinHandle`
was discarded) with no `Drop`, so dropping a backend only closed the job channel and let the worker
free its ~8–15 GB of MLX buffers **asynchronously**. A subsequent model load then raced that teardown
on the shared single-stream Metal context — so an unload didn't deterministically reclaim RAM, and a
load→unload→load swap could corrupt MLX state. Fix: keep the worker `JoinHandle` and add a `Drop` that
closes the channel (the sender is now an `Option`, taken first so `blocking_recv` returns and the
worker exits) then **joins** the thread — the model's buffers are fully freed before drop returns.
Validated by `mlx_sequential_backend_loads`: load a backend, run a **batched** decode, drop it
(join+free), then load a *second* backend in the same process and chat — both answer correctly
(previously the second load hit corrupt MLX state). This is the model-unload-on-idle / model-swap
path. (Note: running multiple model `#[tokio::test]`s in one `cargo test` process still crashes —
each test spins its own tokio runtime; that's a harness artifact, not the production path. Run model
e2e tests individually, as the `#[ignore]` markers already imply.)

## mlx-native — batched sampling (temperature/top-p/top-k requests batch too, not just greedy)
Completed: 2026-06-14
Batched decode is no longer greedy-only. The new fork sampler
`qwen3::sample_rows(logits[B,vocab], temp[B], top_k[B], top_p[B])` samples one token per row with
each row honoring its OWN temperature / top-k / top-p, via a single unified nucleus path: `top_k <= 0`
and `top_p >= 1` keep all tokens (plain temperature-categorical), and `temp == 0` is a per-row argmax
override — so a single batch can MIX greedy and sampling requests. The batching gate relaxed from
`is_greedy` to `is_batchable`: any request batches unless it needs a repetition penalty (per-row
history scattered into the logits) or a fixed seed (per-row RNG keys), which stay on the serial path.
`run_batch`/`run_batch_hybrid` build per-row `[B]` param arrays from each row's `SamplingParams` and
call `sample_rows` in place of argmax at every selection point (the decode step, mid-decode admission,
and the first token after prefill). Validated: the fork's `sample_rows_per_row_collapses_to_argmax`
proves a mixed per-row batch each collapses to its own argmax deterministically; the existing greedy
end-to-end tests now route through `sample_rows` at temp 0 and stay byte-exact (`Paris`/`Tokyo`/
`Berlin`); and `mlx_batched_sampling_two_concurrent` confirms two `temperature=0.7` requests batch
(`run_batch calls=1`, previously they fell back to serial) and stream coherent output. This widens how
many concurrent requests actually share a forward — real agents often run with temperature > 0.

## mlx-native — continuous batching (admit queued requests into a live batch mid-decode)
Completed: 2026-06-14
Batched decode no longer waits for the whole batch to drain before serving the next request. While
a batch decodes, `run_batch`/`run_batch_hybrid` now ADMIT queued greedy jobs from the worker channel
into freed or spare slots (up to `ROZUM_BATCH`): a short request that finishes frees its slot, and a
waiting request is prefilled and stacked into the batch on the next step instead of idling — better
GPU utilization under uneven response lengths and bursty arrivals. The decode loop tracks the KV
`width` and each row's pad explicitly (invariant `pad_i = width − len_i`); admitting a row grows the
shared width (left-padding existing rows) only if the new prompt is longer, then concatenates it on
the batch axis (dense KV or the heterogeneous hybrid `LayerCache`). It's byte-exact by the same
argument as the initial ragged assembly — the new row's left-pad is masked and its RoPE offset is
its true position — so an admitted row decodes identically to running alone. Non-greedy jobs pulled
from the queue are run serially afterward; a lone greedy request still goes serial (keeping the
prefix-KV LRU). Validated end-to-end (`mlx_continuous_admit_three`): three concurrent requests with
`ROZUM_BATCH=2` — the first two batch, the third is admitted into a freed slot mid-decode (ONE
`run_batch` call, `BATCH_ADMIT_COUNT` confirms the admission), and each returns its own correct,
uncontaminated answer (`Paris` / `Tokyo` / `Berlin`). All dense + hybrid byte-exact and scheduler
tests remain green.

## mlx-native — batched/parallel decode for hybrid Qwen3.6 (the primary coding model)
Completed: 2026-06-14
Extends batched decode to the hybrid Qwen3.6 arches (dense `Qwen35` + MoE `Qwen35Moe`) — the models
that actually run the coding agents — so two+ concurrent sessions share one forward. The feared
blocker ("the GatedDeltaNet recurrence can't be left-padded") was a non-issue: we prefill each
sequence separately, so no pad token ever advances the recurrence, and the GDN state is fixed-size
per row. The GatedDeltaNet turned out to be **already batch-generic and row-independent** (kernel
grid spans `b*hv`, conv+recurrent state is `[B,…]`) — proven byte-exact by a synthetic probe with no
model load (`gated_delta_batches_row_independent`). So hybrid batched decode is just: the dense
ragged path for the full-attention layers (left-pad+stack KV, per-row RoPE + key-pad mask, ported to
`qwen3_5::Attention` via two thread-locals — OFF by default, B=1 byte-identical) **plus stacking the
fixed-size conv + recurrent state on the batch axis for the GatedDeltaNet layers** (no padding, rope,
or mask). `run_batch_hybrid` assembles the heterogeneous `qwen3_5::LayerCache` and serves both hybrid
arches (shared Model API); the worker routes hybrid greedy batches to it via `is_hybrid_arch`.
Validated on the real Qwen3.6-27B: **byte-exact** per row vs serial decode incl. the padded row
(`mlx_hybrid_batched_ragged_byte_exact`); two concurrent sessions batch into ONE call with distinct,
uncontaminated answers — `"Paris"` / `"Red"` (`mlx_hybrid_batched_scheduler_two_concurrent`); and
**2.30× throughput** at B=2 (`mlx_hybrid_batched_decode_throughput`, test profile — even higher than
dense's 1.98× because hybrid decode launches more ops per token for batching to amortize). With
single-stream hybrid decode already maxed (~90% of Python), batching is now the only lever that
scales hybrid throughput, and it works. Fork rev `9a3b3949`.

## mlx-native — batched/parallel decode (dense Qwen3): 2 concurrent sessions in one forward
Completed: 2026-06-14
The native MLX backend was capacity-1: one worker thread ran jobs strictly serially, so two
sessions (Claude Code + Codex, or several meeting-room agents) serialized — the second queued
behind the first. It now **batches concurrent greedy requests through one `forward`**. With
`ROZUM_BATCH=N` (default 1 = the proven serial path), `worker_main` drains up to N already-admitted
jobs within a small `ROZUM_BATCH_WINDOW_MS` window (default 10ms), batches the greedy (argmax) ones
(≥2) via `run_batch`, and runs everything else — non-greedy requests, single jobs, non-batchable
arches (Llama/Qwen2/hybrid Qwen3.6) — on the existing serial prefix-KV path. `run_batch` prefills
each sequence separately (correct per-sequence KV, keeps prefix reuse), assembles one left-padded
batched cache (`ConcatKeyValueCache::{kv_used, from_kv}`), then decodes all rows together: per-row
RoPE via `qwen3::set_batch_pad_offsets` + a per-row left-pad mask, argmax per row, per-sequence
detok/stream (`BatchSeq`), and retires a row on EOS/max-tokens/runaway by slicing it out
(`take_axis`) and re-assembling the mask/offsets. `concurrency_capacity()=Some(ROZUM_BATCH)` so
admission admits B. **Why it's a real win:** decode is ~92% CPU graph-build, and batching does ONE
build for B sequences — it amortizes exactly the cost `mlx-native-perf-compile` couldn't reduce
(`mx.compile` was net-negative here), so the two perf threads converge. Validated: B=2 throughput
**126.3 vs 63.9 t/s = 1.98×** (`mlx_batched_decode_probe`); ragged forward byte-exact to 1 bf16 ulp
(`mlx_batched_ragged_byte_exact`); end-to-end `mlx_batched_scheduler_two_concurrent` — two
concurrent requests batch into ONE `run_batch` call and each row gets its own uncontaminated answer
(`France="Paris." Japan="Tokyo"`). B=1 path is byte-identical to before (per-row rope OFF by
default) — zero regression. Continuous batching (admit a queued job mid-decode) and hybrid Qwen3.6
batching are follow-ups.

## launch — `rozum launch codex` works out-of-box (+ quiet /v1/models)
Completed: 2026-06-14
Codex now launches against the local gateway like Claude already does. Codex **ignores
`OPENAI_BASE_URL`** and (≥ 0.137) needs the Responses API, so `rozum launch` detects a `codex`
program and injects the `-c` overrides on top of the user's `~/.codex` (left intact):
`model_provider=rozum`, `model_providers.rozum.base_url=…/v1`, `wire_api="responses"`,
`env_key="OPENAI_API_KEY"`, and `-m local` (only if the user didn't pass a model). Verified:
`rozum launch --model <spec> -- codex exec "…" --dangerously-bypass-approvals-and-sandbox` →
Codex connects (`provider: rozum`) and answers, `rc=0`. Also: `/v1/models` now returns an empty
`models: []` next to the OpenAI `data` so Codex's model-list refresh stops logging a non-fatal
"failed to refresh available models" warning (its `Model` entries have many required fields, but
the launch forces `-m local`, so the list is unused).

## mlx-native — prefix-KV cache: per-session LRU (interleaved sessions each reuse)
Completed: 2026-06-14
The prefix cache kept a single slot per worker, so *interleaved* conversations thrashed it:
session A's turn → session B's turn evicts A → A's next turn re-prefills from scratch (no
reuse for anyone). This matters whenever more than one conversation shares a gateway — several
meeting-room agents, or Claude Code + Codex at once. Replaced the single slot with a small
**LRU** (`PrefixStore`, default 4 slots, `ROZUM_PREFIX_CACHE_SLOTS`): each request reuses the
stored conversation it extends via a **longest-prefix match** (`best_match`), content-based so
no per-dialect session id is needed; the matched entry is replaced at MRU, an unmatched (new)
conversation inserts + evicts the LRU. A worker serves one model, so only the dense or the
hybrid LRU is populated. Verified live (small dense model, A1/B1/A2/B2 interleaved):
`SLOTS=4 → 2 reuse fires` (both A2 and B2 reuse their own prefix), `SLOTS=1 → 0` (thrash).
Each slot holds a conversation's KV, so it costs memory — lower the slot count for very long
contexts. Unit test `prefix_store_best_match`; byte-exact reuse tests still green.

## mlx-native — prefix-KV cache: key on the conversation boundary (make reuse fire)
Completed: 2026-06-14
The prefix-KV cache (dense + hybrid) was keyed on the **full prompt**, so reuse never
actually fired: the trailing generation prompt — especially the thinking-off
`<think></think>` prefill — does NOT recur next turn (the same turn is later re-rendered
as a *completed* message), so consecutive prompts share only the **conversation** prefix
(measured: LCP 3525/3529 — they diverge in the last 4 tokens). With `starts_with(full
prompt)` the match failed every time (`reuse_len=0`), and the byte-exact tests passed
*vacuously* (fresh == fresh). Fix: persist + key on the **conversation boundary** (the
prompt rendered without the generation prompt, `render_prompt_opt(add_gen=false)`); the
next turn `starts_with` that and reuses it. For hybrid, the Linear-state snapshot is now
taken at that boundary too — prefill the conversation part, snapshot, then forward the
tiny generation-prompt tail (`Generate::set_gen_prompt_len`, fork rev `c9ee1940`),
byte-exact (the split is position-local + causal). **Now reuse fires** (e.g.
`reuse=3522/3547, prefill 25 new tokens`) and a turn-2 prefill on a ~3.5k-token context
drops **2.62s → 0.13s (~20×)**. Byte-exact tests now genuinely exercise reuse.

## Gateway — `POST /v1/responses` (OpenAI Responses API): Codex now works
Completed: 2026-06-14
Codex CLI ≥ 0.137 dropped `wire_api="chat"` and **requires** the OpenAI Responses API; the
gateway only had `/v1/chat/completions` (+ Anthropic `/v1/messages`), so Codex got 404 and was
**fully blocked**. Added `responses_handler`: it translates the Responses request
(`instructions` → system; `input` items — messages / `function_call` / `function_call_output`;
flat `tools`; `max_output_tokens`) into the internal `ChatBackend` and streams the typed
Responses SSE protocol (`response.created` → `output_item.added`/`content_part.added` →
`output_text.delta` → `output_text.done`/`content_part.done`/`output_item.done` →
`function_call` items (`arguments.delta`/`.done`) → `response.completed`; non-stream returns the
final `response` object). One render fix was needed: Codex sends a top-level `instructions`
**and** a `developer` message — two system turns — which the Qwen3.6 template rejects
("System message must be at the beginning."); the conversion now folds all system/developer text
into one leading system message. **Codex e2e build task PASSES end-to-end** (`reverse-cli`,
`cargo run -- hello` → `olleh`, `rc=0`, ~71 s). Tests: input/tool conversion, multi-system fold,
response-object shape, SSE smoke.

## mlx-native — prefix-KV cache reuse for the hybrid Qwen3.6 arches
Completed: 2026-06-14
Extends prefix reuse to the hybrid Qwen3.6 models (Qwen35 + Qwen35Moe — the models the e2e
runs). Their `Full(KV)` layers truncate to the shared prefix like dense; their `Linear`
GatedDeltaNet layers carry a **recurrent** state that can't be truncated, so it is deep-copied
(`Array::deep_clone` → own buffer, survives decode buffer donation) at the **end of prefill**
(offset == prompt len) and restored on the next reuse. Fork (`fd284599`):
`LayerCache::{truncate, snapshot, restore}` + `LinearSnap`, `Generate::with_cache` (start from a
pre-populated cache, snapshot the Linear state right after the prefill step) +
`into_cache_and_snapshot`. rozum: `stream_generation` returns the iterator so the hybrid arms
reclaim the cache + snapshot; the worker persists `HybridPrefix{ids, cache, snap}`, and on reuse
truncates Full + restores Linear + prefills only the new suffix. **Byte-exact** vs a fresh
prefill (integration test `mlx_prefix_reuse_byte_exact_hybrid` on the deterministic Qwen3.6-27B).
Now every agentic turn — dense OR hybrid — skips re-prefilling the growing conversation.

## mlx-native — prefix-KV cache reuse across agentic turns (dense)
Completed: 2026-06-14
Every Claude Code / Codex turn used to re-prefill the **entire growing conversation** (a fresh
cache per request) — the dominant agentic latency, not decode. The cap-1 worker now persists the
previous request's prompt ids + KV; when the next prompt strictly extends it (the append-only
agentic-loop case) it truncates the cache to the shared prefix and prefills only the **new
suffix**. Byte-exact — the kept `[0,reuse)` KV is exactly what a fresh prefill computes, and
`create_attention_mask` builds the causal mask from the cache offset (integration test
`mlx_prefix_reuse_byte_exact`: reuse output == fresh prefill). Dense arches (Qwen3 / Qwen3-MoE /
Llama / Qwen2); needs the new fork method `ConcatKeyValueCache::truncate`. Hybrid (Qwen3.6) is a
scoped follow-up (its recurrent state needs snapshotting, not truncation). `ROZUM_PREFIX_CACHE=0`
disables.

## mlx-native — runaway-stop: bound a single runaway generation (reliability)
Completed: 2026-06-14
One greedy generation could loop (repeat a short block / never emit EOS) and generate to the
client's large `max_tokens`, pinning the cap-1 worker for minutes (the e2e `test` task hit a
600 s hang, `result=None`). Two guards in the backend: a hard `max_tokens` ceiling
(`DEFAULT_OUTPUT_CEILING=8192`, `ROZUM_MAX_OUTPUT_TOKENS` overrides) and `is_runaway_loop` in
`stream_generation` — stop when the last 64 generated tokens are exactly periodic with period
≤16 (a short block repeated ≥4×), which catches a greedy loop in ~64 tokens with no false
positives on real text (`ROZUM_REPEAT_GUARD=0` disables). `--max-turns` does NOT help (it bounds
the agentic loop, not one generation). Unit test `runaway_loop_detection`.

## Gateway — parse Qwen3.6's `<function=>` XML tool-call format (agentic coding fix)
Completed: 2026-06-13
Qwen3.6 emits tool calls in EITHER the JSON form
(`<tool_call>{"name":…,"arguments":…}</tool_call>`) OR the Hermes-style XML form
(`<tool_call><function=NAME><parameter=K>V</parameter>…</function></tool_call>`), chosen
nondeterministically. The backend only parsed the JSON form, so the XML calls were
silently dropped — the `<tool_call>` opener suppressed text streaming, the parse then
failed, and the client got an **empty response** with the tokens lost. For agentic
coding (Claude Code / Codex, which live in multi-step tool loops) this meant tool calls
randomly failing. Now `parse_tool_calls` accepts both forms, tolerates a missing
`</tool_call>` (model hit EOS after a complete body), and falls back to emitting the raw
run as text if a `<tool_call>` appeared but nothing parsed — so tokens are never silently
swallowed. Verified read→write_file end-to-end (5/5 OpenAI, 3/3 Anthropic).

## Gateway — CC/Codex compatibility fixes (audit)
Completed: 2026-06-13
A synthetic audit of the gateway against the OpenAI (Codex) and Anthropic (Claude Code)
dialects found the core protocol solid (streaming SSE, non-stream JSON, tool-use, stop
reasons, 422 validation). Two fixes:
- **stream default**: an absent `stream` field defaulted to SSE; the OpenAI/Anthropic
  specs default to non-streaming JSON. A client that omits `stream` now gets JSON, not an
  unparseable SSE stream. (Streaming clients — CC, Codex — always send `stream:true`.)
- **`--enable-thinking` flag (reasoning OFF by default)**: reasoning models (Qwen3) emit
  `<think>…</think>` — even an empty `<think></think>` — which leaked into CC/Codex content.
  The gateway now renders the chat template with `enable_thinking=false` by default (the
  prompt prefills a closed `<think></think>`, so the generated output is clean); pass
  `rozum gateway --enable-thinking` (or set `ROZUM_ENABLE_THINKING`) to turn reasoning back on.
- (`/v1/models` id `claude-rozum-<spec>` is intentional — `rozum launch` exports it as
  `ANTHROPIC_MODEL` so CC pre-selects the local model.)

## Gateway — hybrid decode now pipelines (prod path 62 → ~96 t/s)
Completed: 2026-06-13
The in-process gateway path (`MlxNativeBackend.chat`) decoded the Qwen3.6 hybrid models
~30% slower than the raw engine because `stream_generation` ran each token's GPU sync
(`eval` + `token.item()` host readback) serially, with `pipeline=false` left over from
when the GatedDeltaNet kernel blocking-eval'd its state per call. The retain fix
(`ROZUM_MLX_RETAIN`) removed that eval, so the hybrid models now pipeline like the dense
ones — the next token's forward `async_eval`s while the current token's id is read back.
Prod `backend.chat` decode 62 → ~96 t/s (the per-token sync 14ms → 0); byte-identical
output. (Profiling showed detokenization was never the cost — 0.03 ms/token.) Adds a
prod-path perf test (`mlx_moe_backend_chat_tps`) + a `hybrid_models_need_retain` guard.

## MLX native runtime — pre-allocated KV cache
Completed: 2026-06-13
`ConcatKeyValueCache` now pre-allocates its key/value buffers in 256-position blocks and
writes each decode step in place (`slice_update`), returning a `[:offset]` view — instead
of `concatenate`-ing (and reallocating) the entire history every step (mirrors Python
`mlx_lm`'s `KVCache`). The per-step O(context) copy becomes an amortised O(1) write (one
growth concat every 256 steps); decode t/s is flat across context. Decode output is
byte-identical (greedy IDs unchanged, all chat tests pass); chunked-vs-single prefill
stays argmax-exact (~1 bf16 ulp from the strided-slice SDPA on non-step-aligned single
passes). For long sessions this removes the realloc churn. Fork `d197d1da`.

## MLX native runtime — decode perf root-caused & fixed (+2.7× MoE)
Completed: 2026-06-13
Closed the native-MLX decode gap vs Python `mlx_lm` for the Qwen3.6 hybrid models.
- **Root cause:** `GatedDeltaNet` scaled q/k by `Array::from_f32(inv_scale)` — a *strong*
  f32 0-dim array — which promoted the whole hidden stream bf16→f32 at the first GDN
  layer (Python multiplies by a python float, staying bf16). The f32 stream then forced
  ~1000 bf16→f32 casts/token on the quantized scales/biases at every matmul and ran the
  matmuls in f32. Fix: scale by a scalar cast to q/k's dtype (one line each).
- **Also:** MoE expert-sort for prefill (`SwitchGLU` `_gather_sort`/`sorted_indices`),
  and `fast::rms_norm_no_weight` (null-weight kernel) for the weightless GDN norm.
- **Results (byte-exact, all chat tests pass):** Qwen3.6-35B-A3B-4bit decode 33→~88 t/s,
  prefill 943→~1215 (= Python 1180); dense 27B decode 16→~19.6.
- Tooling added: `mlx_export_to_dot` (mlx-c) + rust wrapper + `count_prims.py` for
  per-token graph-primitive counting. Full log: `docs/mlx-gd-bug/LOG.md`.
- Pins mlx fork `0d4b3729` (mlx-c `d71809d`); reproducible git-rev build verified.

## channel-wakeup fixes + rozum-native-channels (Tier 2)
Completed: 2026-06-11
Two corrections/extensions to the channel-wakeup launch flag that landed via the
`gateway-switch` build-fix:
- **Detection fix:** `ChannelWakeup::flags_for` probed `claude --help` for the
  flag string, but the research-preview `--dangerously-load-development-channels`
  flag is **hidden from `--help`** (verified empirically) — so detection always
  failed and channel wakeup silently never activated. Switched to a
  `claude --version` ≥ 2.1.80 gate (`claude_version_supports_channels`, unit-tested).
- **Server name via env:** `--channel-mcp-name` is now `Option<String>` resolving
  flag → `ROZUM_CHANNEL_MCP_NAME` → default `rozum`, so the name can be set in a
  shell profile/wrapper. Both `--channel-mcp-name` and `--no-channel-wakeup` are
  now hoisted by `reorder_launch_args` like the other launch flags.
- **rozum-native-channels Tier 2:** the mcp-proxy `instructions` now pin the
  Anthropic-independent fallback — if the agent isn't receiving `<channel>` events
  (client without channel support), keep a `meeting.wait_my_turn` long-poll
  outstanding while idle; it returns the instant someone speaks, so no turn is
  missed without channels. This makes `wait_my_turn` the universal native channel
  (Tier 2); `claude/channel` is the Tier-1 optimization, gateway piggyback the
  Tier-3 last resort. Spec: `docs/specs/rozum-native-channels.md`. No new deps.

## gateway-unload-on-idle — free model RAM when agents are attached but idle
Completed: 2026-06-11
The shared gateway now auto-`unload`s the resident model after a long idle window
while keeping the daemon alive, for the case the existing idle-exit deliberately
skips: agents attached (leases held) but not generating. idle-exit only fires at
`live_leases == 0` (process exit); this fills the `leases > 0`-but-idle gap by
dropping just the model's RAM and lazily reloading on the next chat. Implemented
on the **same 30 s idle watchdog tick** (`src/gateway.rs`): evaluate idle-exit
first (frees most when truly abandoned), then idle-unload when the model is
resident, nothing is `generating`, and `last_active` is older than
`ROZUM_GATEWAY_UNLOAD_IDLE_SECS` (default 900 s / 15 min; `0` disables). Reuses
`gateway-switch`'s `Switchboard::unload()` + serialized lazy reload; a new
`is_loaded()` guard makes it fire once (no per-tick re-drain/log spam) and
`can_reload()` keeps a `--dedicated` gateway (no builder) from ever auto-unloading.
Emits a `gateway_idle_unload` obs event. Spec: `docs/specs/model-unload-on-idle.md`.
Follow-ups (need a real model on Metal): cold-vs-warm reload measurement to decide
any fast-reload tier beyond the OS page cache, and pre-warm on a turn signal.
No new deps.

## runtime-config — declare backends, policy & default model in `rozum.toml`
Completed: 2026-06-11
The gateway's backend selection and default model can now be declared once in a
`rozum.toml` instead of re-typed as `--model` / `--backend` every session. A new
`src/config.rs` (`RuntimeConfig`, serde + `toml`) is resolved from `$ROZUM_CONFIG`
→ `./rozum.toml` → `$XDG_CONFIG_HOME/rozum/rozum.toml`; a malformed file (or a
`$ROZUM_CONFIG` that points at a missing one) is a hard error rather than a silent
fall-back, because a config the user deliberately wrote must surface. The schema is
a `[runtime]` block (`model`, `n_ctx`, `policy`, `backend`) plus an ordered list of
`[[backend]]` tables (`id`, `engine`, optional `model`/`n_ctx`/`url`/`enabled`).
Policies: `single` / `fallback` / `fanout`. Engine names span everything rozum can
build — the gateway engines `gguf`/`mistralrs`/`lmstudio`/`mlx`/`url` and the sync
meeting-room engines `hello`/`candle`/`llama-gguf`/`native-rust`/`external-command`
(the latter map to a placeholder in the sync `BackendRegistry`; the gateway builds
the HTTP/native ones for real).

`RuntimeConfig::default()` **is** the old auto-detect chain in code — `Fallback`
over `[gguf, mistralrs, lmstudio, mlx, url]` — so a user who never writes a config
sees zero behaviour change. The daemon's initial model load and every `gateway
switch` now walk this chain (`main.rs::build_from_config` / `build_choice`,
returning the first backend that builds), with the config injected into the
`Switchboard`'s `BackendBuilder` from `gateway-switch`. `--backend B` still
force-bypasses the chain to a single engine. `[runtime].model` / `[runtime].n_ctx`
fill in when `--model` / `--n-ctx` are omitted on `rozum gateway`; per-backend
`url` pins an explicit endpoint for an `lmstudio`/`mlx`/`url` entry. The
library/binary split from `gateway-switch` is preserved: the plan
(`gateway_chain()`) lives in the library, the async build stays in the binary. 12
Metal-free unit tests; lib suite 101 passing. No new deps (`toml` was already in).

### Build fix bundled with this work
The `gateway-switch` commit had swept in stray, incomplete `channel-wakeup` WIP
(`exec_agent` / `exec_agent_anthropic` call sites passing a `&channels` argument
the signatures never accepted), so `master` did not build on default features. A
separate fix commit threads `ChannelWakeup` through and applies `flags_for()`,
which also completes the `channel-wakeup-launch-flag` mechanism: a capable
`claude` now gets `--dangerously-load-development-channels server:<name>` appended
at launch (`--no-channel-wakeup` suppresses; `--channel-mcp-name` sets the name).

## gateway-switch — transparent in-place model/backend switch, reload & unload
Completed: 2026-06-11
`rozum gateway switch --model Y [--backend B] [--n-ctx N]` swaps the resident
model of the running shared daemon **in place**: it drains in-flight work, drops
the old model (never two resident — the memory constraint), loads the new one,
bumps a new `generation`, and resumes. Clients' launch-local proxies hold their
queued requests across the gap (`/v1/admit` advertises a closed window while
draining, so it looks like backpressure, not a failure) and a request already
mid-flight is held in the daemon until the swap finishes — so the swap is
transparent, just slower. The daemon now holds its backend in a `Switchboard`
(swap cell + an injected `BackendBuilder` closure over `rozum`'s own
backend-selection chain), and every chat handler takes a `ChatLease` for the
whole stream so a switch waits for streaming to finish before swapping. Drain
tracks a dedicated `generating` counter (the idle-watchdog `in_flight` counter
can't be used — it's held for parked requests and would deadlock the drain),
bounded by `ROZUM_GATEWAY_DRAIN_SECS` (default 120). `--backend` forces an engine
(`gguf`/`mistralrs`/`lmstudio`/`mlx`/`url`); on a build failure the switch reverts
the spec so the next request lazily reloads the old model.

`rozum gateway reload` drains then re-execs the current binary (transparent
daemon/binary upgrade after a `rozum` upgrade); the brief port gap rides the
proxies' existing replay path. `rozum gateway unload` drops the model to free RAM
but keeps the daemon listening — the next chat lazily reloads it (serialized so
racing requests reload once). `generation` was added to the `active.json`
registry (`#[serde(default)]`, continued monotonically across respawns) so a
proxy can tell "the daemon I was talking to was replaced" from a transient blip;
`rozum gateway status` shows it as `gen:`. Control plane is auth-gated localhost
`POST /control/{switch,unload,reload}`. A `--dedicated` gateway has no builder, so
all three are cleanly refused. No new deps.

## launch-no-model — `rozum launch --no-model` (upstream Anthropic, no gateway)
Completed: 2026-06-11
`rozum launch` can now run an agent with no local model at all: `--no-model`
(and a new first **"Anthropic (cloud — no local model)"** entry in the interactive
picker) bypass the gateway entirely — no daemon spawn, no lease, no launch-local
proxy, and none of the `ANTHROPIC_*`/`OPENAI_*` gateway/model env overrides. The
child inherits the operator's own Anthropic auth (`ANTHROPIC_API_KEY` / claude.ai
OAuth), exactly like a bare `claude`; only rozum's agent-context defaults
(`CLAUDE_CODE_DISABLE_*`, each applied only if unset) still apply. Resolution is
modeled as `LaunchTarget::{Local(spec), Anthropic}`; `--no-model` `conflicts_with`
`--model`/`--dedicated`/`--n-ctx`/`--port` (clap-enforced) and is hoisted by
`reorder_launch_args` like the value flags (also fixing `--dedicated` placement
after the program name). This is the mode that makes Claude Code features
requiring real Anthropic auth — notably **channels** — available to a
rozum-launched agent (empirically a local-gateway base URL does *not* block
channels, but no-model is the clean path). Spec: `docs/specs/launch-wrapper.md`.
No new deps.

## shared-gateway-poison — soft, graduated poison-prompt protection
Completed: 2026-06-11
A request that repeatedly crashes the shared daemon is now handled gently instead
of either retrying forever or hard-banning a possibly-good prompt. The proxy
fingerprints each request (`share::fingerprint`, a hash of the raw body bytes it
forwards verbatim — so the proxy and daemon agree without dialect normalization).
Crash-attribution is precise: an upstream send error is blamed on the prompt only
when the connection was established and then died (`!is_connect()`); a pure connect
failure is a failover gap and stays on the wait-for-health replay path. On a
crash-attributed failure the proxy degrades (the retry takes an exclusive `lane`
write-lock, serializing the risky prefill so no neighbour competes for memory —
clearing most big-prompt OOMs), counts per fingerprint, and after `ROZUM_POISON_MAX`
(default 3) attempts returns a soft, retryable 422 (`poison_refused`). When those
graduated retries are exhausted *and* the crash was the sole in-flight request
(`admit.stats().in_use <= 1`), the fingerprint is confirmed machine-wide to a TTL'd
`poison.json` (`ROZUM_POISON_TTL_SECS`, default 3600); ambiguous concurrent crashes
stay local. A confirmed entry is fast-refused both by the proxy before forwarding
and by the daemon's new `poison_layer` before running the model (defense-in-depth
that survives the very crash it guards against), and decays on the next clean (2xx)
prefill, both locally and machine-wide. Tunables: `ROZUM_POISON_MAX`,
`ROZUM_POISON_TTL_SECS`. No new deps.

## shared-gateway-replay-retry (part 2) — two-tier admission
Completed: 2026-06-11
The daemon now advertises its admission state and each launch's proxy holds its
client's requests at the edge instead of bouncing them off a full daemon.
Tier-1 (global): `GET /v1/admit` reports `{limit,in_use,waiting,free}` from the
daemon's `AdmittingBackend` via a new defaulted `ChatBackend::admission_stats()`
(ungated backends report an always-free window). Tier-2 (per client): each proxy
runs its own `concurrency::AdmissionScheduler` (SJF + reserved fast lane, cost
estimated from body size, unbounded queue — a proxy never sheds its own client)
over the single agent's parallel requests, and `wait_for_window` polls `/v1/admit`
to hold a queued request until the daemon signals room (bounded; fail-open on a
probe failure, so the `429`/`Retry-After` backstop still applies). The local
admission guard is held for the whole stream. Env: `ROZUM_PROXY_ADMIT` (4),
`ROZUM_PROXY_FASTLANE_TOKENS` (1024). Reuses the one `concurrency` module at both
tiers. Completes `shared-gateway-replay-retry`. No new deps.

## shared-gateway-replay-retry (part 1) — replay before first token + smart retry
Completed: 2026-06-11
The launch-local proxy now makes a daemon crash transparent to the agent. The
`forward` path buffers the request body once and re-sends it on a replay loop:
a connection failure *before any response byte reaches the agent* is safe to
replay, so the proxy waits for re-election to bring the daemon back on the same
stable port (`wait_for_health`) and retries — the agent sees a slower response,
not an error. Once a `Response` is returned (status+headers committed), a
mid-stream death surfaces the error instead (we can't un-send tokens). Retries
use capped exponential backoff + ±50% jitter (no `rand` dep — wall-clock nanos),
a per-request attempt cap, wait-for-health between tries, and honor the daemon's
`429`/`Retry-After` by holding and retrying rather than bouncing it back. Tunable
via `ROZUM_PROXY_MAX_ATTEMPTS` (6), `ROZUM_PROXY_BACKOFF_MS` (150),
`ROZUM_PROXY_HEALTH_WAIT_SECS` (60). 3 new tests (backoff math + an end-to-end
replay-after-daemon-returns test). No new deps. (Two-tier admission follows in
part 2.)

## shared-gateway-proxy — launch-local reverse proxy in the request path
Completed: 2026-06-11
New `src/proxy.rs`: a model-free launch-local reverse HTTP proxy (gateway analog
of the mcp-proxy). `proxy::serve` forwards every request to the shared daemon's
stable port and streams the response back verbatim (SSE token streams included),
buffering the request body (the seed for future replay), stripping hop-by-hop and
framing headers both ways, with a no-timeout client. An unreachable daemon yields
a clean 502; `daemon_port` lives in an AtomicU16 so a later phase can re-point it
at a respawned daemon. `rozum launch` (`start_launch_proxy`) binds an ephemeral
loopback port, spawns the proxy, and points the agent at it (failover watchdog +
lease heartbeat still target the daemon); falls back to the daemon directly if the
proxy can't bind. Foundation for replay / poison / two-tier backpressure /
transparent swap. 5 new tests incl. two real end-to-end tokio tests. No new deps.

## models-rm — delete a cached model from disk
Completed: 2026-06-11
`rozum models rm <spec> [-y]` frees disk by deleting a cached model. It
exact-matches the spec against `scan_all_installed()`, refuses if it is the
active gateway model (reads `active.json` + health-probes), prints what will be
freed, and confirms (`--yes`/`-y` skips; a non-TTY without `--yes` is refused).
HuggingFace (`models--owner--name`) and LMStudio (the repo dir holding the
`.gguf`) directories are removed directly; Ollama is delegated to `ollama rm`
(its blobs are content-addressed and shared) and refused if the binary is absent.
Dependency-free `which` helper added. No new deps.

## launch-model-picker — optional --model, interactive picker, takeover-if-idle
Completed: 2026-06-11
`rozum launch --model` is now optional. `resolve_launch_model`: given → use it;
omitted + a healthy gateway running → reuse its model (`using running model: …`);
omitted + nothing running on a TTY → interactive `pick_model_interactive` (cached
models first, `(cached, size)`; then not-cached `RECOMMENDED`, `(not cached, ~GB)`;
a not-cached pick re-confirms the download); omitted + non-TTY → error. Model
mismatch now does **takeover-if-idle** in `ensure_shared_gateway`: a different
running model with no live client leases is SIGTERM'd and replaced on the same
port; with live leases it is reused-with-warning (don't steal a live session).
`--dedicated` still bypasses sharing. No new deps.

## shared-gateway-leases — client leases drive daemon lifetime + status/stop
Completed: 2026-06-11
Third phase of `shared-gateway`. Each launch holds a `leases/<pid>` file
heartbeated every 15s (mtime = liveness); `share::live_lease_count` counts fresh
leases and reaps dead ones. The daemon's idle watchdog now stays up while any
lease is fresh OR a request is in flight OR there was recent HTTP, and idle-exits
(ROZUM_GATEWAY_IDLE_SECS, default 900) only when all are quiet — so leases, not
raw HTTP traffic, are the primary keep-alive for launch clients, while a manually
run `rozum gateway` is still kept alive by traffic. Added `rozum gateway status`
(model/port/pid/n_ctx/uptime/clients) and `rozum gateway stop [--force]` (SIGTERM,
refused while clients attached); `gateway --model` is now optional (required only
to run the daemon). No new deps.

## shared-gateway-failover — respawn the shared daemon on death
Completed: 2026-06-11
Second phase of `shared-gateway`. `share::try_spawn_lock` adds an O_EXCL
`spawn.lock` with stale-steal + drop-release (best-effort anti-stampede; the TCP
bind remains the hard single-owner guarantee). `spawn_failover_watchdog` runs in
each launch alongside the agent: it polls the daemon every 5s and, after two
consecutive misses, respawns it on the same port under the spawn lock (rechecking
health first), waiting up to 120s. Simultaneous watchdogs are damped by the lock
and deduped by the port bind, so a crashed/killed daemon comes back without the
user relaunching; the agent reconnects over the brief gap via its own retry (same
stable URL). No new deps.

## shared-gateway-mvp — share one model daemon across launches
Completed: 2026-06-11
First phase of `shared-gateway`. `rozum launch` no longer always loads its own
in-process model (two launches → two models → OOM). New `src/share.rs` registry
(`active.json` under `$XDG_STATE_HOME/rozum/gateway/`, atomic write +
remove-if-mine, `health_ok` probe, `is_reusable`, stable `DEFAULT_GATEWAY_PORT`
8089). `rozum gateway` publishes the registry and idle-exits after
`ROZUM_GATEWAY_IDLE_SECS` (default 900) when nothing is in flight (in-flight-aware
via an Activity counter in the auth layer, so long generations don't trip it).
`rozum launch` reuses a healthy running gateway (or a different-model one with a
warning), else spawns a detached `rozum gateway` (own process group, stdio →
gateway.log) and waits for health; the TCP-port bind is the single-owner
guarantee. `--dedicated` keeps the old private in-process gateway. Deferred to
later phases: flock anti-stampede + crash re-election, client-pid leases, the
launch-local proxy / replay / poison / two-tier backpressure, switch/reload/
unload, gateway status/stop, the model picker, and `models rm`. 3 share unit
tests (no Xcode); fmt + feature build clean.

## concurrency-backend-abstraction — generic admission for any backend
Completed: 2026-06-11
Lifted the concurrency machinery (scheduler, memory budget, fast lane,
backpressure, circuit breaker) out of the mistralrs modules into a generic
`src/concurrency` module (renamed from `mistralrs_admission`), and re-applied it
as a decorator. `ChatBackend` gained an optional `concurrency_capacity() ->
Option<usize>` hook (default `None`); `concurrency::admit_wrap` wraps a backend in
`AdmittingBackend` iff it advertises a capacity, and passes remote / self-
serializing backends through untouched (the safe default). `MistralrsBackend`
now reports `Some(max_num_seqs)` and its `chat()` is plain inference again — the
decorator owns admission. The budget math (`budgeted_max_num_seqs`,
`ConcurrencyBudget`, `per_seq_prefill_peak`) moved to `concurrency` and is reusable
by any in-process backend. Admission env renamed to generic `ROZUM_ADMIT` /
`ROZUM_ADMIT_FASTLANE_TOKENS` / `ROZUM_ADMIT_QUEUE_MAX`. `build_gateway_backend`
routes every selected backend through `admit_wrap`. 13 concurrency unit tests on
the default build (no Xcode); feature build + fmt clean. The new mlx-rs backend is
the first intended consumer: implement inference + return a capacity, get
admission/fast-lane/backpressure/breaker for free.

## concurrency-load-shedding — backpressure + OOM circuit breaker (Phase D)
Completed: 2026-06-11
Final phase of `mistralrs-concurrency-scheduling`. `AdmissionScheduler.admit`
now returns `Result<AdmitGuard, AdmitError>`: a full wait queue
(`ROZUM_MISTRALRS_QUEUE_MAX`, default 32, 0=unbounded) sheds with `Overloaded`.
`MistralrsBackend::chat()` acquires the slot before returning the stream, so an
overloaded backend surfaces as a genuine HTTP 429 + `Retry-After` (new
`ModelError::Overloaded`, mapped in the gateway for both the OpenAI and Anthropic
dialects). Circuit breaker: `trip()` lowers the live admission limit (floor 1) on
a detected Metal allocation failure and a 30 s cooldown `recover_step()` raises
it back toward capacity; the OOM'd request is surfaced (not auto-retried, to
avoid re-OOM) and detection is best-effort substring matching. Per-class
`max_tokens` was dropped as redundant (cost already weights `max_tokens`). 7
scheduler unit tests (no Xcode); feature build + fmt clean. This completes the
concurrency feature (A+B+C+D); follow-ups — chiefly `concurrency-engine-yield`
for true mid-prefill interleaving — are in BACKLOG.

## concurrency-admission — admission scheduler + fast lane (Phase B+C)
Completed: 2026-06-11
Second phase of `mistralrs-concurrency-scheduling`. New engine-agnostic
`src/mistralrs_admission.rs`: an `AdmissionScheduler` that gates actual
concurrency in front of the static engine `max_num_seqs`, with a runtime
`set_limit` (for Phase D), shortest-job-first queue ordering, and one reserved
fast-lane slot so short interactive requests jump ahead of queued big ones.
`admit(RequestCost) -> AdmitGuard`; the guard is held for the whole `chat()`
stream and releases the slot on completion/disconnect, waking the next waiter
(dead/cancelled waiters are skipped and their slot reclaimed). Config from
`ROZUM_MISTRALRS_ADMIT` (limit ≤ capacity) and `ROZUM_MISTRALRS_FASTLANE_TOKENS`
(default 1024, 0 off). 5 async unit tests, no Xcode needed; feature build clean.

Finding recorded: the fork does **not** yield between prefill chunks (chunking
is internal to `pipeline::step`), so the fast lane gives admission-order
responsiveness but not mid-big-prefill preemption — engine-yield filed as
`concurrency-engine-yield` in BACKLOG. Phase D (backpressure + circuit breaker)
remains.

## concurrency-budget — load-time budgeted engine max_num_seqs (Phase A)
Completed: 2026-06-11
First phase of `mistralrs-concurrency-scheduling`. Replaces the total-`hw.memsize`
1/2 ladder with a footprint budget: `budgeted_max_num_seqs(ConcurrencyBudget)`
(pure, in the lib) returns `clamp((0.8·available − weights − kv_pool) /
per_seq_peak, 1, ceiling)`, where `per_seq_peak = prefill_chunk × ~465 KB/token`
(constant under chunked prefill) and `ceiling` defaults to 8 (Metal is one GPU —
past a handful of concurrent prefills you gain tail latency, not throughput).
`resolve_max_num_seqs` in `main.rs` gathers the footprint from the existing
preflight helpers and applies env overrides (`ROZUM_MISTRALRS_MAX_SEQS` forces,
`ROZUM_MISTRALRS_SEQS_CEILING` caps, `MISTRALRS_PREFILL_CHUNK` sizes the per-slot
cost), logging a `concurrency_budget` obs event. `MistralrsOptions::default()`
now carries a plain serialised floor of 1. 6 lib unit tests (no Xcode), feature
build clean. Phases B+C (admission scheduler + fast lane) and D (backpressure +
circuit breaker) remain in SPRINT.md.

## mistralrs-adaptive-concurrency — memory-adaptive default for max_num_seqs
Completed: 2026-06-11
The mistralrs backend's concurrent-prefill cap (`max_num_seqs`) default is no
longer a fixed `1`. A new pure `default_max_num_seqs(total_ram)` policy keeps
the serialised `1` floor on the 24–36 GB Apple Silicon target band (where two
concurrent large-prompt prefills can OOM the Metal command buffer) and lifts it
to `2` on machines with ≥ 48 GB total unified memory, where PagedAttention +
chunked prefill + the disconnected-seq reaping fix make real concurrency safe.
The gate is on total `hw.memsize` rather than instantaneous free memory (which
over-predicts runtime headroom at load time). `ROZUM_MISTRALRS_MAX_SEQS`
overrides. Rationale + trade-offs documented in
`docs/specs/mistralrs-backend.md`.

## web-basic-auth — HTTP Basic Auth on the web bridge
Completed: 2026-06-06
The web bridge now requires HTTP Basic Auth for `/`, `/ws`, and `/transcript`.
The password must equal the room name; the username is unconstrained and is
used as the participant's alias in the chat. The server stamps every outgoing
`meeting.submit` with the authenticated alias regardless of any client-supplied
`name` field, so a tampered client cannot post under a different name. The
auth username is sent to the client via a new `{kind:"hello",name:...}` WS
envelope right after connect; the page-side name input is removed.

## tui-soft-wrap — soft-wrap long input lines in the TUI
Completed: 2026-06-06
Custom render of the input area: `tui-textarea 0.7` still holds the data and
processes input events, but its renderer is bypassed. `draw_input` builds
visual rows by wrapping each logical line at `inner_width` and places the
cursor manually via `f.set_cursor_position`. Autosize now counts wrapped
visual rows, so a single long line grows the input chunk upward instead of
scrolling horizontally.

## mcp-proxy-auto-mark — auto-emit mark_responding from mcp-proxy
Completed: 2026-06-06
`ProxyState` gained a `heartbeat_task` handle. When `meeting.wait_my_turn`
returns `your_turn:true`, the proxy fires an immediate `meeting.mark_responding`
and spawns a background task that refreshes it every 15 s. The task is aborted
on the agent's next `submit`/`leave` and on a fresh `your_turn:true` (which
restarts the heartbeat). Manual `meeting.mark_responding` calls from the agent
still work and refresh the timer identically.

## mcp-proxy-reconnect — transparent reconnect of mcp-proxy after rozum restart
Completed: 2026-06-06
`ProxyState` remembers the joined room name; `call_room_tool` now
catches transport failures and calls a new `try_reconnect_current_room`
that sleeps a capped backoff (`200ms…5s`, ~18 s total) waiting for the
Unix socket to reappear, reconnects, re-issues `_join_internal` with
the same display name, and retries the original tool call. The agent's
MCP session no longer sees `Transport closed` during a `rozum --room R`
restart.

## room-transcript-persist — room transcript persisted across rozum restarts
Completed: 2026-06-06
`Meeting` gained `persist_path: Option<PathBuf>` and an
`enable_persistence` method that loads
`$XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl` on
construction and re-numbers seq. `post_submission` appends every Turn
as one JSON line. A new top-level `--no-persist` flag disables both
(independent of the existing `rozum web --no-persist`). Web bridges
pick up the loaded history through their normal
`wait_my_turn(since_seq:0)` path. With `rozum --room R` the same room
name reopened after a restart resumes with full transcript intact.

## web-transcript-persist — bridge transcript persisted to disk
Completed: 2026-06-06
The web bridge now appends every `msg` envelope to
`$XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl` (one JSON line per
turn). On startup the bridge loads the last `TRANSCRIPT_CAP=2000` lines back
into the in-memory ring so a page reload after a rozum restart still shows
recent history. A new `--no-persist` flag on `rozum web` disables both the
write and the load. Client-side deduplication now keys on `(seq, ts)` so
persisted entries from earlier sessions — where seq numbering restarts — do
not collide with current-session entries.

## web-transcript-history — transcript replay on connect + lazy older-history paging
Completed: 2026-06-06
The web bridge keeps a bounded in-memory transcript ring (cap 2000). A new
`GET /transcript?from_seq=&limit=` REST endpoint returns slices for paging.
On WebSocket connect the bridge sends a `kind:"history"` envelope with the
last 200 entries; the client replays them through the normal append path with
seq-based deduplication. Scrolling within 60 px of the log top triggers a
fetch of the next older 200 entries and prepends them while preserving the
viewport. `web-transcript-persist` (separate slug) will lift the in-memory
2000 cap by reading from `transcript.jsonl`.

## tui-arrow-scroll — Arrow Up/Down always scrolls the transcript
Completed: 2026-06-06
Dropped the `textarea.lines().len() <= 1` guard so the Up/Down arrows scroll
transcript history even when the input area is multi-line. Textarea cursor
navigation moves to `Ctrl+Arrow` / `Home` / `End`. Per operator request.

## tui-autosize-input — TUI input area grows with multi-line composition
Completed: 2026-06-06
Replaced fixed `Constraint::Length(3)` with a dynamic
`(textarea.lines().len() + 2).clamp(3, max(3, area.height/3))` so the input
area grows upward when the user enters multi-line content via `Alt+Enter`.
Up/Down arrows now scroll the transcript history (in addition to PgUp/PgDn).
Soft-wrap of a single overflowing line is **not** in this slug — split into
`tui-soft-wrap` because `tui-textarea 0.7` has no native wrap.

## web-scrollback-sticky — sticky-bottom scroll, "↓ N new" pill, long-message collapse
Completed: 2026-06-06
`#log` now tracks `data-stick` on scroll; new messages auto-scroll only when
the user is within 40 px of the bottom, otherwise a sticky `↓ N new` pill
appears and clicking it snaps to bottom. Messages whose body exceeds 6 wrapped
lines or 600 characters render collapsed with an `[expand ▾]` / `[collapse ▴]`
toggle. Pure client-side change in `src/web/index.html`.

## web-presence-row — presence row, joined/left, tagged envelopes for the web bridge
Completed: 2026-06-06
`src/web/mod.rs` `room_loop` now emits tagged JSON envelopes
(`kind:"msg"|"presence"|"joined"|"left"`) instead of raw transcript JSON.
`src/web/index.html` dispatches on `env.kind`: presence line above the input
with `✏️` / `⏳` glyphs, header chips for participants, dim system lines for
join/leave. Display names are rendered with `textContent` (no innerHTML) so
they cannot inject HTML.

## web-autosize-input — Claude-style autosizing textarea in the web client
Completed: 2026-06-06
Replaced the single-line `<input id="msg">` with a `<textarea rows="1">` that
grows upward on input up to `30vh` (`20vh` on mobile). `Enter` sends,
`Shift+Enter` inserts a newline, `Esc` clears, no horizontal scroll, collapses
back to one row after send. Verified live by the operator.
