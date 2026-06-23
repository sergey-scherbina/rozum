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

## Sequencing

1. **`rozum-meeting::client` (read side) — FIRST, this PR.** Move the inline disk/parse logic out of the
   `rozum` bin into a `client` module: `resolve_room_root`, `read`, `inbox` (+ cursor), `roster`. The CLI
   handlers become thin presentation over it. Removes storage-format knowledge from the binary.
2. **Client API: write side** — add `post`/`status`/`hello`/`whoami` to the module; CLI fully thin.
3. **Serve the same operations over HTTP** — extend `rest_read` to cover inbox/roster/post so web/remote
   clients use the API, not disk/exec.
4. **Migrate the web `.ssc` + the Rust TUI** to consume the API (drops their direct disk/exec reads).
5. **Models side already clean** — keep; document the gateway API contract if it isn't already.

## Out of scope

The UCC `Tk` client itself (`unified-control-center.md`); the binary split; multi-host/remote auth
(later rung of the Principal model).
