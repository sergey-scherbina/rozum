# Shared gateway: one model process, many launch clients

## Overview

Today every `rozum launch` loads the model **in-process** and runs the gateway
as a task inside the launch process (`run_launch`): two launches = two resident
model copies = OOM on a 24–36 GB Mac. This feature makes the model-serving
gateway a **shared, single-instance, detached process** that multiple `rozum
launch` clients discover and reuse, with single-owner election, transparent
failover, and idle shutdown. It also makes `--model` optional: with nothing
running, launch shows an interactive model picker; with a gateway already up, it
just reuses it. Adds `rozum models rm` to delete a cached model.

The single-machine consensus is deliberately simple: the OS gives us the two
primitives we need — **one process can bind a TCP port** (the singleton
guarantee) and **the OS releases the port + an advisory lock when that process
dies** (the failover trigger). No distributed consensus.

This composes with `concurrency-backend-abstraction`: sharing solves *memory*
(one resident model); the `AdmittingBackend` already in front of the gateway
solves *concurrency* (many launch clients hitting one gateway are gated by the
admission scheduler / fast lane / backpressure).

## Interface

### CLI

```
rozum launch [--model X] [--dedicated] [--port P] [--n-ctx N] <program> [args…]
    # --model now OPTIONAL:
    #   given        → ensure a shared gateway for X (reuse / spawn / takeover)
    #   omitted + a gateway already running → reuse it (print which model)
    #   omitted + nothing running → interactive model picker (TTY only)
    # --dedicated → bypass sharing; run a private in-process gateway (old behaviour)

rozum gateway --model X [--port P] [--n-ctx N] [--no-idle-timeout]
    # The shared daemon. Registers itself, accepts client leases, and idle-exits.
    # Manually-run gateways are discoverable & shareable by launches too.

rozum gateway status        # show the active shared gateway: model, pid, port, clients, uptime
rozum gateway stop          # ask the active shared gateway to exit (refused if clients attached, unless --force)
rozum gateway switch --model Y [--backend B] [--n-ctx N]
                            # transparently swap the resident model (and/or backend) in place:
                            # drain → unload → load Y → resume. Clients' requests are held by
                            # their proxy across the gap, not failed.
rozum gateway reload        # graceful restart of the daemon (e.g. after upgrading the rozum binary)
rozum gateway unload        # free the model but keep the daemon (lazy-reload on next request) — frees RAM

rozum models rm <spec>      # delete a cached model (confirm; refuse if it is the active model)
rozum models list [--remote]   # (existing) data source for the picker
```

### Rendezvous state (under `$XDG_STATE_HOME/rozum/gateway/`)

```
active.json        # { model, backend, port, pid, n_ctx, started_at, generation } — the registry
spawn.lock         # advisory flock, held only during a spawn attempt (anti-stampede)
leases/<pid>       # one file per live launch client; mtime = heartbeat
poison.json        # advisory set of request fingerprints that crashed the daemon (TTL'd)
```

`generation` increments on every (re)spawn / switch, so a proxy can tell "the
daemon I was talking to was replaced" from "same daemon, transient blip".

Stable port: the shared gateway uses a fixed port (default 8089, `--port`/`ROZUM_GATEWAY_PORT` to override) recorded in `active.json`, so a respawn after a crash reuses the **same** port and already-connected agents reconnect transparently.

## Behavior

### Discovery & reuse

- [x] `rozum launch --model X`: if `active.json` exists, the port answers
      `GET /v1/models`, and the model is compatible → **reuse**: point the agent's
      `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` at the running port and exec the
      agent. No new model is loaded. (`ensure_shared_gateway` in `main.rs`.)
- [x] A stale registry (port not answering) is treated as "none running" — the
      HTTP health probe is the authoritative liveness signal.

### Single-owner election & spawn

- [x] When no usable gateway is found the launch spawns a detached `rozum gateway
      --model X --port P` and waits for health. Concurrent (re)spawns are damped by
      `share::try_spawn_lock` (O_EXCL anti-stampede with stale-steal); the TCP bind
      below is the hard correctness guarantee.
- [x] Concurrent spawners are deduplicated by the TCP bind: exactly one `rozum
      gateway` binds the port; the rest fail `EADDRINUSE` and exit, then all
      launches discover the survivor via the health poll.
- [x] The detached gateway (own process group, stdio → `gateway.log`) outlives
      the launching client (it does not die when the agent exits).

### Failover

- [x] While the agent runs, each launch runs a background watchdog that polls the
      daemon; on death it respawns on the **same** port. A `share::try_spawn_lock`
      (O_EXCL, stale-steal) keeps simultaneous watchdogs from each respawning;
      the TCP bind dedups any that slip through. (`spawn_failover_watchdog`.)
- [x] Because the port is stable, the respawned gateway is transparent to the
      already-connected agent after a brief reconnect window (the agent's own
      retry reconnects to the same URL).

### Lifetime (idle shutdown)

- [x] Each launch holds a lease (`leases/<pid>`, heartbeated every 15 s; mtime =
      liveness). The daemon stays up while any lease is fresh (`LEASE_FRESH_SECS`
      60), a request is in flight, OR there was HTTP within `ROZUM_GATEWAY_IDLE_SECS`
      (default 900) — and idle-exits only when all are quiet, freeing
      the port and the model. `ROZUM_GATEWAY_IDLE_SECS=0` keeps it up indefinitely.
- [x] `rozum gateway stop` SIGTERMs the daemon (refused while clients are attached
      unless `--force`); `rozum gateway status` prints model/pid/port/n_ctx/uptime/clients.

### Model resolution when `--model` is omitted

- [x] Omitted **and** a gateway is already running → use it; print
      `using running model: <model>`. (`resolve_launch_model`.)
- [x] Omitted **and** nothing running, on a TTY → show an interactive picker
      (below). Non-TTY (piped/CI) → error: "no --model given and no gateway
      running; pass --model".
- [x] The picker lists models we can actually run: **cached models first**, each
      annotated `(cached, <size>)`; then downloadable models annotated
      `(not cached, ~<size>)`. Data from `models::scan_all_installed()` (cached)
      + the curated remote list (`models::RECOMMENDED`). (`pick_model_interactive`.)
- [x] Selecting a **not-cached** model re-confirms: "Download <spec> now and use
      it? [y/N]"; on yes, that model becomes the gateway's model (downloaded on
      first load via hf-hub).

### Model mismatch (requested ≠ running)

- [x] `--model Y` while a gateway serves `X`: **takeover-if-idle** — when no other
      launch holds a lease (`live_lease_count == 0`), the running daemon is
      SIGTERM'd and a fresh one for `Y` is spawned on the same port; otherwise
      **reuse `X` with a warning** (and point the agent's `ANTHROPIC_MODEL` at
      `X`) so a live client's session isn't stolen. (`ensure_shared_gateway`.)
- [x] `--dedicated` always bypasses sharing and runs a private in-process gateway
      on its own port (the pre-feature behaviour), regardless of what is running.

### Cache deletion

- [x] `rozum models rm <spec>` resolves the spec to a cached model (exact match
      on the spec shown by `models list`), prints what will be freed, and
      **confirms** before deleting. Refuses (no delete) if that model is the
      **active** gateway model. (`run_models_rm`.)
- [x] HuggingFace (`~/.cache/huggingface/hub/models--…`) and LMStudio (the
      per-model repo dir containing the `.gguf`) are removed directly. Ollama is
      delegated to `ollama rm <tag>` if the binary exists, else refused with a
      note (its blobs are content-addressed and shared — not safe to `rm`).
- [x] Prints the reclaimed size on success. (`--yes` skips the prompt for scripts;
      a non-TTY without `--yes` is refused.)

### Client transparency on daemon loss (replay, poison, retry)

The agent talks to a **launch-local model-free proxy** (mirrors `mcp-proxy` for
rooms); the proxy forwards to the shared daemon and owns the resilience policy.
This is what lets clients "not notice" a daemon crash/restart/swap.

- [x] `rozum launch` runs a tiny reverse proxy on a per-launch local port and
      points the agent's `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` at it; the proxy
      forwards to the shared daemon's stable port. (No model in the proxy.)
      (`src/proxy.rs`; `start_launch_proxy` in `main.rs`.)
- [x] **Replay before first token:** if the daemon connection fails *before any
      response byte has been forwarded to the agent*, the proxy waits for the
      daemon to come back (re-election respawns it) and re-sends the buffered
      request — transparently. The agent sees a slower response, not an error.
      (`forward` retry loop + `wait_for_health` in `src/proxy.rs`.)
- [x] **Mid-stream is not replayable:** once tokens have been forwarded, a daemon
      death surfaces an error to the agent (we can't un-send tokens); the agent
      decides whether to retry the whole turn. Documented, not hidden. (The replay
      boundary is returning the `Response`: status+headers commit the stream.)
- [x] **Poison-prompt protection (soft, graduated):** the proxy fingerprints each
      request and counts crash-attributed attempts. The escalation is gentle, not
      a hair-trigger ban:
      1. **Degrade-then-retry first.** A first crash-attributed retry goes out
         under the most conservative settings (serialized: admission limit 1, so
         no neighbour competes for memory). Many "poison" prompts are just
         big-prompt OOMs that succeed once nothing else is resident.
      2. **Refuse only after `ROZUM_POISON_MAX` (default 3) crash-attributed
         attempts**, with a clear, *soft* 422 ("this request keeps crashing the
         model; refused for now — retry later"). It is per-fingerprint, not a
         blanket block.
      (`forward` in `src/proxy.rs`: crash-attribution = an *established-connection*
      send failure (`!e.is_connect()`, so a failover gap is not blamed on the
      prompt); the retry after a crash takes the exclusive `lane` write-lock to
      serialize the risky prefill; `poison`/`poison_max` count + refuse via
      `poisoned()`.)
- [x] **Shared persistence only on high-confidence attribution.** A fingerprint is
      written to the shared, TTL'd `poison.json` only when the daemon died with
      this request as the **sole in-flight** one (unambiguous cause). Ambiguous
      cases (concurrent in-flight requests) are counted **locally at the proxy
      only** — never shared — so a coincidental crash can't ban a good prompt for
      everyone. A restarted daemon loads `poison.json` and fast-refuses confirmed
      entries; they are advisory and expire (`ROZUM_POISON_TTL_SECS`, default
      3600, shortened from the earlier 24 h) and decay on the next clean success.
      (`share::{fingerprint,is_poisoned,record_poison,clear_poison}` over a raw-body
      hash; sole-in-flight = the proxy's `admit.stats().in_use <= 1`; the daemon's
      `gateway::poison_layer` fast-refuses before running the model; decay on a 2xx
      prefill, both locally and machine-wide.)
- [x] **Smart retry policy:** retries use exponential backoff + jitter, a per-
      request attempt cap, wait-for-health (don't fire at a warming daemon), and
      honor the daemon's backpressure (`429` + `Retry-After` → hold and retry) —
      so a crowd of reconnecting proxies doesn't stampede the fresh daemon.
      (`RetryPolicy` in `src/proxy.rs`: `ROZUM_PROXY_MAX_ATTEMPTS`/`_BACKOFF_MS`/
      `_HEALTH_WAIT_SECS`.)

### Two-tier admission: daemon backpressure → proxy queue

The daemon owns the **global** admission limit; the proxy holds its client's
requests in a **local priority queue** and only forwards what the daemon has room
for — so prompts wait at the edge instead of bouncing off a full daemon.

- [x] The daemon advertises its admission state to proxies — current free slots /
      queue depth — via a cheap `GET /v1/admit` probe (`admit_handler` in
      `gateway.rs`, fed by `ChatBackend::admission_stats()` →
      `AdmittingBackend`); `429` + `Retry-After` remains the in-band response
      signal. The proxy uses `/v1/admit` as a **forwarding window**
      (`wait_for_window`): it holds a queued request until the daemon signals room
      and always backs off on `429`/`Retry-After`. Fail-open if the probe can't be
      read (older daemon / auth), so the 429 backstop still applies.
- [x] Each proxy runs its **own** `concurrency::AdmissionScheduler` over the
      requests from its single agent (which can fire parallel tool/sub-agent
      calls): shortest-job-first by `RequestCost` (estimated from body size), with
      the reserved fast lane — so a small request *may* jump ahead of a big one
      queued at the proxy. Reuses the same module (`proxy_admit_config`,
      `ROZUM_PROXY_ADMIT`/`_FASTLANE_TOKENS`, unbounded queue — a proxy never sheds
      its own client). The guard is held for the whole stream.
- [x] Net effect: two cooperating tiers — proxy-local ordering (per client) +
      daemon-central limit (global) — keep the daemon at its budgeted concurrency
      without premature sends, and keep each client's quick turns responsive.

### Transparent model / backend switch

- [ ] `rozum gateway switch --model Y [--backend B]` drains the daemon (stop
      admitting new requests — queue/hold via the admission limit), finishes
      in-flight requests, unloads the current model, loads `Y` (and/or backend
      `B`), bumps `generation`, and resumes. **In-place and sequential** — never
      two models resident (memory). Clients' proxies hold their requests across
      the gap and replay if needed, so the swap is transparent (just slower).
- [ ] `rozum gateway reload` does the same drain + respawn from the current
      `rozum` binary (transparent daemon/binary upgrade).
- [ ] `rozum gateway unload` frees the model but keeps the daemon; the next
      request lazy-reloads it. Frees RAM without losing the rendezvous.

## Out of scope

- **Multiple resident models** ("multi-slot if memory allows"). One resident model
  at a time; tracked as `shared-gateway-multislot` in BACKLOG (would gate a second
  model on `ConcurrencyBudget` saying both fit).
- Cross-machine / networked sharing (this is loopback, single host).
- Hot-swapping the model of a running gateway mid-session (takeover only happens
  when idle).
- A long-lived system daemon / launchd service (the gateway is lazily spawned and
  idle-exits; install-as-service is future work).

## Design

### The daemon is `rozum gateway`

`rozum launch` stops embedding the gateway. Instead it **ensures** a shared
`rozum gateway --model X --port P` exists (spawned detached via the current exe
+ `setsid`/`CREATE_NEW_PROCESS_GROUP`), then execs the agent pointed at it. The
gateway gains: write `active.json` on bind, accept leases, idle-exit. A user
running `rozum gateway` by hand is therefore a first-class shareable target.

### Election protocol

```
ensure_shared_gateway(model, port):
  if registry_healthy_and_compatible(model): return registry.port      # reuse
  lock = try_flock("spawn.lock")                                        # non-blocking
  if lock.acquired:
     spawn_detached("rozum gateway --model {model} --port {port}")
     wait_until_healthy(port)            # gateway binds the port (the real singleton)
     # gateway wrote active.json itself
     return port
  else:
     backoff_poll_until_healthy(port)    # someone else is spawning; just wait
     return port
```

`spawn.lock` only suppresses redundant spawns; the **TCP bind is the ground
truth** for "one gateway". OS release of the port (and the lock) on process death
is the failover signal — no liveness protocol to maintain.

### Why a stable port

The launch-local proxy reconnects to the daemon's **stable** port after a crash;
a random respawn port would dangle. Single resident model ⇒ one port suffices;
multi-slot (BACKLOG) would key the port by model. (The agent itself points at the
proxy's local port, which is stable for the launch's whole lifetime regardless of
daemon churn.)

### Leases vs idle-by-traffic

Lifetime is keyed to **client liveness**, not HTTP traffic: an agent can sit idle
while the user reads, and killing the model then would be wrong. A lease is a
file named by the launch client's pid, heartbeated; the gateway reaps dead pids
(`kill(pid,0)`) and idle-exits when none remain. This reuses the heartbeat idiom
already in `multi-agent` claims and the `mcp-proxy`.

### Picker

Reuses `models::scan_all_installed()` for the cached set and the curated remote
list for downloadables; renders cached-first with right-aligned `(cached, size)`
/ `(not cached, ~size)` annotations; a not-cached pick gets a yes/no download
confirm. TTY-gated; non-interactive callers must pass `--model`.

### Cache rm

Resolves `<spec>` to an `InstalledModel` (reusing the scanners), guards against
the active model (reads `active.json`), confirms, deletes the self-contained dir
(HF/LMStudio) or delegates to `ollama rm`, and reports freed bytes via
`models::format_size`.

### Launch-local proxy (the transparency enabler)

Each `rozum launch` runs a tiny **model-free** reverse proxy (the gateway analog
of `mcp-proxy` for rooms) and points the agent at it. The proxy forwards to the
shared daemon and is where replay, poison handling, retry/backoff, and "hold the
request across a swap" live — none of which can be done if the agent talks to the
daemon directly (we'd be at the mercy of the agent's own retry behaviour). Cost:
one extra loopback hop and a small per-launch process; no extra model memory.

### Replay & poison protection

The proxy buffers each request body and tracks how many bytes it has forwarded to
the agent. On a daemon-side failure:
- **0 bytes forwarded** → safe to replay: wait for re-election to bring the daemon
  back (same port, possibly higher `generation`), re-send, transparent.
- **>0 bytes forwarded** → not replayable (tokens already streamed); surface the
  error.

Poison handling is deliberately **soft and graduated** (a crash-attributed prompt
is more often a transient big-prompt OOM than a truly malformed input):
1. **Degrade-then-retry**: the first crash-attributed retry runs serialized
   (admission limit 1) so no neighbour competes for memory — this alone clears
   most cases.
2. **Refuse only after `ROZUM_POISON_MAX` (default 3)** crash-attributed attempts,
   with a soft, retryable 422 — per-fingerprint, never a blanket block.
3. **Share only on high confidence**: write to `poison.json` (machine-wide,
   TTL'd) only when the daemon died with this request as the **sole in-flight**
   one. Ambiguous cases (concurrent in-flight) stay local to the proxy, so a
   coincidental crash can't ban a good prompt for everyone. Entries are advisory,
   short-TTL (default 1 h), and decay on the next clean success. A restarted
   daemon loads them to fast-refuse before running the model — protection that
   survives the very crash it guards against, without being a hair-trigger.

Fingerprint = hash of normalized messages + sampling params.

### Retry & two-tier admission

Reconnect/replay uses capped exponential backoff + jitter (the `mcp-proxy`
idiom), waits for daemon health before firing, and caps attempts per request.

Backpressure is **two-tier and daemon-driven**:
- The **daemon** is the global authority on concurrency (its
  `concurrency::AdmissionScheduler`). It exposes free-slots / queue-depth /
  `Retry-After` (response headers + a cheap `GET /v1/admit`).
- Each **proxy** keeps its client's requests in its **own**
  `concurrency::AdmissionScheduler` (SJF + fast lane) and only forwards within the
  window the daemon advertises — prompts wait *at the proxy*, ordered so a small
  request can overtake a big one, rather than being fired early and bounced. On
  `429`/`Retry-After` the proxy shrinks its window and parks.

Reusing the same `concurrency` module at both tiers keeps one implementation of
SJF/fast-lane/limits; the proxy tier is just a second instance with a small window.

### Transparent switch / reload / unload

A model or backend change is **in-place and sequential** because two models can't
be resident at once (memory): drain (admission limit → 0, finish in-flight),
`unload`, `load Y`/rebuild backend, bump `generation`, resume. The proxies hold
their pending requests across the gap (bounded by a timeout) and replay, so the
swap is transparent — clients just see a slower turn. `reload` is the same drain
+ respawn-from-current-binary (transparent binary upgrade); `unload` drops the
model and lazy-reloads on the next request. Blue/green (spawn the new model
alongside, flip, kill old) is rejected — it needs both models resident.

### Composition

The shared gateway keeps wrapping its backend in `concurrency::admit_wrap`, so
N launch clients on one gateway are admission-controlled exactly as designed —
and the admission **limit → 0** drain is precisely how `switch`/`reload` quiesce
the daemon. This feature is what finally makes that multi-client path real.

## Decisions

- **Detached `rozum gateway` daemon, not an in-process owner** — decouples model
  lifetime from any single agent; ownership never needs to "transfer" (a live
  model can't move between processes). Rejected: first-launch-owns-it (its exit
  would kill the model out from under other clients).
- **Port bind as the singleton + flock as anti-stampede** — both are simple OS
  primitives with automatic release on death; together they give "exactly one,
  failover, no herd" without a consensus library. Rejected: a hand-rolled
  lease/heartbeat election (more moving parts, more failure modes).
- **Stable port** — required for transparent failover of an already-launched
  agent. Rejected: random port per gateway (dangling `ANTHROPIC_BASE_URL`).
- **Leases over idle-by-traffic** — correct lifetime for interactive sessions
  with long think time. Rejected: HTTP-inactivity timeout (kills mid-session).
- **Mismatch = takeover-if-idle else reuse-with-warning, `--dedicated` escape** —
  the common case is "same model in two terminals" (share); a genuine second
  model can't fit, so we don't silently OOM. Rejected: always refuse (annoying);
  always spawn second (OOM).
- **`models rm` delegates Ollama to `ollama rm`** — Ollama blobs are shared and
  content-addressed; direct `rm` could corrupt other models.
- **Launch-local proxy in the request path** — chosen because transparent replay,
  poison handling, retry policy, and "hold the request across a model swap" are
  only possible if rozum controls the path; relying on the agent's own retry
  can't do replay-control or poison breaking, and would fail a turn during a
  swap. Mirrors the existing `mcp-proxy`. Rejected: agent → daemon directly
  (loses all of the above; resilience reduced to the agent's behaviour).
- **Replay only before the first streamed token** — once tokens are sent we can't
  un-send them, so mid-stream failures are surfaced, not silently retried.
- **Soft, graduated poison policy** — degrade-then-retry (serialize) first, refuse
  only after a higher threshold (3), share machine-wide only on sole-in-flight
  high confidence, short TTL + decay-on-success. A big-prompt OOM usually clears
  once serialized, so we don't ban legitimate prompts; the shared set is reserved
  for genuinely reproducible, unambiguous crashers. Rejected: the earlier
  hair-trigger (threshold 2, 24 h machine-wide on any in-flight) — too eager.
- **Two-tier, daemon-driven backpressure** — the daemon is the single global
  authority on concurrency and advertises room; proxies hold their client's
  requests in a local SJF/fast-lane queue and only forward within that window, so
  prompts wait at the edge (ordered, small-can-overtake) instead of being fired
  early and bounced. Reuses the one `concurrency` module at both tiers. Rejected:
  proxies fire freely and rely only on the daemon's 429 (wasteful round-trips,
  stampedes, no edge ordering).
- **In-place sequential swap, not blue/green** — two models don't fit in memory,
  so a swap accepts a short drain+reload gap (proxies hold requests) rather than
  doubling RAM.

## Risks / sharp edges

- **Failover window**: a brief period where the port is down during respawn; the
  agent must retry. Acceptable; documented.
- **Detached-process portability**: detaching differs on Unix (`setsid`) vs
  Windows; the target is macOS, but keep the spawn helper isolated.
- **Lease leak**: a hard-killed client leaves a stale lease; mitigated by pid
  liveness checks (not just mtime) when reaping.
- **Picker only on TTY**: scripted launches must pass `--model`; clearly errored.
- **Two gateways for two models** is intentionally *not* allowed (memory); users
  who really want it use `--dedicated` and own the consequences.
- **Poison false positives**: a legitimate prompt present during an unrelated
  crash could be refused until its TTL expires. Conservative attribution + a
  clear 422 message ("looks like it crashed the model; refused — retry later")
  keep it tolerable; tune `ROZUM_POISON_MAX`/`_TTL_SECS`.
- **Swap gap latency**: `switch`/`reload` parks requests for the drain+reload
  duration; a proxy hold-timeout bounds it (then it surfaces an error rather than
  hanging the agent forever).
- **Crash storm**: many proxies replaying after a crash — mitigated by backoff +
  jitter + wait-for-health + 429/Retry-After, but a pathological poison prompt
  hitting many clients at once relies on the shared `poison.json` to converge.

## Results

### `shared-gateway-mvp` (done)

`src/share.rs` — registry (`ActiveGateway` in `active.json`, atomic
write/remove-if-mine), `health_ok(port)` probe (the authoritative liveness
signal), `is_reusable` (model match), `DEFAULT_GATEWAY_PORT = 8089`, `gateway_dir`
under `$XDG_STATE_HOME/rozum/gateway/`. 3 unit tests (no feature/Xcode).

`rozum gateway` (`gateway::serve_on` + `ServeConfig`) now: publishes/removes the
registry, and idle-exits after `ROZUM_GATEWAY_IDLE_SECS` (default 900, `0` =
never) when no request is in flight and none has arrived — in-flight-aware via an
`Activity` counter updated in `auth_layer`, so a long generation can't trip it.

`rozum launch` (`ensure_shared_gateway`): reuse a healthy compatible gateway
(or a different-model one with a warning, MVP), else spawn a **detached** `rozum
gateway` (own process group, stdio → `gateway.log`) and poll for health (fails
fast if the daemon exits during load; 300 s cap otherwise). `--dedicated` keeps
the old private in-process gateway. `exec_agent` factors the agent env wiring and
points `ANTHROPIC_MODEL` at the *effective* model.

### `shared-gateway-failover` (done)

`share::try_spawn_lock(stale_secs)` — O_EXCL `spawn.lock` with stale-steal +
drop-release (best-effort anti-stampede; the TCP bind is the real guarantee).
`spawn_failover_watchdog(model, n_ctx, port)` runs in the launch alongside the
agent: polls health every 5 s, and after 2 consecutive misses respawns the daemon
under the spawn lock (rechecking health under the lock first), waiting up to 120 s
for it to come back. Simultaneous watchdogs are damped by the lock and deduped by
the port bind. Transparent to the agent modulo its own retry over the brief gap.

### `shared-gateway-leases` (done)

`share`: `leases/<pid>` (touch = rewrite → bump mtime), `live_lease_count(fresh)`
(counts mtime-fresh leases, reaps clearly-dead ones), `LEASE_FRESH_SECS=60`.
Launch heartbeats its lease every 15 s (`spawn_lease_heartbeat`). The daemon's
idle watchdog now stays up while `live_lease_count>0 || in_flight>0 || recent
HTTP`, exiting only when all quiet — so leases (not raw HTTP) are the primary
keep-alive, while a manually-run `rozum gateway` is still kept by HTTP traffic.
`rozum gateway status` / `stop [--force]` added (`Gateway` gained an optional
`status`/`stop` subcommand; `--model` is now optional, required only to run).

Deferred (later phases): the
launch-local proxy, replay, poison, two-tier backpressure
(`shared-gateway-proxy`/`-replay-retry`/`-poison`); `switch`/`reload`/`unload`.

Verification: `cargo fmt --check` clean; 67 lib tests (3 new in `share`) on the
default build (no Xcode); `cargo check --features mistralrs` clean.

### `launch-model-picker` (done)

`rozum launch --model` is now optional (`Option<String>`). `resolve_launch_model`
resolves it: given → use it; omitted + a healthy gateway running → reuse its model
(`using running model: <model>`); omitted + nothing running on a TTY →
`pick_model_interactive`; omitted + non-TTY → error. The picker lists
`models::scan_all_installed()` first as `(cached, <size>)`, then the not-yet-cached
`models::RECOMMENDED` as `(not cached, ~<GB>)`; a not-cached pick re-confirms the
download. Mismatch policy is now **takeover-if-idle** in `ensure_shared_gateway`:
a different running model with **no** live leases is SIGTERM'd and replaced on the
same port; with live leases it is reused-with-warning (don't steal a live
session). `--dedicated` still bypasses everything.

### `models-rm` (done)

`rozum models rm <spec> [-y]` (`run_models_rm`): exact-matches `spec` against
`scan_all_installed()`, refuses if it is the active gateway model (reads
`active.json` + `health_ok`), prints what will be freed, and confirms
(`confirm_delete`: `--yes`/`-y` skips; non-TTY without `--yes` refused). Deletes
HuggingFace (the `models--owner--name` dir) and LMStudio (the repo dir holding the
`.gguf`) directly via `remove_dir_all`; Ollama is delegated to `ollama rm` (its
blobs are shared/content-addressed) and refused if the binary is absent. A
dependency-free `which` helper locates `ollama`.

Verification: `cargo fmt --check` clean; 67 lib tests; default + `--features
mistralrs` build/check clean.

### `shared-gateway-proxy` (done)

`src/proxy.rs` — a launch-local **model-free** reverse HTTP proxy (the gateway
analog of `meeting::proxy`). `proxy::serve(listener, daemon_port)` runs an axum
`fallback` that forwards every request to `http://127.0.0.1:{daemon_port}{path?query}`:
it buffers the request body (the seed for future replay), strips hop-by-hop +
framing headers both ways, sends via a no-timeout reqwest client, and streams the
response back verbatim via `Body::from_stream(resp.bytes_stream())` — so SSE token
streams pass through unchanged. An unreachable daemon yields a clean `502`
`upstream_error` (the surface that `shared-gateway-replay-retry` will replace with
replay-before-first-token). `daemon_port` is held in an `AtomicU16` so a later
phase can re-point the proxy at a respawned daemon without rebuilding the router.

`main.rs` `start_launch_proxy` binds an ephemeral `127.0.0.1` port, spawns
`proxy::serve`, and `exec_agent` now points the agent at the **proxy** port (the
failover watchdog and lease heartbeat still target the daemon's stable port). If
the proxy can't bind, launch falls back to pointing the agent straight at the
daemon. The proxy task dies with the launch, like the old in-process gateway.

Verification: 5 new tests (header filtering + two real end-to-end tokio tests:
method/path/query/body/header pass-through and streamed body; dead-daemon 502);
70 lib tests total; fmt + `--features mistralrs` clean. No new deps.

Deferred to `-replay-retry`/`-poison`: replay-before-first-token, smart retry,
two-tier admission, and poison fingerprinting all build on this buffered-body
forward path.

### `shared-gateway-replay-retry` (done)

**Replay before first token + smart retry** (`src/proxy.rs`). `forward` buffers
the request body once and runs a retry loop: a connection failure *before any
response byte reaches the agent* (i.e. before a `Response` is returned, which
commits status+headers) is safe to replay — the proxy waits for re-election to
bring the daemon back on the same stable port (`wait_for_health`) and re-sends
the buffered body. Once streaming starts, a death surfaces as a stream error
(no un-sending tokens). Retries use capped exponential backoff + ±50% jitter (no
`rand` — wall-clock nanos), a per-request attempt cap, wait-for-health between
tries, and honor `429`/`Retry-After` by holding and retrying rather than
bouncing. Tunables: `ROZUM_PROXY_MAX_ATTEMPTS` (6), `ROZUM_PROXY_BACKOFF_MS`
(150), `ROZUM_PROXY_HEALTH_WAIT_SECS` (60).

**Two-tier admission.** Tier-1 (global): the daemon exposes its admission state
via `GET /v1/admit` (`gateway.rs::admit_handler` ← new
`ChatBackend::admission_stats()` → `AdmissionSnapshot`, implemented by
`concurrency::AdmittingBackend`); ungated backends report an always-free window.
Tier-2 (per client): each proxy holds its own `concurrency::AdmissionScheduler`
(`proxy_admit_config`, `ROZUM_PROXY_ADMIT` default 4, `ROZUM_PROXY_FASTLANE_TOKENS`
default 1024, unbounded queue) over its single agent's parallel requests —
SJF + fast lane, cost estimated from body size, the guard held for the whole
stream. Before forwarding, `wait_for_window` polls `/v1/admit` and holds the
request at the edge until the daemon signals room (bounded by the health-wait
budget; **fail-open** on any probe failure so the `429` backstop still applies).
Reuses the one `concurrency` module at both tiers.

Verification: `cargo fmt --check` clean; 77 lib tests (proxy backoff math, cost /
fast-lane, end-to-end replay-after-daemon-returns, end-to-end edge-gating until
the daemon opens the window; `admission_stats` snapshot); `--features mistralrs`
check clean. No new deps.

### `shared-gateway-poison` (done)

**Soft, graduated poison handling** in the proxy (`forward` in `src/proxy.rs`).
Each request is fingerprinted by `share::fingerprint` — a `DefaultHasher` over the
**raw body bytes the proxy forwards verbatim**, so the proxy and the daemon derive
the same value with no dialect-normalization divergence (raw-body equality is a
robust superset of the spec's "normalized messages + sampling params": the agent
re-sending the same turn sends byte-identical JSON).

Crash attribution is precise: an `Err` from the upstream send is blamed on the
prompt **only if the connection was established and then died** (`!e.is_connect()`)
— a pure connect failure is a failover gap (the daemon isn't listening), which
stays on the existing wait-for-health replay path and is never counted as poison.
On a crash-attributed failure the proxy (1) **degrades**: the retry takes the
exclusive `lane` write-lock so the risky prefill runs serialized (no neighbour
competes for memory — most "poison" prompts are big-prompt OOMs that clear once
alone); (2) **counts** per fingerprint in `poison` (surviving across separate
turns); (3) after `ROZUM_POISON_MAX` (default 3) crash-attributed attempts,
returns a **soft, retryable 422** (`poisoned()`, `type: poison_refused`) — never a
hard block.

**Shared persistence only on high confidence.** When the graduated retries are
exhausted *and* the crash was the **sole in-flight** request at this proxy
(`admit.stats().in_use <= 1`), the fingerprint is confirmed machine-wide via
`share::record_poison` to a TTL'd `poison.json` (`ROZUM_POISON_TTL_SECS`, default
3600). Ambiguous (concurrent in-flight) crashes stay local — a coincidental crash
can't ban a good prompt for everyone. A confirmed entry is fast-refused two ways:
the proxy checks `is_poisoned` *before forwarding* (so it never re-kills the shared
daemon), and the daemon's `gateway::poison_layer` middleware re-checks it
*before running the model* (defense-in-depth for direct hits, surviving the very
crash it guards against). Entries are advisory: they expire on TTL and **decay on
the next clean (2xx) prefill** — `clear_poison` drops them both locally and
machine-wide.

Tunables: `ROZUM_POISON_MAX` (3), `ROZUM_POISON_TTL_SECS` (3600).

Verification: `cargo fmt --check` clean; 81 lib tests (+4: `share` fingerprint
stability + poison-set record/refuse/decay/expire; proxy poison-count helper; an
end-to-end test with a connection-dropping "crasher" upstream that asserts the
soft 422 after repeated crashes); `--features mistralrs` check clean. No new deps.
The two env-mutating tests are serialized via `share::POISON_ENV_LOCK` so the
`unsafe` `XDG_STATE_HOME` writes never race a concurrent read.
