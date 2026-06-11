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

- [ ] `rozum launch --model X`: if `active.json` exists, its `pid` is alive, the
      port answers `GET /v1/models`, and the model is compatible → **reuse**:
      point the agent's `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` at the running
      port and exec the agent. No new model is loaded.
- [ ] A stale registry (dead pid, or port not answering) is treated as "none
      running".

### Single-owner election & spawn

- [ ] When no usable gateway is found, the launch contends for `spawn.lock`
      (non-blocking `flock`): the **winner** spawns a detached `rozum gateway
      --model X --port P`, waits for it to become healthy, then proceeds; **losers**
      skip spawning and poll `active.json`/health with backoff until it is up, then reuse.
- [ ] Even without the lock, concurrent spawners are deduplicated by the TCP bind:
      exactly one `rozum gateway` binds the port; the rest fail `EADDRINUSE` and exit.
- [ ] The detached gateway outlives the launching client (it does not die when
      the agent exits).

### Failover

- [ ] If an in-flight agent request fails because the gateway died, the next
      launch (or a relaunch) re-runs discovery → election → exactly one respawn
      on the **same** port. No thundering herd (flock + bind both bound it to one).
- [ ] Because the port is stable, a respawned gateway is transparent to an
      already-connected agent after a brief reconnect window.

### Lifetime (idle shutdown)

- [ ] Each launch client maintains a lease (`leases/<pid>`, heartbeated). The
      gateway reaps leases whose pid is dead; when **no live lease** remains for
      `ROZUM_GATEWAY_IDLE_SECS` (default 300) it shuts down gracefully, freeing
      the port and the model. `--no-idle-timeout` keeps it up indefinitely.
- [ ] `rozum gateway stop` exits the daemon (refused while clients are attached
      unless `--force`); `rozum gateway status` prints model/pid/port/clients/uptime.

### Model resolution when `--model` is omitted

- [ ] Omitted **and** a gateway is already running → use it; print
      `using running model: <model>`.
- [ ] Omitted **and** nothing running, on a TTY → show an interactive picker
      (below). Non-TTY (piped/CI) → error: "no model specified and no gateway
      running; pass --model".
- [ ] The picker lists models we can actually run: **cached models first**, each
      annotated `(cached, <size>)`; then downloadable models annotated
      `(not cached, ~<size>)`. Data from `models::scan_all_installed()` (cached)
      + the curated remote list (`models list --remote`).
- [ ] Selecting a **not-cached** model re-confirms: "Download ~<size> and use it?
      [y/N]"; on yes, that model becomes the gateway's model (downloaded on first
      load via hf-hub).

### Model mismatch (requested ≠ running)

- [ ] `--model Y` while a gateway serves `X`:
      - if the running gateway has **no live leases** (idle) → **take over**: stop
        it, spawn one for `Y`;
      - if it has live clients → **reuse `X` with a warning** ("model X is in use;
        ignoring --model Y") and set the agent's `ANTHROPIC_MODEL` to `X`.
- [ ] `--dedicated` always bypasses sharing and runs a private in-process gateway
      on its own port (the pre-feature behaviour), regardless of what is running.

### Cache deletion

- [ ] `rozum models rm <spec>` resolves the spec to a cached model, prints what
      will be freed, and **confirms** before deleting. Refuses (no delete) if that
      model is the **active** gateway model.
- [ ] HuggingFace (`~/.cache/huggingface/hub/models--…`) and LMStudio (per-model
      dir) are removed directly. Ollama is delegated to `ollama rm <tag>` if the
      binary exists, else skipped with a note (its blobs are content-addressed and
      shared — not safe to `rm` directly).
- [ ] Prints the reclaimed size on success.

### Client transparency on daemon loss (replay, poison, retry)

The agent talks to a **launch-local model-free proxy** (mirrors `mcp-proxy` for
rooms); the proxy forwards to the shared daemon and owns the resilience policy.
This is what lets clients "not notice" a daemon crash/restart/swap.

- [ ] `rozum launch` runs a tiny reverse proxy on a per-launch local port and
      points the agent's `ANTHROPIC_BASE_URL`/`OPENAI_BASE_URL` at it; the proxy
      forwards to the shared daemon's stable port. (No model in the proxy.)
- [ ] **Replay before first token:** if the daemon connection fails *before any
      response byte has been forwarded to the agent*, the proxy waits for the
      daemon to come back (re-election respawns it) and re-sends the buffered
      request — transparently. The agent sees a slower response, not an error.
- [ ] **Mid-stream is not replayable:** once tokens have been forwarded, a daemon
      death surfaces an error to the agent (we can't un-send tokens); the agent
      decides whether to retry the whole turn. Documented, not hidden.
- [ ] **Poison-prompt protection (soft, graduated):** the proxy fingerprints each
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
- [ ] **Shared persistence only on high-confidence attribution.** A fingerprint is
      written to the shared, TTL'd `poison.json` only when the daemon died with
      this request as the **sole in-flight** one (unambiguous cause). Ambiguous
      cases (concurrent in-flight requests) are counted **locally at the proxy
      only** — never shared — so a coincidental crash can't ban a good prompt for
      everyone. A restarted daemon loads `poison.json` and fast-refuses confirmed
      entries; they are advisory and expire (`ROZUM_POISON_TTL_SECS`, default
      3600, shortened from the earlier 24 h) and decay on the next clean success.
- [ ] **Smart retry policy:** retries use exponential backoff + jitter, a per-
      request attempt cap, wait-for-health (don't fire at a warming daemon), and
      honor the daemon's backpressure (below) — so a crowd of reconnecting proxies
      doesn't stampede the fresh daemon.

### Two-tier admission: daemon backpressure → proxy queue

The daemon owns the **global** admission limit; the proxy holds its client's
requests in a **local priority queue** and only forwards what the daemon has room
for — so prompts wait at the edge instead of bouncing off a full daemon.

- [ ] The daemon advertises its admission state to proxies — current free slots /
      queue depth / `Retry-After` — via response headers on each call and a cheap
      `GET /v1/admit` (or `/health`) probe. The proxy uses this as a **forwarding
      window**: it does not send a queued request until the daemon signals room
      (and always backs off on `429` + `Retry-After`).
- [ ] Each proxy runs its **own** `concurrency::AdmissionScheduler` over the
      requests from its single agent (which can fire parallel tool/sub-agent
      calls): shortest-job-first by `RequestCost`, with the reserved fast lane —
      so a small request *may* jump ahead of a big one queued at the proxy ("if it
      gets lucky"), exactly as in the central scheduler. Reuses the same module.
- [ ] Net effect: two cooperating tiers — proxy-local ordering (per client) +
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

(Filled after implementation.)
