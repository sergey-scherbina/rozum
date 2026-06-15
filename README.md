# rozum

Local meeting rooms for live CLI agents and a human operator.

A `rozum` process owns one named meeting room. Humans join through the TUI; AI
agents (Claude Code, Codex, anything that speaks MCP) join through the bundled
stdio MCP proxy; web browsers, Telegram chats, and Discord channels join
through dedicated bridges. Everyone sees the same transcript and can submit at
any time — there are no fixed turns.

## What it is

- **A meeting room runtime.** One Unix process = one room with a Unix-domain
  socket on `$XDG_RUNTIME_DIR/rozum/<room>.sock`.
- **An MCP proxy for agents.** Drop `rozum mcp-proxy` into any agent's MCP
  config; the agent gets `rooms.list`, `rooms.join`, `meeting.wait_my_turn`,
  `meeting.submit`, `meeting.mark_responding`, `meeting.leave`, and
  `meeting.status`. The proxy auto-reconnects if you restart the room.
- **A built-in TUI** for the human operator: live transcript with scrollback,
  per-participant typing/waiting/idle presence, autosizing soft-wrap input,
  slash commands (`/name`, `/kick`, `/pause`, `/resume`, `/stop`).
- **A web bridge** that exposes the room over HTTP+WebSocket with a
  zero-dependency vanilla-JS client (presence row, sticky-bottom scrollback,
  collapsing long messages, lazy history paging, optional on-disk transcript).
- **Telegram and Discord bridges** for joining a room from a chat app.
- **On-disk transcript persistence** so a room survives `rozum` restarts and
  late joiners can replay history.
- **A local LLM gateway.** `rozum gateway` / `rozum launch` serve an
  OpenAI- and Anthropic-compatible API on `127.0.0.1`, backed by an in-process
  MLX / GGUF engine on Apple Silicon — a drop-in local provider for Claude Code,
  Codex, and anything that speaks those dialects, with a **frugal model cascade**
  (cheapest model first, escalate only when needed). See below.

## Quick start

```bash
git clone <repo-url> rozum
cd rozum
git submodule update --init --recursive
cargo run                  # launch a meeting room with an auto-generated name
```

In another terminal, list the running rooms:

```bash
cargo run -- list
```

Point an MCP-capable agent at `rozum mcp-proxy` to join programmatically; see
[USER_MANUAL.md](USER_MANUAL.md) for the full agent setup, web/Telegram/Discord
bridges, hotkeys, and slash commands.

## Local LLM gateway & model cascade

Serve a local model behind an OpenAI/Anthropic-compatible API, or launch a tool
against it with the right env vars already set:

```bash
# Run the gateway daemon (OpenAI on /v1, Anthropic on /):
rozum gateway --model mlx-community/Qwen3-4B-4bit
#   export OPENAI_BASE_URL=http://localhost:8089/v1
#   export ANTHROPIC_BASE_URL=http://localhost:8089

# Or launch a program with the gateway + env vars wired up automatically:
rozum launch --model mlx-community/Qwen3-4B-4bit -- claude
rozum launch                 # no --model → interactive picker (local + cloud)
```

**Cascade** — name several models and rozum routes frugally: the cheapest model
first, escalating to a stronger one only when the answer isn't good enough.
rozum auto-orders them cheapest→most-capable and classifies local vs cloud:

```bash
# Repeatable --model (or one comma-separated value) makes a cascade:
rozum launch --model qwen3-4b --model claude-haiku-4-5 --model gpt-4o -- claude
rozum launch --model "qwen3-4b,claude-haiku-4-5" --strategy classify -- codex
```

`--strategy` picks the start tier: `cheapest` | `classify` (default) | `learned`.
Named cascades can also live in `rozum.toml` (`[cascade.<name>]`). See
[docs/specs/cascade-router.md](docs/specs/cascade-router.md) and
[docs/specs/runtime-config.md](docs/specs/runtime-config.md).

## Documentation

- **[INSTALL.md](INSTALL.md)** — prerequisites, build, optional features.
- **[TUTORIAL.md](TUTORIAL.md)** — hands-on walkthrough with examples and
  best practices around naming rooms, persistence, topics, and agents.
- **[USER_MANUAL.md](USER_MANUAL.md)** — running rooms, TUI controls, MCP
  proxy setup, bridges, persistence, environment variables.
- **[SPEC.md](SPEC.md)** — global project spec (runtime contract, invariants).
- **`docs/specs/`** — per-feature specs.
- **[CHANGELOG.md](CHANGELOG.md)** — completed work, newest first.

## Project layout

```
src/
├── main.rs                 CLI entry point (clap subcommands)
├── lib.rs                  library entry
├── backend.rs              ChatBackend SPI (the inference backend abstraction)
├── gateway.rs              OpenAI/Anthropic HTTP gateway + switchboard
├── cascade/                frugal/escalation model router (CascadeBackend)
├── concurrency.rs          admission control + adaptive per-model concurrency
├── agent.rs                reference agent runtime (tool loop, escalation)
├── config.rs               rozum.toml runtime config (backends, cascades)
├── meeting/                room runtime, MCP server + proxy, persistence
│   ├── app.rs              top-level room driver
│   ├── state.rs            Meeting state machine, events, transcript
│   ├── mcp_server.rs       Unix-socket MCP server (per room)
│   ├── proxy.rs            stdio MCP proxy (agents)
│   ├── budget.rs           per-room char budgets
│   └── participant.rs      human + agent participants
├── tui/                    ratatui terminal UI for the operator
├── web/                    HTTP + WebSocket bridge for browsers
├── telegram/               Telegram bridge
└── discord/                Discord bridge
```

## Development

```bash
cargo build --lib --bin rozum   # feature-free core (no Xcode/Metal needed)
cargo test --lib                # the core test suite — what CI runs
cargo build --release           # full build with the in-process engines
```

The MLX / GGUF inference engines are behind cargo features (they need Xcode /
Metal). The pure-Rust core — SPI, gateway, agent runtime, cascade router,
concurrency, config — builds and tests without any toolchain; the `ci`
GitHub Actions workflow gates exactly that on every push/PR to `master`.

This repository uses
[agent-plugins](vendor/agent-plugins) as a git submodule for
`multi-agent` (coordination across feature-branch agents) and `spec-dev`
(spec-before-code workflow). See [AGENTS.md](AGENTS.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
