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

    /// Local LLM gateway — OpenAI and Anthropic dialects on 127.0.0.1
    ///
    /// After starting, set:
    ///   export ANTHROPIC_BASE_URL=http://localhost:<port>
    ///   export OPENAI_BASE_URL=http://localhost:<port>/v1
    Gateway {
        /// Port to listen on (default 8089)
        #[arg(long, default_value_t = 8089)]
        port: u16,

        /// Model spec: absolute .gguf path, "ollama:<name>", or "lmstudio:<repo>"
        #[arg(long)]
        model: String,

        /// Context window size in tokens (forwarded to GGUF backend)
        #[arg(long, default_value_t = 32768)]
        n_ctx: u32,
    },

    /// Start the gateway and launch a program with ANTHROPIC_/OPENAI_ env vars set.
    ///
    /// Example: rozum launch --model qwen3.5:9b-mlx claude
    /// Example: rozum launch --model qwen2.5-coder:32b -- aider --no-auto-commits
    Launch {
        /// Model spec (same as `gateway --model`)
        #[arg(long)]
        model: String,

        /// Port for the gateway (auto-picks a free port if not specified)
        #[arg(long)]
        port: Option<u16>,

        /// Context window size in tokens
        #[arg(long, default_value_t = 32768)]
        n_ctx: u32,

        /// Program to launch and its arguments
        #[arg(trailing_var_arg = true, required = true)]
        program: Vec<String>,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse_from(reorder_launch_args(std::env::args().collect()));

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
            if let Err(e) = rozum::web::run_bridge_with(&room, &name, port, !no_persist).await {
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
        Some(Command::Gateway { port, model, n_ctx }) => {
            run_gateway(port, model, n_ctx).await;
        }
        Some(Command::Launch {
            model,
            port,
            n_ctx,
            program,
        }) => {
            run_launch(model, port, n_ctx, program).await;
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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tracing::info!(url = %url, "starting web-bridge");
    if let Err(e) = rozum::web::run_bridge(&room_name, "web", port).await {
        tracing::error!(error = %e, "web-bridge exited");
    }
}

async fn run_gateway(port: u16, model_spec: String, n_ctx: u32) {
    let backend = build_gateway_backend(&model_spec, n_ctx).await;
    eprintln!("rozum gateway  http://127.0.0.1:{port}");
    eprintln!("  model:              {model_spec}");
    eprintln!();
    eprintln!("  # Claude Code:");
    eprintln!("  export ANTHROPIC_BASE_URL=http://127.0.0.1:{port}");
    eprintln!("  export ANTHROPIC_API_KEY=rozum-local   # any value to enable custom URL");
    eprintln!();
    eprintln!("  # OpenAI Codex / aider:");
    eprintln!("  export OPENAI_BASE_URL=http://127.0.0.1:{port}/v1");
    eprintln!("  export OPENAI_API_KEY=rozum-local");
    if let Err(e) = rozum::gateway::run(backend, port, model_spec).await {
        eprintln!("gateway error: {e}");
        std::process::exit(1);
    }
}

/// Pull rozum-known flags out of the program's trailing args.
///
/// `rozum launch claude --model X --port 9000` is equivalent to
/// `rozum launch --model X --port 9000 claude`.
/// Stops scanning at `--` so the user can still pass identically-named flags
/// to the child program after an explicit separator:
///   `rozum launch --model X claude -- --model claude-specific-flag`
fn reorder_launch_args(mut args: Vec<String>) -> Vec<String> {
    let Some(launch_idx) = args.iter().position(|a| a == "launch") else {
        return args;
    };

    const KNOWN_FLAGS: &[&str] = &["--model", "--port", "--n-ctx"];

    // Collect args after "launch", pull known flag+value pairs to the front.
    let tail: Vec<String> = args.split_off(launch_idx + 1);
    let mut pulled: Vec<String> = Vec::new();
    let mut remaining: Vec<String> = Vec::new();
    let mut iter = tail.into_iter().peekable();

    while let Some(arg) = iter.next() {
        if arg == "--" {
            // Stop reordering at explicit separator; pass the rest through verbatim.
            remaining.push(arg);
            remaining.extend(iter);
            break;
        }
        if let Some(flag) = KNOWN_FLAGS.iter().find(|f| arg == **f) {
            pulled.push((*flag).to_owned());
            if let Some(value) = iter.next() {
                pulled.push(value);
            }
        } else if let Some(flag) = KNOWN_FLAGS
            .iter()
            .find(|f| arg.starts_with(&format!("{f}=")))
        {
            // Support --flag=value form too.
            let _ = flag;
            pulled.push(arg);
        } else {
            remaining.push(arg);
        }
    }

    args.extend(pulled);
    args.extend(remaining);
    args
}

async fn run_launch(model_spec: String, port: Option<u16>, n_ctx: u32, program: Vec<String>) {
    use std::process::Command as StdCommand;

    let (program_name, args) = program
        .split_first()
        .expect("clap requires at least one arg");

    // Pick a free port if not specified.
    let port = port.unwrap_or_else(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .ok()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(8089)
    });

    let backend = build_gateway_backend(&model_spec, n_ctx).await;

    // Bind the listener before forking off the child so it can connect
    // immediately without racing the gateway startup.
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("rozum launch: failed to bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };

    eprintln!("rozum launch  gateway=http://127.0.0.1:{port}  model={model_spec}");
    eprintln!("  → running: {} {}", program_name, args.join(" "));

    // Compute the claude-prefixed alias before moving model_spec into the gateway task.
    let claude_alias = rozum::gateway::claude_model_alias(&model_spec);

    // Start the gateway in a background task.
    let gateway_handle = tokio::spawn(async move {
        if let Err(e) = rozum::gateway::serve_on(backend, listener, model_spec).await {
            eprintln!("gateway error: {e}");
        }
    });

    // Build the child command with both API conventions set.
    // Claude Code precedence: ANTHROPIC_AUTH_TOKEN > ANTHROPIC_API_KEY > OAuth.
    // Using ANTHROPIC_AUTH_TOKEN AND explicitly clearing ANTHROPIC_API_KEY avoids
    // the "Auth conflict" warning while still leaving the user's global OAuth
    // login intact (no `claude /logout` required).
    let base = format!("http://127.0.0.1:{port}");
    let mut cmd = StdCommand::new(program_name);
    cmd.args(args);
    cmd.env("ANTHROPIC_BASE_URL", &base);
    cmd.env("ANTHROPIC_AUTH_TOKEN", "rozum-local");
    cmd.env_remove("ANTHROPIC_API_KEY");
    // Ask Claude Code to query our /v1/models endpoint so the local model
    // shows up in the /model picker.
    cmd.env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
    // Pre-select our model so Claude Code starts on it without the user
    // having to open /model and pick it manually.
    cmd.env("ANTHROPIC_MODEL", &claude_alias);
    cmd.env("ANTHROPIC_DEFAULT_OPUS_MODEL", &claude_alias);
    cmd.env("ANTHROPIC_DEFAULT_SONNET_MODEL", &claude_alias);
    cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", &claude_alias);
    cmd.env("OPENAI_BASE_URL", format!("{base}/v1"));
    cmd.env("OPENAI_API_KEY", "rozum-local");
    cmd.env("ROZUM_GATEWAY_URL", &base);

    // Run the child synchronously in a blocking task so we can wait on its exit code.
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .ok()
        .and_then(|r| r.ok());

    // Tear down the gateway and exit with the child's code.
    gateway_handle.abort();
    let code = match status {
        Some(s) => s.code().unwrap_or(1),
        None => {
            eprintln!("rozum launch: failed to spawn '{program_name}'");
            127
        }
    };
    std::process::exit(code);
}

async fn build_gateway_backend(
    model_spec: &str,
    n_ctx: u32,
) -> std::sync::Arc<dyn rozum::ChatBackend> {
    // 1. Try in-process GGUF (fastest, most efficient, needs --features gguf)
    if let Some(b) = try_build_gguf_backend(model_spec, n_ctx) {
        return b;
    }

    // 2. Try Ollama HTTP (works with GGUF, MLX, and any Ollama-supported format)
    if let Some(b) = rozum::openai_http::try_ollama(model_spec).await {
        return b;
    }

    // 3. Try mlx_lm.server at default port (for MLX models run separately)
    if let Some(b) = rozum::openai_http::try_mlx_server(model_spec).await {
        return b;
    }

    // 4. Try user-specified URL via env
    if let Ok(url) = std::env::var("ROZUM_BACKEND_URL") {
        eprintln!("using HTTP backend at {url}");
        return std::sync::Arc::new(rozum::openai_http::OpenAiHttpBackend::new(url, model_spec));
    }

    let bare = model_spec
        .strip_prefix("ollama:")
        .or_else(|| model_spec.strip_prefix("mlx:"))
        .unwrap_or(&model_spec);
    eprintln!("warning: no backend found for '{model_spec}'");
    eprintln!("  tried: in-process GGUF, Ollama (localhost:11434), mlx_lm.server (localhost:8080)");
    eprintln!("  hints:");
    eprintln!("    ollama serve && ollama pull {bare}");
    eprintln!("    python -m mlx_lm.server --model <mlx-model-id>");
    eprintln!("    ROZUM_BACKEND_URL=http://your-server/v1 rozum gateway ...");
    eprintln!("  falling back to HelloBackend (echo server)");
    std::sync::Arc::new(rozum::HelloBackend::new())
}

#[cfg(feature = "gguf")]
fn try_build_gguf_backend(
    model_spec: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::gguf::{GgufBackend, GgufOptions, resolve_model_path};
    let path = resolve_model_path(model_spec)?;
    let opts = GgufOptions {
        n_ctx,
        ..GgufOptions::default()
    };
    match GgufBackend::new(path, opts) {
        Ok(b) => Some(std::sync::Arc::new(b)),
        Err(e) => {
            eprintln!("warning: GgufBackend load failed: {e}");
            None
        }
    }
}

#[cfg(not(feature = "gguf"))]
fn try_build_gguf_backend(
    _model_spec: &str,
    _n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    None
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
