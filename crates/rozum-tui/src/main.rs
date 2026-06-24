//! `rozum-tui` — thin meeting-room TUI frontend. Attaches a ratatui client to the meeting daemon.
//! Links only `rozum-meeting` (no model engines), so it builds in seconds. The legacy in-process
//! room (model-as-participant sampling, web/telegram/discord bridges) stays in the umbrella
//! `rozum` binary. Part of the binary split (`docs/specs/binary-split.md`).

use clap::Parser;
use rozum_meeting::tui::attach::run_attach;

#[derive(Parser)]
#[command(
    name = "rozum-tui",
    about = "Thin frontend: attach a TUI to the meeting daemon. No model engines linked."
)]
struct Cli {
    /// Room to open (default = your project's room).
    #[arg(long)]
    room: Option<String>,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    if let Err(e) = run_attach(cli.room).await {
        eprintln!("rozum-tui: {e}");
        std::process::exit(1);
    }
}
