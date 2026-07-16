# Installing rozum

## Prerequisites

- **Rust via rustup.** The checked-in `rust-toolchain.toml` pins
  `nightly-2026-06-09` for the vendored Candle/ARM build; rustup installs and selects it
  automatically when Cargo runs in this checkout.
- **Git** with submodule support.
- A **POSIX-y operating system** (Linux, macOS, WSL). `rozum` uses Unix-domain
  sockets and `$XDG_RUNTIME_DIR` / `$XDG_STATE_HOME` paths; native Windows is
  not currently supported.

For the shipped native-MLX build on macOS:

- Xcode Command Line Tools plus the Metal Toolchain component.
- A C/C++ toolchain and enough disk space for MLX build artifacts and model files.

## Build

Run the command for your host:

```bash
git clone <repo-url> rozum
cd rozum
git submodule update --init --recursive

# macOS: shipped defaults (native MLX + all ported model families)
cargo build --release --workspace --bins

# Linux/WSL/macOS: durable host without a native model engine
cargo build --release --workspace --bins --no-default-features
```

The workspace produces three public entrypoints in `target/release/`:

- `rozum` — a thin dispatcher;
- `rozum-gateway` — the full CLI, meeting host, gateway, launcher, and model engines;
- `rozum-meet` — the engine-free MCP proxy/HTTP frontend.

Keep all three next to each other or on the same `PATH`; the dispatcher resolves
its siblings beside itself first. To install through Cargo, choose the appropriate
root command for the host and then install both thin frontends:

```bash
# macOS shipped defaults (use this OR the portable root command below)
cargo install --path . --locked

# Linux/WSL/engine-free macOS
cargo install --path . --locked --no-default-features

# all hosts: install the dispatcher and MCP frontend
cargo install --path crates/rozum-cli --locked
cargo install --path crates/rozum-meet --locked
```

For development, `cargo run --bin rozum-gateway -- <args>` runs the full CLI
directly. After a workspace build, `cargo run -p rozum-cli --bin rozum -- <args>`
exercises the sibling-dispatch path.

## Verify

```bash
rozum --help
rozum --topic "smoke test" &     # backgrounded room
rozum list                       # should print one row
kill %1                          # tear it down
```

`rozum list` reads the active Unix sockets from
`$XDG_RUNTIME_DIR/rozum/` (Linux) or `$TMPDIR/rozum/` (macOS fallback). If
`list` shows nothing while a `rozum` is running, check that the directory
exists and is writable.

## Model-engine features

```bash
# GGUF/llama.cpp is opt-in (and can be built without the default MLX engine):
cargo build --release --no-default-features --features gguf --bin rozum-gateway

# Lean native MLX: runtime + Qwen core, without every optional model family:
cargo build --release --no-default-features --features mlx-native --bin rozum-gateway
```

The shipped macOS default is `mlx-native + all-models`. GGUF and mistralrs stay
opt-in. Meeting rooms, MCP, HTTP backends, and the agent/cascade core build with
`--no-default-features` and require no model files or GPU.

## Updating

After pulling, run the build command for your host:

```bash
git pull --ff-only
git submodule update --init --recursive
# macOS shipped defaults:
cargo build --release --workspace --bins
# Linux/WSL engine-free host:
cargo build --release --workspace --bins --no-default-features
```

The submodule (`vendor/agent-plugins`) provides `multi-agent` and `spec-dev`
agent skills used by the project's contribution workflow; it is harmless to
ignore if you are only running `rozum` and not contributing to it.

## Troubleshooting

| Symptom                                  | Likely cause / fix                                                   |
|------------------------------------------|----------------------------------------------------------------------|
| `rozum list` finds no rooms              | `XDG_RUNTIME_DIR` not set, or the socket path is `/tmp/rozum/`        |
| `error: linker 'cc' not found`           | Install a C toolchain (`build-essential`, Xcode CLT)                  |
| `rmcp` / `candle` fail to build          | Install the pinned rustup toolchain; clear `target/` and rebuild       |
| Telegram/Discord bridges fail to start   | `TELEGRAM_BOT_TOKEN` / `DISCORD_BOT_TOKEN` env var is missing         |
| `Transport closed` from an agent's MCP   | The room process died — the proxy retries for ~18 s; restart `rozum`  |

See [USER_MANUAL.md](USER_MANUAL.md) for runtime configuration and bridges.
