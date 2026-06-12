# busi integration & the rozum agent runtime

**Status: design / plan.** How an application (the first being **busi**, an
accounting app that compiles Scala→Rust and exposes an **MCP** interface) embeds a
local model through rozum to drive its own tools. The headline new capability is a
**headless, embeddable agent runtime** — reusable far beyond busi.

Companion reading: `portability-and-the-backend-spi.md` (the SPI is the durable
boundary; this runtime sits *above* it), `training-and-lora-exploration.md` (the
optional fine-tune step), `mlx-native-runtime.md` (the local model serving below).

## The one architectural fact that decides everything

rozum today is a **stateless** model gateway: `/v1/messages` takes
`messages + tools` and returns *either* text *or* a `tool_use` — and the
**agentic loop** (model → tool call → execute → tool result → model → …) is the
*client's* job. Today that client is Claude Code (external). For an embedded app
like busi, that loop must exist **headless and embeddable**.

**Mental model:**

> **rozum agent runtime** = a headless, embeddable "Claude Code" that runs a
> **local** model and drives an app's tools to completion.
> **busi** = an **MCP server** (the tools + the accounting rules + validation) plus
> the *activation prompts*.
> **rozum** = the **MCP host** (model + loop + tool orchestration).

The app owns the *truth* (its tools validate everything; the model never invents
accounting). rozum owns the *loop* and the *local private brain*.

What already exists and is reused: `rmcp` MCP client/server plumbing (used for
meeting rooms), tool-use parsing into `ToolUse` events, multi-turn tool-history
rendering, the `ChatBackend` SPI + local MLX serving, concurrency/admission. **The
new part is the orchestration loop + a stable embed API.**

## Two integration modes (the Scala→Rust advantage)

Because busi compiles to **Rust** and rozum **is** Rust, the tightest mode is
available:

- **Mode A — in-process Rust crate (recommended for desktop / embedded).** busi's
  Rust output depends on the `rozum-agent` crate; the loop runs in busi's process;
  the model lives in busi's process (the `!Send` worker thread); tools execute via a
  direct callback (or busi's in-process MCP). **No HTTP, no subprocess, fully
  private, lowest latency.** The natural fit for busi.
- **Mode B — HTTP + MCP (for server / shared model).** busi runs its MCP server;
  rozum runs as a daemon with the agent runtime; busi calls a `/v1/agent` endpoint
  (prompt + which MCP server to drive). For a shared resident model across clients.

Both sit behind the same agent-runtime abstraction. Start with A; add B when a
shared daemon is needed.

## End-to-end data flow (one request)

```
user prompt (busi UI)
  → busi builds (system context + user prompt + tool set)
  → rozum-agent loop:
        model call (with tools)
        → tool_use?  → busi executes the tool (VALIDATES) → tool_result → ↺
        → final text → stop
  → busi shows / applies the result
```

The model never leaves the loop; **busi validates every operation**. A rejected
operation comes back as a tool error the model corrects. No hallucinated entries
can be committed.

## What rozum must build

1. **`rozum-agent` — the headless agent runtime (the core new piece).**
   - Input: `(backend, system_prompt, user_prompt, tool_source, budget)` where
     `budget` caps steps / tokens / wall-time.
   - Loop: call the model with the tool set → parse `tool_use` → execute via the
     tool source → feed the `tool_result` back (reusing the multi-turn tool-history
     rendering) → repeat until a final answer, the step budget, or a stop signal.
   - Output: final response **+ transcript + the list of operations performed**
     (so busi can show/audit what happened).
   - This is Claude Code's loop minus the UI, as a library. Streaming + cancellation
     ride the existing `ChatEvent` stream.
2. **Tool-source adapters** (how the loop reaches the app's tools):
   - **MCP client** — connect to the app's MCP server, auto-discover tools (reuses
     `rmcp` + the room MCP-client plumbing), execute tool calls over MCP. Decoupled;
     the app just runs its MCP server.
   - **Direct callback** — `Fn(ToolCall) -> ToolResult` for in-process Mode A (no
     MCP serialization). The app passes a closure that calls its own logic.
   - Both expose the same `ToolSource` trait to the loop.
3. **`rozum-embed` — a stable, minimal, versioned public crate.** The only surface
   an embedder links: build a backend (local MLX or HTTP), construct the agent
   runtime, pick a tool source. Keeps busi off rozum's internals so rozum can evolve.
4. **(Optional, high value) constrained / structured decoding.** Enforce the model's
   tool-argument output against the app's JSON tool schemas *during decoding* →
   reliability for small local models (they can't emit an invalid arg). This is the
   backlog `structured-output` item, now driven by a concrete consumer.
5. **(Optional) `/v1/agent` HTTP endpoint** for Mode B (prompt + MCP-server pointer
   → runs the loop → returns result + transcript).
6. **Model lifecycle for embedding** — load/unload, the `!Send` worker, concurrency:
   mostly exists; expose it cleanly through `rozum-embed`.

## What busi / scalascript must do

1. **MCP tool design — the most important lever (it sets the required model size).**
   High-level, **atomic, deterministic** operations (push multi-step logic into
   busi, e.g. `post_transaction(...)` that does the whole double entry, not three
   low-level calls); **strict schemas** (types, enums, required fields); **clear,
   actionable error messages**; validation *inside* each tool. The cleaner and
   higher-level the MCP surface, the smaller/cheaper/more-local the model can be.
2. **The Rust glue.** Since busi compiles to Rust, it needs a thin Rust layer that
   links `rozum-agent`, wires busi's tools (callback or its MCP server) and the model
   choice. **scalascript must be able to express a crate dependency / FFI** to this
   layer (the integration seam on the busi side).
3. **Activation prompts / templates.** The system prompt (busi context: chart of
   accounts, conventions, the operating instructions) + user-facing triggers
   (slash-commands → structured prompts) that pre-shape tasks and lower the required
   model intelligence.
4. **An eval harness.** 20–50 representative real flows + a *task-success* metric
   (did the model produce the correct busi operations end-to-end). This — not a
   guess — picks the model size and is the fine-tune signal. (Same lesson as the
   training spec: the limiting reagents are *data + eval*, not the model.)
5. **Model bundling / serving choice.** Ship-with-model (local MLX via rozum, data
   never leaves the machine — the killer feature for accounting) vs point at a shared
   rozum daemon.
6. **(Later) the fine-tune.** Once flows + eval exist, QLoRA a small model on the
   collected `(prompt → tool-call)` traces → a fast, private, on-device busi model
   (the `tune-toolcall-format` / domain-tune backlog pattern).

## Required model complexity (summary; full reasoning in chat history)

The model needs **agentic tool-use competence, not accounting knowledge** (busi has
that). Required size is set by: how multi-step/branching the flows are, how many/how
clear the tools are, and how much the activation prompts pre-structure the task —
all of which busi controls. Rough tiers: simple single-step flows → 1.5–3B
(tool-tuned, optionally QLoRA); typical multi-step over ~10–30 well-shaped tools →
**7–14B local (the sweet spot)**; complex branching/recovery/long-context →
32B-local or a frontier model with escalation. **Tool-tuned beats bigger-but-generic.**
Don't guess — the busi eval harness picks the smallest model that clears the bar.

## Phased plan

- **P0 (rozum)** — `rozum-agent` runtime + the `ToolSource` trait + the MCP-client
  and callback adapters + the `rozum-embed` public crate. Validate against a toy MCP
  server (a couple of fake tools, assert the loop calls them and finishes).
- **P1 (busi + rozum)** — busi designs its MCP tool surface (atomic, strict schemas,
  good errors) + the Rust glue; drive busi's real tools with an off-the-shelf
  tool-capable model; build the eval harness. Find the capability ceiling.
- **P2** — minimize the model (eval-pick the smallest that passes) + add
  constrained/structured decoding for tool-arg reliability.
- **P3** — QLoRA a small model on busi traces → a local, private, fast busi model;
  route the rote 80% to it, escalate the hard 20% (busi validates either way).

## Why this is the right shape

The new capability — **a headless, embeddable agent runtime (MCP host with a local
model)** — is *not* a busi-specific hack. It lives **above the `ChatBackend` SPI**,
is engine/hardware-agnostic (the portability taxonomy's durable layer), and lets
**any** Rust app with its own MCP surface embed a local private agent. busi is its
first consumer; the runtime is a general rozum layer. And the hard part stays where
it belongs: the app's tool/MCP design and its eval — not the model.
