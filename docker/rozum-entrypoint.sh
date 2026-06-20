#!/bin/sh
# rozum-agent entrypoint. Normally a transparent passthrough (`exec "$@"`).
#
# When ROZUM_EGRESS=strict (set by `rozum launch` for NetPolicy::GatewayStrict /
# ROZUM_SANDBOX_NETWORK=gateway-strict, which also grants --cap-add=NET_ADMIN), it
# installs an iptables egress allowlist FIRST: the container may reach the host
# gateway (host.docker.internal) + loopback + already-established connections, and
# nothing else. This closes the Docker bridge-egress gap — the agent can talk to the
# local model gateway but cannot reach the internet (no exfiltration / fetching
# untrusted code). See docs/specs/model-sandbox.md.
set -e

if [ "$ROZUM_EGRESS" = "strict" ]; then
  # The IPv4 host-gateway address Docker injected for host.docker.internal.
  gw="$(grep -i 'host\.docker\.internal' /etc/hosts 2>/dev/null | grep -v ':' | awk '{print $1}' | head -1)"
  if command -v iptables >/dev/null 2>&1 && [ -n "$gw" ]; then
    iptables  -P OUTPUT DROP
    iptables  -A OUTPUT -o lo -j ACCEPT
    iptables  -A OUTPUT -m state --state ESTABLISHED,RELATED -j ACCEPT
    iptables  -A OUTPUT -d "$gw" -j ACCEPT
    # Deny ALL IPv6 egress (the gateway is reached over IPv4; an open v6 path would
    # be a bypass). Best-effort — not all kernels load ip6tables.
    ip6tables -P OUTPUT DROP 2>/dev/null || true
    echo "rozum: strict egress active — only the host gateway ($gw) is reachable" >&2
  else
    # Fail loud, not silently-open: a strict request that can't be enforced must not
    # masquerade as enforced. Refuse to run rather than give a false guarantee.
    echo "rozum: ERROR strict egress requested but iptables/host-gateway unavailable" >&2
    echo "rozum:   (need --cap-add=NET_ADMIN + iptables in the image + host.docker.internal)" >&2
    exit 70
  fi
fi

exec "$@"
