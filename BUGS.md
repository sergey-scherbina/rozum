# Bugs

One entry per bug, newest first. Status flow: `open → needs-info → fixed → done`.
See `vendor/agent-plugins/bugs/commands/bugs.md`.

---

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
