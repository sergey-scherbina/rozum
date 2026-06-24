#!/usr/bin/env bash
# Correctness gate for text chunked prefill (docs/specs/mistralrs-chunked-prefill.md).
#
# Chunked prefill is NOT bit-exact vs the single-pass path: the GatedDeltaNet
# linear-attention layers reorder their FP reductions across chunk boundaries, so a
# byte-diff vs unchunked is the wrong test (and on a reasoning model a single early
# argmax flip cascades). What we actually require is:
#   1. FAITHFULNESS: a secret embedded in an EARLY chunk is recalled at the final
#      position -> proves cross-chunk attention (the chunked KV) is correct.
#   2. DETERMINISM: two chunked runs at the same chunk size are byte-identical.
# CHUNK must be a multiple of the paged block size (32); mistralrs rounds it up, and
# sub-block sizes are a known footgun (they crash / drop prompt tokens).
set -uo pipefail

MODEL="${MODEL:-mlx-community/Qwen3.6-27B-4bit}"
PORT="${PORT:-8077}"
CHUNK="${CHUNK:-512}"
NCTX="${NCTX:-8192}"
BIN=target/release/rozum-gateway
# Both runs need the in-process mistralrs backend; without this the preflight
# can fall back to the LM Studio HTTP server and the comparison is meaningless.
export ROZUM_FORCE_MISTRALRS=1

SECRET="ZEPHYR-4471"
# ~1300-token filler so the prompt splits into several CHUNK-sized chunks; the secret
# sits early (chunk 0/1), the question is last, so a correct answer requires attending
# across chunk boundaries. Filler must be VARIED, not one sentence repeated: a highly
# repetitive single-pass prefill makes some models (e.g. 35B-A3B) degenerate into
# token-looping, which would break the baseline rather than test chunking.
FILLER=$(python3 -c "
import random; random.seed(7)
subj=['The maintainer','A reviewer','The on-call engineer','The release manager','Our intern','The architect']
verb=['audited','refactored','benchmarked','documented','reverted','profiled','rate-limited','sharded']
obj=['the caching layer','the device-map fit check','the paged-attention pool','the tokenizer','the retry path','the gateway router','the KV budget','the scheduler']
tail=['before lunch.','after the incident.','in staging.','without telling anyone.','to cut tail latency.','then wrote it up.','and filed a ticket.','over the weekend.']
lines=[f'{random.choice(subj)} {random.choice(verb)} {random.choice(obj)} {random.choice(tail)}' for _ in range(120)]
lines.insert(8, 'Remember this one fact: the secret access code is $SECRET.')
print(' '.join(lines))")
PROMPT="$FILLER

Ignore all the text above except one fact. What is the secret access code? Reply with only the code, nothing else."

req() {
  curl -s "http://127.0.0.1:${PORT}/v1/chat/completions" \
    -H 'content-type: application/json' \
    -d "$(jq -n --arg m "$MODEL" --arg p "$PROMPT" \
      '{model:$m, temperature:0, max_tokens:800, stream:false,
        messages:[{role:"user", content:$p}]}')" \
  | jq -r '.choices[0].message.content // .error // .'
}

run() {
  local label="$1"; shift
  echo "### $label" >&2
  "$@" $BIN gateway --model "$MODEL" --port "$PORT" --n-ctx "$NCTX" >/tmp/cpf_$label.log 2>&1 &
  local pid=$!
  for _ in $(seq 1 180); do
    grep -q "ready (context" /tmp/cpf_$label.log 2>/dev/null && break
    kill -0 $pid 2>/dev/null || { echo "gateway died; see /tmp/cpf_$label.log" >&2; tail -20 /tmp/cpf_$label.log >&2; return 1; }
    sleep 1
  done
  sleep 1
  req > /tmp/cpf_$label.out
  kill $pid 2>/dev/null; wait $pid 2>/dev/null
}

run unchunked
run chunked  env MISTRALRS_PREFILL_CHUNK=$CHUNK
run chunked2 env MISTRALRS_PREFILL_CHUNK=$CHUNK

echo "=== unchunked ===";          cat /tmp/cpf_unchunked.out
echo "=== chunked (CHUNK=$CHUNK) ==="; cat /tmp/cpf_chunked.out
echo "==="

rc=0
# 1. Faithfulness: the secret must survive chunked prefill.
if grep -q "$SECRET" /tmp/cpf_unchunked.out; then echo "ok:   unchunked recalled $SECRET"; else echo "FAIL: unchunked did NOT recall $SECRET (baseline broken)"; rc=1; fi
if grep -q "$SECRET" /tmp/cpf_chunked.out;   then echo "PASS: chunked recalled $SECRET across chunk boundaries"; else echo "FAIL: chunked did NOT recall $SECRET (prefill corruption)"; rc=1; fi
# 2. Determinism: greedy chunked output is reproducible.
if diff -q /tmp/cpf_chunked.out /tmp/cpf_chunked2.out >/dev/null; then echo "PASS: chunked is deterministic"; else echo "FAIL: chunked nondeterministic"; diff /tmp/cpf_chunked.out /tmp/cpf_chunked2.out; rc=1; fi

[ $rc -eq 0 ] && echo "GATE PASS" || echo "GATE FAIL"
exit $rc
