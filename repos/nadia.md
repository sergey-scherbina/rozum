# nadia

url: git@github.com:sergey-scherbina/nadia.git
web: https://github.com/sergey-scherbina/nadia
path: ../nadia   (sibling of rozum, like scalascript)

Reference for both halves, from this side: `docs/nadia.md`.

## Overview

An LLM coding agent in Scala and ScalaScript, driving a local model through the
rozum gateway. Two front-ends over one loop: a headless **batch CLI** — a drop-in
row in `scripts/bench/agentic.sh` next to `claude` / `codex` / `opencode` — and an
interactive **REPL**.

The repo holds two of the three implementations: **Scala 3** (`scala/`, over its
own 323-line SDK) and **ScalaScript** (`src/`, over `std.agent`). The third is
Rust, and it lives here in `crates/nadia` — the reference, and the one carrying
subagents, the HTTP control surface and the Telegram front-end.

It also ships deployable: a container image, Kubernetes/ECS/Cloud Run manifests,
and `--provider local|huggingface|openai|bedrock|vertex` so the model can come
from a gateway you run, from the Hub, from Bedrock or from Vertex. `local` —
this gateway, no credential — stays the default. `huggingface` routes on the
repository id, because an id alone does not say whether the Hub is serving it
(partner-hosted, needs a token) or only storing the weights
(`mlx-community/…` — fetched and served by your own gateway, no token).
See `nadia:docs/deployment.md`.

Spec: `nadia:SPEC.md`.

## Why it exists here

It is the app leaf of the split this repo already specified in
`docs/specs/integration.md`: rozum is the stateless model service (Contract 1),
the agent side implements Contracts 2–3, and
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

## What rozum owes it — settled

Nothing, as it turned out: no `rozum launch` branch was needed. nadia reads
`OPENAI_BASE_URL` / `ROZUM_GATEWAY_URL`, which launch already exports to every
child, and its workspace defaults to the cwd launch has already jailed — so the
matrix row is `rozum launch … nadia run "$prompt"` with no provider flags at
all, against `claude`'s injected env and `opencode`'s written config. That is
the whole benefit of being wired by the plain env contract rather than by a
per-agent special case.

## Dependencies

Per implementation, which is the point of having three. The **Rust** one depends
on this workspace (`rozum-agent`, `rozum-gateway`) and nothing else; **Scala 3**
on a JDK and upickle; **ScalaScript** on the `ssc` toolchain and `std.*`. All
three want a gateway with a tool-capable model — or, since the deployment work,
any OpenAI-compatible endpoint including Bedrock and Vertex.

The two upstream scalascript gaps that once blocked P0 are resolved or retracted;
what remains is tracked in `nadia:BACKLOG.md`.

## Agents / coordination

AGENTS.md in the repo. Shares this project's `rozum` meeting room.
