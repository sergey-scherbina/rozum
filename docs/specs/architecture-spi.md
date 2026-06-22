# Architecture: SPI boundaries & plugin legibility

## Overview

A map of rozum's extension points — the seams along which models, tools, agents,
and services vary — so the system is **legible**: a reader (or a future agent) can
see *where* a concern lives and *how* to add one without spelunking. The goal is
not "plugin-ize everything." Two of the four axes are already behind clean SPIs;
one axis (the agent / model tool-format layer) is the real tangle and the only
high-value extraction; the fourth (services) is deliberately left as subcommands.

This aligns with the North Star (`SPEC.md`): the `ChatBackend` SPI is the durable,
hardware-agnostic layer; this spec names the *other* seams to the same standard.

## Interface

The contracts callers depend on, by axis. **EXISTS** = already an SPI (document,
don't rebuild). **PROPOSED** = extract from today's tangle.

### Models — `trait ChatBackend` (EXISTS) — `src/backend.rs:250`

Async chat with tool-use / streaming / cancel. ~12 impls: engines
(`MlxNativeBackend`, `MistralrsBackend`, `GgufBackend`, `LlamaGgufCommandBackend`,
`CandleBackend`, `AnthropicHttpBackend`), decorators (`BackendOrchestrator`
cascade `:1172`, `AdmittingBackend` admission `src/concurrency.rs:737`), and test
doubles. Selection is project-owned via `BackendRegistry` / `BackendConfig` /
`BackendPolicy` (`SPEC.md` Runtime Contract). **Action: document, not change.**

### Tools — `trait ToolSource` (EXISTS) — `src/agent.rs:49`

`tools() -> Vec<ToolDef>` + `async dispatch(name, args)`. `CallbackToolSource`
(in-process) today; an MCP-client adapter is the planned sibling so external MCP
servers and in-process tools share one seam. **Action: document; land the MCP
adapter so "rozum's own tools" and "external tools" are one SPI.**

### Agent dialect — `trait WireProtocol` (PROPOSED)

The agent-facing wire format. Today hand-branched in `src/gateway.rs`: OpenAI Chat
(`/v1/chat/completions`), Anthropic Messages (`/v1/messages`), OpenAI Responses
(`/v1/responses`, Codex — `:1380`), per-protocol serializers, `tool_choice`
normalization (`:1101`–`:1141`), and agent-specific tool-set policy
(`codex_lean_keep` `:2072`). Proposed seam:

```rust
trait WireProtocol {            // one per agent dialect: Chat | Messages | Responses
    fn parse_request(&self, body: &Value) -> ChatRequest;          // → internal
    fn serialize(&self, ev: &ChatEvent, sink: &mut ResponseSink);  // internal → wire (stream/non-stream)
    fn tool_policy(&self) -> ToolPolicy;                            // e.g. codex-lean filter
}
```

### Model tool format — `trait ToolDialect` (PROPOSED)

The model-facing tool format, keyed by model family. Today spread across three
files: emit-parsing in `src/serving.rs` (`parse_tool_calls`, `parse_glm_tool_call`),
prompt rendering in `src/mlx_native_backend.rs` (`glm_conversation`,
`harmony_conversation`, `render_prompt_opt`), and the constraint envelope in
`ToolConstraint` (qwen `<tool_call>` / harmony / GLM `name\njson`) +
`src/constrain.rs`. Proposed seam:

```rust
trait ToolDialect {             // one per model family: QwenXml | Harmony | GlmNameJson | …
    fn render_tools(&self, msgs, tools) -> Conversation;  // tool defs + results → the model's prompt form
    fn parse_calls(&self, text: &str) -> Vec<ToolCall>;   // model output → structured calls
    fn constraint(&self, tools) -> Option<Box<dyn ConstraintDriver>>; // logit envelope (e.g. GLM anchor)
}
```

### Services — subcommands (UNCHANGED, by decision)

`gateway`, `web`, `meetings`, `mcp`, `telegram`, `discord`, `launch`, … stay match
arms on the `Command` enum (`src/main.rs`). Not a plugin axis — see Decisions.

## Behavior

- [ ] `SPEC.md` (or this spec) names all four axes and which are SPI vs subcommand,
  so a reader finds the seam for any concern in one hop.
- [ ] `ChatBackend` and `ToolSource` are documented as the model/tool SPIs with
  their impl inventory and file anchors (no code change).
- [ ] The gateway's agent-dialect branching is extractable behind `WireProtocol`
  with the three existing dialects (Chat / Messages / Responses) as the first impls;
  request parse + response serialize + tool policy move out of `gateway.rs` body.
- [ ] The per-model tool-format logic is extractable behind `ToolDialect` with
  Qwen-XML, Harmony, and GLM-name-json as the first impls; `serving.rs` parsing +
  `mlx_native_backend.rs` rendering + `ToolConstraint` branches resolve through it.
- [ ] Adding a new agent = one `WireProtocol` impl; adding a new model tool format
  = one `ToolDialect` impl — neither touches the other or the engines.
- [ ] No behaviour change: the matrix (Qwen3.6-35B 10/10, gpt-oss claude 5/5) and
  serving tests are identical before/after each extraction (pure refactor gate).

## Out of scope

- **Services as plugins.** No process/registry/dynamic-load machinery for
  subcommands — net indirection, no payoff for a single local binary (Decisions).
- **Rewriting the gateway or the engines.** This is *extraction* (move existing
  logic behind a named trait), not a redesign. Each step is behaviour-preserving.
- **A dynamic/external plugin loader** (dylibs, WASM, out-of-process). All SPIs are
  in-tree Rust traits; "plugin" here means *a named impl behind a trait*, not a
  loadable artifact. Revisit only if third parties need to extend rozum.
- The `ChatBackend` and `ToolSource` SPIs themselves — they exist and are correct.

## Design

### Current map (the four axes, grounded)

| Axis | Seam today | Where | State |
|---|---|---|---|
| Models / engines | `ChatBackend` + registry | `backend.rs:250`, `concurrency.rs:737` | **SPI ✓** |
| Tools | `ToolSource` (+MCP) | `agent.rs:49` | **SPI ✓** (MCP adapter pending) |
| Agent dialect | hand-branched | `gateway.rs` (`:1380`, `:1101`, `:2072`) | **tangled → WireProtocol** |
| Model tool format | spread x3 files | `serving.rs`, `mlx_native_backend.rs`, `constrain.rs` | **tangled → ToolDialect** |
| Services | `Command` enum | `main.rs` | **subcommands (keep)** |

### Why the two tangles are the whole story

Every per-agent / per-model fix from the agentic-matrix work landed in exactly these
two un-factored layers: codex-lean + Responses shaping (WireProtocol), and
harmony-recovery, GLM `name\njson` constraint, Qwen-XML parsing, loop-breaker /
read-repair (ToolDialect + orchestration policy). Naming the seams turns each future
fix into a contained impl instead of surgery threaded through `gateway.rs` +
`serving.rs` + `mlx_native_backend.rs`. That *is* the legibility the goal asks for.

### Staged plan (each step behaviour-preserving, matrix-gated)

1. **Document** (no code): add an "Extension points" section to `SPEC.md` naming the
   four axes + this spec. Makes the existing SPIs visible. *Cheapest, do first.*
2. **Extract `ToolDialect`** — move `parse_*`, `*_conversation`, and the
   `ToolConstraint` envelope branches behind one trait keyed by model family. Highest
   value: it is the most-spread concern and the one that keeps drawing fixes.
3. **Extract `WireProtocol`** — move request-parse / response-serialize / tool-policy
   per dialect out of the `gateway.rs` body into three impls.
4. **(Optional) MCP `ToolSource` adapter** — unify external + in-process tools.

Cross-cutting robustness (loop-breaker, read-repair) stays an orchestration policy at
the gateway level, parameterised by `WireProtocol` + `ToolDialect` rather than owned
by either — call it out explicitly so it doesn't silently re-tangle.

## Decisions

- **Document models/tools, extract agent/model-format — don't "plugin-ize all."**
  Two axes are already SPIs; re-abstracting them is churn. The agent-dialect and
  model-tool-format layers are genuinely tangled and keep attracting fixes — extract
  those. Rejected: a uniform plugin pass (redundant where SPIs exist).
- **Services stay subcommands.** A registry/process model for `gateway`/`web`/
  `meetings` adds indirection with no payoff for a single local binary; match arms in
  `main.rs` are more legible than a plugin host. Rejected: services-as-plugins.
- **In-tree trait impls, not loadable plugins.** "Plugin" = a named impl behind a
  trait, compiled in. No dylib/WASM/out-of-process loader — that solves third-party
  extension, which rozum does not need yet. Rejected: dynamic loader (premature).
- **Two seams, not one `AgentProfile`.** The tangle is two orthogonal concerns —
  *agent* dialect (Chat/Messages/Responses) and *model* tool format (Qwen/Harmony/
  GLM). Folding them into one profile re-couples what varies independently (any agent
  × any model). Rejected: a single combined profile.
- **Refactor is behaviour-preserving + matrix-gated.** Each extraction must leave the
  agentic matrix and serving tests byte-identical; the spec is the gate, the matrix
  is the proof. No semantics change rides along.

## Results

<!-- Fill after each stage: what moved, line-count deltas, matrix/serving parity. -->
_Pending — spec gate only; no code yet._
