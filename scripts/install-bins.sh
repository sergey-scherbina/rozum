#!/usr/bin/env bash
# Build and install this repo's binaries, the only way that cannot install a stale one.
#
# WHY THIS EXISTS. On 2026-08-05 a fix landed, was measured working in a worktree, and then went
# onto the machine as `cp target/release/nadia ~/.cargo/bin/… || cargo build …`. The `cp` succeeded
# — on a binary built two days earlier — so the `||` never ran, and every matrix run that day used
# a binary without the fix. The measurement that followed was read as confirming the fix; it could
# not have.
#
# The shape of that mistake is not "I typed the wrong command". It is that installing was a thing
# people did by hand, differently each time. So: one path, and it always
#
#   1. BUILDS first (never installs whatever happens to be in target/),
#   2. EXECS the fresh binary before publishing it (a binary that cannot start must not replace a
#      running service's — BUGS.md BUG-013),
#   3. publishes by RENAME, so there is no window where the path is missing or half-written,
#   4. prints what it replaced and with what, because "installed" without a version is how the
#      stale one hid.
#
# It does NOT restart services. Bouncing a job is `deploy-ucc-web.sh`'s business, which knows the
# right order; a script that restarts things while the operator is mid-task is a lesson this
# project has already paid for once.
#
#   scripts/install-bins.sh                # all three, to their usual homes
#   scripts/install-bins.sh nadia          # just one
#   DEST=~/.rozum/bin scripts/install-bins.sh rozum-gateway
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

CARGO_BIN="${DEST:-$HOME/.cargo/bin}"

# name → (cargo package, cargo bin, profile, destination dir)
targets() {
  case "$1" in
    rozum-gateway) echo "rozum rozum-gateway release $CARGO_BIN" ;;
    nadia)         echo "nadia nadia release $CARGO_BIN" ;;
    rozum)         echo "rozum-cli rozum debug $CARGO_BIN" ;;
    *) echo "unknown binary: $1 (known: rozum-gateway nadia rozum)" >&2; return 1 ;;
  esac
}

install_one() {
  local name="$1"
  read -r pkg bin profile dir <<<"$(targets "$name")"
  local flag=""; [ "$profile" = release ] && flag="--release"

  echo "==> building $name ($pkg, $profile)"
  cargo build $flag -p "$pkg" --bin "$bin" >/dev/null

  local src="target/$profile/$bin" dst="$dir/$name"
  [ -x "$src" ] || { echo "FAIL: $src missing after a successful build" >&2; exit 1; }

  # What is being replaced, and by what. Both times a stale binary hid on this machine, the install
  # said only "installed".
  local before="none"
  [ -f "$dst" ] && before="$(date -r "$dst" '+%Y-%m-%d %H:%M')"
  local after; after="$(date -r "$src" '+%Y-%m-%d %H:%M')"

  mkdir -p "$dir"
  cp "$src" "$dst.new.$$"
  chmod +x "$dst.new.$$"
  local rc=0
  "$dst.new.$$" --help >/dev/null 2>&1 || rc=$?
  if [ "$rc" -ne 0 ]; then
    rm -f "$dst.new.$$"
    echo "FAIL: freshly built $name will not exec (rc=$rc) — NOT installing; $dst untouched" >&2
    exit 1
  fi
  mv -f "$dst.new.$$" "$dst"
  echo "    $dst  ($before  ->  $after)"
}

for name in "${@:-rozum-gateway nadia rozum}"; do
  install_one "$name"
done
