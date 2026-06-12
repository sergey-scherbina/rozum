# scalascript agent SDK — design spec

**Status: design / plan.** The **generic, domain-agnostic** layer that lets any
scalascript application embed a tool-using LLM agent. It is the middle tier of the
busi integration (`busi-integration-and-agent-runtime.md`): below it sits **rozum**
(a stateless model service speaking the OpenAI/Anthropic API), above it sits the
**app** (busi — accounting tools + prompts). **busi-specific = nothing here**; the
*next* scalascript app reuses this SDK unchanged.

Implementation lives on the scalascript side — as a library, or partly in the
compiler where type-derivation/codegen helps (the SDK's call). This spec is the
design + the public API contract, not scalascript syntax.

## Purpose & boundaries

The SDK turns a **stateless model** ("given messages + tools, return text *or* tool
calls") into an **agent** ("given a prompt + tools, drive to a result"), and hides
all the plumbing (transport, streaming, retries, schema wiring) behind a small typed
surface. It does **three** things and nothing else:

1. **Talks to the model** over the gateway API (Contract 1) — HTTP/SSE or in-process.
2. **Runs the agent loop** (Contract 2) — message assembly, tool dispatch, budget,
   error/retry, stop.
3. **Frames tools** (Contract 3) — declare a tool, derive its schema, dispatch calls
   to the app's handler, format results/errors.

(The three contracts are defined in `busi-integration-and-agent-runtime.md`; this spec
is their realized design.)

It explicitly does **not**: own session/business state (the app does), execute side
effects (the app's tool handlers do), or know any domain.

## Position in the stack

```
app (busi)        defines Tools (handler+schema) + prompts + config; calls run()/stream()
   │  uses
scalascript SDK   ModelClient · AgentLoop · ToolRegistry · SchemaDerivation ·
   │  consumes    EndpointPool/retry · Transcript · (opt) McpServer
rozum gateway     stateless: POST /v1/chat/completions (tools, SSE) → tool_calls|text
   │  below SPI
local MLX / etc.  the model
```

## Public API (what the app codes against)

Language-neutral signatures; scalascript renders them idiomatically.

```
// ── Configuration ───────────────────────────────────────────────────────────
AgentClient(config: {
    endpoints : [Endpoint]          // one or many rozum instances, or an embedded backend
    model     : String              // model id
    defaults  : RunOptions          // temperature=0, budgets, retry policy
})

Endpoint = Http(url: Url, auth?: Token) | Embedded(backend)   // transport-abstracted

// ── Tools (the app provides these; the SDK frames them) ─────────────────────
Tool(
    name        : String,
    description : String,           // what it does + when to use it (model reads this)
    schema      : JsonSchema,       // STRICT params; may be DERIVED from the handler's type
    handler     : (args: Json) -> Result<Json, ToolError>   // app code; validates + executes
)
ToolError(message: String, retryable: Bool = true)   // a message the model can act on

// ── Run (one agent turn) ────────────────────────────────────────────────────
client.run(
    system : String,                // activation/system prompt (app builds it)
    user   : String,                // the user prompt
    tools  : [Tool],
    options: RunOptions = defaults,
    resume?: Transcript             // continue a prior session
) -> AgentResult

client.stream(...) -> Stream<AgentEvent>   // same, but yields events for UI

// ── Results & events ────────────────────────────────────────────────────────
RunOptions {
    temperature : Float = 0
    maxSteps    : Int                // tool-call rounds before abort
    maxTokens   : Int
    wallTime    : Duration
    toolChoice  : Auto | Required | None
    retry       : RetryPolicy        // attempts, backoff, across endpoints
}
AgentResult {
    text       : String              // final assistant text
    operations : [ExecutedOp]        // {tool, args, result|error} in order — for audit/apply
    transcript : Transcript          // full message list (resume-able)
    stop       : Stop                // Done | MaxSteps | MaxTokens | WallTime | Cancelled | Error
}
AgentEvent = TextDelta(String)
           | ToolCallStarted(id, name, argsPartial)
           | ToolCallResult(id, name, result|error)
           | Stopped(Stop)
           | Errored(Error)
```

That is the whole surface an app needs: configure, declare tools, `run`/`stream`,
read `AgentResult`.

## Components & responsibilities (SDK internals)

- **ModelClient** — implements Contract 1. Serializes `messages + tools` into the
  gateway request, parses the response into *either* `text` *or* `tool_calls`, handles
  SSE streaming (text deltas + tool-call argument deltas), surfaces `finish_reason`.
  Transport-abstracted: HTTP for distributed, an in-process call for `Embedded`.
- **AgentLoop** — implements Contract 2 (below). The orchestrator.
- **ToolRegistry** — holds the app's `Tool`s; serializes their schemas into requests;
  dispatches an incoming `tool_call` to the matching handler; turns the handler's
  `Result` into a `tool` message (success JSON, or the `ToolError` message). Enforces:
  unknown tool → structured error back to the model (not a crash).
- **SchemaDerivation** — produce a tool's `JsonSchema` from its typed handler
  signature (enums, required, ranges). **Compiler/macro-amenable**; explicit schemas
  are the fallback. Strict schemas are what let small models not mis-fill args.
- **EndpointPool + retry** — round-robin / least-loaded across rozum instances, health
  awareness, retry a failed model call on another instance with capped backoff. This
  is where **distribution & fault-tolerance** live (the app never deals with it).
- **Transcript** — the ordered message list + executed operations; resume-able and
  the audit record.
- **(Optional) McpServer** — expose the same typed tools over MCP so an *external*
  agent can drive the app too (one tool definition, two consumers).

## The agent loop (Contract 2, realized)

```
messages = resume ?? [ system(systemPrompt), user(userPrompt) ]
loop while within budget(options):
    resp = modelClient.call(messages, tools=registry.schemas(), options)   // pool+retry inside
    if resp.kind == tool_calls:
        messages += assistant(resp.tool_calls)
        for call in resp.tool_calls:
            out = registry.dispatch(call.name, parse(call.arguments))       // app handler; validates
            messages += tool(call.id, out.jsonOrErrorMessage)
            emit ToolCallResult; record ExecutedOp
        continue
    else:                                   // text → final
        return AgentResult(resp.text, ops, messages, Done)
budget exhausted → return AgentResult(lastText, ops, messages, MaxSteps|MaxTokens|WallTime)
cancelled → return …(Cancelled)
```

Notes: the model never leaves the loop; **every operation is the app's handler,
which validates** (a rejected op comes back as a `ToolError` the model corrects).
One stop token may still emit a trailing tool round that's discarded — harmless.

## Transport (Contract 1 consumption)

- **Distributed (default):** `POST /v1/chat/completions` (or `/v1/messages`) to a
  rozum instance; SSE for streaming. JSON only — works from any scalascript HTTP
  client. The `EndpointPool` picks the instance + retries.
- **Embedded (optional):** `Endpoint::Embedded(backend)` calls an in-process rozum
  backend directly (Rust component + small model, no network). The loop/tools are
  identical; only `ModelClient`'s transport differs. (This is why the loop is
  transport-abstracted.)

## Determinism, observability, safety

- **Determinism:** `temperature = 0` by default; the full `Transcript` is recorded →
  reproducible eval (the busi eval harness replays prompts and checks operations).
- **Observability:** token counts, per-step latency, tool-call success rate, the
  transcript. Enough to debug and to build the eval/tune signal.
- **Safety boundary (important):** the SDK **never** performs a side effect. It only
  *calls the app's handler*. All mutation, validation, permissions, and the
  source-of-truth live in the app's handlers. A compromised/hallucinating model can at
  most *request* an operation; the handler validates and can reject it. This is the
  core guarantee that makes a small local model safe to drive accounting.

## Error taxonomy

- **Transport/model error** (timeout, 5xx, instance down) → `EndpointPool` retries on
  another instance with backoff; exhausted → `AgentResult.stop = Error`.
- **Tool error** (handler returns `ToolError`, or args fail validation) → fed back to
  the model as a `tool` message; the model corrects and retries. NOT fatal.
- **Unknown tool / malformed arguments** → structured error message back to the model
  (never a crash).
- **Budget exhausted** → graceful stop with a partial result + the reason.

## Non-goals (for the SDK)

- No domain logic, no business validation (the app's handlers own that).
- No session/business persistence (the app owns state; the SDK is stateless between
  `run`s, save the resume-able transcript).
- No model serving / no inference (that's rozum below the SPI).
- No bespoke wire protocol — OpenAI/Anthropic over HTTP+SSE is the contract.

## Testing / conformance

- **Mock gateway** — a fake Contract-1 endpoint scripted to return canned
  text/tool_calls → test the loop, tool dispatch, budget, retry without a real model.
- **Fake tools** — handlers returning fixed results/errors → assert the loop calls the
  right tool, feeds results back, and finishes.
- **Golden transcripts** — record a few real runs; assert structure (not exact model
  text) stays stable across SDK changes.
- **Conformance against rozum** — a small live suite hitting a real rozum gateway to
  pin the Contract-1 assumptions.

## Phased build

- **P0** — ModelClient (HTTP+SSE) + AgentLoop + ToolRegistry over explicit schemas +
  `run()`; validate against rozum's gateway with two fake tools.
- **P1** — `stream()` + AgentEvents; EndpointPool + retry/failover; Transcript/audit.
- **P2** — SchemaDerivation from typed handlers (compiler/macro); resume-able sessions.
- **P3** — (optional) Embedded transport; (optional) McpServer for external agents.

## Relationship to rozum & busi

- **rozum** provides Contract 1 (the gateway), an optional **Rust reference
  agent-runtime** as the *executable twin* of this SDK (same Contracts 2–3, for the
  embedded mode), and `structured-output` so the SDK can pass JSON schemas and get
  schema-valid tool args. See `busi-integration-and-agent-runtime.md` + BACKLOG
  ("Agent integration (busi)").
- **busi** is a thin domain layer: it provides `Tool`s (handlers + schemas), the
  system/activation prompts, the eval set, and the model choice — nothing generic.
