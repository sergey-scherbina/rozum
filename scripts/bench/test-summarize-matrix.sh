#!/usr/bin/env bash
# The capability headline must not count cells that did not measure the model.
#
# This test exists because the mistake was made, on this repository's own data, by someone reading
# `per-run.csv` with the `pass` column and nothing else. It put the resident model at 73/88 with
# `fix` at 10/18 and produced the conclusion "the model is weak at editing code". The truth was
# 51/52: fourteen of the fifteen failures were an agent process dying, the gateway crashing or a
# timeout, most of them against a model build that had been deleted a month earlier.
#
# `summarize_matrix.py` already declared `NON_CAPABILITY_RC = {1, 2}` and its module docstring
# already promised those were "EXCLUDED from every rate". They were not: `rate()` counted every row,
# so the exclusion applied to whole models and never to a bad cell inside a good one. A promise in a
# docstring that the code does not keep is worse than no promise, and the only thing that makes it
# stay kept is a test.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fails=0
n=0

ok() { n=$((n + 1)); printf '  ok   %s\n' "$1"; }
bad() { n=$((n + 1)); fails=$((fails + 1)); printf '  FAIL %s\n' "$1"; }
want_line() { # $1=file $2=grep -E pattern $3=description
  if grep -qE "$2" "$1"; then ok "$3"; else
    bad "$3"
    printf '       looked for: %s\n' "$2"
    sed -n '1,12p' "$1" | sed 's/^/       | /'
  fi
}

hdr='agent,model,task,difficulty,seconds,pass,rc,timeout,turns,tool_uses,agent_peak_mb,peak_cpu_pct,model_footprint_mb,repairs'
row() { # agent task pass rc  → one CSV line for the curated-tier model
  printf 'claude,mlx-community:GLM-4.7-Flash-4bit,%s,2,10.0,%s,%s,0,3,2,300,50,12000,0\n' "$2" "$3" "$4"
}

# ── the case that was got wrong ──────────────────────────────────────────────────────────────────
# Four cells: two pass, one is a real capability miss (rc=10), one is the gateway falling over
# (rc=2). The honest read is 2/3 — the infra cell measured nothing. Counting it gives 2/4, and that
# is the number that becomes "the model is weak".
{
  echo "$hdr"
  row claude greet 1 0
  row claude build 1 0
  row claude fix   0 10
  row claude debug 0 2
} > "$tmp/infra.csv"

python3 "$here/summarize_matrix.py" "$tmp/infra.csv" > "$tmp/infra.out" 2>&1
want_line "$tmp/infra.out" '▶ CAPABILITY .* 2/3 ' "an infra failure is not counted against the model"
want_line "$tmp/infra.out" 'excluded from it: 1 cell\(s\)' "and the exclusion is stated, not silent"

# ── a passing cell is never excluded, whatever its rc ────────────────────────────────────────────
# 13 of the 105 rows in `agentic-ucc-1782922643` are `pass=1` carrying `rc=1`, written before the
# structured codes existed. Excluding those would silently restate every historical number — the
# same class of error, one turn of the screw further on.
{
  echo "$hdr"
  row claude greet 1 1
  row claude build 1 2
  row claude fix   0 10
} > "$tmp/legacy.csv"

python3 "$here/summarize_matrix.py" "$tmp/legacy.csv" > "$tmp/legacy.out" 2>&1
want_line "$tmp/legacy.out" '▶ CAPABILITY .* 2/3 ' "green cells with a legacy rc still count"
want_line "$tmp/legacy.out" 'excluded from it: 0 cell\(s\)' "nothing dropped when every failure is real"

# ── delivery failures DO count: they are about the model, and hiding them is the opposite bug ────
# rc=11/12/13 say the agent delivered nothing, half a project, or an untouched tree. That is a
# property of this model with this driver. Excluding them would flatter a model that never delivers.
{
  echo "$hdr"
  row claude greet 1 0
  row claude build 0 11
  row claude fix   0 12
  row claude test  0 13
} > "$tmp/delivery.csv"

python3 "$here/summarize_matrix.py" "$tmp/delivery.csv" > "$tmp/delivery.out" 2>&1
want_line "$tmp/delivery.out" '▶ CAPABILITY .* 1/4 ' "delivery failures stay in the denominator"
want_line "$tmp/delivery.out" 'excluded from it: 0 cell\(s\)' "and nothing is excluded for them"

# ── the two implementations must agree, byte for byte ────────────────────────────────────────────
# `summarize_matrix.ssc` is a declared byte-identical port. A rule added to one of them and not the
# other is the failure BUG-026 is this repository's standing lesson about.
SSC="${SSC:-}"
if [ -z "$SSC" ]; then
  for c in "$HOME/work/my/scalascript/bin/ssc" "$(command -v ssc 2>/dev/null)"; do
    [ -n "$c" ] && [ -x "$c" ] && { SSC="$c"; break; }
  done
fi
if [ -z "$SSC" ]; then
  # Said out loud. A parity check that silently becomes a no-op is how the two drift.
  printf '  SKIP parity: no ssc toolchain found (set SSC=/path/to/bin/ssc)\n'
else
  for case in infra legacy delivery; do
    "$SSC" run "$here/summarize_matrix.ssc" -- "$tmp/$case.csv" > "$tmp/$case.ssc.out" 2>/dev/null
    if cmp -s "$tmp/$case.out" "$tmp/$case.ssc.out"; then
      ok "py and ssc agree byte-for-byte on '$case'"
    else
      bad "py and ssc DIFFER on '$case'"
      diff "$tmp/$case.out" "$tmp/$case.ssc.out" | head -6 | sed 's/^/       /'
    fi
  done
fi

echo
if [ "$fails" -eq 0 ]; then
  echo "summarize-matrix: $n/$n ok"
else
  echo "summarize-matrix: $((n - fails))/$n ok, $fails FAILED" >&2
fi
exit "$([ "$fails" -eq 0 ] && echo 0 || echo 1)"
