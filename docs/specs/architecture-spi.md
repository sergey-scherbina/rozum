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

### Agent dialect — wire protocols (MAPPED, no trait — see Decisions)

The agent-facing wire format: OpenAI Chat (`/v1/chat/completions`), OpenAI Responses
(`/v1/responses`, Codex), Anthropic Messages (`/v1/messages`). **Investigated for a
`WireProtocol` trait; found already factored** — each dialect is a *thin* handler over
named per-dialect `*_to_internal` parse fns + a `*_sse_stream` serializer, all converging on
the internal `ChatRequest` / `ChatEvent`. So a trait is **not** warranted (it would force
uniformity over genuinely different typed extractors + SSE sequences + axum routes — a
behaviour change or a fat trait, net-negative on matrix-critical code). The legibility win is
a one-hop **map**, now the `gateway.rs` module doc:

| Dialect | Parse | Serialize | Handler |
|---|---|---|---|
| OpenAI Chat | `oai_messages_to_internal` / `oai_tools_to_internal` | `oai_sse_stream` | `oai_chat_handler` |
| OpenAI Responses (Codex) | `responses_input_to_internal` / `responses_tools_to_internal` (+ `codex_lean_keep`) | `responses_sse_stream` | `responses_handler` |
| Anthropic Messages | `anthropic_messages_to_internal` / `anthropic_tools_to_internal` | `anthropic_sse_stream` | `anthropic_handler` |

Cross-cutting (owned by neither dialect): loop-breaker (`chat_or_loopbreak` /
`detect_stuck_loop`), `parse_response_format`, `tool_choice` normalization.

### Model tool format — `trait ToolDialect` (PROPOSED)

The model-facing tool format, keyed by model family. Today spread across three
files: emit-parsing in `src/serving.rs` (`parse_tool_calls`, `parse_glm_tool_call`),
prompt rendering in `src/mlx_native_backend.rs` (`glm_conversation`,
`harmony_conversation`, `render_prompt_opt`), and the constraint envelope in
`ToolConstraint` (qwen `<tool_call>` / harmony / GLM `name\njson`) +
`src/constrain.rs`. Proposed seam:

```rust
trait ToolDialect {             // one per model family: Qwen(default) | Harmony | Glm
    fn render_message(&self, msg) -> Conversation;  // history msg → the model's prompt form
    fn uses_glm_envelope(&self) -> bool;            // selects the constraint envelope (GLM anchor)
}
// dialect_for(template) -> &'static dyn ToolDialect   (chosen once from template markers)
```

**Parse is deliberately NOT on this trait.** `serving::parse_tool_calls` is a generic
*union* that tries every form (`<tool_call>`, loose JSON, GLM `name\njson`) — robust
across dialects, so it needs no per-family dispatch. The dialect owns only what genuinely
varies per family: **render** + the **constraint envelope** selector. (The earlier sketch
listed `parse_calls`/`constraint(tools)`; the union parser makes per-family parse
unnecessary, and the constraint envelope reduces to the `uses_glm_envelope` flag the
existing `ToolConstraint` consumes.)

### Services — subcommands (UNCHANGED, by decision)

`gateway`, `web`, `meetings`, `mcp`, `telegram`, `discord`, `launch`, … stay match
arms on the `Command` enum (`src/main.rs`). Not a plugin axis — see Decisions.

## Behavior

- [x] `SPEC.md` (or this spec) names all four axes and which are SPI vs subcommand,
  so a reader finds the seam for any concern in one hop. (Stage 1, `SPEC.md`
  "Extension points".)
- [x] `ChatBackend` and `ToolSource` are documented as the model/tool SPIs with
  their impl inventory and file anchors (no code change). (Stage 1.)
- [x] The gateway's agent-dialect layer is legible in one hop: the `gateway.rs`
  module doc maps each of the three dialects to its parse fns / serializer / handler.
  (Stage 3 — investigated a `WireProtocol` trait; found already factored, mapped
  instead — see Interface + Decisions.)
- [x] The per-model tool-format logic resolves through `ToolDialect` —
  `dialect_for(template)` picks `Qwen`/`Harmony`/`Glm`; render + the constraint
  envelope flag flow through it. (Stage 2; parse stays a generic union — see
  Interface.)
- [x] Adding a model tool format = one `ToolDialect` impl + one `dialect_for` arm
  (engines untouched). Adding an agent dialect = copy the parse/serialize/handler
  triple the module-doc map names (no trait, by decision).
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

- **Document models/tools, extract only what's genuinely tangled — don't "plugin-ize
  all."** Two axes are already SPIs; re-abstracting them is churn. Of the two suspected
  tangles, only one was real: the model-tool-format dispatch *was* scattered (extracted
  Stage 2 → `ToolDialect`); the agent-dialect layer turned out *already factored* (Stage 3
  → mapped, not extracted). Rejected: a uniform plugin pass (redundant where SPIs exist).
- **Agent dialect: a map, not a `WireProtocol` trait.** Investigated (Stage 3). The
  gateway is already at a clean per-route boundary — named per-dialect `*_to_internal`
  parse fns + `*_sse_stream` serializers + thin handlers, all converging on
  `ChatRequest`/`ChatEvent`. A unifying trait would force uniformity over genuinely
  different typed extractors (`OaiChatReq` vs `Value` vs `AnthropicMsg`) and SSE event
  sequences → either looser validation (behaviour change) or a fat trait that adds
  indirection without removing complexity, on matrix-critical code. The legibility goal is
  met by the `gateway.rs` module-doc map. Rejected: a forced trait (net-negative). This is
  the same principle as "services stay subcommands" — abstraction only where it pays.
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

- **Stage 1 (docs) — DONE** (`SPEC.md` "Extension points", merged `2b0a135`). Names the
  four axes; makes the existing `ChatBackend` / `ToolSource` SPIs visible in one hop.
- **Stage 2 (`ToolDialect`) — DONE.** Consolidated the scattered template-marker dispatch
  (`contains("<|channel|>"/"<|observation|>")` in render + constraint) into one
  `trait ToolDialect` + `dialect_for(template)` (`Qwen`/`Harmony`/`Glm`) in
  `mlx_native_backend.rs`. Render routes through `dialect.render_message`; the constraint
  GLM flag through `dialect.uses_glm_envelope()`. The render fns
  (`harmony_conversation`/`glm_conversation`) are unchanged impl bodies. Behaviour-preserving:
  448/0 lib tests, `cargo check` clean. **Honest scope correction:** parse stays a generic
  union (`serving::parse_tool_calls`) — robust across dialects, not per-family; the dialect
  owns only render + the envelope flag (what actually varies). Adding a model family's tool
  format = one `ToolDialect` impl + one arm in `dialect_for`.
- **Stage 3 (`WireProtocol`) — DONE as a map, trait rejected.** Investigated the gateway
  wire layer for a trait extraction; found it already factored at a clean per-route boundary
  (named per-dialect parse + serialize fns + thin handlers converging on
  `ChatRequest`/`ChatEvent`). The stale module doc claimed "two dialects" (there are three —
  Responses/Codex was added since). Replaced it with an accurate **wire-protocol map**
  (`gateway.rs` module doc): each of OpenAI Chat / OpenAI Responses / Anthropic Messages →
  its parse fns, serializer, handler, tool policy, plus the cross-cutting orchestration.
  Docs-only, behaviour-preserving (no code change). A `WireProtocol` trait was **rejected**
  as forced abstraction over different typed extractors + SSE sequences — net-negative on
  matrix-critical code (see Decisions). Adding an agent dialect = copy the triple the map
  names.
- **Stage 4 (MCP `ToolSource` adapter) — optional follow-up.** Not blocking; the tool SPI
  already exists, this only unifies external MCP + in-process tools behind it.
- **Net outcome.** The legibility goal is met: every concern (model / tool / agent dialect /
  model tool-format / service) has a named seam findable in one hop — `SPEC.md` "Extension
  points" → `ChatBackend` (`backend.rs`), `ToolSource` (`agent.rs`), `ToolDialect`
  (`mlx_native_backend.rs`), the `gateway.rs` wire-protocol map, the `Command` enum. One new
  trait (`ToolDialect`) where the dispatch was genuinely scattered; maps + docs everywhere it
  was already factored. No churn, no forced abstraction, no behaviour change.
