# Installing rozum

## Prerequisites

- **Rust 1.85+** with the 2024 edition (`rustup default stable` is fine on any
  recent Rust release).
- **Git** with submodule support.
- A **POSIX-y operating system** (Linux, macOS, WSL). `rozum` uses Unix-domain
  sockets and `$XDG_RUNTIME_DIR` / `$XDG_STATE_HOME` paths; native Windows is
  not currently supported.

Optional, only if you plan to enable the `local-models` feature:

- A C/C++ toolchain (`build-essential` on Debian/Ubuntu, Xcode CLT on macOS).
- Disk space for a tiny model file (a few hundred MB for the smallest GGUFs).

## Build

```bash
git clone <repo-url> rozum
cd rozum
git submodule update --init --recursive
cargo build --release
```

The release binary lands at `target/release/rozum`. For convenience, either:

```bash
cargo install --path .                  # installs into ~/.cargo/bin/rozum
# or
sudo install -m 0755 target/release/rozum /usr/local/bin/rozum
```

For development, `cargo run -- <args>` is equivalent to `rozum <args>` and
recompiles on each invocation.

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

## Optional features

```bash
cargo build --release --features local-models
```

Enables in-process Candle inference backends. Default builds skip this — the
default product (meeting rooms, MCP proxy, TUI, web bridge) needs no model
files and no GPU.

## Updating

```bash
git pull --ff-only
git submodule update --init --recursive
cargo build --release
```

The submodule (`vendor/agent-plugins`) provides `multi-agent` and `spec-dev`
agent skills used by the project's contribution workflow; it is harmless to
ignore if you are only running `rozum` and not contributing to it.

## Troubleshooting

| Symptom                                  | Likely cause / fix                                                   |
|------------------------------------------|----------------------------------------------------------------------|
| `rozum list` finds no rooms              | `XDG_RUNTIME_DIR` not set, or the socket path is `/tmp/rozum/`        |
| `error: linker 'cc' not found`           | Install a C toolchain (`build-essential`, Xcode CLT)                  |
| `rmcp` / `candle` fail to build          | Use Rust ≥ 1.85; clear `target/` and rebuild                          |
| Telegram/Discord bridges fail to start   | `TELEGRAM_BOT_TOKEN` / `DISCORD_BOT_TOKEN` env var is missing         |
| `Transport closed` from an agent's MCP   | The room process died — the proxy retries for ~18 s; restart `rozum`  |

See [USER_MANUAL.md](USER_MANUAL.md) for runtime configuration and bridges.
