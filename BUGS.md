# Bugs

One entry per bug, newest first. Status flow: `open → needs-info → fixed → done`.
See `vendor/agent-plugins/bugs/commands/bugs.md`.

---

## BUG-002 — `mcp-proxy` processes pile up (orphaned when an agent re-spawns its MCP)

- **Status:** fixed (`91a03c7` on `feature/meeting-web-pwa-ssc`, worktree `../rozum-meeting-pwa`;
  cargo check/--tests clean + a functional idle test: a silent proxy now exits at the first 60 s
  tick, rc=0). The earlier `c0117bd` spawned the watchdog AFTER `serve()`, which blocks on the MCP
  handshake — so `91a03c7` moves it before. Not yet on master.
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
