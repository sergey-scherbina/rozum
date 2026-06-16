#!/usr/bin/env bash
# Agentic end-to-end benchmark for rozum.
#
# Drives a REAL `rozum launch claude` / `rozum launch codex` against a local MLX
# model — the whole stack as a user runs it: rozum launch starts a private
# in-process model (`--dedicated`), applies its Claude-Code prompt trimming and
# Codex provider config, and execs the agent. We pass every agent flag on the
# command line. For each (agent × model × task) it gives a real coding task
# (trivial -> hard, with tool use), verifies the result independently of the
# agent, and measures wall time, the whole process-tree peak RAM + CPU, and the
# model's resident footprint (/usr/bin/time -l on the rozum process).
#
# Two independent timeouts (per your design — model vs everything else):
#   - ROZUM_GEN_TIMEOUT_SECS  (engine, default 180): bounds a single model request
#     inside the gateway. A wedged generation aborts; the agent loop continues.
#   - RUN_TIMEOUT  (default 1200): the whole agentic task — many model calls plus
#     cargo builds and tool ops, which don't depend on any one model request.
#
# Tasks (increasing difficulty):
#   greet  - reply with one word (no tools)            -> output contains "pong"
#   build  - create reverse-cli, run it                -> cargo run -- hello == olleh
#   fix    - find+EDIT a one-line bug (returns input)  -> cargo run -- hello == olleh
#   test   - implement reverse + a #[test] + cargo test-> cargo test green & run == olleh
#   debug  - failing test, run-read-fix loop           -> cargo test green
#
# Usage:
#   scripts/bench/agentic.sh
#   AGENTIC_MODELS="mlx-community:Qwen3-30B-A3B-4bit" AGENTS=claude scripts/bench/agentic.sh
#   TASKS="greet build" RUN_TIMEOUT=600 scripts/bench/agentic.sh
#
# Env knobs:
#   AGENTIC_MODELS  space-separated specs (default: a curated tool-use-capable set)
#   AGENTS          "claude codex" (default both, if installed)
#   TASKS           subset of: greet build fix test debug (default all)
#   RUN_TIMEOUT     whole-task wall ceiling, seconds (default 1200)
#   GEN_TIMEOUT     ROZUM_GEN_TIMEOUT_SECS for the in-process gateway (default 180)
#   MAX_TURNS       Claude --max-turns (default 30)
#   NCTX            override gateway context (default: omit -> model max, auto)
#   BENCH_BIN       rozum binary (default target/release/rozum, absolute)
#   BENCH_OUT       output dir (default scripts/bench/results/agentic-<ts>)
#   KEEP=1          keep per-run workdirs
#
# Requires: claude and/or codex, cargo, jq, perl, macOS /usr/bin/time -l.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

RUN_TIMEOUT="${RUN_TIMEOUT:-1200}"
GEN_TIMEOUT="${GEN_TIMEOUT:-180}"
MAX_TURNS="${MAX_TURNS:-30}"
OUT="${BENCH_OUT:-$here/results/agentic-$(date +%Y%m%d-%H%M%S)}"
NCTX_OPT=(); [ -n "${NCTX:-}" ] && NCTX_OPT=(--n-ctx "$NCTX")

BIN="${BENCH_BIN:-}"
if [ -z "$BIN" ]; then
  if   [ -x "$repo/target/release/rozum" ]; then BIN="$repo/target/release/rozum"
  elif [ -x "$repo/target/debug/rozum"   ]; then BIN="$repo/target/debug/rozum"
  else echo "no rozum binary; build with: cargo build --release --bin rozum" >&2; exit 1; fi
fi
case "$BIN" in /*) ;; *) BIN="$repo/$BIN" ;; esac   # launch runs in a temp cwd → need absolute

# Default: every installed model whose chat template actually supports tools
# (Qwen2.5 / Qwen3 / Qwen3.6 / Llama-3.2 / Mistral-v0.3 — all `<tool_call>`-style),
# small → large. Models whose template has no tool support (gemma-3, Phi-3-mini,
# SmolLM2) are excluded — the gateway can't offer them tools. Small models still
# "know" the format but are weak at the multi-step agentic loop; that gap is the
# point. Override with AGENTIC_MODELS.
DEFAULT_MODELS="\
mlx-community:Qwen2.5-0.5B-Instruct-4bit \
mlx-community:Qwen3-0.6B-4bit \
mlx-community:Llama-3.2-1B-Instruct-4bit \
mlx-community:Qwen3-4B-4bit \
mlx-community:Mistral-7B-Instruct-v0.3-4bit \
mlx-community:Qwen2.5-Coder-7B-Instruct-4bit \
mlx-community:Qwen3.6-27B-4bit \
mlx-community:Qwen3-30B-A3B-4bit \
mlx-community:Qwen3.6-35B-A3B-4bit"
read -r -a MODELS <<<"${AGENTIC_MODELS:-$DEFAULT_MODELS}"
read -r -a TASK_LIST <<<"${TASKS:-greet build fix test debug}"

AGENT_RUN=()
for a in ${AGENTS:-claude codex}; do command -v "$a" >/dev/null && AGENT_RUN+=("$a") || echo "skip agent '$a' (not on PATH)"; done
[ "${#AGENT_RUN[@]}" -gt 0 ] || { echo "no agent CLIs available" >&2; exit 1; }
command -v cargo >/dev/null || { echo "need cargo" >&2; exit 1; }

mkdir -p "$OUT/runs"
CSV="$OUT/per-run.csv"
echo "agent,model,task,difficulty,seconds,pass,rc,timeout,turns,tool_uses,tree_peak_mb,peak_cpu_pct,model_footprint_mb" > "$CSV"
declare -A DIFF=( [greet]=1 [build]=2 [fix]=3 [test]=4 [debug]=5 )

# ── helpers ──────────────────────────────────────────────────────────────────

# Kill every descendant of $1 (not $1 itself), depth-first. Used on timeout to
# stop the agent so the rozum-launch parent unblocks, exits, and /usr/bin/time
# flushes its footprint line.
kill_descendants() {
  local pid="$1" c
  for c in $(pgrep -P "$pid" 2>/dev/null); do kill_descendants "$c"; kill -TERM "$c" 2>/dev/null; done
}

# One sample of a process tree (root + all descendants): total RSS (KB) + CPU%.
tree_sample() { # $1=root_pid
  ps -axo pid=,ppid=,rss=,pcpu= | awk -v root="$1" '
    { p=$1; ppid[p]=$2; rss[p]=$3; cpu[p]=$4; ids[++n]=p }
    END{
      inset[root]=1; changed=1
      while(changed){ changed=0
        for(i=1;i<=n;i++){ p=ids[i]; if(!inset[p] && inset[ppid[p]]){ inset[p]=1; changed=1 } } }
      r=0; c=0; for(i=1;i<=n;i++){ p=ids[i]; if(inset[p]){ r+=rss[p]; c+=cpu[p] } }
      printf "%d %.1f", r, c
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
  case "$1" in
    fix)
      printf '[package]\nname = "reverse-cli"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/main.rs" <<'EOF'
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
      printf '[package]\nname = "mathlib"\nversion = "0.1.0"\nedition = "2021"\n' >"$2/Cargo.toml"
      mkdir -p "$2/src"
      cat >"$2/src/lib.rs" <<'EOF'
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
      greet) grep -qiE '\bpong\b' "$log" && { echo "    PASS  said pong"; exit 0; } || { echo "    FAIL  no 'pong'"; exit 1; } ;;
      *)
        [ -f Cargo.toml ] || { echo "    FAIL  Cargo.toml missing"; fail=1; }
        ls src/*.rs >/dev/null 2>&1 || { echo "    FAIL  no src/*.rs"; fail=1; }
        if [ "$t" = test ] || [ "$t" = debug ]; then
          cargo test -q >/dev/null 2>"$w/cargo.err" && echo "    PASS  cargo test green" || { echo "    FAIL  cargo test red"; fail=1; }
        fi
        if [ "$t" = build ] || [ "$t" = test ] || [ "$t" = fix ]; then
          out="$(cargo run -q -- hello 2>"$w/run.err")"
          [ "$out" = olleh ] && echo "    PASS  cargo run -- hello -> olleh" || { echo "    FAIL  cargo run -> '$out'"; fail=1; }
        fi
        exit $fail ;;
    esac )
}

# ── main loop ────────────────────────────────────────────────────────────────

echo "rozum agentic benchmark (real rozum launch)"
echo "  binary       : $BIN"
echo "  agents       : ${AGENT_RUN[*]}    models: ${#MODELS[@]}    tasks: ${TASK_LIST[*]}"
echo "  run timeout  : ${RUN_TIMEOUT}s   gen timeout: ${GEN_TIMEOUT}s   ctx: ${NCTX:-auto(max)}"
echo "  out          : $OUT"
echo

for spec in "${MODELS[@]}"; do
  echo "================ model: $spec ================"
  for agent in "${AGENT_RUN[@]}"; do
    for task in "${TASK_LIST[@]}"; do
      diff=${DIFF[$task]:-0}
      work="$(mktemp -d /tmp/rozum-agentic-XXXXXX)"
      setup_task "$task" "$work"
      prompt="$(prompt_for "$task")"
      glog="$work/launch.log"; sfile="$work/samples.txt"

      if [ "$agent" = claude ]; then
        aargs=(claude -p "$prompt" --output-format stream-json --verbose
               --dangerously-skip-permissions --max-turns "$MAX_TURNS")
      else
        aargs=(codex exec "$prompt" --dangerously-bypass-approvals-and-sandbox)
      fi

      start=$(perl -MTime::HiRes=time -e 'printf "%.2f", time')
      # Real `rozum launch`, in the task workdir, under /usr/bin/time -l. The model
      # request timeout goes to the in-process gateway via ROZUM_GEN_TIMEOUT_SECS.
      ( cd "$work"; exec env ROZUM_GEN_TIMEOUT_SECS="$GEN_TIMEOUT" /usr/bin/time -l \
          "$BIN" launch --model "$spec" --dedicated --no-channel-wakeup --no-piggyback \
          "${NCTX_OPT[@]}" "${aargs[@]}" ) >"$glog" 2>&1 &
      TP=$!
      ROZ=""; for _ in $(seq 1 40); do ROZ="$(pgrep -P "$TP" 2>/dev/null | head -1)"; [ -n "$ROZ" ] && break; sleep 0.25; done

      ( while kill -0 "$TP" 2>/dev/null; do tree_sample "$TP" >>"$sfile"; echo >>"$sfile"; sleep 2; done ) & SAMP=$!
      # Whole-task watchdog: on RUN_TIMEOUT, stop the agent subtree so rozum exits
      # cleanly and time flushes the footprint (don't SIGKILL the whole tree).
      ( sleep "$RUN_TIMEOUT"; [ -n "$ROZ" ] && kill_descendants "$ROZ" ) & WD=$!
      wait "$TP"; rc=$?
      kill "$WD" "$SAMP" 2>/dev/null; wait "$SAMP" 2>/dev/null
      secs=$(perl -MTime::HiRes=time -e 'printf "%.1f", time-'"$start")
      tmo=$(awk -v s="$secs" -v t="$RUN_TIMEOUT" 'BEGIN{print (s>=t-2)?1:0}')

      tree_mb=$(awk '{if($1>m)m=$1}END{printf "%.0f", m/1024}' "$sfile" 2>/dev/null); tree_mb=${tree_mb:-0}
      peak_cpu=$(awk '{if($2>m)m=$2}END{printf "%.0f", m}' "$sfile" 2>/dev/null); peak_cpu=${peak_cpu:-0}
      foot=$(grep -m1 'peak memory footprint' "$glog" | awk '{printf "%.0f", $1/1048576}'); foot=${foot:-}

      turns="-"; tools="-"
      if [ "$agent" = claude ]; then
        turns=$(grep -c '"type":"assistant"' "$glog" 2>/dev/null || echo 0)
        tools=$(grep -o '"type":"tool_use"' "$glog" 2>/dev/null | wc -l | tr -d ' ')
      fi

      detail="$(verify_task "$task" "$work" "$glog")"; pass=$([ $? = 0 ] && echo 1 || echo 0)
      [ "$tmo" = 1 ] && tflag=" (RUN_TIMEOUT)" || tflag=""
      printf "  [%s] %-6s %ss%s  pass=%s  tree=%sMB  cpu=%s%%  model=%sMB  turns=%s tools=%s\n" \
        "$agent" "$task" "$secs" "$tflag" "$pass" "$tree_mb" "$peak_cpu" "${foot:-?}" "$turns" "$tools"
      echo "$detail"
      printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$agent" "$spec" "$task" "$diff" "$secs" "$pass" "$rc" "$tmo" "$turns" "$tools" "$tree_mb" "$peak_cpu" "$foot" >> "$CSV"

      [ "${KEEP:-0}" = 1 ] && echo "    kept: $work" || rm -rf "$work"
    done
  done
  echo
done

echo "============================================================"
column -s, -t "$CSV"
echo
echo "CSV + per-run logs: $OUT"
