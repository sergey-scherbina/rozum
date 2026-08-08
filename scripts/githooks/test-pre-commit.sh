#!/usr/bin/env bash
#
# Prove the shared-checkout guard refuses what it should and — the half that matters more — allows
# everything the shared checkout is FOR. A guard that blocks a merge would be worse than no guard:
# merging finished branches is the main thing that happens there.
#
# Runs against a throwaway repo, so it cannot touch this one.

set -uo pipefail
HOOK="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/pre-commit"
TMP="$(mktemp -d)"; trap 'rm -rf "$TMP"' EXIT
pass=0; fail=0
ok(){ printf '  ok   %s\n' "$1"; pass=$((pass+1)); }
no(){ printf '  FAIL %s\n' "$1"; fail=$((fail+1)); }

mkdir -p "$TMP/hooks" && cp "$HOOK" "$TMP/hooks/pre-commit" && chmod +x "$TMP/hooks/pre-commit"

# ── a "shared checkout": a plain repo, .git is a directory ────────────────────────────────────
main="$TMP/main"
git init -q "$main" && cd "$main"
git config core.hooksPath "$TMP/hooks"
git config user.email t@t && git config user.name t
mkdir -p src .work/active
echo one > src/lib.rs; echo claim > .work/active/x.claim; echo board > SPRINT.md
git add . && git commit -q --no-verify -m base
git branch -M master

commit_try(){ git commit -q -m "$1" >/dev/null 2>&1; }

# 1. feature work → refused
echo two > src/lib.rs; git add src/lib.rs
if commit_try "src edit"; then no "a src/ edit must be refused"; git reset -q --soft HEAD~1; else ok "a src/ edit is refused"; fi
git restore --staged src/lib.rs; git checkout -- src/lib.rs

# 2. coordination → allowed
echo claim2 > .work/active/x.claim; git add .work/active/x.claim
if commit_try "claim"; then ok "a claim commits"; else no "a claim must commit"; fi

# 3. a board file → allowed
echo board2 > SPRINT.md; git add SPRINT.md
if commit_try "board"; then ok "a board file commits"; else no "a board file must commit"; fi

# 4. mixed → refused (the src/ file is the point; allowing it because a claim rode along is how the
#    rule gets bypassed by accident)
echo three > src/lib.rs; echo claim3 > .work/active/x.claim; git add .
if commit_try "mixed"; then no "a mixed commit must be refused"; git reset -q --soft HEAD~1; else ok "a mixed commit is refused"; fi
git reset -q --hard >/dev/null

# 5. THE MERGE CASE — the shared checkout's whole job
git checkout -q -b feature/x
echo feat > src/feature.rs; git add src/feature.rs; git commit -q --no-verify -m feat
git checkout -q master
if git merge --no-ff -q -m "merge feature/x" feature/x >/dev/null 2>&1; then ok "a merge commits"; else no "a MERGE must commit — this is what the shared checkout is for"; fi

# 6. a revert → allowed. Revert a PLAIN commit, not the merge above: `git revert <merge>` needs
#    `-m` and fails before the hook is ever consulted, which the first draft of this test read as a
#    hook bug. A test that blames the wrong thing is worse than a missing one.
git checkout -q -b revme master
echo r > src/r.rs; git add src/r.rs; git commit -q --no-verify -m "to be reverted"
# A CLEAN revert is refused, and that is the hook's known limit rather than a bug: git writes no
# marker for a revert that applies cleanly, and pre-commit gets no argument to tell it from a hand
# edit. Pinned here so it stays a decision and does not quietly become a surprise — and so the
# message keeps NAMING it, which is what makes the block a two-second decision instead of a puzzle.
if git revert --no-edit -q HEAD >/dev/null 2>&1; then
  no "a clean revert is expected to be REFUSED (known limit) — if it now passes, update this test"
else
  ok "a clean revert is refused, as documented"
  git revert --quit >/dev/null 2>&1 || true
fi
# The message must NAME the revert limit, or the block is a puzzle instead of a decision. Stage an
# offending path first: with a clean index the hook exits 0 and prints nothing, and the first draft
# of this check tested the empty string.
echo msg > src/msg.rs; git add src/msg.rs
out="$(bash "$HOOK" 2>&1 || true)"
if printf '%s' "$out" | grep -qi 'revert'; then ok "the message names the revert limit"; else no "the message must name the revert limit"; printf '    got: [%s]\n' "$(printf '%s' "$out" | head -2 | tr '\n' ' ')"; fi
git restore --staged src/msg.rs >/dev/null 2>&1; rm -f src/msg.rs
git checkout -q master

# 7. a rebase that replays a src/ commit → allowed
git checkout -q -b feature/y master~1 2>/dev/null || git checkout -q -b feature/y master
echo y > src/y.rs; git add src/y.rs; git commit -q --no-verify -m y
if git rebase -q master >/dev/null 2>&1; then ok "a rebase replays"; else no "a rebase must replay"; fi
git checkout -q master

# ── a "worktree": .git is a file ───────────────────────────────────────────────────────────────
wt="$TMP/wt"
git worktree add -q "$wt" -b feature/z >/dev/null 2>&1
cd "$wt"
git config user.email t@t && git config user.name t
echo z > src/z.rs; git add src/z.rs
if commit_try "worktree src edit"; then ok "a worktree commits src/ freely"; else no "a WORKTREE must commit src/ — the hook runs there too via hooksPath"; fi

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
