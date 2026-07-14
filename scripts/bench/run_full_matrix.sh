#!/usr/bin/env bash
# Prepared matrix run (operator-requested): 2 models × 3-agent × all-tasks (incl. rpn).
# Captures red/green + TIME (seconds) + REASON (verify detail in stdout). KEEP=1 keeps workdirs.
# Slot-safe: models load SEQUENTIALLY (one at a time); adaptive load + admission gate prevent overcommit
# (a model that doesn't fit is REFUSED cleanly before any weights load → a matrix FAIL, never a reboot).
set -u
cd "$(dirname "$0")/../.."                                   # repo root (worktree)
BIN="${BENCH_BIN:-$(pwd)/target/release/rozum-gateway}"
if [ ! -x "$BIN" ]; then
  if [ -x "$(pwd)/target/debug/rozum-gateway" ]; then
    BIN="$(pwd)/target/debug/rozum-gateway"
  else
    echo "ABORT: no rozum-gateway binary; build with: cargo build --release --bin rozum-gateway" >&2
    exit 1
  fi
fi
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="scripts/bench/results/full-matrix-$STAMP"
LOG="/tmp/full_matrix-$STAMP.log"

# Two models (operator request), the pipeline-cascade value A/B:
#   1. Qwen3.6-35B-A3B-DWQ — single strongest local agentic coder in the current bench default set.
#   2. GLM-4-32B,gpt-oss-20b — LAZY in-process pipeline (comma-spec): GLM-32B plans → gpt-oss executes,
#      one resident at a time (in-process swap fixed: docs/pipeline-swap-bug.md). It RELOADS both tiers
#      per agent turn, so it is slow → RUN_TIMEOUT is bumped well above the single-model 280 s below.
MODELS="${MODELS:-mlx-community:Qwen3.6-35B-A3B-4bit-DWQ mlx-community:GLM-4-32B-0414-4bit,mlx-community:gpt-oss-20b-MXFP4-Q4}"

if [ "${MATRIX_RAM_SCHEDULE:-0}" = 1 ] && command -v python3 >/dev/null 2>&1; then
  PLAN_JSON="${OUT}.schedule.json"
  if python3 scripts/bench/plan_matrix_schedule.py --models "$MODELS" --nctx "${NCTX:-32768}" --bench-bin "$BIN" --out "$PLAN_JSON"; then
    PLANNED_MODELS="$(
      python3 - "$PLAN_JSON" <<'PY'
import json
import sys

with open(sys.argv[1]) as f:
    plan = json.load(f)
print(" ".join(row["model"] for row in plan.get("models", []) if row.get("verdict") in ("load", "unknown")))
PY
    )"
    if [ -n "$PLANNED_MODELS" ]; then
      MODELS="$PLANNED_MODELS"
      echo ">> RAM-aware model order: $MODELS"
      echo ">> schedule: $PLAN_JSON"
    fi
  else
    echo ">> RAM-aware scheduler failed; continuing with configured MODELS" >&2
  fi
fi

# Optional safety lever (DEFAULT keep the safe 3 GiB): to squeeze the 35B in when RAM is tight, the
# operator may lower keep-free — pass KEEP_FREE_GIB=2 to this script. Left UNSET = the safe 3 GiB default.
EXTRA_ENV=()
if [ -n "${KEEP_FREE_GIB:-}" ]; then
  EXTRA_ENV+=("ROZUM_GATEWAY_MIN_FREE_RAM_BYTES=$(( KEEP_FREE_GIB * 1073741824 ))")
  echo ">> keep-free lowered to ${KEEP_FREE_GIB} GiB (safety headroom reduced — operator opt-in)"
fi

echo ">> slot check"; pgrep -fl 'gateway --model|mlx_mem_probe' 2>/dev/null | grep -viE 'claude|mcp-proxy|grep' && { echo "ABORT: slot busy"; exit 1; }
[ -f "$HOME/.rozum/residency.lock" ] && { echo "ABORT: residency lock present"; exit 1; }

MODEL_COUNT="$(printf '%s\n' "$MODELS" | awk '{print NF}')"
echo ">> launching: $MODEL_COUNT models × {claude,codex,opencode} × {greet,build,fix,test,debug,rpn}, REPS=1, KEEP=1"
echo ">> out=$OUT  log=$LOG"

# RUN_TIMEOUT=900: the GLM-32B,gpt-oss lazy pipeline reloads both tiers per agent turn (~1–2 min each),
# so a multi-turn task needs far more wall-clock than a single resident model. The fast Qwen3.6-35B runs
# still finish quickly under the same ceiling. Override RUN_TIMEOUT to taste.
# NCTX=32768: the matrix tasks are small (prompts + a little code), so a 32k window is ample.
# Capping it (vs the model's 262k max) shrinks the reserved KV → smaller footprint → the big 35B
# loads with MORE free-RAM headroom left for the agent's per-request prefill/decode (safer margin;
# the admission gate + MemAvailable already prevent overcommit, this just keeps runtime comfortable).
env "${EXTRA_ENV[@]}" \
  AGENTIC_MODELS="$MODELS" \
  AGENTS="claude codex opencode" \
  TASKS="greet build fix test debug rpn" \
  REPS=1 KEEP=1 RUN_TIMEOUT="${RUN_TIMEOUT:-900}" NCTX="${NCTX:-32768}" \
  REPAIR="${REPAIR:-1}" \
  ROZUM_SAMPLING_SEED=1234 \
  ROZUM_CODEX_TOOL_CAPTURE="${ROZUM_CODEX_TOOL_CAPTURE:-1}" \
  BENCH_BIN="$BIN" BENCH_OUT="$OUT" \
  bash scripts/bench/agentic.sh 2>&1 | tee "$LOG"

echo ">> done. CSV: $OUT/per-run.csv  | full log: $LOG"
# Summarize: prefer the ScalaScript port (verified byte-identical to the .py) when its toolchain is
# present — `ssc` on PATH or the sibling checkout at `../scalascript/bin/ssc`; fall back to the
# zero-dependency python (and again to python if the ssc run itself errors). Degrades gracefully so a
# missing/half-built scalascript never breaks the matrix summary.
SUMMARIZE_SSC="$(command -v ssc 2>/dev/null || echo ../scalascript/bin/ssc)"
if [ -x "$SUMMARIZE_SSC" ] && [ -f scripts/bench/summarize_matrix.ssc ]; then
  "$SUMMARIZE_SSC" run scripts/bench/summarize_matrix.ssc -- "$OUT/per-run.csv" 2>/dev/null \
    || { command -v python3 >/dev/null 2>&1 && python3 scripts/bench/summarize_matrix.py "$OUT/per-run.csv"; } \
    || true
elif command -v python3 >/dev/null 2>&1; then
  python3 scripts/bench/summarize_matrix.py "$OUT/per-run.csv" || true
fi
command -v python3 >/dev/null 2>&1 && python3 scripts/bench/matrix_capabilities.py "$OUT/per-run.csv" --out "$OUT/capabilities.json" --green-min-runs "${GREEN_MIN_RUNS:-3}" || true
if command -v python3 >/dev/null 2>&1; then
  python3 scripts/bench/memory_correctness_frontier.py "$OUT/per-run.csv" \
    --min-correctness "${FRONTIER_MIN_CORRECTNESS:-0.80}" | tee "$OUT/memory-correctness-frontier.txt" || true
  python3 scripts/bench/memory_correctness_frontier.py "$OUT/per-run.csv" \
    --min-correctness "${FRONTIER_MIN_CORRECTNESS:-0.80}" --json \
    > "$OUT/memory-correctness-frontier.json" || true
fi
# Refresh the UCC model-picker star ratings from the accumulated results (~/.rozum/ucc/model-ratings.json).
command -v python3 >/dev/null 2>&1 && python3 scripts/bench/export_model_ratings.py || true
echo ">> rerun reds only:"
echo "   scripts/bench/rerun_reds.py \"$OUT\" --nctx ${NCTX:-32768} --run-timeout ${RUN_TIMEOUT:-900}"
echo ">> slot after:"; pgrep -fl 'gateway --model' 2>/dev/null | grep -v claude || echo "  clean"
