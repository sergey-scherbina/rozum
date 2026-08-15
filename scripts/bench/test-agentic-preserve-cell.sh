#!/usr/bin/env bash
# Execute `preserve_cell` from `agentic.sh` — the rule that a FAILED cell leaves its evidence
# behind.
#
# What it guards has already cost two conclusions. HISTORY.md 2026-08-04 withdrew "the gate
# repaired it" because the transcript was gone; on 2026-08-15 an `rpn` cell failed printing 20 for
# `3 4 + 5 *`, and the program that printed it had been deleted before anyone could ask whether the
# model wrote bad arithmetic or the harness delivered a broken file. Both times the artifact was
# removed by a line that runs a second after the row is written.
#
# Real directories on disk, no model and no gateway, well under a second.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=/dev/null
source "$here/agentic.sh"
if ! declare -F preserve_cell >/dev/null; then
  echo "FAIL: sourcing agentic.sh did not define preserve_cell — the source guard moved" >&2
  exit 1
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
fails=0
n=0

ok() { n=$((n + 1)); printf '  ok   %s\n' "$1"; }
bad() { n=$((n + 1)); fails=$((fails + 1)); printf '  FAIL %s\n' "$1" >&2; }

# A workdir shaped like the ones the harness makes: the model's program, the agent transcript, the
# sample dump — and a build tree that must NOT be copied.
mk_work() {
  local w="$1"
  mkdir -p "$w/src" "$w/target/debug/deps"
  printf 'fn main() { println!("20"); }\n' > "$w/src/main.rs"
  printf '[package]\nname = "rpn-calc"\n' > "$w/Cargo.toml"
  printf '{"type":"assistant"}\n' > "$w/agent.log"
  printf 'sample\n' > "$w/samples.txt"
  # Stand-in for the build tree's bulk.
  dd if=/dev/zero of="$w/target/debug/deps/big.rlib" bs=1024 count=200 2>/dev/null
}

echo "preserve_cell:"

work="$tmp/work"; mk_work "$work"
dest_root="$tmp/out/runs"
got="$(preserve_cell "$work" "$dest_root" "claude-qwen-rpn")"

[ "$got" = "$dest_root/claude-qwen-rpn" ] && ok "echoes the directory it wrote" \
  || bad "echoed '$got', wanted '$dest_root/claude-qwen-rpn'"

# The three things a red is diagnosed from.
[ -f "$got/src/main.rs" ] && ok "keeps the program the model wrote" || bad "src/main.rs missing"
[ -f "$got/agent.log" ]   && ok "keeps the agent transcript"        || bad "agent.log missing"
[ -f "$got/samples.txt" ] && ok "keeps the sample dump"             || bad "samples.txt missing"
grep -q 'println!("20")' "$got/src/main.rs" 2>/dev/null \
  && ok "the program is readable, not just present" || bad "src/main.rs did not survive intact"

# The one thing it must not copy — the reason "keep everything" was never affordable by default.
[ -e "$got/target" ] && bad "copied target/ — that is the bulk this exists to leave behind" \
  || ok "leaves target/ behind"

# REPS>1 sends the same label twice; the SPLIT across reps is the finding, so neither may clobber.
work2="$tmp/work2"; mk_work "$work2"
printf 'fn main() { println!("35"); }\n' > "$work2/src/main.rs"
got2="$(preserve_cell "$work2" "$dest_root" "claude-qwen-rpn")"
[ "$got2" != "$got" ] && ok "a second cell with the same label gets its own directory" \
  || bad "second cell reused '$got'"
grep -q 'println!("20")' "$got/src/main.rs" 2>/dev/null \
  && ok "the first cell is untouched by the second" || bad "the first cell was overwritten"

# A workdir the agent never created (rc=2, gateway gone) is not an error to preserve.
[ -z "$(preserve_cell "$tmp/never-existed" "$dest_root" "x")" ] \
  && ok "a missing workdir is silent, not a failure" || bad "a missing workdir produced output"

echo
if [ "$fails" -eq 0 ]; then
  echo "PASS — $n cases"
else
  echo "FAIL — $fails of $n cases" >&2
fi
exit $((fails > 0))
