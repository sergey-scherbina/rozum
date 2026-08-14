#!/usr/bin/env bash
# Execute `classify_rc` from `agentic.sh` against real directories on disk.
#
# The structured exit codes have been the boards' vocabulary for a year — "0 rc11" is quoted as
# evidence in a dozen entries — and until now nothing ran them. The rule lived inside a 1000-line
# harness that needs a model, a gateway and ~10 minutes to reach the four lines that decide the
# number, so every change to it was reasoned about and none was checked. That is how rc=10 came to
# mean two different things (BACKLOG `bench-rc-partial-delivery`).
#
# So: source the harness (its guard stops before any benchmark), build the workdir shapes by hand,
# and assert the code. Runs in well under a second, needs no model and no network.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=/dev/null
source "$here/agentic.sh"
if ! declare -F classify_rc >/dev/null; then
  echo "FAIL: sourcing agentic.sh did not define classify_rc — the source guard moved or broke" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fails=0
n=0

# $1=name $2=expected $3=task $4=workdir $5=tmo $6=raw_rc $7=pass
check() {
  local name="$1" want="$2" got
  n=$((n + 1))
  got="$(classify_rc "$3" "$4" "$5" "$6" "$7")"
  if [ "$got" = "$want" ]; then
    printf '  ok   %-42s rc=%s\n' "$name" "$got"
  else
    printf '  FAIL %-42s rc=%s, wanted %s\n' "$name" "$got" "$want"
    fails=$((fails + 1))
  fi
}

mk() { # $1=name, then flags: manifest src lib emptysrc rootrs seed
  local d="$tmp/$1"; shift
  rm -rf "$d"; mkdir -p "$d"
  local what
  for what in "$@"; do
    case "$what" in
      manifest) printf '[package]\nname = "x"\nversion = "0.1.0"\nedition = "2021"\n' >"$d/Cargo.toml" ;;
      src)      mkdir -p "$d/src"; printf 'fn main() {}\n' >"$d/src/main.rs" ;;
      lib)      mkdir -p "$d/src"; printf 'pub fn f() {}\n' >"$d/src/lib.rs" ;;
      emptysrc) mkdir -p "$d/src" ;;
      rootrs)   printf 'fn main() {}\n' >"$d/main.rs" ;;
      # `seed` must come LAST in the flag list — it records the state as the seeded baseline, the
      # way `setup_task` does. Anything written after it counts as the agent's work.
      seed)     seed_manifest "$d" ;;
    esac
  done
  echo "$d"
}

# The harness writes these into the workdir itself, and the verifier's `cargo` adds more. A run
# where the agent did nothing still ends with all of them present, so `workdir_untouched` has to see
# past them — build one that looks like a real post-run workdir, not a clean fixture.
post_run_debris() { # $1=workdir
  printf 'agent=claude\ntask=fix\n'   >"$1/agentic.meta"
  printf '{"type":"assistant"}\n'     >"$1/agent.log"
  printf '1024 2.0\n'                 >"$1/samples.txt"
  printf '    FAIL  cargo test red\n' >"$1/verify.out"
  printf 'error[E0308]\n'             >"$1/cargo.err"
  printf '[[package]]\nname = "x"\n'  >"$1/Cargo.lock"
  mkdir -p "$1/target/debug"; printf 'binary\n' >"$1/target/debug/x"
}

echo "classify_rc:"

# ── the distinction this test exists for ─────────────────────────────────────────────────────────
# Complete delivery that simply fails the verifier is a CAPABILITY miss. Manifest without source is
# not: cargo has nothing to build, so there is no program to be wrong about. Before rc=12 both of
# these answered 10, and the boards read 10 as "the model wrote wrong code".
check "complete project, verify red   → 10" 10 build "$(mk complete manifest src)" 0 0 0
check "manifest only, no src          → 12" 12 build "$(mk manifest_only manifest)"  0 0 0
check "manifest + empty src/          → 12" 12 build "$(mk empty_src manifest emptysrc)" 0 0 0
check "manifest + root main.rs        → 12" 12 build "$(mk root_rs manifest rootrs)" 0 0 0
check "nothing written                → 11" 11 build "$(mk nothing)" 0 0 0
check "src but no manifest            → 11" 11 build "$(mk src_only src)" 0 0 0
check "lib.rs counts as source        → 10" 10 debug "$(mk libcrate manifest lib)" 0 0 0

# ── the seeded tasks: presence cannot answer "did the agent do anything" ─────────────────────────
# `fix`, `debug` and `multibug` start with a manifest AND a source file because the harness wrote
# them, so rc=11 and rc=12 are unreachable there and an agent that changes nothing scored rc=10 —
# "delivered a complete program and it is wrong". rc=13 is the before/after that presence cannot give.
d="$(mk seed_untouched manifest lib seed)"; post_run_debris "$d"
check "seeded, agent changed nothing  → 13" 13 fix "$d" 0 0 0

d="$(mk seed_edited manifest lib seed)"; post_run_debris "$d"
printf 'pub fn f() { /* fixed */ }\n' >"$d/src/lib.rs"
check "seeded, source edited          → 10" 10 fix "$d" 0 0 0

d="$(mk seed_added manifest lib seed)"; post_run_debris "$d"
printf 'fn main() {}\n' >"$d/src/main.rs"
check "seeded, new file beside it     → 10" 10 fix "$d" 0 0 0

d="$(mk seed_deleted manifest lib seed)"; post_run_debris "$d"; rm -f "$d/src/lib.rs"
check "seeded, source deleted         → 12" 12 fix "$d" 0 0 0

d="$(mk seed_pass manifest lib seed)"; post_run_debris "$d"
check "untouched but verify PASSED    → 0"   0 fix "$d" 0 0 1

d="$(mk seed_nomanifest manifest lib seed)"; post_run_debris "$d"
check "no seed manifest at all        → 10" 10 fix "$(mk nomanifest manifest lib)" 0 0 0

# ── the REAL seeding path, not the fixture ───────────────────────────────────────────────────────
# Everything above builds the workdir with `mk`, which imitates `setup_task`. An imitation that
# drifts from the thing it imitates tests itself, so drive `setup_task` directly for every task the
# harness seeds: whatever it writes is what the manifest must cover, today and after someone edits
# the seed.
for t in fix debug multibug; do
  d="$tmp/real_$t"; mkdir -p "$d"; setup_task "$t" "$d"; post_run_debris "$d"
  check "setup_task $t, then nothing   → 13" 13 "$t" "$d" 0 0 0
done
d="$tmp/real_fix_edited"; mkdir -p "$d"; setup_task fix "$d"; post_run_debris "$d"
printf 'fn main() { println!("olleh"); }\n' >"$d/src/main.rs"
check "setup_task fix, then an edit  → 10" 10 fix "$d" 0 0 0

# `wordcount` seeds a data file and no project, so "nothing happened" is still rc=11 — the
# established code for it. rc=13 is not allowed to quietly take cases that already had an answer.
d="$tmp/real_wordcount"; mkdir -p "$d"; setup_task wordcount "$d"; post_run_debris "$d"
check "setup_task wordcount, nothing → 11" 11 wordcount "$d" 0 0 0

# ── greet writes no files BY DESIGN ──────────────────────────────────────────────────────────────
# The gotcha recorded with the backlog item: widening the file check without excluding greet would
# turn every failed greet into a delivery failure it is not.
check "greet, no files, verify red    → 10" 10 greet "$(mk greet_empty)" 0 0 0
check "greet, no files, verify green  → 0"   0 greet "$(mk greet_pass)"  0 0 1

# ── precedence: the earlier answers must not be disturbed ────────────────────────────────────────
# A timeout outranks everything (the workdir is whatever the agent got to), infra outranks a pass,
# and a pass outranks the file checks — a green cell is green even if it built in a layout we would
# otherwise call partial.
check "timeout beats partial          → 124" 124 build "$(mk t_partial manifest)" 1 0 0
check "timeout beats a clean pass     → 124" 124 build "$(mk t_pass manifest src)" 1 0 1
check "infra (raw 2) beats pass       → 2"     2 build "$(mk infra manifest src)"  0 2 1
check "pass beats partial             → 0"     0 build "$(mk pass_partial manifest)" 0 0 1
check "agent error passes through     → 7"     7 build "$(mk agenterr manifest)"   0 7 0
check "agent error beats partial      → 143" 143 build "$(mk sigterm)"             0 143 0

echo
# ── every reader knows every code ────────────────────────────────────────────────────────────────
# A code the harness emits and a reader does not recognise is worse than no code: the cell renders
# as the reader's default, which for the console is ✗ ("the model wrote wrong code") — the exact
# misreading rc=12 was added to stop. Four files have to agree, and BUG-026 is this repo's standing
# lesson that a contract kept in three of four is not a contract. So assert it instead of hoping.
repo="$(cd "$here/../.." && pwd)"
knows() { # $1=file $2=grep -E pattern $3=description
  n=$((n + 1))
  if grep -qE "$2" "$repo/$1"; then
    printf '  ok   %-42s %s\n' "$3" "${1##*/}"
  else
    printf '  FAIL %-42s %s does not know it\n' "$3" "$1"
    fails=$((fails + 1))
  fi
}

# `knows` is satisfied by ONE match, which is not enough for the console: it carries three language
# dictionaries, and a code added to the English one only renders as a bare number for everybody
# else. Measured — renaming the English key alone left the one-match check green, which is exactly
# the half-done change this is here to catch.
knows_n() { # $1=file $2=grep -E pattern $3=expected count $4=description
  local got
  n=$((n + 1))
  got="$(grep -cE "$2" "$repo/$1")"
  if [ "$got" = "$3" ]; then
    printf '  ok   %-42s %s (%s)\n' "$4" "${1##*/}" "$got"
  else
    printf '  FAIL %-42s %s has %s, wanted %s\n' "$4" "$1" "$got" "$3"
    fails=$((fails + 1))
  fi
}

echo "readers of the exit codes:"
for code in 10 11 12 13; do
  knows_n clients/control/site/matrix.html "rc_$code:'" 3                "console label rc=$code, all 3 langs"
  knows clients/control/site/matrix.html   "$code: t\('rc_$code'\)"      "console detail map rc=$code"
  knows scripts/bench/summarize_matrix.py  "get\(\"rc\"\) == \"$code\""  "summarize_matrix.py rc=$code"
  knows scripts/bench/summarize_matrix.ssc "c\.rc == \"$code\""          "summarize_matrix.ssc rc=$code"
  knows docs/specs/agentic-delivery-hardening.md "\\| \`$code\` \\|"     "the spec documents rc=$code"
done

echo
if [ "$fails" -eq 0 ]; then
  echo "classify_rc: $n/$n ok"
else
  echo "classify_rc: $((n - fails))/$n ok, $fails FAILED" >&2
fi
exit "$([ "$fails" -eq 0 ] && echo 0 || echo 1)"
