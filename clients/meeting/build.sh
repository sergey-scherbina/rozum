#!/usr/bin/env bash
# Build the pure-.ssc rozum meeting server to a stable binary.
# Requires the scalascript toolchain (bin/ssc) staged via `./install.sh --dev`.
set -euo pipefail
SSC_ROOT="${SSC_ROOT:-/Users/sergiy/work/my/scalascript}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
OUT="${1:-$HOME/.local/bin/rozum-meeting-ssc}"
export PATH="$PATH:$SSC_ROOT/bin"
# `build-rust` moved to the OPTIONAL tools/compatibility tier — the standard `bin/ssc` now refuses
# it ("requires the optional ScalaScript tools/compatibility tier; run ssc-tools explicitly"),
# exactly as `emit-spa` did to the UCC deploy on 2026-07-13. Prefer `ssc-tools` when it exists.
SSC_BIN="ssc"
[ -x "$SSC_ROOT/bin/ssc-tools" ] && SSC_BIN="$SSC_ROOT/bin/ssc-tools"

# ── what a failure MEANS, said at the moment it happens ────────────────────────────────────────
#
# BUG-030 cost days to two different readings of the same 1,500-line cargo dump. The compiler error
# names OUR line (`hashStr` in meeting.ssc), so it reads as a defect in this repo — and twice it was
# neither: the fix was upstream both times, and the second time it was upstream ALREADY, sitting in
# their `main` while the toolchain we compile against was staged from an older tree. There is no way
# to tell those two apart from the error text, and the difference is "file a report" vs "restage".
#
# So on failure, say which one it is. `bin/lib/.build-digest` records the tree the staged toolchain
# was built from and `scripts/launcher-input-digest` computes the tree in front of it; the pair is
# the only honest freshness test — NOT "the stamp equals HEAD", which has been wrong in both
# directions since their content-addressed cache landed (a board-only commit reads as stale, and
# their own script says so at length).
#
# It runs ONLY after a failure. On the happy path it would be pure cost — the digest walks the whole
# tree — and a check that slows down every green build is the check people delete.
diagnose_failure() {
  local staged computed cache_entry behind
  echo "" >&2
  echo "build.sh: the ssc build FAILED. Before reading the cargo output as a defect in this repo:" >&2

  # TWO ways the compiler can lack a fix that landed upstream, and they need different answers.
  # The toolchain can be staged from an older tree than the checkout (restage), or the CHECKOUT
  # itself can be behind their main with a toolchain that matches it perfectly (pull, then restage).
  # Only the first is "stale" in the digest's sense, and reporting the second as "current, so not
  # staleness" would send the reader off to write an upstream report for a bug already fixed —
  # which is the exact detour BUG-030 took twice. Counted against the refs on disk, so this reads
  # the last fetch and does not reach the network from a build.
  behind="$(git -C "$SSC_ROOT" rev-list --count HEAD..origin/main 2>/dev/null || true)"

  if [ ! -x "$SSC_ROOT/scripts/launcher-input-digest" ]; then
    echo "  cannot judge toolchain freshness: no $SSC_ROOT/scripts/launcher-input-digest" >&2
    return 0
  fi
  staged="$(cat "$SSC_ROOT/bin/lib/.build-digest" 2>/dev/null || true)"
  computed="$(cd "$SSC_ROOT" && ./scripts/launcher-input-digest 2>/dev/null | tail -1)"
  if [ -z "$computed" ]; then
    echo "  cannot judge toolchain freshness: the digest script produced nothing" >&2
    return 0
  fi

  if [ "$staged" = "$computed" ]; then
    echo "  the toolchain at $SSC_ROOT is CURRENT with its own tree ($computed)." >&2
    echo "  So this is NOT staleness: either our source, or a live defect in their compiler." >&2
    echo "  If it is theirs, minimise it to a few lines first and file it — see BUG-030 for what" >&2
    echo "  that report has to carry (both lanes, and the version)." >&2
    if [ -n "$behind" ] && [ "$behind" -gt 0 ] 2>/dev/null; then
      # Deliberately NOT phrased as "pull and restage". That repo lands ~313 commits a day and most
      # of them are boards and claims, which their own digest excludes precisely because they cannot
      # reach the compiled bytes — so "behind" is the normal state and treating it as a cause would
      # send every reader chasing an upstream fix that mostly is not there. It is a fact worth
      # having and nothing more.
      echo "  (That tree is $behind commits behind origin/main as of the last fetch there. Most of" >&2
      echo "  their commits are boards and claims and cannot change the compiler, so this alone is" >&2
      echo "  no reason to distrust the error — but if the thing you hit WAS fixed since, pull and" >&2
      echo "  restage, and read the sharing note below first.)" >&2
    fi
  else
    echo "  the toolchain at $SSC_ROOT is STALE." >&2
    echo "    staged from: ${staged:-<none>}" >&2
    echo "    tree is now: $computed" >&2
    echo "  A fix that landed upstream is NOT in the compiler you just ran. Restage before believing" >&2
    echo "  the error." >&2
    # Only meaningful here. In the behind-main case `computed` is the OLD tree's digest, so a cache
    # hit on it would say the toolchain you already have can be restored — which is not the point.
    cache_entry="${SSC_TOOLCHAIN_CACHE:-$HOME/.cache/ssc-toolchain}/$computed"
    if [ -d "$cache_entry/lib" ]; then
      echo "  Their content-addressed cache already HAS this tree ($cache_entry), so restaging is a" >&2
      echo "  copy of seconds, not a ~3.5 min sbt build." >&2
    fi
  fi
  echo "  Do NOT run install.sh in their SHARED checkout — siblings measure against that staged" >&2
  echo "  toolchain and swapping it under them is how a green verdict gets attributed to the wrong" >&2
  echo "  tree. Stage your own and point this script at it:" >&2
  echo "    git -C $SSC_ROOT worktree add --detach /tmp/tc origin/main" >&2
  echo "    (cd /tmp/tc && ./install.sh --dev)" >&2
  echo "    SSC_ROOT=/tmp/tc $HERE/build.sh $OUT" >&2
}

# `set +e` around the build and an explicit `$?`: with `if ! cmd`, `$?` inside the branch is the
# NEGATED status and reads 0 on a failure. That exact shape has already reported a failed build as
# a success in this repo.
set +e
"$SSC_BIN" build-rust "$HERE/meeting.ssc" -o "$OUT"
rc=$?
set -e
if [ "$rc" -ne 0 ]; then
  diagnose_failure
  exit "$rc"
fi
echo "built: $OUT"
