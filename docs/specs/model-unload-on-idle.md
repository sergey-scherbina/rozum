# Model Unload-on-Idle — free RAM when agents are attached but idle, fast reload on demand

> **Status: core implemented** (`feature/model-unload-on-idle`). The timed
> auto-unload trigger, `gateway_idle_unload` obs event, `--dedicated` guard, and
> `ROZUM_GATEWAY_UNLOAD_IDLE_SECS` (default 300) are done, reusing `gateway-switch`'s
> `Switchboard::unload()` + serialized lazy reload, on the existing idle watchdog
> tick. **Still open:** the cold-vs-warm reload *measurement* and any fast-reload
> tier beyond the OS page cache (needs a real model on Metal, not runnable in CI);
> and the pre-warm-on-turn-signal follow-up.

## Motivation

A resident local model holds many GB of unified memory. When every agent has
been idle for a long time (no inference, but sessions still open / leases still
held), that memory is wasted and squeezes everything else on the machine. We
want to **drop the model from memory after a long idle period but keep the
gateway daemon alive**, then **reload quickly** when an agent needs it again.

## How this relates to idle-exit (they are NOT the same — they are complementary)

The existing **idle-exit** watchdog (`src/gateway.rs`, ~`:1612`) loops every 30 s
and calls `std::process::exit(0)` only when **all** of:

```
in_flight == 0  &&  live_lease_count(LEASE_FRESH_SECS) == 0  &&  idle_for >= idle_secs
```

The decisive clause is **`live_leases == 0`**: idle-exit fires only when **no
launch is attached at all**. When it fires it kills the whole process, freeing
everything (model + listener + registry); the next `rozum launch` re-spawns a
daemon.

Unload-on-idle targets the case idle-exit **deliberately excludes**: **leases > 0
but nobody is generating.** Agents are attached (sessions open, holding leases)
but have not run inference for a while — idle-exit will never fire, so the model
sits resident and wasted. Unload-on-idle drops just the model's memory and
**keeps the daemon** (stable port, registry, admission state, the open agent
sessions), with a lazy reload on the next request.

| | trigger condition | action | when it applies |
|---|---|---|---|
| **idle-exit** (exists) | `in_flight==0 && leases==0 && idle ≥ `idle_secs`` | process exits, frees all | no agent attached |
| **unload-on-idle** (this spec) | model loaded `&& generating==0 && idle ≥ `unload_secs`` (lease count irrelevant) | `Switchboard::unload()`, daemon stays up | agent(s) attached but idle |

They share the same `Activity::last_active` clock and can live in the **same 30 s
watchdog tick**; their conditions are disjoint in the common case (idle-exit
needs `leases==0`, unload is the `leases>0` path). Tick order: check the
exit condition first (frees the most when nobody's attached); otherwise check the
unload condition. If somehow both qualify (`leases==0` and both timers elapsed),
exiting wins — it's strictly more freeing. So unload-on-idle is "free the model
*without* killing the daemon, for attached-but-idle agents", and idle-exit stays
"kill the daemon when truly abandoned".

## Behavior (proposed)

- A background watchdog in the daemon tracks "time since last generation
  finished" (reuse the `generating` counter `gateway-switch` already added, plus
  a `last_active: Instant`).
- When idle for `ROZUM_GATEWAY_UNLOAD_IDLE_SECS` (**default 300 s / 5 min**;
  `0` disables) **and** the model is currently loaded, the watchdog calls the
  existing `Switchboard::unload()`. The threshold is the one tunable, "as always".
- The next inference request lazily reloads via the existing serialized
  `reload_lock` path (already implemented). `/v1/admit` should advertise a closed
  window while reloading so the launch proxies hold requests (same UX as a
  `switch` drain) rather than erroring.
- **Shares the existing idle-exit watchdog tick** (the 30 s loop at
  `gateway.rs:~1612`). Per the table above, the conditions are disjoint in the
  common case; evaluate exit first, then unload. Independent env knobs
  (`idle_secs` for exit, `ROZUM_GATEWAY_UNLOAD_IDLE_SECS` for unload). Reuse
  `Activity::last_active` for the clock and the `generating` counter (added by
  `gateway-switch`) to mean "model in use right now".
- A `--dedicated` gateway has no `BackendBuilder`, so it cannot reload → it MUST
  NOT auto-unload (same guard `unload()` already has).
- Observability: emit an obs event on auto-unload and on lazy-reload (the reload
  event already exists) so the latency is measurable.

## Fast reload — the real design problem

Reload latency is the whole UX risk: an agent that returns after idle must not
wait a cold load. Options, cheapest → most involved (to be measured, not assumed):

1. **Rely on the OS page cache.** MLX/GGUF weights are `mmap`'d from the HF/GGUF
   cache on disk. Dropping the model frees the *Metal/GPU-resident* buffers and
   any rebuilt host structures, but the underlying weight files often stay warm
   in the page cache, so reload ≈ re-mmap + re-upload to Metal, not a disk read.
   **Measure this first** — it may already be "fast enough" and make 2–4 moot.
2. **Partial unload.** Free only what is expensive-to-hold but cheap-to-rebuild
   (KV cache pool, scratch, Metal buffers), and keep the cheap-to-hold,
   slow-to-rebuild bits resident. Requires knowing what mistralrs/the backend
   actually lets us drop granularly — likely limited without backend support.
3. **Keep files warm deliberately.** On unload, `mmap`/`madvise(WILLNEED)` or
   touch the weight files so the page cache retains them; reload then avoids disk.
4. **Snapshot to a temp file** (the user's "сохранять во временные файлы"): only
   worthwhile if some preprocessed/dequantized form is faster to reload than the
   on-disk checkpoint. For already-mmap'd quantized safetensors this is probably
   *not* a win (the checkpoint is already the fast source); flag as "investigate
   only if (1) proves slow".

The honest first step is **(1) measure cold vs warm reload time** for our actual
models (Qwen3-class MLX/GGUF on Metal). The optimization tier is chosen by that
number, not designed up front.

## Decisions

- **Threshold:** default **5 min** (`ROZUM_GATEWAY_UNLOAD_IDLE_SECS=300`),
  env-overridable; `0` disables. Independent of the idle-exit window.
  Was 15 min until 2026-08-16. The threshold trades RAM held while nobody is
  asking against how long the next question waits, and the second half was
  finally MEASURED once BUG-052 made unload reachable: a warm reload is **1.1 s**
  end-to-end (weights still in the OS page cache, MLX mmaps them). A second of
  latency against ~7.4 GiB is not a trade worth waiting ten more minutes to make.
  The durable plist deliberately does NOT pin this any more — it said 1200 while
  the code said 900, and only the plist decided anything.
- **Relationship to idle-exit:** resolved — complementary, share one watchdog
  tick (see the section above).
- **Idle clock:** "no generation in progress" (`generating==0`) + `last_active`
  age, not lease count.
- **Pre-warm (adopted as an optional follow-up):** a `channel-wakeup` `your_turn`
  signal — or any MCP room activity for *this* agent — should proactively
  trigger a lazy reload *before* the agent issues its first inference call, so
  the model is warm by the time it generates. Nice synergy with `channel-wakeup`;
  gate behind the fast-reload measurement (only worth it if reload is slow
  enough to notice). Implement after the core auto-unload lands.

## Open Questions

1. **Reload-latency budget.** Acceptable first-request delay after an auto-unload
   (≤ a few seconds?). Answered by the cold-vs-warm measurement below, not chosen
   up front — it selects which fast-reload tier (if any beyond page-cache) we need.

## Scope

- `src/gateway.rs` — extend the existing idle watchdog to also call
  `Switchboard::unload()`; reuse `last_active`/`generating`; `--dedicated` guard.
- No new control-plane verb (reuses `unload` + lazy reload).
- Bench harness for cold/warm reload to pick the fast-reload tier.
- Env: `ROZUM_GATEWAY_UNLOAD_IDLE_SECS` (default 300, 0=off).
- **Unload must release the RESIDENCY RESERVATION, not just the memory** (BUG-053).
  The reservation was bound to the process guard, so a gateway that had freed
  7.35 GiB went on publishing it and no sibling could use the RAM. The primary is
  now published only while `backend.is_some()`, and — necessarily paired — the
  lazy reload re-admits under the admit lock before it allocates
  (`share::readmit_my_reservation`), refusing with a 503 when the host has since
  been committed elsewhere. Lowering without re-admitting is an overcommit hole.
- Follow-up: pre-warm reload on a `channel-wakeup`/MCP turn signal.

## Out of scope (for the first cut)

- Cross-machine / multi-daemon coordination.
- Speculative snapshotting (tier 4) unless tier 1 measurement forces it.
