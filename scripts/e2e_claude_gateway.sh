#!/usr/bin/env bash
# E2E smoke: drive a real (headless) Claude Code against the rozum gateway backed
# by a local MLX model, have it create + build a tiny Rust project, and verify the
# result independently. Spec: docs/e2e/claude-gateway-smoke.md
#
# Usage:  scripts/e2e_claude_gateway.sh [task]
#   task = build (default) | test    — see the spec for what each asks.
# Env:  ROZUM=<rozum binary>  MODEL=<spec>  PORT=<n>  KEEP=1 (keep workdir)
set -uo pipefail

ROZUM="${ROZUM:-$(cd "$(dirname "$0")/.." && pwd)/target/release/rozum}"
MODEL="${MODEL:-mlx-community:Qwen3.6-35B-A3B-4bit}"
PORT="${PORT:-8400}"
TASK="${1:-build}"
WORK="$(mktemp -d "/tmp/rozum-e2e-XXXXXX")"

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
cleanup() { kill "${GW:-0}" 2>/dev/null; [ "${KEEP:-0}" = 1 ] && echo "kept: $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT

say "workdir $WORK  | model $MODEL  | task $TASK"
[ -x "$ROZUM" ] || { echo "FAIL: rozum binary not found at $ROZUM"; exit 2; }
command -v claude >/dev/null || { echo "FAIL: claude CLI not on PATH"; exit 2; }
command -v cargo  >/dev/null || { echo "FAIL: cargo not on PATH"; exit 2; }

say "launch gateway"
"$ROZUM" gateway --port "$PORT" --model "$MODEL" >"$WORK/gateway.log" 2>&1 &
GW=$!
for i in $(seq 1 90); do curl -s -m2 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 && break; sleep 1; done
curl -s -m2 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 || { echo "FAIL: gateway not ready"; tail "$WORK/gateway.log"; exit 1; }
ALIAS="$(curl -s "http://127.0.0.1:$PORT/v1/models" | python3 -c 'import sys,json;print(json.load(sys.stdin)["data"][0]["id"])')"
echo "ready; model alias = $ALIAS"

# Redirect Claude Code at the local gateway (what `rozum launch` does under the hood).
export ANTHROPIC_BASE_URL="http://127.0.0.1:$PORT"
export ANTHROPIC_API_KEY="rozum-local"
export ANTHROPIC_MODEL="$ALIAS"
export ANTHROPIC_DEFAULT_SONNET_MODEL="$ALIAS"
cd "$WORK"

case "$TASK" in
  build)
    PROMPT='Create a minimal Rust binary project in the current directory: a Cargo.toml (package name "reverse-cli", edition 2021, no dependencies) and src/main.rs. The program reverses its first command-line argument (by characters) and prints the result. Then run "cargo run -- hello" and confirm it prints "olleh". Keep it minimal.' ;;
  test)
    PROMPT='In the CURRENT directory (do NOT create a subdirectory), create a minimal Rust BINARY project: a Cargo.toml (package "reverse-cli", edition 2021, no dependencies) and src/main.rs. Implement `fn reverse(s: &str) -> String` that reverses by characters; main reads its first CLI argument and prints reverse(arg). ALSO add a `#[cfg(test)]` unit test in src/main.rs asserting `reverse("hello") == "olleh"`. Then run "cargo test" (must pass) and "cargo run -- hello" (must print olleh). Do NOT just scaffold a default project — actually implement reverse. Keep it minimal.' ;;
  *) echo "unknown task: $TASK"; exit 2 ;;
esac

say "claude -p (headless agentic run; bounded by --max-turns)"
START=$(date +%s)
# stream-json: observable in real time (survives a timeout); --max-turns bounds the
# loop so a meandering local model can't run forever.
timeout "${E2E_TIMEOUT:-600}" claude -p "$PROMPT" --model "$ALIAS" \
  --dangerously-skip-permissions --max-turns "${E2E_MAX_TURNS:-20}" \
  --output-format stream-json --verbose >"$WORK/claude.jsonl" 2>&1
RC=$?
ELAPSED=$(( $(date +%s) - START ))
# Summarize the agentic run from the event stream.
python3 - "$WORK/claude.jsonl" <<'PY'
import sys, json
asst=tools=0; result=None; stop=None
for line in open(sys.argv[1]):
    line=line.strip()
    if not line: continue
    try: e=json.loads(line)
    except Exception: continue
    t=e.get("type")
    if t=="assistant":
        asst+=1
        for b in e.get("message",{}).get("content",[]):
            if b.get("type")=="tool_use": tools+=1
    elif t=="result":
        result=e.get("subtype"); stop=e.get("num_turns")
print(f"  assistant msgs={asst}  tool_uses={tools}  result={result}  num_turns={stop}")
PY
echo "(claude rc=$RC, ${ELAPSED}s)"

say "VERIFY (independent of claude)"
fail=0
ls -R "$WORK" | grep -vE 'gateway.log|claude.jsonl|cargo.err|run.err' | head -20
[ -f Cargo.toml ] && echo "PASS  Cargo.toml exists" || { echo "FAIL  Cargo.toml missing"; fail=1; }
ls src/*.rs >/dev/null 2>&1 && echo "PASS  src/*.rs exists" || { echo "FAIL  no src/*.rs"; fail=1; }
if [ "$TASK" = test ]; then
  if cargo test -q 2>"$WORK/cargo.err"; then echo "PASS  cargo test green"; else echo "FAIL  cargo test"; sed -n '1,15p' "$WORK/cargo.err"; fail=1; fi
fi
# Behavior check (un-fakeable): both tasks ask for a binary that reverses, so this
# verifies the actual implementation — a scaffold-only run fails here as it should.
OUT="$(cargo run -q -- hello 2>"$WORK/run.err")"
if [ "$OUT" = "olleh" ]; then echo "PASS  cargo run -- hello -> olleh"; else echo "FAIL  cargo run -- hello -> '$OUT'"; sed -n '1,15p' "$WORK/run.err"; fail=1; fi

say "RESULT: $([ $fail = 0 ] && echo '✅ PASS' || echo '❌ FAIL')  (workdir: $WORK)"
exit $fail
