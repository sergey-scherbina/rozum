//! `rozum-meet` — thin meeting-room frontend binary.
//!
//! Hosts the meeting MCP transports and messenger bridges. It links only `rozum-meeting`, so it
//! carries NO model engines (no `mlx-sys` / `llama-cpp-2` C++) and builds in seconds — the point of
//! the binary split (`docs/specs/binary-split.md`): a frontend fix must not drag the heavy backend
//! through a rebuild.
//!
//! Subcommands mirror the umbrella `rozum` binary so the MCP config can point at either:
//! - `rozum-meet mcp-proxy` — per-session stdio bridge (what CC spawns today).
//! - `rozum-meet mcp-http`  — long-lived HTTP MCP server; CC connects via `{type:"http", url}`
//!   and reconnects on drop, so there is no per-session child to crash (BUG-004 fix, Phase 2).
//! - `rozum-meet telegram|discord` — daemon-backed external-chat bridges.

use clap::{Parser, Subcommand};
use rozum_meeting::meeting::{daemon_proxy::run_daemon_proxy, http_proxy::run_http_proxy};

#[derive(Parser)]
#[command(
    name = "rozum-meet",
    about = "Thin meeting-room frontend: MCP and messenger bridges. No model engines linked."
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
    /// Bridge a Telegram chat to an existing daemon room.
    ///
    /// Reads TELEGRAM_BOT_TOKEN, TELEGRAM_CHAT_ID, and TELEGRAM_ALLOWED_USER_IDS
    /// (the allowlist is required for groups; `*` explicitly trusts every sender).
    Telegram {
        /// Room name to join (must already exist).
        #[arg(long)]
        room: String,
        /// Display name in the room.
        #[arg(long, default_value = "telegram")]
        name: String,
    },
    /// Bridge a Discord channel to an existing daemon room.
    ///
    /// Reads DISCORD_BOT_TOKEN, DISCORD_CHANNEL_ID, and the required
    /// DISCORD_ALLOWED_USER_IDS (`*` explicitly trusts every non-bot sender).
    Discord {
        /// Room name to join (must already exist).
        #[arg(long)]
        room: String,
        /// Display name in the room.
        #[arg(long, default_value = "discord")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    match Cli::parse().cmd {
        Cmd::McpProxy => run_daemon_proxy().await,
        Cmd::McpHttp { port, project } => run_http_proxy(port, project).await,
        Cmd::Telegram { room, name } => rozum_meeting::telegram::run_from_env(&room, &name).await,
        Cmd::Discord { room, name } => rozum_meeting::discord::run_from_env(&room, &name).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn telegram_cli_keeps_public_defaults() {
        let cli = Cli::try_parse_from(["rozum-meet", "telegram", "--room", "ops"]).unwrap();
        match cli.cmd {
            Cmd::Telegram { room, name } => {
                assert_eq!(room, "ops");
                assert_eq!(name, "telegram");
            }
            _ => panic!("expected telegram command"),
        }
    }

    #[test]
    fn discord_cli_accepts_custom_display_name() {
        let cli = Cli::try_parse_from([
            "rozum-meet",
            "discord",
            "--room",
            "support",
            "--name",
            "on-call",
        ])
        .unwrap();
        match cli.cmd {
            Cmd::Discord { room, name } => {
                assert_eq!(room, "support");
                assert_eq!(name, "on-call");
            }
            _ => panic!("expected discord command"),
        }
    }

    #[test]
    fn messenger_commands_require_a_room() {
        assert!(Cli::try_parse_from(["rozum-meet", "telegram"]).is_err());
        assert!(Cli::try_parse_from(["rozum-meet", "discord"]).is_err());
    }

    #[test]
    fn messenger_help_names_sender_allowlist_environment() {
        let telegram = match Cli::try_parse_from(["rozum-meet", "telegram", "--help"]) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("--help must stop argument parsing"),
        };
        assert!(telegram.contains("TELEGRAM_ALLOWED_USER_IDS"));

        let discord = match Cli::try_parse_from(["rozum-meet", "discord", "--help"]) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("--help must stop argument parsing"),
        };
        assert!(discord.contains("DISCORD_ALLOWED_USER_IDS"));
    }
}

/// Linked for its side effect: the build-stamp marker (`docs/specs/deployment-drift.md`). A crate
/// nothing references is a crate the linker never pulls in, so the stamp would be absent from
/// exactly the thin binaries that most need to say how old they are.
#[used]
static BUILD_STAMP: &str = rozum_stamp::MARK_PREFIX;
