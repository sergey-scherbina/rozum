#!/usr/bin/env bash
# Agentic end-to-end benchmark for rozum.
#
# Drives a REAL `rozum launch claude` / `rozum launch codex` against a local MLX
# model — the whole stack as a user runs it. Per model the harness loads it ONCE
# (a shared `rozum gateway`, under /usr/bin/time -l for the resident footprint),
# then runs every task through `rozum launch` (no --model), which *reuses* that
# resident model — no reload between tasks. `rozum launch` still applies its
# Seatbelt sandbox jail (default-on) + Claude-Code prompt trimming + Codex/opencode
# provider config; every agent flag is passed on the command line. Each task is its own process tree, measured independently:
# wall time, the agent tree's peak RAM, peak CPU% (agent + gateway), pass/fail.
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
#   AGENTIC_MODELS  space-separated specs (default: Qwen3.6-35B-A3B — the standard model)
#   AGENTS          subset of "claude codex opencode" (default all three, if installed)
#   TASKS           subset of: greet build fix test debug (default all)
#   RUN_TIMEOUT     whole-task wall ceiling, seconds (default 1200)
#   GEN_TIMEOUT     ROZUM_GEN_TIMEOUT_SECS for the in-process gateway (default 180)
#   MAX_TURNS       Claude --max-turns (default 15 — caps the re-edit/retry loop
#                   weak models fall into; see SPRINT.md "agentic-loop-root-cause")
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

RUN_TIMEOUT="${RUN_TIMEOUT:-600}"
GEN_TIMEOUT="${GEN_TIMEOUT:-300}"
MAX_TURNS="${MAX_TURNS:-15}"
PORT_BASE="${BENCH_PORT_BASE:-8300}"
OUT="${BENCH_OUT:-$here/results/agentic-$(date +%Y%m%d-%H%M%S)}"
NCTX_OPT=(); [ -n "${NCTX:-}" ] && NCTX_OPT=(--n-ctx "$NCTX")

BIN="${BENCH_BIN:-}"
if [ -z "$BIN" ]; then
  if   [ -x "$repo/target/release/rozum" ]; then BIN="$repo/target/release/rozum"
  elif [ -x "$repo/target/debug/rozum"   ]; then BIN="$repo/target/debug/rozum"
  else echo "no rozum binary; build with: cargo build --release --bin rozum" >&2; exit 1; fi
fi
case "$BIN" in /*) ;; *) BIN="$repo/$BIN" ;; esac   # launch runs in a temp cwd → need absolute

# Default: the models that actually do agentic coding (4B → 35B), small → large.
# The agentic matrix found a 7B→27B capability cliff: sub-4B / weak tool models
# (Qwen2.5-0.5B, Qwen3-0.6B, Llama-3.2-1B) only manage `greet` even with the
# JSON-repair, and template-less / incompatible models (gemma, Phi-3, SmolLM2,
# Mistral-v0.3) can't drive tools at all — all dropped. Override with AGENTIC_MODELS.
# Standardized on the two kept models: Qwen3.6-35B-A3B (strongest local agentic coder, clears
# codex's apply_patch bar) + gpt-oss-20b (OpenAI reasoning MoE). Override with
# AGENTIC_MODELS="spec1 spec2 ...".
DEFAULT_MODELS="mlx-community:Qwen3.6-35B-A3B-4bit mlx-community:gpt-oss-20b-MXFP4-Q4"
read -r -a MODELS <<<"${AGENTIC_MODELS:-$DEFAULT_MODELS}"
read -r -a TASK_LIST <<<"${TASKS:-greet build fix test debug}"

AGENT_RUN=()
for a in ${AGENTS:-claude codex opencode}; do command -v "$a" >/dev/null && AGENT_RUN+=("$a") || echo "skip agent '$a' (not on PATH)"; done
[ "${#AGENT_RUN[@]}" -gt 0 ] || { echo "no agent CLIs available" >&2; exit 1; }
command -v cargo >/dev/null || { echo "need cargo" >&2; exit 1; }

mkdir -p "$OUT/runs"
CSV="$OUT/per-run.csv"
echo "agent,model,task,difficulty,seconds,pass,rc,timeout,turns,tool_uses,agent_peak_mb,peak_cpu_pct,model_footprint_mb" > "$CSV"
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
    build) echo 'In the CURRENT directory (do NOT create a subdirectory), create a minimal Rust binary project: a Cargo.toml (package name "reverse-cli", edition 2021, no dependencies) and src/main.rs. The program reverses its first command-line argument (by characters) and prints the result. Then run "cargo run -- hello" and confirm it prints "olleh". Keep it minimal. The moment the program prints "olleh", you are DONE — reply with one short confirmation line and STOP; do not run it again.' ;;
    test)  echo 'In the CURRENT directory (do NOT create a subdirectory), create a minimal Rust BINARY project: a Cargo.toml (package "reverse-cli", edition 2021, no dependencies) and src/main.rs. Implement `fn reverse(s: &str) -> String` that reverses by characters; main reads its first CLI argument and prints reverse(arg). ALSO add a `#[cfg(test)]` unit test asserting `reverse("hello") == "olleh"`. Then run "cargo test" (must pass) and "cargo run -- hello" (must print olleh). Actually implement reverse; do not just scaffold. Keep it minimal. The moment the program prints "olleh", you are DONE — reply with one short confirmation line and STOP; do not run it again.' ;;
    fix)   echo 'There is a Rust project in the current directory. Running "cargo run -- hello" should print "olleh" (the reverse of the argument) but it prints "hello". Find and fix the bug in src/main.rs, then run "cargo run -- hello" to confirm it prints "olleh". Make the minimal change; do not rewrite the whole file. Apply the fix ONCE: if an edit fails with "String to replace not found", the change is already applied — do NOT retry the edit, just run the program. The moment it prints "olleh", you are DONE — reply with one short confirmation line and STOP.' ;;
    debug) echo 'There is a Rust library in the current directory. "cargo test" fails because of a bug in src/lib.rs. Fix the bug so the test passes. Do NOT modify the test. Then run "cargo test" to confirm it passes. Make the minimal change. Apply the fix ONCE: if an edit fails with "String to replace not found", the change is already applied — do NOT retry the edit, just run the test. The moment the test passes, you are DONE — reply with one short confirmation line and STOP.' ;;
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

"$BIN" gateway stop --force >/dev/null 2>&1 || true
idx=0
for spec in "${MODELS[@]}"; do
  port=$((PORT_BASE + idx)); idx=$((idx + 1)); base="http://127.0.0.1:$port"
  glog="$OUT/runs/${spec//[:\/]/_}.gateway.log"
  echo "================ model: $spec  (port $port) ================"

  # Load the model ONCE: a shared gateway under /usr/bin/time -l. The backend's
  # cap_mlx_memory keeps the MLX cache bounded (`ROZUM_MLX_CACHE_GB`), so it no longer
  # accumulates across the model's tasks (which used to grow to ~28 GB and starve the
  # next agent — the rc=2 cascade). Each task runs `rozum launch` (no --model) → reuse.
  # ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 is REQUIRED: with the default the gateway is reloadable,
  # so the lifecycle watchdog spawns and its `clients_gone` branch (gateway.rs ~2906) self-
  # exits the process the instant one agent's leases drop — killing the shared gateway in the
  # gap between the claude and codex phases, so every later codex task sees a dead gateway and
  # returns rc=2 at 0.0s (looks like "codex is broken"; it isn't). =0 disables the watchdog so
  # the load-once gateway survives all agents. See BUGS.md BUG-001 / project-agentic-bench-clients-gone.
  ROZUM_GATEWAY_IDLE_SECS=0 ROZUM_GATEWAY_UNLOAD_IDLE_SECS=0 ROZUM_GEN_TIMEOUT_SECS="$GEN_TIMEOUT" /usr/bin/time -l \
    "$BIN" gateway --model "$spec" --port "$port" --offline "${NCTX_OPT[@]}" \
    >"$glog" 2>&1 &
  TIME_PID=$!
  GW_PID=""; for _ in $(seq 1 40); do GW_PID="$(pgrep -P "$TIME_PID" 2>/dev/null | head -1)"; [ -n "$GW_PID" ] && break; sleep 0.25; done
  ok=0
  for _ in $(seq 1 240); do curl -s -m2 "$base/v1/models" >/dev/null 2>&1 && { ok=1; break; }
    kill -0 "$TIME_PID" 2>/dev/null || break; sleep 1; done
  if [ "$ok" != 1 ]; then echo "  ! gateway not ready (see $glog)"; kill -INT "$GW_PID" 2>/dev/null; wait "$TIME_PID" 2>/dev/null; continue; fi
  echo "  model loaded once; running ${#TASK_LIST[@]} tasks × ${#AGENT_RUN[@]} agent(s)"

  for agent in "${AGENT_RUN[@]}"; do
    for task in "${TASK_LIST[@]}"; do
      diff=${DIFF[$task]:-0}
      work="$(mktemp -d /tmp/rozum-agentic-XXXXXX)"
      setup_task "$task" "$work"
      prompt="$(prompt_for "$task")"
      alog="$work/agent.log"; sfile="$work/samples.txt"
      # Build the runner per agent. ALL THREE route through `rozum launch` (no --model): it
      # reuses the resident shared gateway AND jails the agent (Seatbelt sandbox, default-on).
      # claude via Anthropic env, codex via injected provider flags, opencode via a written
      # provider config (+ `-m rozum/local`).
      if [ "$agent" = claude ]; then
        # --lean strips non-coding tools (incl. AskUserQuestion, which in headless `-p`
        # can't be answered → a model that calls it to "verify" loops until the timeout)
        # from claude's request: 33 tools / ~4.9K schema tokens → 4 / ~0.8K. Big win on a
        # local model's context/KV/prefill, and fewer ways for a weak model to derail.
        aargs=(claude -p "$prompt" --output-format stream-json --verbose
               --dangerously-skip-permissions --max-turns "$MAX_TURNS")
        runner=("$BIN" launch --no-channel-wakeup --no-piggyback --lean "${aargs[@]}")
      elif [ "$agent" = codex ]; then
        aargs=(codex exec "$prompt" --dangerously-bypass-approvals-and-sandbox)
        runner=("$BIN" launch --no-channel-wakeup --no-piggyback "${aargs[@]}")
      else  # opencode — `rozum launch` wires the gateway provider + -m rozum/local
        aargs=(opencode run "$prompt")
        runner=("$BIN" launch --no-channel-wakeup --no-piggyback "${aargs[@]}")
      fi

      start=$(perl -MTime::HiRes=time -e 'printf "%.2f", time')
      ( cd "$work"; exec "${runner[@]}" ) </dev/null >"$alog" 2>&1 &
      LP=$!
      # Agent-tree RSS + (agent + gateway) CPU; the model's RAM is the gateway footprint.
      ( while kill -0 "$LP" 2>/dev/null; do
          read ar ac < <(tree_sample "$LP")
          gc=$(ps -o pcpu= -p "$GW_PID" 2>/dev/null | tr -d ' '); gc=${gc:-0}
          awk -v ar="${ar:-0}" -v ac="${ac:-0}" -v gc="$gc" 'BEGIN{printf "%d %.1f\n", ar, ac+gc}' >>"$sfile"
          sleep 2
        done ) & SAMP=$!
      ( sleep "$RUN_TIMEOUT"; kill_descendants "$LP"; kill -TERM "$LP" 2>/dev/null ) & WD=$!
      wait "$LP"; rc=$?
      kill "$WD" "$SAMP" 2>/dev/null; wait "$SAMP" 2>/dev/null
      secs=$(perl -MTime::HiRes=time -e 'printf "%.1f", time-'"$start")
      tmo=$(awk -v s="$secs" -v t="$RUN_TIMEOUT" 'BEGIN{print (s>=t-2)?1:0}')

      agent_mb=$(awk '{if($1>m)m=$1}END{printf "%.0f", m/1024}' "$sfile" 2>/dev/null | tr -dc '0-9'); agent_mb=${agent_mb:-0}
      peak_cpu=$(awk '{if($2>m)m=$2}END{printf "%.0f", m}' "$sfile" 2>/dev/null | tr -dc '0-9'); peak_cpu=${peak_cpu:-0}

      turns="-"; tools="-"
      if [ "$agent" = claude ]; then
        turns=$(grep -c '"type":"assistant"' "$alog" 2>/dev/null | tr -dc '0-9'); turns=${turns:-0}
        tools=$(grep -o '"type":"tool_use"' "$alog" 2>/dev/null | wc -l | tr -dc '0-9'); tools=${tools:-0}
      fi

      detail="$(verify_task "$task" "$work" "$alog")"; pass=$([ $? = 0 ] && echo 1 || echo 0)
      [ "$tmo" = 1 ] && tflag=" (RUN_TIMEOUT)" || tflag=""
      printf "  [%s] %-6s %ss%s  pass=%s  agent=%sMB  cpu=%s%%  turns=%s tools=%s\n" \
        "$agent" "$task" "$secs" "$tflag" "$pass" "$agent_mb" "$peak_cpu" "$turns" "$tools"
      echo "$detail"
      printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' \
        "$agent" "$spec" "$task" "$diff" "$secs" "$pass" "$rc" "$tmo" "$turns" "$tools" "$agent_mb" "$peak_cpu" "" >> "$CSV"

      [ "${KEEP:-0}" = 1 ] && echo "    kept: $work" || rm -rf "$work"
    done
  done

  # Stop the shared gateway GRACEFULLY. A `kill -KILL` landing while the MLX worker is inside
  # a Metal eval corrupts the IOGPU driver's buffer accounting → KERNEL PANIC
  # (`IOGPUGroupMemory::remove_memory_object() not found`) that REBOOTS the Mac — see BUGS.md
  # BUG-001. So: SIGINT → the gateway's graceful shutdown drains in-flight generation, joins the
  # MLX worker, and frees buffers with the GPU idle; we wait generously for that. SIGKILL is a
  # last resort only, loudly flagged. At end-of-model the gateway is idle so graceful exit takes
  # a few seconds; the long window is insurance against a wedged eval. /usr/bin/time only flushes
  # the peak-footprint line on a CLEAN exit — another reason to avoid SIGKILL.
  TEARDOWN_GRACE="${TEARDOWN_GRACE:-180}"; GPU_SETTLE="${GPU_SETTLE:-8}"
  kill -INT "$GW_PID" 2>/dev/null
  gone=0
  for _ in $(seq 1 "$TEARDOWN_GRACE"); do kill -0 "$TIME_PID" 2>/dev/null || { gone=1; break; }; sleep 1; done
  if [ "$gone" != 1 ]; then
    echo "  ! gateway did not exit gracefully in ${TEARDOWN_GRACE}s — forcing SIGKILL"
    echo "    (PANIC RISK: a SIGKILL on a live Metal eval can panic the GPU driver and reboot the host)"
    kill -KILL "$TIME_PID" 2>/dev/null
  fi
  wait "$TIME_PID" 2>/dev/null
  # Let the kernel finish async IOGPU reclamation of the just-exited process's GPU buffers
  # before the next gateway allocates ~15-28 GB on the same Metal device (the cross-process
  # remove_memory_object race).
  sleep "$GPU_SETTLE"
  foot=$(grep -m1 'peak memory footprint' "$glog" | awk '{printf "%.0f", $1/1048576}')
  awk -F, -v m="$spec" -v f="${foot:-}" 'BEGIN{OFS=","} NR==1{print;next} $2==m{$13=f} {print}' "$CSV" > "$CSV.tmp" && mv "$CSV.tmp" "$CSV"
  echo "  model footprint: ${foot:-n/a}MB"
  echo
done

echo "============================================================"
column -s, -t "$CSV"
echo
echo "CSV + per-run logs: $OUT"
