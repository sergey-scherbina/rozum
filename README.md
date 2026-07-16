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
  Codex, opencode, and anything that speaks those dialects, with a **frugal model
  cascade** (cheapest model first, escalate only when needed). See below.
- **A structural sandbox.** Every `rozum launch <agent>` runs the agent in a
  Seatbelt jail (macOS) — writes confined to its workspace, secrets denied, only
  the local gateway reachable off-box. On by default; `--no-sandbox` opts out.
- **A local-model conference.** Local models can join a meeting room as **live
  participants** alongside humans: `rozum meetings participant --model <spec>
  --room <name>` joins a model that reads the room and replies like anyone else,
  and `scripts/demo-conference.sh` brings up a whole sandboxed conference
  (several models + humans) in one command. See the user manual.

## Quick start

```bash
git clone <repo-url> rozum
cd rozum
git submodule update --init --recursive
cargo build --workspace --no-default-features --bins
./target/debug/rozum       # launch a meeting room with an auto-generated name
```

In another terminal, list the running rooms:

```bash
./target/debug/rozum list
```

Point an MCP-capable agent at `./target/debug/rozum mcp-proxy` to join programmatically; see
[USER_MANUAL.md](USER_MANUAL.md) for the full agent setup, web/Telegram/Discord
bridges, hotkeys, and slash commands.

## Local LLM gateway & model cascade

Serve a local model behind an OpenAI/Anthropic-compatible API, or launch a tool
against it with the right env vars already set:

```bash
# Run the gateway daemon (OpenAI on /v1, Anthropic on /):
rozum gateway --model mlx-community:gpt-oss-20b-MXFP4-Q4
#   export OPENAI_BASE_URL=http://localhost:8089/v1
#   export ANTHROPIC_BASE_URL=http://localhost:8089

# Or launch a coding agent with the gateway + env vars wired up automatically.
# The agent (Claude Code / Codex / opencode) runs jailed in a Seatbelt sandbox by
# default — writes confined to its workspace, only the gateway reachable off-box:
rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude
rozum launch --model mlx-community:gpt-oss-20b-MXFP4-Q4 codex
rozum launch                 # no --model → interactive picker (local + cloud)
rozum launch --no-sandbox …  # opt out of the jail (ROZUM_SANDBOX=0)
```

The two curated local models are **Qwen3.6-35B-A3B** (strongest local agentic coder)
and **gpt-oss-20b** (OpenAI reasoning MoE); `rozum models list` shows them,
`--all` adds the extended fallback catalog. Any HuggingFace/MLX spec works too.

**Cascade** — name several models and rozum routes frugally: the cheapest model
first, escalating to a stronger one only when the answer isn't good enough.
rozum auto-orders them cheapest→most-capable and classifies local vs cloud:

```bash
# Repeatable --model (or one comma-separated value) makes a cascade:
rozum launch --model gpt-oss-20b --model claude-haiku-4-5 --model gpt-4o claude
rozum launch --model "mlx-community:gpt-oss-20b-MXFP4-Q4,claude-haiku-4-5" --strategy classify codex
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
Cargo.toml                  workspace + `rozum-gateway` engine binary
src/                        full CLI, config/sandbox/service, compatibility facade
crates/
├── rozum-cli/              thin user-facing `rozum` dispatcher
├── rozum-meet/             engine-free `rozum-meet` MCP frontend
├── rozum-meeting/          room runtime, persistence, bridges
├── rozum-gateway/          OpenAI/Anthropic serving and switchboard
├── rozum-core/             backend SPI, serving, admission, shared residency
├── rozum-models/           model catalog and Hugging Face integration
├── rozum-agent/            reference agent runtime and tool loop
├── rozum-{mlx,gguf,...}/   optional in-process engine adapters
└── rozum-{tui,web}/        operator frontends
clients/                    UCC and other browser clients
scripts/                    smoke, benchmark, deployment, and release helpers
```

## Development

```bash
# Portable workspace (Linux/macOS; no native model engine):
cargo build --workspace --no-default-features --bins
cargo test --workspace --no-default-features --lib

# Shipped macOS defaults (native MLX + every ported model family):
cargo build --workspace --bins
cargo test --workspace --lib
```

The shipped default feature set is `mlx-native + all-models`; it needs the macOS
Metal toolchain. GGUF/llama.cpp is opt-in with `--features gguf`. The durable
workspace builds without either engine under `--no-default-features`.

The `ci` workflow gates shipped defaults and every workspace library on macOS,
the whole no-default workspace on Linux, and an explicit portable-package
allow-list on Windows. Native Windows meeting/control/service seams remain out
of scope and are not presented as supported by that job.

This repository uses
[agent-plugins](vendor/agent-plugins) as a git submodule for
`multi-agent` (coordination across feature-branch agents) and `spec-dev`
(spec-before-code workflow). See [AGENTS.md](AGENTS.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
