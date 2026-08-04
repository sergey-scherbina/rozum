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
`vendor/agent-plugins/`. **Read `vendor/agent-plugins/AGENTS.md` — the index —
and load any listed skill's `commands/<name>.md` on demand when its *When to
use* matches.** Use them; don't reinvent the protocols by hand.

**This used to be a list of three skills, and that is why it is not one now.**
A hand-kept list here and in `AGENTS.md` left `bugs`, `scrumban`, `isolate`,
`performance` and `policy` unnamed — vendored, working, and invisible to any
agent that read only these two files. The index cannot drift that way: any
subdirectory with a `commands/<name>.md` is a skill, and new ones appear with no
edit here. **Do not re-expand this into a list.** Keep the submodule current
(`git submodule update --remote vendor/agent-plugins`) — a stale pin once had
`multi-agent`'s claim-staleness threshold at 20 minutes when the enforcing code
said 45, which is how a live claim gets taken from the agent holding it.
