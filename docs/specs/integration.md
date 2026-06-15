# busi integration & the agent runtime (distributed-first)

**Status: design / plan.** How an application (first: **busi**, an accounting app
written in **scalascript** — which can compile to Rust, though not all of busi is
guaranteed to be Rust — and which exposes an **MCP** interface) embeds a local model
through rozum to drive its own tools. Designed **distributed-first** (scaling +
fault-tolerance), with an in-process embedded mode kept as an option for small models.

Companion reading: `portability-and-the-backend-spi.md` (the SPI is rozum's durable
boundary; this sits above it), `training-and-lora-exploration.md` (the optional
fine-tune), `mlx-native-runtime.md` (local model serving below).

## The architectural decision: busi is the agent; rozum is a stateless model service

rozum's gateway is **stateless per call**: `messages + tools` in, *either* text *or*
`tool_calls` out. The **agentic loop** (model → tool call → execute → tool result →
model → …) is the *caller's* job. The question is who runs that loop. For a
**distributed, scalable, fault-tolerant** system the answer is:

> **busi owns the agent loop and executes tools in-process; rozum is a stateless
> model service it calls over HTTP.**

Why, specifically:
- **Session / orchestration state lives in busi** (it already owns the user session,
  context, permissions). The loop just grows a `messages` array — that *is* the
  state, and it belongs where the session is.
- **rozum stays stateless** → trivially horizontally scalable and fault-tolerant:
  any instance serves any request; an instance dies → busi retries on another. The
  conversation isn't lost because busi holds it.
- The alternative (**rozum owns the loop**) makes the *agent layer* stateful (live
  sessions in rozum) → losing an instance mid-loop loses state unless externalized,
  plus an extra hop rozum→MCP→busi per tool call. Worse for distribution.

So: **the model layer is stateless and replicated; the orchestration state is in
busi.** Standard, robust LLM-app shape — and it cleanly accommodates "not all of busi
is Rust" (HTTP is language-agnostic).

## Transports — what actually exists

- **HTTP (OpenAI/Anthropic + SSE) — the spine.** busi → rozum gateway: send
  `messages + tools` (JSON-Schema), get back `tool_calls` or text; stream via SSE.
  Language-agnostic (scalascript just needs an HTTP+JSON+SSE client). **This is the
  busi↔rozum integration.**
- **MCP — optional, the *other* direction.** MCP is a tool-*provider* protocol
  (provider → external agent). It's for when an **external** agent (Claude Code, …)
  should drive busi. busi's **own** embedded model does NOT need MCP internally — busi
  executes its tools in-process; the model only needs the tool *schemas* in the
  request, not MCP.
- **Embedded Rust crate — optional optimization.** When a busi component is Rust + the
  model is small + no network is wanted: link rozum in-process. Kept, not primary.
- **Nothing exotic needed.** HTTP+SSE+JSON covers it; gRPC/WebSocket are unnecessary.
  Scaling is a load balancer + health + retries (deployment, not a new protocol).

## End-to-end data flow (busi owns the loop)

```
user prompt (busi UI)
  → busi builds [system (activation), user] + tool schemas
  → busi agent loop:
        POST rozum /v1/chat/completions (messages, tools)
        → tool_calls?  → busi executes each tool in-process (VALIDATES)
                       → append assistant + tool results → ↺
        → final text   → stop
  → busi shows / applies the result (+ keeps the transcript for audit)
```

The model never leaves the loop; **busi validates every operation** (a rejected
operation returns as a tool error the model corrects). No hallucinated entries commit.

## Layering — generic infrastructure vs domain logic

A crucial split (same principle as rozum's portability taxonomy — push generic infra
into the reusable layer, keep domain logic in the leaf). Three tiers:

```
┌───────────────────────────────────────────────────────────────────────┐
│ rozum (Rust)        stateless model service: the OpenAI/Anthropic       │
│                     gateway (tools + SSE) + local MLX/etc. below the SPI │
│                     + (optional) a Rust reference agent-loop.            │
├───────────────────────────────────────────────────────────────────────┤
│ scalascript         GENERIC agent SDK — identical in ANY app, not       │
│ library / compiler  accounting-specific. Build ONCE, reuse everywhere.  │
├───────────────────────────────────────────────────────────────────────┤
│ busi (on scalascript)  DOMAIN: the accounting tools, prompts, rules,    │
│                        eval. Thin layer over the SDK.                    │
└───────────────────────────────────────────────────────────────────────┘
```

**GENERIC → belongs in scalascript (as a library, or — scalascript's call — in the
compiler where type-derivation/codegen helps). Reusable by ANY scalascript app, not
just busi.** Its full design + public API is its own spec: `agent-sdk.md`.
In brief:**
- **Model client** — HTTP/JSON/SSE client to an OpenAI/Anthropic-compatible endpoint.
- **Agent loop** — Contract 2 below (message assembly, tool-call handling, budget,
  stop, retry/error). The scalascript twin of rozum's Rust agent-runtime.
- **Tool framework** — declare/register a tool, serialize its schema into the request,
  dispatch `tool_calls` to handlers, format results/errors (Contract 3 plumbing). The
  *framework* is generic; the *tools* are domain.
- **JSON-Schema derivation from scalascript types** — generating a tool's parameter
  schema from a typed handler signature. Especially **compiler/macro-amenable**.
- **Streaming** — SSE parsing + partial token / tool-call-argument assembly.
- **Endpoint pool + retry / failover / health** — talk to N rozum instances, retry on
  failure, prefer healthy ones. This is where distribution/fault-tolerance lives.
- **Transcript / audit log** — recording the loop's steps + executed operations.
- **Prompt templating** — the *mechanism* (the *content* is domain).
- **(Optional) MCP-server framework** — exposing typed tools over MCP for external
  agents (the protocol is generic; the tools are domain).

**DOMAIN → belongs in busi, on top of the SDK (accounting-specific; would differ in
any other app):**
- **The actual tools** — `post_transaction`, `create_invoice`, `reconcile`,
  `lookup_account`, … : the operations + their validation rules.
- **System / activation prompt CONTENT** — chart of accounts, conventions, operating
  instructions, the user-facing command templates.
- **The eval set + success metric** — representative accounting flows.
- **Model choice / domain fine-tune** — picked by the eval; the QLoRA on busi traces.

Net: the scalascript team builds a generic **"agent SDK"** once; busi is a thin
accounting layer over it; the *next* scalascript app reuses the SDK unchanged.

## Specification for the scalascript side — the three contracts

This is what scalascript implements against. rozum provides the gateway (Contract 1)
+ this spec, and optionally a Rust reference implementation of Contracts 2–3 (an
*executable* spec that the scalascript SDK mirrors, and that powers the embedded mode).

### Contract 1 — Model call (rozum gateway API)

`POST /v1/chat/completions` (OpenAI form; `/v1/messages` Anthropic form is equivalent):
```jsonc
// request
{ "model": "<id>",
  "messages": [ {"role":"system","content":"…"}, {"role":"user","content":"…"} ],
  "tools": [ {"type":"function",
              "function":{"name":"…","description":"…","parameters": <JSON-Schema> }} ],
  "tool_choice": "auto",      // or force a specific tool
  "temperature": 0,           // determinism for reproducible eval
  "stream": true }
```
```jsonc
// response (non-stream): choices[0].message is EITHER
{ "content": "final text…" }                 // finish_reason "stop"
// OR
{ "tool_calls": [ {"id":"call_1","type":"function",
                   "function":{"name":"…","arguments":"<json string>"}} ] }  // finish_reason "tool_calls"
```
Streaming: SSE `data:` deltas (text deltas; tool-call argument deltas), terminated by
`[DONE]`. `finish_reason ∈ {stop, tool_calls, length}`.

### Contract 2 — Agent loop (the algorithm)

```
messages = [system(activation prompt), user(prompt)]
repeat (within budget):
    resp = POST model with (messages, tools)
    if resp.finish_reason == "tool_calls":
        append assistant(resp.tool_calls) to messages
        for call in resp.tool_calls:
            result = dispatch(call.name, parse(call.arguments))   # busi handler; validates
            append tool(tool_call_id=call.id, content=result_or_error) to messages
        continue
    else:                                  # final text
        return { text: resp.content, operations: executed, transcript: messages }
on model/transport error: retry on another rozum instance, capped backoff, up to N
budget: max_steps, max_tokens, wall_time   (then stop with a partial/abort result)
```

### Contract 3 — Tool

```
Tool {
    name        : String        // stable, unique
    description : String        // what it does + when to use it (the model reads this)
    schema      : JSONSchema     // parameters; STRICT — types, enums, required
    handler     : (args: Json) -> Result<Json, ToolError>
                  // validates, executes the domain op, returns a structured result
                  // OR a ToolError = a clear, actionable message the model can fix from
}
```
Design rules (these set the required model size — see below): **high-level, atomic,
deterministic** tools (push multi-step logic into the op, e.g. one
`post_transaction` that does the whole double entry); **strict schemas**; **clear
errors**; validation inside the handler.

### Rust reference runtime (implemented — `src/agent.rs`)

Contracts 2–3 have an executable Rust implementation that the scalascript SDK mirrors and
that powers the in-process embedded mode:

- **Contract 3** — `ToolSource` trait (`fn tools() -> Vec<ToolDef>`, `async fn dispatch(name,
  args) -> Result<Value, ToolError>`) with `CallbackToolSource`, the direct in-process
  adapter (register `(ToolDef, handler)` pairs; a `ToolError` is the recoverable message fed
  back to the model).
- **Contract 2** — `run_agent(backend, system, user, tools, budget) -> AgentOutcome`, the
  loop: `[system,user] → model → (tool calls → dispatch → append results)* → final text`,
  bounded by `Budget {max_steps, max_tokens, wall_time, temperature}` (temp 0 default for
  reproducible runs). `AgentOutcome` carries `{text, stop, steps, operations, transcript}` —
  the audit trail. It speaks only the `ChatBackend` SPI, so it runs against any backend.
- Validated model-free (scripted `MockBackend`: full tool loop with result feedback, budget
  cap, unknown-tool + handler-validation recovery) AND end-to-end against native MLX
  (`agent_loop_real_backend`: Qwen3-4B calls `add(3,5)` → `{sum:8}` → "The result of 3 + 5
  is 8.", with constrained decoding guaranteeing valid args).
- **Follow-up**: an MCP-client `ToolSource` adapter (over `rmcp`) so the runtime can use tools
  from an external MCP server; the trait is ready for it.

## What rozum provides (less than it first seemed)

With "busi is the agent", rozum's new work shrinks:
- **A rock-solid stateless gateway with tools** (mostly exists: `tool_calls`/`tool_use`
  + multi-turn history + SSE). Action: pin down + stabilize the Contract-1 surface.
- **Distributed readiness**: the gateway as a deployable service, health/readiness,
  horizontal scale (stateless), a model pool/router. Partly exists (shared-gateway
  daemon, `concurrency::admit_wrap`, the launch proxy's replay/retry).
- **The protocol spec** (Contracts 1–3) + an **optional Rust reference agent-runtime**
  that is *dual-purpose*: (a) the in-process embedded mode, and (b) the *executable
  spec* the scalascript SDK mirrors.

## What busi provides (domain only)

The accounting tools + their validation, the system/activation prompt content, the
eval set, and (later) the domain fine-tune. Everything generic is in the scalascript
SDK, not here.

## Required model complexity (summary)

The model needs **agentic tool-use competence, not accounting knowledge** (busi has
that). Size is set by flow depth, tool count/clarity, and how much the activation
prompts pre-structure the task — all controlled by busi/scalascript. Rough tiers:
simple single-step → 1.5–3B (tool-tuned, optional QLoRA); typical multi-step over
~10–30 well-shaped tools → **7–14B local (sweet spot)**; complex branching/recovery →
32B-local or a frontier model with escalation. **Tool-tuned beats bigger-but-generic.**
Don't guess — busi's eval harness picks the smallest model that clears the bar.

## Phased plan

- **P0a (scalascript SDK)** — the generic agent SDK: model client + agent loop + tool
  framework + schema derivation + streaming + endpoint pool/retry. (Lib or compiler —
  scalascript's call.) Validate against rozum's gateway with two fake tools.
- **P0b (rozum)** — stabilize the Contract-1 gateway surface + (optional) the Rust
  reference agent-runtime + distributed-readiness basics.
- **P1 (busi)** — design the accounting tool surface (atomic, strict schemas, good
  errors) + activation prompts + the eval harness; drive it with an off-the-shelf
  tool-capable model; find the capability ceiling.
- **P2** — eval-pick the smallest sufficient model + constrained/structured decoding
  (rozum side) for tool-arg reliability.
- **P3** — QLoRA a small model on busi traces → a fast, private, on-device busi model;
  route the rote 80% to it, escalate the hard 20% (busi validates either way).

## Why this shape

- The model layer (rozum) stays **stateless** → scales + fails over for free; the
  state stays in busi where the session lives.
- The **generic agent SDK in scalascript** is the same "push reusable infra into the
  durable layer" move as rozum's own extraction taxonomy — the *next* scalascript app
  reuses it; busi is a thin domain leaf.
- The hard part stays where it belongs: busi's **tool/MCP design and its eval** — not
  the model, and not bespoke plumbing.
