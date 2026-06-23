# Services & clients — separate services, clean APIs, one unified client

Status: design + first increment (sunny-civet, 2026-06-23). Operator-driven. Pairs with
`docs/specs/unified-control-center.md` (the client) and `docs/specs/architecture-spi.md` (the seams).

## Drivers (operator: ALL THREE)

- **(a) Failure isolation / reliability** — a heavy, crash-prone gateway must never take down the
  tiny, always-up meeting room.
- **(b) One unified client over clean APIs** — the UCC dashboard needs *operations*, not internal
  formats, from each subsystem.
- **(c) Code cleanliness** — one place owns each concern; no client knows another's storage format.

## Current state (grounded, 2026-06-23)

**Models ↔ meetings — ALREADY separated, and well:**
- Separate crates: `rozum-meeting` vs `rozum-gateway`/`rozum-models` (+ 4 engines). `rozum-meeting`
  has **no Cargo dependency** on gateway/models.
- Coupling is only via the gateway's **HTTP API**: `model_participant` calls
  `{gateway_url}/chat/completions` — a clean seam, not a code coupling.
- **Separate processes**: the meeting daemon (tiny, always-up, no GPU) and the gateway (heavy, GPU/
  RAM-gated, crash-prone) are distinct processes → failure isolation (a) already holds.
- One `rozum` binary — but that's only a *delivery* vehicle; the running processes are distinct.

**Server ↔ clients — only HALF clean:**
- Gateway = a clean HTTP-API server; its clients (Claude/Codex/`model_participant`) touch only the
  API. ✅
- Meetings = a daemon (socket for post/join + an axum REST read, `rest_read.rs`) BUT the CLI/web/TUI
  also **read the jsonl/principal/cursor files DIRECTLY** — so clients depend on the internal storage
  format. ⚠️ This is the one real gap.

## Target architecture

```
ONE client surface (UCC: one .ssc → TUI / web / CLI)   ← unify at the CLIENT layer
        │  stable service APIs (operations, never storage format)
services:  [meetings]        [models / gateway]        [hardware / placement — North Star]
            tiny, always-up    heavy, GPU/RAM-gated       (later)
        │
rozum-core  (SPI, share/residency ledger, identity, obs)   ← shared core, not duplicated
```

**Separate at the SERVICE layer; unify at the CLIENT layer.** A unified "models + meetings" dashboard
is only buildable if each service exposes clean operations and one client aggregates them. Do NOT split
the binary for its own sake — process separation already delivers (a); a binary split adds packaging/
versioning cost with no benefit on one host.

## Decisions (made)

1. **Keep models/meetings as separate services** (already true). The only cross-coupling is
   `model_participant → gateway HTTP`; keep it API-only.
2. **Meetings gets ONE client API** — a single contract (`rozum-meeting::client`) encapsulating every
   operation (resolve-room, read, inbox, roster, post, status, identity). **No client touches the
   storage format.** Local clients call it in-process (it may still read the disk *behind the API* for
   efficiency — the format stays internal); remote/web clients get the *same* operations over HTTP
   (extend the existing `rest_read` axum surface). This single move serves (a) [daemon owns the
   format], (b) [one client over the API], and (c) [one place knows the format].
3. **No binary split.** Process separation already gives failure isolation; revisit only if independent
   deploy/versioning is ever needed.
4. **rozum-core stays the shared core** (identity, residency/host-safety, obs) — used by both services,
   never duplicated.

## The two service API contracts

**Meetings service — `rozum-meeting::client` (DONE) + HTTP (`rest_read`).** The single contract:
- *reads*: `resolve_room_root`, `read`, `inbox`(+`InboxCursor`), `roster`;
- *identity/write*: `post_identity` (the agent-vs-human posting rule), `whoami` (`Identity` enum),
  `establish` (hello), `daemon_status`;
- *HTTP* (`maybe_spawn_from_env`, basic-auth): `GET /rooms/{name}/{days,messages/{date}}`,
  `GET /rooms/{name}/inbox/{handle}`, `GET /roster`.
Local clients call the module in-process (it reads disk behind the API); remote/web fetch the HTTP JSON.
No client touches the jsonl/principal/cursor format.

**Models/gateway service — the gateway's HTTP API.** Already a clean service (`rozum gateway`):
- *inference*: OpenAI `POST /v1/chat/completions` + `/v1/responses`, Anthropic `POST /v1/messages`
  (the dialects Claude Code / Codex / `model_participant` consume);
- *control*: gateway status + the host residency/share ledger (`rozum-core::share`).
- *control snapshot*: `GET /control/status` → the `control::status()` JSON (active gateway + residency
  + installed catalog) for a dashboard / the UCC web target, alongside the existing
  `/control/{switch,unload,reload}`. Available while a gateway runs; an always-up control surface (the
  gateway-as-daemon, or a light control server, so the dashboard works with NO model loaded) is a
  follow-up — until then a client falls back to `rozum gateway status --json` (the CLI always works).
`rozum-meeting::model_participant` consumes **only** `{gateway_url}/v1/chat/completions` — the sole
meetings↔models coupling, an HTTP seam, not a code dependency. Keep it API-only.

## Sequencing

1. **`rozum-meeting::client` (read side)** — DONE (`c838655`).
2. **Client API write side** (`post_identity`/`whoami`/`establish`/`daemon_status`) — DONE (`2826a1d`).
3. **HTTP parity** (inbox + roster over `rest_read`) — DONE (`a653cff`); POST-over-HTTP deferred.
4. **Document the gateway service API contract** — DONE (above).
5. **Migrate the web `.ssc` + Rust TUI to the API** — **DEFERRED / largely superseded.** The web `.ssc`
   already consumes the client API *indirectly* (it execs `rozum meetings …`, now thin over the API),
   and the hand-written Rust TUI is **replaced** by the UCC `Tk` app (`unified-control-center.md`) —
   migrating it now is throwaway work. Re-open only if a need predates UCC (e.g. the web switching from
   exec to HTTP fetch, which also needs the REST server enabled by default).

## Out of scope

The UCC `Tk` client itself (`unified-control-center.md`); the binary split; multi-host/remote auth
(later rung of the Principal model).
