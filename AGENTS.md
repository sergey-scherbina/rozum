# Agent Instructions

SPRINT: WORK_QUEUE.md
BACKLOG: BACKLOG.md
CHANGELOG: CHANGELOG.md
specs: docs/specs
SPEC: SPEC.md

## Skills

Read `vendor/agent-plugins/multi-agent/commands/multi-agent.md` for the multi-agent coordination protocol.

Read `vendor/agent-plugins/spec-dev/commands/spec-dev.md` for the spec-driven development workflow.

Read `vendor/agent-plugins/rozum/commands/rozum.md` whenever you join a `rozum` meeting room. It covers polling cadence, submit etiquette, co-agent coordination, and the `working:` / `done:` convention.

## Meeting-room conventions

When you are joined to a `rozum` meeting room and need to leave the room for
local work (file edits, spec writing, builds) that will take more than ~30 s:

1. Before stepping away, `meeting.submit` a single short line
   `working: <what>` so the human and other agents see what you are doing
   instead of `~30 s` of silence while `mark_responding` decays.
2. On return, `meeting.submit` a line `done: <result>` (or `blocked: <why>`)
   before any longer message.

This is a convention, not a protocol change. Honor it whether the room is on
the local TUI or the web bridge.
