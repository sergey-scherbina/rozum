#!/usr/bin/env bash
# rozum solve — ADAPTIVE N-model plan→execute→verify→critic→repair loop.
#
# Give a LIST of models (one, two, three, …); roles auto-assign and the loop runs BY DEFAULT,
# no extra flags. With one model it plays every role (and still gets the deterministic cargo
# check + repair loop). The whole point: a weak model alone fails silently; wrapped in
# plan→do→CHECK(cargo, the ground truth)→critique→fix it either converges or fails fast+honestly.
#
#   MODELS="A B C" scripts/bench/solve.sh "<task>"
#   MODELS="mlx-community:Qwen3.6-35B-A3B-4bit" solve.sh "<task>"     # one model, all roles
#
# Roles from the ORDERED list (degrade gracefully):
#   planner  = MODELS[0]                      — reasons out the complete solution (GLM/35B strong)
#   executor = MODELS[1] if ≥2 else MODELS[0] — the agent that lands+fixes code (gpt-oss/35B)
#   critic   = MODELS[2] if ≥3 else MODELS[0] — interprets a real failure into a fix (35B/GLM)
# (critic falls back to the planner — both are reasoning roles — and executor to the planner.)
#
# ORDER MATTERS — put your STRONGEST reasoner first (it plans, and critiques when N<3). The critic
# drives convergence: it turns a real failure into the next fix. A WEAK critic gives useless/empty
# guidance and the loop stalls. Measured A/B on rpn from the SAME buggy gpt-oss plan: critic=gpt-oss
# → NOT SOLVED in 3 rounds; critic=Qwen3.6-35B → SOLVED in 2 (compile fix → arg-split fix →
# stderr→stdout fix). So spend your best model on PLAN and CRITIC; the executor can be cheaper.
#
# Pipeline (per task):
#   1. PLAN    planner → the complete solution as structured files (=== FILE: path ===).
#   2. LAND    write_solution.py writes them (deterministic — most reliable for create-from-scratch).
#   3. CHECK   `cargo build` (+ optional run-output + `cargo test`) — DETERMINISTIC ground truth.
#              The judge is cargo, NEVER a model (a model "looks good" is the false-success trap).
#   4. if red: CRITIC reads the REAL error+files → says what to fix → EXECUTOR (agent) applies it.
#   5. LOOP    back to CHECK, up to ROUNDS times, until green or the budget is spent.
#
# Residency is OPTIMAL: ONE gateway, switched IN-PROCESS (the fixed model-swap) only when the next
# phase needs a DIFFERENT model — so a 1-model run never swaps; a 3-model run swaps only on a role
# change. One model resident at a time → fits a 36 GB no-reboot host (admission gate guards it).
set -u
here="$(cd "$(dirname "$0")" && pwd)"; root="$(cd "$here/../.." && pwd)"
BIN="${BIN:-$root/target/release/rozum-gateway}"

# Model list → roles. MODELS wins; else the legacy PLANNER/EXECUTOR pair; else a sane default trio.
if [ -n "${MODELS:-}" ]; then read -r -a M <<<"$MODELS"
elif [ -n "${PLANNER:-}${EXECUTOR:-}" ]; then M=("${PLANNER:-mlx-community:GLM-4-32B-0414-4bit}" "${EXECUTOR:-mlx-community:gpt-oss-20b-MXFP4-Q4}")
else M=(mlx-community:GLM-4-32B-0414-4bit mlx-community:gpt-oss-20b-MXFP4-Q4 mlx-community:Qwen3.6-35B-A3B-4bit); fi
N=${#M[@]}
PLANNER="${M[0]}"
EXECUTOR="${M[$(( N>=2 ? 1 : 0 ))]}"
CRITIC="${M[$(( N>=3 ? 2 : 0 ))]}"

AGENT="${AGENT:-claude}"               # the CLI that drives the EXECUTOR model (claude/codex/opencode)
ROUNDS="${ROUNDS:-3}"                   # max repair rounds after the initial land
PORT="${PORT:-8313}"; BASE="http://127.0.0.1:$PORT"
NCTX="${NCTX:-32768}"
TASK="${1:?usage: MODELS=\"A B C\" solve.sh \"<task>\"}"
WORK="${WORK:-$(mktemp -d /tmp/solve-XXXXXX)}"; mkdir -p "$WORK"
# DETERMINISTIC verify gate: exit 0 = solved. Default = it must compile; if a unit test exists it
# must pass; and if VERIFY_ARG is set, `cargo run -- $VERIFY_ARG` must equal VERIFY_EXPECT.
VERIFY_ARG="${VERIFY_ARG:-}"; VERIFY_EXPECT="${VERIFY_EXPECT:-}"

echo "═══ rozum solve — $N model(s), up to $ROUNDS repair round(s) ═══"
echo "  planner : $PLANNER"
echo "  executor: $EXECUTOR   (agent: $AGENT)"
echo "  critic  : $CRITIC"
echo "  task    : $TASK"
echo "  work    : $WORK"
echo

GWPID=""; current=""
stop_gw(){
  [ -n "$GWPID" ] && { kill -INT "$GWPID" 2>/dev/null; for _ in $(seq 1 90); do kill -0 "$GWPID" 2>/dev/null||break; sleep 1; done; GWPID=""; }
  "$BIN" gateway stop >/dev/null 2>&1 || true
  for _ in $(seq 1 60); do pgrep -f 'release/rozum-gateway gateway --model' >/dev/null 2>&1 || break; sleep 1; done
}
trap stop_gw EXIT
wait_ready(){ for _ in $(seq 1 240); do curl -s -m2 "$BASE/v1/models" >/dev/null 2>&1 && return 0; kill -0 "$GWPID" 2>/dev/null||return 1; sleep 1; done; return 1; }

# Switch the single gateway to $1 in-process — ONLY if it isn't already that model (so same-model
# phases cost nothing). Uses the fixed /control/switch swap path.
ensure(){
  local m="$1"
  [ "$m" = "$current" ] && return 0
  echo "  ↻ swap → $m"
  local r; r="$(curl -s -m180 "$BASE/control/switch" -H 'content-type: application/json' -d "{\"model\":\"$m\"}")"
  echo "$r" | grep -q '"status":"switched"' || { echo "  !! switch failed: $r"; return 1; }
  current="$m"
}

# One-shot chat to the currently-loaded model. $1=prompt $2=max_tokens. Echoes the reply.
chat(){
  python3 - "$BASE" "$1" "${2:-1500}" <<'PY'
import json,sys,urllib.request
base,prompt,mt=sys.argv[1],sys.argv[2],int(sys.argv[3])
model=json.load(urllib.request.urlopen(base+"/v1/models"))["data"][0]["id"]
body=json.dumps({"model":model,"temperature":0.2,"max_tokens":mt,"messages":[{"role":"user","content":prompt}]}).encode()
try:
  d=json.load(urllib.request.urlopen(urllib.request.Request(base+"/v1/chat/completions",body,{"content-type":"application/json"}),timeout=300))
  print(d["choices"][0]["message"]["content"])
except Exception as e: print("CHAT-ERROR:%s"%e)
PY
}

# DETERMINISTIC check — the ground truth. Writes the real error to $WORK/check.err; exit 0 = solved.
check(){
  ( cd "$WORK" || exit 1
    cargo build -q 2>"$WORK/check.err" || exit 1
    if [ -n "$VERIFY_ARG" ]; then
      out="$(timeout 60 cargo run -q -- "$VERIFY_ARG" 2>>"$WORK/check.err")"
      [ "$out" = "$VERIFY_EXPECT" ] || { printf '`cargo run -- %s` printed %q (expected: %q)\n' "$VERIFY_ARG" "$out" "$VERIFY_EXPECT" >>"$WORK/check.err"; exit 1; }
    fi
    if grep -rqs '#\[test\]' src 2>/dev/null; then
      cargo test -q >>"$WORK/check.err" 2>&1 || exit 1
    fi
    exit 0 )
}
diag(){ grep -vE '^\s*(Compiling|Finished|Updating|Blocking|Downloaded|Running)' "$WORK/check.err" 2>/dev/null | head -40; }
files_dump(){ ( cd "$WORK" && for f in Cargo.toml src/*.rs; do [ -f "$f" ] && { echo "=== $f ==="; cat "$f"; }; done ) 2>/dev/null | head -120; }

# ── 0. load the planner (first model) ────────────────────────────────────────
ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 \
  "$BIN" gateway --model "$PLANNER" --port "$PORT" --n-ctx "$NCTX" --offline >"$WORK/gw.log" 2>&1 & GWPID=$!
current="$PLANNER"
wait_ready || { echo "!! gateway did not load"; grep -iE 'refus|overcommit|not ready' "$WORK/gw.log"|tail -3; exit 1; }

# ── 1. PLAN  +  2. LAND ──────────────────────────────────────────────────────
echo "── PLAN ($PLANNER) ──"
PLAN_PROMPT="$TASK

Output the COMPLETE solution — every file's full final contents — each EXACTLY in this structured format (NO markdown code fences):
=== FILE: <relative/path> ===
<the full raw file contents>
=== END ===
Emit one block per file (e.g. Cargo.toml, src/main.rs). Do NOT run anything; just emit the files."
chat "$PLAN_PROMPT" 1800 >"$WORK/solution.md"
echo "  plan: $(wc -l <"$WORK/solution.md") lines → $WORK/solution.md"
WROTE=$(python3 "$here/write_solution.py" "$WORK" "$WORK/solution.md" 2>/dev/null || true)
echo "  landed: $(echo "$WROTE" | tr '\n' ' ')"
# Make WORK a cargo workspace ROOT so cargo doesn't walk up into a stray parent Cargo.toml.
[ -f "$WORK/Cargo.toml" ] && ! grep -q '\[workspace\]' "$WORK/Cargo.toml" && printf '\n[workspace]\n' >> "$WORK/Cargo.toml"

# ── 3/4/5. CHECK → (CRITIC → EXECUTOR-FIX) loop ──────────────────────────────
round=0; pass=0
while :; do
  if check; then pass=1; break; fi
  [ "$round" -ge "$ROUNDS" ] && break
  round=$((round+1))
  echo "── round $round/$ROUNDS — CHECK red ──"; diag | sed 's/^/    /' | head -8
  ERR="$(diag)"

  # CRITIC: a model turns the REAL error into a concrete fix instruction (advisor, not judge).
  ensure "$CRITIC" || break
  echo "  ── CRITIC ($CRITIC) ──"
  CRIT_PROMPT="A Rust project fails its check. The EXACT error:
$ERR

Current files:
$(files_dump)

In 3-6 lines say SPECIFICALLY what to change to make it pass (exact edits/lines). No code fences, no full rewrite — just the concrete fix."
  CRITIQUE="$(chat "$CRIT_PROMPT" 500)"
  echo "$CRITIQUE" | sed 's/^/    /' | head -8

  # EXECUTOR: the agent applies the fix in $WORK (real tool calls), then we re-CHECK (not its word).
  ensure "$EXECUTOR" || break
  echo "  ── EXECUTOR ($AGENT @ $EXECUTOR) ──"
  FIX_PROMPT="There is a Rust project in the current directory that does NOT pass its check yet — do NOT start over, FIX the existing files.

The exact failure:
$ERR

Guidance on how to fix it:
$CRITIQUE

Make the minimal change, then ACTUALLY run the build/test yourself and read the output. Only stop when it really passes."
  ( cd "$WORK" && case "$AGENT" in
      claude)   "$BIN" launch --no-channel-wakeup --no-piggyback --lean claude -p "$FIX_PROMPT" --output-format stream-json --verbose --dangerously-skip-permissions --max-turns 15 ;;
      codex)    "$BIN" launch --no-channel-wakeup --no-piggyback codex exec "$FIX_PROMPT" --dangerously-bypass-approvals-and-sandbox ;;
      opencode) "$BIN" launch --no-channel-wakeup --no-piggyback opencode run "$FIX_PROMPT" ;;
      *) echo "unknown AGENT=$AGENT"; exit 2 ;;
    esac ) >"$WORK/executor_r${round}.log" 2>&1
done

# ── result ───────────────────────────────────────────────────────────────────
echo
echo "═══ RESULT — $([ "$pass" = 1 ] && echo "✅ SOLVED" || echo "❌ NOT SOLVED") after $round repair round(s) ═══"
( cd "$WORK" && ls -1 Cargo.toml src/*.rs 2>/dev/null | sed 's/^/  /' )
[ "$pass" != 1 ] && { echo "  last error:"; diag | sed 's/^/    /' | head -6; }
echo "  kept: $WORK"
[ "$pass" = 1 ]
