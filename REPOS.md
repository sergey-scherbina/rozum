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
