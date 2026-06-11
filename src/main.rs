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

        /// Model spec: absolute .gguf path, "lmstudio:<repo>", or any model id
        /// understood by mlx_lm.server / ROZUM_BACKEND_URL
        #[arg(long)]
        model: String,

        /// Context window in tokens. Default: the model's max context (from its
        /// config.json), capped so the KV cache stays within a fraction of RAM;
        /// falls back to 32768 if the model max is unknown. Lower it to save RAM.
        #[arg(long)]
        n_ctx: Option<u32>,
    },

    /// Start the gateway and launch a program with ANTHROPIC_/OPENAI_ env vars set.
    ///
    /// Example: rozum launch --model /path/to/qwen-coder.gguf claude
    /// Example: rozum launch --model mlx-community/Qwen2.5-Coder-32B-Instruct-4bit claude
    /// Example: rozum launch --model qwen2.5-coder:32b -- aider --no-auto-commits
    Launch {
        /// Model spec (same as `gateway --model`)
        #[arg(long)]
        model: String,

        /// Port for the gateway (auto-picks a free port if not specified)
        #[arg(long)]
        port: Option<u16>,

        /// Context window in tokens. Default: the model's max context (from its
        /// config.json), capped so the KV cache stays within a fraction of RAM;
        /// falls back to 32768 if the model max is unknown. Lower it to save RAM.
        #[arg(long)]
        n_ctx: Option<u32>,

        /// Program to launch and its arguments
        #[arg(trailing_var_arg = true, required = true)]
        program: Vec<String>,
    },

    /// Inspect installed and recommended local LLM models
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },
}

#[derive(Subcommand)]
enum ModelsAction {
    /// List installed models (default), or `--remote` for the curated download list
    List {
        /// Show curated download recommendations instead of installed models
        #[arg(long)]
        remote: bool,
    },

    /// Show details for a model spec (works for installed and non-installed)
    Info {
        /// Model spec: `mlx-community:...`, `hf:<user>/<repo>`, `<ollama-tag>`,
        /// `lmstudio:<repo>`, or an absolute path
        spec: String,
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
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    // Default: warn everywhere, but info for hf-hub and mistralrs
                    // so the user sees download progress and load events.
                    tracing_subscriber::EnvFilter::new(
                        "warn,hf_hub=info,mistralrs=info,mistralrs_core=info",
                    )
                }),
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
        Some(Command::Models { action }) => {
            run_models(action).await;
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

async fn run_gateway(port: u16, model_spec: String, n_ctx: Option<u32>) {
    let n_ctx = resolve_n_ctx(&model_spec, n_ctx);
    let backend = match build_gateway_backend(&model_spec, n_ctx).await {
        Some(b) => b,
        None => {
            print_no_backend_hints(&model_spec);
            std::process::exit(1);
        }
    };
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

async fn run_launch(
    model_spec: String,
    port: Option<u16>,
    n_ctx: Option<u32>,
    program: Vec<String>,
) {
    use std::process::Command as StdCommand;

    let n_ctx = resolve_n_ctx(&model_spec, n_ctx);
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

    let backend = match build_gateway_backend(&model_spec, n_ctx).await {
        Some(b) => b,
        None => {
            print_no_backend_hints(&model_spec);
            std::process::exit(1);
        }
    };

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

    // Trim Claude Code's system prompt (bundled skills, git instructions, CLAUDE.md)
    // and non-essential background calls so its large prompts fit the local model's
    // smaller context window. Defaults only: a value the user already exported wins.
    for (k, v) in [
        ("CLAUDE_CODE_DISABLE_BUNDLED_SKILLS", "1"),
        ("CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS", "1"),
        ("CLAUDE_CODE_DISABLE_CLAUDE_MDS", "1"),
        ("CLAUDE_CODE_ATTRIBUTION_HEADER", "0"),
        ("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1"),
        ("DISABLE_NON_ESSENTIAL_MODEL_CALLS", "1"),
    ] {
        if std::env::var_os(k).is_none() {
            cmd.env(k, v);
        }
    }

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

async fn run_models(action: ModelsAction) {
    use rozum::models;

    match action {
        ModelsAction::List { remote: false } => {
            let installed = models::scan_all_installed();
            if installed.is_empty() {
                println!("No local models found.");
                println!();
                println!("Cache directories scanned:");
                println!("  ~/.cache/huggingface/hub/      (mistralrs / hf-hub)");
                println!("  ~/.ollama/models/blobs/        (via Ollama)");
                println!("  ~/.cache/lm-studio/models/     (via LMStudio)");
                println!();
                println!("See `rozum models list --remote` for recommended models to download.");
                return;
            }
            println!(
                "{:<10}  {:>10}  {}",
                "SOURCE", "SIZE", "SPEC (pass to --model)"
            );
            for m in &installed {
                println!(
                    "{:<10}  {:>10}  {}",
                    m.source.label(),
                    models::format_size(m.size_bytes),
                    m.spec
                );
            }
            let total: u64 = installed.iter().map(|m| m.size_bytes).sum();
            println!();
            println!(
                "{} models, {} total",
                installed.len(),
                models::format_size(total)
            );
        }

        ModelsAction::List { remote: true } => {
            println!("Curated download recommendations (Apple Silicon 24-36 GB):");
            println!();
            println!("{:<55} {:>7}  {}", "SPEC", "SIZE", "NOTES");
            for m in models::RECOMMENDED {
                println!(
                    "{:<55} {:>4.1} GB  {}",
                    m.spec, m.approx_size_gb, m.display_name
                );
                println!("{:<55} {:>7}  {}", "", "", m.notes);
            }
            println!();
            println!("Install by launching with any of these specs, e.g.:");
            println!("  rozum launch --model mlx-community:Qwen3-4B-4bit claude");
            println!("Search more on HuggingFace: https://huggingface.co/mlx-community");
        }

        ModelsAction::Info { spec } => {
            run_info(&spec).await;
        }
    }
}

async fn run_info(spec: &str) {
    use rozum::models;

    println!("Model spec:  {spec}");
    println!();

    let installed = models::scan_all_installed();
    let local = installed.iter().find(|m| m.spec == spec);
    match local {
        Some(m) => {
            println!("Status:      installed locally");
            println!("Source:      {}", m.source.label());
            println!("Size:        {}", models::format_size(m.size_bytes));
            println!("Path:        {}", m.path.display());
            println!();
            println!("Run with:    rozum launch --model {spec} claude");
        }
        None => {
            println!("Status:      not installed locally");
            // Try HuggingFace metadata if the spec maps to an HF repo
            if let Some(hf_id) = models::spec_to_hf_id(spec) {
                println!("Fetching HuggingFace metadata for '{hf_id}' ...");
                println!();
                match models::fetch_hf_info(&hf_id).await {
                    Ok(info) => {
                        println!("HuggingFace:  https://huggingface.co/{}", info.id);
                        if let Some(a) = &info.author {
                            println!("Author:       {a}");
                        }
                        if let Some(p) = &info.pipeline_tag {
                            println!("Pipeline:     {p}");
                        }
                        if let Some(l) = &info.license {
                            println!("License:      {l}");
                        }
                        if let Some(d) = info.downloads {
                            println!("Downloads:    {d}");
                        }
                        if let Some(l) = info.likes {
                            println!("Likes:        {l}");
                        }
                        if info.total_bytes > 0 {
                            println!(
                                "Total size:   {} ({} files)",
                                models::format_size(info.total_bytes),
                                info.files.len()
                            );
                        } else {
                            println!("Files:        {}", info.files.len());
                        }
                        if !info.tags.is_empty() {
                            let tags = info
                                .tags
                                .iter()
                                .take(10)
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", ");
                            println!("Tags:         {tags}");
                        }
                        println!();
                        println!("Install by running:");
                        println!("  rozum launch --model {spec} claude");
                        println!("First launch will download into ~/.cache/huggingface/hub/");
                    }
                    Err(e) => {
                        println!("HuggingFace lookup failed: {e}");
                    }
                }
            } else {
                println!();
                println!("Spec form not recognised as a HuggingFace repo.");
                println!("Known forms:");
                println!("  mlx-community:<repo>");
                println!("  hf:<owner>/<repo>");
                println!("  <owner>/<repo>          (bare HuggingFace id)");
                println!("  <name>[:<tag>]          (Ollama-style — must be already pulled)");
                println!("  lmstudio:<owner>/<repo>");
                println!("  /absolute/path.gguf");
            }
        }
    }
}

/// Try to build a real backend for `model_spec`. Returns `None` if nothing
/// is reachable; caller exits with an error if it returns None.
async fn build_gateway_backend(
    model_spec: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    rozum::obs::log_event(serde_json::json!({
        "event": "backend_select_start", "model": model_spec, "n_ctx": n_ctx,
    }));

    // 1. Try in-process GGUF (fastest for GGUF files, needs --features gguf)
    if let Some(b) = try_build_gguf_backend(model_spec, n_ctx) {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"gguf","model":model_spec}),
        );
        return Some(b);
    }

    // 2. Try in-process native MLX via mistralrs (needs --features mistralrs)
    if let Some(b) = try_build_mistralrs_backend(model_spec, n_ctx).await {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"mistralrs","model":model_spec}),
        );
        return Some(b);
    }

    // 3. Try LM Studio's local server (native MLX runtime; covers Qwen3.6 MLX
    //    today, ahead of mistralrs AFQ support)
    if let Some(b) = rozum::openai_http::try_lmstudio_http(model_spec).await {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"lmstudio-http","model":model_spec}),
        );
        return Some(b);
    }

    // 4. Try mlx_lm.server (Python, for MLX-format models)
    if let Some(b) = rozum::openai_http::try_mlx_server(model_spec).await {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"mlx-server-http","model":model_spec}),
        );
        return Some(b);
    }

    // 3. Try user-specified URL via env (any OpenAI-compatible server)
    if let Ok(url) = std::env::var("ROZUM_BACKEND_URL") {
        eprintln!("backend: custom HTTP at {url}");
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"custom-http","url":url,"model":model_spec}),
        );
        return Some(std::sync::Arc::new(
            rozum::openai_http::OpenAiHttpBackend::new(url, model_spec),
        ));
    }

    rozum::obs::log_event(serde_json::json!({
        "event": "backend_select_failed", "model": model_spec,
        "note": "no backend: no local file, mistralrs load failed, no LM Studio/mlx_lm.server, ROZUM_BACKEND_URL unset",
    }));
    None
}

fn print_no_backend_hints(model_spec: &str) {
    eprintln!("no backend found for '{model_spec}'");
    eprintln!();
    // If the in-process load was skipped for a concrete reason (RAM preflight),
    // that is the actual cause — surface it first so the user is not left
    // guessing why a backend that is compiled in still did not run.
    if let Some(reason) = skip_reason_slot().lock().unwrap().take() {
        eprintln!("The in-process MLX model (mistralrs) is available but was NOT loaded:");
        eprintln!("  {reason}");
        eprintln!();
        eprintln!("To run it anyway despite low free RAM:");
        eprintln!("  ROZUM_FORCE_MISTRALRS=1 rozum launch --model {model_spec} claude");
        eprintln!("Or free memory (close other apps) and relaunch.");
        eprintln!();
        eprintln!("Other ways to get a backend:");
    } else {
        eprintln!("rozum needs an in-process model or an HTTP server to talk to.");
        eprintln!("Pick one:");
    }
    eprintln!();
    eprintln!("  in-process GGUF (Metal on Apple Silicon, .gguf files):");
    eprintln!("    brew install cmake");
    eprintln!("    cargo build --features gguf");
    eprintln!("    rozum launch --model /path/to/model.gguf       claude");
    eprintln!("    rozum launch --model 'lmstudio:<user>/<repo>'   claude");
    eprintln!(
        "    rozum launch --model '<ollama-name>:<tag>'      claude   # reads ~/.ollama/models/blobs/"
    );
    eprintln!();
    eprintln!("  in-process native MLX (mistralrs, on by default, Metal, safetensors):");
    eprintln!("    rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude");
    eprintln!("    rozum launch --model hf:Qwen/Qwen3-4B claude");
    eprintln!();
    eprintln!("  LM Studio (GUI app, native MLX runtime, covers Qwen3.6 today):");
    eprintln!("    1. Download LM Studio: https://lmstudio.ai");
    eprintln!("    2. Inside LM Studio, install the model (Search tab → mlx-community/Qwen3.6...)");
    eprintln!("    3. Start the local server (Developer tab → Status: Running)");
    eprintln!("    4. rozum launch --model <model-id-shown-in-lmstudio>  claude");
    eprintln!();
    eprintln!("  mlx_lm.server (Python, native MLX safetensors):");
    eprintln!("    pip install mlx-lm");
    eprintln!("    python -m mlx_lm.server --model mlx-community/<repo> &");
    eprintln!("    rozum launch --model mlx-community/<repo>  claude");
    eprintln!();
    eprintln!("  any OpenAI-compatible HTTP server:");
    eprintln!("    ROZUM_BACKEND_URL=http://your-server/v1 rozum launch --model <id> claude");
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

/// Context window when the model's max is unknown (non-HF path, no config.json).
const N_CTX_FALLBACK: u32 = 32_768;
/// Practical cap for the auto context. Models advertise huge maxes (Qwen3.6:
/// 262144) whose KV pool won't fit in RAM; 32k covers large agent prompts
/// (Claude Code tokenizes to ~24k) and fits a ~16-20 GB model on a 32 GB+ Mac.
const N_CTX_AUTO_CAP: u32 = 32_768;

/// Resolve the effective context window: an explicit `--n-ctx` wins; otherwise
/// pick the model's max context capped by [`auto_n_ctx`].
fn resolve_n_ctx(model_spec: &str, requested: Option<u32>) -> u32 {
    let n = requested.unwrap_or_else(|| auto_n_ctx(model_spec));
    eprintln!(
        "context window: {n} tokens{}",
        if requested.is_some() { "" } else { " (auto)" }
    );
    n
}

/// The model's `max_position_embeddings` (from config.json), capped at
/// [`N_CTX_AUTO_CAP`]; falls back to [`N_CTX_FALLBACK`] when the config can't
/// be read. The KV cache is what actually scales with this, so the cap keeps the
/// pre-allocated PagedAttention pool bounded; lower it (or raise it) with `--n-ctx`.
#[cfg(feature = "mistralrs")]
fn auto_n_ctx(model_spec: &str) -> u32 {
    let id = rozum::mistralrs_backend::normalize_spec(model_spec);
    cached_config_json(&id)
        .and_then(|cfg| {
            let t = cfg.get("text_config").cloned().unwrap_or(cfg);
            t.get("max_position_embeddings")
                .and_then(|v| v.as_u64())
                .map(|v| v as u32)
        })
        .map_or(N_CTX_FALLBACK, |model_max| model_max.min(N_CTX_AUTO_CAP))
}

#[cfg(not(feature = "mistralrs"))]
fn auto_n_ctx(_model_spec: &str) -> u32 {
    N_CTX_FALLBACK
}

/// Total physical RAM in bytes (macOS `sysctl hw.memsize`).
#[cfg(feature = "mistralrs")]
fn total_ram_bytes() -> Option<u64> {
    let out = std::process::Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Currently available RAM in bytes (macOS `vm_stat`: free + inactive +
/// speculative + purgeable pages, i.e. what can be handed out without swapping).
#[cfg(feature = "mistralrs")]
fn available_ram_bytes() -> Option<u64> {
    let out = std::process::Command::new("vm_stat").output().ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    let page_size = s
        .lines()
        .next()
        .and_then(|l| l.split("page size of ").nth(1))
        .and_then(|r| r.split(' ').next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(16384);
    let mut pages = 0u64;
    for line in s.lines() {
        for label in [
            "Pages free:",
            "Pages inactive:",
            "Pages speculative:",
            "Pages purgeable:",
        ] {
            if let Some(rest) = line.strip_prefix(label) {
                if let Ok(v) = rest.trim().trim_end_matches('.').parse::<u64>() {
                    pages += v;
                }
            }
        }
    }
    (pages > 0).then(|| pages * page_size)
}

/// Sum of `*.safetensors` blob sizes for a HuggingFace repo in the local cache,
/// following symlinks to the blobs. `None` if the repo isn't cached yet.
#[cfg(feature = "mistralrs")]
fn cached_weights_bytes(model_id: &str) -> Option<u64> {
    let (org, repo) = model_id.split_once('/')?;
    let home = std::env::var("HOME").ok()?;
    let snapshots = std::path::Path::new(&home)
        .join(".cache/huggingface/hub")
        .join(format!("models--{org}--{repo}"))
        .join("snapshots");
    let mut total = 0u64;
    for snap in std::fs::read_dir(&snapshots).ok()?.flatten() {
        for entry in std::fs::read_dir(snap.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let p = entry.path();
            if p.extension().is_some_and(|e| e == "safetensors") {
                // metadata() follows the symlink to the actual blob.
                if let Ok(meta) = std::fs::metadata(&p) {
                    total += meta.len();
                }
            }
        }
    }
    (total > 0).then_some(total)
}

/// Parse the model's `config.json` from the local HuggingFace cache, if present.
#[cfg(feature = "mistralrs")]
fn cached_config_json(model_id: &str) -> Option<serde_json::Value> {
    let (org, repo) = model_id.split_once('/')?;
    let home = std::env::var("HOME").ok()?;
    let snapshots = std::path::Path::new(&home)
        .join(".cache/huggingface/hub")
        .join(format!("models--{org}--{repo}"))
        .join("snapshots");
    for snap in std::fs::read_dir(&snapshots).ok()?.flatten() {
        let cfg = snap.path().join("config.json");
        if let Ok(bytes) = std::fs::read(&cfg) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                return Some(v);
            }
        }
    }
    None
}

/// Estimate the KV-cache size (bytes) for `n_ctx` tokens, from the model's
/// `config.json`. Only **full-attention** layers hold a context-sized KV cache
/// (hybrid models like Qwen3.6 interleave linear-attention layers with fixed
/// state). A rough guard for the memory preflight, not mistralrs's exact paged
/// allocation. KV is bf16 (2 bytes/elem). `None` if required fields are missing.
#[cfg(feature = "mistralrs")]
fn kv_cache_bytes(model_id: &str, n_ctx: u32) -> Option<u64> {
    kv_cache_bytes_from_config(&cached_config_json(model_id)?, n_ctx)
}

/// Pure KV-cache math, split out so it can be unit-tested without the HF cache.
#[cfg(feature = "mistralrs")]
fn kv_cache_bytes_from_config(cfg: &serde_json::Value, n_ctx: u32) -> Option<u64> {
    // Vision/omni checkpoints nest the LM under `text_config`; dense models are flat.
    let t = cfg.get("text_config").unwrap_or(cfg);
    let u = |k: &str| t.get(k).and_then(|v| v.as_u64());
    let num_layers = u("num_hidden_layers")?;
    let kv_heads = u("num_key_value_heads").or_else(|| u("num_attention_heads"))?;
    let head_dim = u("head_dim").or_else(|| Some(u("hidden_size")? / u("num_attention_heads")?))?;
    // How many layers keep a context-sized KV cache.
    let full_layers = match t.get("layer_types").and_then(|v| v.as_array()) {
        Some(types) => types
            .iter()
            .filter(|v| v.as_str() == Some("full_attention"))
            .count() as u64,
        None => match u("full_attention_interval") {
            Some(interval) if interval > 0 => num_layers / interval,
            _ => num_layers, // dense model: every layer attends
        },
    };
    const KV_DTYPE_BYTES: u64 = 2; // bf16 keys + values
    Some(2 * full_layers * kv_heads * head_dim * KV_DTYPE_BYTES * n_ctx as u64)
}

/// Estimate resident RAM (bytes) to run `model_id` at `n_ctx`: weights + a
/// context-sized KV cache (from `config.json`) + ~5% for activations and Metal
/// scratch buffers. When the config is unavailable (non-HF path, or a checkpoint
/// without `config.json`) we can't size the KV cache from architecture, so we
/// fall back to [`blind_footprint_bytes`] — still context-aware, just coarser.
#[cfg(feature = "mistralrs")]
fn runtime_footprint_bytes(model_id: &str, weights: u64, n_ctx: u32) -> u64 {
    match kv_cache_bytes(model_id, n_ctx) {
        Some(kv) => weights + kv + weights / 20,
        None => blind_footprint_bytes(weights, n_ctx),
    }
}

/// Footprint estimate without a model config. We can't compute the KV cache from
/// architecture, so we keep the historical `weights x 1.4` heuristic but make it
/// move with `n_ctx`: decompose `1.4` into `1.0` weights + `0.1` fixed overhead +
/// `0.3` KV, where the `0.3` was implicitly calibrated at 32k context. Scaling
/// only the KV part by `n_ctx / 32k` keeps the 32k value at exactly the old `1.4`
/// (no regression) while letting smaller/larger contexts shrink/grow the estimate.
#[cfg(feature = "mistralrs")]
fn blind_footprint_bytes(weights: u64, n_ctx: u32) -> u64 {
    const CALIB_CTX: f64 = 32_768.0; // context the historical x1.4 was tuned at
    const OVERHEAD_FRAC: f64 = 0.1; // activations + Metal scratch, context-independent
    const KV_FRAC_AT_CALIB: f64 = 0.3; // KV/weights for a dense model at CALIB_CTX
    let kv_frac = KV_FRAC_AT_CALIB * (n_ctx as f64 / CALIB_CTX);
    (weights as f64 * (1.0 + OVERHEAD_FRAC + kv_frac)) as u64
}

/// When the in-process load is skipped for a concrete, user-actionable reason
/// (currently: the RAM preflight), the message is stashed here so the final
/// "no backend found" output can lead with *why* instead of a generic list.
/// `eprintln!` from the gateway is easy to miss (the agent TUI scrolls it away),
/// so the reason must reappear at the end, right where the user is looking.
fn skip_reason_slot() -> &'static std::sync::Mutex<Option<String>> {
    static SLOT: std::sync::OnceLock<std::sync::Mutex<Option<String>>> = std::sync::OnceLock::new();
    SLOT.get_or_init(|| std::sync::Mutex::new(None))
}

/// Warn (and by default refuse) to load an in-process model that won't fit in
/// the RAM currently available, instead of letting it swap-thrash the machine
/// into an unkillable hang. Runtime footprint is `weights + KV cache + ~10%
/// overhead`, with the KV cache sized from the model's own `config.json` and the
/// requested `n_ctx` (see [`runtime_footprint_bytes`]). The check compares
/// against *available* RAM, not total, since the agent + IDE are already
/// resident. Override with `ROZUM_FORCE_MISTRALRS=1`.
#[cfg(feature = "mistralrs")]
fn memory_preflight_ok(model_id: &str, n_ctx: u32) -> bool {
    let Some(weights) = cached_weights_bytes(model_id) else {
        return true; // not cached yet (will download): can't measure, don't block
    };
    let total = total_ram_bytes().unwrap_or(0);
    let available = available_ram_bytes().unwrap_or(total);
    let est_runtime = runtime_footprint_bytes(model_id, weights, n_ctx);
    let kv = kv_cache_bytes(model_id, n_ctx);
    let gb = |b: u64| ((b as f64 / 1e9) * 10.0).round() / 10.0;
    rozum::obs::log_event(serde_json::json!({
        "event": "memory_preflight", "model": model_id, "n_ctx": n_ctx,
        "weights_gb": gb(weights), "kv_cache_gb": kv.map(gb),
        "est_runtime_gb": gb(est_runtime),
        "available_gb": gb(available), "total_ram_gb": gb(total),
    }));
    // Fits if it leaves a little headroom in what's actually free right now.
    if est_runtime as f64 <= available as f64 * 0.9 {
        return true;
    }
    let forced = std::env::var("ROZUM_FORCE_MISTRALRS").is_ok();
    let kv_note = match kv {
        Some(k) => format!(
            "{:.0} GB weights + ~{:.1} GB KV cache at n_ctx={n_ctx}",
            gb(weights),
            gb(k)
        ),
        None => format!(
            "{:.0} GB weights + KV estimated at n_ctx={n_ctx} (no config.json)",
            gb(weights)
        ),
    };
    let msg = format!(
        "model '{model_id}' needs ~{:.0} GB resident ({kv_note}) but only ~{:.0} GB RAM is free \
         right now ({:.0} GB total). Loading it in-process will swap-thrash and hang. {} \
         Lower --n-ctx to shrink the KV cache, free memory, or pick a smaller 4-bit model \
         (e.g. mlx-community:Qwen3-8B-4bit, or a 7-14B coder).",
        gb(est_runtime),
        gb(available),
        gb(total),
        if forced {
            "ROZUM_FORCE_MISTRALRS set: loading anyway."
        } else {
            "Skipping in-process load (set ROZUM_FORCE_MISTRALRS=1 to force)."
        },
    );
    eprintln!("WARNING: {msg}");
    tracing::warn!("{msg}");
    rozum::obs::log_event(serde_json::json!({
        "event": "memory_warning", "model": model_id, "forced": forced, "message": msg,
    }));
    if !forced {
        *skip_reason_slot().lock().unwrap() = Some(msg);
    }
    forced
}

/// Budget the engine's `max_num_seqs` at load time from the actual model
/// footprint vs the RAM free right now. `ROZUM_MISTRALRS_MAX_SEQS` forces an
/// exact value; otherwise `budgeted_max_num_seqs` clamps to `[1, ceiling]`
/// (`ROZUM_MISTRALRS_SEQS_CEILING`, default 8). Per-slot cost tracks the prefill
/// chunk (`MISTRALRS_PREFILL_CHUNK`). Spec:
/// docs/specs/mistralrs-concurrency-scheduling.md (Phase A).
#[cfg(feature = "mistralrs")]
fn resolve_max_num_seqs(model_id: &str, n_ctx: u32) -> usize {
    use rozum::mistralrs_backend::{
        ConcurrencyBudget, DEFAULT_SEQS_CEILING, budgeted_max_num_seqs, per_seq_prefill_peak,
    };
    let env_usize = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok());
    if let Some(n) = env_usize("ROZUM_MISTRALRS_MAX_SEQS").filter(|&n| n >= 1) {
        return n;
    }
    let ceiling = env_usize("ROZUM_MISTRALRS_SEQS_CEILING")
        .filter(|&n| n >= 1)
        .unwrap_or(DEFAULT_SEQS_CEILING);
    // Prefill chunk drives the per-slot peak; mirror the engine's paged default.
    let chunk = env_usize("MISTRALRS_PREFILL_CHUNK")
        .filter(|&n| n >= 1)
        .unwrap_or(4096);
    let budget = ConcurrencyBudget {
        available_ram: available_ram_bytes(),
        weights: cached_weights_bytes(model_id),
        kv_pool: kv_cache_bytes(model_id, n_ctx),
        per_seq_peak: per_seq_prefill_peak(chunk),
        ceiling,
    };
    let n = budgeted_max_num_seqs(&budget);
    let gb = |b: u64| ((b as f64 / 1e9) * 10.0).round() / 10.0;
    rozum::obs::log_event(serde_json::json!({
        "event": "concurrency_budget", "model": model_id, "n_ctx": n_ctx,
        "max_num_seqs": n, "ceiling": ceiling, "prefill_chunk": chunk,
        "available_gb": budget.available_ram.map(gb),
        "weights_gb": budget.weights.map(gb),
        "kv_pool_gb": budget.kv_pool.map(gb),
    }));
    if n > 1 {
        eprintln!("mistralrs: concurrency budget → max_num_seqs={n} (ceiling {ceiling})");
    }
    n
}

#[cfg(feature = "mistralrs")]
async fn try_build_mistralrs_backend(
    model_spec: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::mistralrs_backend::{MistralrsBackend, MistralrsOptions, normalize_spec};
    // Filesystem paths and `lmstudio:` specs belong to the GGUF backend.
    if std::path::Path::new(model_spec).exists() || model_spec.starts_with("lmstudio:") {
        return None;
    }
    let id = normalize_spec(model_spec);
    if !memory_preflight_ok(&id, n_ctx) {
        return None;
    }
    let opts = MistralrsOptions {
        n_ctx,
        max_num_seqs: resolve_max_num_seqs(&id, n_ctx),
    };
    match MistralrsBackend::new(&id, opts).await {
        Ok(b) => {
            eprintln!("backend: mistralrs (in-process, Metal) — model: {id}");
            Some(std::sync::Arc::new(b))
        }
        Err(e) => {
            eprintln!("warning: mistralrs load failed: {e}");
            rozum::obs::log_event(serde_json::json!({
                "event": "backend_load_failed", "backend": "mistralrs", "model": id, "error": e.to_string(),
            }));
            None
        }
    }
}

#[cfg(not(feature = "mistralrs"))]
async fn try_build_mistralrs_backend(
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

#[cfg(all(test, feature = "mistralrs"))]
mod tests {
    use super::*;
    use serde_json::json;

    // Qwen3.6-35B-A3B: 40 layers, 1-in-4 full attention (10), 2 KV heads,
    // head_dim 256. Only full-attention layers count toward the context KV cache.
    #[test]
    fn kv_cache_counts_only_full_attention_layers() {
        let mut layer_types = Vec::new();
        for i in 0..40 {
            layer_types.push(if i % 4 == 3 {
                "full_attention"
            } else {
                "linear_attention"
            });
        }
        let cfg = json!({
            "text_config": {
                "num_hidden_layers": 40,
                "num_key_value_heads": 2,
                "head_dim": 256,
                "layer_types": layer_types,
            }
        });
        let kv = kv_cache_bytes_from_config(&cfg, 32_768).unwrap();
        // 2(K+V) * 10 full layers * 2 kv_heads * 256 * 2 bytes * 32768 tokens
        assert_eq!(kv, 2 * 10 * 2 * 256 * 2 * 32_768);
    }

    // Dense model with no layer_types: every layer attends, head_dim derived.
    #[test]
    fn kv_cache_dense_model_uses_all_layers() {
        let cfg = json!({
            "num_hidden_layers": 32,
            "num_key_value_heads": 8,
            "hidden_size": 4096,
            "num_attention_heads": 32, // head_dim = 128
        });
        let kv = kv_cache_bytes_from_config(&cfg, 4096).unwrap();
        assert_eq!(kv, 2 * 32 * 8 * 128 * 2 * 4096);
    }

    // Blind fallback must equal the historical x1.4 at the 32k calibration point
    // (no regression) and move monotonically with context.
    #[test]
    fn blind_footprint_calibrates_to_old_heuristic_and_scales() {
        let w = 20_000_000_000u64;
        assert_eq!(blind_footprint_bytes(w, 32_768), (w as f64 * 1.4) as u64);
        assert!(blind_footprint_bytes(w, 4_096) < blind_footprint_bytes(w, 32_768));
        assert!(blind_footprint_bytes(w, 131_072) > blind_footprint_bytes(w, 32_768));
    }
}
