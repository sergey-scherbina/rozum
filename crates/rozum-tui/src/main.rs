//! `rozum-tui` — thin meeting-room TUI frontend. Execs the GENERATED client
//! (`crates/rozum-meeting-tui`, emitted from `clients/control/meetings.ssc`).
//! Links only `rozum-meeting` (no model engines), so it builds in seconds. The legacy in-process
//! room (model-as-participant sampling, web/telegram/discord bridges) stays in the umbrella
//! `rozum` binary. Part of the binary split (`docs/specs/binary-split.md`).

use clap::Parser;
use rozum_meeting::tui::launch_generated;

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

fn main() {
    let cli = Cli::parse();
    // No async runtime any more: this binary's whole job is to hand the terminal to the generated
    // client, which it does by replacing itself.
    if let Err(e) = launch_generated(cli.room) {
        eprintln!("rozum-tui: {e}");
        std::process::exit(1);
    }
}
