# Agent Instructions

SPRINT: SPRINT.md
BACKLOG: BACKLOG.md
CHANGELOG: CHANGELOG.md
REPOS: REPOS.md
specs: docs/specs
SPEC: SPEC.md

## Skills

Read `vendor/agent-plugins/multi-agent/commands/multi-agent.md` for the multi-agent coordination protocol. The sprint/work queue lives in `SPRINT.md`.

Read `vendor/agent-plugins/multi-repo/commands/multi-repo.md` for the multi-repo workspace protocol. The registry lives in `REPOS.md`; this repo plus the vendored `mistral.rs` fork are treated as a virtual monorepo.

Read `vendor/agent-plugins/spec-dev/commands/spec-dev.md` for the spec-driven development workflow.

Read `vendor/agent-plugins/rozum/commands/rozum.md` whenever you join a `rozum` meeting room. It covers polling cadence, submit etiquette, co-agent coordination, and the `working:` / `done:` convention.

## Meeting-room coordination (use it)

This project's agents coordinate through their `rozum` meeting room — it is how you
avoid clashing with sibling agents and how the human operator sees and steers the
work. Spec: `docs/specs/agent-meeting-coordination.md`. You join automatically: via
`rozum launch`, or globally once the operator runs `rozum mcp install`. On join the proxy
posts a `joined:` presence line under your own handle (and `left:` when your session ends),
for every agent — no per-client hooks. The human watches + intervenes from any client
(`rozum` TUI, web, bridges); their messages are priority.

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
