#!/usr/bin/env bash
# pipeline-swap-repro.sh — fresh-system A/B to settle ONE question:
#
#   Is the in-process GLM-9B -> Qwen3-4B swap failure ("long prompt after the swap -> HTTP 500 /
#   `mlx: eval failed`") a REAL MLX in-process-swap bug, or just memory/GPU degradation that
#   accumulates over a long session of model loads?
#
# RUN THIS RIGHT AFTER A REBOOT (fresh GPU + ample free RAM). It loads ONE model at a time
# (no-reboot-safe) and tears each down gracefully. It records free RAM before every cell so a
# failure can be correlated with memory pressure.
#
# Background (what's already known — do NOT re-test these):
#   - The pipeline's executor inherently gets a LONG prompt (the planner's plan is appended), so
#     "long prompt after swap" IS the real-world pipeline case.
#   - The failure reproduces via the gateway's OWN /control/switch (not just the lazy pipeline), so
#     it is NOT the LazyPipelineBackend orchestration — it's the in-process swap path itself.
#   - RULED OUT as fixes (each tested live, none worked): a teardown mlx_synchronize stream-flush
#     (this was HARMFUL — it broke ALL model-switching; reverted in 1482dd7 — DO NOT RE-ADD IT),
#     MLX cache-evict (set_cache_limit 0/restore), reset_peak_memory, settle-before-build (1.5s),
#     settle-after-build (2s), inline-drop vs spawn_blocking, a separate tokio task per tier.
#   - The build/drop paths are IDENTICAL between the lazy pipeline and the Switchboard.
#   - single Qwen3-4B handled a 1449-word prompt fine WHEN RAM WAS PLENTIFUL (~22 GiB free); the
#     swap-long failures appeared once the session had eaten RAM down to ~6 GiB free. Hence this script.
#
# Decision the output gives you:
#   - swap-long PASSES on a fresh system w/ ample RAM  -> it was SESSION/RAM DEGRADATION; the
#     in-process pipeline is fine on a healthy system. No code fix needed; just don't run it after a
#     marathon of loads. (Re-confirm by re-running after deliberately eating RAM.)
#   - swap-long FAILS on a fresh system w/ ample RAM   -> REAL MLX in-process-swap bug. Next step:
#     investigate per-load MLX memory-limit / cache-limit RESET in build_from_config / the worker
#     load path (NOT a teardown flush — that path is poisoned, see above). Meanwhile solve.sh
#     (separate processes) avoids it entirely.
set -u
here="$(cd "$(dirname "$0")" && pwd)"; root="$(cd "$here/../.." && pwd)"
BIN="${BIN:-$root/target/release/rozum}"
GLM="${GLM:-mlx-community:GLM-4-9B-0414-4bit}"
QWEN="${QWEN:-mlx-community:Qwen3-4B-4bit}"
NCTX="${NCTX:-4096}"
PORT="${PORT:-8350}"; BASE="http://127.0.0.1:$PORT"
REPS="${REPS:-3}"

[ -x "$BIN" ] || { echo "!! build first: cargo build --release  (missing $BIN)"; exit 1; }

freeram() { top -l 1 -n 0 2>/dev/null | awk '/PhysMem/{for(i=1;i<=NF;i++) if($i=="unused" || $i=="free"){print $(i-1); exit}}'; }
pressure() { sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null; }

echo "═══ pipeline-swap-repro ═══"
echo "  uptime:   $(uptime | sed 's/.*up //; s/,.*users.*//')"
echo "  free RAM: $(freeram)   pressure: $(pressure) (1=normal 2=warn 4=critical)"
echo "  reps:     $REPS   n_ctx: $NCTX"
echo "  NOTE: run this on a FRESH BOOT for a clean verdict."
echo

GWPID=""
stop_gw() {
  [ -n "$GWPID" ] && { kill -INT "$GWPID" 2>/dev/null; for _ in $(seq 1 90); do kill -0 "$GWPID" 2>/dev/null || break; sleep 1; done; GWPID=""; }
}
trap stop_gw EXIT
start_gw() { # $1 = model spec (single model; switchable)
  ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 \
    "$BIN" gateway --model "$1" --n-ctx "$NCTX" --port "$PORT" --offline >/tmp/repro_gw.log 2>&1 & GWPID=$!
  for _ in $(seq 1 120); do curl -sf -m2 "$BASE/v1/models" >/dev/null 2>&1 && return 0; kill -0 "$GWPID" 2>/dev/null || return 1; sleep 1; done
  return 1
}

# python helpers: chat returns "OK"/"FAIL: ..."; switch returns the status string.
PYCHAT='
import json,sys,urllib.request
base,prompt,mt=sys.argv[1],sys.argv[2],int(sys.argv[3])
body=json.dumps({"model":"x","messages":[{"role":"user","content":prompt}],"temperature":0.2,"max_tokens":mt}).encode()
try:
  d=json.load(urllib.request.urlopen(urllib.request.Request(base+"/v1/chat/completions",body,{"content-type":"application/json"}),timeout=180))
  c=d["choices"][0]["message"]["content"]
  print("OK" if c.strip() else "EMPTY")
except Exception as e:
  print("FAIL:%s"%e)
'
PYSWITCH='
import json,sys,urllib.request
base,model=sys.argv[1],sys.argv[2]
try:
  d=json.load(urllib.request.urlopen(urllib.request.Request(base+"/control/switch",json.dumps({"model":model}).encode(),{"content-type":"application/json"}),timeout=180))
  print(d.get("status","?"))
except Exception as e: print("switch-FAIL:%s"%e)
'
chat()   { python3 -c "$PYCHAT" "$BASE" "$1" "${2:-120}"; }
switch() { python3 -c "$PYSWITCH" "$BASE" "$1"; }

SHORT="Write a Rust function add(a:i32,b:i32)->i32. Only the function."
LONG="$(python3 -c 'print("Carefully consider each step of the algorithm and the data structures involved, validate inputs and handle every edge case. "*55 + " Now write a Rust function add(a:i32,b:i32)->i32. Only the function.")')"

declare -A PASS FAIL
cell() { # $1=name  -> increments PASS/FAIL from the LAST echoed token being OK/anything-else
  :; }
record() { local name="$1" res="$2"; if [ "$res" = OK ]; then PASS[$name]=$(( ${PASS[$name]:-0} + 1 )); else FAIL[$name]=$(( ${FAIL[$name]:-0} + 1 )); echo "      ($name) -> $res"; fi; }

for rep in $(seq 1 "$REPS"); do
  echo "── rep $rep/$REPS  (free RAM: $(freeram), pressure $(pressure)) ──"

  # Cell 1: single Qwen, SHORT prompt (sanity).
  start_gw "$QWEN" && record qwen_short "$(chat "$SHORT" 80)" || record qwen_short "FAIL:no-load"; stop_gw

  # Cell 2: single Qwen, LONG prompt — does Qwen handle long ALONE on this system?
  start_gw "$QWEN" && record qwen_long "$(chat "$LONG" 80)" || record qwen_long "FAIL:no-load"; stop_gw

  # Cell 3: GLM -> switch -> Qwen, SHORT prompt — does swap+short work?
  if start_gw "$GLM"; then chat "Say hello." 15 >/dev/null; s=$(switch "$QWEN"); [ "$s" = switched ] && record swap_short "$(chat "$SHORT" 80)" || record swap_short "FAIL:switch=$s"; fi; stop_gw

  # Cell 4: GLM plan -> switch -> Qwen with the plan (LONG) — THE failing case.
  if start_gw "$GLM"; then
    plan="$(python3 -c "$PYCHAT" "$BASE" "Give a concise plan (no code) to write a Rust add function." 300 2>/dev/null; \
            curl -s -m120 "$BASE/v1/chat/completions" -H 'content-type: application/json' \
              -d '{"model":"x","messages":[{"role":"user","content":"Give a concise plan (no code) to write a Rust add(a,b) function."}],"max_tokens":300}' \
              | python3 -c 'import json,sys; print(json.load(sys.stdin)["choices"][0]["message"]["content"])' 2>/dev/null)"
    s=$(switch "$QWEN")
    [ "$s" = switched ] && record swap_long "$(chat "Write a Rust function add(a:i32,b:i32)->i32. Only the function.

[Plan from advisor]
$plan" 120)" || record swap_long "FAIL:switch=$s"
  fi; stop_gw

  # Cell 5: the ACTUAL feature — lazy pipeline --model GLM,Qwen, one request.
  ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 \
    "$BIN" gateway --model "$GLM,$QWEN" --n-ctx "$NCTX" --port "$PORT" --offline >/tmp/repro_gw.log 2>&1 & GWPID=$!
  ok=0; for _ in $(seq 1 120); do curl -sf -m2 "$BASE/v1/models" >/dev/null 2>&1 && { ok=1; break; }; kill -0 "$GWPID" 2>/dev/null || break; sleep 1; done
  [ "$ok" = 1 ] && record lazy_pipeline "$(chat "Write a Rust function is_prime(n:u64)->bool. Only the function." 150)" || record lazy_pipeline "FAIL:no-load"
  stop_gw
done

echo
echo "═══ RESULT (pass/total over $REPS reps) ═══"
for cell in qwen_short qwen_long swap_short swap_long lazy_pipeline; do
  p=${PASS[$cell]:-0}; f=${FAIL[$cell]:-0}; printf "  %-14s %d/%d\n" "$cell" "$p" "$((p+f))"
done
echo
echo "VERDICT GUIDE:"
echo "  swap_long & lazy_pipeline PASS (≈$REPS/$REPS) on this fresh system  -> it was SESSION/RAM"
echo "      degradation; the in-process pipeline works on a healthy host. No code fix needed."
echo "  swap_long FAILS while qwen_long PASSES (Qwen handles long alone, but not after a swap)"
echo "      -> REAL in-process-swap bug. Next: per-load MLX memory/cache-limit RESET in the worker"
echo "      load path (NOT a teardown flush — that broke everything, see header). solve.sh avoids it."
echo "  free RAM at the top was LOW (<8 GiB)  -> not a clean run; reboot and re-run."
