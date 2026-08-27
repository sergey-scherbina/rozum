#!/usr/bin/env bash
# Install or update the `rozum` command (its `rozum-gateway` engine + `rozum` CLI
# dispatcher) to ~/.cargo/bin.
#
# This is a thin entry point — `scripts/install-bins.sh` already does the real work
# (build, exec-check the fresh binary before publishing, atomic rename, restart any
# launchd job pointing at the replaced path) with lessons paid for in BUGS.md. Read
# that script's header before touching either file: duplicating its logic here is
# exactly how a second, divergent install path would start.
#
#   ./install.sh              # rozum-gateway + rozum (the `rozum` command)
#   ./install.sh nadia        # any binary scripts/install-bins.sh knows about
#   DEST=~/.rozum/bin ./install.sh rozum-gateway
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [ $# -eq 0 ]; then
  set -- rozum-gateway rozum
fi

exec "$ROOT/scripts/install-bins.sh" "$@"
