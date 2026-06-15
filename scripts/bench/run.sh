#!/usr/bin/env bash
# Local-model benchmark harness for rozum.
#
# For each model it: starts a private gateway (offline, single resident) under
# `/usr/bin/time -l`, waits until it serves (= model load time), runs every task
# in tasks.jsonl through a STREAMING POST /v1/chat/completions, and records per
# task: TTFT (time to first content token), end-to-end time, generated tokens,
# pure decode rate (tok/s excluding prefill), a heuristic correctness PASS/FAIL
# (answer-key keyword match per task), and resident memory; plus a per-model peak
# physical-memory footprint. One model is resident at a time, torn down before
# the next, so the memory numbers are clean.
#
# TTFT note: the OpenAI SSE endpoint flushes response headers immediately, so
# curl's %{time_starttransfer} is NOT the first token. We timestamp the arrival
# of the first data chunk that carries non-empty content (via perl/Time::HiRes).
# The stream carries no usage object, so generated tokens are counted as content
# chunks (1 chunk == 1 token for the MLX backend).
#
# Correctness is a keyword answer-key match (`checks` in tasks.jsonl, ALL must
# match, case-insensitive) — a cheap signal, NOT code execution. Raw model
# outputs are saved under out/ for eyeballing real quality.
#
# Usage:
#   scripts/bench/run.sh                 # all default (installed) models, small -> large
#   scripts/bench/run.sh mlx-community:Qwen3-4B-4bit mlx-community:Phi-3-mini-4k-instruct-4bit
#   BENCH_NCTX=4096 BENCH_KILL_JAVA=1 scripts/bench/run.sh
#
# Env knobs:
#   BENCH_NCTX        context window passed to every model (default 8192)
#   BENCH_LOAD_TIMEOUT  seconds to wait for a model to load before skipping (default 600)
#   BENCH_PORT_BASE   first port; incremented per model (default 8101)
#   BENCH_BIN         rozum binary (default target/debug/rozum, then target/release/rozum)
#   BENCH_OUT         output dir (default scripts/bench/results/<timestamp>)
#   BENCH_KILL_JAVA=1 pkill stray Bloop / scala-cli JVM daemons first (they skew RAM/CPU)
#
# Requires: curl, jq, macOS /usr/bin/time -l (for peak footprint).
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
cd "$repo"

NCTX="${BENCH_NCTX:-8192}"
LOAD_TIMEOUT="${BENCH_LOAD_TIMEOUT:-600}"
PORT_BASE="${BENCH_PORT_BASE:-8101}"
OUT="${BENCH_OUT:-$here/results/$(date +%Y%m%d-%H%M%S)}"

BIN="${BENCH_BIN:-}"
if [ -z "$BIN" ]; then
  if   [ -x target/release/rozum ]; then BIN=target/release/rozum
  elif [ -x target/debug/rozum   ]; then BIN=target/debug/rozum
  else echo "no rozum binary; build with: cargo build --bin rozum" >&2; exit 1; fi
fi

# Default model set: installed mlx-community models, small -> large so quick
# feedback comes first and the heavy ones run last.
DEFAULT_MODELS=(
  mlx-community:Qwen2.5-0.5B-Instruct-4bit
  mlx-community:Qwen3-0.6B-4bit
  mlx-community:Llama-3.2-1B-Instruct-4bit
  mlx-community:gemma-3-1b-it-4bit
  mlx-community:SmolLM2-1.7B-Instruct
  mlx-community:Phi-3-mini-4k-instruct-4bit
  mlx-community:Qwen3-4B-4bit
  mlx-community:gemma-3-4b-it-4bit
  mlx-community:Mistral-7B-Instruct-v0.3-4bit
  mlx-community:Qwen2.5-Coder-7B-Instruct-4bit
  mlx-community:Qwen3.6-27B-4bit
  mlx-community:Qwen3-30B-A3B-4bit
  mlx-community:Qwen3.6-35B-A3B-4bit
)
if [ "$#" -gt 0 ]; then MODELS=("$@"); else MODELS=("${DEFAULT_MODELS[@]}"); fi

command -v jq >/dev/null || { echo "need jq" >&2; exit 1; }
[ -f "$here/tasks.jsonl" ] || { echo "missing $here/tasks.jsonl" >&2; exit 1; }

if [ "${BENCH_KILL_JAVA:-0}" = "1" ]; then
  pkill -f 'bloop' 2>/dev/null && echo "killed bloop"
  pkill -f 'scala-cli' 2>/dev/null && echo "killed scala-cli"
fi

mkdir -p "$OUT/out"
PERTASK="$OUT/per-task.csv"
PERMODEL="$OUT/per-model.csv"
echo "model,task_id,difficulty,http,ttft_s,total_s,out_tokens,decode_tps,pass,rss_mb" > "$PERTASK"
echo "model,load_seconds,peak_footprint_mb,loaded_ok" > "$PERMODEL"

# Tear down any pre-existing shared gateway so it doesn't fight for memory.
"$BIN" gateway stop --force >/dev/null 2>&1 || true

echo "rozum local-model benchmark"
echo "  binary : $BIN"
echo "  n_ctx  : $NCTX    models: ${#MODELS[@]}    out: $OUT"
echo

idx=0
for spec in "${MODELS[@]}"; do
  port=$((PORT_BASE + idx)); idx=$((idx + 1))
  base="http://127.0.0.1:$port"
  tlog="$OUT/out/${spec//[:\/]/_}.gateway.log"
  echo "=== [$idx/${#MODELS[@]}] $spec  (port $port) ==="

  # Start the gateway under /usr/bin/time -l. Offline => no remote tiers.
  # ROZUM_GATEWAY_IDLE_SECS=0 disables idle-exit so it survives the whole suite.
  ROZUM_GATEWAY_IDLE_SECS=0 /usr/bin/time -l \
    "$BIN" gateway --model "$spec" --port "$port" --offline --n-ctx "$NCTX" \
    >"$tlog" 2>&1 &
  TIME_PID=$!

  # Find the rozum child (env vars are inherited; time execs rozum directly).
  RPID=""
  for _ in $(seq 1 20); do RPID="$(pgrep -P "$TIME_PID" || true)"; [ -n "$RPID" ] && break; sleep 0.2; done

  # Wait until the HTTP server accepts connections == model finished loading
  # (the gateway builds the backend before it starts serving).
  SECONDS=0; loaded_ok=1
  until curl -s -o /dev/null "$base/health"; do
    if ! kill -0 "$TIME_PID" 2>/dev/null; then echo "  ! gateway exited during load (see $tlog)"; loaded_ok=0; break; fi
    if [ "$SECONDS" -ge "$LOAD_TIMEOUT" ]; then echo "  ! load timeout after ${SECONDS}s"; loaded_ok=0; break; fi
    sleep 1
  done
  load_secs=$SECONDS

  if [ "$loaded_ok" = "1" ]; then
    echo "  loaded in ${load_secs}s, running $(wc -l <"$here/tasks.jsonl" | tr -d ' ') tasks"
    while IFS= read -r task; do
      [ -z "$task" ] && continue
      tid=$(jq -r '.id' <<<"$task")
      diff=$(jq -r '.difficulty' <<<"$task")
      req=$(jq -c --arg m "$spec" '{model:$m, temperature:0, stream:true, max_tokens:.max_tokens, messages:[{role:"user",content:.prompt}]}' <<<"$task")
      raw="$OUT/out/${spec//[:\/]/_}.${tid}.sse"
      txtfile="$OUT/out/${spec//[:\/]/_}.${tid}.txt"

      # Stream the response; tee the raw SSE to a file while perl timestamps the
      # first content chunk (TTFT) and the stream end (total). perl reads to EOF
      # so curl is never cut short by a broken pipe.
      T0=$(perl -MTime::HiRes=time -e 'printf "%.6f", time')
      read ttft total < <(curl -N -s -X POST "$base/v1/chat/completions" \
            -H 'content-type: application/json' -H 'authorization: Bearer rozum-local' \
            -d "$req" \
          | tee "$raw" \
          | perl -MTime::HiRes=time -e 'my $s=$ARGV[0]; my $seen=0; my $t1=0;
              while(<STDIN>){ if(!$seen && /"content":"[^"]/){ $t1=time-$s; $seen=1 } }
              printf "%.4f %.4f", $t1, time-$s;' "$T0")
      ttft=${ttft:-0}; total=${total:-0}

      # Reconstruct text + count generated tokens from the saved SSE.
      grep '^data: ' "$raw" | sed 's/^data: //' | grep -v '^\[DONE\]$' \
        | jq -rs '[.[].choices[0].delta.content // ""]|join("")' > "$txtfile" 2>/dev/null
      out_tok=$(grep '^data: ' "$raw" | sed 's/^data: //' | grep -v '^\[DONE\]$' \
        | jq -s '[.[]|select(.choices[0].delta.content!=null and .choices[0].delta.content!="")]|length' 2>/dev/null)
      out_tok=${out_tok:-0}

      # Decode rate excludes prefill (the TTFT window).
      decode=$(awk -v c="$out_tok" -v t="$total" -v f="$ttft" 'BEGIN{ d=t-f; if(d>0&&c>0) printf "%.1f", c/d; else print "0" }')

      # Heuristic correctness: every `checks` regex must match (case-insensitive).
      pass=1
      while IFS= read -r rx; do
        [ -z "$rx" ] && continue
        grep -iqE "$rx" "$txtfile" || { pass=0; break; }
      done < <(jq -r '.checks[]?' <<<"$task")
      [ "$out_tok" -eq 0 ] && pass=0
      pf=$([ "$pass" = 1 ] && echo PASS || echo FAIL)

      rss_kb=$( [ -n "$RPID" ] && ps -o rss= -p "$RPID" 2>/dev/null | tr -d ' ' || echo 0)
      rss_kb=${rss_kb:-0}
      rss_mb=$(awk -v k="$rss_kb" 'BEGIN{ printf "%.0f", k/1024 }')

      printf '%s,%s,%s,%s,%s,%s,%s,%s,%s,%s\n' "$spec" "$tid" "$diff" "200" "$ttft" "$total" "$out_tok" "$decode" "$pass" "$rss_mb" >> "$PERTASK"
      printf '    %-14s ttft=%6ss  total=%7ss  out=%-4s  %7s tok/s  %s  rss=%sMB\n' "$tid" "$ttft" "$total" "$out_tok" "$decode" "$pf" "$rss_mb"
    done < "$here/tasks.jsonl"
  fi

  # Graceful shutdown so /usr/bin/time still reports the child's rusage.
  [ -n "$RPID" ] && kill -INT "$RPID" 2>/dev/null
  for _ in $(seq 1 30); do kill -0 "$TIME_PID" 2>/dev/null || break; sleep 0.5; done
  kill -KILL "$TIME_PID" 2>/dev/null || true
  wait "$TIME_PID" 2>/dev/null || true

  peak_bytes=$(grep -m1 'peak memory footprint' "$tlog" | awk '{print $1}')
  peak_mb=$(awk -v b="${peak_bytes:-0}" 'BEGIN{ printf "%.0f", b/1048576 }')
  printf '%s,%s,%s,%s\n' "$spec" "$load_secs" "$peak_mb" "$loaded_ok" >> "$PERMODEL"
  echo "  peak footprint: ${peak_mb}MB"
  echo
done

echo "============================================================"
echo "Per-model (load time + peak memory):"
column -s, -t "$PERMODEL"
echo
echo "Per-task (time + tokens/sec):"
column -s, -t "$PERTASK"
echo
echo "Raw CSVs + model outputs: $OUT"
