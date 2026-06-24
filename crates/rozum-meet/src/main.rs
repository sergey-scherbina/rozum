//! `rozum-meet` — thin meeting-room frontend binary.
//!
//! Hosts the two MCP transports for the meeting room and nothing else. It links only
//! `rozum-meeting`, so it carries NO model engines (no `mlx-sys` / `llama-cpp-2` C++) and builds
//! in seconds — the point of the binary split (`docs/specs/binary-split.md`): a frontend fix must
//! not drag the heavy backend through a rebuild.
//!
//! Subcommands mirror the umbrella `rozum` binary so the MCP config can point at either:
//! - `rozum-meet mcp-proxy` — per-session stdio bridge (what CC spawns today).
//! - `rozum-meet mcp-http`  — long-lived HTTP MCP server; CC connects via `{type:"http", url}`
//!   and reconnects on drop, so there is no per-session child to crash (BUG-004 fix, Phase 2).

use clap::{Parser, Subcommand};
use rozum_meeting::meeting::{daemon_proxy::run_daemon_proxy, http_proxy::run_http_proxy};

#[derive(Parser)]
#[command(
    name = "rozum-meet",
    about = "Thin meeting-room frontend: MCP bridges (stdio + HTTP). No model engines linked."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Stdio MCP proxy (per-session child; add to an agent MCP config).
    McpProxy,
    /// HTTP MCP server (long-lived; agents connect via {type:"http", url} and reconnect on drop).
    McpHttp {
        /// Loopback port to bind (serves `/mcp`).
        #[arg(long, default_value_t = 8779)]
        port: u16,
        /// Pin the project room (absolute path); default = detect from cwd.
        #[arg(long)]
        project: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match Cli::parse().cmd {
        Cmd::McpProxy => run_daemon_proxy().await,
        Cmd::McpHttp { port, project } => run_http_proxy(port, project).await,
    }
}
