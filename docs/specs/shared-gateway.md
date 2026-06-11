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

rozum models rm <spec>      # delete a cached model (confirm; refuse if it is the active model)
rozum models list [--remote]   # (existing) data source for the picker
```

### Rendezvous state (under `$XDG_STATE_HOME/rozum/gateway/`)

```
active.json        # { model, port, pid, n_ctx, started_at } — the registry
spawn.lock         # advisory flock, held only during a spawn attempt (anti-stampede)
leases/<pid>       # one file per live launch client; mtime = heartbeat
```

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

An agent is launched with `ANTHROPIC_BASE_URL=http://127.0.0.1:P`. If a crashed
gateway respawned on a *different* port, that URL would dangle. A fixed port per
shared gateway makes failover transparent (the agent retries the same URL; brief
`connection refused` window, which Claude Code rides out — same resilience the
`mcp-proxy` reconnect already relies on). Single resident model ⇒ a single port
suffices; multi-slot (BACKLOG) would key the port by model.

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

### Composition

The shared gateway keeps wrapping its backend in `concurrency::admit_wrap`, so
N launch clients on one gateway are admission-controlled exactly as designed —
this feature is what finally makes that multi-client path real.

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

## Results

(Filled after implementation.)
