#!/usr/bin/env bash
#
# Stage A gate for `ucc-meetings-in-tk`: ONE .ssc source (clients/control/meetings.ssc) emits the
# React client and the ratatui client, and BOTH render the meeting daemon's transcript envelope —
# including the derived `badge` column, which is the thing this stage adds over the read-only PoC.
#
# Deliberately runs against an isolated fixture on an OS-assigned port and touches no operator
# service (:8089, :8401, :8411 are never contacted).
#
# The fixture now runs with --require-auth, and that flag is the whole point of this gate. Until
# scalascript honoured the headers signal on the terminal target, an emitted binary sent a bare GET
# and every daemon route answers 401 — so a client could look correct and read nothing. The fixture
# answers 401 without an Authorization header, which means a regression there turns this gate red
# instead of quietly emptying the transcript.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONTROL_DIR="$(cd "$HERE/.." && pwd)"
ROOT="$(cd "$CONTROL_DIR/../.." && pwd)"
APP="$CONTROL_DIR/meetings.ssc"
FIXTURE="$HERE/ucc-meetings-fixture.py"

# In a worktree, $ROOT/../scalascript does not resolve (the worktree lives under .worktrees/),
# so fall back to the checkout next to the MAIN repo before giving up.
if [[ -z "${SSC_TOOLS:-}" ]]; then
  MAIN="$(git -C "$ROOT" worktree list | head -1 | awk '{print $1}')"
  for cand in "$ROOT/../scalascript/bin/ssc-tools" "$MAIN/../scalascript/bin/ssc-tools"; do
    [[ -x "$cand" ]] && { SSC_TOOLS="$cand"; break; }
  done
fi
SSC_TOOLS="${SSC_TOOLS:-}"
if [[ ! -x "$SSC_TOOLS" ]]; then
  echo "FAIL: ScalaScript CLI not executable: ${SSC_TOOLS:-<unset>}" >&2
  echo "Set SSC_TOOLS=/absolute/path/to/bin/ssc-tools" >&2
  exit 1
fi
for tool in python3 curl cargo; do
  command -v "$tool" >/dev/null 2>&1 || { echo "FAIL: required tool is missing: $tool" >&2; exit 1; }
done

TMP="$(mktemp -d "${TMPDIR:-/tmp}/rozum-ucc-meetings.XXXXXX")"
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
python3 -u "$FIXTURE" --port 0 --require-auth >"$FIXTURE_LOG" 2>&1 &
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
[[ -n "$PORT" ]] || { echo "FAIL: fixture did not publish a port" >&2; cat "$FIXTURE_LOG" >&2; exit 1; }
ROOM="$(sed -n 's/^ROOM=//p' "$FIXTURE_LOG" | head -1)"
DATE="$(sed -n 's/^DATE=//p' "$FIXTURE_LOG" | head -1)"

BASE="http://127.0.0.1:$PORT"
TOKEN="smoke-token"

# The fixture must actually refuse an unauthenticated read — otherwise the auth assertion below
# proves nothing, and this gate would pass with the headers support ripped out again.
UNAUTH_CODE="$(curl --silent --output /dev/null --write-out '%{http_code}' "$BASE/rooms/$ROOM/messages/$DATE")"
[[ "$UNAUTH_CODE" == "401" ]] || { echo "FAIL: fixture answered $UNAUTH_CODE unauthenticated, expected 401" >&2; exit 1; }

FEED="$(curl --fail --silent --show-error -H "Authorization: Bearer $TOKEN" "$BASE/rooms/$ROOM/messages/$DATE")"
for needle in 'hello' '"badge"' '[ALERT]' '"messages"'; do
  grep -Fq "$needle" <<<"$FEED" || {
    echo "FAIL: fixture body is missing $needle: $FEED" >&2
    exit 1
  }
done

export ROZUM_MEETING_BASE="$BASE" ROZUM_MEETING_ROOM="$ROOM" ROZUM_MEETING_DATE="$DATE"
# NOTE: setting the token at EMIT time is exactly what the client's own warning says not to do in
# production — ssc run --v1 folds env() into the generated Rust as a literal. It is correct HERE
# because the value is a throwaway fixture token and baking it is what lets the emitted binary
# authenticate without a runtime config. See `ui-fetch-credentials` upstream for the durable fix.
export ROZUM_MEETING_TOKEN="$TOKEN"

# ── web ───────────────────────────────────────────────────────────────────────
#
# The web assertions are PRESENCE checks, and that is a property of the target rather than a
# weak test. `env()` resolves at DIFFERENT times on the two targets: the terminal emit bakes the
# resolved URL in as a Rust string literal, while the React artifact keeps the program — the
# emitted JS still reads `_arith('+', "rozum · ", room)`, so the room and the base are runtime
# values in the browser and never appear literally in the file. Asserting `rozum · smoke-room`
# here would therefore fail no matter how correct the client is. What is checkable is that the
# shared view and the shared binding survived the emission; the BEHAVIOURAL proof (a real fetch,
# real rows) is the terminal half below, which is also where the daemon parity matters.
WEB_OUT="$TMP/index.html"
"$SSC_TOOLS" emit-spa --frontend react "$APP" >"$WEB_OUT"
[[ -s "$WEB_OUT" ]] || { echo "FAIL: React emission is empty" >&2; exit 1; }
for needle in '/rooms/' '/messages/' 'meetingsTranscript' 'meetingsRooms' 'badge' 'mentions'; do
  grep -Fq "$needle" "$WEB_OUT" || {
    echo "FAIL: React artifact lost the shared view/binding fragment: $needle" >&2
    exit 1
  }
done
echo "PASS: React artifact emitted from meetings.ssc (shared view + binding present)"

# ── terminal ──────────────────────────────────────────────────────────────────
NATIVE_WORK="$TMP/native"
mkdir -p "$NATIVE_WORK"
( cd "$NATIVE_WORK" && "$SSC_TOOLS" run --v1 "$APP" )
MANIFEST="$NATIVE_WORK/tui-out/Cargo.toml"
[[ -f "$MANIFEST" ]] || { echo "FAIL: TUI emission did not create $MANIFEST" >&2; exit 1; }
RUST_SOURCE="$NATIVE_WORK/tui-out/src/main.rs"
grep -Fq 'sig_int(signals, "meetingsRefresh")' "$RUST_SOURCE" || {
  echo "FAIL: emitted TUI dropped the shared refresh-tick dependency" >&2
  exit 1
}
grep -Fq 'refresh_fetches(&mut signals, &mut observed_fetch_ticks);' "$RUST_SOURCE" || {
  echo "FAIL: emitted TUI does not re-fetch before interactive redraws" >&2
  exit 1
}

export CARGO_TARGET_DIR="$TMP/cargo-target"
cargo build --quiet --manifest-path "$MANIFEST"
SNAPSHOT="$(SSC_TUI_SNAPSHOT=1 cargo run --quiet --manifest-path "$MANIFEST")"
# Reaching these rows AT ALL is the authenticated-read proof: the fixture refuses without a header.
for needle in 'agent' 'hello' 'db down' '[ALERT]' '12:34' "$ROOM" 'other-room'; do
  grep -Fq "$needle" <<<"$SNAPSHOT" || {
    echo "FAIL: TUI snapshot is missing $needle" >&2
    printf '%s\n' "$SNAPSHOT" >&2
    exit 1
  }
done
# The composer must be wired on the terminal target too — a TextInput that cannot submit is the
# exact failure tui-fetch-post fixed, and it is invisible in a snapshot.
grep -Fq 'fn send_action(' "$RUST_SOURCE" || {
  echo "FAIL: emitted TUI has no write path — the composer cannot submit" >&2
  exit 1
}
grep -Fq '"meetingsDraft"' "$RUST_SOURCE" || {
  echo "FAIL: the composer's draft signal was not seeded into the emitted store" >&2
  exit 1
}
# The picker: a selectable room table whose chosen row writes the ready-made url into the signal
# the transcript follows. Without the selection machinery the list renders and nothing happens.
for needle in 'fn is_table(' 'fn move_row(' 'row_field(' '"meetingsUrl"'; do
  grep -Fq "$needle" "$RUST_SOURCE" || {
    echo "FAIL: emitted TUI has no room picker — missing $needle" >&2
    exit 1
  }
done
echo "PASS: ratatui crate built and rendered the transcript — badge column included"
# Live arrival: without a clock the client only updates when a key is pressed, which is the one
# gap that kept attach.rs alive. A no-op bump helper means the interval was silently dropped.
grep -Fq 'fn bump_interval_ticks(signals:' "$RUST_SOURCE" || {
  echo "FAIL: emitted TUI has no clock — it would only refresh on a keypress" >&2
  exit 1
}
grep -Fq '"meetingsRefresh".to_string()' "$RUST_SOURCE" || {
  echo "FAIL: the refresh tick is not the one the clock advances" >&2
  exit 1
}
echo "PASS: authenticated read + composer + room picker + a clock that refreshes unattended"
echo "PASS: ucc-meetings-in-tk Stage A dual-target smoke"
