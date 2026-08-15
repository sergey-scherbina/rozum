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
# It DOES restart the job that execs a binary it replaced, and waits for it to come back — see
# `restart_owner` for why: publishing over a running service's binary IS a restart of that service,
# so the only question is whether it happens visibly now or as an outage later. macOS kills a
# process whose executable is replaced, and launchd's respawn throttle then keeps the port dark for
# about a minute while an installer that "does not restart services" reports success.
#
# This header said the opposite until 2026-08-15, having been written before that behaviour landed.
# A comment that contradicts its own file is worse than no comment: the operator who read it
# deployed expecting nothing to bounce.
#
#   scripts/install-bins.sh                # all three, to their usual homes
#   scripts/install-bins.sh nadia          # just one
#   DEST=~/.rozum/bin scripts/install-bins.sh rozum-gateway
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# NEVER install a binary built in a worktree.
#
# `mlx-sys` bakes the ABSOLUTE path of its build directory into the binary — that is where
# `mlx.metallib` lives — so a gateway built in `.worktrees/feature/x` keeps working exactly until
# that worktree is removed, and then dies at RUNTIME with "Failed to load the default metallib".
# Not at install: `--help` never touches Metal, so the exec-check passes happily.
#
# Measured 2026-08-08, and it took the operator's assistant down: installed from a worktree,
# removed the worktree an hour later, then restarted the gateway — which could no longer load the
# model at all. The main checkout's target/ is stable, so build there.
case "$ROOT" in
  */.worktrees/*)
    echo "REFUSING: $ROOT is a worktree." >&2
    echo "  mlx-sys bakes this directory's absolute path into the binary (mlx.metallib lives" >&2
    echo "  there), so removing the worktree later breaks the installed binary at runtime —" >&2
    echo "  and the exec-check cannot see it, because --help never loads Metal." >&2
    echo "  Build and install from the main checkout instead." >&2
    exit 1 ;;
esac

CARGO_BIN="${DEST:-$HOME/.cargo/bin}"

# name → (cargo package, cargo bin, profile, destination dir)
targets() {
  case "$1" in
    rozum-gateway) echo "rozum rozum-gateway release $CARGO_BIN" ;;
    nadia)         echo "nadia nadia release $CARGO_BIN" ;;
    rozum)         echo "rozum-cli rozum debug $CARGO_BIN" ;;
    # The MCP hot path (`com.rozum.mcp-http`, :8779). It was THREE WEEKS old on 2026-08-08 because
    # no install path knew it existed — the whole reason this list is now checked against the
    # launchd roster rather than remembered.
    rozum-meet)    echo "rozum-meet rozum-meet release $CARGO_BIN" ;;
    # Emitted by the ScalaScript toolchain, not by cargo: `clients/control/deploy-ucc-web.sh` owns
    # it. Named here so that "this script does not update it" is a statement rather than a silence.
    # Built by the ScalaScript toolchain, not cargo — handled by `install_ssc`, not by `targets`.
    rozum-meeting-ssc|rozum-ucc-ssc) echo "SSC" ;;
    *) echo "unknown binary: $1 (known: rozum-gateway nadia rozum rozum-meet)" >&2; return 1 ;;
  esac
}

# Publish one built binary at one path: exec it first, rename it into place, say what changed.
# A cargo binary from this workspace must carry its build stamp, or `doctor --services` cannot tell
# what is DEPLOYED from what is MERGED and reports it as "age unknown" forever.
#
# Checked HERE, on the real artifact, because the unit test could not: the first stamp survived a
# debug build and was dead-stripped from release, so the suite was green while every deployed binary
# went out unstamped. A property that only holds in the profile nobody ships is not a property.
require_stamp() {
  local file="$1" what="$2"
  grep -q 'ROZUM+BUILD+MARK=[0-9a-f]' "$file" 2>/dev/null && return 0
  echo "FAIL: $what carries no build stamp — refusing to publish it." >&2
  echo "      (crates/rozum-stamp must be linked AND referenced; see docs/specs/deployment-drift.md)" >&2
  exit 1
}

publish() {
  local src="$1" dst="$2" what="$3"
  [ -x "$src" ] || { echo "FAIL: $src missing after a successful build" >&2; exit 1; }
  require_stamp "$src" "$what"

  # What is being replaced, and by what. Both times a stale binary hid on this machine, the install
  # said only "installed".
  local before="none"
  [ -f "$dst" ] && before="$(date -r "$dst" '+%Y-%m-%d %H:%M')"
  local after; after="$(date -r "$src" '+%Y-%m-%d %H:%M')"

  mkdir -p "$(dirname "$dst")"
  cp "$src" "$dst.new.$$"
  chmod +x "$dst.new.$$"
  # Bounded, because "does not exec" includes "never returns" — an unbounded check turns a bad
  # binary into an install that hangs forever instead of one that fails.
  local rc=0
  timeout 20 "$dst.new.$$" --help >/dev/null 2>&1 || rc=$?
  if [ "$rc" -ne 0 ]; then
    rm -f "$dst.new.$$"
    echo "FAIL: freshly built $what will not exec (rc=$rc$([ "$rc" = 124 ] && echo ', it hung')) — NOT installing; $dst untouched" >&2
    exit 1
  fi
  # Keep what is being replaced until the REPLACEMENT is proven, not just the candidate.
  local backup=""
  if [ -f "$dst" ]; then backup="$dst.prev.$$"; cp -p "$dst" "$backup"; fi
  mv -f "$dst.new.$$" "$dst"

  # The check above ran while the OLD binary was still in place, and on 2026-08-13 that is exactly
  # why it passed: the staged copy was the `rozum` dispatcher, whose `--help` execs `rozum-gateway`
  # — which was still the engine. The self-exec loop cannot exist until after the `mv`, so the
  # PUBLISHED path has to be exercised too, with a timeout, because the failure is a hang and not
  # an error. `timeout` reports 124, which is the whole point.
  local prc=0
  timeout 20 "$dst" --help >/dev/null 2>&1 || prc=$?
  if [ "$prc" -ne 0 ]; then
    if [ -n "$backup" ]; then mv -f "$backup" "$dst"; fi
    echo "FAIL: $what at $dst does not exec after publishing (rc=$prc)$([ "$prc" = 124 ] && echo ' — it hung, which is what a binary execing itself looks like')" >&2
    echo "      the previous binary is back in place; nothing was restarted" >&2
    exit 1
  fi
  [ -n "$backup" ] && rm -f "$backup"

  echo "    $dst  ($before  ->  $after)"
  restart_owner "$dst"
}

# Publish an already-built binary at one more path (a second copy some job execs).
install_to() {
  local name="$1" dst="$2"
  read -r _pkg bin profile _dir <<<"$(targets "$name")"
  publish "target/$profile/$bin" "$dst" "$name"
}

# The meeting PWA: emitted from `clients/meeting/meeting.ssc` by the ScalaScript toolchain.
#
# It was the last binary on this machine that no install path touched — built 2026-06-29 and never
# again, while every cargo binary was refreshed. That is the same rot that had three copies of the
# engine at three different ages; being outside cargo is not a reason to be outside the update.
#
# Its exec-check cannot be `--help`: this program takes no flags, it goes straight to `serve(8405)`
# and panics with "Address already in use" when the service is up. That panic is still PROOF THE
# BINARY RUNS, which is all the check is for (BUG-013: launchd could not exec; BUG-028: the runtime
# could not find its metallib). So: run it briefly and accept either "bound the port" or "the port
# was taken" — reject only a binary that cannot start at all.
# $1 = installed binary name, $2 = build script relative to the repo root.
#
# PARAMETERISED once there were two. The meeting PWA and the UCC's public routes are the same job —
# one .ssc source, one `build-rust`, one exec-check, one publish, one restart — and the second one
# arriving is exactly when a copy-paste would have been made and then drifted. `rozum-meet` and
# `rozum-meeting-ssc` already taught that lesson here (BUG-029).
install_ssc() {
  local name="$1" script="$2" dst
  dst="$(extra_paths_for "$name" | head -1)"
  dst="${dst:-$HOME/.local/bin/$name}"

  if [ ! -x "$ROOT/$script" ]; then
    echo "SKIP $name: $script missing" >&2; return 0
  fi
  echo "==> building $name (ScalaScript → Rust)"
  local tmp="$dst.new.$$"
  local log="$tmp.log"
  if ! "$ROOT/$script" "$tmp" >"$log" 2>&1; then
    rm -f "$tmp"
    # SAY WHY, and say what it is TODAY — the reason has changed twice and a stale reason here is
    # worse than none, because it sends the next reader looking for a bug that is already fixed.
    #
    # 2026-08-08: failed inside the STD library (`jsonCoreRenderFields extracts Cons which is not a
    # known enum constructor`, `_normSegments uses unsupported infix operator ::`). Those are fixed.
    # 2026-08-13: failed on ONE line of our own source — `hashStr` in `clients/meeting/meeting.ssc`
    # does `s.trim.toList.map(c => c.toInt).sum`, and the Rust lane had no `toInt` for a character
    # (`error[E0277]: the trait bound &char: SscToInt is not satisfied`). Reported to scalascript
    # with a fix; it is in their main since `2315f2ecf` and the build is green again (2026-08-14).
    #
    # WHICH LEAVES NO STANDING REASON HERE, and that is the point of not writing one. Both entries
    # above were true when written and false a week later, and the second cost a reader the assumption
    # that the toolchain was fine. So the failure now EXPLAINS ITSELF: `build.sh` compares the staged
    # toolchain's digest against its own tree and prints which of "restage" / "our source" / "their
    # compiler" it is, at the moment it fails, from the machine's own state instead of from a comment.
    # Print that block whole — its last lines are the suggested commands, so a blind `tail` would
    # show the remedy and hide the diagnosis.
    echo "SKIP $name: the ssc build FAILED — $dst left as it is. Reason:" >&2
    if grep -q 'build.sh: the ssc build FAILED' "$log"; then
      sed -n '/build.sh: the ssc build FAILED/,$p' "$log" | sed 's/^/      /' >&2
    else
      tail -3 "$log" | sed 's/^/      /' >&2
    fi
    rm -f "$log"
    return 0
  fi
  rm -f "$log"
  chmod +x "$tmp"

  local out rc=0
  out="$("$tmp" 2>&1 & sleep 2; kill %1 2>/dev/null; wait 2>/dev/null)" || rc=$?
  case "$out" in
    *"Address already in use"*) : ;;   # reached its serve call — the service holds the port
    *) if ! kill -0 %1 2>/dev/null && [ -z "$out" ] && [ "$rc" -ge 126 ]; then
         rm -f "$tmp"
         echo "FAIL: freshly built $name will not exec (rc=$rc) — $dst untouched" >&2
         exit 1
       fi ;;
  esac

  local before="none"
  [ -f "$dst" ] && before="$(date -r "$dst" '+%Y-%m-%d %H:%M')"
  mkdir -p "$(dirname "$dst")"
  mv -f "$tmp" "$dst"
  echo "    $dst  ($before  ->  $(date -r "$dst" '+%Y-%m-%d %H:%M'))"
  restart_owner "$dst"
}

install_one() {
  local name="$1"
  read -r pkg bin profile dir <<<"$(targets "$name")"
  local flag=""; [ "$profile" = release ] && flag="--release"

  echo "==> building $name ($pkg, $profile)"
  cargo build $flag -p "$pkg" --bin "$bin" >/dev/null

  publish "target/$profile/$bin" "$dir/$name" "$name"
}

# Restart the launchd job that execs this path, and wait for it to come back.
#
# WHY THIS IS NOT OPTIONAL. `mv` is atomic for the FILESYSTEM, not free for the process already
# running from that path: macOS kills a running process whose executable file is replaced, with
# `last exit reason = OS_REASON_CODESIGNING` — which reads like the new binary is unsigned and is
# not. Measured 2026-08-08: publishing `rozum-meeting-ssc` killed the live :8405 phone service, the
# freshly built binary verified and ran fine by hand, and launchd's respawn throttle kept the port
# dark for about a minute while the install script reported success and exited.
#
# So: publishing a binary a job execs IS a restart of that job. Do it deliberately and prove it came
# back, rather than leaving an outage whose length is launchd's throttle and whose cause reads as a
# signature problem.
#
# This is not free for `com.rozum.gateway`: restarting it drops the resident model, which then
# reloads on the next request. That cost is REAL and it is also unavoidable — the replacement kills
# the old process either way. What this buys is that the drop happens now, visibly, instead of at
# whatever moment the operator next opens the phone.
restart_owner() {
  local dst="$1" plist label
  for plist in "$HOME/Library/LaunchAgents"/com.rozum.*.plist; do
    [ -f "$plist" ] || continue
    [ "$(plutil -extract ProgramArguments.0 raw -o - "$plist" 2>/dev/null)" = "$dst" ] || continue
    label="$(basename "$plist" .plist)"
    echo "    restarting $label (its binary was just replaced)"
    # `kickstart -k` restarts the PROCESS from the job definition launchd already loaded. That is
    # exactly right here — the plist did not change, only the file it points at. It is NOT enough
    # after editing a plist: measured 2026-08-15 while enabling the .ssc console routes, where the
    # new `EnvironmentVariables` entry was invisible to the restarted process and the switch stayed
    # off while every answer still looked correct. A plist edit needs `bootout` + `bootstrap`.
    launchctl kickstart -k "gui/$(id -u)/$label" >/dev/null 2>&1 || true
    # A pid is NOT proof. Measured on the very first run of this code: it reported "back (pid 6507)"
    # while the OLD process still held :8405, so the new one panicked on bind, exited, and launchd
    # respawned it — the service that answered a few seconds later was pid 6644. Wait for a pid that
    # SURVIVES, which is the cheapest available stand-in for "it is serving"; `doctor --services`
    # is what actually probes the endpoint, and it is one command away for whoever wants certainty.
    # A PERIODIC job holds no pid between ticks, so waiting for one is asking the wrong question —
    # measured on the first real deploy: `com.rozum.doctor` (StartInterval 300) was reported as
    # "did not settle" while being perfectly healthy, which is a false red on the very job whose
    # purpose is to not cry wolf.
    if plutil -extract StartInterval raw -o - "$plist" >/dev/null 2>&1; then
      echo "    $label is periodic — it runs on its own schedule, nothing to wait for"
      continue
    fi
    local i pid="" prev="" stable=0
    for i in $(seq 1 45); do
      pid="$(launchctl print "gui/$(id -u)/$label" 2>/dev/null | awk -F'= ' '/^\tpid =/ {print $2; exit}')"
      if [ -n "$pid" ] && [ "$pid" = "$prev" ]; then
        stable=$((stable + 1))
        [ "$stable" -ge 3 ] && break
      else
        stable=0
      fi
      prev="$pid"
      sleep 1
    done
    if [ -n "$pid" ] && [ "$stable" -ge 3 ]; then
      echo "    $label is back (pid $pid, held for 3s)"
    else
      # Loud, and non-fatal on purpose: the binary IS published, so silence would be the lie.
      echo "FAIL: $label did not settle within 45s after replacing $dst — check: launchctl print gui/$(id -u)/$label" >&2
      FAILED_RESTARTS=1
    fi
  done
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
# Is (binary, installed-program) a pair this build declares? Asks the registry when a built gateway
# is available, and falls back to the historical list when there is none to ask — which is exactly
# the first install on a fresh machine.
declared_pair() {
  local want="$1" have="$2"
  local reg=""
  for candidate in "$ROOT/target/release/rozum-gateway" "$ROOT/target/debug/rozum-gateway" "$(command -v rozum-gateway 2>/dev/null)"; do
    [ -n "$candidate" ] && [ -x "$candidate" ] || continue
    reg="$("$candidate" services --json 2>/dev/null)" && [ -n "$reg" ] && break
    reg=""
  done
  if [ -n "$reg" ]; then
    # `rozum-ctrl` is the gateway binary under another name: the deploy's own decision, kept here
    # because it is a fact about THIS machine's paths, not about what the product declares.
    #
    # The rewrite is REMEMBERED, and the `rozum` exception below applies only when it happened.
    # Without that, the exception also matched a path genuinely named `rozum-gateway`, and on
    # 2026-08-13 a plain `install-bins.sh` published the 634 KB `rozum` dispatcher over the 56 MB
    # engine that SIX launchd jobs exec — including the resident model. Nothing fell over only
    # because every one of them was holding the old inode; the next restart would have found a
    # binary that execs itself. This is the same mistake as 2026-08-08 with the sides swapped, so
    # the fix is to stop encoding "these two names are interchangeable" at all.
    local rewritten=0
    [ "$have" = rozum-ctrl ] && { have=rozum-gateway; rewritten=1; }
    printf '%s' "$reg" | grep -q "\"program\": \"$have\"" || return 1
    [ "$want" = "$have" ] || { [ "$want" = rozum ] && [ "$rewritten" = 1 ]; } || return 1
    return 0
  fi
  case "$want:$have" in
    rozum-gateway:rozum-gateway|nadia:nadia|rozum:rozum|rozum:rozum-ctrl|rozum-meet:rozum-meet|rozum-meeting-ssc:rozum-meeting-ssc|rozum-ucc-ssc:rozum-ucc-ssc) return 0 ;;
    *) return 1 ;;
  esac
}

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
    # WHICH program lives at which name is the DEPLOY's decision, not a guess from the filename —
    # and guessing cost something: `~/.rozum/bin/rozum-ctrl` is `deploy-ucc-web.sh`'s `$BIN`, i.e.
    # the 627 KB DISPATCHER (`rozum-cli`), and on 2026-08-08 this list assumed the name meant the
    # engine and published the 54 MB `rozum-gateway` over it. Nothing broke only because
    # `com.rozum.ucc-control` was still running its old process.
    #
    # The pairs now come from `src/services.rs` via `rozum-gateway services --json`, so this file is
    # no longer a second copy of that list. The fallback is the old hardcoded set, for the one case
    # that matters: bootstrapping a machine where no gateway binary exists yet to ask.
    if ! declared_pair "$name" "$(basename "$prog")"; then continue; fi
    [ "$prog" = "$CARGO_BIN/$name" ] && continue
    out+=("$prog")
  done
  printf '%s\n' "${out[@]}" | sort -u | grep -v '^$' || true
}

# `"${@:-a b c}"` expands the default as ONE word, so a no-argument run asked for a binary called
# "rozum-gateway nadia rozum rozum-meet rozum-meeting-ssc" and died in `cargo` with "package name
# cannot be empty". Every use so far had passed an explicit name, which is exactly why the plainest
# invocation was the broken one.
FAILED_RESTARTS=0

[ $# -gt 0 ] || set -- rozum-gateway nadia rozum rozum-meet rozum-meeting-ssc rozum-ucc-ssc

for name in "$@"; do
  case "$name" in
    rozum-meeting-ssc) install_ssc rozum-meeting-ssc clients/meeting/build.sh; continue ;;
    rozum-ucc-ssc)     install_ssc rozum-ucc-ssc clients/control/build-public-matrix.sh; continue ;;
  esac
  install_one "$name"
  while read -r p; do
    [ -n "$p" ] || continue
    echo "==> also $p (a launchd job execs this copy)"
    install_to "$name" "$p"
  done <<<"$(extra_paths_for "$name")"
done

# A publish that left a service down is not a success, whatever the rename did.
[ "$FAILED_RESTARTS" = 0 ] || exit 1
