//! `rozum-web` — thin meeting-room web-bridge frontend. Links only `rozum-meeting` (no model
//! engines), so it builds in seconds. Mirrors the umbrella `rozum web` subcommand. Part of the
//! binary split (`docs/specs/binary-split.md`).

use clap::Parser;
use rozum_meeting::web::run_bridge_with;

#[derive(Parser)]
#[command(
    name = "rozum-web",
    about = "Thin frontend: expose a rozum room over HTTP/WebSocket. No model engines linked."
)]
struct Cli {
    /// Room name to join (must be running).
    #[arg(long)]
    room: String,
    /// Display name in the room.
    #[arg(long, default_value = "web")]
    name: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Disable transcript persistence to disk.
    #[arg(long)]
    no_persist: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run_bridge_with(&cli.room, &cli.name, cli.port, !cli.no_persist).await {
        eprintln!("rozum-web: {e}");
        std::process::exit(1);
    }
}
