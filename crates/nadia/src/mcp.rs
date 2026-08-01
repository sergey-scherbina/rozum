//! MCP servers as extra tools — the client direction of `nadia:SPEC.md` §2.1.
//!
//! The bar for a seventh built-in tool stays where it is. This is the other answer to "I need one
//! more": tools nadia does not define, does not ship and is not responsible for, connected by the
//! operator for a run. The transport, the handshake and the call plumbing already exist in
//! `rozum_agent::mcp_tool_source::McpToolSource` (rozum owns the loop and its SPI); what lives
//! here is what an application owns — where the config is, which servers are connected, what the
//! tools are called, and what happens when a server is broken.
//!
//! Three rules from the spec are load-bearing and are implemented here rather than assumed:
//!
//! - **Opt-in per run.** A config file that merely exists adds nothing. Six tools already cost
//!   ~1.5–2k schema tokens per request and one server can add a dozen more; for a 4B model each
//!   one dilutes selection, so the operator decides when to pay, not the filesystem.
//! - **A named server that will not start is a hard error before the loop begins.** A run that
//!   silently lost half its tools produces a confidently wrong answer.
//! - **The jail does not extend to a server.** It is a separate process with its own access to
//!   the machine; the path jail and the seatbelt profile confine nadia, not it. Startup says so.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use rozum_agent::agent::{ToolError, ToolSource};
use rozum_core::backend::ToolDef;
use rozum_agent::mcp_tool_source::McpToolSource;

/// The ecosystem's `mcpServers` object, as Claude Code and the rest already write it — so an
/// operator's existing file works unchanged and nobody has to learn a nadia-shaped config.
#[derive(Debug, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers")]
    pub servers: BTreeMap<String, ServerSpec>,
}

/// One server entry. `command` is what makes it a stdio server; an entry carrying `url`/`type`
/// instead is a transport we do not speak, and [`ServerSpec::stdio_command`] refuses it BY NAME —
/// an operator who configured an HTTP server and saw no error would conclude it was connected.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ServerSpec {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Present on http/sse entries. Only ever used to explain the refusal.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
}

impl ServerSpec {
    /// The command to spawn, or why this entry cannot be one.
    pub fn stdio_command(&self, name: &str) -> Result<&str, String> {
        match self.command.as_deref() {
            Some(c) if !c.trim().is_empty() => Ok(c),
            _ => {
                let what = self
                    .url
                    .as_deref()
                    .map(|u| format!("a url ({u})"))
                    .or_else(|| self.kind.as_deref().map(|t| format!("type `{t}`")))
                    .unwrap_or_else(|| "no `command`".into());
                Err(format!(
                    "MCP server `{name}` has {what}: nadia speaks the stdio transport only, so \
                     this server cannot be connected. Give it a `command` that starts the server \
                     on stdio, or drop it from --mcp."
                ))
            }
        }
    }
}

/// Where a config is looked for, in order: an explicit path, the workspace's own `.mcp.json`,
/// then the user's. Returns the first that exists, so a project can override the user's default.
pub fn config_path(explicit: Option<&Path>, workspace: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let workspace_cfg = workspace.join(".mcp.json");
    if workspace_cfg.is_file() {
        return Some(workspace_cfg);
    }
    let user = std::env::var_os("HOME")
        .map(PathBuf::from)?
        .join(".config")
        .join("nadia")
        .join("mcp.json");
    user.is_file().then_some(user)
}

/// Read + parse a config. A path the caller ASKED for that is missing or malformed is an error,
/// never an empty config: `--mcp-config typo.json` must not look like "no servers configured".
pub fn load_config(path: &Path) -> Result<McpConfig, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("MCP config {}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("MCP config {}: {e}", path.display()))
}

/// The prefix an MCP tool carries into the model's tool list: `mcp__<server>__<tool>`, the
/// ecosystem's convention. It makes a collision with the six built-ins impossible and keeps two
/// servers exporting the same tool name apart. The un-prefixed name is what goes back on the wire.
pub fn prefixed(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// One connected server: its tools, renamed for the model, dispatching back under their real
/// names. A [`ToolSource`] like any other, so it composes with nadia's six under `MultiToolSource`
/// and passes through the same loop breaker and the same approval gate.
pub struct McpServer {
    name: String,
    inner: McpToolSource,
    defs: Vec<ToolDef>,
}

impl McpServer {
    /// Spawn + MCP-initialize `spec`, then cache its tools under the `mcp__<server>__` prefix.
    pub async fn connect(name: &str, spec: &ServerSpec) -> Result<Self, String> {
        let command = spec.stdio_command(name)?;
        let env: Vec<(String, String)> =
            spec.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let inner = McpToolSource::spawn(command, &spec.args, &env)
            .await
            .map_err(|e| format!("MCP server `{name}`: {e}"))?;
        let defs = inner
            .tools()
            .into_iter()
            .map(|t| ToolDef {
                name: prefixed(name, &t.name),
                description: t.description,
                input_schema: t.input_schema,
            })
            .collect();
        Ok(Self { name: name.to_string(), inner, defs })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The tool names this server contributed, as the model sees them.
    pub fn tool_names(&self) -> Vec<String> {
        self.defs.iter().map(|d| d.name.clone()).collect()
    }
}

#[async_trait]
impl ToolSource for McpServer {
    fn tools(&self) -> Vec<ToolDef> {
        self.defs.clone()
    }

    async fn dispatch(&self, name: &str, args: Value) -> Result<Value, ToolError> {
        let prefix = format!("mcp__{}__", self.name);
        let real = name.strip_prefix(&prefix).ok_or_else(|| {
            // Reachable only if the router hands us a name we never advertised.
            ToolError::new(format!("`{name}` is not a tool of MCP server `{}`", self.name))
        })?;
        self.inner.dispatch(real, args).await
    }
}

/// Is this the name of an MCP tool rather than one of the six? The approval gate asks, because a
/// server is an arbitrary program and its calls are gated exactly like `bash` — treating its tools
/// as safer because they have tidy names would be backwards.
pub fn is_mcp_tool(name: &str) -> bool {
    name.starts_with("mcp__")
}

/// Which servers to connect, from `--mcp NAME…` / `--mcp-all` against the config. Selecting a
/// name that is not in the config is an error naming what IS there: a typo must not read as
/// "that server contributed no tools".
pub fn select<'a>(
    cfg: &'a McpConfig,
    wanted: &[String],
    all: bool,
) -> Result<Vec<(String, &'a ServerSpec)>, String> {
    if all {
        return Ok(cfg.servers.iter().map(|(k, v)| (k.clone(), v)).collect());
    }
    let mut out = Vec::new();
    for name in wanted {
        match cfg.servers.get(name) {
            Some(spec) => out.push((name.clone(), spec)),
            None => {
                let known: Vec<&str> = cfg.servers.keys().map(String::as_str).collect();
                return Err(if known.is_empty() {
                    format!("no MCP server `{name}` — the config has none configured")
                } else {
                    format!("no MCP server `{name}` — the config has: {}", known.join(" "))
                });
            }
        }
    }
    Ok(out)
}

/// The line printed once per connected server. It names the tools AND that they act outside the
/// workspace jail: an operator whose mental model is "nadia is confined" is otherwise quietly
/// wrong, because a server is a separate process with its own access to the machine.
pub fn connected_line(server: &McpServer) -> String {
    format!(
        "mcp `{}`: {} tool(s) — {} · runs OUTSIDE the workspace jail, gated like bash",
        server.name(),
        server.tools().len(),
        server.tool_names().join(" ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_from(json: &str) -> McpConfig {
        serde_json::from_str(json).expect("parse")
    }

    #[test]
    fn reads_the_ecosystem_config_shape() {
        let cfg = cfg_from(
            r#"{"mcpServers":{"rozum":{"command":"rozum","args":["mcp-proxy"],"env":{"A":"b"}}}}"#,
        );
        let s = cfg.servers.get("rozum").expect("server");
        assert_eq!(s.stdio_command("rozum").unwrap(), "rozum");
        assert_eq!(s.args, vec!["mcp-proxy".to_string()]);
        assert_eq!(s.env.get("A").map(String::as_str), Some("b"));
    }

    #[test]
    fn an_http_entry_is_refused_by_name_not_skipped() {
        let cfg = cfg_from(r#"{"mcpServers":{"remote":{"url":"https://example.com/mcp"}}}"#);
        let err = cfg.servers["remote"].stdio_command("remote").unwrap_err();
        assert!(err.contains("remote"), "must name the server: {err}");
        assert!(err.contains("stdio"), "must say what IS supported: {err}");
        assert!(err.contains("https://example.com/mcp"), "must quote what it found: {err}");
    }

    #[test]
    fn selecting_an_unknown_server_lists_the_real_ones() {
        let cfg = cfg_from(r#"{"mcpServers":{"rozum":{"command":"rozum"},"fs":{"command":"x"}}}"#);
        assert_eq!(select(&cfg, &["rozum".into()], false).unwrap().len(), 1);
        assert_eq!(select(&cfg, &[], true).unwrap().len(), 2);
        let err = select(&cfg, &["rozom".into()], false).unwrap_err();
        assert!(err.contains("rozom") && err.contains("rozum") && err.contains("fs"), "{err}");
        // Nothing configured at all says that, rather than listing an empty set.
        let empty = cfg_from("{}");
        assert!(select(&empty, &["x".into()], false).unwrap_err().contains("none configured"));
    }

    #[test]
    fn names_are_prefixed_so_the_six_can_never_be_shadowed() {
        assert_eq!(prefixed("rozum", "meeting.submit"), "mcp__rozum__meeting.submit");
        // The built-ins are exactly the names a server must not be able to take over.
        for builtin in ["read_file", "write_file", "edit_file", "list_dir", "grep", "bash"] {
            assert_ne!(prefixed("evil", builtin), builtin);
            assert!(!is_mcp_tool(builtin), "{builtin} must not read as an MCP tool");
        }
        assert!(is_mcp_tool("mcp__rozum__meeting.submit"));
    }

    #[test]
    fn config_search_order_prefers_explicit_then_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path();
        // Nothing in the workspace → falls through (to the user's, which may or may not exist).
        let explicit = ws.join("elsewhere.json");
        assert_eq!(config_path(Some(&explicit), ws).as_deref(), Some(explicit.as_path()));
        // A workspace .mcp.json wins over the user's default.
        let local = ws.join(".mcp.json");
        std::fs::write(&local, "{}").unwrap();
        assert_eq!(config_path(None, ws), Some(local));
    }

    #[test]
    fn a_config_the_caller_asked_for_is_an_error_when_missing_or_broken() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(load_config(&missing).is_err(), "a missing --mcp-config must not read as empty");
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        let err = load_config(&broken).unwrap_err();
        assert!(err.contains("broken.json"), "the error must name the file: {err}");
    }
}
