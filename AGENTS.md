# Agent Instructions

**This file is the source of truth for how work is organized here. Read it at the start of every
task and again after any context rotation — it changes, and a stale copy in your head is how a rule
gets skipped.** In the autonomous loop, re-read it from `origin/master` at the top of every
iteration and apply whatever changed. `CLAUDE.md` exists only to send you here.

SPRINT: SPRINT.md
BACKLOG: BACKLOG.md
CHANGELOG: CHANGELOG.md
REPOS: REPOS.md
specs: docs/specs
SPEC: SPEC.md

## Skills

Skills live in the `vendor/agent-plugins/` submodule. **Read `vendor/agent-plugins/AGENTS.md` —
the index — and load any listed skill's `commands/<name>.md` on demand when its *When to use*
matches.** The index is the source of truth, not this file: any subdirectory with a
`commands/<name>.md` is a skill, so skills added to the submodule appear automatically with no edit
here. Update them all with `git submodule update --remote vendor/agent-plugins`.

> **Why this is a pointer and not a list.** It used to name four skills by hand, and the four it did
> not name (`bugs`, `scrumban`, `isolate`, and later `performance` + `policy`) were invisible to
> every agent that read only this file — while `BUGS.md` quietly pointed at the `bugs` skill on its
> own. A hand-kept list drifts silently; the index cannot. **Do not re-expand this into a list.**

Two of them are load-bearing here and are worth naming for their project bindings, not to shorten
the index: **`multi-agent`** — the claim/heartbeat/worktree protocol; the work queue is `SPRINT.md`
and claims live in `.work/active/<slug>.claim`. **`multi-repo`** — the registry is `REPOS.md`; this
repo plus the vendored `mistral.rs` fork are one virtual monorepo.

**Keep the submodule current.** A stale pin is not a cosmetic problem: on 2026-08-04 the pin was 7
commits behind, and one of those commits raised the `multi-agent` claim-staleness threshold from 20
to 45 minutes *to match the enforcing code*. An agent following the stale copy would have declared a
live claim stale and taken work another agent was doing.

## Meeting-room coordination (use it)

This project's agents coordinate through their `rozum` meeting room — it is how you
avoid clashing with sibling agents and how the human operator sees and steers the
work. Spec: `docs/specs/agent-meeting-coordination.md`. You join automatically: via
`rozum launch`, or globally once the operator runs `rozum mcp install`. On join the proxy
posts a `joined:` presence line under your own handle (and `left:` when your session ends),
for every agent — no per-client hooks. The human watches + intervenes from any client
(`rozum` TUI, web, bridges); their messages are priority.

**First thing in a session, identify yourself: `rozum meetings hello <your-handle>`** (once).
This binds this session to your OWN name (keyed by `$CLAUDE_CODE_SESSION_ID`) so every
`meetings post` shows YOU — not the operator's account, which is what you inherit otherwise
(then everyone looks like the same person and the human can't tell who is who). `rozum meetings
whoami` confirms it; `rozum meetings who` lists who else is live and where (worktree/cwd). The
human is one identity by account/login; each agent is its own — keep them distinct.

Coordinate on your own judgement, when it helps — not on every step:

1. When you START something non-trivial (or step away for >~30 s of local work —
   edits, builds, spec writing), `meeting.submit` a short `working: <what>` so the
   human and siblings see it instead of silence while `mark_responding` decays.
2. BEFORE editing files or starting a task, check recent messages
   (`meeting.wait_my_turn` / `meeting.status`): if a sibling is on the same
   files/task, coordinate instead of clashing; check `responding` so two agents
   don't write the same reply.
3. When BLOCKED or unsure, ask in the room.
4. On finish/return, `meeting.submit` `done: <result>` (or `blocked: <why>`) before
   any longer message.

Quick one-shot post from a shell/script (or a hook): `rozum meetings post "<text>"`
(posts to the cwd project's room; `--room <name>` / `--as <who>` to override).
Anyone may submit at any time — no turn-taking, no moderator. Honor this whether the
room is on the local TUI, the web bridge, or a daemon client.
