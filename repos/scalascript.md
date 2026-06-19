# scalascript

url: git@github.com:sergey-scherbina/scalascript.git
path: ../scalascript   (the operator's existing checkout, a sibling of rozum)

## Overview

ScalaScript (`.ssc`) — a meta-programming / specification language with hybrid
**Markdown + Scala 3** syntax: `.ssc` files combine YAML front-matter, Markdown
prose, and Scala 3 code blocks, and are first-class executables. Fully
autonomous: real compilation, real execution, no AI at runtime. Target-agnostic:
the same `.ssc` source drives multiple backends.

## Backends and maturity

| Block lang | Backends | Maturity for rozum's web UI |
|---|---|---|
| ` ```scalascript` / ` ```ssc` | interpreter · JS transpiler · JVM · Rust | — |
| ` ```scala` | interpreter · Scala.js (JS) · JVM · Rust (passthrough) | — |
| ` ```rust` | Rust passthrough verbatim | escape hatch |

- **JS backend — mature.** Natural fit for the meeting **frontend**
  (`src/meeting/web_index.html`): author the UI in `.ssc`, transpile to JS.
  Relevant worktrees: `v146-sse-streaming`, `v1-18-A7-frontend-cli`.
- **Rust backend — early ("R.1 hello-world subset").** Source:
  `runtime/backend/rust/src/main/scala/scalascript/codegen/rust`. Docs:
  `docs/rust-backend.md`, `docs/rust-effects.md`. Not yet able to emit the
  rozum **backend** (axum + async + SSE + broadcast + basic-auth in `web.rs`).
  Example crate: `arith-loop-rust/`.

## Build / run

```bash
./setup.sh          # install scala-cli, init submodules, sync agent skills
./install.sh        # build ssc binary via sbt + stage bin/
bin/ssc  file.ssc   # interpreter
bin/jssc file.ssc   # transpile to JS + run via Node
bin/sscc file.ssc   # compile to JVM
# Rust: see docs/rust-backend.md for the cargo-emitting path
```

## Why it's registered here

The operator wants to dogfood ScalaScript as the authoring language for the
rozum meeting web interface. Realistic split: **frontend in ScalaScript→JS now**;
**backend in ScalaScript→Rust later**, once the Rust backend grows async + an
HTTP server story. Co-developing the missing Rust-backend pieces is in scope.

## Agents / coordination

AGENTS.md in the repo. Skills: `/multi-agent`, `/spec-dev`. The two projects
also share a `rozum` meeting room (see `busi-scalascript-needs` socket history).

## Dependencies

Self-contained language runtime; no external deps. Requires `scala-cli`
(+ Node.js for the JS backend, `cargo` for the Rust backend).
