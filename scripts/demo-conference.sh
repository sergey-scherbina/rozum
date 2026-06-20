#!/usr/bin/env bash
# Demo conference — MODEL side. Brings up local models as live participants in one meeting
# room, ready for humans (and a cloud Claude) to join. Spec: docs/specs/demo-conference.md.
# The polished PWA front end + the Seatbelt sandbox wrap are added on top (co-agent's half).
#
# Usage:
#   scripts/demo-conference.sh                 # default: gpt-oss + qwen3.6 (needs a roomy Mac)
#   LOCAL=gpt-oss scripts/demo-conference.sh   # one local model (safe on 36 GB) + cloud Claude
#   ROOM=myroom NCTX=4096 scripts/demo-conference.sh
#
# Then a human joins:    rozum meetings attach --room <ROOM>      (TUI)
#                        rozum meetings web    --room <ROOM>      (browser)
# A cloud Claude joins by running Claude Code in the room (it speaks MCP to the daemon).
# Ctrl-C stops everything it started.
set -uo pipefail

BIN="${ROZUM_BIN:-$(command -v rozum || echo ./target/release/rozum)}"
ROOM="${ROOM:-conference}"
NCTX="${NCTX:-8192}"
export ROZUM_MLX_CACHE_GB="${ROZUM_MLX_CACHE_GB:-1}"  # free MLX cache — two models on one box

# Which local models join. `LOCAL` is a space-separated set of short keys: gpt-oss | qwen3.6.
LOCAL="${LOCAL:-gpt-oss qwen3.6}"
spec_of() { case "$1" in
  gpt-oss)  echo "mlx-community:gpt-oss-20b-MXFP4-Q4" ;;
  qwen3.6)  echo "mlx-community:Qwen3.6-35B-A3B-4bit" ;;
  *)        echo "$1" ;;  # pass an explicit spec through as its own handle
esac; }
port_of() { case "$1" in gpt-oss) echo 8102 ;; qwen3.6) echo 8101 ;; *) echo 8103 ;; esac; }
gb_of()   { case "$1" in gpt-oss) echo 11 ;; qwen3.6) echo 20 ;; *) echo 12 ;; esac; }

PIDS=()
cleanup() {
  echo; echo "stopping conference…"
  for p in "${PIDS[@]:-}"; do kill "$p" 2>/dev/null || true; done
  # gateways detach from us → stop whatever listens on the ports we used (robust to re-exec);
  # participants by room.
  for m in $LOCAL; do lsof -ti "tcp:$(port_of "$m")" 2>/dev/null | xargs kill 2>/dev/null || true; done
  pkill -f "meetings participant.*--room $ROOM" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

# ── memory preflight ──────────────────────────────────────────────────────────────────────
need=0; for m in $LOCAL; do need=$((need + $(gb_of "$m"))); done
free_pct=$(memory_pressure 2>/dev/null | grep -i 'free percentage' | grep -oE '[0-9]+' | head -1)
total_gb=$(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 ))
free_gb=$(( total_gb * ${free_pct:-50} / 100 ))
echo "rozum demo conference  ·  room '$ROOM'  ·  local models: $LOCAL"
echo "  weights need ~${need} GB resident; ~${free_gb} GB free of ${total_gb} GB."
if [ "$need" -gt "$(( free_gb - 4 ))" ]; then
  echo "  ⚠ TIGHT — two big models may OOM. Safer: LOCAL=gpt-oss scripts/demo-conference.sh (+ cloud Claude),"
  echo "    or lower NCTX. Continuing in 5 s (Ctrl-C to abort)…"; sleep 5
fi

"$BIN" meetings start >/dev/null 2>&1 || true

# ── start a gateway per local model + wait until it actually answers ────────────────────────
wait_ready() { local port="$1" name="$2"; printf "  warming %s (:%s) " "$name" "$port"
  for _ in $(seq 1 60); do
    if curl -s -m 30 "http://127.0.0.1:$port/v1/chat/completions" -H 'content-type: application/json' \
         -d '{"model":"x","messages":[{"role":"user","content":"hi"}],"max_tokens":3}' 2>/dev/null | grep -q choices
    then echo "ready"; return 0; fi; printf "."; sleep 5
  done; echo "TIMEOUT (see /tmp/demo-gw-$port.log)"; return 1; }

for m in $LOCAL; do
  spec=$(spec_of "$m"); port=$(port_of "$m")
  "$BIN" gateway --model "$spec" --port "$port" --n-ctx "$NCTX" >"/tmp/demo-gw-$port.log" 2>&1 &
  PIDS+=($!)
done
for m in $LOCAL; do wait_ready "$(port_of "$m")" "$m" || { echo "  gateway for $m didn't come up — aborting"; exit 1; }; done

# ── join each model to the room as a live participant ───────────────────────────────────────
PERSONA="a friendly participant in a live demo of rozum — software that runs AI language models \
LOCALLY on the operator's own Mac (Apple Silicon), so people and coding agents use them privately, \
no cloud. The operator is also presenting 'busi', their product for solving problems. Be warm, \
concrete, and concise; actually answer questions instead of deflecting."
peers=""; for m in $LOCAL; do peers="$peers $m"; done
for m in $LOCAL; do
  others=""; for o in $peers; do [ "$o" = "$m" ] || others="$others --peer $o"; done
  "$BIN" meetings participant --model "$(spec_of "$m")" --room "$ROOM" --as "$m" \
    --gateway-url "http://127.0.0.1:$(port_of "$m")/v1" $others \
    --persona "You are $m, $PERSONA" >"/tmp/demo-part-$m.log" 2>&1 &
  PIDS+=($!)
  echo "  $m joined '$ROOM'"
done

cat <<EOF

  ✅ conference is LIVE — room '$ROOM' with: $LOCAL
  join as a human:
     TUI:  $BIN meetings attach --room $ROOM
     web:  $BIN meetings web    --room $ROOM      (open the printed URL + secret)
  add cloud Claude: run Claude Code in this repo (it joins the room over MCP).
  talk to a model with an @mention, e.g.:  "@gpt-oss что такое rozum?"

  Ctrl-C here stops the models + gateways it started.
EOF
wait
