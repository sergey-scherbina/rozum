# Spec: host-wide model-residency single-flight gate (reboot fix, BUG-003)

Status: in progress (2026-06-22). Owners — code: `sunny-civet`
(`feature/gateway-residency-guard`); spec + board + verification: `nimble-raven`
(`feature/reboot-singleflight-spec`). Coordinated in the rozum room (n=25–29).

## Problem — a concurrent second model-load reboots the machine

On 2026-06-22 13:41 the 36 GiB Mac (Mac16,6) **rebooted via a kernel watchdog
panic**, not a crash of any app. Evidence (in `/Library/Logs/DiagnosticReports/`,
full writeup in memory `project-reboot-watchdog-oom`):

- `panic-full-2026-06-22-134243…panic`: `watchdog timeout: no checkins from
  watchdogd in 92 seconds`. Userspace `watchdogd` was starved → kernel panicked.
- 3× `JetsamEvent-2026-06-22-13{30,35:08,35:18}.ips`: every kill reason =
  `vm-compressor-space-shortage`; dozens of system daemons killed; `largestProcess
  = rozum` in all three.
- At 13:35:18 **three concurrent model-loaded `rozum` processes**: ≈24.8 + 18.7 +
  18.0 GB = **≈61.6 GB resident on a 36 GiB box**. Two distinct binary UUIDs ⇒ two
  different worktree builds running at once (two matrix runs:
  `feature/matrix-nondeterminism-flip` + the main checkout).

Chain: N concurrent big models → compressor exhaustion → jetsam cascade →
`watchdogd` can't check in for 92 s → kernel watchdog panic → reboot.

**This is a different mechanism from `project-matrix-kernel-panic`** (that was an
IOGPU `remove_memory_object()` double-free on GPU teardown, fixed by
`TEARDOWN_GRACE`). The `TEARDOWN_GRACE` fix does **not** address RAM overcommit.

### Why it isn't already prevented

- The shared-gateway **port singleton** (`DEFAULT_GATEWAY_PORT`, `share.rs`) only
  dedupes gateways that go through the rendezvous on the *same port*. A dedicated
  `rozum gateway --port N` (what the matrix bench starts) or
  `rozum launch --dedicated` (ephemeral port) bypasses it entirely.
- The per-process MLX cap (`cap_mlx_memory`, `crates/rozum-mlx/src/mlx_native_backend.rs`)
  sets `set_memory_limit(total − 8 GB)` ⇒ **~28 GB per process on a 36 GiB Mac**,
  *unaware of sibling rozum processes*. Three such processes each "allowed" 28 GB =
  guaranteed system OOM. This is the amplifier.
- No existing check is **machine-global**: the mistralrs `memory_preflight_ok`
  (`src/main.rs`, `cfg(feature="mistralrs")` only) reads instantaneous free RAM
  with no reservation (racy, and not on the default MLX/GGUF path); warm-residency
  admission (`Switchboard::plan_residency`) is scoped to one daemon's own residents.

## Design — one resident model per host, enforced by an advisory file lock

A host-wide single-flight gate: **every gateway acquires a `flock(2)` advisory lock
BEFORE bringing model weights resident, and holds it for its process lifetime.** A
second loader waits a bounded window, then refuses with a clear message — turning a
host reboot into a recoverable error.

Why `flock`, not a RAM ledger (the alternative considered and rejected for v1):

- The OS releases `flock` when the holder's fd closes — **including process death /
  SIGKILL / panic**. No stale-lock state, no reaper, no cleanup to get wrong. A
  RAM-accounting ledger needs PID liveness reaping and is racy (two loaders both
  read "free RAM", both pass, both load). The lock is strictly serialized.
- "One resident model at a time" is the correct invariant for this box: even two
  mid-size models can co-exceed 36 GiB once each MLX cap claims ~28 GB. A precise
  RAM ledger is a possible v2 refinement (admit a genuinely-fitting second small
  model), gated behind the same escape hatch.

### Implementation (`crates/rozum-core/src/share.rs`) — DONE by sunny-civet

- `residency_lock_path()` → `gateway_dir()/residency.lock` (host-wide, port/run/
  worktree-independent: every gateway on the box shares one gate).
- `acquire_residency() -> Result<Option<ResidencyGuard>, ResidencyDenied>`:
  - `Ok(None)` — gate bypassed (escape hatch) or lockfile IO/no-advisory-locks ⇒
    **fail open** (the gate is a safety net, never a correctness requirement).
  - `Ok(Some(guard))` — acquired; **caller holds `guard` for the model's lifetime**.
  - `Err(ResidencyDenied{holder, waited_secs})` — another gateway held it past the
    wait window; `holder` (from the registry, best-effort) names the blocker.
  - Blocking (`try_lock` every 2 s). Call on a blocking thread (`spawn_blocking`)
    from async.
- `ResidencyGuard` — wraps the open `File`; dropping it (or process death) releases.
- Knobs:
  - `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS` (default **240**) — generously past the
    matrix teardown window (`TEARDOWN_GRACE` 180 s + `GPU_SETTLE`) so a back-to-back
    bench model-swap (old gateway exiting as the new one starts) never falsely
    refuses. `0` = refuse immediately.
  - `ROZUM_ALLOW_CONCURRENT_RESIDENT=1` — operator escape hatch; skip the gate.

### Wiring — the part that makes it actually fire (in progress)

The helper exists in `share.rs`, but the gate only works if **every model-load
entry calls it and holds the guard for the model's life**. Load sites (verified by
the codebase map; line numbers approximate, re-grep before editing):

1. `run_gateway` — `src/main.rs:~901`, before `build_from_config`/cascade/spec-decode.
   Covers the manual `rozum gateway` daemon **and** the launch-spawned shared daemon
   **and** the matrix bench's `rozum gateway --port N`. The guard must live as long
   as the served model (store it in the serving scope, drop on shutdown/unload).
2. `run_launch_dedicated` — `src/main.rs:~1604`, before `build_gateway_backend`.
   This path currently has **no preflight at all**; it is the most important
   addition.
3. (Backstop, optional v1) `cap_mlx_memory` — `crates/rozum-mlx/src/mlx_native_backend.rs:~363`:
   make the per-process cap sibling-aware so even a forced/escaped second MLX
   process can't claim near-total RAM. Secondary to the lock.

Interaction with idle-unload: when a daemon idle-*unloads* the model (frees weights,
keeps the process for lazy reload, `gateway.rs`), it must **release the residency
guard** so another gateway can load, and **re-acquire** on lazy reload. (If
re-acquire isn't wired, simplest v1 is: hold the guard for process lifetime and rely
on `clients_gone`/idle *exit* to release — confirm which, and document it.)

## Acceptance / done-when

- Two concurrent `rozum gateway` (or `rozum launch`) model-loads on the same box:
  the **second waits up to the window then refuses with a clear message** naming the
  holder — it never brings a second model resident. Verified by starting one real
  gateway, then a second, and observing the refusal (no second RSS spike).
- A **single** gateway / launch / matrix run is unaffected (no false refusal;
  back-to-back model swap within the wait window succeeds once the first frees).
- `ROZUM_ALLOW_CONCURRENT_RESIDENT=1` restores the old concurrent behavior (escape).
- The matrix bench cannot start a second model-loaded gateway while one is live —
  i.e. the 2026-06-22 reboot scenario is structurally impossible by default.
- Unit test for the gate (acquire → second `acquire` in the same process / a child
  blocks→denies with `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS=0`); `cargo check` default
  **and** `--no-default-features` green; gateway/core test suites pass.

## Verification plan (nimble-raven)

1. **Static**: grep that `acquire_residency()` is called at all three load sites and
   the guard is held for the model's lifetime (not dropped immediately). Confirm the
   matrix path (`scripts/bench/agentic.sh` → `rozum gateway --port N`) goes through
   `run_gateway`.
2. **Behavioral (single model only — never run two big models to "test" the OOM)**:
   `ROZUM_GATEWAY_RESIDENCY_WAIT_SECS=0`; start gateway A (tiny model); attempt
   gateway B → expect immediate clear refusal, B never loads. Then stop A → B
   succeeds. This proves the gate without ever risking a second reboot.
3. Confirm fail-open: point `gateway_dir()` at a read-only path → load still proceeds
   (gate logs a warning, doesn't block).

## Operational rule (until the gate is on master)

On this Mac: **never hold >1 model-loaded gateway at once.** Don't start a
matrix/launch if another is already serving a model. (Room-broadcast n=25/26.)
