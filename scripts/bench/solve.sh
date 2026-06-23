#!/usr/bin/env bash
# rozum solve — the planner→executor pipeline (lazy sequential model pipeline, planner-executor variant:
# always-advance + forward-output). Stage 1: a PLANNER model reasons out the COMPLETE solution one-shot
# (where GLM is strong). Stage 2: an EXECUTOR — a REAL agent (codex/opencode) through the gateway with its
# delivery fixes — implements the solution agentically (where gpt-oss is strong). SEQUENTIAL: one model
# resident at a time (planner unloaded before the executor loads) → fits a 36 GB no-reboot host.
#
#   PLANNER=… EXECUTOR=… AGENT=codex scripts/bench/solve.sh "<task>"   [WORK=<dir> to keep the result]
set -u
here="$(cd "$(dirname "$0")" && pwd)"; root="$(cd "$here/../.." && pwd)"
BIN="${BIN:-$root/target/release/rozum}"
PLANNER="${PLANNER:-mlx-community:GLM-4-32B-0414-4bit}"
EXECUTOR="${EXECUTOR:-mlx-community:gpt-oss-20b-MXFP4-Q4}"
AGENT="${AGENT:-codex}"
PORT="${PORT:-8313}"; BASE="http://127.0.0.1:$PORT"
TASK="${1:?usage: solve.sh \"<task>\"}"
WORK="${WORK:-$(mktemp -d /tmp/solve-XXXXXX)}"; mkdir -p "$WORK"

GWPID=""
# Stop both the planner gateway we start directly (GWPID) AND any shared gateway `rozum launch` spawned
# (it persists by default) — so the model slot is released and the next stage / a no-reboot host is clean.
stop_gw(){
  [ -n "$GWPID" ] && { kill -INT "$GWPID" 2>/dev/null; for _ in $(seq 1 90); do kill -0 "$GWPID" 2>/dev/null||break; sleep 1; done; GWPID=""; }
  "$BIN" gateway stop >/dev/null 2>&1 || true
  for _ in $(seq 1 60); do pgrep -f 'release/rozum gateway --model' >/dev/null 2>&1 || break; sleep 1; done
}
trap stop_gw EXIT
wait_ready(){ for _ in $(seq 1 240); do curl -s -m2 "$BASE/v1/models" >/dev/null 2>&1 && return 0; kill -0 "$GWPID" 2>/dev/null||return 1; sleep 1; done; return 1; }

echo "═══ STAGE 1 — PLANNER ($PLANNER): one-shot solution ═══"
ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 "$BIN" gateway --model "$PLANNER" --port "$PORT" --offline >"$WORK/planner_gw.log" 2>&1 & GWPID=$!
wait_ready || { echo "!! planner did not load"; grep -iE 'refus|overcommit' "$WORK/planner_gw.log"|tail -2; exit 1; }
PLAN_PROMPT="$TASK

Produce the COMPLETE solution as the full final contents of every file needed. Show each file in a fenced block headed by its path. Do NOT run anything — just give the files and a one-line build/run command."
python3 - "$BASE" "$PLAN_PROMPT" >"$WORK/solution.md" <<'PY'
import json,sys,urllib.request
base,prompt=sys.argv[1],sys.argv[2]
model=json.load(urllib.request.urlopen(base+"/v1/models"))["data"][0]["id"]
body=json.dumps({"model":model,"temperature":0.2,"messages":[{"role":"user","content":prompt}],"max_tokens":1500}).encode()
d=json.load(urllib.request.urlopen(urllib.request.Request(base+"/v1/chat/completions",body,{"content-type":"application/json"}),timeout=240))
print(d["choices"][0]["message"]["content"])
PY
echo "  solution: $(wc -l <"$WORK/solution.md") lines, $(wc -c <"$WORK/solution.md") bytes → $WORK/solution.md"
echo "  ↓ unloading planner (lazy swap)"; stop_gw

echo "═══ STAGE 2 — EXECUTOR ($AGENT @ $EXECUTOR): implement the solution ═══"
# `rozum launch --model` starts the executor gateway itself (one model resident; the planner is already
# unloaded above) and runs the REAL agent through it — so the gateway's delivery fixes apply (the fair test).
EXEC_PROMPT="$TASK

A vetted, correct solution is provided below. IMPLEMENT it exactly: create each file with the given contents, then build/run/test and fix any error until it actually works.

<solution>
$(cat "$WORK/solution.md")
</solution>"
( cd "$WORK" && case "$AGENT" in
    codex)    "$BIN" launch --model "$EXECUTOR" codex exec "$EXEC_PROMPT" --dangerously-bypass-approvals-and-sandbox ;;
    opencode) "$BIN" launch --model "$EXECUTOR" opencode run "$EXEC_PROMPT" ;;
    claude)   "$BIN" launch --model "$EXECUTOR" claude -p "$EXEC_PROMPT" --dangerously-skip-permissions ;;
    *) echo "unknown AGENT=$AGENT"; exit 2 ;;
  esac ) >"$WORK/executor_agent.log" 2>&1
stop_gw
echo "═══ RESULT (in $WORK) ═══"
( cd "$WORK" && ls -1 *.toml src/*.rs 2>/dev/null; echo "--- cargo run -- hello ---"; timeout 60 cargo run -- hello 2>&1 | tail -2 )
echo "kept: $WORK"
