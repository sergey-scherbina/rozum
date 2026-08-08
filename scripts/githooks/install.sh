#!/usr/bin/env bash
# Point this clone's git at the tracked hooks. One line, but it must be run per clone: hooks are not
# checked out into `.git/hooks` by git, and `core.hooksPath` is local config, not repo content.
#
# Repo-level on purpose: it covers every worktree too. A hook dropped into `.git/hooks` does not,
# which would leave the guard installed exactly where mistakes are hardest to make.
set -euo pipefail
root="$(git rev-parse --show-toplevel)"
git -C "$root" config core.hooksPath scripts/githooks
echo "hooks: core.hooksPath → scripts/githooks   (covers this checkout and every worktree)"
echo "check: bash scripts/githooks/test-pre-commit.sh"
