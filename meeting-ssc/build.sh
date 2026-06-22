#!/usr/bin/env bash
# Build the pure-.ssc rozum meeting server to a stable binary.
# Requires the scalascript toolchain (bin/ssc) built via `./install.sh --dev`.
set -euo pipefail
SSC_ROOT="${SSC_ROOT:-/Users/sergiy/work/my/scalascript}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$HOME/.local/bin/rozum-meeting-ssc}"
export PATH="$PATH:$SSC_ROOT/bin"
ssc build-rust "$HERE/meeting.ssc" -o "$OUT"
echo "built: $OUT"
