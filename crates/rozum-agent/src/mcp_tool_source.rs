//! MCP-client [`ToolSource`]: lets rozum's embedded agent loop consume tools served by an
//! external **stdio MCP server** (a child process). The MCP-*server* side (rozum exposing
//! meeting tools to agents) lives in `meeting/proxy.rs`; this is the *client* side.
//!
//! Spec: `docs/specs/mcp-toolsource.md`. arch-spi Stage 4 (the tool axis): an
//! [`McpToolSource`] composes with the in-process [`CallbackToolSource`] under
//! [`MultiToolSource`](crate::agent::MultiToolSource), so external and in-process tools share
//! the one `ToolSource` SPI.

use async_trait::async_trait;
use serde_json::{Map, Value};

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
};

use crate::agent::{ToolError, ToolSource};
use crate::backend::ToolDef;

/// Construction error for an [`McpToolSource`] — a failed spawn, MCP handshake, or tool listing.
/// (Per-*call* failures surface as [`ToolError`] so the model can recover; construction failures
/// are the caller's to handle — e.g. proceed without this source.)
#[derive(Debug, Clone)]
pub struct McpToolError(pub String);

impl std::fmt::Display for McpToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for McpToolError {}

/// A [`ToolSource`] backed by an external MCP server reached over an rmcp client connection.
/// Owns the [`RunningService`] (keeping the child process alive) and a snapshot of the server's
/// tools captured at connect (`ToolSource::tools` is synchronous, so the list cannot be fetched
/// lazily).
pub struct McpToolSource {
    service: RunningService<RoleClient, ()>,
    defs: Vec<ToolDef>,
}

impl McpToolSource {
    /// Spawn a stdio MCP server as a child process, MCP-initialize, and cache its tool list.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, McpToolError> {
        use rmcp::transport::TokioChildProcess;
        use tokio::process::Command;

        let mut cmd = Command::new(command);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        let transport = TokioChildProcess::new(cmd)
            .map_err(|e| McpToolError(format!("spawn MCP server `{command}`: {e}")))?;
        let service = ()
            .serve(transport)
            .await
            .map_err(|e| McpToolError(format!("MCP initialize `{command}`: {e}")))?;
        Self::from_service(service).await
    }

    /// Build from an already-initialized rmcp client service: list the server's tools and cache
    /// them as [`ToolDef`]s. Both [`spawn`](Self::spawn) and tests funnel through here (each runs
    /// `().serve(transport)` with its own concrete transport — a child process, or an in-memory
    /// duplex — so this core never has to name the transport's generic bound).
    pub async fn from_service(
        service: RunningService<RoleClient, ()>,
    ) -> Result<Self, McpToolError> {
        let tools = service
            .list_all_tools()
            .await
            .map_err(|e| McpToolError(format!("MCP list_tools: {e}")))?;
        let defs = tools
            .into_iter()
            .map(|t| ToolDef {
                name: t.name.to_string(),
                description: t.description.map(|d| d.to_string()).unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
            })
            .collect();
        Ok(Self { service, defs })
    }
}

#[async_trait]
impl ToolSource for McpToolSource {
    fn tools(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let arguments: Map<String, Value> = args.as_object().cloned().unwrap_or_default();
        let result: CallToolResult = self
            .service
            .peer()
            .call_tool(CallToolRequestParams::new(name.to_owned()).with_arguments(arguments))
            .await
            .map_err(|e| ToolError::new(format!("MCP call `{name}`: {e}")))?;
        if result.is_error.unwrap_or(false) {
            return Err(ToolError::new(call_result_text(&result)));
        }
        Ok(call_result_value(result))
    }
}

/// Flatten a `CallToolResult` to JSON: prefer the server's `structured_content`; otherwise the
/// concatenated text blocks (as a JSON string), so a plain-text tool still yields a usable value.
fn call_result_value(result: CallToolResult) -> Value {
    if let Some(structured) = result.structured_content {
        return structured;
    }
    Value::String(call_result_text(&result))
}

/// The text content of a `CallToolResult`, joined — used for both the success-text fallback and
/// the error message handed back to the model.
fn call_result_text(result: &CallToolResult) -> String {
    let mut out = String::new();
    for c in &result.content {
        if let Some(text) = c.as_text() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&text.text);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{CallbackToolSource, MultiToolSource};
    use rmcp::{
        ServerHandler,
        handler::server::{router::tool::ToolRouter, wrapper::Parameters},
        model::Content,
        tool, tool_handler, tool_router,
    };
    use serde_json::json;

    // ── A minimal in-process MCP server with one `echo` tool, to drive the client over an
    //    in-memory duplex (no external binary). ──
    #[derive(serde::Deserialize, schemars::JsonSchema)]
    struct EchoParams {
        message: String,
    }

    #[derive(Clone)]
    struct EchoServer {
        tool_router: ToolRouter<Self>,
    }

    #[tool_router(router = tool_router)]
    impl EchoServer {
        fn new() -> Self {
            Self { tool_router: Self::tool_router() }
        }

        #[tool(name = "echo", description = "Echo back the given message")]
        async fn echo(&self, params: Parameters<EchoParams>) -> CallToolResult {
            CallToolResult::success(vec![Content::text(params.0.message)])
        }

        #[tool(name = "boom", description = "Always reports a tool error")]
        async fn boom(&self) -> CallToolResult {
            CallToolResult::error(vec![Content::text("kaboom")])
        }
    }

    #[tool_handler(router = self.tool_router)]
    impl ServerHandler for EchoServer {}

    /// Connect an `McpToolSource` to the in-process echo server over a duplex pipe.
    async fn connect_echo() -> McpToolSource {
        let (client_t, server_t) = tokio::io::duplex(8192);
        tokio::spawn(async move {
            if let Ok(service) = EchoServer::new().serve(server_t).await {
                let _ = service.waiting().await;
            }
        });
        let service = ().serve(client_t).await.expect("client initialize");
        McpToolSource::from_service(service).await.expect("list tools")
    }

    #[tokio::test]
    async fn lists_and_calls_the_server_tool() {
        let src = connect_echo().await;

        // tools() exposes the server's tools as ToolDefs with name + schema.
        let tools = src.tools();
        let echo = tools.iter().find(|t| t.name == "echo").expect("echo tool present");
        assert!(echo.input_schema.is_object(), "schema: {:?}", echo.input_schema);

        // dispatch routes to call_tool and returns the result.
        let out = src.dispatch("echo", json!({ "message": "ping" })).await.unwrap();
        assert!(out.to_string().contains("ping"), "out: {out}");
    }

    #[tokio::test]
    async fn unknown_tool_is_a_recoverable_tool_error() {
        let src = connect_echo().await;
        // A name the server doesn't have → Err(ToolError), never a panic.
        let err = src.dispatch("nope", json!({})).await;
        assert!(err.is_err(), "expected ToolError, got {err:?}");
    }

    #[tokio::test]
    async fn tool_reported_error_maps_to_tool_error_with_message() {
        let src = connect_echo().await;
        // A tool that returns `is_error: true` → Err(ToolError) carrying the server's message
        // (recoverable — fed back to the model), not a panic or a success value.
        let err = src.dispatch("boom", json!({})).await;
        match err {
            Err(e) => assert!(e.to_string().contains("kaboom"), "msg: {e}"),
            Ok(v) => panic!("expected ToolError, got Ok({v})"),
        }
    }

    #[tokio::test]
    async fn composes_with_in_process_tools_under_multitoolsource() {
        let mcp = connect_echo().await;
        let local = CallbackToolSource::new().with_tool(
            ToolDef {
                name: "local_add".into(),
                description: "add one".into(),
                input_schema: json!({ "type": "object" }),
            },
            |_args| Ok(json!({ "ok": true })),
        );
        let multi = MultiToolSource::new().with(mcp).with(local);

        let names: Vec<String> = multi.tools().into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"echo".to_string()), "names: {names:?}");
        assert!(names.contains(&"local_add".to_string()), "names: {names:?}");

        // calls route to the owning source
        let a = multi.dispatch("echo", json!({ "message": "x" })).await.unwrap();
        assert!(a.to_string().contains('x'));
        let b = multi.dispatch("local_add", json!({})).await.unwrap();
        assert_eq!(b, json!({ "ok": true }));
    }
}
