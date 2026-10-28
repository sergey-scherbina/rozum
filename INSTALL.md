# Installing rozum

## Quick start: a Mac with Apple Silicon

Every step is a command to paste in Terminal. Nothing here needs an AI to help.

1. **The tools, once.** Apple's command-line tools and Metal toolchain, and Rust:

   ```bash
   xcode-select --install                          # if it says they are installed already, go on
   xcodebuild -downloadComponent MetalToolchain
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # accept the defaults; then open a new Terminal
   ```

2. **rozum itself.** About 3 minutes on an M4 Max, up to 20 the first time (it builds MLX):

   ```bash
   git clone --recurse-submodules https://github.com/sergey-scherbina/rozum.git ~/rozum
   cd ~/rozum && ./install.sh
   ```

   `install.sh` builds `rozum` and `rozum-gateway`, checks that the new programs start, and puts them in
   `~/.cargo/bin`. **Keep `~/rozum`**: the programs read their GPU kernels (`mlx.metallib`) from it. To update:
   `cd ~/rozum && git pull --ff-only && git submodule update --init --recursive && ./install.sh`.

3. **Check it:** `rozum --help`, then `rozum doctor`.

4. **A model that fits your Mac.** The recommended ones, and whether one fits your free memory now:

   ```bash
   rozum models list --remote                      # --all for smaller ones (16 GB Macs)
   rozum gateway --model mlx-community:Qwen3.6-35B-A3B-4bit --dry-run
   ```

   The verdict says if it would load; if it would not, close big apps (an IDE, a browser) or take a smaller model.
   Qwen3.6-35B-A3B wants about 24 GB free; gpt-oss-20b about 14 GB.

5. **Work with it.** In your project, start a coding agent on the local model — the first time downloads it
   (12–17 GB):

   ```bash
   cd ~/work/my-project
   rozum launch --offline --model mlx-community:Qwen3.6-35B-A3B-4bit -- claude
   ```

   `--offline`: no cloud at all. The agent runs in rozum's sandbox: it writes only in the project, cannot read your
   keys, and reaches only the local model. Codex and opencode work the same way (`-- codex`, `-- opencode`).

6. **From your phone:** [okay-remote](https://github.com/okay-effects/okay-remote) runs these agents on this Mac and
   shows them on your phone; with rozum installed, its **Local · …** agents use your model.

The rest of this page is the full reference: other hosts, engines, the Cargo way.

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
git clone --recurse-submodules https://github.com/sergey-scherbina/rozum.git rozum
cd rozum

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

For a fuller check — and the one to run when something behaves oddly later:

```bash
rozum doctor                # read-only readiness report for the local demo path
rozum doctor --services     # also: every com.rozum.* job, and whether its endpoint answers
```

The second form matters because a launchd/systemd job that cannot exec looks
exactly like a healthy one in `launchctl list` until something probes the
endpoint it exists to serve. `rozum services` prints what this build declares:
each label, the binary it should run, and how it is probed.

If you plan to serve a model, check it fits before you rely on it:

```bash
rozum gateway --model <spec> --dry-run
```

RAM is admitted, not hoped for — see [docs/models.md](docs/models.md) for the
levers when a model does not fit.

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
