# Bugs

One entry per bug, newest first. Status flow: `open → needs-info → fixed → done`.
See `vendor/agent-plugins/bugs/commands/bugs.md`.

---

## BUG-012 — UCC launch registries: concurrency races + terminal reconnect loop (audit sweep)

- **Status:** fixed on `a1c073c`, deployed 2026-07-08; live-verified (concurrent launch, stop-during-start).
- **Source:** adversarial audit of the day's async-launch + terminal work (two review agents), not a
  field report — caught before the operator hit them.
- **Severity:** P2 — real races, but they need concurrent actions / restarts / same-second launches
  to trigger; the happy path was already working.

**Findings + fixes (all in `control.rs` + `terminal.ssc`).**
1. Lost update: `live_sessions/agents/coders` rewrite the registry on the STATUS-POLL path, so a
   poll's save could clobber a concurrent launch → orphan process / row stuck `starting…`. Fixed:
   `registry_lock()` serializes every load-modify-save.
2. Orphan on stop-during-spawn: a stop that removed a `starting…` (pid 0) record while the bg task
   was mid-spawn left an untracked participant/coder process. Fixed: `update_*_record` returns
   whether it hit; the spawn kills the just-spawned pid if the record is gone.
3. Eternal `starting…`: a control-serve restart mid-launch orphaned the row forever (prune kept all
   `starting…`). Fixed: `STARTING_TTL_SECS` (900s) prune / show-failed.
4. ID collision: two launches of the same agent in one second shared a tmux name → the 2nd
   `new-session` 500'd (sessions) or updated the wrong record (agents/coders). Fixed:
   `next_launch_seq()` suffix.
5. Terminal infinite reconnect: `onopen` reset the retry budget every cycle, so an
   open-then-immediately-close (session already ended) looped forever. Fixed: reset only after a 5s
   stable connection.
6. Terminal duplicate sockets: a tap during the retry wait + the pending timer both called
   `connect()`, doubling output. Fixed: `clearTimeout` + already-opening guard; manual reconnect
   resets the budget.

**Verified.** `cargo test --workspace` 635/0 (incl. new next_launch_seq + starting-TTL tests);
live: two simultaneous `/session/launch` → distinct ids `…-0`/`…-1`, both running, 2 tmux, no 500;
coder launch + immediate stop → 0 rows, 0 stray processes. Also removed `footprint_report`/
`footprint_for` (orphaned when launches went async). Not fixed (LOW, noted): `remain-on-exit` set
just after `new-session` has a sub-ms window on an instant-failing launch (cold-start takes seconds,
so not reachable in practice); non-SGR/modified-wheel mouse reports (CC uses SGR only).

---

## BUG-011 — phone terminal: "open terminal failed: terminal does not support clear"

- **Status:** fixed, deployed 2026-07-07 ~20:1x.
- **Reporter:** operator — first REAL phone attach to a session terminal (screenshot: the error
  text + immediate «отключено — переподключиться?»). This was the last never-browser-validated
  UCC piece (P4 terminal byte-flow).
- **Severity:** P1 — the terminal view was unusable from the phone.

**Root cause.** `session_ws_bridge` spawns the PTY child `tmux attach -t rozum-<id>` with the
inherited environment — and control-serve runs under launchd, which has NO `TERM`. The tmux
client refuses a terminal without a usable terminfo entry ("terminal does not support clear"),
exits, and the WebSocket closes right after the error bytes reach xterm.js.

**Fix.** `cmd.env("TERM", "xterm-256color")` on the PTY child (xterm.js is xterm-compatible).

**Verified.** Headless-Chrome attach to a live tmux session via `terminal.html?id=…` over the
funnel: the xterm screen shows the actual claude REPL content (no error), input round-trips.

---

## BUG-010 — «запустить сессию» does nothing: formBody posted EMPTY fields (framework bug)

- **Status:** fixed (scalascript `3edbf883a` + rozum async launch), deployed 2026-07-07.
- **Reporter:** operator — "Я здесь нажимаю «запустить сессию» - ничего не происходит" (from the
  phone, sessions form fully filled).
- **Severity:** P1 — the launch POST fired instantly but carried
  `{"agent":"","model":"","workdir":"","prompt":""}` → 400, silently.

**Root cause (framework, std/ui SPA bridge).** `.ssc` forms reference field signals by NAME —
`formBody([("agent","seAgent"),…])` — but `_ssc_ui_signal(name, init)` DISCARDED the name, and the
submit-time store `_sv` is keyed by NUMERIC signal id, so `sv["seAgent"]` resolved to `''` for every
field. Every by-name formBody in every emitted SPA posted empties. Repro'd live in headless Chrome
with request capture (body was key-correct but value-empty while the page visibly showed all
values).

**Fix 1 (scalascript `3edbf883a`).** `_signalsByName` registry (+ registration in
`_ssc_ui_signal`/`_ssc_ui_seedSignal`) and `_ssc_ui_resolveFormFields`: the render walk resolves
field refs to bridge ids AND collects the signals so their `_sv` entries stay fresh; unresolved
refs pass through verbatim. Regression test `SpaFormBodyNamedSignalsTest` (real JsRuntimeSignals,
headless node).

**Fix 2 (rozum, same operator symptom).** Even with the body fixed, a cold-start launch blocks for
minutes with zero feedback and the Tailscale funnel can time the request out. `session_launch_route`
is now ASYNC: validates fast, records the session as `starting…` immediately (the row in Live
sessions IS the feedback), loads the gateway + creates tmux in a background task, flips status to
`running` / `failed: <reason>`. Failed rows stay visible until closed (✕) — launch errors finally
reach the phone. `live_sessions()` prunes only completed records whose tmux died. New `status`
column in the sessions table.

---

## BUG-009 — every UCC page click bounced to #/ — agent/model pickers "did nothing"

- **Status:** fixed on `f8cf165`, redeployed 2026-07-07 ~05:3x; verified in a real browser.
- **Reporter:** operator — "Теперь не работает выбор агента в сессии" (after BUG-008 restored
  navigation). Almost certainly ALSO the UI half of the original BUG-006 complaint ("не работает
  выбор агента и модели в сессии") — it predates today's deploys.
- **Severity:** P1 — every in-page button (agent picker, model select, …) appeared dead.

**Symptom.** On `#/sessions`, tapping claude/codex/opencode (or a model `select`) visually did
nothing. Browser repro showed why it LOOKED dead: the click actually fired AND the signal set, but
the page instantly navigated `#/sessions` → `#/`, hiding the form again.

**Root cause.** The deploy script's injected close-on-click-outside handler:
`if(document.querySelector("[role=dialog]") && !e.target.closest("[role=dialog]")) location.hash="/"`.
The Model-details modal lives in an always-present `data-ssc-cond` branch (`display:none` when
closed) and `querySelector` finds hidden nodes — so the condition was true on EVERY click anywhere,
and any click outside the (invisible) dialog warped to home. Menu links survived only because their
own `href="#/…"` default action re-set the hash afterward.

**Fix.** Guard on real visibility: `_dlg.getClientRects().length` (0 inside `display:none` subtrees,
and unlike `offsetParent` it works under `position:fixed`).

**Verified** (puppeteer-core + system Chrome, busi-SSO cookie): agent picker claude→codex→opencode→
claude all update the label with hash staying `#/sessions`; model `select` fills the form model;
dialog still opens via `#/detail/…` and still closes to `#/` on a genuine outside click. Repro/verify
scripts: scratchpad `ucc-repro3.js` / `ucc-verify.js` pattern.

---

## BUG-008 — UCC menu navigation dead after the 03:27 redeploy (compiler/std skew)

- **Status:** fixed on `9a39a60` + site re-emitted and redeployed 2026-07-07 ~05:0x.
- **Reporter:** operator — "Опять не работает навигация в контрол центре."
- **Severity:** P1 — menu taps change the URL hash but the page never re-renders.

**Symptom.** After the BUG-007 deploy (03:27), tapping UCC menu items did nothing (hash changed,
view didn't). The 02:21 page was fine.

**Root cause.** The 03:27 SPA was emitted with a SKEWED toolchain: the repaired `/tmp/ssc-tk/bin/ssc`
launcher pinned the **Jun-29** `ssc.jar` from `~/work/my/scalascript/bin/lib` while `ssc.lib.path`/
`ssc.std.path` pointed at the **Jul-7** live std/plugins tree. That jar predates the std/ui React
bridge fix that registers `window.addEventListener('hashchange', () => _syncBridgeSignals())` — so
the emitted SPA never resynced bridge signals on hash change and navigation went dead. (Earlier
deploys used the since-removed `coord-main` worktree build, which had the fix; scalascript refreshed
`bin/lib` to a fresh consistent build at 03:59, after our emit.) Diff proof: old-jar emit vs
fresh-jar emit differ by exactly that one hashchange hook.

**Fix.** `deploy-ucc-web.sh` now makes the `/tmp/ssc-tk/bin/ssc` launcher a one-line DELEGATE to the
operator's canonical `~/work/my/scalascript/bin/ssc` (kept in lockstep with `bin/lib` by the
scalascript repo), so compiler and std can never skew again; the jar heredoc stays only as a
fallback, and a caller-provided `$SSC` is never rewritten. Site re-emitted with the canonical ssc.

**Verified.** Deployed page contains the `_syncBridgeSignals` hashchange hook; Node sandbox check:
2 hashchange listeners registered, `#/sessions`/`#/coders`/`#/agents`/`#/matrix`/`#/` all run
clean; deploy JS syntax + runtime-init checks green; `cache-control: no-store` + non-caching SW →
a plain reload on the phone picks it up.

---

## BUG-007 — UCC web launch fails on a cold host: "no shared gateway running"

- **Status:** fixed on `452e192` (+ deploy-script fix `0094bee`), merged to master and DEPLOYED to
  control-serve 2026-07-07; verified live end-to-end (cold start, switch, stop, inference).
- **Open note (minor, pre-existing):** the `prompt` field seeding uses `tmux send-keys … Enter`;
  in a HEADLESS tmux (no client attached) the CC REPL received the text but Enter did not always
  submit during shell testing — from the phone terminal (real xterm.js attach) typing is
  interactive so this shouldn't bite. Watch it when the operator validates the terminal from the
  browser; if seeded prompts sit unsubmitted, delay + retry the Enter or submit after first attach.
- **Reporter:** operator — "Что у нас за проблемы с запуском моделей и агентов через веб
  интерфейс? Почему это не работает?" (2026-07-07, after the BUG-006 deploy).
- **Severity:** P1 — the next bug in the BUG-006 chain: with the body parsing fixed, launching
  models/agents from the web still only works if someone already started a shared gateway from a
  terminal.

**Symptom.** Authenticated `POST /control/session/launch` (same for agent/coder launch and chat)
returns 409: `could not load <model>: rozum gateway switch: no shared gateway running` — while the
attached admission report says `fits: true`. On a cold host (after reboot, or after the gateway
idle-exited) every model-needing UCC action fails.

**Root cause.** `control.rs::ensure_gateway` knew only two cases: reuse the registered gateway if
it already serves the model, else `rozum gateway switch`. But `switch` swaps the model on a
*running* daemon and refuses when none is running. The CLI path (`rozum launch` →
`ensure_shared_gateway`, src/main.rs) handles cold start by spawning a detached daemon; the UCC
duplicate never got that branch.

**Fix.** `ensure_gateway` (now async): health-check the registry record (a stale record from a
crashed gateway falls through instead of returning a dead port); `switch` only when a healthy
gateway serves a different model; otherwise cold-start a detached `rozum gateway --model … --port
8089` daemon (own process group, output → gateway.log — the same shape as `rozum launch`'s
`spawn_detached_gateway`) and wait ≤300s for it to register and answer health. The daemon runs the
residency admission gate itself and idle-exits per `ROZUM_GATEWAY_IDLE_SECS` (default 900s), so a
web-started gateway frees RAM when unused.

**Verified.** `cargo test -p rozum-gateway ucc_` + `control::tests::`; live authenticated smoke on
:8411 (busi SSO cookie, SPA-shaped JSON body without Content-Type): cold host → launch returns
`{"ok":true,"id":…}`, gateway self-starts and registers on :8089, tmux session appears with the
claude REPL up, session stop works. The switch branch verified too: second launch with
`mlx-community:Qwen3.6-35B-A3B-4bit-DWQ` (the operator's target) swapped the model in place
(generation 1→2, same pid) and the claude session came up against it.

**Deploy fallout fixed along the way (same branch).** `deploy-ucc-web.sh` died mid-run on this
deploy and TRUNCATED the live `~/.rozum/ucc/site/index.html` to 0 bytes (the incident class
`ucc-duplicate-const-fix` warned about): the SSC launcher `/tmp/ssc-tk/bin/ssc` pointed at the
since-removed `scalascript/.worktrees/coord-main/bin/lib` jar dir (java: ClassNotFoundException),
and line 165's `emit-spa > "$SITE/index.html"` truncates the target before java starts. Fixed:
emit to `index.html.new` + non-empty check + `mv` (a failed emit leaves the live page untouched);
launcher default jar dir → the main checkout `scalascript/bin/lib`; launcher auto-regens when it
references a stale jar dir. Page regenerated and redeployed (414478 bytes, JS checks green).

---

## BUG-006 — UCC session launch buttons silently do nothing

- **Status:** fixed on `e451e6a` + hardened on `0a537df`; deployed to control-serve 2026-07-07.
- **Reporter:** operator — "В контрол центре не работает выбор агента и модели в сессии; хочу через веб интерфейс запустить сессию клауди и квен3.6, но ничего не происходит".
- **Severity:** P1 — blocks the phone/web control-center path for interactive coding sessions.

**Symptom.** In the UCC `#/sessions` page, selecting an agent/model/workdir and pressing
`launch session` leaves the UI unchanged and no tmux-backed session appears.

**Root cause.** The ScalaScript SPA's `formBody(...)` sends a JSON string body without
`Content-Type: application/json`. Axum `Json<T>` extractors on `/control/session/launch`
and sibling action routes reject those requests before handler execution, while the SPA does
not surface the non-2xx response. Stop/project actions also need to accept the same JSON body
shape instead of treating raw body text as the id/name.

**Fix.** UCC write routes now parse the browser body directly: agent/coder/session launch accept
JSON objects regardless of content type; stop routes accept JSON `{ "id": ... }` plus legacy
form/plain ids; project creation accepts JSON `{ "name": ... }` plus legacy form/plain names.
Malformed JSON-like bodies and missing ids/names return structured 400s instead of falling through.

**Verified.** `cargo test -p rozum-gateway ucc_`; `cargo test -p rozum-gateway control::tests::`;
`clients/control/deploy-ucc-web.sh`; unauthenticated `POST /control/session/launch` now reaches auth
middleware and returns 401 rather than the old extractor failure.

---

## BUG-005 — uncached model under `--offline` refused with bogus "~4398046511103 MB overcommit"

- **Status:** fixed on `feature/footprint-uncached-sentinel` (cargo check `--no-default-features` +
  2 unit tests green). Found while queuing the `matrix-add-coders` Qwen3-Coder smoke.
- **Reporter:** operator (smoke run "gateway not ready").
- **Severity:** P2 — blocks loading any not-yet-downloaded model on the offline bench path with a
  baffling, non-actionable error; no data loss / no reboot.

**Symptom.** `rozum-gateway gateway --model mlx-community:Qwen3-Coder-30B-A3B-Instruct-4bit --offline`
on an **empty** host (0 resident, ~20 GB free) refused:
`loading this model (~4398046511103 MB) would overcommit host RAM … Waited 240s` → the bench saw
"gateway not ready" and ran zero tasks. The baseline models worked only because they were already
cached.

**Root cause (two parts, both in `src/main.rs`).**
1. `estimate_model_footprint_bytes` returns the unknown-size **sentinel `u64::MAX/4`** when a spec
   isn't found in the local cache (`scan_all_installed`). `u64::MAX/4` bytes = **4_398_046_511_103
   MB** — the exact number in the message. It was meant to mean "only admit on an empty host," but
   it exceeds *any* physical RAM, so the gate (`share::admits`) refuses it **even on a totally empty
   host** → the model can NEVER load via the gate when its size is unknown.
2. Under `--offline` (which `agentic.sh` sets) the model also can't be downloaded to make its size
   knowable → permanent dead-end, reported as a fake petabyte overcommit.

**Fix.**
- `acquire_residency_or_exit`: for a single (non-cascade) model that is **not locally cached** AND
  `is_offline()`, exit early with a clear, actionable message naming the real problem and a
  copy-pasteable pre-download command (`hf_download_hint`) — instead of feeding the sentinel to the
  gate. Online is unchanged (the load path can still download).
- The gate refusal message now detects a sentinel-sized footprint (`>= u64::MAX/8`,
  `UNKNOWN_FOOTPRINT_FLOOR`) and prints "size is UNKNOWN (a model isn't downloaded locally)" with the
  pre-download hint, instead of quoting the absurd sentinel-in-MB — covering online-uncached and the
  cascade-with-an-uncached-tier paths too.
- Operational: the bench needs models pre-cached (it runs `--offline`); the `matrix-add-coders`
  smoke queue now pre-downloads via `uv + huggingface_hub` before starting the gateway.

**Related:** distinct from `footprint-before-download` (a261fb0, which moved the *estimate* after
download on the launch path) — this is the gateway path's unknown-size **sentinel** semantics +
the offline dead-end.

## BUG-004 — `mcp-proxy` dies mid-session → `mcp__rozum__*` tools vanish (no rozum-side trace)

- **Status:** fixed on `feature/mcp-proxy-resilience` (cargo check clean). Pending: install
  to `~/.cargo/bin/rozum` + a live away-session soak. The fix is the inverse correction to
  BUG-002: that one made the watchdog reap orphans, this one stops it reaping *live* sessions.
- **Reporter:** operator — mid-session the `mcp__rozum__*` tools disappeared (harness emitted
  "MCP servers have disconnected: rozum") while the meeting daemon stayed up and the CLI kept
  working. Correctly diagnosed by the operator: the **stdio `mcp-proxy`** bridging this Claude
  Code session to the daemon died; the daemon + CLI are an independent path. "MCP off" ≠ "rozum
  down".
- **Severity:** P2 — no data loss, but the agent silently loses room coordination for the rest
  of the session (Claude Code does **not** re-spawn a dead stdio MCP server; recovery today is a
  manual `/mcp` reconnect or a CC restart — neither doable by the agent itself).

**Symptom.** `rozum mcp-proxy` (the per-session stdio child Claude Code spawns from
`~/.claude.json`: `{type:stdio, command:rozum, args:[mcp-proxy]}`) exits during a live session.
The only trace was `eprintln!("proxy error")` → Claude Code's per-server MCP log, which records
nothing on a clean `exit(0)` and only an opaque transport-close otherwise → root cause
**uninspectable after the fact**.

**Root cause (two parts).**
1. **No observability.** The proxy had no log of its own, so an exit reason could not be
   recovered.
2. **The idle watchdog reaped live sessions.** BUG-002's fix reaps a proxy idle past
   `ROZUM_MCP_PROXY_IDLE_SECS` (default 2 h) with an unconditional `exit(0)`. Its safety
   assumption — "an actively room-using agent calls `meeting.wait_my_turn` ~every 25 s, so only
   an abandoned proxy goes silent that long" — is **false for an interactive human-driven CC
   session**: the human is coding/chatting, not running a room poll loop. Step away >2 h and the
   still-wanted proxy is reaped → tools vanish. (A `serve()`/transport error → `exit(1)` is the
   other, now-logged, candidate.)

**Fix** (`crates/rozum-meeting/src/meeting/daemon_proxy.rs`):
- **Observability:** `proxy_log()` writes lifecycle lines (start, initialize, daemon-connect,
  every exit + reason) to `$RUNTIME/mcp-proxy.log` (rotates at 256 KiB; `ROZUM_MCP_PROXY_LOG=0`
  to disable). `install_panic_logger()` records panics (payload + location) before the process
  dies. `run_daemon_proxy` now logs `serve-error` / `stdin-eof` / `join-error` distinctly.
- **Watchdog (the real fix):** past the soft window it reaps **only if the client transport is
  actually gone** (`Peer::is_transport_closed()` — flips when the rmcp loop tears down, i.e. CC
  disconnected). A live-but-idle session keeps its transport open → **not reaped**. A stuck
  orphan whose pipe never closed (the BUG-002 case) is bounded by a new generous hard cap
  `ROZUM_MCP_PROXY_MAX_IDLE_SECS` (default 24 h, `0` disables). `ROZUM_MCP_PROXY_IDLE_SECS=0`
  still disables the watchdog entirely. This keeps BUG-002's orphan-cleanup while closing the
  live-session false-reap.

**Strategic follow-up (HTTP transport).** The deeper fragility is structural: a *per-session
stdio child* is a single point of failure that Claude Code won't restart in-session. rmcp 1.7
ships a `streamable_http_server` transport and Claude Code supports `type:"http"` MCP servers —
the long-lived daemon could expose an HTTP MCP endpoint that CC connects to and **reconnects**
to on drop, with no per-session child to crash. Bigger lift (session identity / per-client cwd /
project detection move off the child); deserves its own spec. See the resilience analysis.

---

## BUG-003 — concurrent model-loaded gateways exhaust host RAM → watchdog kernel panic → reboot

- **Status:** fixed on master (`3bcee03` v1 single-flight) + **v2 RAM-ledger**
  (`feature/gateway-residency-ram-ledger`): the gate now admits a genuinely-fitting
  small 2nd model and refuses only a true overcommit. Pending matrix re-validation
  under load (`validate-gate-live`).
- **Reporter:** operator ("система ребутнулась") — the Mac rebooted 2026-06-22 13:41.
- **Severity:** P0 — reboots the host, so any matrix run is untrustworthy (same
  class as BUG-001, **different mechanism**).

**Symptom.** The Mac (Mac16,6 / M4, 36 GiB, macOS 26.5.1) rebooted. The panic is a
**watchdog timeout**, NOT the BUG-001 IOGPU double-free:
- `panic-full-2026-06-22-134243.panic`: `watchdog timeout: no checkins from
  watchdogd in 92 seconds`. Userspace `watchdogd` was starved → kernel watchdog
  panic → reboot.
- 3× `JetsamEvent-2026-06-22-13{30,35:08,35:18}.ips`, every kill reason =
  `vm-compressor-space-shortage`; dozens of system daemons mass-killed
  (assistantd, secd, trustd, MTLCompilerService…). `largestProcess = rozum`.
- At 13:35:18 there were **3 concurrent big rozum processes** — pid 23694 ≈24.8 GB,
  pid 25158 ≈18.7 GB, pid 25274 ≈18.0 GB → **≈61.6 GB resident on a 36 GiB box**,
  two distinct binary UUIDs (= >1 build/gateway running at once).

**Root cause.** More than one **model-loaded gateway** resident at once. The trigger
was two matrix runs overlapping — a `nondet-*` matrix (35B + GLM-4-32B) in the
`feature/matrix-nondeterminism-flip` worktree **and** an `agentic-35b-leanprompt`
run in the main worktree. Each `scripts/bench/agentic.sh` starts a **dedicated**
`rozum gateway --model … --port 8300+` (`agentic.sh:214`), which **bypasses** the
shared-gateway port singleton (`DEFAULT_GATEWAY_PORT` 8089 / `active.json`) — so the
registry never sees the second resident model and nothing stops the overcommit.
A single big model is contained (Metal OOM is process-fatal but local, BUG-001/[[35B
prefill OOM]]); the system-killer is **N concurrent instances**. The BUG-001
`TEARDOWN_GRACE` fix addresses GPU teardown, not this RAM-overcommit path.

**Fix.** A **host-wide model-residency admission gate** (`crates/rozum-core/src/share.rs`,
`acquire_residency`): every model-loaded gateway takes an advisory `flock` on
`gateway_dir()/residency.lock` **before** bringing weights resident and holds it for
its process lifetime (wired into `run_gateway` + `run_launch_dedicated` in
`src/main.rs`). It is independent of port/run/worktree, so it catches exactly the
dedicated-bench path the port singleton misses. A second loader waits up to
`ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` (default 240s, past the matrix teardown window)
then **refuses with a clear message naming the holder** — a reboot becomes a
recoverable error. `flock` is released by the OS on fd close / process death (incl.
SIGKILL), so there is no stale-lock failure mode. Escape hatch
`ROZUM_ALLOW_CONCURRENT_RESIDENT=1` for the rare two-small-models case. Unit tests:
`residency_gate_admits_one_and_releases_on_drop`, `residency_escape_hatch_skips_the_gate`.
Memory: `[[project-reboot-watchdog-oom]]`.

**v2 (RAM ledger).** The v1 hard mutex refuses even a tiny 2nd model. v2 replaces it
with a host RAM budget: each gateway *reserves* its estimated footprint (`residents/<pid>`
flock-held file) before loading; admit iff sole OR `in_use + footprint ≤ total_ram ×
ROZUM_GATEWAY_RAM_BUDGET_FRAC` (0.65) — a genuinely-small 2nd model co-resides, a true
overcommit (two big models) still refuses. Reservation up-front under a brief admit lock
⇒ no free-RAM-read TOCTOU; per-pid flock liveness ⇒ same death-safety as v1, no reaper.
Footprint estimated caller-side from the catalog (core stays model-free); unknown model ⇒
huge estimate ⇒ admitted only when host empty (conservative). 4 unit tests (sole / refuse-
overcommit + admit-fitting / reap-dead / hatch) + real-binary smoke. Spec § v2.

---

## BUG-002 — `mcp-proxy` processes pile up (orphaned when an agent re-spawns its MCP)

- **Status:** fixed + ON MASTER (cherry-picked `5be81a5`+`c742e2b` onto master `8eaf21a`) + INSTALLED to `~/.cargo/bin/rozum` (release, mlx-native+gguf). Verified: the installed binary self-reaps an idle proxy at the 60s tick (rc=0). Origin `91a03c7` on `feature/meeting-web-pwa-ssc`;
  cargo check/--tests clean + a functional idle test: a silent proxy now exits at the first 60 s
  tick, rc=0). The earlier `c0117bd` spawned the watchdog AFTER `serve()`, which blocks on the MCP
  handshake — so `91a03c7`/`c742e2b` moves it before.
- **Reporter:** operator ("почему у меня запущено три процесса розума?" → found ~6 stale
  `mcp-proxy`/`mpc-proxy`, some 4 days old).
- **Severity:** P3 — resource leak / clutter, not a correctness break.

**Symptom.** Several `rozum mcp-proxy` (and a typo'd-config `rozum mpc-proxy`) stdio bridges
linger for days, tiny RSS, doing no work. One live claude session held BOTH an old `mpc-proxy`
(4 d) and a working `mcp-proxy` (1.7 d) — a superseded duplicate that never exited.

**Root cause.** An MCP stdio proxy exits only on **stdin-EOF**. Parent *death* is fine (the
agent's pipe end closes → EOF → exit), which is why the lingering ones all had *live* parents.
The gap is parent **abandonment**: the agent stays alive but re-spawns a fresh MCP server on a
config reload / binary change and **never closes the old proxy's stdin**, so `service.waiting()`
blocks forever. (`mpc-proxy` is just a stale typo in an agent's MCP config — clap accepts the
abbreviation and runs `mcp-proxy` anyway; it marks the old entry, it is not the cause.)

**Fix.** Idle watchdog in `src/meeting/daemon_proxy.rs`: `forward_raw` stamps `last_active` on
every agent request; a 60 s-tick task exits the proxy once silent past
`ROZUM_MCP_PROXY_IDLE_SECS` (default 2 h, `0` disables). An actively room-using agent calls
`meeting.wait_my_turn` ~every 25 s, so only a genuinely-abandoned proxy goes silent that long.
Stale ones were also killed by hand at report time.

---

## BUG-001 — agentic matrix reboots the Mac (kernel panic on gateway teardown)

- **Status:** done (harness-side fix validated across inter-model teardowns on master, 2026-06-18)
- **Reporter:** found in-house (heavy-bench days); root-caused in
  `[[project-matrix-kernel-panic]]`.
- **Severity:** P0 — reboots the host, so any matrix run is untrustworthy.

**Symptom.** Running `scripts/bench/agentic.sh` over multiple models would reboot the Mac
(Mac16,6 / M4, macOS 26.5.1). Confirmed a **kernel panic**, not a RAM OOM / jetsam, from
`/Library/Logs/DiagnosticReports/*.panic`:
- `IOGPUGroupMemory::remove_memory_object() memory object not found @IOGPUGroupMemory.cpp:323`
  — the GPU driver double-frees / use-after-frees a Metal buffer the kernel already dropped.
- A later instance: `watchdog timeout: no checkins from watchdogd in 93s` with P-cores
  offline — the GPU/system fully wedged, then watchdog-killed.

**Repro (DESTRUCTIVE — do not run to reproduce).** The full matrix reboots the machine.
This was localized by **code-path analysis**, not by re-running, precisely because the repro
is a host reboot.

**Root cause.** The harness tore each model's shared gateway down with
`kill -INT` → wait 60 s → **unconditional `kill -KILL`** (`agentic.sh`). rozum has graceful
shutdown and `join()`s the MLX worker on Drop, but if the final Metal eval is **wedged under
memory pressure** the worker thread is stuck inside a GPU dispatch, Drop's `join()` blocks,
the 60 s grace expires, and `kill -KILL` lands **on top of live GPU command buffers** →
IOGPU accounting corruption → kernel panic. `ROZUM_MLX_RETAIN` (retained command buffers for
the hybrid-decode fast path) widens the window.

**Fix (harness-side, validated no-panic on 27B on the original
`feature/matrix-teardown-panic-fix` branch; that branch went 70 commits stale, so the
still-needed change was ported fresh onto current master rather than merged):**
`scripts/bench/agentic.sh` now tears the gateway down **gracefully**: `kill -INT` →
wait `TEARDOWN_GRACE` (180 s, env-overridable) for a clean exit → SIGKILL **only as a loudly
flagged last resort** → then a `GPU_SETTLE` (8 s) pause to let the kernel finish async IOGPU
reclamation before the next gateway allocates on the same Metal device. Also adds
`ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0` to the gateway launch so the shared gateway isn't
self-exited (`clients_gone`) between the claude/codex phases — see
`[[project-agentic-bench-clients-gone]]` (a different matrix bug, co-fixed here as it makes
the run reach a clean teardown at all).

- **Fix commit:** `326bb9d` (`scripts/bench/agentic.sh` graceful teardown + idle-secs).
- **Validation 2026-06-18 — DONE.** Two matrix runs on master with the fix, neither produced a
  new `.panic` file (baseline stayed at 1):
  1. Single-model (`Qwen3.6-35B-A3B-4bit` × claude+codex+opencode × 5 tasks):
     **15/15 PASS, rc=0, 0 timeouts** (`results/agentic-20260618-081632`) — validated the
     end-of-model graceful teardown + the `clients_gone` idle-secs fix.
  2. **Inter-model (the original panic point):** claude × `Qwen3.6-27B → Qwen3-30B-A3B →
     Qwen3.6-35B-A3B`, `ROZUM_MLX_CACHE_GB=1` — **15/15 PASS, rc=0, 0 timeouts, NO new panic
     across 2 inter-model teardown transitions**, and **no SIGKILL fired** (every gateway exited
     gracefully within `TEARDOWN_GRACE`; footprint flushed cleanly between models 17.8→19.6→21.1 GB)
     (`results/agentic-20260618-083911`). This is the transition where the kernel panic originally
     occurred — now clean.
- **Remaining (separate hardening item, NOT this bug):** the deeper rozum-side bounded/non-wedging
  teardown (a real Metal-eval timeout so Drop's `join()` can't block forever). Defense-in-depth so
  even a buggy harness can't SIGKILL into a live eval; can't be validated without risking a reboot,
  so left as a tracked follow-up.

**Open follow-up (defense-in-depth, NOT done — deliberately).** The deepest fix is rozum
*itself* guaranteeing a bounded, non-wedging teardown (a real Metal-eval timeout that returns
control; ensure Drop's `join()` can't block forever). That touches the GPU teardown hot path
(`mlx_native_backend.rs` Drop/join, `gateway.rs` `shutdown_signal`) and **cannot be validated
without risking a reboot**, so it is left as a tracked follow-up rather than shipped blind.
The harness fix removes the proven panic trigger (SIGKILL into a live eval); this hardens the
engine so even a buggy/aggressive harness can't panic the GPU.
</content>
