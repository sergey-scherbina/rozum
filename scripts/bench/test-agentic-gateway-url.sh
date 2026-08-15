#!/usr/bin/env bash
# Execute `agent_gateway_mismatch` from `agentic.sh` — the rule that stops a run from reporting a
# cell against a gateway that never served it.
#
# The defect this guards was invisible for as long as the knob existed: `BENCH_GATEWAY_URL` steers
# the harness and nothing else, because every agent gets its base URL from `rozum launch`, which
# resolves the ACTIVE gateway itself. It took a recording proxy to see it — the run announced
# :8199 and the proxy logged zero bodies. A unit test is the cheap half of that proof, and unlike
# the proxy it runs in a second with no model, no gateway and no network.
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=/dev/null
source "$here/agentic.sh"
if ! declare -F agent_gateway_mismatch >/dev/null; then
  echo "FAIL: sourcing agentic.sh did not define agent_gateway_mismatch — the source guard moved" >&2
  exit 1
fi

fails=0
n=0

# $1=name $2=announced URL $3=active port $4=expect: "ok" (agree) or a substring of the reason
check() {
  local name="$1" got
  n=$((n + 1))
  got="$(agent_gateway_mismatch "$2" "$3")"
  if [ "$4" = ok ]; then
    if [ -z "$got" ]; then
      printf '  ok   %-46s (agrees)\n' "$name"
    else
      printf '  FAIL %-46s expected agreement, got: %s\n' "$name" "$got"
      fails=$((fails + 1))
    fi
  elif [ -n "$got" ] && [[ "$got" == *"$4"* ]]; then
    printf '  ok   %-46s %s\n' "$name" "$got"
  else
    printf '  FAIL %-46s expected a reason containing %q, got: %q\n' "$name" "$4" "$got"
    fails=$((fails + 1))
  fi
}

echo "agent_gateway_mismatch:"

# The normal borrowed run: the operator points at the gateway that is actually resident.
check "same port agrees"            "http://127.0.0.1:8089" 8089 ok
check "same port, trailing slash"   "http://127.0.0.1:8089/" 8089 ok
check "host spelled differently"    "http://localhost:8089"  8089 ok

# The measured defect, 2026-08-15: a proxy on :8199 in front of :8089. Everything answered, the
# header line said :8199, and the agent's traffic went to :8089.
check "proxy in front of the daemon" "http://127.0.0.1:8199" 8089 "will use :8089"

# No registered gateway: `rozum launch` spawns its own — a second copy of the weights on a host
# sized for one, which is the eviction the share-by-default path exists to prevent.
check "nothing active"              "http://127.0.0.1:8199" ""   "would start its own"

# A URL with no port cannot be compared at all; silence there would be the same lie, quieter.
check "no explicit port"            "http://gateway.local"  8089 "no explicit port"

echo
if [ "$fails" -eq 0 ]; then
  echo "PASS — $n cases"
else
  echo "FAIL — $fails of $n cases" >&2
fi
exit $((fails > 0))
