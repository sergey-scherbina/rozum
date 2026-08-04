# CLAUDE.md

**Read [`AGENTS.md`](AGENTS.md) first, before anything else — every task, and again after any
context rotation.** It is the source of truth: boards, specs, the multi-repo registry, the skills
index, and the meeting-room conventions.

Nothing else belongs in this file. It exists for one mechanical reason — this is the file the
harness loads automatically and `AGENTS.md` is not — so its whole job is to send you there. Anything
written here instead would be a second copy of a rule, and the copy is always the one that goes
stale: the three-skill list that used to live here hid `bugs`, `scrumban`, `isolate`, `performance`
and `policy` from every agent that read it and stopped. Put the rule in `AGENTS.md`; leave this a
pointer. Harness settings go in `.claude/settings.json`, not in prose here.
