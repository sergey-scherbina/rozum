# Repositories

Virtual monorepo for the local-LLM-hosting side of rozum. `rozum` consumes
`mistral.rs` through `[patch.crates-io]` pinned to a git rev on the fork below;
day-to-day work on the engine happens in the vendored checkout under `.vendor/`
(git-ignored — local only, not committed to `rozum`).

See `vendor/agent-plugins/multi-repo/commands/multi-repo.md` for the protocol.

## rozum
url: git@github.com:sergey-scherbina/rozum.git
path: .
branch: master

## mistral-rs
url: https://github.com/sergey-scherbina/mistral.rs.git
path: .vendor/mistral-rs
branch: qwen36-chunked-prefill
