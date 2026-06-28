#!/bin/bash
# P5 — the residency admission QUEUE under REAL contention (spec docs/specs/residency-admission-queue.md).
# Antagonist = a BATCH GLM-32B gateway in a loop (~22GB). Matrix = an INTERACTIVE Qwen3-4B agentic run.
# They cannot coreside (22+7 > 27 budget) → the interactive matrix must PREEMPT the batch antagonist.
# Asserts: 0 jetsam, 0 dead cells, the antagonist YIELDS (preempted), the matrix completes — i.e. the
# queue+priority+preemption kill contention-jetsam without a single retry.
#
# Proven 2026-06-28: antagonist preempted @ load, matrix ran 6/6 cells, 0 dead, 0 jetsam, no reboot;
# the batch then correctly WAITED 240s for the interactive matrix (never preempts up-priority).
set -u
cd "$(git rev-parse --show-toplevel)" || exit 1
BIN="${BENCH_BIN:-$PWD/target/release/rozum-gateway}"
GLM="${ANTAGONIST_MODEL:-mlx-community:GLM-4-32B-0414-4bit}"
MATRIX_MODEL="${MATRIX_MODEL:-mlx-community:Qwen3-4B-4bit}"
ANTAG_PORT="${ANTAG_PORT:-8600}"; ANTAG_BASE="http://127.0.0.1:$ANTAG_PORT"
OBS="$HOME/.rozum/gateway.jsonl"
: > /tmp/p5_antag.log; : > /tmp/p5_matrix.log
rm -rf "$HOME/.rozum/waiters" "$HOME/.rozum/preempt" 2>/dev/null
before=$(grep -c '"reason":"preempted"' "$OBS" 2>/dev/null); before=${before:-0}

echo "================ P5 CONTENTION TEST ================"
echo "RAM before: $(memory_pressure 2>/dev/null | grep -i 'free percentage')"

antagonist() { # batch GLM loop: load → (get preempted, exit) → reload (auto-requeue). watchdog yields.
  for i in $(seq 1 25); do
    echo "[antag $i] $(date +%T) loading GLM (batch)…" >> /tmp/p5_antag.log
    ROZUM_RESIDENCY_PRIO=batch ROZUM_GATEWAY_UNLOAD_IDLE_SECS=8 ROZUM_GATEWAY_IDLE_SECS=0 \
      "$BIN" gateway --model "$GLM" --port "$ANTAG_PORT" --n-ctx 4096 --offline >> /tmp/p5_antag.log 2>&1 &
    local gw=$!
    for _ in $(seq 1 120); do curl -s -m2 "$ANTAG_BASE/v1/models" >/dev/null 2>&1 && break; kill -0 $gw 2>/dev/null || break; sleep 1; done
    curl -s -m100 "$ANTAG_BASE/v1/chat/completions" -H 'content-type: application/json' \
      -d '{"model":"x","messages":[{"role":"user","content":"count slowly"}],"max_tokens":120}' >/dev/null 2>&1 &
    wait $gw 2>/dev/null
    echo "[antag $i] $(date +%T) exited" >> /tmp/p5_antag.log
    sleep 2
  done
}
antagonist & ANTAG_PID=$!
sleep 30  # let the antagonist become resident with GLM first

echo ">>> $(date +%T) launching INTERACTIVE matrix ($MATRIX_MODEL) against the resident batch GLM…"
BENCH_BIN="$BIN" AGENTIC_MODELS="$MATRIX_MODEL" AGENTS=claude \
  TASKS="${TASKS:-greet build rpn}" REPS="${REPS:-2}" RUN_TIMEOUT=300 GEN_TIMEOUT=240 \
  BENCH_PORT_BASE="${BENCH_PORT_BASE:-8500}" BENCH_OUT="$PWD/scripts/bench/results/p5-$(date +%H%M%S)" \
  bash scripts/bench/agentic.sh > /tmp/p5_matrix.log 2>&1

kill $ANTAG_PID 2>/dev/null
pkill -INT -f "rozum-gateway gateway --model" 2>/dev/null; sleep 4
pkill -KILL -f "rozum-gateway gateway --model $GLM" 2>/dev/null

echo "================ P5 RESULT ================"
echo "--- matrix pass-rate ---"; grep -A6 "pass-rate (agent" /tmp/p5_matrix.log | head -6
dead=$(grep -cE "0.0s +pass=0 +agent=0MB" /tmp/p5_matrix.log); dead=${dead:-0}
jetsam=$(cat /tmp/p5_matrix.log /tmp/p5_antag.log | grep -cE "Killed: ?9"); jetsam=${jetsam:-0}
after=$(grep -c '"reason":"preempted"' "$OBS" 2>/dev/null); after=${after:-0}
preempts=$((after - before))
survived=$(grep -q 'model loaded once' /tmp/p5_matrix.log && echo yes || echo NO)
echo "--- ASSERTIONS ---"
echo "  dead cells (want 0):            $dead"
echo "  jetsam Killed:9 (want 0):       $jetsam"
echo "  antagonist PREEMPTED (want >0): $preempts"
echo "  matrix gateway survived:        $survived"
echo "RAM after: $(memory_pressure 2>/dev/null | grep -i 'free percentage')"; uptime
if [ "$dead" -eq 0 ] && [ "$jetsam" -eq 0 ] && [ "$preempts" -gt 0 ] && [ "$survived" = yes ]; then
  echo "VERDICT: ✅ PASS — queue+priority+preemption killed contention-jetsam, 0 retries"
else
  echo "VERDICT: ✗ FAIL — see assertions above"
fi
