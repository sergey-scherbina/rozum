#!/bin/bash
# smmr-D: directly measure a BIG model's full-context-prefill peak (active+cache high-water) and compare
# to the reserved admission footprint. Closes the spec's open caveat ("big-model full-prefill peak not
# directly measured"). If measured_peak ≤ reserved → the conservative+tightened footprint COVERS the
# spike → co-residency safe by construction confirmed. If >, the footprint under-reserves (the gap).
set -u
cd /Users/sergiy/work/my/rozum-wt/pipeline-swap-settle
BIN="$PWD/target/release/rozum-gateway"
MODEL="${MODEL:-mlx-community:Qwen3.6-35B-A3B-4bit}"
NCTX="${NCTX:-16384}"; PORT=8700; BASE="http://127.0.0.1:$PORT"

echo "================ smmr-D PEAK MEASUREMENT: $MODEL @ n_ctx $NCTX ================"
# Wait for host room first (need ~25GB for the big model) — never overcommit.
for w in $(seq 1 90); do
  v=$("$BIN" gateway --model "$MODEL" --n-ctx "$NCTX" --dry-run 2>&1 | grep -iE "WOULD LOAD|WOULD REFUSE")
  case "$v" in *"WOULD LOAD"*) echo "[$w] host has room → measuring"; break;; esac
  echo "[$w] waiting for room: $v"; [ "$w" = 90 ] && { echo "ABORT: host stayed busy ~3h"; exit 1; }; sleep 120
done
echo "RAM before: $(memory_pressure 2>/dev/null|grep -i 'free percentage')"
RESERVED=$("$BIN" gateway --model "$MODEL" --n-ctx "$NCTX" --dry-run 2>&1 | grep -oE "footprint: +[0-9.]+" | sed -E 's/.*: +//' | head -1)
echo "reserved admission footprint: ${RESERVED} GiB"

# Build a big prompt (~12k tokens ≈ 9.6k words → a large prefill that fits n_ctx 16384).
python3 - <<'PY' > /tmp/smmrd_prompt.txt
print("Summarize the following log in one sentence.\n")
line = "event 12345 occurred at node alpha with latency 42ms and status ok across the mesh fabric. "
print(line * 600)
PY
WC=$(wc -w < /tmp/smmrd_prompt.txt); echo "prompt words: $WC (~$((WC*4/3)) tokens est.)"

ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 \
  "$BIN" gateway --model "$MODEL" --port "$PORT" --n-ctx "$NCTX" --offline >/tmp/smmrd_gw.log 2>&1 &
GW=$!
for i in $(seq 1 240); do curl -s -m2 "$BASE/v1/models" >/dev/null 2>&1 && { echo "ready ${i}s"; break; }
  grep -q "would overcommit" /tmp/smmrd_gw.log && { echo "ABORT: admission refused"; kill -INT $GW; exit 1; }
  kill -0 $GW 2>/dev/null || { echo "gateway died"; tail -3 /tmp/smmrd_gw.log; exit 1; }; sleep 1; done

echo "=== baseline /stats (loaded, no prefill yet) ==="
curl -s -m3 "$BASE/stats" | python3 -c 'import sys,json; d=json.load(sys.stdin); print(" mlx_memory_mb:", d.get("mlx_memory_mb"))' 2>/dev/null

echo "=== driving the BIG prefill (max_tokens=8 — we want the PREFILL spike, not long gen) ==="
PROMPT_JSON=$(python3 -c 'import json;print(json.dumps(open("/tmp/smmrd_prompt.txt").read()))')
curl -s -m300 "$BASE/v1/chat/completions" -H 'content-type: application/json' \
  -d "{\"model\":\"x\",\"messages\":[{\"role\":\"user\",\"content\":$PROMPT_JSON}],\"max_tokens\":8}" \
  -o /tmp/smmrd_resp.json 2>&1
echo "  response: $(python3 -c 'import json;print(json.load(open("/tmp/smmrd_resp.json")).get("choices",[{}])[0].get("message",{}).get("content","?")[:60])' 2>/dev/null || echo '(see log)')"

echo "=== peak /stats AFTER prefill (get_peak_memory = cumulative high-water, incl. the prefill spike) ==="
PEAK_JSON=$(curl -s -m3 "$BASE/stats")
echo "$PEAK_JSON" | python3 -c '
import sys,json
d=json.load(sys.stdin); m=d.get("mlx_memory_mb",{})
active=m.get("active",0); peak=m.get("peak",0); cache=m.get("cache",0)
foot_gib=(peak+cache)/1024.0
print(f"  active={active}MB peak={peak}MB cache={cache}MB")
print(f"  MEASURED peak footprint (peak+cache) = {foot_gib:.2f} GiB")
import os
reserved=float(os.environ.get("RESERVED","0"))
print(f"  RESERVED admission footprint           = {reserved:.2f} GiB")
if reserved>0:
    if foot_gib <= reserved:
        print(f"  VERDICT: ✅ COVERED — reserved {reserved:.2f} ≥ measured {foot_gib:.2f} (margin {reserved-foot_gib:.2f} GiB). Footprint covers a full-context prefill → safe by construction confirmed.")
    else:
        print(f"  VERDICT: ✗ GAP — measured {foot_gib:.2f} > reserved {reserved:.2f} (under-reserve by {foot_gib-reserved:.2f} GiB). Footprint must grow.")
' RESERVED="$RESERVED"
kill -INT $GW 2>/dev/null; sleep 2
echo "RAM after: $(memory_pressure 2>/dev/null|grep -i 'free percentage')"; uptime
