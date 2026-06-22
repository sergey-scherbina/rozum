#!/usr/bin/env bash
# Gateway sampling-determinism probe (matrix-nondeterminism isolate).
#
# POSTs a BYTE-IDENTICAL /v1/chat/completions request N times to an ALREADY-RUNNING
# gateway and reports how many DISTINCT completions came back. It does NOT start a
# gateway, so it can never cause a second concurrent model load (the 2026-06-22 reboot
# cause — project-reboot-watchdog-oom). Start ONE gateway yourself, then point this at it.
#
# Why: the gateway leaves SamplingParams.seed unset, so the sampler + MLX RNG seed from
# entropy → any temperature>0 request yields a different token stream every run → matrix
# cells flip pass/fail on an identical config. See docs/specs/matrix-nondeterminism.md.
#
# Usage:
#   BASE=http://127.0.0.1:8300 N=6 TEMP=1.0 scripts/bench/nondeterminism-probe.sh
#
# Isolate protocol — run each against a FRESHLY-started SINGLE gateway (never two at once):
#   1. plain gateway,                       TEMP=1.0 -> expect distinct>1  (E1 present)
#   2. gateway w/ ROZUM_SAMPLING_SEED=42,    TEMP=1.0 -> expect distinct=1  (E1 fixed by seed)
#   3. plain gateway,                       TEMP=0   -> dense: distinct=1; MoE: watch (E2 numerics)
set -uo pipefail
BASE="${BASE:-http://127.0.0.1:8300}"
N="${N:-6}"
TEMP="${TEMP:-1.0}"
MAXTOK="${MAXTOK:-80}"
PROMPT="${PROMPT:-Write a Rust function that reverses a string by characters. Reply with only the code, no prose.}"

MODEL="$(curl -s -m5 "$BASE/v1/models" \
  | python3 -c 'import sys,json; print(json.load(sys.stdin)["data"][0]["id"])' 2>/dev/null || true)"
[ -n "$MODEL" ] || { echo "no model at $BASE/v1/models — is a single gateway up?" >&2; exit 1; }

tmp="$(mktemp -d)"; trap 'rm -rf "$tmp"' EXIT
body="$tmp/body.json"
python3 - "$MODEL" "$PROMPT" "$TEMP" "$MAXTOK" >"$body" <<'PY'
import json, sys
model, prompt, temp, maxtok = sys.argv[1], sys.argv[2], float(sys.argv[3]), int(sys.argv[4])
print(json.dumps({"model": model,
                  "messages": [{"role": "user", "content": prompt}],
                  "temperature": temp, "max_tokens": maxtok}))
PY

echo "probe: base=$BASE model=$MODEL N=$N temp=$TEMP maxtok=$MAXTOK"
for i in $(seq 1 "$N"); do
  curl -s -m120 "$BASE/v1/chat/completions" -H 'content-type: application/json' \
    --data-binary @"$body" \
    | python3 -c 'import sys,json; d=json.load(sys.stdin); print(d.get("choices",[{}])[0].get("message",{}).get("content",""), end="")' \
    >"$tmp/out.$i" 2>/dev/null || true
  printf "  run %d: sha=%s  bytes=%s\n" "$i" \
    "$(shasum "$tmp/out.$i" | cut -c1-12)" "$(wc -c <"$tmp/out.$i" | tr -d ' ')"
done

distinct="$(for f in "$tmp"/out.*; do shasum "$f"; done | awk '{print $1}' | sort -u | wc -l | tr -d ' ')"
echo "distinct completions: $distinct / $N"
if [ "$distinct" = 1 ]; then echo "=> DETERMINISTIC for this config"; else echo "=> NON-DETERMINISTIC (flip source)"; fi
