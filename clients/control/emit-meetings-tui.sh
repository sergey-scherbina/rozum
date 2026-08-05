#!/usr/bin/env bash
#
# Regenerate the vendored terminal meeting client from clients/control/meetings.ssc.
#
# The crate is CHECKED IN so that building rozum needs cargo and nothing else — no ScalaScript
# toolchain, on any platform, in CI. `ucc-meetings-dual-target.sh` proves a fresh emission is
# byte-identical to what is committed, so the vendored copy cannot drift from its source.
#
# The environment below is fixed ON PURPOSE, and each value is a decision:
#
#   ROZUM_MEETING_TOKEN   deliberately UNSET. `env()` on this target resolves in the EMITTING
#                         process, so a value here would be compiled into the binary — the whole
#                         reason the client declares a credential instead. The gate fails if a
#                         token-shaped string ever appears in the generated source.
#   ROZUM_MEETING_BASE    the local daemon. A shipped binary talks to the daemon on the machine it
#                         runs on; every other url it uses comes ready-made from that daemon's own
#                         responses, built from the request's origin.
#   ROZUM_MEETING_DATE    `today`, a symbolic date the daemon resolves. A real date would be the day
#                         the binary was BUILT, and the transcript would be empty forever after.
#   ROZUM_MEETING_ROOM    the project room; the picker changes it at runtime.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
APP="$HERE/meetings.ssc"
OUT="${1:-$ROOT/crates/rozum-meeting-tui}"

if [[ -z "${SSC_TOOLS:-}" ]]; then
  MAIN="$(git -C "$ROOT" worktree list | head -1 | awk '{print $1}')"
  for cand in "$ROOT/../scalascript/bin/ssc-tools" "$MAIN/../scalascript/bin/ssc-tools"; do
    [[ -x "$cand" ]] && { SSC_TOOLS="$cand"; break; }
  done
fi
[[ -x "${SSC_TOOLS:-}" ]] || { echo "FAIL: set SSC_TOOLS=/path/to/bin/ssc-tools" >&2; exit 1; }

WORK="$(mktemp -d "${TMPDIR:-/tmp}/rozum-emit-meetings.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

(
  cd "$WORK"
  unset ROZUM_MEETING_TOKEN
  ROZUM_MEETING_BASE="http://127.0.0.1:8401" \
  ROZUM_MEETING_ROOM="rozum" \
  ROZUM_MEETING_DATE="today" \
  "$SSC_TOOLS" run --v1 "$APP" >/dev/null
)

[[ -f "$WORK/tui-out/src/main.rs" ]] || { echo "FAIL: emission produced no crate" >&2; exit 1; }

mkdir -p "$OUT/src"
cp "$WORK/tui-out/src/main.rs" "$OUT/src/main.rs"
cp "$WORK/tui-out/Cargo.toml"  "$OUT/Cargo.toml.emitted"

echo "emitted → $OUT (src/main.rs + Cargo.toml.emitted)"
