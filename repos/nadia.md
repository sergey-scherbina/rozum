# nadia

url: git@github.com:sergey-scherbina/nadia.git
path: ../nadia   (sibling of rozum, like scalascript)

## Overview

An LLM coding agent written in ScalaScript, driving a local model through the
rozum gateway. Two front-ends over one loop: a headless **batch CLI** — built to
be a drop-in row in `scripts/bench/agentic.sh` next to `claude` / `codex` /
`opencode` — and an interactive **REPL**. Later: subagents as actors, and a
Telegram front-end.

Spec: `nadia:SPEC.md`.

## Why it exists here

It is the app leaf of the split this repo already specified in
`docs/specs/integration.md`: rozum is the stateless model service (Contract 1),
the scalascript side implements the agent (Contracts 2–3), and
`crates/rozum-agent` is the executable Rust twin of that algorithm.

nadia consumes `scalascript:runtime/std/agent.ssc` (loop, streaming, retry,
schema derivation, MCP bridge — P0–P2 shipped) and adds only what an app owns:
tools, prompts, safety policy, UI.

## The boundary that must not blur

nadia sends **neutral OpenAI-form** tool JSON. Rendering that into the syntax a
model family was trained on — Qwen `<tool_call>`, GLM `<arg_key>`, DeepSeek
`<｜tool▁sep｜>`, harmony — and parsing the reply back stays in this repo
(`crates/rozum-core/src/serving.rs`, the chat templates, constrained decoding).
A second parser on the agent side would be a second source of truth, and the
failure mode is a gateway defect that reads as a model defect — which this
project has already paid for twice (`docs/specs/`, gateway patch-revert work).

## What rozum owes it

One branch in `rozum launch` (`nadia`: gateway base URL + model id), the same
shape as the existing `claude` / `codex` / `opencode` branches. Then
`AGENTS=nadia scripts/bench/agentic.sh` needs no harness change — the matrix is
already parameterized by `AGENTS=` and resolves each agent as a CLI on `PATH`.

## Dependencies

`scalascript` (the `ssc` toolchain and `std.*`), and a running rozum gateway
with a tool-capable model. Two upstream gaps are currently blocking P0 —
`std.process.exec` unbound on the standard lane and no stdin primitive; both are
tracked in `nadia:BACKLOG.md` (NAD-1, NAD-2).

## Agents / coordination

AGENTS.md in the repo. Shares this project's `rozum` meeting room.
