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

## mlx-lm
url: https://github.com/sergey-scherbina/mlx-rs.git
path: .vendor/mlx-lm
branch: main
# Fork of oxideai/mlx-rs (workspace: mlx-rs + mlx-lm + mlx-lm-utils). We extend
# the mlx-lm crate with the model architectures rozum needs (MoE, Qwen3.6
# hybrid) for the native MLX runtime. Fork URL provisional until created.

## scalascript
url: git@github.com:sergey-scherbina/scalascript.git
path: ../scalascript
branch: main
# ScalaScript (`.ssc`): Markdown+Scala 3 meta-language, target-agnostic
# (interpreter · JS transpiler · JVM · Rust). Points at the operator's existing
# working checkout (sibling dir), not a fresh `.vendor/` clone. Candidate to
# author the rozum meeting web UI: the JS backend is mature (frontend);
# the Rust backend is early ("R.1 hello-world subset"). See repos/scalascript.md.

## nadia
url: git@github.com:sergey-scherbina/nadia.git
path: ../nadia
branch: master
# An LLM coding agent in Scala and ScalaScript, driving a local model
# through this gateway. Batch CLI (a drop-in row in scripts/bench/agentic.sh
# next to claude/codex/opencode) + interactive REPL. The third implementation
# is Rust and lives HERE in crates/nadia — the reference, and the one carrying
# subagents-as-actors, the HTTP control surface and the Telegram front-end.
# Also deployable: a container image, k8s/ECS/Cloud Run manifests, and
# --provider local|huggingface|openai|bedrock|vertex — `local`, meaning this
# gateway with no credential, stays the default (nadia:docs/deployment.md).
# It is the app leaf of the split specified in
# docs/specs/integration.md — rozum stays the stateless model service, and
# per-family tool rendering/parsing stays HERE, never on the agent side.
# Both halves documented from this side in docs/nadia.md; see repos/nadia.md.
