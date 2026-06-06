use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rozum", about = "rozum meeting-room agent")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Room name (auto-generated if omitted)
    #[arg(long)]
    room: Option<String>,

    /// Meeting topic
    #[arg(long, default_value = "")]
    topic: String,

    /// Your display name in the meeting
    #[arg(long)]
    r#as: Option<String>,

    /// Start web interface on this port (e.g. --web-port 8080)
    #[arg(long)]
    web_port: Option<u16>,

    /// Disable on-disk transcript persistence for the room
    /// ($XDG_STATE_HOME/rozum/rooms/<name>/room-transcript.jsonl).
    #[arg(long)]
    no_persist: bool,

    /// Hard cap on total transcript characters before the room auto-ends
    /// with reason "budget". Unlimited if not set.
    #[arg(long)]
    budget: Option<usize>,

    /// Per-turn soft warning threshold (in tokens; chars are budgeted at
    /// roughly 4× this number). No warning if not set.
    #[arg(long)]
    per_turn_budget: Option<usize>,
}

#[derive(Subcommand)]
enum Command {
    /// List active rozum meeting rooms
    List,

    /// Run stdio MCP proxy (add to agent MCP config)
    #[command(alias = "mpc-proxy")]
    McpProxy,

    /// Bridge a Telegram chat to a rozum room
    Telegram {
        /// Room name to join (must be running)
        #[arg(long)]
        room: String,

        /// Display name in the room
        #[arg(long, default_value = "telegram")]
        name: String,
    },

    /// Bridge a Discord channel to a rozum room
    Discord {
        /// Room name to join (must be running)
        #[arg(long)]
        room: String,

        /// Display name in the room
        #[arg(long, default_value = "discord")]
        name: String,
    },

    /// Expose a rozum room over HTTP/WebSocket for web clients
    Web {
        /// Room name to join (must be running)
        #[arg(long)]
        room: String,

        /// Display name in the room
        #[arg(long, default_value = "web")]
        name: String,

        /// Port to listen on
        #[arg(long, default_value_t = 8080)]
        port: u16,

        /// Disable transcript persistence to disk
        /// ($XDG_STATE_HOME/rozum/rooms/<room>/transcript.jsonl)
        #[arg(long)]
        no_persist: bool,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // The default subcommand launches a TUI. Anything written to stderr
    // (tracing output, stray eprintln!) corrupts the terminal because
    // ratatui owns the screen in raw mode. Route logs to a file in that
    // case; keep stderr for non-TUI subcommands.
    let writes_to_stderr = cli.command.is_some();
    if writes_to_stderr {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
            )
            .with_writer(std::io::stderr)
            .init();
    } else {
        init_tui_logging();
    }

    match cli.command {
        None => {
            // Default: launch meeting room.
            run_room(
                cli.room,
                &cli.topic,
                cli.r#as,
                cli.web_port,
                !cli.no_persist,
                cli.budget,
                cli.per_turn_budget,
            )
            .await;
        }
        Some(Command::List) => {
            let rooms = rozum::meeting::list_rooms().await;
            if rooms.is_empty() {
                println!("No active rozum rooms.");
                println!("Start one with: rozum --topic \"Your topic\"");
                return;
            }
            println!("{:<20} {:<30} {:>4}", "NAME", "TOPIC", "PARTICIPANTS");
            for r in rooms {
                println!(
                    "{:<20} {:<30} {:>4}",
                    r.name,
                    if r.topic.is_empty() {
                        "(open floor)".into()
                    } else {
                        r.topic
                    },
                    r.participants.len()
                );
            }
        }
        Some(Command::McpProxy) => {
            if let Err(e) = rozum::meeting::run_proxy().await {
                eprintln!("proxy error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Web {
            room,
            name,
            port,
            no_persist,
        }) => {
            if let Err(e) =
                rozum::web::run_bridge_with(&room, &name, port, !no_persist).await
            {
                eprintln!("web bridge error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Discord { room, name }) => {
            let token = std::env::var("DISCORD_BOT_TOKEN").unwrap_or_else(|_| {
                eprintln!("error: DISCORD_BOT_TOKEN not set");
                std::process::exit(1);
            });
            let channel_id = std::env::var("DISCORD_CHANNEL_ID").unwrap_or_else(|_| {
                eprintln!("error: DISCORD_CHANNEL_ID not set");
                std::process::exit(1);
            });
            if let Err(e) = rozum::discord::run_bridge(&room, &name, token, channel_id).await {
                eprintln!("discord bridge error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Telegram { room, name }) => {
            let token = std::env::var("TELEGRAM_BOT_TOKEN").unwrap_or_else(|_| {
                eprintln!("error: TELEGRAM_BOT_TOKEN not set");
                std::process::exit(1);
            });
            let chat_id: i64 = std::env::var("TELEGRAM_CHAT_ID")
                .unwrap_or_else(|_| {
                    eprintln!("error: TELEGRAM_CHAT_ID not set");
                    std::process::exit(1);
                })
                .parse()
                .unwrap_or_else(|_| {
                    eprintln!("error: TELEGRAM_CHAT_ID must be a numeric chat ID");
                    std::process::exit(1);
                });
            if let Err(e) = rozum::telegram::run_bridge(&room, &name, token, chat_id).await {
                eprintln!("telegram bridge error: {e}");
                std::process::exit(1);
            }
        }
    }
}

async fn run_room(
    room: Option<String>,
    topic: &str,
    display_name: Option<String>,
    web_port: Option<u16>,
    persist: bool,
    budget: Option<usize>,
    per_turn_budget: Option<usize>,
) {
    use rozum::meeting::app::RoomConfig;

    let name = room.unwrap_or_else(rozum::meeting::generate_room_name);
    let username =
        display_name.unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "user".into()));

    let web_url = web_port.map(|port| {
        let host = local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "localhost".to_owned());
        format!("http://{host}:{port}")
    });

    let budget_guard = rozum::meeting::budget::BudgetGuard::new(
        per_turn_budget.unwrap_or(usize::MAX),
        budget.unwrap_or(usize::MAX),
    );
    let config = RoomConfig {
        name: name.clone(),
        topic: topic.to_owned(),
        human_display_name: username,
        budget: budget_guard,
        web_url: web_url.clone(),
        persist,
    };

    if let Some(port) = web_port {
        let room_name = name.clone();
        let url = web_url.unwrap_or_default();
        tokio::spawn(async move {
            start_web_bridge(room_name, port, url).await;
        });
    }

    tracing::info!(room = %name, "rozum room starting");
    if let Err(e) = rozum::meeting::run_room(config, false).await {
        tracing::error!(error = %e, "room error");
        std::process::exit(1);
    }
}

fn local_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    Some(socket.local_addr().ok()?.ip())
}

async fn start_web_bridge(room_name: String, port: u16, url: String) {
    let socket_path = rozum::meeting::room_path::room_socket(&room_name);
    for _ in 0..100 {
        if socket_path.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    tracing::info!(url = %url, "starting web-bridge");
    if let Err(e) = rozum::web::run_bridge(&room_name, "web", port).await {
        tracing::error!(error = %e, "web-bridge exited");
    }
}

/// Send tracing output to a log file under `$XDG_STATE_HOME/rozum/log/`
/// instead of stderr so the TUI keeps a clean screen. The file is
/// truncated on each launch — useful for debugging the most recent run
/// without accumulating noise across sessions.
fn init_tui_logging() {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local/state"))
                .unwrap_or_else(|| std::path::PathBuf::from(".local/state"))
        });
    let log_dir = base.join("rozum").join("log");
    let _ = std::fs::create_dir_all(&log_dir);
    let log_path = log_dir.join("rozum.log");
    match std::fs::File::create(&log_path) {
        Ok(file) => {
            let writer = std::sync::Mutex::new(file);
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
                )
                .with_writer(writer)
                .with_ansi(false)
                .init();
        }
        Err(_) => {
            // Silent fallback: drop tracing entirely. Stderr stays clean.
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::sink)
                .try_init();
        }
    }
}
