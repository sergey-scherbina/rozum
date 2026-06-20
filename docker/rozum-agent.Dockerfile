# rozum-agent — the container image for the Docker sandbox backend.
#
# It is what `ROZUM_SANDBOX_BACKEND=docker rozum launch <agent>` runs the agent
# inside (docs/specs/model-sandbox.md). It carries everything a coding agent needs
# to BUILD — a Rust toolchain (from the `rust` base), git, and Node.js — plus the
# three agent CLIs (claude / codex / opencode) on PATH.
#
# The agent talks ONLY to the rozum gateway on the HOST, reached at
# `host.docker.internal` — rozum wires that env (ANTHROPIC_/OPENAI_ base URLs,
# auth=rozum-local) into the container at launch, so the image needs no creds and
# the container can be fully ephemeral (`--rm`). Writes are confined to the mounted
# workspace; the rest of the host filesystem is simply not present.
#
# Build:  scripts/build-agent-image.sh          (or the docker build below)
#         docker build -t rozum-agent:latest -f docker/rozum-agent.Dockerfile .
# Use:    ROZUM_SANDBOX_BACKEND=docker rozum launch claude
#         (override the image with ROZUM_SANDBOX_DOCKER_IMAGE=<name>)

# Pinned-major Rust on Debian slim (multi-arch: pulls arm64 on Apple Silicon).
FROM rust:1-slim-bookworm

# Build essentials + git + curl/ca-certs (for the NodeSource repo) + TLS headers
# (many crates link OpenSSL). One layer, apt lists dropped to keep the image lean.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      git curl ca-certificates build-essential pkg-config libssl-dev \
 && rm -rf /var/lib/apt/lists/*

# Node.js 22 LTS — the agent CLIs need Node >= 18; Debian's own is too old.
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
 && apt-get install -y --no-install-recommends nodejs \
 && rm -rf /var/lib/apt/lists/*

# The agent CLIs on PATH. Versions float with the registry; pass --build-arg to
# pin if a run must be reproducible. claude is env/flag-driven and works as-is
# against the gateway; codex/opencode have caveats under Docker (model-sandbox.md).
ARG CLAUDE_PKG=@anthropic-ai/claude-code
ARG CODEX_PKG=@openai/codex
ARG OPENCODE_PKG=opencode-ai
RUN npm install -g "$CLAUDE_PKG" "$CODEX_PKG" "$OPENCODE_PKG" \
 && npm cache clean --force

# The `rust` base puts cargo/rustc on PATH via ENV, which covers a directly-exec'd
# agent and its non-login child shells. But agents often run build commands through
# a LOGIN shell (`bash -lc`), which sources /etc/profile and resets PATH — dropping
# cargo. Put the toolchain on the login PATH too so `cargo build` works no matter how
# the agent spawns it.
RUN printf 'export PATH=%s:$PATH\n' "$CARGO_HOME/bin" > /etc/profile.d/rust.sh

# No bundled colors / pager noise in a non-interactive jail.
ENV CARGO_TERM_COLOR=never \
    GIT_PAGER=cat \
    PAGER=cat

# rozum sets `-w <workspace>` at launch (the mounted host cwd); this is only a
# sane default for a bare `docker run`.
WORKDIR /work
CMD ["bash"]
