# Agent Instructions

**This file is the source of truth for how work is organized here. Read it at the start of every
task and again after any context rotation, and read it from `origin/master` — not your own
checkout, which is only as fresh as your last fetch.** In the autonomous loop that means re-reading
it at the top of every iteration and applying whatever changed. `CLAUDE.md` exists only to send you
here.

This is rule zero of the `multi-agent` skill; the reasoning lives there
(`vendor/agent-plugins/multi-agent/commands/multi-agent.md`) and is not repeated here. It is
restated at all only because you read this file before you load any skill.

SPRINT: SPRINT.md
BACKLOG: BACKLOG.md
CHANGELOG: CHANGELOG.md
REPOS: REPOS.md
specs: docs/specs
SPEC: SPEC.md

## The shared checkout belongs to nobody

**Enforced since 2026-08-08.** `bash scripts/githooks/install.sh` once per clone points git at the
tracked hooks; `pre-commit` then refuses any staged path outside `.work/**` and the boards
(`SPRINT.md`, `BACKLOG.md`, `BUGS.md`, `CHANGELOG.md`, `REPOS.md`) **in the shared checkout only** —
worktrees are untouched, and merges, rebases and cherry-picks pass, because that is what this
checkout is for. Known limit, tested and named in the message: a CLEAN `git revert` is also refused,
because git leaves no marker a hook can read for one. Use `--no-verify` there.
`scripts/githooks/test-pre-commit.sh` proves all nine cases against a throwaway repo.



Two agents lost work in it on 2026-08-06, in opposite directions, and neither mistake was exotic:

- One committed a tool fix straight into the shared checkout (`scripts/rust-item-spans.py`), where
  it sat unpushed while a sibling rebased the branch under it and its hash changed.
- The sibling ran `git add -A` there to commit a claim file, swept that same edit into a
  coordination commit, and **pushed it** — publishing someone else's work in progress and
  splitting it across two histories.

So, concretely, in the shared checkout:

- **Never `git add -A` / `git add .`** — name the path: `git add .work/active/<slug>.claim`. Every
  coordination commit touches files you can list; if you cannot list them, you are in the wrong
  tree.
- **Never leave feature work there.** Not a one-line tool fix, not a script tweak. `git worktree
  add .worktrees/feature/<slug>` costs a second and cannot be swept up by anyone.
- **Check before you commit**: `git status --short` in the shared checkout should show only what
  you are about to commit. Anything else is a sibling's, and rebasing on top of it rewrites their
  hashes.

The shared checkout is for reading state, coordination commits (claims, releases, board), and
fast-forwarding a finished branch. That is the whole list.

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
