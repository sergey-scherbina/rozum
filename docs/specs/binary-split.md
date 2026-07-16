# Binary split — heavy backend binaries vs thin frontends

Status: implemented on `master`. Thin frontends `rozum-meet` / `rozum-web` / `rozum-tui`, the
dispatcher (`rozum`), and the engine binary (`rozum-gateway`) are all shipped. Continuation of the workspace split
(`docs/specs/workspace-split.md`, which split the *crates*); this splits the *binaries*.

## Install & usage (after the split)

- `cargo install --path .`               → installs **`rozum-gateway`** (full CLI + engines).
- `cargo install --path crates/rozum-cli` → installs **`rozum`** (the thin dispatcher).
- `cargo install --path crates/rozum-meet` (and `rozum-web` / `rozum-tui`) → thin frontends.

`rozum <cmd>` keeps working: the dispatcher `exec`s `rozum-meet` for `mcp-proxy`/`mcp-http` and
`rozum-gateway` for everything else. Targets are resolved next to the dispatcher binary first,
then `PATH`, so an uninstalled `target/release/rozum` finds its just-built siblings.

Bench/e2e/smoke scripts that drive the gateway now resolve `…/rozum-gateway` (they need the
engine binary directly, no dispatcher hop). The user's launchd services (`com.rozum.*`) call
`rozum-meeting-ssc` / `rozum-ctrl` / python — NOT `rozum gateway` — so the rename does not touch
them.

## Why

The former single `rozum` binary linked everything. Its engine features pulled MLX C++ and,
when GGUF was selected, llama.cpp via CMake/Xcode — a clean build could take tens of minutes.
The current shipped defaults are `mlx-native + all-models`; GGUF is opt-in. Two concrete costs
motivated the split:

1. **A frontend fix pays backend build cost.** BUG-004 was a ~130-line change in `rozum-meeting`
   (the MCP bridge), yet shipping it meant rebuilding the monolith. Incremental reuse kept it to
   ~3 min this time, but a cold build or a Cargo.lock drift would have rebuilt MLX/llama for a
   change that touches neither.
2. **Rebuild can silently drop engine features.** `cargo install --path .` with the wrong
   `--features` produces a binary missing `mlx-native`/`gguf` — a working-looking `rozum` that
   can't serve models. The frontend has no business depending on that footgun.

The crates are already cleanly layered (`rozum-meeting` declares *zero* dependency on the
model-runtime side; engines sit behind their own features). So the binary split is mechanical, not
a re-architecture.

## Topology

**Heavy backend (the only binary that links engines):**

- `rozum-gateway` (a.k.a. `rozumd`) — the model-serving gateway + engines (`mlx`/`gguf`/`mistralrs`/
  `x86` behind features). Links MLX/llama C++. Rebuilt only when engine/gateway code changes.

**Thin frontends (link `rozum-core` / `rozum-meeting` / `rozum-agent`; NO engines, no C++):**

- `rozum-meet` — MCP bridges: `mcp-proxy` (stdio) + `mcp-http` (HTTP). **Done (PoC).**
- `rozum-tui` — the ratatui meeting client.
- `rozum-web` — the axum web bridge.
- `rozum-cli` — control / launch / admin; talks to the gateway over HTTP, does not link it.

**Umbrella (UX continuity):**

- `rozum` — a thin dispatcher. `rozum mcp-proxy` → exec `rozum-meet mcp-proxy`, `rozum gateway` →
  exec `rozum-gateway`, etc. Preserves existing muscle memory AND the installed MCP config
  (`command: rozum, args: [mcp-proxy]`) while the heavy code lives only in `rozum-gateway`.
  Alternative: keep the fat `rozum` during migration and slim it last.

## Payoff

- Frontend fix → rebuild a thin bin in **seconds**, never touching the C++ engines.
- Point the MCP config straight at `rozum-meet` → BUG-004-class fixes are fully decoupled from the
  backend build.
- Per-binary install: `cargo install --path crates/rozum-meet` for the frontend; the gateway is
  installed/rebuilt on its own cadence.
- The HTTP transport (Phase 2 of `mcp-proxy-resilience`) ships as a thin frontend — `rozum-meet
  mcp-http` — so the resilient transport doesn't depend on the heavy build either.

## Migration (incremental, each step ships independently)

1. **`rozum-meet` (done).** Proves a frontend binary builds with no engine deps.
2. Extract `rozum-tui`, `rozum-web`, `rozum-cli` the same way (each a thin bin over existing
   crates; module bodies already live in `rozum-meeting` / `rozum-core`).
3. Rename the engine-linking binary to `rozum-gateway`; keep `gateway` subcommand.
4. Slim `rozum` to a dispatcher (or keep fat during transition). Update `rozum mcp install` to
   write whichever entrypoint is canonical.

## Decisions for the operator

- **Umbrella dispatcher vs distinct names.** Dispatcher keeps `rozum <cmd>` working everywhere
  (recommended); distinct names are simpler but break existing configs/scripts.
- **MCP config target.** Point Claude Code at `rozum-meet` (thinnest, fastest to fix) or keep
  `rozum` (dispatcher) for one entrypoint. Recommend `rozum` dispatcher → `rozum-meet`.
- **Install/distribution.** More artifacts; a dispatcher keeps one user-facing entrypoint.
