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

Output the COMPLETE solution — every file's full final contents — each EXACTLY in this structured format (NO markdown code fences):
=== FILE: <relative/path> ===
<the full raw file contents>
=== END ===
Emit one such block per file (e.g. Cargo.toml, src/main.rs). Do NOT run anything; just emit the files."
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

echo "═══ STAGE 2a — deterministic handoff: write the planner's files, verify ═══"
# The forward-output handoff is DETERMINISTIC (parse + write the planner's files) — for create-from-scratch
# the bottleneck is landing CORRECT code reliably, not an agent loop. The agentic executor only runs as a
# FIX fallback when the planner's code doesn't build (gpt-oss's strength: fix/debug, not from-scratch).
WROTE=$(python3 "$here/write_solution.py" "$WORK" "$WORK/solution.md")
echo "  wrote: $(echo "$WROTE" | tr '\n' ' ')"
# Make WORK a cargo WORKSPACE ROOT so `cargo` does NOT walk UP the tree and pick up a stray parent
# Cargo.toml (e.g. a leftover in /tmp) as the workspace — which would fail the build spuriously.
[ -f "$WORK/Cargo.toml" ] && ! grep -q '\[workspace\]' "$WORK/Cargo.toml" && printf '\n[workspace]\n' >> "$WORK/Cargo.toml"
build_ok(){ ( cd "$WORK" && cargo build -q 2>"$WORK/build.err" ); }
if build_ok; then
  echo "  ✅ the planner's solution BUILDS as-is — no executor needed"
else
  echo "═══ STAGE 2b — EXECUTOR ($AGENT @ $EXECUTOR): fix the build ═══"
  ERR="$(tail -8 "$WORK/build.err" 2>/dev/null)"
  FIX_PROMPT="There is a Rust project in the current directory that does not build. Fix it so that \"cargo run -- hello\" works. Make the minimal change. The build error is:
$ERR"
  ( cd "$WORK" && case "$AGENT" in
      codex)    "$BIN" launch --model "$EXECUTOR" codex exec "$FIX_PROMPT" --dangerously-bypass-approvals-and-sandbox ;;
      opencode) "$BIN" launch --model "$EXECUTOR" opencode run "$FIX_PROMPT" ;;
      claude)   "$BIN" launch --model "$EXECUTOR" claude -p "$FIX_PROMPT" --dangerously-skip-permissions ;;
      *) echo "unknown AGENT=$AGENT"; exit 2 ;;
    esac ) >"$WORK/executor_agent.log" 2>&1
  stop_gw
fi
echo "═══ RESULT (in $WORK) ═══"
( cd "$WORK" && ls -1 *.toml src/*.rs 2>/dev/null; echo "--- cargo run -- hello ---"; timeout 60 cargo run -- hello 2>&1 | tail -2 )
echo "kept: $WORK"
