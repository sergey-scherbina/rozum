# Running rozum as a Service (distributed readiness)

## Goal

Make the gateway a **deployable, horizontally-scalable, stateless model service**: put N
identical `rozum gateway` instances behind a load balancer, let any instance serve any
request, and roll deployments without dropping in-flight work. This is the
`rozum-distributed-readiness` item — the model-service side of the
`busi` / agent-SDK split (`integration.md`): orchestration + session state live in the
client; rozum just answers `messages + tools → tool_calls/text` over HTTP.

## Stateless by construction

A request carries everything needed to answer it — the full `messages` array (+ `tools`).
The gateway keeps **no per-session server state** that another instance would need:

- The prefix-KV cache (native MLX) and any KV reuse are a **per-instance latency
  optimization**, not affinity. A request routed to a cold instance still produces the
  same answer — it just reprefills. So the load balancer needs **no sticky sessions**; round-
  robin / least-connections is fine.
- Admission control (`concurrency::admit_wrap`) is per-instance. Two-tier backpressure
  (`shared-gateway.md`) lets a launch-local proxy hold requests at the edge; for a fleet, the
  LB + each instance's `/ready` + 429 shedding cover the same role.

## Health vs readiness

Two endpoints, deliberately distinct (the standard k8s split):

- **`GET /health`** — *liveness*. 200 as long as the process serves HTTP. Never touches the
  model. An orchestrator uses it to decide whether to **restart** the instance.
- **`GET /ready`** — *readiness*. 200 when the instance can serve a request **now**; 503
  otherwise. The load balancer uses it to decide whether to **route** to the instance.
  Body: `{ready, loaded, shutting_down, model}`.

`/ready` is 503 when: the instance is **shutting down** (draining), or the model is unloaded
*and* this gateway can't rebuild it (a `--dedicated` instance whose model was freed). A
transient model **swap**-drain (`/control/switch`) does **not** flip readiness — those
requests park for the brief swap and still succeed, so the instance stays in rotation.

## Graceful shutdown (rolling deploys)

`axum::serve(...).with_graceful_shutdown(...)` wired to SIGTERM/SIGINT:

1. Signal received → `mark_shutting_down()`: `/ready` flips to **503** and new chats are
   **rejected** (`enter()` returns 503 `shutting_down`) rather than parked.
2. Sleep `ROZUM_SHUTDOWN_GRACE_SECS` (default 3) — one readiness-probe cycle, so the LB
   deregisters this instance before any connection is cut.
3. The shutdown future returns → axum stops accepting new connections and **drains** the
   open ones (in-flight streaming requests run to completion).
4. Process exits; `active.json` registration is removed.

So a rolling deploy is: start new instances (they pass `/ready`), then SIGTERM old ones —
each old instance bleeds out cleanly while the new ones absorb traffic. The orchestrator's
own termination grace period bounds the worst case (a very long stream) with SIGKILL.

## Out of scope / follow-ups

- **Model pool / router** — one instance serving multiple resident models with size-class
  routing is `shared-gateway-multislot` + `concurrency-multi-instance`, not this item. Today
  an instance hosts one model; a heterogeneous fleet is "one deployment per model behind its
  own LB pool", which is enough for the agent-SDK use case.
- **Cross-instance admission coordination** (`concurrency-cross-process`) — each instance
  self-limits; a global budget across the fleet is a later optimization.

## Validation

Unit tests (`gateway::tests`): `readiness_reflects_servability` (loaded / lazy-reloadable →
ready; unloaded+dedicated → not), `shutdown_flips_readiness`, and
`enter_rejects_new_chats_while_shutting_down` (no leaked `generating` token). The signal
wiring + drain are exercised by hand (SIGTERM a running gateway, watch `/ready` → 503 then a
clean exit after in-flight finishes).
