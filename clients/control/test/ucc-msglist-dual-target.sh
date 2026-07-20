#!/usr/bin/env bash
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTROL_DIR="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$CONTROL_DIR/../.." && pwd)"
APP="$CONTROL_DIR/meeting-message-list.ssc"
FIXTURE="$HERE/ucc-msglist-fixture.py"

SSC_TOOLS="${SSC_TOOLS:-$ROOT/../scalascript/bin/ssc-tools}"
if [[ ! -x "$SSC_TOOLS" ]]; then
  echo "FAIL: ScalaScript CLI not executable: $SSC_TOOLS" >&2
  echo "Set SSC_TOOLS=/absolute/path/to/bin/ssc-tools" >&2
  exit 1
fi
for tool in python3 curl cargo; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "FAIL: required tool is missing: $tool" >&2
    exit 1
  fi
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/rozum-ucc-msglist.XXXXXX")"
FIXTURE_PID=""
cleanup() {
  if [[ -n "$FIXTURE_PID" ]] && kill -0 "$FIXTURE_PID" 2>/dev/null; then
    kill "$FIXTURE_PID" 2>/dev/null || true
    wait "$FIXTURE_PID" 2>/dev/null || true
  fi
  rm -rf "$TMP"
}
trap cleanup EXIT

FIXTURE_LOG="$TMP/fixture.log"
python3 -u "$FIXTURE" --port 0 >"$FIXTURE_LOG" 2>&1 &
FIXTURE_PID=$!

PORT=""
for _ in {1..100}; do
  PORT="$(sed -n 's/^PORT=//p' "$FIXTURE_LOG" | head -1)"
  [[ -n "$PORT" ]] && break
  if ! kill -0 "$FIXTURE_PID" 2>/dev/null; then
    echo "FAIL: fixture exited before publishing its port" >&2
    cat "$FIXTURE_LOG" >&2
    exit 1
  fi
  sleep 0.05
done
if [[ -z "$PORT" ]]; then
  echo "FAIL: fixture did not publish a port" >&2
  cat "$FIXTURE_LOG" >&2
  exit 1
fi

BASE="http://127.0.0.1:$PORT"
FEED="$(curl --fail --silent --show-error "$BASE/chat/messages?room=rozum")"
grep -Fq 'smoke-message' <<<"$FEED" || {
  echo "FAIL: deterministic message fixture returned an unexpected body: $FEED" >&2
  exit 1
}

WEB_OUT="$TMP/index.html"
ROZUM_UCC_BASE="$BASE" "$SSC_TOOLS" emit-spa --frontend react "$APP" >"$WEB_OUT"
[[ -s "$WEB_OUT" ]] || { echo "FAIL: React emission is empty" >&2; exit 1; }
grep -Fq 'rozum meeting messages' "$WEB_OUT" || {
  echo "FAIL: React artifact is missing the shared title" >&2
  exit 1
}
grep -Fq '/chat/messages?room=rozum' "$WEB_OUT" || {
  echo "FAIL: React artifact is missing the shared fetch binding" >&2
  exit 1
}
echo "PASS: React artifact emitted from meeting-message-list.ssc"

NATIVE_WORK="$TMP/native"
mkdir -p "$NATIVE_WORK"
(
  cd "$NATIVE_WORK"
  ROZUM_UCC_BASE="$BASE" "$SSC_TOOLS" run --v1 "$APP"
)
MANIFEST="$NATIVE_WORK/tui-out/Cargo.toml"
[[ -f "$MANIFEST" ]] || { echo "FAIL: TUI emission did not create $MANIFEST" >&2; exit 1; }

export CARGO_TARGET_DIR="$TMP/cargo-target"
cargo build --quiet --manifest-path "$MANIFEST"
SNAPSHOT="$(SSC_TUI_SNAPSHOT=1 cargo run --quiet --manifest-path "$MANIFEST")"
grep -Fq 'rozum meeting messages' <<<"$SNAPSHOT" || {
  echo "FAIL: TUI snapshot is missing the shared title" >&2
  printf '%s\n' "$SNAPSHOT" >&2
  exit 1
}
grep -Fq 'smoke-agent' <<<"$SNAPSHOT" || {
  echo "FAIL: TUI snapshot is missing the fetched author" >&2
  printf '%s\n' "$SNAPSHOT" >&2
  exit 1
}
grep -Fq 'smoke-message' <<<"$SNAPSHOT" || {
  echo "FAIL: TUI snapshot is missing the fetched message" >&2
  printf '%s\n' "$SNAPSHOT" >&2
  exit 1
}
echo "PASS: ratatui crate built and rendered fetched rows from the same source"
echo "PASS: ucc-poc-msglist dual-target smoke"
