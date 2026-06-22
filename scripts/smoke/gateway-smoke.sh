#!/usr/bin/env bash
# Fast, deterministic gateway smoke — catch gateway regressions in SECONDS without the
# slow, flaky, reboot-prone full agentic matrix. One tiny model, single-stream, no agent
# CLI. Asserts the regressions that actually bite: liveness, schema validity, and
# DETERMINISM (the class of bug that made the matrix flip — fixed by seed-pinning).
#
# Why these checks and not tool-calling: a 0.6B model is below the agentic cliff, so a
# tool-call assertion would FLAKE (the model may not call the tool) — that's covered by
# unit tests (parse_tool_calls etc.). This smoke keeps only DETERMINISTIC assertions so a
# red is always a real gateway regression, never model variance.
#
# Usage:  scripts/smoke/gateway-smoke.sh
#   SMOKE_MODEL (default mlx-community/Qwen3-0.6B-4bit, must be cached + --offline-loadable)
#   SMOKE_BIN   (default $HOME/.cargo/bin/rozum)   SMOKE_PORT (default 8396)
# Exit 0 = all pass, 1 = any fail.
set -uo pipefail

MODEL="${SMOKE_MODEL:-mlx-community/Qwen3-0.6B-4bit}"
PORT="${SMOKE_PORT:-8396}"
BIN="${SMOKE_BIN:-$HOME/.cargo/bin/rozum}"
BASE="http://127.0.0.1:$PORT"
GWLOG="$(mktemp -t rozum-smoke-gw.XXXXXX)"
pass=0; fail=0
ok()   { pass=$((pass+1)); echo "  PASS: $1"; }
bad()  { fail=$((fail+1)); echo "  FAIL: $1"; }

content() { python3 -c 'import sys,json
try: d=json.load(sys.stdin)
except Exception as e: print(""); sys.exit()
try: print(d["choices"][0]["message"]["content"])
except Exception: print("")'; }

req() {
  curl -s --max-time 30 "$BASE/v1/chat/completions" -H 'content-type: application/json' -d "$1"
}

echo "gateway-smoke: model=$MODEL port=$PORT"
# Seed-pinned + idle-immortal so the run is fully reproducible; --offline = no fetch.
ROZUM_SAMPLING_SEED=42 ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 \
  "$BIN" gateway --model "$MODEL" --port "$PORT" --offline >"$GWLOG" 2>&1 &
GW=$!
cleanup() { kill "$GW" 2>/dev/null; wait "$GW" 2>/dev/null; rm -f "$GWLOG"; }
trap cleanup EXIT

# Surface the REAL failure cause, not the generic no-backend hints (e.g. a missing
# tokenizer from a partially-downloaded model — the model must be FULLY cached).
gw_cause() { grep -iE 'fail|error|tokenizer|backend unavailable|not.?found|oom|panic|exceeds|does not fit' "$GWLOG" | tail -4 || tail -5 "$GWLOG"; }

# Wait for ready (model load).
ready=0
for _ in $(seq 1 90); do
  if curl -sf --max-time 2 "$BASE/v1/models" >/dev/null 2>&1; then ready=1; break; fi
  kill -0 "$GW" 2>/dev/null || { echo "  gateway died during load — cause:"; gw_cause; exit 1; }
  sleep 1
done
[ "$ready" = 1 ] && ok "gateway came up and serves /v1/models" || { bad "gateway never became ready — cause:"; gw_cause; echo "smoke: $pass passed, $fail failed"; exit 1; }

BODY='{"model":"'"$MODEL"'","messages":[{"role":"user","content":"Reply with exactly: hello world"}],"max_tokens":16,"temperature":0}'

# 1. Liveness + schema: a greedy request returns non-empty content.
A="$(req "$BODY" | content)"
[ -n "$A" ] && ok "greedy request returns non-empty content" || bad "greedy request returned no content"

# 2. Determinism: the SAME greedy request twice → byte-identical (catches leaked
#    gateway state / nondeterminism — the class of bug that flipped the matrix).
B="$(req "$BODY" | content)"
if [ -n "$A" ] && [ "$A" = "$B" ]; then ok "greedy output is deterministic across runs"
else bad "greedy output differs across runs ([$A] != [$B])"; fi

# 3. Concurrency-safe determinism: two greedy requests with a DIFFERENT prompt in
#    between must not perturb a repeat of the first (no cross-request state bleed).
OTHER='{"model":"'"$MODEL"'","messages":[{"role":"user","content":"Count to three."}],"max_tokens":16,"temperature":0}'
_=$(req "$OTHER" >/dev/null 2>&1)
C="$(req "$BODY" | content)"
if [ -n "$A" ] && [ "$A" = "$C" ]; then ok "output stable after an interleaved different request"
else bad "interleaved request perturbed a repeat ([$A] != [$C])"; fi

echo "smoke: $pass passed, $fail failed"
[ "$fail" = 0 ]
