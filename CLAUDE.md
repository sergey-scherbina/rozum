# CLAUDE.md

## Before you start anything

**Read `AGENTS.md` first — every time you begin a task.** It is the source of
truth for how work is organized in this repo. Do not work from memory:

- Re-read it at the start of each task and after any context rotation — it
  changes, and a stale copy in your head will make you skip a rule.
- Check that you have not forgotten anything it asks for: the pipeline files
  (`SPRINT.md` / `BACKLOG.md` / `CHANGELOG.md`), the multi-repo registry
  (`REPOS.md`), the spec locations (`SPEC.md`, `docs/specs/`), and the
  meeting-room conventions.
- In the autonomous loop, re-read `AGENTS.md` from `origin/master` at the top of
  every iteration and apply any updated rules.

## Skills

This repo drives its workflow through the `agent-plugins` skills vendored at
`vendor/agent-plugins/`. `AGENTS.md` links each one; use them — don't reinvent
the protocols by hand:

- **multi-agent** — coordination for parallel feature-branch work: claims,
  heartbeats, triage, worktrees, the autonomous loop.
  `/multi-agent` · `vendor/agent-plugins/multi-agent/commands/multi-agent.md`
- **spec-dev** — write the spec before the code, keep them in sync.
  `/spec-dev` · `vendor/agent-plugins/spec-dev/commands/spec-dev.md`
- **multi-repo** — manage `REPOS.md` as a virtual monorepo. Active here: this
  repo plus the vendored `mistral.rs` fork (`.vendor/mistral-rs`).
  `/multi-repo` · `vendor/agent-plugins/multi-repo/commands/multi-repo.md`

The **rozum** meeting-room etiquette skill applies whenever you join a room —
see `AGENTS.md` and `vendor/agent-plugins/rozum/commands/rozum.md`.
