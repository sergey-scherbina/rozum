#!/usr/bin/env bash
#
# meeting-watch.sh — one line per new meeting mention, for an agent harness to turn into an event.
#
# WHY THIS EXISTS, given `rozum-native-channels.md` already has a three-tier wakeup ladder.
#
# That spec's constraint is exact and still true: we do not control the agent's client, and the
# only way to reach a TRULY IDLE session is a message the client itself chooses to inject. Tier 1
# (Anthropic's `claude/channel`) is Claude-Code-only and behind a flag; tier 2 (`wait_my_turn`)
# reaches only an agent that keeps a poll open; tier 3 (piggyback) reaches nobody until the agent
# speaks first. In the common case — an agent sitting idle mid-task — none of them fire.
#
# The missing piece was not in the protocol. It was that we kept looking from the SERVER side.
# A harness that can run a background command and surface its output as events (Claude Code's
# `Monitor`, and anything with the same shape) already owns exactly the injection point the spec
# says only the client can provide. This script is the other half of that: something whose stdout
# IS the event stream.
#
# `meetings inbox` is the source rather than a transcript tail because it is durable and
# cursor-based on disk: a mention is delivered exactly once, and survives a restart of both the
# watcher and the daemon. A tail can only promise one of those two.
#
# Usage (from an agent harness that streams a command's stdout as events):
#     HANDLE=<your-handle> scripts/meeting-watch.sh
# Environment: HANDLE (required in practice), ROOMS, GAP, ROZUM.
set -u
ROZUM="${ROZUM:-$HOME/.cargo/bin/rozum}"
HANDLE="${HANDLE:?set HANDLE to your meeting handle (see: rozum meetings whoami)}"
ROOMS="${ROOMS:-rozum commons scalascript busi}"
GAP="${GAP:-5}"
down=0
while true; do
  for r in $ROOMS; do
    out="$("$ROZUM" meetings inbox --as "$HANDLE" --room "$r" 2>&1)"
    rc=$?
    if [ $rc -ne 0 ]; then
      # Silence must not read as "all quiet": say it once, and once more when it recovers.
      [ $down -eq 0 ] && { printf 'WATCH-ERROR %s: %s\n' "$r" "$(echo "$out" | head -1)"; down=1; }
      continue
    fi
    [ $down -eq 1 ] && { printf 'WATCH-OK: meeting daemon reachable again\n'; down=0; }
    case "$out" in
      *"no new messages"*|"") : ;;
      *) printf '[%s] %s\n' "$r" "$(echo "$out" | tr '\n' ' ' | cut -c1-600)" ;;
    esac
  done
  sleep "$GAP"
done
