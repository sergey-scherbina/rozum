# MCP `ToolSource` adapter

## Overview

An MCP-**client** implementation of the [`ToolSource`] trait (`src/agent.rs`) so rozum's
embedded agent loop can consume tools served by any external **stdio MCP server** (a child
process — the same shape agents like Claude Code / Codex configure: `command` + `args` +
`env`). It closes the "(follow-up) an MCP-client adapter" note in `agent.rs` and is Stage 4 of
`docs/specs/architecture-spi.md` (the tool axis): external MCP tools and in-process
`CallbackToolSource` then compose behind the one tool SPI via the existing `MultiToolSource`.

This is the **tool-source** side (rozum as an MCP *client*), distinct from rozum's MCP
*server* (`meeting/proxy.rs`, which exposes meeting tools to agents).

## Interface

```rust
pub struct McpToolSource { /* RunningService client + cached ToolDefs */ }

impl McpToolSource {
    /// Spawn a stdio MCP server as a child process, MCP-initialize, and cache its tool list.
    pub async fn spawn(command: &str, args: &[String], env: &[(String, String)])
        -> Result<Self, McpToolError>;

    /// Core: initialize over any rmcp client transport (used by `spawn`; lets tests drive an
    /// in-memory duplex against a minimal in-process server).
    pub async fn connect<T>(transport: T) -> Result<Self, McpToolError> where T: /* IntoTransport<RoleClient> */;
}

#[async_trait]
impl ToolSource for McpToolSource {
    fn tools(&self) -> Vec<ToolDef>;                                      // the cached list
    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError>;
}
```

- **`tools()` is synchronous** (the trait requires it), so the tool list is fetched **once at
  connect** (`list_all_tools`) and cached. Each MCP `Tool` maps to `ToolDef { name,
  description, input_schema }` (MCP `inputSchema` → `input_schema`).
- **`dispatch`** issues `call_tool(CallToolRequestParams::new(name).with_arguments(args))`. A
  normal `CallToolResult` → `Ok(Value)` (its `content` flattened to JSON); a result with
  `is_error: true` → `Err(ToolError)` (recoverable — the message goes back to the model);
  transport / protocol failure → `Err(ToolError)` too (the agent loop only sees `ToolError`).
- **Lifetime**: the value owns the `RunningService` (keeps the child alive); dropping it shuts
  the server down. `McpToolError` is the construction error (spawn / handshake); per-call
  failures surface as `ToolError`.

## Behavior

- [ ] `spawn` launches the child, completes the MCP `initialize` handshake, and caches the
  server's tools; a bad command or failed handshake returns `Err(McpToolError)`.
- [ ] `tools()` returns every server tool as a `ToolDef` with name, description, and the MCP
  `inputSchema` as `input_schema`.
- [ ] `dispatch(name, args)` calls the server and returns its structured content as `Ok(Value)`.
- [ ] A tool that reports an error (`is_error`) yields `Err(ToolError)` with the server's
  message (recoverable — fed back to the model), not a panic or a construction error.
- [ ] An unknown tool name / transport failure yields `Err(ToolError)`, never a panic.
- [ ] An `McpToolSource` composes with an in-process `CallbackToolSource` under
  `MultiToolSource` (the union exposes both tool sets; calls route to the owner).
- [ ] Tested without an external binary: an in-memory duplex transport drives `connect`
  against a minimal in-process rmcp server exposing one tool (`echo`).

## Out of scope

- **Wiring into a CLI / gateway flow.** This is the `ToolSource` *impl*; surfacing it (a config
  of MCP servers for an embedded-agent command) is a separate, later step.
- **Non-stdio transports** (HTTP/SSE MCP servers). Stdio child-process only for v1 — it is the
  dominant local-MCP shape and the one rmcp `transport-child-process` already gives us.
- **Dynamic tool-list refresh** (`tools/list_changed`). Tools are cached at connect; a server
  that mutates its tool set mid-session is a later concern.
- **Secret management.** Env vars are passed through as given; no vaulting.
- The gateway/launch path (external agents bring their own tools) — unaffected; this adds reach
  only to the *embedded* agent loop.

## Design

- **rmcp 1.7 client, not hand-rolled JSON-RPC.** The crate is already a dependency with the
  `client` + `transport-child-process` features; `meeting/proxy.rs` already uses its client
  (`serve` / `peer` / `call_tool(CallToolRequestParams…)`). Reuse it. (`room_client.rs`'s
  hand-written JSON-RPC is socket-specific to the meeting room; not reused here.)
- **`connect<T>(transport)` core + `spawn(command,…)` convenience.** `spawn` builds a
  `TokioChildProcess` transport from a `tokio::process::Command`; `connect` runs the rmcp
  client handshake (`().serve(transport)`), takes `peer()`, calls `list_all_tools()`, maps to
  `ToolDef`, and stores the `RunningService` + the cached defs. Splitting out `connect<T>` makes
  it testable over an in-memory duplex without spawning a process.
- **Mapping.** `Tool { name, description, input_schema } → ToolDef`. `CallToolResult` →
  `Value`: prefer structured content; otherwise collect text `Content` into a JSON value. Map
  `is_error` and any rmcp `Err` to `ToolError`.

## Decisions

- **rmcp over hand-rolled.** Proper protocol coverage (initialize, capabilities, pagination),
  already vendored and used. Rejected: extending `room_client.rs`'s socket JSON-RPC (it is
  unix-socket + line-delimited, wrong transport, and would re-implement what rmcp gives).
- **stdio child-process first.** Matches how local MCP servers ship and how agents configure
  them; `transport-child-process` is already enabled. HTTP/SSE deferred.
- **Cache tools at connect.** `ToolSource::tools()` is sync; the only correct place to do the
  async `list_tools` is construction. Trade-off: no live refresh (out of scope, above).
- **Errors split by phase.** Construction failures (`spawn`/handshake) are `McpToolError` (the
  caller decides whether to proceed without this source); per-call failures are `ToolError`
  (the model sees them and can recover) — never panic across the FFI/child boundary.

## Results

<!-- Fill after implementation: tests, the in-memory duplex harness, any rmcp API specifics. -->
_Pending — spec gate._
