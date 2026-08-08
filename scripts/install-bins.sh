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

# Publish one built binary at one path: exec it first, rename it into place, say what changed.
publish() {
  local src="$1" dst="$2" what="$3"
  [ -x "$src" ] || { echo "FAIL: $src missing after a successful build" >&2; exit 1; }

  # What is being replaced, and by what. Both times a stale binary hid on this machine, the install
  # said only "installed".
  local before="none"
  [ -f "$dst" ] && before="$(date -r "$dst" '+%Y-%m-%d %H:%M')"
  local after; after="$(date -r "$src" '+%Y-%m-%d %H:%M')"

  mkdir -p "$(dirname "$dst")"
  cp "$src" "$dst.new.$$"
  chmod +x "$dst.new.$$"
  local rc=0
  "$dst.new.$$" --help >/dev/null 2>&1 || rc=$?
  if [ "$rc" -ne 0 ]; then
    rm -f "$dst.new.$$"
    echo "FAIL: freshly built $what will not exec (rc=$rc) — NOT installing; $dst untouched" >&2
    exit 1
  fi
  mv -f "$dst.new.$$" "$dst"
  echo "    $dst  ($before  ->  $after)"
}

# Publish an already-built binary at one more path (a second copy some job execs).
install_to() {
  local name="$1" dst="$2"
  read -r _pkg bin profile _dir <<<"$(targets "$name")"
  publish "target/$profile/$bin" "$dst" "$name"
}

install_one() {
  local name="$1"
  read -r pkg bin profile dir <<<"$(targets "$name")"
  local flag=""; [ "$profile" = release ] && flag="--release"

  echo "==> building $name ($pkg, $profile)"
  cargo build $flag -p "$pkg" --bin "$bin" >/dev/null

  publish "target/$profile/$bin" "$dir/$name" "$name"
}

# Every path launchd actually execs, read from the plists rather than assumed.
#
# This machine had THREE copies of one program, of three different ages:
#   ~/.cargo/bin/rozum-gateway   Aug 8   (bridges, meeting daemon, doctor)
#   ~/.rozum/bin/rozum-gateway   Aug 5   (com.rozum.gateway — the RESIDENT MODEL server)
#   ~/.rozum/bin/rozum-ctrl      Aug 1   (com.rozum.ucc-control; same binary, another name)
# Hand installs had been going to the first one only, so the most important service on the
# machine ran three-day-old code and nothing said so. `~/.cargo/bin` vs `~/.rozum/bin` is a
# divergence this project has been bitten by before; deriving the list from the roster is the
# only version that cannot drift, because the roster is what the machine obeys.
extra_paths_for() {
  local name="$1" out=()
  local plist
  for plist in "$HOME/Library/LaunchAgents"/com.rozum.*.plist; do
    [ -f "$plist" ] || continue
    # `plutil -extract` reads the VALUE; parsing `plutil -p` text picked up the first `/Users/`
    # it saw, which was an EnvironmentVariables key, and quietly matched nothing useful.
    local prog
    prog="$(plutil -extract ProgramArguments.0 raw -o - "$plist" 2>/dev/null)"
    case "$prog" in
      *.sh|"") continue ;;
    esac
    # `rozum-ctrl` is the gateway binary under another name — same program, own copy.
    case "$name:$(basename "$prog")" in
      rozum-gateway:rozum-gateway|rozum-gateway:rozum-ctrl|nadia:nadia|rozum:rozum) ;;
      *) continue ;;
    esac
    [ "$prog" = "$CARGO_BIN/$name" ] && continue
    out+=("$prog")
  done
  printf '%s\n' "${out[@]}" | sort -u | grep -v '^$' || true
}

for name in "${@:-rozum-gateway nadia rozum}"; do
  install_one "$name"
  while read -r p; do
    [ -n "$p" ] || continue
    echo "==> also $p (a launchd job execs this copy)"
    install_to "$name" "$p"
  done <<<"$(extra_paths_for "$name")"
done
