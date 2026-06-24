# MCP proxy resilience

Status: in progress. Phase 1 (in-process hardening) implemented on
`feature/mcp-proxy-resilience`. Phase 2 (HTTP transport) is designed here, not yet built.

Tracks BUG-004 (`mcp-proxy` dies mid-session → tools vanish) and the structural follow-up.

## Problem

Claude Code reaches the rozum meeting daemon through a **per-session stdio child** it spawns
from `~/.claude.json`:

```json
"rozum": {"type":"stdio","command":"/Users/sergiy/.cargo/bin/rozum","args":["mcp-proxy"]}
```

That child (`rozum mcp-proxy`, `crates/rozum-meeting/src/meeting/daemon_proxy.rs`) is the **sole**
carrier of the `mcp__rozum__*` tools into the session. It bridges stdio ⇄ the daemon
`meeting.sock`. The meeting daemon (`rozum meetings start`) is independent and the CLI hits the
socket directly — so **"MCP disconnected" ≠ "rozum down"**; only the bridge died.

Two structural facts make this fragile:

1. **Claude Code does not restart a dead stdio MCP server within a session.** Once the child
   exits, the tools are gone until a manual `/mcp` reconnect or a CC restart — neither doable by
   the agent itself.
2. **The bridge is a single point of failure with no logs.** Its only trace was
   `eprintln!("proxy error")` captured into CC's per-server MCP log, which records nothing on a
   clean `exit(0)` and only an opaque transport-close otherwise.

Likely death causes: the idle watchdog reaping a live-but-idle interactive session (see below),
or a `serve()`/transport error → `exit(1)`.

## Phase 1 — in-process hardening (implemented)

`crates/rozum-meeting/src/meeting/daemon_proxy.rs`:

- **Observability.** `proxy_log()` appends lifecycle lines (start, initialize, daemon-connect,
  every exit + reason) to `$RUNTIME/mcp-proxy.log` (rotates at 256 KiB; `ROZUM_MCP_PROXY_LOG=0`
  disables). `install_panic_logger()` records panic payload + location before the default hook.
  `run_daemon_proxy` distinguishes `serve-error` / `stdin-eof` / `join-error` exits.
- **Watchdog no longer reaps live sessions.** BUG-002 added a watchdog that reaped any proxy idle
  past `ROZUM_MCP_PROXY_IDLE_SECS` (default 2 h) with an unconditional `exit(0)`. Its assumption
  ("a room-using agent polls `wait_my_turn` every ~25 s, so silence = abandoned") is false for an
  *interactive human-driven* session. Now, past the soft window the watchdog reaps **only if the
  client transport is actually gone** (`rmcp::Peer::is_transport_closed()` — true once the rmcp
  loop tears down, i.e. CC disconnected). A live-but-idle session keeps its transport open and is
  not reaped. A stuck orphan whose pipe never closes (the BUG-002 case) is bounded by a new hard
  cap `ROZUM_MCP_PROXY_MAX_IDLE_SECS` (default 24 h, `0` disables). `ROZUM_MCP_PROXY_IDLE_SECS=0`
  still disables the watchdog entirely.

Why not an external re-exec supervisor: MCP is stateful — CC sends `initialize` once over the
pipe. A respawned worker on the same pipe never gets a fresh handshake, so resilience must be
in-process (Phase 1) or move off the per-session child entirely (Phase 2).

### Env vars

| Var | Default | Effect |
|-----|---------|--------|
| `ROZUM_MCP_PROXY_LOG` | on | `0` disables the proxy's own log |
| `ROZUM_MCP_PROXY_IDLE_SECS` | 7200 | soft idle window; `0` disables the watchdog |
| `ROZUM_MCP_PROXY_MAX_IDLE_SECS` | 86400 | hard cap (reap even if client connected); `0` off |

## Phase 2 — HTTP transport (designed, not built)

The deeper fix removes the per-session child. The long-lived meeting daemon exposes an **HTTP MCP
endpoint**; Claude Code connects to it as `{type:"http", url:"…"}` and **reconnects on drop**.
Nothing per-session to crash.

Feasibility: rmcp 1.7 ships `transport-streamable-http-server` (`StreamableHttpService`, a
`tower::Service`) with stateful sessions (`Mcp-Session-Id`), SSE keep-alive, and
`with_allowed_hosts`/`with_allowed_origins` (DNS-rebinding protection). axum / tower-http /
reqwest are already in the dependency tree (meeting web bridge, gateway). So this is additive,
not a new stack.

Design:

- **Host it in the daemon.** The daemon already is the single room writer; add a `/mcp` route on
  a stable `127.0.0.1` port. Collapses (daemon + N stdio proxies) into one server.
- **Per-client identity.** The stdio proxy derives the project from its own cwd
  (`detect_project()`); an HTTP daemon does not share the client's cwd. CC config is per-project,
  so bake identity into the URL: each repo's `.mcp.json` → `…/mcp?project=/abs/path` (or a path
  segment). `rozum mcp install` writes this form.
- **Sessions.** Use the streamable-http stateful session store so a reconnect resumes the session
  (token, room, read cursor) instead of re-joining cold.
- **Channel-wakeup.** Server→client notifications ride the SSE GET stream (rmcp supports it), so
  wakeups keep working.
- **Security.** Bind loopback only; set allowed hosts/origins; consider a per-install bearer token
  in the URL for defense in depth.

Costs / open questions:

- Port lifecycle: stable port, in-use handling, advertise it to `rozum mcp install`.
- Config migration from stdio → http; keep the stdio proxy as a fallback transport.
- Reconnect semantics: confirm CC's reconnect/backoff for `type:"http"` and that session resume
  works across it.

Recommendation: ship Phase 1 now (makes the stdio path robust + observable); spike Phase 2 behind
a flag once an incident log confirms the dominant failure mode.
