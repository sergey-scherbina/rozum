#!/usr/bin/env bash
# E2E smoke: drive a real (headless) OpenAI Codex CLI against the rozum gateway
# backed by a local MLX model, same tasks as the Claude runner. Spec:
# docs/e2e/claude-gateway-smoke.md (covers both agents).
#
# Codex CLI ≥ 0.137 dropped `wire_api="chat"` and REQUIRES the OpenAI Responses API
# (POST /v1/responses). The gateway now implements it (`responses_handler`), so Codex
# connects via `wire_api="responses"`. (Codex also ignores OPENAI_BASE_URL, so we set
# an explicit `-c model_provider`.) The script preflights /v1/responses and aborts
# with a clear message if it's missing (an older gateway binary).
#
# Usage:  scripts/e2e_codex_gateway.sh [build|test]
# Env:  ROZUM=<rozum binary>  MODEL=<spec>  PORT=<n>  KEEP=1
set -uo pipefail

ROZUM="${ROZUM:-$(cd "$(dirname "$0")/.." && pwd)/target/release/rozum}"
MODEL="${MODEL:-mlx-community:Qwen3.6-35B-A3B-4bit}"
PORT="${PORT:-8420}"
TASK="${1:-build}"
WORK="$(mktemp -d "/tmp/rozum-e2e-codex-XXXXXX")"

say() { printf '\n\033[1m== %s ==\033[0m\n' "$*"; }
cleanup() { kill "${GW:-0}" 2>/dev/null; [ "${KEEP:-0}" = 1 ] && echo "kept: $WORK" || rm -rf "$WORK"; }
trap cleanup EXIT

say "workdir $WORK  | model $MODEL  | task $TASK | agent codex"
[ -x "$ROZUM" ] || { echo "FAIL: rozum binary not found at $ROZUM"; exit 2; }
command -v codex >/dev/null || { echo "FAIL: codex CLI not on PATH"; exit 2; }
command -v cargo >/dev/null || { echo "FAIL: cargo not on PATH"; exit 2; }

say "launch gateway"
"$ROZUM" gateway --port "$PORT" --model "$MODEL" >"$WORK/gateway.log" 2>&1 &
GW=$!
for i in $(seq 1 90); do curl -s -m2 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 && break; sleep 1; done
curl -s -m2 "http://127.0.0.1:$PORT/v1/models" >/dev/null 2>&1 || { echo "FAIL: gateway not ready"; exit 1; }
echo "ready"

# Preflight: Codex needs the Responses API. A 1-token request both confirms the
# endpoint exists (not 404 → not an old binary) and that it returns a response.
RC_CODE=$(curl -s -o /dev/null -w '%{http_code}' -m30 "http://127.0.0.1:$PORT/v1/responses" \
  -H 'Content-Type: application/json' \
  -d '{"model":"x","input":"hi","max_output_tokens":1,"stream":false}')
if [ "$RC_CODE" = 404 ]; then
  say "❌ gateway has no /v1/responses (HTTP 404) — rebuild with the responses-api change"
  exit 3
fi
echo "preflight /v1/responses -> HTTP $RC_CODE"

case "$TASK" in
  build) PROMPT='Create a minimal Rust binary project in the current directory: a Cargo.toml (package name "reverse-cli", edition 2021, no dependencies) and src/main.rs. The program reverses its first command-line argument (by characters) and prints the result. Then run "cargo run -- hello" and confirm it prints "olleh". Keep it minimal.' ;;
  test)  PROMPT='In the CURRENT directory (do NOT create a subdirectory), create a minimal Rust BINARY project: a Cargo.toml (package "reverse-cli", edition 2021, no dependencies) and src/main.rs. Implement `fn reverse(s: &str) -> String` that reverses by characters; main reads its first CLI argument and prints reverse(arg). ALSO add a `#[cfg(test)]` unit test in src/main.rs asserting `reverse("hello") == "olleh"`. Then run "cargo test" (must pass) and "cargo run -- hello" (must print olleh). Do NOT just scaffold a default project — actually implement reverse. Keep it minimal.' ;;
  *) echo "unknown task: $TASK"; exit 2 ;;
esac

export OPENAI_API_KEY=rozum-local
say "codex exec (headless agentic run)"
START=$(date +%s)
timeout "${E2E_TIMEOUT:-600}" codex exec "$PROMPT" -m local \
  --dangerously-bypass-approvals-and-sandbox -C "$WORK" \
  -c model_provider=rozum \
  -c 'model_providers.rozum.name="rozum"' \
  -c "model_providers.rozum.base_url=\"http://127.0.0.1:$PORT/v1\"" \
  -c 'model_providers.rozum.wire_api="responses"' \
  -c 'model_providers.rozum.env_key="OPENAI_API_KEY"' >"$WORK/codex.out" 2>&1
echo "(codex rc=$?, $(( $(date +%s) - START ))s)"; tail -8 "$WORK/codex.out"

say "VERIFY (independent of codex)"
fail=0; cd "$WORK"
ls -R "$WORK" | grep -vE 'gateway.log|codex.out|cargo.err|run.err' | head -20
[ -f Cargo.toml ] && echo "PASS  Cargo.toml exists" || { echo "FAIL  Cargo.toml missing"; fail=1; }
ls src/*.rs >/dev/null 2>&1 && echo "PASS  src/*.rs exists" || { echo "FAIL  no src/*.rs"; fail=1; }
[ "$TASK" = test ] && { cargo test -q 2>"$WORK/cargo.err" && echo "PASS  cargo test green" || { echo "FAIL  cargo test"; fail=1; }; }
if [ -f src/main.rs ] || grep -qE '\[\[bin\]\]' Cargo.toml 2>/dev/null; then
  OUT="$(cargo run -q -- hello 2>"$WORK/run.err")"
  [ "$OUT" = olleh ] && echo "PASS  cargo run -- hello -> olleh" || { echo "FAIL  cargo run -> '$OUT'"; fail=1; }
elif [ "$TASK" = build ]; then echo "FAIL  no bin target (build needs one)"; fail=1
else echo "SKIP  cargo run (lib-only crate; test green is the goal)"; fi

say "RESULT: $([ $fail = 0 ] && echo '✅ PASS' || echo '❌ FAIL')  (workdir: $WORK)"
exit $fail
