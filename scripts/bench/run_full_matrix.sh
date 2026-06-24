#!/usr/bin/env bash
# Prepared matrix run (operator-requested): 2 models × 3-agent × all-tasks (incl. rpn).
# Captures red/green + TIME (seconds) + REASON (verify detail in stdout). KEEP=1 keeps workdirs.
# Slot-safe: models load SEQUENTIALLY (one at a time); adaptive load + admission gate prevent overcommit
# (a model that doesn't fit is REFUSED cleanly before any weights load → a matrix FAIL, never a reboot).
set -u
cd "$(dirname "$0")/../.."                                   # repo root (worktree)
BIN="$(pwd)/target/release/rozum"
STAMP="$(date +%Y%m%d-%H%M%S)"
OUT="scripts/bench/results/full-matrix-$STAMP"
LOG="/tmp/full_matrix-$STAMP.log"

# Two models (operator request), the pipeline-cascade value A/B:
#   1. Qwen3.6-35B-A3B — single strongest local agentic coder.
#   2. GLM-4-32B,gpt-oss-20b — LAZY in-process pipeline (comma-spec): GLM-32B plans → gpt-oss executes,
#      one resident at a time (in-process swap fixed: docs/pipeline-swap-bug.md). It RELOADS both tiers
#      per agent turn, so it is slow → RUN_TIMEOUT is bumped well above the single-model 280 s below.
MODELS="mlx-community:Qwen3.6-35B-A3B-4bit mlx-community:GLM-4-32B-0414-4bit,mlx-community:gpt-oss-20b-MXFP4-Q4"

# Optional safety lever (DEFAULT keep the safe 3 GiB): to squeeze the 35B in when RAM is tight, the
# operator may lower keep-free — pass KEEP_FREE_GIB=2 to this script. Left UNSET = the safe 3 GiB default.
EXTRA_ENV=()
if [ -n "${KEEP_FREE_GIB:-}" ]; then
  EXTRA_ENV+=("ROZUM_GATEWAY_MIN_FREE_RAM_BYTES=$(( KEEP_FREE_GIB * 1073741824 ))")
  echo ">> keep-free lowered to ${KEEP_FREE_GIB} GiB (safety headroom reduced — operator opt-in)"
fi

echo ">> slot check"; pgrep -fl 'gateway --model|mlx_mem_probe' 2>/dev/null | grep -viE 'claude|mcp-proxy|grep' && { echo "ABORT: slot busy"; exit 1; }
[ -f "$HOME/.rozum/residency.lock" ] && { echo "ABORT: residency lock present"; exit 1; }

echo ">> launching: 2 models × {claude,codex,opencode} × {greet,build,fix,test,debug,rpn}, REPS=1, KEEP=1"
echo ">> out=$OUT  log=$LOG"

# RUN_TIMEOUT=900: the GLM-32B,gpt-oss lazy pipeline reloads both tiers per agent turn (~1–2 min each),
# so a multi-turn task needs far more wall-clock than a single resident model. The fast Qwen3.6-35B runs
# still finish quickly under the same ceiling. Override RUN_TIMEOUT to taste.
env "${EXTRA_ENV[@]}" \
  AGENTIC_MODELS="$MODELS" \
  AGENTS="claude codex opencode" \
  TASKS="greet build fix test debug rpn" \
  REPS=1 KEEP=1 RUN_TIMEOUT="${RUN_TIMEOUT:-900}" \
  ROZUM_SAMPLING_SEED=1234 \
  BENCH_BIN="$BIN" BENCH_OUT="$OUT" \
  bash scripts/bench/agentic.sh 2>&1 | tee "$LOG"

echo ">> done. CSV: $OUT  | full log: $LOG"
echo ">> slot after:"; pgrep -fl 'gateway --model' 2>/dev/null | grep -v claude || echo "  clean"
