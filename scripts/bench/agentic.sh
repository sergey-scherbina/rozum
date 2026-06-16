#!/usr/bin/env bash
# Agentic end-to-end benchmark for rozum.
#
# Drives a real, headless coding agent (Claude Code and/or OpenAI Codex) against
# a local MLX model through the rozum gateway — exactly what `rozum launch claude`
# / `rozum launch codex` set up, but with an explicit gateway so the harness owns
# the lifecycle and can attribute resources. For every (agent × model × task) it:
#   - gives the agent a real coding task (trivial -> hard, with tool use),
#   - verifies the result independently of the agent (files, cargo test, run output),
#   - measures wall time, the agent process-tree peak RAM, peak combined CPU%, and
#     the model's resident footprint (gateway, /usr/bin/time -l).
#
# Tasks (increasing difficulty):
#   greet  - reply with one word (no tools)            -> output contains "pong"
#   build  - create reverse-cli, run it                -> cargo run -- hello == olleh
#   fix    - find+EDIT a one-line bug (returns input)  -> cargo run -- hello == olleh
#   test   - implement reverse + a #[test] + cargo test-> cargo test green & run == olleh
#   debug  - failing test, run-read-fix loop           -> cargo test green
#
# Usage:
#   scripts/bench/agentic.sh                       # default models x agents x tasks
#   AGENTIC_MODELS="mlx-community:Qwen3-4B-4bit" AGENTS=claude scripts/bench/agentic.sh
#   TASKS="greet build" scripts/bench/agentic.sh
#
# Env knobs:
#   AGENTIC_MODELS  space-separated specs (default: a curated tool-use-capable set)
#   AGENTS          "claude codex" (default both, if installed)
#   TASKS           subset of: greet build fix test debug (default all)
#   AGENTIC_TIMEOUT per-run wall ceiling, seconds (default 300)
#   MAX_TURNS       Claude --max-turns (default 25)
#   NCTX            gateway context window (default 16384)
#   BENCH_BIN       rozum binary (default target/release/rozum)
#   BENCH_OUT       output dir (default scripts/bench/results/agentic-<ts>)
#   KEEP=1          keep per-run workdirs for inspection
#
# Requires: claude and/or codex, cargo, jq, curl, macOS /usr/bin/time -l.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

NCTX="${NCTX:-16384}"
TIMEOUT="${AGENTIC_TIMEOUT:-300}"
MAX_TURNS="${MAX_TURNS:-25}"
PORT_BASE="${BENCH_PORT_BASE:-8300}"
OUT="${BENCH_OUT:-$here/results/agentic-$(date +%Y%m%d-%H%M%S)}"

BIN="${BENCH_BIN:-}"
if [ -z "$BIN" ]; then
  if   [ -x target/release/rozum ]; then BIN=target/release/rozum
  elif [ -x target/debug/rozum   ]; then BIN=target/debug/rozum
  else echo "no rozum binary; build with: cargo build --release --bin rozum" >&2; exit 1; fi
fi

# Curated default: models that can actually drive tool use. Tiny models (<=1B)
# flail on agentic tasks (no usable tool-calling) and just burn the timeout, so
# they're excluded by default — override with AGENTIC_MODELS to include them.
DEFAULT_MODELS="mlx-community:Qwen2.5-Coder-7B-Instruct-4bit mlx-community:Qwen3-30B-A3B-4bit"
read -r -a MODELS <<<"${AGENTIC_MODELS:-$DEFAULT_MODELS}"
read -r -a TASK_LIST <<<"${TASKS:-greet build fix test debug}"

# Agents: default both, drop any not installed.
AGENT_SEL="${AGENTS:-claude codex}"
AGENT_RUN=()
for a in $AGENT_SEL; do command -v "$a" >/dev/null && AGENT_RUN+=("$a") || echo "skip agent '$a' (not on PATH)"; done
[ "${#AGENT_RUN[@]}" -gt 0 ] || { echo "no agent CLIs available" >&2; exit 1; }
command -v cargo >/dev/null || { echo "need cargo" >&2; exit 1; }
command -v jq    >/dev/null || { echo "need jq" >&2; exit 1; }

mkdir -p "$OUT/runs"
CSV="$OUT/per-run.csv"
echo "agent,model,task,difficulty,seconds,pass,rc,turns,tool_uses,agent_peak_mb,peak_cpu_pct,model_footprint_mb" > "$CSV"

declare -A DIFF=( [greet]=1 [build]=2 [fix]=3 [test]=4 [debug]=5 )

# ── helpers ──────────────────────────────────────────────────────────────────

# Sum RSS (KB) + CPU% over a process tree (root + all descendants), plus the
# gateway's CPU. Prints: agent_tree_rss_kb  agent_tree_cpu  gw_cpu
tree_stats() { # $1=agent_root_pid  $2=gw_pid
  ps -axo pid=,ppid=,rss=,pcpu= | awk -v ag="$1" -v gw="$2" '
    { p=$1; ppid[p]=$2; rss[p]=$3; cpu[p]=$4; ids[++n]=p }
    END{
      inset[ag]=1; changed=1
      while(changed){ changed=0
        for(i=1;i<=n;i++){ p=ids[i]; if(!inset[p] && inset[ppid[p]]){ inset[p]=1; changed=1 } } }
      ar=0; ac=0
      for(i=1;i<=n;i++){ p=ids[i]; if(inset[p]){ ar+=rss[p]; ac+=cpu[p] } }
      printf "%d %.1f %.1f", ar, ac, cpu[gw]+0
    }'
}

prompt_for() {
  case "$1" in
    greet) echo 'Reply with exactly the single word: pong  (nothing else, no punctuation).' ;;
    build) echo 'Create a minimal Rust binary project in the current directory: a Cargo.toml (package name "reverse-cli", edition 2021, no dependencies) and src/main.rs. The program reverses its first command-line argument (by characters) and prints the result. Then run "cargo run -- hello" and confirm it prints "olleh". Keep it minimal.' ;;
    test)  echo 'In the CURRENT directory (do NOT create a subdirectory), create a minimal Rust BINARY project: a Cargo.toml (package "reverse-cli", edition 2021, no dependencies) and src/main.rs. Implement `fn reverse(s: &str) -> String` that reverses by characters; main reads its first CLI argument and prints reverse(arg). ALSO add a `#[cfg(test)]` unit test asserting `reverse("hello") == "olleh"`. Then run "cargo test" (must pass) and "cargo run -- hello" (must print olleh). Actually implement reverse; do not just scaffold. Keep it minimal.' ;;
    fix)   echo 'There is a Rust project in the current directory. Running "cargo run -- hello" should print "olleh" (the reverse of the argument) but it prints "hello". Find and fix the bug in src/main.rs, then run "cargo run -- hello" to confirm it prints "olleh". Make the minimal change; do not rewrite the whole file.' ;;
    debug) echo 'There is a Rust library in the current directory. "cargo test" fails because of a bug in src/lib.rs. Fix the bug so the test passes. Do NOT modify the test. Then run "cargo test" to confirm it passes. Make the minimal change.' ;;
  esac
}

setup_task() { # $1=task  $2=workdir — pre-create files for fix/debug
  local t="$1" w="$2"
  case "$t" in
    fix)
      printf '[package]\nname = "reverse-cli"\nversion = "0.1.0"\nedition = "2021"\n' >"$w/Cargo.toml"
      mkdir -p "$w/src"
      cat >"$w/src/main.rs" <<'EOF'
use std::env;

/// Reverse a string by characters.
fn reverse(s: &str) -> String {
    // BUG: returns the input unchanged.
    s.to_string()
}

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    println!("{}", reverse(&arg));
}
EOF
      ;;
    debug)
      printf '[package]\nname = "mathlib"\nversion = "0.1.0"\nedition = "2021"\n' >"$w/Cargo.toml"
      mkdir -p "$w/src"
      cat >"$w/src/lib.rs" <<'EOF'
/// Add two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a - b
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }
}
EOF
      ;;
  esac
}

verify_task() { # $1=task  $2=workdir  $3=agent_log — echoes detail, returns 0=pass
  local t="$1" w="$2" log="$3" fail=0
  ( cd "$w"
    case "$t" in
      greet)
        if grep -qiE '\bpong\b' "$log"; then echo "    PASS  said pong"; else echo "    FAIL  no 'pong' in output"; exit 1; fi ;;
      *)
        [ -f Cargo.toml ] || { echo "    FAIL  Cargo.toml missing"; fail=1; }
        ls src/*.rs >/dev/null 2>&1 || { echo "    FAIL  no src/*.rs"; fail=1; }
        if [ "$t" = test ] || [ "$t" = debug ]; then
          if cargo test -q >/dev/null 2>"$w/cargo.err"; then echo "    PASS  cargo test green"; else echo "    FAIL  cargo test red"; fail=1; fi
        fi
        if [ "$t" = build ] || [ "$t" = test ] || [ "$t" = fix ]; then
          out="$(cargo run -q -- hello 2>"$w/run.err")"
          if [ "$out" = olleh ]; then echo "    PASS  cargo run -- hello -> olleh"; else echo "    FAIL  cargo run -> '$out'"; fail=1; fi
        fi
        exit $fail ;;
    esac )
}

# ── main loop ────────────────────────────────────────────────────────────────

echo "rozum agentic benchmark"
echo "  binary : $BIN"
echo "  agents : ${AGENT_RUN[*]}    models: ${#MODELS[@]}    tasks: ${TASK_LIST[*]}"
echo "  out    : $OUT"
echo

"$BIN" gateway stop --force >/dev/null 2>&1 || true
idx=0
for spec in "${MODELS[@]}"; do
  port=$((PORT_BASE + idx)); idx=$((idx + 1))
  base="http://127.0.0.1:$port"
  glog="$OUT/runs/${spec//[:\/]/_}.gateway.log"
  echo "================ model: $spec  (port $port) ================"

  ROZUM_GATEWAY_IDLE_SECS=0 /usr/bin/time -l \
    "$BIN" gateway --model "$spec" --port "$port" --offline --n-ctx "$NCTX" \
    >"$glog" 2>&1 &
  TIME_PID=$!
  GW_PID=""
  for _ in $(seq 1 20); do GW_PID="$(pgrep -P "$TIME_PID" || true)"; [ -n "$GW_PID" ] && break; sleep 0.2; done

  ok=0
  for _ in $(seq 1 120); do curl -s -m2 "$base/v1/models" >/dev/null 2>&1 && { ok=1; break; }
    kill -0 "$TIME_PID" 2>/dev/null || break; sleep 1; done
  if [ "$ok" != 1 ]; then echo "  ! gateway not ready (see $glog)"; kill -INT "$GW_PID" 2>/dev/null; wait "$TIME_PID" 2>/dev/null; continue; fi
  alias="$(curl -s "$base/v1/models" | jq -r '.data[0].id')"
  echo "  ready; model alias = $alias"

  for agent in "${AGENT_RUN[@]}"; do
    for task in "${TASK_LIST[@]}"; do
      diff=${DIFF[$task]:-0}
      work="$(mktemp -d "/tmp/rozum-agentic-XXXXXX")"
      setup_task "$task" "$work"
      prompt="$(prompt_for "$task")"
      alog="$work/agent.log"
      sfile="$work/samples.txt"

      # Launch the agent headless in the background; sample its process tree + the
      # gateway CPU once a second until it exits or the timeout fires.
      start=$(perl -MTime::HiRes=time -e 'printf "%.2f", time')
      if [ "$agent" = claude ]; then
        ( cd "$work"
          export ANTHROPIC_BASE_URL="$base" ANTHROPIC_API_KEY="rozum-local"
          export ANTHROPIC_MODEL="$alias" ANTHROPIC_DEFAULT_SONNET_MODEL="$alias"
          timeout "$TIMEOUT" claude -p "$prompt" --model "$alias" \
            --dangerously-skip-permissions --max-turns "$MAX_TURNS" \
            --output-format stream-json --verbose ) >"$alog" 2>&1 &
      else
        ( export OPENAI_API_KEY="rozum-local"
          timeout "$TIMEOUT" codex exec "$prompt" -m local \
            --dangerously-bypass-approvals-and-sandbox -C "$work" \
            -c model_provider=rozum -c 'model_providers.rozum.name="rozum"' \
            -c "model_providers.rozum.base_url=\"$base/v1\"" \
            -c 'model_providers.rozum.wire_api="responses"' \
            -c 'model_providers.rozum.env_key="OPENAI_API_KEY"' ) >"$alog" 2>&1 &
      fi
      AGP=$!

      ( while kill -0 "$AGP" 2>/dev/null; do tree_stats "$AGP" "$GW_PID" >>"$sfile"; echo >>"$sfile"; sleep 1; done ) &
      SAMP=$!
      wait "$AGP"; rc=$?
      kill "$SAMP" 2>/dev/null; wait "$SAMP" 2>/dev/null
      secs=$(perl -MTime::HiRes=time -e 'printf "%.1f", time-'"$start")

      # Peak resources from the samples.
      peak_mb=$(awk '{if($1>m)m=$1}END{printf "%.0f", m/1024}' "$sfile" 2>/dev/null); peak_mb=${peak_mb:-0}
      peak_cpu=$(awk '{c=$2+$3; if(c>m)m=c}END{printf "%.0f", m}' "$sfile" 2>/dev/null); peak_cpu=${peak_cpu:-0}

      # Claude stream-json (NDJSON) carries turn/tool counts; count by substring
      # (tolerant of any non-JSON noise). Codex doesn't emit this.
      turns="-"; tools="-"
      if [ "$agent" = claude ]; then
        turns=$(grep -c '"type":"assistant"' "$alog" 2>/dev/null || echo 0)
        tools=$(grep -o '"type":"tool_use"' "$alog" 2>/dev/null | wc -l | tr -d ' ')
      fi

      detail="$(verify_task "$task" "$work" "$alog")"; pass=$([ $? = 0 ] && echo 1 || echo 0)
      [ "$rc" = 124 ] && tmo=" (TIMEOUT)" || tmo=""
      printf "  [%s] %-6s %ss%s  pass=%s  mem=%sMB  cpu=%s%%  turns=%s tools=%s\n" \
        "$agent" "$task" "$secs" "$tmo" "$pass" "$peak_mb" "$peak_cpu" "$turns" "$tools"
      echo "$detail"
      printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$agent" "$spec" "$task" "$diff" "$secs" "$pass" "$rc" "$turns" "$tools" "$peak_mb" "$peak_cpu" "" >> "$CSV"

      [ "${KEEP:-0}" = 1 ] && echo "    kept: $work" || rm -rf "$work"
    done
  done

  # Graceful stop so /usr/bin/time prints the peak footprint. A run killed by its
  # timeout can leave an in-flight generation the gateway must drain first, so be
  # patient (up to 60s) before the hard kill.
  kill -INT "$GW_PID" 2>/dev/null
  for _ in $(seq 1 60); do kill -0 "$TIME_PID" 2>/dev/null || break; sleep 1; done
  kill -KILL "$TIME_PID" 2>/dev/null; wait "$TIME_PID" 2>/dev/null
  foot=$(grep -m1 'peak memory footprint' "$glog" | awk '{printf "%.0f", $1/1048576}')
  # Backfill the model footprint column for this model's rows.
  awk -F, -v m="$spec" -v f="${foot:-}" 'BEGIN{OFS=","} NR==1{print;next} $2==m{$12=f} {print}' "$CSV" > "$CSV.tmp" && mv "$CSV.tmp" "$CSV"
  echo "  model footprint: ${foot:-n/a}MB"
  echo
done

echo "============================================================"
column -s, -t "$CSV"
echo
echo "CSV + per-run logs: $OUT"
