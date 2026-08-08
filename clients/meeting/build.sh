#!/usr/bin/env bash
# Build the pure-.ssc rozum meeting server to a stable binary.
# Requires the scalascript toolchain (bin/ssc) built via `./install.sh --dev`.
set -euo pipefail
SSC_ROOT="${SSC_ROOT:-/Users/sergiy/work/my/scalascript}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$HOME/.local/bin/rozum-meeting-ssc}"
export PATH="$PATH:$SSC_ROOT/bin"
# `build-rust` moved to the OPTIONAL tools/compatibility tier — the standard `bin/ssc` now refuses
# it ("requires the optional ScalaScript tools/compatibility tier; run ssc-tools explicitly"),
# exactly as `emit-spa` did to the UCC deploy on 2026-07-13. Prefer `ssc-tools` when it exists.
SSC_BIN="ssc"
[ -x "$SSC_ROOT/bin/ssc-tools" ] && SSC_BIN="$SSC_ROOT/bin/ssc-tools"
"$SSC_BIN" build-rust "$HERE/meeting.ssc" -o "$OUT"
echo "built: $OUT"
