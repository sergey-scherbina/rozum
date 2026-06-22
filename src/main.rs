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

    /// Use the legacy in-process single-room runtime (with web/telegram/discord
    /// bridges + model-as-participant sampling) instead of attaching a TUI to
    /// the meeting daemon. `--web-port` implies this.
    #[arg(long)]
    legacy_room: bool,
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
        /// understood by mlx_lm.server / ROZUM_BACKEND_URL. Required to run the
        /// daemon; not needed for `status` / `stop`. **Repeatable** — pass it
        /// several times (or one comma-separated value) to run the models as a
        /// cascade, e.g. `--model qwen3-4b --model claude-haiku-4-5`.
        #[arg(long)]
        model: Vec<String>,

        /// Cascade start-tier strategy when more than one model is given:
        /// `classify` (default), `learned`, or `cheapest`.
        #[arg(long)]
        strategy: Option<String>,

        /// Offline: disable all remote (cloud) cascade tiers — use only local
        /// models. Sets `ROZUM_OFFLINE`; the model picker hides cloud entries too.
        #[arg(long)]
        offline: bool,

        /// Context window in tokens. Default: the model's max context (from its
        /// config.json), capped so the KV cache stays within a fraction of RAM;
        /// falls back to 32768 if the model max is unknown. Lower it to save RAM.
        #[arg(long)]
        n_ctx: Option<u32>,

        /// Let reasoning models think (emit `<think>…</think>`). OFF by default:
        /// the gateway disables thinking so CC/Codex get clean output. Sets
        /// `ROZUM_ENABLE_THINKING` for the in-process backend.
        #[arg(long)]
        enable_thinking: bool,

        /// Speculative decoding: a small **draft** model (same tokenizer family,
        /// e.g. `mlx-community:Qwen3-4B-4bit`) proposes tokens the target verifies
        /// in one forward — faster decode, byte-identical greedy output. Sets
        /// `ROZUM_DRAFT_MODEL` (the matrix can also set it via env).
        #[arg(long)]
        draft_model: Option<String>,

        /// `status` or `stop` the shared gateway; omit to run the daemon.
        #[command(subcommand)]
        action: Option<GatewayAction>,
    },

    /// Start the gateway and launch a program with ANTHROPIC_/OPENAI_ env vars set.
    ///
    /// Example: rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude
    /// Example: rozum launch --model mlx-community:gpt-oss-20b-MXFP4-Q4 codex
    /// Example: rozum launch --model /path/to/model.gguf -- aider --no-auto-commits
    Launch {
        /// Model spec (same as `gateway --model`). Optional: if omitted and a
        /// shared gateway is already running, reuse its model; if nothing is
        /// running on a TTY, show an interactive picker (cached models first).
        /// **Repeatable** — pass it several times (or one comma-separated value)
        /// to run the models as a cascade (rozum auto-orders cheapest→most-capable).
        #[arg(long)]
        model: Vec<String>,

        /// Cascade start-tier strategy when more than one model is given:
        /// `classify` (default), `learned`, or `cheapest`.
        #[arg(long)]
        strategy: Option<String>,

        /// Offline: disable all remote (cloud) cascade tiers and hide cloud models
        /// in the picker — use only local models. Sets `ROZUM_OFFLINE`.
        #[arg(long)]
        offline: bool,

        /// Port for the gateway (auto-picks a free port if not specified)
        #[arg(long)]
        port: Option<u16>,

        /// Context window in tokens. Default: the model's max context (from its
        /// config.json), capped so the KV cache stays within a fraction of RAM;
        /// falls back to 32768 if the model max is unknown. Lower it to save RAM.
        #[arg(long)]
        n_ctx: Option<u32>,

        /// Bypass the shared gateway: run a private in-process model just for
        /// this launch (the pre-sharing behaviour). Use when you intentionally
        /// want a second model resident and own the memory cost.
        #[arg(long)]
        dedicated: bool,

        /// Don't run any local model: launch the agent against your configured
        /// upstream Anthropic credentials (real claude.ai login or API key).
        /// The gateway, model picker, and `--model`/`--dedicated`/`--n-ctx`/
        /// `--port` are all bypassed. This is also the first ("Anthropic") entry
        /// in the interactive model picker.
        #[arg(long, conflicts_with_all = ["model", "dedicated", "n_ctx", "port"])]
        no_model: bool,

        /// Don't inject `--dangerously-load-development-channels server:<name>`
        /// into a Claude Code child. By default (when the child is `claude` and
        /// its build supports the flag) the rozum mcp-proxy is registered as a
        /// channel so room activity wakes an idle session. Spec: channel-wakeup.
        #[arg(long)]
        no_channel_wakeup: bool,

        /// MCP server name the agent registered `rozum mcp-proxy` under; used for
        /// the channel-wakeup flag (`server:<name>`). Must match the name in the
        /// agent's MCP config or the channel registers nothing (silently). Can
        /// also be set with `ROZUM_CHANNEL_MCP_NAME` (the flag wins); handy for a
        /// shell profile or launch wrapper. Defaults to `rozum`.
        #[arg(long)]
        channel_mcp_name: Option<String>,

        /// Disable Tier-3 piggyback wakeup: don't fold pending meeting-room
        /// activity into the agent's chat requests as a system note. Piggyback is
        /// the fallback rung — already auto-off when Tier-1 channels are active for
        /// the agent, and on otherwise; this flag forces it off unconditionally.
        /// Force it on instead with `ROZUM_PIGGYBACK=1`. Only ever active for an
        /// agent that joined a room. Spec: `docs/specs/rozum-native-channels.md`.
        #[arg(long)]
        no_piggyback: bool,

        /// Point the agent at an external OpenAI-compatible server instead of a
        /// local model — e.g. Ollama (`http://localhost:11434/v1`), vLLM, or any
        /// `/v1` endpoint. The CLI equivalent of `ROZUM_BACKEND_URL`. Forces that
        /// backend (skips the local GGUF/MLX chain) and runs a lightweight
        /// in-process gateway (no shared daemon, no model load). Pass the upstream
        /// model name with `--model` (e.g. `--model qwen3:8b`).
        #[arg(long, conflicts_with = "no_model")]
        backend_url: Option<String>,

        /// Lean mode for local models: optimize Claude Code's request. (1) Strip non-coding
        /// tools via `--disallowedTools` (meeting-room MCP, plan/worktree/cron/task/workflow/
        /// skill/notebook/web) — CC otherwise ships ~33 tool schemas (~4.9K tokens) every
        /// request; --lean cuts it to ~0.8K (Read/Write/Edit/Bash). (2) Add
        /// `--exclude-dynamic-system-prompt-sections` so per-machine bits (incl. git status,
        /// which changes on every edit) leave the system prefix → it stays cacheable across
        /// turns instead of re-prefilling. CC's core system prompt is load-bearing and is left
        /// intact. No-op for non-`claude`; each lever skipped if you set it yourself.
        #[arg(long)]
        lean: bool,

        /// Disable the agent sandbox for this launch (sugar for `ROZUM_SANDBOX=0`).
        /// By default a launched agent runs jailed under Seatbelt on macOS — writes
        /// confined to the workspace + toolchain caches, secrets denied, loopback-only
        /// network, no per-action prompts (docs/specs/model-sandbox.md). Pass this to
        /// run it unconfined. No-op off macOS (the jail is macOS-only there anyway).
        #[arg(long)]
        no_sandbox: bool,

        /// Program to launch and its arguments
        #[arg(trailing_var_arg = true, required = true)]
        program: Vec<String>,
    },

    /// Inspect installed and recommended local LLM models
    Models {
        #[command(subcommand)]
        action: ModelsAction,
    },

    /// Install the gateway as an always-warm user service (launchd / systemd --user),
    /// instead of the lazy-spawn + idle-exit default.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Run / control the meeting-room daemon (hosts many disk-backed rooms).
    Meetings {
        #[command(subcommand)]
        action: MeetingsAction,
    },

    /// Generate a git commit message for the staged diff with a local model.
    ///
    /// Reads `git diff --cached` and prints a commit message. With a single
    /// `--model` it generates directly; with a comma-list (`small,big`) it runs a
    /// small-first cascade — the small model answers, and a structural commit-message
    /// gate escalates to the big model only when the cheap answer is unusable.
    CommitMsg {
        /// Model spec, or a `small,big` comma-list for the small-first cascade.
        /// Defaults to the configured `rozum.toml` model.
        #[arg(long)]
        model: Option<String>,

        /// Context window (tokens).
        #[arg(long)]
        n_ctx: Option<u32>,
    },

    /// Register the rozum meeting mcp-proxy in an agent's config, so bare agents auto-join.
    ///
    /// Uses each agent's own `mcp add`/`mcp remove`, so their config stays valid. After
    /// `install`, a bare `claude`/`codex` run gets the `meeting.*` tools + the channel and
    /// auto-joins its project's room — no `rozum launch` needed.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },

    /// Show or set this machine's local meeting identity (your stable handle in rooms).
    Identity {
        #[command(subcommand)]
        action: IdentityAction,
    },

    /// Read-only readiness report for the local demo path.
    Doctor {
        /// Probe an already-running meeting web/PWA endpoint.
        #[arg(long)]
        web_url: Option<String>,
        /// Treat warnings as a failing preflight.
        #[arg(long)]
        strict: bool,
    },
}

/// `rozum identity whoami|set-name` — the local human's stable meeting identity.
#[derive(Subcommand)]
enum IdentityAction {
    /// Show your local meeting identity (stable token + display name + file).
    Whoami,
    /// Set your display name in meeting rooms (keeps the stable token).
    SetName {
        /// The display name.
        name: String,
    },
}

/// `rozum mcp install/uninstall` — register/remove the meeting mcp-proxy in an agent's config.
#[derive(Subcommand)]
enum McpAction {
    /// Register `rozum mcp-proxy` in the agent's user-level MCP config.
    Install {
        /// Which agent(s): `claude`, `codex`, `opencode`, or `all` (default).
        #[arg(long, default_value = "all")]
        agent: String,
    },
    /// Remove the rozum mcp-proxy registration.
    Uninstall {
        /// Which agent(s): `claude`, `codex`, `opencode`, or `all` (default).
        #[arg(long, default_value = "all")]
        agent: String,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Generate + install + start the service (runs `rozum gateway --model …` at login).
    Install {
        /// Model spec(s); repeatable (or one comma value) to run a cascade. Same as `gateway --model`.
        #[arg(long)]
        model: Vec<String>,
        /// Context window in tokens.
        #[arg(long)]
        n_ctx: Option<u32>,
        /// Gateway port (default 8089).
        #[arg(long)]
        port: Option<u16>,
        /// Only local models (sets `ROZUM_OFFLINE` in the service env).
        #[arg(long)]
        offline: bool,
        /// Cascade start-tier strategy (`classify` | `learned` | `cheapest`).
        #[arg(long)]
        strategy: Option<String>,
    },
    /// Stop + uninstall the service.
    Uninstall,
    /// Start the installed service (load it / `systemctl --user start`).
    Start,
    /// Stop the running service without uninstalling it (unload / `systemctl --user stop`).
    Stop,
    /// Show the service status.
    Status,
}

#[derive(Subcommand)]
enum MeetingsAction {
    /// Start the meeting daemon (detached by default; `--foreground` stays attached).
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running meeting daemon (graceful).
    Stop,
    /// Show the meeting daemon status and its rooms.
    Status,
    /// Attach a TUI to a room (defaults to the cwd project's room).
    Attach {
        /// Room name (from `rooms.list`); default is the cwd project's room.
        #[arg(long)]
        room: Option<String>,
    },
    /// Install + start as a launchd/systemd user service (auto-start at login).
    Install,
    /// Stop + remove the user service.
    Uninstall,
    /// Post a one-shot message to a room (the cwd project's room by default).
    ///
    /// The transport for coordination hooks (SessionStart/Stop) and quick human/script
    /// posts. Auto-spawns the daemon if it isn't running.
    Post {
        /// The message text.
        text: String,
        /// Target room name (from `rozum meetings status`); default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// Author display name (default: $USER, or $ROZUM_MEETING_AS). Hooks pass the agent's name.
        #[arg(long = "as")]
        as_display: Option<String>,
    },

    /// Join a room as a LIVE AI participant backed by a local model (via the gateway).
    ///
    /// The model reads the room and replies like any other participant — no moderator,
    /// no turn-taking. Run one per model (e.g. gpt-oss, qwen3.6) for a demo conference.
    /// Spec: `docs/specs/demo-conference.md`.
    Participant {
        /// Model spec the gateway serves (e.g. `mlx-community:gpt-oss-20b-MXFP4-Q4`).
        #[arg(long)]
        model: String,
        /// Room to join (created if absent).
        #[arg(long)]
        room: String,
        /// Roster handle (default derived from the model, e.g. `gpt-oss`).
        #[arg(long = "as")]
        as_handle: Option<String>,
        /// When to reply: `mention` (default) | `always` | `manual`.
        #[arg(long = "reply-policy", default_value = "mention")]
        reply_policy: String,
        /// Gateway base URL serving the model (OpenAI `/v1`).
        #[arg(long = "gateway-url", default_value = "http://127.0.0.1:8080/v1")]
        gateway_url: String,
        /// Other model handles in the room — so `--reply-policy always` never loops model↔model.
        #[arg(long = "peer")]
        peers: Vec<String>,
        /// Persona / context for the model's system prompt (who it is, what the conference is,
        /// domain facts) so it answers on-topic rather than generically.
        #[arg(long)]
        persona: Option<String>,
        /// Read the persona from a file (takes precedence over `--persona`). Handy for a long one.
        #[arg(long = "persona-file")]
        persona_file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
enum GatewayAction {
    /// Show the active shared gateway (model, port, pid, uptime, clients).
    Status,
    /// Stop the active shared gateway (refused if clients are attached, unless --force).
    Stop {
        #[arg(long)]
        force: bool,
    },
    /// Transparently swap the resident model (and/or backend) in place: drain →
    /// unload → load the new one → resume. Clients' requests are held by their
    /// proxy across the gap, not failed.
    Switch {
        /// New model spec to load.
        #[arg(long)]
        model: String,
        /// Context window for the new model (default: keep the current one).
        #[arg(long)]
        n_ctx: Option<u32>,
        /// Force a specific engine: gguf, mistralrs, lmstudio, mlx, mlx-server, url.
        #[arg(long)]
        backend: Option<String>,
    },
    /// Graceful restart of the daemon from the current binary (e.g. after
    /// upgrading `rozum`). Drains in-flight work, then re-execs.
    Reload,
    /// Free the resident model but keep the daemon (lazy-reload on next request).
    Unload,
}

#[derive(Subcommand)]
enum ModelsAction {
    /// List installed models (default), or `--remote` for the curated download list
    List {
        /// Show curated download recommendations instead of installed models
        #[arg(long)]
        remote: bool,

        /// With `--remote`, also list the extended fallback catalog (older / niche models)
        #[arg(long)]
        all: bool,
    },

    /// Show details for a model spec (works for installed and non-installed)
    Info {
        /// Model spec: `mlx-community:...`, `hf:<user>/<repo>`,
        /// `modelscope:<owner>/<repo>`, `ollama:<name>[:<tag>]`, `lmstudio:<repo>`,
        /// or an absolute path
        spec: String,
    },

    /// Delete a cached model (frees disk). Refused if it is the active gateway
    /// model. HuggingFace/LMStudio dirs are removed directly; Ollama is delegated
    /// to `ollama rm` (its blobs are content-addressed and shared).
    Rm {
        /// Spec of an installed model, exactly as shown by `rozum models list`
        spec: String,

        /// Skip the confirmation prompt (required for non-interactive use)
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[tokio::main]
async fn main() {
    // Register the in-process engine constructors that live above `rozum-core`
    // (inversion of control for the workspace split — core never depends on an
    // engine). A no-op when the corresponding feature is off.
    rozum::gguf::register_engine();
    rozum::mlx_native_backend::register_telemetry();

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
            // Default: attach a TUI to the meeting daemon. The legacy in-process
            // room (with web/telegram/discord bridges + model-as-participant
            // sampling) is the escape hatch: `--legacy-room`, or implicitly when
            // `--web-port` is set (the web bridge needs the in-process room).
            if cli.legacy_room || cli.web_port.is_some() {
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
            } else if let Err(e) = rozum::tui::attach::run_attach(cli.room).await {
                eprintln!("rozum: {e}");
                std::process::exit(1);
            }
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
            // Default: bridge to the meeting daemon (`meeting.sock`). Set
            // ROZUM_LEGACY_PROXY=1 for the old per-room-socket proxy.
            let legacy = std::env::var_os("ROZUM_LEGACY_PROXY").is_some();
            let res = if legacy {
                rozum::meeting::run_proxy().await
            } else {
                rozum::meeting::daemon_proxy::run_daemon_proxy().await
            };
            if let Err(e) = res {
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
        Some(Command::Gateway {
            port,
            model,
            strategy,
            offline,
            n_ctx,
            enable_thinking,
            draft_model,
            action,
        }) => match action {
            None => {
                // Reasoning models think by default in their chat template; the
                // gateway disables it (clean CC/Codex output) unless --enable-thinking
                // (or ROZUM_ENABLE_THINKING) is set. The native backend reads this env
                // per request when rendering the prompt.
                if enable_thinking {
                    // SAFETY: set before the backend worker thread is spawned.
                    unsafe { std::env::set_var("ROZUM_ENABLE_THINKING", "1") };
                }
                // Speculative decoding: --draft-model sets ROZUM_DRAFT_MODEL, which
                // run_gateway reads (env so the agentic matrix can enable it on the
                // gateway it spawns without passing the flag).
                if let Some(d) = &draft_model {
                    // SAFETY: set before the backend worker thread is spawned.
                    unsafe { std::env::set_var("ROZUM_DRAFT_MODEL", d) };
                }
                apply_cascade_strategy(strategy.as_deref());
                apply_offline(offline);
                let cfg = load_runtime_config_or_exit();
                let Some(model) = join_models(model).or_else(|| cfg.model.clone()) else {
                    eprintln!(
                        "rozum gateway: --model is required to run the daemon \
                         (or set [runtime].model in rozum.toml)"
                    );
                    std::process::exit(2);
                };
                run_gateway(port, model, n_ctx, cfg).await;
            }
            Some(GatewayAction::Status) => run_gateway_status().await,
            Some(GatewayAction::Stop { force }) => run_gateway_stop(force),
            Some(GatewayAction::Switch {
                model,
                n_ctx,
                backend,
            }) => run_gateway_switch(model, n_ctx, backend).await,
            Some(GatewayAction::Reload) => run_gateway_reload().await,
            Some(GatewayAction::Unload) => run_gateway_unload().await,
        },
        Some(Command::Launch {
            model,
            strategy,
            offline,
            port,
            n_ctx,
            dedicated,
            no_model,
            no_channel_wakeup,
            channel_mcp_name,
            no_piggyback,
            backend_url,
            lean,
            no_sandbox,
            mut program,
        }) => {
            apply_cascade_strategy(strategy.as_deref());
            apply_offline(offline);
            apply_lean_flags(&mut program, lean);
            // `--no-sandbox` is sugar for `ROZUM_SANDBOX=0` — keep a single source of
            // truth so `sandbox_workspace()` (which reads the env) stays the only
            // place the jail decision lives. The flag wins; `=0` it explicitly.
            if no_sandbox {
                unsafe { std::env::set_var("ROZUM_SANDBOX", "0") };
            }
            // No-noise principle (docs/specs/model-sandbox.md): when the jail is active,
            // a headless agent runs without per-action approval prompts — the sandbox,
            // not interactive confirmation, is the safety boundary. Must come AFTER the
            // `--no-sandbox` env set so it sees the jail's real on/off state.
            apply_sandbox_autonomy_flags(&mut program);
            let model = join_models(model);
            // Precedence: --channel-mcp-name > ROZUM_CHANNEL_MCP_NAME > "rozum".
            let server_name = channel_mcp_name
                .or_else(|| std::env::var("ROZUM_CHANNEL_MCP_NAME").ok())
                .unwrap_or_else(|| "rozum".to_owned());
            let channels = ChannelWakeup {
                suppressed: no_channel_wakeup,
                server_name,
            };
            // Resolve both wakeup tiers once (Tier-1 flags are probed here, not
            // twice). Piggyback (Tier 3) is the fallback — auto-off when channels
            // (Tier 1) are active, unless forced by `--no-piggyback`/`ROZUM_PIGGYBACK`.
            let wakeup = WakeupPolicy::resolve(&channels, no_piggyback, &program[0]);
            match backend_url {
                // External OpenAI-compatible server (Ollama/vLLM/…): force it,
                // skip the local-model resolution + shared daemon entirely.
                Some(url) => {
                    let Some(model_spec) = model else {
                        eprintln!(
                            "rozum launch: --backend-url requires --model (the upstream model \
                             name, e.g. --model qwen3:8b)"
                        );
                        std::process::exit(2);
                    };
                    run_launch_url(url, model_spec, port, n_ctx, wakeup, program).await;
                }
                None => {
                    run_launch(model, port, n_ctx, dedicated, no_model, wakeup, program).await;
                }
            }
        }
        Some(Command::Models { action }) => {
            run_models(action).await;
        }
        Some(Command::Service { action }) => {
            run_service(action);
        }
        Some(Command::Meetings { action }) => match action {
            MeetingsAction::Start { foreground } => run_meetings_start(foreground).await,
            MeetingsAction::Stop => run_meetings_stop(),
            MeetingsAction::Status => run_meetings_status().await,
            MeetingsAction::Attach { room } => {
                if let Err(e) = rozum::tui::attach::run_attach(room).await {
                    eprintln!("attach error: {e}");
                    std::process::exit(1);
                }
            }
            MeetingsAction::Install => run_meetings_install(),
            MeetingsAction::Uninstall => run_meetings_uninstall(),
            MeetingsAction::Post { text, room, as_display } => {
                run_meetings_post(text, room, as_display).await
            }
            MeetingsAction::Participant {
                model,
                room,
                as_handle,
                reply_policy,
                gateway_url,
                peers,
                persona,
                persona_file,
            } => {
                run_meetings_participant(
                    model,
                    room,
                    as_handle,
                    reply_policy,
                    gateway_url,
                    peers,
                    persona,
                    persona_file,
                )
                .await
            }
        },
        Some(Command::CommitMsg { model, n_ctx }) => run_commit_msg(model, n_ctx).await,
        Some(Command::Mcp { action }) => match action {
            McpAction::Install { agent } => run_mcp_install(&agent),
            McpAction::Uninstall { agent } => run_mcp_uninstall(&agent),
        },
        Some(Command::Identity { action }) => match action {
            IdentityAction::Whoami => run_identity_whoami(),
            IdentityAction::SetName { name } => run_identity_set_name(&name),
        },
        Some(Command::Doctor { web_url, strict }) => run_doctor(web_url, strict).await,
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

async fn run_doctor(web_url: Option<String>, strict: bool) {
    let report = rozum::doctor::run(rozum::doctor::DoctorOptions { web_url, strict }).await;
    print!("{}", report.render());
    if report.should_fail(strict) {
        std::process::exit(1);
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
    use rozum::tui::app::RoomConfig;

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
    if let Err(e) = rozum::tui::run_room(config, false).await {
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

/// Estimate a model's resident RAM **need** for the host residency gate (BUG-003 v2 +
/// smmr, `docs/specs/safe-multi-model-residency.md`): the calibrated
/// [`rozum::model_source::runtime_footprint_bytes`] = weights + KV at `n_ctx` +
/// activation reserve. This is the model's need, which `smmr-A`'s per-process MLX cap
/// then ENFORCES — so a model can neither balloon past it (the uncapped MLX cache grows
/// to ~`total−8 GB` regardless of size, the reboot mechanism) nor be admitted beyond
/// it. The same figure is used for the residency reservation AND the cap, so they
/// match. An unknown model gets a deliberately huge estimate so it only loads when the
/// host is otherwise empty (under-counting is the direction that reboots). Optional
/// `ROZUM_GATEWAY_FOOTPRINT_INFLATE` (default 1.0) pads it for extra conservatism.
///
/// Supersedes v2's weights-only `size×inflate+base` and the smmr interim floor: those
/// over-counted small models to stay safe WITHOUT a cap; with `smmr-A` enforcing the
/// cap, the true min-need is correct and is what makes real co-residency possible.
fn estimate_model_footprint_bytes(model: &str, n_ctx: u32) -> u64 {
    let inflate = std::env::var("ROZUM_GATEWAY_FOOTPRINT_INFLATE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f >= 1.0)
        .unwrap_or(1.0);
    match rozum::models::scan_all_installed()
        .into_iter()
        .find(|m| m.spec == model)
    {
        Some(m) => {
            let fp = rozum::model_source::runtime_footprint_bytes(model, n_ctx, m.size_bytes);
            ((fp as f64) * inflate) as u64
        }
        None => u64::MAX / 4, // unknown size → only admits when nothing else resident
    }
}

/// Reserve host RAM for a model about to load (BUG-003 v2), or exit with a clear
/// message if it would overcommit. Hold the returned guard for as long as the model
/// is resident (binding it at the caller's function scope is enough). Runs the
/// (possibly long) blocking wait off the async runtime. `None` = gate bypassed /
/// unavailable → loading proceeds (the gate is a safety net, not correctness).
async fn acquire_residency_or_exit(model: &str, n_ctx: u32) -> Option<rozum::share::ResidencyGuard> {
    let footprint = estimate_model_footprint_bytes(model, n_ctx);
    let model_owned = model.to_string();
    match tokio::task::spawn_blocking(move || {
        rozum::share::acquire_residency(&model_owned, footprint)
    })
    .await
    {
        Ok(Ok(guard)) => guard,
        Ok(Err(denied)) => {
            let mb = |b: u64| b / 1_048_576;
            let who = if denied.holders.is_empty() {
                "another rozum gateway".to_string()
            } else {
                denied
                    .holders
                    .iter()
                    .map(|(p, m)| format!("pid {p} {m}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            eprintln!(
                "rozum gateway: refusing to load '{model}' (~{} MB) — it would overcommit host RAM. \
                 {} MB already reserved by [{}]; budget {}. Waited {}s.",
                mb(denied.footprint_bytes),
                mb(denied.in_use_bytes),
                who,
                denied
                    .budget_bytes
                    .map(|b| format!("~{} MB", mb(b)))
                    .unwrap_or_else(|| "unknown".into()),
                denied.waited_secs,
            );
            eprintln!(
                "  Loading models past host RAM can panic/reboot the machine (BUG-003). Stop a \
                 resident gateway (`rozum gateway stop`), use a smaller model, raise \
                 ROZUM_GATEWAY_RAM_BUDGET_FRAC, or set ROZUM_ALLOW_CONCURRENT_RESIDENT=1 to override."
            );
            std::process::exit(1);
        }
        // The blocking task itself failed (panic / cancel) — fail open rather than
        // block a legitimate load on the safety net.
        Err(e) => {
            eprintln!("rozum gateway: residency gate unavailable ({e}); proceeding");
            None
        }
    }
}

async fn run_gateway(port: u16, model_spec: String, n_ctx: Option<u32>, cfg: rozum::RuntimeConfig) {
    let n_ctx = resolve_n_ctx(&model_spec, n_ctx.or(cfg.n_ctx));
    let cfg = std::sync::Arc::new(cfg);
    // Host-wide RAM gate: reserve this model's footprint before loading so the
    // resident models can't overcommit host RAM (whole-system OOM → watchdog kernel
    // panic → reboot, BUG-003). Held for this process's lifetime; covers the initial
    // load below plus every lazy reload / `switch` (all same-process).
    let _residency = acquire_residency_or_exit(&model_spec, n_ctx).await;
    // Speculative decoding: if a draft model is configured (`--draft-model` /
    // `ROZUM_DRAFT_MODEL`), build the target+draft pair; else the plain target.
    // Spec: docs/specs/speculative-decoding.md.
    let draft_spec = std::env::var("ROZUM_DRAFT_MODEL")
        .ok()
        .filter(|s| !s.trim().is_empty());
    // `--model cascade[:name]` / a comma-separated list → a CascadeBackend, resolved
    // the SAME way the reload builder does (so the request-surface works from a cold
    // start, not only on lazy reload). A cascade spec takes precedence over spec-decode
    // (a draft pairs with one model, not a cascade of them).
    let backend = match try_cascade_backend(&cfg, &model_spec, n_ctx).await {
        Some(result) => result,
        None => match &draft_spec {
            Some(draft) => build_spec_decode_backend(&cfg, &model_spec, draft.trim(), n_ctx).await,
            None => build_from_config(&cfg, &model_spec, n_ctx).await,
        },
    };
    let backend = match backend {
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
    // Run as a shareable daemon: publish the registry so `rozum launch` clients
    // discover & reuse this model, and idle-exit to free RAM when unused.
    // ROZUM_GATEWAY_IDLE_SECS=0 keeps it up indefinitely (default 900s).
    let idle_secs = std::env::var("ROZUM_GATEWAY_IDLE_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(900);
    let cfg = rozum::gateway::ServeConfig {
        idle_secs: (idle_secs > 0).then_some(idle_secs),
        register_n_ctx: Some(n_ctx),
        // Enable in-place `gateway switch` / lazy `unload` reload: the daemon
        // rebuilds the backend through this same selection chain.
        builder: Some(gateway_backend_builder(std::sync::Arc::clone(&cfg))),
        backend_hint: None,
    };
    if let Err(e) = rozum::gateway::run(backend, port, model_spec, cfg).await {
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

    const KNOWN_FLAGS: &[&str] = &[
        "--model",
        "--port",
        "--n-ctx",
        "--channel-mcp-name",
        "--backend-url",
    ];
    // Value-less flags: pulled to the front without consuming a following arg.
    const KNOWN_BOOL_FLAGS: &[&str] = &[
        "--no-model",
        "--dedicated",
        "--no-channel-wakeup",
        "--no-piggyback",
        "--lean",
        "--no-sandbox",
    ];

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
        if KNOWN_BOOL_FLAGS.iter().any(|f| arg == *f) {
            pulled.push(arg);
        } else if let Some(flag) = KNOWN_FLAGS.iter().find(|f| arg == **f) {
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

/// What `rozum launch` should run the agent against.
enum LaunchTarget {
    /// Run a local model spec, gateway-backed (or `--dedicated`).
    Local(String),
    /// Run no local model: point the agent at its configured upstream Anthropic.
    Anthropic,
}

/// Channel-wakeup launch policy: whether (and under what MCP server name) to
/// register the rozum mcp-proxy as a Claude Code channel so room activity wakes
/// an idle session. Spec: `docs/specs/channel-wakeup.md`.
struct ChannelWakeup {
    suppressed: bool,
    server_name: String,
}

/// The two meeting-room wakeup policies resolved at launch. `channel_flags` are
/// the Tier-1 flags to inject (computed once — `flags_for` both probes
/// `claude --version` and prints, so it must not run twice). `piggyback` is the
/// resolved Tier-3 decision threaded to both the launch-local proxy reader and
/// the agent's mcp-proxy writer. Piggyback is the *fallback* rung: on by default,
/// but auto-off when Tier-1 channels are active (they already wake the agent),
/// unless the operator forces it. Precedence: `--no-piggyback` > `ROZUM_PIGGYBACK`
/// > auto (off iff channels active).
struct WakeupPolicy {
    channel_flags: Option<Vec<String>>,
    piggyback: bool,
}

impl WakeupPolicy {
    /// Resolve the launch-time wakeup policy for `program` (the agent argv[0]).
    fn resolve(channels: &ChannelWakeup, no_piggyback: bool, program: &str) -> Self {
        let channel_flags = channels.flags_for(program);
        let piggyback = resolve_piggyback(
            no_piggyback,
            rozum::meeting::piggyback::env_override(),
            channel_flags.is_some(),
        );
        WakeupPolicy {
            channel_flags,
            piggyback,
        }
    }
}

/// Decide whether Tier-3 piggyback runs, given the `--no-piggyback` flag, the
/// explicit `ROZUM_PIGGYBACK` override (if any), and whether Tier-1 channels are
/// active. Precedence: flag (force off) > env override > auto. Auto makes
/// piggyback the *fallback* — on only when channels are NOT already waking the
/// agent. Spec: `docs/specs/rozum-native-channels.md`.
fn resolve_piggyback(
    no_piggyback: bool,
    env_override: Option<bool>,
    channels_active: bool,
) -> bool {
    if no_piggyback {
        return false;
    }
    env_override.unwrap_or(!channels_active)
}

#[cfg(test)]
mod backend_engine_tests {
    use super::{is_mlx_server_engine, reorder_launch_args};

    #[test]
    fn backend_url_value_flag_hoisted_from_after_program() {
        // `--backend-url URL` placed after the program is pulled (with its value)
        // ahead of the program so clap parses it as a launch flag.
        let got = reorder_launch_args(
            [
                "rozum",
                "launch",
                "claude",
                "--backend-url",
                "http://localhost:11434/v1",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        );
        assert_eq!(
            got,
            vec![
                "rozum",
                "launch",
                "--backend-url",
                "http://localhost:11434/v1",
                "claude"
            ]
        );
    }

    #[test]
    fn no_sandbox_bool_flag_hoisted_from_after_program() {
        // `--no-sandbox` placed after the program is a value-less flag, pulled
        // ahead of the program (without consuming the next arg) so clap parses it.
        let got = reorder_launch_args(
            ["rozum", "launch", "claude", "--no-sandbox", "--lean"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        assert_eq!(
            got,
            vec!["rozum", "launch", "--no-sandbox", "--lean", "claude"]
        );
    }

    #[test]
    fn flags_after_double_dash_stay_with_the_child() {
        // The `--` separator stops reordering: a child-program `--no-sandbox`
        // is left in place, not hoisted into a launch flag.
        let got = reorder_launch_args(
            ["rozum", "launch", "claude", "--", "--no-sandbox"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        );
        assert_eq!(
            got,
            vec!["rozum", "launch", "claude", "--", "--no-sandbox"]
        );
    }

    #[test]
    fn mlx_server_engine_aliases_are_distinct_from_native_mlx() {
        for e in ["mlx-server", "mlx_lm_server", "mlx-lm-server"] {
            assert!(is_mlx_server_engine(e), "{e} should route to mlx_lm.server");
        }
        // Native MLX engine names must NOT route to the Python server.
        for e in ["mlx", "mlx-native", "mlx_lm", "lmstudio", "gguf", "url", ""] {
            assert!(
                !is_mlx_server_engine(e),
                "{e} must not route to mlx_lm.server"
            );
        }
    }
}

#[cfg(test)]
mod wakeup_tests {
    use super::resolve_piggyback;

    #[test]
    fn piggyback_is_the_fallback_rung() {
        // Auto (no flag, no env): on only when Tier-1 channels are NOT active.
        assert!(
            resolve_piggyback(false, None, false),
            "no channels → fallback on"
        );
        assert!(
            !resolve_piggyback(false, None, true),
            "channels active → auto-off (redundant)"
        );
        // `--no-piggyback` always wins, even without channels.
        assert!(!resolve_piggyback(true, None, false));
        assert!(!resolve_piggyback(true, Some(true), true));
        // Explicit env override beats the auto rule (force on despite channels;
        // force off despite no channels).
        assert!(
            resolve_piggyback(false, Some(true), true),
            "ROZUM_PIGGYBACK=1 forces on"
        );
        assert!(
            !resolve_piggyback(false, Some(false), false),
            "ROZUM_PIGGYBACK=0 forces off"
        );
    }
}

impl ChannelWakeup {
    /// The flags to append for `program_name`, or `None` to inject nothing.
    /// Only Claude Code understands the flag, and only builds ≥ 2.1.80 expose it
    /// (research preview). An older `claude` would reject an unknown flag, so we
    /// gate on `claude --version` and degrade silently. The flag is hidden from
    /// `--help`, so version is the reliable probe.
    fn flags_for(&self, program_name: &str) -> Option<Vec<String>> {
        if self.suppressed {
            return None;
        }
        let base = std::path::Path::new(program_name).file_name()?.to_str()?;
        if base != "claude" {
            return None;
        }
        let out = std::process::Command::new(program_name)
            .arg("--version")
            .output()
            .ok()?;
        let version = String::from_utf8_lossy(&out.stdout);
        if !claude_version_supports_channels(&version) {
            eprintln!(
                "rozum launch: claude '{}' predates channel support (need ≥ 2.1.80); \
                 skipping wakeup.",
                version.trim()
            );
            return None;
        }
        eprintln!(
            "rozum launch: channel wakeup on — registering mcp-proxy as 'server:{}'.",
            self.server_name
        );
        Some(vec![
            "--dangerously-load-development-channels".to_owned(),
            format!("server:{}", self.server_name),
        ])
    }
}

/// True if a `claude --version` string ("2.1.172 (Claude Code)") is ≥ 2.1.80,
/// the first build to support channels. Unparseable output → false (degrade).
fn claude_version_supports_channels(version: &str) -> bool {
    let nums: Vec<u32> = version
        .split_whitespace()
        .next()
        .unwrap_or("")
        .split('.')
        .map(|p| p.trim().parse::<u32>().unwrap_or(0))
        .collect();
    match nums.as_slice() {
        [maj, min, patch, ..] => (*maj, *min, *patch) >= (2, 1, 80),
        _ => false,
    }
}

async fn run_launch(
    model: Option<String>,
    port: Option<u16>,
    n_ctx: Option<u32>,
    dedicated: bool,
    no_model: bool,
    wakeup: WakeupPolicy,
    program: Vec<String>,
) {
    let WakeupPolicy {
        channel_flags,
        piggyback,
    } = wakeup;
    let model_spec = match resolve_launch_target(model, no_model).await {
        // No target and none resolvable (non-TTY without --model, or cancelled).
        None => std::process::exit(2),
        Some(LaunchTarget::Anthropic) => {
            eprintln!(
                "rozum launch: no local model — running against your configured \
                 Anthropic credentials."
            );
            // No gateway, proxy, lease, or model env: the agent uses its own auth.
            exec_agent_anthropic(program, channel_flags).await; // -> ! (execs + exits)
        }
        Some(LaunchTarget::Local(m)) => m,
    };
    let n_ctx = resolve_n_ctx(&model_spec, n_ctx);

    if dedicated {
        let wakeup = WakeupPolicy {
            channel_flags,
            piggyback,
        };
        run_launch_dedicated(model_spec, port, n_ctx, wakeup, program).await;
        return; // unreachable: the dedicated path execs + exits
    }

    // Shared path: discover & reuse a running gateway, or spawn a detached daemon.
    let (gw_port, effective_model) = match ensure_shared_gateway(&model_spec, n_ctx, port).await {
        Some(x) => x,
        None => std::process::exit(1),
    };
    // Hold a lease so the daemon knows a client is using it (and idle-exits only
    // when none remain). The lease goes stale and is reaped after we exit.
    let me_pid = std::process::id();
    rozum::share::touch_lease(me_pid);
    spawn_lease_heartbeat(me_pid);

    // Failover: while the agent runs, keep the shared daemon alive — if it dies,
    // one launch respawns it on the same port. Spec: shared-gateway-failover.
    spawn_failover_watchdog(effective_model.clone(), n_ctx, gw_port);

    // Launch-local reverse proxy: the agent talks to a per-launch loopback port
    // that forwards to the shared daemon. This is the path later phases use for
    // transparent replay / poison handling / model-swap holds; here it is a
    // transparent pass-through. Spec: shared-gateway-proxy.
    let agent_port = match start_launch_proxy(gw_port, piggyback).await {
        Some(p) => p,
        None => {
            // Couldn't bind a local proxy port — fall back to pointing the agent
            // straight at the daemon (loses replay/poison, but still works).
            eprintln!("rozum launch: proxy unavailable; pointing agent directly at the daemon.");
            gw_port
        }
    };
    exec_agent(
        program,
        &effective_model,
        agent_port,
        channel_flags,
        piggyback,
    )
    .await
}

/// Bind an ephemeral loopback port, start the launch-local reverse proxy on it
/// (forwarding to the shared daemon on `daemon_port`), and return the proxy's
/// port. `piggyback` gates the Tier-3 room-activity reader. The proxy task dies
/// with this process when the agent exits.
async fn start_launch_proxy(daemon_port: u16, piggyback: bool) -> Option<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.ok()?;
    let port = listener.local_addr().ok()?.port();
    tokio::spawn(async move {
        if let Err(e) = rozum::proxy::serve(listener, daemon_port, piggyback).await {
            eprintln!("rozum launch: proxy exited: {e}");
        }
    });
    eprintln!("rozum launch: proxy on :{port} → shared gateway :{daemon_port}");
    Some(port)
}

/// Resolve what `rozum launch` should run the agent against when neither
/// `--model` nor `--no-model` may be given:
///
/// - `--no-model` → `Anthropic` (no local model; upstream credentials).
/// - `--model X` → `Local(X)` (mismatch with a running gateway is handled in
///   `ensure_shared_gateway`: takeover-if-idle, else reuse-with-warning).
/// - omitted + a healthy gateway already running → reuse its model (`Local`).
/// - omitted + nothing running, on a TTY → interactive picker (Anthropic first).
/// - omitted + nothing running, not a TTY → error (scripted launches must pass
///   `--model` or `--no-model`).
///
/// Returns `None` to abort (non-TTY without a choice, or the user cancelled).
async fn resolve_launch_target(model: Option<String>, no_model: bool) -> Option<LaunchTarget> {
    use std::io::IsTerminal;

    if no_model {
        return Some(LaunchTarget::Anthropic);
    }
    if let Some(m) = model {
        return Some(LaunchTarget::Local(m));
    }

    // Omitted: reuse a healthy running gateway's model if there is one.
    if let Some(active) = rozum::share::read_active() {
        if rozum::share::health_ok(active.port).await {
            eprintln!("rozum launch: using running model: {}", active.model);
            return Some(LaunchTarget::Local(active.model));
        }
    }

    // Nothing running. A picker only makes sense on an interactive terminal.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        eprintln!(
            "rozum launch: no --model/--no-model given and no gateway running. Pass \
             --no-model to use upstream Anthropic, or --model, e.g. \
             `rozum launch --model mlx-community:gpt-oss-20b-MXFP4-Q4 claude`."
        );
        return None;
    }

    pick_launch_target_interactive()
}

/// Interactive launch-target picker (TTY only). Lists "Anthropic (cloud)" first
/// — choosing it runs no local model — then locally-cached models (annotated
/// `(cached, <size>)`), then curated downloadable models (annotated
/// `(not cached, ~<size>)`). Selecting a not-cached model re-confirms the
/// download. Returns the chosen target, or `None` if cancelled.
fn pick_launch_target_interactive() -> Option<LaunchTarget> {
    use rozum::models;
    use std::io::Write as _;

    enum Kind {
        Anthropic,
        Model { spec: String, cached: bool },
    }
    struct Choice {
        label: String,
        kind: Kind,
    }

    let installed = models::scan_all_installed();
    let cached: std::collections::HashSet<&str> =
        installed.iter().map(|m| m.spec.as_str()).collect();

    // Offline (`--offline` / ROZUM_OFFLINE): no remote/cloud entries — local models only.
    let offline = is_offline();
    let mut choices: Vec<Choice> = Vec::new();
    if !offline {
        // Anthropic (no local model, no gateway — the agent uses its own credentials).
        choices.push(Choice {
            label: "Anthropic (no local model — agent uses your Anthropic credentials directly)"
                .to_owned(),
            kind: Kind::Anthropic,
        });
        // Hosted models (Anthropic + OpenAI) — selectable as a tier; routed via the HTTP backends.
        for r in models::RECOMMENDED_REMOTE {
            choices.push(Choice {
                label: format!("{}  (cloud · {})  — {}", r.spec, r.provider, r.display_name),
                kind: Kind::Model {
                    spec: r.spec.to_owned(),
                    cached: true,
                },
            });
        }
    }
    for m in &installed {
        choices.push(Choice {
            label: format!(
                "{}  (local, cached, {})",
                m.spec,
                models::format_size(m.size_bytes)
            ),
            kind: Kind::Model {
                spec: m.spec.clone(),
                cached: true,
            },
        });
    }
    for r in models::RECOMMENDED {
        if !cached.contains(r.spec) {
            choices.push(Choice {
                label: format!(
                    "{}  (local, not cached, ~{:.1} GB)  — {}",
                    r.spec, r.approx_size_gb, r.display_name
                ),
                kind: Kind::Model {
                    spec: r.spec.to_owned(),
                    cached: false,
                },
            });
        }
    }

    eprintln!(
        "Select what to launch the agent against{}:",
        if offline {
            " (offline — cloud models hidden)"
        } else {
            ""
        }
    );
    for (i, c) in choices.iter().enumerate() {
        eprintln!("  {:>2}) {}", i + 1, c.label);
    }
    eprintln!("Tip: pick several (e.g. \"2 9 4\") to run them as a cascade — rozum orders them");
    eprintln!("     cheapest→most-capable and escalates only when needed.");
    eprint!("Enter number(s) [1-{}] (q to cancel): ", choices.len());
    let _ = std::io::stderr().flush();

    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return None;
    }
    let line = line.trim();
    if line.is_empty() || line.eq_ignore_ascii_case("q") {
        eprintln!("cancelled.");
        return None;
    }

    // Parse one or more indices (space/comma separated). Duplicates collapse, order preserved.
    let mut picks: Vec<usize> = Vec::new();
    for tok in line
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|t| !t.is_empty())
    {
        match tok
            .parse::<usize>()
            .ok()
            .filter(|&n| n >= 1 && n <= choices.len())
        {
            Some(n) if !picks.contains(&n) => picks.push(n),
            Some(_) => {}
            None => {
                eprintln!("rozum launch: '{tok}' is not a valid choice.");
                return None;
            }
        }
    }
    if picks.is_empty() {
        eprintln!("cancelled.");
        return None;
    }

    // Single selection → keep the existing behavior (incl. the Anthropic-only mode + download prompt).
    if picks.len() == 1 {
        return match &choices[picks[0] - 1].kind {
            Kind::Anthropic => Some(LaunchTarget::Anthropic),
            Kind::Model { spec, cached } => {
                if !cached {
                    eprint!("Download {spec} now and use it? [y/N]: ");
                    let _ = std::io::stderr().flush();
                    let mut yn = String::new();
                    let _ = std::io::stdin().read_line(&mut yn);
                    if !yn.trim().eq_ignore_ascii_case("y") {
                        eprintln!("cancelled.");
                        return None;
                    }
                }
                Some(LaunchTarget::Local(spec.clone()))
            }
        };
    }

    // Multiple selections → a cascade. The "Anthropic (no local model)" entry can't be a tier.
    let mut specs: Vec<String> = Vec::new();
    for &p in &picks {
        match &choices[p - 1].kind {
            Kind::Anthropic => {
                eprintln!(
                    "rozum launch: the 'Anthropic (no local model)' option can't be part of a \
                     cascade — pick specific models (e.g. claude-haiku-4-5)."
                );
                return None;
            }
            Kind::Model { spec, .. } => specs.push(spec.clone()),
        }
    }
    // Joined as a comma list → the gateway builds an auto-ordered cascade. Uncached locals download
    // lazily when their tier is first reached.
    eprintln!(
        "Cascade: {} (auto-ordered cheapest→most-capable).",
        specs.join(", ")
    );
    Some(LaunchTarget::Local(specs.join(",")))
}

/// `--backend-url`: serve an external OpenAI-compatible backend (Ollama, vLLM, …)
/// through a lightweight in-process gateway. No local model is loaded and the
/// shared daemon is bypassed — `model_spec` is just the upstream model name
/// forwarded to that server. The gateway dies with this process on the agent's
/// exit, like the `--dedicated` path.
async fn run_launch_url(
    url: String,
    model_spec: String,
    port: Option<u16>,
    n_ctx: Option<u32>,
    wakeup: WakeupPolicy,
    program: Vec<String>,
) -> ! {
    let WakeupPolicy {
        channel_flags,
        piggyback,
    } = wakeup;
    let _ = n_ctx; // informational for a remote backend; the upstream owns its KV
    let port = port.unwrap_or_else(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .ok()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(rozum::share::DEFAULT_GATEWAY_PORT)
    });
    let backend = rozum::concurrency::admit_wrap(std::sync::Arc::new(
        rozum::openai_http::OpenAiHttpBackend::new(&url, &model_spec),
    ) as std::sync::Arc<dyn rozum::ChatBackend>);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("rozum launch: failed to bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!(
        "rozum launch  (backend-url)  gateway=http://127.0.0.1:{port}  → {url}  model={model_spec}"
    );
    let model_for_task = model_spec.clone();
    tokio::spawn(async move {
        if let Err(e) =
            rozum::gateway::serve_on(backend, listener, model_for_task, Default::default()).await
        {
            eprintln!("gateway error: {e}");
        }
    });
    exec_agent(program, &model_spec, port, channel_flags, piggyback).await
}

/// Pre-sharing behaviour: load a private model in-process for just this launch.
async fn run_launch_dedicated(
    model_spec: String,
    port: Option<u16>,
    n_ctx: u32,
    wakeup: WakeupPolicy,
    program: Vec<String>,
) {
    let WakeupPolicy {
        channel_flags,
        piggyback,
    } = wakeup;
    let port = port.unwrap_or_else(|| {
        std::net::TcpListener::bind("127.0.0.1:0")
            .ok()
            .and_then(|l| l.local_addr().ok())
            .map(|a| a.port())
            .unwrap_or(rozum::share::DEFAULT_GATEWAY_PORT)
    });
    // Same host-wide RAM gate as `run_gateway` (BUG-003): a dedicated in-process
    // model reserves its footprint so it can't overcommit host RAM next to another
    // resident gateway. Held for this launch's lifetime (drops when `exec_agent` returns).
    let _residency = acquire_residency_or_exit(&model_spec, n_ctx).await;
    let backend = match build_gateway_backend(&model_spec, n_ctx).await {
        Some(b) => b,
        None => {
            print_no_backend_hints(&model_spec);
            std::process::exit(1);
        }
    };
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("rozum launch: failed to bind 127.0.0.1:{port}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("rozum launch  (dedicated)  gateway=http://127.0.0.1:{port}  model={model_spec}");
    let model_for_task = model_spec.clone();
    tokio::spawn(async move {
        if let Err(e) =
            rozum::gateway::serve_on(backend, listener, model_for_task, Default::default()).await
        {
            eprintln!("gateway error: {e}");
        }
    });
    // The in-process gateway dies with this process on exec_agent's exit.
    exec_agent(program, &model_spec, port, channel_flags, piggyback).await
}

/// Reuse a healthy running shared gateway, else spawn a detached `rozum gateway`
/// daemon and wait for it to come up. Returns `(port, effective_model)` — the
/// effective model may differ from `model_spec` if a gateway for another model is
/// already running (MVP: reuse it with a warning rather than load a second model).
async fn ensure_shared_gateway(
    model_spec: &str,
    n_ctx: u32,
    port: Option<u16>,
) -> Option<(u16, String)> {
    use rozum::share;
    let want_port = port.unwrap_or(share::DEFAULT_GATEWAY_PORT);

    // 1/2. A healthy running gateway → reuse it, or take it over if it serves a
    //      different model and no other client is attached (idle).
    if let Some(active) = share::read_active() {
        if share::health_ok(active.port).await {
            if share::is_reusable(&active, model_spec) {
                eprintln!(
                    "rozum launch: reusing shared gateway on :{} (model {})",
                    active.port, active.model
                );
                return Some((active.port, active.model));
            }
            // Different model. Takeover only when no other launch holds a lease —
            // a single resident model can't host two, and stealing a model out
            // from under a live client would break their session.
            let clients = share::live_lease_count(share::LEASE_FRESH_SECS);
            if clients == 0 {
                eprintln!(
                    "rozum launch: gateway on :{} is idle (model '{}'); taking it over for '{}'…",
                    active.port, active.model, model_spec
                );
                let _ = std::process::Command::new("kill")
                    .arg(active.pid.to_string())
                    .status();
                share::remove_active_if_mine(active.pid);
                // Wait for the port to free before respawning on it.
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                while share::health_ok(active.port).await {
                    if std::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                // fall through to the spawn path below
            } else {
                eprintln!(
                    "rozum launch: gateway already running model '{}' for {clients} client(s); \
                     using it and ignoring '{}'. Use --dedicated for a private model, or \
                     `rozum gateway switch`.",
                    active.model, model_spec
                );
                return Some((active.port, active.model));
            }
        }
    }

    // 3. Nothing usable → spawn a detached daemon and wait for health.
    eprintln!("rozum launch: starting shared gateway for '{model_spec}' on :{want_port}…");
    let mut child = match spawn_detached_gateway(model_spec, want_port, n_ctx) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rozum launch: failed to spawn gateway daemon: {e}");
            return None;
        }
    };
    // Poll for health; fail fast if the daemon process exits (load error).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if share::health_ok(want_port).await {
            return Some((want_port, model_spec.to_string()));
        }
        if let Ok(Some(status)) = child.try_wait() {
            eprintln!(
                "rozum launch: gateway daemon exited before becoming ready ({status}); \
                 see {}",
                share::gateway_dir().join("gateway.log").display()
            );
            return None;
        }
        if std::time::Instant::now() >= deadline {
            eprintln!(
                "rozum launch: gateway not ready after 300s (still downloading?); \
                 see {}",
                share::gateway_dir().join("gateway.log").display()
            );
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

async fn run_meetings_start(foreground: bool) {
    use rozum::meeting::daemon::{daemon_alive, serve_daemon};
    use rozum::meeting::registry::RoomRegistry;
    use rozum::meeting::room_path::meeting_sock;
    use rozum::meeting::store::rozum_state_dir;

    let sock = meeting_sock();

    if !foreground {
        if daemon_alive(&sock).await {
            println!("meeting daemon already running ({})", sock.display());
            return;
        }
        match spawn_detached_meetings() {
            Ok(_) => {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
                while std::time::Instant::now() < deadline {
                    if daemon_alive(&sock).await {
                        println!("meeting daemon started ({})", sock.display());
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                eprintln!(
                    "meeting daemon spawned but not ready yet ({})",
                    sock.display()
                );
            }
            Err(e) => eprintln!("failed to spawn meeting daemon: {e}"),
        }
        return;
    }

    // Foreground: this process IS the daemon.
    if daemon_alive(&sock).await {
        eprintln!("meeting daemon already running ({})", sock.display());
        return;
    }
    let state_dir = rozum_state_dir();
    let _ = std::fs::create_dir_all(&state_dir);
    let pid_path = state_dir.join("meetings.pid");
    let _ = std::fs::write(&pid_path, std::process::id().to_string());

    let registry = std::sync::Arc::new(RoomRegistry::new(state_dir));
    if let Err(e) = serve_daemon(&sock, registry).await {
        eprintln!("meeting daemon error: {e}");
    }
    let _ = std::fs::remove_file(&pid_path);
}

fn spawn_detached_meetings() -> std::io::Result<std::process::Child> {
    use rozum::meeting::store::rozum_state_dir;
    use std::process::{Command as StdCommand, Stdio};
    let exe = std::env::current_exe()?;
    let dir = rozum_state_dir().join("meetings");
    let _ = std::fs::create_dir_all(&dir);
    let log = dir.join("meetings.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut cmd = StdCommand::new(exe);
    cmd.arg("meetings")
        .arg("start")
        .arg("--foreground")
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// `rozum meetings post <text>` — one-shot post to a room (project room by default).
/// Auto-spawns the daemon if down. Author display = `--as`, else $ROZUM_MEETING_AS, else $USER.
async fn run_meetings_post(text: String, room: Option<String>, as_display: Option<String>) {
    use rozum::meeting::daemon::daemon_alive;
    use rozum::meeting::daemon_proxy::{detect_project, spawn_daemon};
    use rozum::meeting::room_path::meeting_sock;
    use rozum::meeting::tui_client::{PostTarget, post_once};

    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        spawn_daemon().await;
    }
    // Identity: an explicit `--as`/$ROZUM_MEETING_AS label (a hook/agent) posts with that
    // display under an ephemeral token; otherwise this is the human → use the stable local
    // identity (one participant across launches/clients).
    let explicit = as_display
        .or_else(|| std::env::var("ROZUM_MEETING_AS").ok())
        .filter(|s| !s.trim().is_empty());
    let (display, token) = match explicit {
        Some(d) => (d, None),
        None => {
            let id = rozum::meeting::local_identity::load_or_create();
            (id.display, Some(id.token))
        }
    };
    // Room precedence: explicit --room, then a configured shared room (ROZUM_MEETING_ROOM, so
    // hook posts land where the agents are), then the cwd project's room.
    let shared = std::env::var("ROZUM_MEETING_ROOM")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let target = match (room, shared) {
        (Some(name), _) => PostTarget::Named(name),
        (None, Some(name)) => PostTarget::Shared(name),
        (None, None) => match detect_project() {
            Some(p) => PostTarget::Project(p),
            None => {
                eprintln!("meetings post: no project detected — run inside a repo, or pass --room");
                std::process::exit(1);
            }
        },
    };
    match post_once(&sock, target, &display, token.as_deref(), &text).await {
        Ok(room) => eprintln!("posted to '{room}' as {display}"),
        Err(e) => {
            eprintln!("meetings post: {e}");
            std::process::exit(1);
        }
    }
}

/// `rozum meetings participant` — run a local model as a live room participant.
#[allow(clippy::too_many_arguments)]
async fn run_meetings_participant(
    model: String,
    room: String,
    as_handle: Option<String>,
    reply_policy: String,
    gateway_url: String,
    peers: Vec<String>,
    persona: Option<String>,
    persona_file: Option<std::path::PathBuf>,
) {
    use rozum::meeting::model_participant::{ReplyPolicy, derive_handle, run};
    let policy: ReplyPolicy = match reply_policy.parse() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("meetings participant: {e}");
            std::process::exit(1);
        }
    };
    let handle = as_handle
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| derive_handle(&model));
    // --persona-file takes precedence over inline --persona.
    let persona = match persona_file {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("meetings participant: --persona-file {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        None => persona,
    };
    if let Err(e) = run(model, room, handle, policy, gateway_url, peers, persona).await {
        eprintln!("meetings participant: {e}");
        std::process::exit(1);
    }
}

fn run_identity_whoami() {
    use rozum::meeting::local_identity;
    let id = local_identity::load_or_create();
    let path = local_identity::identity_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "(no config dir)".into());
    println!("display: {}", id.display);
    println!("token:   {}", id.token);
    println!("file:    {path}");
    println!("(Your stable local identity — every local meeting client of yours maps to it.)");
}

fn run_identity_set_name(name: &str) {
    match rozum::meeting::local_identity::set_display(name) {
        Ok(id) => println!("display name set to '{}' (stable token kept).", id.display),
        Err(e) => {
            eprintln!("identity set-name: {e}");
            std::process::exit(1);
        }
    }
}

/// The agents whose MCP config `rozum mcp install` can manage non-interactively (each owns a
/// native `mcp add`/`mcp remove`). `opencode`'s `mcp add` is interactive, so it's guidance-only.
const MCP_AGENTS: &[&str] = &["claude", "codex"];

/// Expand the `--agent` selector into concrete agent names. `all` → every supported agent.
fn expand_mcp_agents(agent: &str) -> Vec<String> {
    match agent.trim().to_lowercase().as_str() {
        "all" | "" => MCP_AGENTS.iter().map(|s| s.to_string()).collect(),
        other => vec![other.to_string()],
    }
}

/// The `<agent> mcp add` invocation that registers `rozum mcp-proxy` (user scope), or `None`
/// for an agent without a non-interactive add (e.g. opencode). Pure — unit-tested.
fn mcp_add_spec(agent: &str, rozum: &str) -> Option<(&'static str, Vec<String>)> {
    let r = rozum.to_string();
    match agent {
        // claude mcp add --scope user rozum -- <rozum> mcp-proxy
        "claude" => Some((
            "claude",
            vec![
                "mcp".into(), "add".into(), "--scope".into(), "user".into(),
                "rozum".into(), "--".into(), r, "mcp-proxy".into(),
            ],
        )),
        // codex mcp add rozum -- <rozum> mcp-proxy
        "codex" => Some((
            "codex",
            vec!["mcp".into(), "add".into(), "rozum".into(), "--".into(), r, "mcp-proxy".into()],
        )),
        _ => None,
    }
}

/// The `<agent> mcp remove` invocation, or `None` for an unmanaged agent. Pure — unit-tested.
fn mcp_remove_spec(agent: &str) -> Option<(&'static str, Vec<String>)> {
    match agent {
        "claude" => Some((
            "claude",
            vec!["mcp".into(), "remove".into(), "--scope".into(), "user".into(), "rozum".into()],
        )),
        "codex" => Some(("codex", vec!["mcp".into(), "remove".into(), "rozum".into()])),
        _ => None,
    }
}

fn rozum_exe() -> String {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "rozum".into())
}

fn run_mcp_install(agent: &str) {
    let rozum = rozum_exe();
    for a in expand_mcp_agents(agent) {
        let Some((prog, args)) = mcp_add_spec(&a, &rozum) else {
            println!(
                "  {a}: no non-interactive `mcp add` — register manually: its MCP server \
                 `rozum` = command `{rozum} mcp-proxy` (e.g. `opencode mcp add`)."
            );
            continue;
        };
        // Idempotent: drop any prior registration first (ignore failure), then add fresh.
        if let Some((rp, ra)) = mcp_remove_spec(&a) {
            let _ = std::process::Command::new(rp).args(&ra).output();
        }
        match std::process::Command::new(prog).args(&args).output() {
            Ok(o) if o.status.success() => {
                println!("  {a}: registered `rozum` mcp-proxy (user scope).");
            }
            Ok(o) => println!(
                "  {a}: `{prog} mcp add` failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!("  {a}: cannot run `{prog}` ({e}) — is it installed + on PATH?"),
        }
    }
    println!(
        "Done. Bare agents now auto-join their project's room via `rozum mcp-proxy`, which posts \
         a `joined:` presence line on join and `left:` on exit — under the agent's own handle, for \
         every agent, with no settings.json edits. (Run the daemon with `rozum meetings start`, or \
         it auto-spawns.)"
    );
}

fn run_mcp_uninstall(agent: &str) {
    for a in expand_mcp_agents(agent) {
        let Some((prog, args)) = mcp_remove_spec(&a) else {
            println!("  {a}: remove the `rozum` MCP server manually.");
            continue;
        };
        match std::process::Command::new(prog).args(&args).output() {
            Ok(o) if o.status.success() => println!("  {a}: removed `rozum` mcp-proxy."),
            Ok(o) => println!(
                "  {a}: `{prog} mcp remove` failed (maybe not registered): {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => println!("  {a}: cannot run `{prog}` ({e})."),
        }
    }
}

#[cfg(test)]
mod mcp_install_tests {
    use super::*;

    #[test]
    fn expand_all_lists_managed_agents() {
        assert_eq!(expand_mcp_agents("all"), vec!["claude", "codex"]);
        assert_eq!(expand_mcp_agents("codex"), vec!["codex"]);
    }

    #[test]
    fn add_spec_per_agent_and_skips_opencode() {
        let (prog, args) = mcp_add_spec("claude", "/abs/rozum").unwrap();
        assert_eq!(prog, "claude");
        assert!(args.windows(2).any(|w| w == ["--scope", "user"]), "claude is user-scoped");
        assert!(args.contains(&"/abs/rozum".to_string()), "the rozum path is the registered command");
        assert_eq!(args.last().unwrap(), "mcp-proxy");
        let (cprog, cargs) = mcp_add_spec("codex", "/abs/rozum").unwrap();
        assert_eq!(cprog, "codex");
        assert_eq!(cargs.last().unwrap(), "mcp-proxy");
        // opencode's `mcp add` is interactive → no non-interactive spec.
        assert!(mcp_add_spec("opencode", "/abs/rozum").is_none());
    }

    #[test]
    fn remove_spec_matches_managed_agents() {
        assert_eq!(mcp_remove_spec("claude").unwrap().0, "claude");
        assert_eq!(mcp_remove_spec("codex").unwrap().0, "codex");
        assert!(mcp_remove_spec("opencode").is_none());
    }
}

fn run_meetings_stop() {
    use rozum::meeting::room_path::meeting_sock;
    use rozum::meeting::store::rozum_state_dir;
    let pid_path = rozum_state_dir().join("meetings.pid");
    let Ok(pid_s) = std::fs::read_to_string(&pid_path) else {
        println!("no meeting daemon pidfile; not running?");
        return;
    };
    let pid = pid_s.trim();
    match std::process::Command::new("kill").arg(pid).status() {
        Ok(s) if s.success() => {
            println!("stopped meeting daemon (pid {pid})");
            let _ = std::fs::remove_file(&pid_path);
            let _ = std::fs::remove_file(meeting_sock());
        }
        Ok(s) => eprintln!("kill {pid} exited with {s}"),
        Err(e) => eprintln!("failed to signal pid {pid}: {e}"),
    }
}

async fn run_meetings_status() {
    use rozum::meeting::daemon::{daemon_alive, daemon_rooms};
    use rozum::meeting::room_path::meeting_sock;
    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        println!("meeting daemon: not running ({})", sock.display());
        return;
    }
    println!("meeting daemon: running ({})", sock.display());
    match daemon_rooms(&sock).await {
        Ok(rooms) if rooms.is_empty() => println!("  (no rooms yet)"),
        Ok(rooms) => {
            for (name, project) in rooms {
                println!("  {name}   project: {}", project.as_deref().unwrap_or("-"));
            }
        }
        Err(e) => eprintln!("  rooms.list failed: {e}"),
    }
}

fn meetings_service_spec() -> (String, Vec<String>) {
    let program = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "rozum".into());
    (
        program,
        vec!["meetings".into(), "start".into(), "--foreground".into()],
    )
}

fn ensure_meetings_log_dir() {
    let dir = rozum::meeting::store::rozum_state_dir().join("meetings");
    let _ = std::fs::create_dir_all(dir);
}

#[cfg(target_os = "macos")]
fn run_meetings_install() {
    let (program, args) = meetings_service_spec();
    let plist = rozum::service::meetings_launchd_plist(&program, &args, &[]);
    let path = rozum::service::meetings_launchd_plist_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    ensure_meetings_log_dir();
    if let Err(e) = std::fs::write(&path, plist) {
        eprintln!("rozum meetings install: write {}: {e}", path.display());
        std::process::exit(1);
    }
    let ps = path.to_string_lossy();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &ps])
        .status(); // idempotent
    let st = std::process::Command::new("launchctl")
        .args(["load", "-w", &ps])
        .status();
    report_status(
        st,
        &format!(
            "installed + started launchd meeting service → {}",
            path.display()
        ),
    );
}

#[cfg(target_os = "macos")]
fn run_meetings_uninstall() {
    let path = rozum::service::meetings_launchd_plist_path();
    let ps = path.to_string_lossy();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &ps])
        .status();
    let _ = std::fs::remove_file(&path);
    println!("uninstalled launchd meeting service ({})", path.display());
}

#[cfg(not(target_os = "macos"))]
fn run_meetings_install() {
    let (program, args) = meetings_service_spec();
    let unit = rozum::service::meetings_systemd_unit(&program, &args, &[]);
    let path = rozum::service::meetings_systemd_unit_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    ensure_meetings_log_dir();
    if let Err(e) = std::fs::write(&path, unit) {
        eprintln!("rozum meetings install: write {}: {e}", path.display());
        std::process::exit(1);
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let st = std::process::Command::new("systemctl")
        .args([
            "--user",
            "enable",
            "--now",
            rozum::service::MEETINGS_SYSTEMD_UNIT,
        ])
        .status();
    report_status(
        st,
        &format!(
            "installed + started systemd --user meeting service → {}",
            path.display()
        ),
    );
}

#[cfg(not(target_os = "macos"))]
fn run_meetings_uninstall() {
    let _ = std::process::Command::new("systemctl")
        .args([
            "--user",
            "disable",
            "--now",
            rozum::service::MEETINGS_SYSTEMD_UNIT,
        ])
        .status();
    let path = rozum::service::meetings_systemd_unit_path();
    let _ = std::fs::remove_file(&path);
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!("uninstalled systemd meeting service ({})", path.display());
}

async fn run_gateway_status() {
    use rozum::share;
    let Some(active) = share::read_active() else {
        println!("no shared gateway running");
        return;
    };
    let healthy = share::health_ok(active.port).await;
    let uptime = share::now_unix().saturating_sub(active.started_at);
    let clients = share::live_lease_count(share::LEASE_FRESH_SECS);
    println!(
        "shared gateway: {}",
        if healthy {
            "running"
        } else {
            "STALE (not responding)"
        }
    );
    println!("  model:   {}", active.model);
    println!("  port:    {}", active.port);
    println!("  pid:     {}", active.pid);
    println!("  n_ctx:   {}", active.n_ctx);
    println!("  gen:     {}", active.generation);
    println!("  uptime:  {uptime}s");
    println!("  clients: {clients}");
}

fn run_gateway_stop(force: bool) {
    use rozum::share;
    let Some(active) = share::read_active() else {
        println!("no shared gateway running");
        return;
    };
    let clients = share::live_lease_count(share::LEASE_FRESH_SECS);
    if clients > 0 && !force {
        eprintln!(
            "rozum gateway stop: {clients} client(s) attached; refusing. Use --force to stop anyway."
        );
        std::process::exit(1);
    }
    // SIGTERM via `kill`; the daemon's stale registry is harmless (health probe
    // treats a dead port as "none running").
    let status = std::process::Command::new("kill")
        .arg(active.pid.to_string())
        .status();
    match status {
        Ok(s) if s.success() => {
            share::remove_active_if_mine(active.pid);
            println!("stopped shared gateway (pid {})", active.pid);
        }
        _ => {
            eprintln!("rozum gateway stop: failed to signal pid {}", active.pid);
            std::process::exit(1);
        }
    }
}

/// POST a control command to the running shared gateway and return its JSON
/// reply. Sends the bearer token if `ROZUM_GATEWAY_TOKEN` is set (same as the
/// daemon expects). No request timeout — a `switch` reload can take a while.
async fn gateway_control_post(
    path: &str,
    body: serde_json::Value,
) -> Result<serde_json::Value, String> {
    use rozum::share;
    let Some(active) = share::read_active() else {
        return Err("no shared gateway running".into());
    };
    let url = format!("http://127.0.0.1:{}{}", active.port, path);
    let mut req = reqwest::Client::new().post(&url).json(&body);
    if let Ok(token) = std::env::var("ROZUM_GATEWAY_TOKEN") {
        req = req.bearer_auth(token);
    }
    let resp = req.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        Ok(json)
    } else {
        let msg = json
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("control request failed");
        Err(format!("{status}: {msg}"))
    }
}

async fn run_gateway_switch(model: String, n_ctx: Option<u32>, backend: Option<String>) {
    let mut body = serde_json::json!({ "model": model });
    if let Some(n) = n_ctx {
        body["n_ctx"] = serde_json::json!(n);
    }
    if let Some(b) = &backend {
        body["backend"] = serde_json::json!(b);
    }
    match gateway_control_post("/control/switch", body).await {
        Ok(j) => println!(
            "switched to {} (generation {})",
            model,
            j.get("generation").and_then(|g| g.as_u64()).unwrap_or(0)
        ),
        Err(e) => {
            eprintln!("rozum gateway switch: {e}");
            std::process::exit(1);
        }
    }
}

async fn run_gateway_reload() {
    match gateway_control_post("/control/reload", serde_json::json!({})).await {
        Ok(_) => println!("gateway reloading (re-exec from current binary)"),
        Err(e) => {
            eprintln!("rozum gateway reload: {e}");
            std::process::exit(1);
        }
    }
}

async fn run_gateway_unload() {
    match gateway_control_post("/control/unload", serde_json::json!({})).await {
        Ok(j) => println!(
            "model unloaded (generation {}); next request reloads it",
            j.get("generation").and_then(|g| g.as_u64()).unwrap_or(0)
        ),
        Err(e) => {
            eprintln!("rozum gateway unload: {e}");
            std::process::exit(1);
        }
    }
}

/// Heartbeat this launch's lease so the shared daemon counts it as a live client.
fn spawn_lease_heartbeat(pid: u32) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            rozum::share::touch_lease(pid);
        }
    });
}

/// Background failover: watch the shared gateway and respawn it if it dies, so
/// clients only see a brief reconnect window. A spawn lock keeps simultaneous
/// watchdogs from each respawning (port-bind dedups them anyway). Dies with this
/// process when the agent exits.
fn spawn_failover_watchdog(model: String, n_ctx: u32, port: u16) {
    use rozum::share;
    use std::time::{Duration, Instant};
    tokio::spawn(async move {
        let mut misses = 0u32;
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if share::health_ok(port).await {
                misses = 0;
                continue;
            }
            misses += 1;
            if misses < 2 {
                continue; // tolerate a transient blip before acting
            }
            // Daemon looks down. Coordinate a single respawn; others wait.
            match share::try_spawn_lock(120) {
                Some(_lock) => {
                    if share::health_ok(port).await {
                        misses = 0;
                        continue; // recovered under the lock
                    }
                    eprintln!("rozum launch: shared gateway down — respawning on :{port}…");
                    match spawn_detached_gateway(&model, port, n_ctx) {
                        Ok(mut child) => {
                            let deadline = Instant::now() + Duration::from_secs(120);
                            while !share::health_ok(port).await {
                                if matches!(child.try_wait(), Ok(Some(_)))
                                    || Instant::now() >= deadline
                                {
                                    break; // died or timed out — next loop retries
                                }
                                tokio::time::sleep(Duration::from_millis(500)).await;
                            }
                        }
                        Err(e) => eprintln!("rozum launch: respawn failed: {e}"),
                    }
                }
                None => { /* another launch is respawning — wait and re-poll */ }
            }
            misses = 0;
        }
    });
}

/// Spawn `rozum gateway --model … --port … --n-ctx …` as a detached process that
/// outlives this launch (own process group, stdio to a log file).
fn spawn_detached_gateway(
    model_spec: &str,
    port: u16,
    n_ctx: u32,
) -> std::io::Result<std::process::Child> {
    use std::process::{Command as StdCommand, Stdio};
    let exe = std::env::current_exe()?;
    let _ = rozum::share::ensure_dir();
    let log = rozum::share::gateway_dir().join("gateway.log");
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;
    let mut cmd = StdCommand::new(exe);
    cmd.arg("gateway")
        .arg("--model")
        .arg(model_spec)
        .arg("--port")
        .arg(port.to_string())
        .arg("--n-ctx")
        .arg(n_ctx.to_string())
        // Born from a `rozum launch`: shut down immediately once the last client
        // lease drops, even if the watchdog never polled while a lease was live.
        .env("ROZUM_GATEWAY_LAUNCH_MANAGED", "1")
        .stdin(Stdio::null())
        .stdout(log_file.try_clone()?)
        .stderr(log_file);
    // Own process group so a Ctrl-C / terminal close on the launch doesn't kill
    // the shared daemon.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// The launch-env var **names** forwarded into a Docker-backend jail (`docker run
/// -e NAME` forwards each value from this process's env, which `exec_agent` sets).
/// Superset of every key `exec_agent` + `apply_rozum_agent_env` may set; a name
/// that isn't set is a harmless no-op. The Seatbelt backend ignores this (the child
/// shares this process's env directly).
const SANDBOX_FORWARD_ENV: &[&str] = &[
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY",
    "OPENAI_BASE_URL",
    "OPENAI_API_KEY",
    "ROZUM_GATEWAY_URL",
    "ROZUM_PIGGYBACK",
    "OPENCODE_CONFIG",
    "CLAUDE_CODE_DISABLE_BUNDLED_SKILLS",
    "CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS",
    "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
    "CLAUDE_CODE_ATTRIBUTION_HEADER",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
    "DISABLE_NON_ESSENTIAL_MODEL_CALLS",
];

/// Load the `[sandbox]` table from `rozum.toml` (docs/specs/model-sandbox.md "Config
/// surface"). A missing/malformed config yields the empty default — it must never break
/// the jail. Read once per call in the unsandboxed launcher; the file is tiny.
fn sandbox_config() -> rozum::SandboxConfig {
    rozum::RuntimeConfig::load().map(|c| c.sandbox).unwrap_or_default()
}

/// The active sandbox backend: `ROZUM_SANDBOX_BACKEND` env wins, else `[sandbox] backend`,
/// else the default (Seatbelt).
fn resolve_sandbox_backend(sbx: &rozum::SandboxConfig) -> rozum::sandbox::SandboxBackend {
    use rozum::sandbox::SandboxBackend;
    if std::env::var_os("ROZUM_SANDBOX_BACKEND").is_some() {
        SandboxBackend::from_env()
    } else if let Some(b) = &sbx.backend {
        SandboxBackend::parse(b)
    } else {
        SandboxBackend::default()
    }
}

/// The active network policy: `ROZUM_SANDBOX_NETWORK` env wins, else `[sandbox] network`,
/// else the default (GatewayOnly).
fn resolve_sandbox_network(sbx: &rozum::SandboxConfig) -> rozum::sandbox::NetPolicy {
    use rozum::sandbox::NetPolicy;
    if std::env::var_os("ROZUM_SANDBOX_NETWORK").is_some() {
        NetPolicy::from_env()
    } else if let Some(n) = &sbx.network {
        NetPolicy::parse(n)
    } else {
        NetPolicy::default()
    }
}

/// Resolve a `[sandbox] workspace` token to a path: `"."`/empty → the launch cwd,
/// `"~/…"` → `$HOME`-relative, else verbatim.
fn resolve_workspace_token(tok: &str) -> std::path::PathBuf {
    use std::path::PathBuf;
    if tok.is_empty() || tok == "." {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else if let Some(rest) = tok.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(|h| PathBuf::from(h).join(rest))
            .unwrap_or_else(|| PathBuf::from(tok))
    } else {
        PathBuf::from(tok)
    }
}

/// The primary workspace dir to jail a launched agent in, or `None` for no jail, for the
/// resolved `backend`. **Default ON** (the launch cwd); `ROZUM_SANDBOX=0`/empty disables
/// it, `=1` forces the cwd, `=<dir>` jails to <dir>. The Seatbelt backend is macOS-only,
/// so off macOS the jail stays OFF *unless* the Docker backend is selected — so `launch`
/// is never broken by an unavailable jail.
fn sandbox_workspace_for(backend: rozum::sandbox::SandboxBackend) -> Option<std::path::PathBuf> {
    if backend == rozum::sandbox::SandboxBackend::Seatbelt && !cfg!(target_os = "macos") {
        return None;
    }
    let cwd = || std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    match std::env::var("ROZUM_SANDBOX") {
        Ok(s) if s.is_empty() || s == "0" => None, // explicit opt-out
        Ok(s) if s == "1" => Some(cwd()),
        Ok(s) => Some(std::path::PathBuf::from(s)),
        Err(_) => Some(cwd()), // unset → DEFAULT ON
    }
}

/// Whether a jail is active (config-aware backend resolution), for callers that only
/// need the on/off decision (autonomy flags, the gateway-host choice).
fn sandbox_workspace() -> Option<std::path::PathBuf> {
    sandbox_workspace_for(resolve_sandbox_backend(&sandbox_config()))
}

/// The host the agent uses to reach the rozum gateway. Normally the host loopback
/// (`127.0.0.1`); under an **active Docker jail** the container's loopback is itself,
/// so it must reach the host gateway via `host.docker.internal` instead (the
/// `--add-host` alias `to_docker_run_args` emits). Every gateway URL in `exec_agent`
/// derives from this single choke point, so picking the right host here makes all of
/// them (Anthropic/OpenAI base URLs, codex `-c base_url`) correct with no other change.
fn sandbox_gateway_host() -> &'static str {
    let backend = resolve_sandbox_backend(&sandbox_config());
    if backend == rozum::sandbox::SandboxBackend::Docker
        && sandbox_workspace_for(backend).is_some()
    {
        rozum::sandbox::CONTAINER_GATEWAY_HOST
    } else {
        "127.0.0.1"
    }
}

/// Build the base `Command` for an agent child, **jailed by default**. The backend is
/// `ROZUM_SANDBOX_BACKEND` (`seatbelt` default, macOS-only; `docker` = a container on
/// any OS). Writes are confined to the workspace (its cwd) + toolchain caches, secrets
/// are denied/absent, the gateway is reachable but nothing else off-box, and there are
/// NO per-action prompts (docs/specs/model-sandbox.md). `ROZUM_SANDBOX=0` disables it.
/// Used by EVERY agent-exec path so the jail is uniform. For both backends the returned
/// command ends with `program_name`, so the caller's later `.args(...)`/`.env(...)`
/// append to the jailed invocation exactly as for an unsandboxed command.
fn sandboxed_command(program_name: &str) -> std::process::Command {
    use rozum::sandbox::{SandboxBackend, SandboxPolicy};
    use std::process::Command as StdCommand;
    // Merge env + the `[sandbox]` config (env wins on network/backend; the config adds
    // the path lists env can't express — extra workspaces, read-only refs, extra secrets).
    let sbx = sandbox_config();
    let backend = resolve_sandbox_backend(&sbx);
    let Some(primary) = sandbox_workspace_for(backend) else {
        return StdCommand::new(program_name);
    };
    let mut workspaces = vec![primary.clone()];
    workspaces.extend(sbx.workspace.iter().map(|t| resolve_workspace_token(t)));
    let read_only: Vec<std::path::PathBuf> =
        sbx.read_only.iter().map(|t| resolve_workspace_token(t)).collect();
    let extra_secrets: Vec<std::path::PathBuf> =
        sbx.secret_deny.iter().map(|t| resolve_workspace_token(t)).collect();
    let network = resolve_sandbox_network(&sbx);
    if !sbx.workspace.is_empty() || !read_only.is_empty() || !extra_secrets.is_empty() {
        eprintln!(
            "  → sandbox config: +{} workspace(s), {} read-only, +{} secret den(y/ies)",
            sbx.workspace.len(),
            read_only.len(),
            extra_secrets.len()
        );
    }
    let policy =
        SandboxPolicy::rust_coding_with(&workspaces, &read_only, &extra_secrets, network);
    let ws = primary;
    match backend {
        SandboxBackend::Seatbelt => match rozum::sandbox::write_seatbelt_profile_temp(&policy) {
            Ok(profile) => {
                eprintln!(
                    "  → sandboxed (Seatbelt): workspace={} profile={}",
                    ws.display(),
                    profile.display()
                );
                let mut c = StdCommand::new("sandbox-exec");
                c.arg("-f").arg(&profile).arg(program_name);
                c
            }
            Err(e) => {
                eprintln!("  ! sandbox profile write failed ({e}); running UNsandboxed");
                StdCommand::new(program_name)
            }
        },
        SandboxBackend::Docker => {
            // The image must contain the agent CLI (`program_name`) on PATH and, for
            // build tasks, a Rust toolchain. Operator-supplied via
            // ROZUM_SANDBOX_DOCKER_IMAGE. The workspace + toolchain caches are bind
            // mounts; the rest of the host FS is simply absent. env values are
            // forwarded by name (the gateway URLs already use host.docker.internal via
            // `sandbox_gateway_host`); the agent's args are appended after program_name.
            let image = rozum::sandbox::default_docker_image();
            // Preflight: if the image isn't present locally, `docker run` would try to
            // pull it (and fail confusingly for the unpublished default). Print a clear
            // hint instead, then still proceed so behavior stays predictable.
            let present = StdCommand::new("docker")
                .args(["image", "inspect", &image])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if !present {
                eprintln!(
                    "  ! docker image '{image}' not found locally — build it first:\n    \
                     scripts/build-agent-image.sh   (or set ROZUM_SANDBOX_DOCKER_IMAGE)"
                );
            }
            eprintln!(
                "  → sandboxed (Docker): image={image} workspace={} gateway via {}",
                ws.display(),
                rozum::sandbox::CONTAINER_GATEWAY_HOST
            );
            let limits = rozum::sandbox::DockerLimits::from_env();
            let mut c = StdCommand::new("docker");
            c.args(policy.to_docker_run_args(&image, &ws, SANDBOX_FORWARD_ENV, &limits));
            c.arg(program_name);
            c
        }
    }
}

/// Build the agent child command (env wiring) and exec it, exiting with its code.
/// `model_for_alias` is the model the gateway is actually serving.
async fn exec_agent(
    mut program: Vec<String>,
    model_for_alias: &str,
    port: u16,
    channel_flags: Option<Vec<String>>,
    piggyback: bool,
) -> ! {
    // channel-wakeup-launch-flag: append the `--dangerously-load-development-channels`
    // flag for a capable `claude` (resolved once at launch), so a launched agent
    // gets woken on room events.
    if let Some(flags) = channel_flags {
        program.extend(flags);
    }
    let (program_name, args) = program
        .split_first()
        .expect("clap requires at least one arg");
    let claude_alias = rozum::gateway::claude_model_alias(model_for_alias);
    eprintln!("  → running: {} {}", program_name, args.join(" "));

    // Gateway host: the host loopback normally, `host.docker.internal` under an active
    // Docker jail (the container's own loopback isn't the host). Every URL below derives
    // from `base`, so this one choke point makes them all container-correct.
    let base = format!("http://{}:{port}", sandbox_gateway_host());
    // Optionally jail the agent (docs/specs/model-sandbox.md) — `sandboxed_command`
    // returns the `sandbox-exec`/`docker run` wrapper when the jail is on, else a plain
    // command; every later `cmd.args(...)` / `cmd.env(...)` appends to it.
    let mut cmd = sandboxed_command(program_name);
    cmd.args(args);
    cmd.env("ANTHROPIC_BASE_URL", &base);
    cmd.env("ANTHROPIC_AUTH_TOKEN", "rozum-local");
    cmd.env_remove("ANTHROPIC_API_KEY");
    // Tier-3 piggyback: export the launch's decision so the agent's mcp-proxy
    // writer matches the launch-local proxy reader exactly (both on, or both off).
    cmd.env("ROZUM_PIGGYBACK", if piggyback { "1" } else { "0" });
    cmd.env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
    cmd.env("ANTHROPIC_MODEL", &claude_alias);
    cmd.env("ANTHROPIC_DEFAULT_OPUS_MODEL", &claude_alias);
    cmd.env("ANTHROPIC_DEFAULT_SONNET_MODEL", &claude_alias);
    cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", &claude_alias);
    cmd.env("OPENAI_BASE_URL", format!("{base}/v1"));
    cmd.env("OPENAI_API_KEY", "rozum-local");
    cmd.env("ROZUM_GATEWAY_URL", &base);

    // Codex ignores `OPENAI_BASE_URL` — it needs an explicit model provider, and
    // (≥ 0.137) the Responses API. Inject `-c` overrides on top of the user's config
    // (their `~/.codex` is left intact) so `rozum launch codex` just works, like
    // Claude does. Mirrors what the e2e runner sets.
    let is_codex = program_name == "codex" || program_name.ends_with("/codex");
    if is_codex {
        let has_model = args
            .iter()
            .any(|a| a == "-m" || a == "--model" || a.starts_with("--model="));
        cmd.args(codex_provider_flags(&base, has_model));
        // Codex inherits the user's global `model_reasoning_effort` (often `xhigh`), which on a
        // LOCAL model burns long reasoning chains for little gain — measured codex on Qwen3-30B-A3B
        // at 7+ min/task. Default the rozum-launched codex to `medium`; skip if the user sets it.
        if !args.iter().any(|a| a.contains("model_reasoning_effort")) {
            cmd.args(["-c", "model_reasoning_effort=medium"]);
        }
        eprintln!("  → codex: routed at the rozum gateway (model_provider=rozum, wire_api=responses, reasoning=medium)");
    }

    // opencode reads providers from a config file, not env. Write one that adds an
    // OpenAI-compatible `rozum` provider at the gateway and point OPENCODE_CONFIG at it.
    // opencode's own tools (edit/bash/read/…) are built in, so a provider-only config is
    // enough. Default the model to `rozum/local` if the user didn't pass `-m`.
    let is_opencode = program_name == "opencode" || program_name.ends_with("/opencode");
    if is_opencode {
        if let Some(path) = write_opencode_config(&base) {
            cmd.env("OPENCODE_CONFIG", &path);
        }
        let has_model = args
            .iter()
            .any(|a| a == "-m" || a == "--model" || a.starts_with("--model="));
        if !has_model {
            cmd.args(["-m", "rozum/local"]);
        }
        eprintln!("  → opencode: routed at the rozum gateway (provider=rozum, OpenAI-compatible)");
    }

    apply_rozum_agent_env(&mut cmd);
    spawn_agent_and_exit(cmd, program_name).await
}

/// Codex CLI `-c` overrides that point it at the local rozum gateway over the
/// OpenAI Responses API (Codex ignores `OPENAI_BASE_URL`). The gateway ignores the
/// model name, so `-m local` is just a label (only added if the user didn't pass one).
fn codex_provider_flags(base: &str, has_model: bool) -> Vec<String> {
    let mut f = vec![
        "-c".into(),
        "model_provider=rozum".into(),
        "-c".into(),
        "model_providers.rozum.name=\"rozum\"".into(),
        "-c".into(),
        format!("model_providers.rozum.base_url=\"{base}/v1\""),
        "-c".into(),
        "model_providers.rozum.wire_api=\"responses\"".into(),
        "-c".into(),
        "model_providers.rozum.env_key=\"OPENAI_API_KEY\"".into(),
    ];
    if !has_model {
        f.push("-m".into());
        f.push("local".into());
    }
    f
}

/// Write a temp opencode config defining a `rozum` OpenAI-compatible provider that
/// points at the local gateway, and return its path (for `OPENCODE_CONFIG`). The model
/// id is a label the gateway ignores; the user selects it as `-m rozum/local`.
///
/// Written under canonical `/tmp` (e.g. `/private/tmp` on macOS) — NOT `$TMPDIR`. The
/// Docker backend bind-mounts `/tmp` (it's a toolchain path) at the same canonical path,
/// so the file is visible inside the container at its `OPENCODE_CONFIG` path; Docker
/// Desktop does NOT reliably share `$TMPDIR` (`/var/folders`), which left the config
/// invisible. Seatbelt/no-sandbox read it fine from `/tmp` either way.
fn write_opencode_config(base: &str) -> Option<std::path::PathBuf> {
    let cfg = serde_json::json!({
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            "rozum": {
                "npm": "@ai-sdk/openai-compatible",
                "name": "Rozum",
                "options": { "baseURL": format!("{base}/v1"), "apiKey": "rozum-local" },
                "models": { "local": { "name": "local" } }
            }
        }
    });
    let dir = std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    let path = dir.join(format!("rozum-opencode-{}.json", std::process::id()));
    std::fs::write(&path, cfg.to_string()).ok().map(|_| path)
}

/// Launch the agent with no local model: leave its upstream Anthropic auth
/// (`ANTHROPIC_API_KEY` / claude.ai login) untouched and set none of the
/// gateway/model env. Only the rozum agent-context defaults are applied.
async fn exec_agent_anthropic(mut program: Vec<String>, channel_flags: Option<Vec<String>>) -> ! {
    if let Some(flags) = channel_flags {
        program.extend(flags);
    }
    let (program_name, args) = program
        .split_first()
        .expect("clap requires at least one arg");
    eprintln!(
        "  → running: {} {}  (upstream Anthropic)",
        program_name,
        args.join(" ")
    );

    // Jail the agent here too, uniform with the local-model paths (model-sandbox.md).
    let mut cmd = sandboxed_command(program_name);
    cmd.args(args);
    apply_rozum_agent_env(&mut cmd);
    spawn_agent_and_exit(cmd, program_name).await
}

/// Non-coding tools `--lean` strips from a launched `claude` via `--disallowedTools`.
/// A headless coding launch keeps the core (Read/Write/Edit/Bash + any Glob/Grep/LS/
/// MultiEdit), and drops meeting-room (rozum MCP), planning, worktree, cron, task,
/// workflow, skill, notebook, and web tools — they're schema tokens the model pays for on
/// every request and extra ways for a weak model to derail. **Measured** (Qwen3-4B, real
/// `rozum launch claude`): 33 tools / ~4,878 tool-schema tokens → 4 tools / ~761 (−84%).
/// `--allowedTools` is a *permission* whitelist, not a request shaper (it left the count
/// unchanged / higher) — `--disallowedTools` is what actually removes schemas from the
/// request. `mcp__rozum` is a server-level wildcard that drops all rozum MCP tools.
/// Names that aren't present are harmless no-ops, so the list can be a safe superset.
const LEAN_DISALLOW: &[&str] = &[
    "AskUserQuestion",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Task",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TaskUpdate",
    "CronCreate",
    "CronDelete",
    "CronList",
    "EnterPlanMode",
    "ExitPlanMode",
    "EnterWorktree",
    "ExitWorktree",
    "Workflow",
    "Skill",
    "ScheduleWakeup",
    "Agent",
    "LSP",
    "mcp__rozum",
];

/// `--lean`: optimize a launched `claude`'s request for a local model. No-op for non-`claude`
/// programs (codex is already capped to `medium` reasoning unconditionally in `exec_agent`).
/// Two safe levers (CC's system prompt itself is load-bearing and
/// is NOT touched — stripping it breaks the agent; only the tool schemas are pure overhead):
///
///   1. `--exclude-dynamic-system-prompt-sections` — move per-machine bits (cwd, env, **git
///      status**, memory paths) out of the system prompt into the first user message. CC
///      otherwise re-embeds git status in the system prefix, and it changes every time the
///      agent edits a file — busting the prefix-KV cache and forcing a full re-prefill of the
///      ~1.4K-token system+tools block *every turn*. Relocating it keeps the prefix
///      byte-identical → cached across turns. Safe (relocates, removes nothing). Skipped if
///      the operator set their own system prompt.
///   2. `--disallowedTools <LEAN_DISALLOW>` — drop the non-coding tool schemas (33 tools /
///      ~4.9K tokens → 4 / ~0.8K). Variadic flag, so it goes LAST. Skipped if the operator
///      manages the tool set (`--allowedTools`/`--disallowedTools`).
fn apply_lean_flags(program: &mut Vec<String>, lean: bool) {
    if !lean {
        return;
    }
    let Some(p0) = program.first() else { return };
    let is_claude = p0 == "claude" || p0.ends_with("/claude");
    // codex reasoning is capped to `medium` unconditionally in `exec_agent` (local models
    // don't benefit from `xhigh`), so `--lean` has nothing extra to do for codex.
    if !is_claude {
        return;
    }

    // (1) Stabilize the system-prompt prefix for cache reuse.
    let user_handles_sys = program.iter().any(|a| {
        a.starts_with("--system-prompt") || a == "--exclude-dynamic-system-prompt-sections"
    });
    if !user_handles_sys {
        program.push("--exclude-dynamic-system-prompt-sections".into());
    }

    // (2) Strip non-coding tool schemas — variadic flag, must come last.
    let user_manages_tools = program
        .iter()
        .any(|a| a.starts_with("--allowedTools") || a.starts_with("--disallowedTools"));
    if user_manages_tools {
        eprintln!(
            "rozum launch: --lean tool-strip skipped (you pass --allowedTools/--disallowedTools); \
             keeping --exclude-dynamic-system-prompt-sections"
        );
    } else {
        eprintln!(
            "rozum launch: --lean → claude --exclude-dynamic-system-prompt-sections + \
             --disallowedTools (strip {} non-coding tools)",
            LEAN_DISALLOW.len()
        );
        program.push("--disallowedTools".into());
        program.extend(LEAN_DISALLOW.iter().map(|t| (*t).to_string()));
    }
}

/// model-sandbox "No-noise principle" (docs/specs/model-sandbox.md): when the jail is
/// active, a **headless** agent (the model-as-agent case — it cannot answer a prompt,
/// and a denied escalation makes it loop, matrix Finding 1a) runs with per-action
/// approval prompts disabled. The structural sandbox — not interactive confirmation —
/// is the safety boundary, so we can safely say "yes to everything in-bounds". Injects
/// the agent's bypass flag in place; mirrors what the agentic bench passes explicitly.
/// Interactive sessions (the operator is present to answer) are left untouched.
fn apply_sandbox_autonomy_flags(program: &mut Vec<String>) {
    // Only when the jail is actually on — never grant no-prompt autonomy unsandboxed.
    if let Some(flag) = rozum::sandbox::autonomy_flag_for(program, sandbox_workspace().is_some()) {
        eprintln!("  → sandbox autonomy: appending {flag} (jailed → no approval prompts)");
        program.push(flag.into());
    }
}

#[cfg(test)]
mod sandbox_autonomy_tests {
    use rozum::sandbox::autonomy_flag_for;

    fn v(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn injects_only_when_jailed_and_headless() {
        // Jail off → never inject, even headless.
        assert_eq!(autonomy_flag_for(&v(&["claude", "-p", "hi"]), false), None);
        // claude headless under the jail → skip-permissions.
        assert_eq!(
            autonomy_flag_for(&v(&["claude", "-p", "hi"]), true),
            Some("--dangerously-skip-permissions")
        );
        // claude interactive (no -p) → left alone (operator can answer).
        assert_eq!(autonomy_flag_for(&v(&["claude"]), true), None);
        // codex exec → bypass approvals+sandbox.
        assert_eq!(
            autonomy_flag_for(&v(&["codex", "exec", "hi"]), true),
            Some("--dangerously-bypass-approvals-and-sandbox")
        );
        // bare codex (interactive) → left alone.
        assert_eq!(autonomy_flag_for(&v(&["codex"]), true), None);
        // opencode run → skip-permissions; bare opencode (TUI) → left alone.
        assert_eq!(
            autonomy_flag_for(&v(&["opencode", "run", "hi"]), true),
            Some("--dangerously-skip-permissions")
        );
        assert_eq!(autonomy_flag_for(&v(&["opencode"]), true), None);
        // Unknown program → None.
        assert_eq!(autonomy_flag_for(&v(&["aider", "-p"]), true), None);
    }

    #[test]
    fn respects_explicit_user_policy_and_is_idempotent() {
        // Already-present flag → no duplicate.
        assert_eq!(
            autonomy_flag_for(&v(&["claude", "-p", "x", "--dangerously-skip-permissions"]), true),
            None
        );
        // Explicit claude permission mode → don't override.
        assert_eq!(
            autonomy_flag_for(&v(&["claude", "-p", "x", "--permission-mode", "default"]), true),
            None
        );
        // codex with an explicit sandbox/approval choice → don't override.
        assert_eq!(
            autonomy_flag_for(&v(&["codex", "exec", "x", "-s", "workspace-write"]), true),
            None
        );
        assert_eq!(
            autonomy_flag_for(&v(&["codex", "exec", "x", "--ask-for-approval=never"]), true),
            None
        );
        // Resolved-path program names are matched by basename.
        assert_eq!(
            autonomy_flag_for(&v(&["/usr/local/bin/claude", "-p", "x"]), true),
            Some("--dangerously-skip-permissions")
        );
    }
}

#[cfg(test)]
mod opencode_tests {
    use super::write_opencode_config;

    #[test]
    fn opencode_config_points_at_gateway() {
        let p = write_opencode_config("http://127.0.0.1:9999").expect("write config");
        let s = std::fs::read_to_string(&p).expect("read config");
        let v: serde_json::Value = serde_json::from_str(&s).expect("valid json");
        assert_eq!(v["provider"]["rozum"]["options"]["baseURL"], "http://127.0.0.1:9999/v1");
        assert_eq!(v["provider"]["rozum"]["npm"], "@ai-sdk/openai-compatible");
        assert!(v["provider"]["rozum"]["models"]["local"].is_object());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn opencode_config_lives_under_tmp_so_docker_mounts_it() {
        // Must be under canonical /tmp (a toolchain bind mount), NOT $TMPDIR — Docker
        // Desktop doesn't reliably share /var/folders, which left the config invisible
        // in the container. Regression guard for the model-sandbox opencode fix.
        let p = write_opencode_config("http://127.0.0.1:9999").expect("write config");
        let tmp = std::fs::canonicalize("/tmp").unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
        assert!(
            p.starts_with(&tmp),
            "opencode config must live under {} (got {})",
            tmp.display(),
            p.display()
        );
        let _ = std::fs::remove_file(&p);
    }
}

#[cfg(test)]
mod cascade_startup_tests {
    use super::try_cascade_backend;

    // A cascade of two OpenAI-compatible remote tiers: each builds by just constructing
    // the HTTP client (no API key, no network, no model load), so this exercises the
    // gateway STARTUP routing (`try_cascade_backend`) end-to-end without heavy I/O.
    const TOML: &str = "\
        [cascade.test]\n\
        [[cascade.test.tiers]]\n\
        model = \"gpt-4o-mini\"\n\
        location = \"remote\"\n\
        api = \"openai\"\n\
        [[cascade.test.tiers]]\n\
        model = \"gpt-4o\"\n\
        location = \"remote\"\n\
        api = \"openai\"\n";

    #[tokio::test]
    async fn cascade_specs_route_to_a_cascade_at_startup() {
        let cfg = std::sync::Arc::new(rozum::RuntimeConfig::from_toml_str(TOML).unwrap());

        // `cascade:test` → builds the named cascade from rozum.toml (Some(Some(_))).
        assert!(
            matches!(try_cascade_backend(&cfg, "cascade:test", 4096).await, Some(Some(_))),
            "`cascade:test` must build the named cascade at startup"
        );
        // bare `cascade` → the [cascade.default] table; absent here → a cascade spec that
        // fails to build → Some(None) (the caller must NOT fall back to a literal model).
        assert!(
            matches!(try_cascade_backend(&cfg, "cascade", 4096).await, Some(None)),
            "`cascade` with no default table is still a cascade spec (Some(None))"
        );
        // a comma list of two remote names → an auto-ordered cascade (Some(Some(_))).
        assert!(
            matches!(try_cascade_backend(&cfg, "gpt-4o-mini,gpt-4o", 4096).await, Some(Some(_))),
            "a comma-separated model list must build an auto-cascade"
        );
        // a plain single model → NOT a cascade spec → None (caller does its normal build).
        assert!(
            try_cascade_backend(&cfg, "qwen3-4b", 4096).await.is_none(),
            "a plain single model must not route to the cascade path"
        );
    }
}

#[cfg(test)]
mod nctx_tests {
    use super::{N_CTX_FALLBACK, auto_n_ctx, model_max_ctx};

    #[test]
    fn auto_n_ctx_is_model_max_when_config_cached() {
        // Qwen3-4B's config.json has max_position_embeddings = 40960. When that snapshot
        // is cached locally, `auto` must report the real model max (the bug was returning a
        // fixed 32768 fallback while the mlx backend loaded 40960). If the model isn't
        // cached in this environment, the helper returns None → the fallback; tolerate both.
        let spec = "mlx-community:Qwen3-4B-4bit";
        match model_max_ctx(spec) {
            Some(max) => {
                assert_eq!(max, 40_960, "Qwen3-4B max_position_embeddings");
                assert_eq!(
                    auto_n_ctx(spec),
                    40_960,
                    "auto must equal the model max for mlx"
                );
            }
            None => assert_eq!(auto_n_ctx(spec), N_CTX_FALLBACK),
        }
    }
}

#[cfg(test)]
mod lean_tests {
    use super::{LEAN_DISALLOW, apply_lean_flags};

    const EXCL: &str = "--exclude-dynamic-system-prompt-sections";

    fn lean(args: &[&str], on: bool) -> Vec<String> {
        let mut p: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        apply_lean_flags(&mut p, on);
        p
    }
    fn has(v: &[String], s: &str) -> bool {
        v.iter().any(|a| a == s)
    }

    #[test]
    fn full_lean_for_plain_claude() {
        let out = lean(&["claude", "-p", "fix it"], true);
        // Original args preserved; then exclude-dynamic, then the variadic --disallowedTools last.
        assert_eq!(&out[..3], &["claude", "-p", "fix it"]);
        assert_eq!(out[3], EXCL);
        assert_eq!(out[4], "--disallowedTools");
        assert_eq!(out.len(), 5 + LEAN_DISALLOW.len());
        assert!(has(&out, "AskUserQuestion") && has(&out, "mcp__rozum"));
        // Coding-core tools are NOT stripped.
        assert!(!has(&out, "Bash") && !has(&out, "Edit"));
        // Works for an absolute path too.
        assert!(has(
            &lean(&["/usr/bin/claude", "-p", "x"], true),
            "--disallowedTools"
        ));
    }

    #[test]
    fn lean_is_noop_for_codex() {
        // codex reasoning is capped in exec_agent, not via --lean → --lean leaves codex args alone.
        assert_eq!(lean(&["codex", "exec", "x"], true), vec!["codex", "exec", "x"]);
    }

    #[test]
    fn keeps_exclude_dynamic_but_skips_tool_strip_when_user_manages_tools() {
        // User set --disallowedTools → don't override the tool set, but still stabilize
        // the system prefix.
        let out = lean(
            &["claude", "-p", "x", "--disallowedTools", "AskUserQuestion"],
            true,
        );
        assert!(has(&out, EXCL), "exclude-dynamic still applied");
        assert!(!has(&out, "mcp__rozum"), "LEAN_DISALLOW list not appended");
        // --allowedTools likewise.
        assert!(has(
            &lean(&["claude", "--allowedTools", "Read"], true),
            EXCL
        ));
    }

    #[test]
    fn skips_exclude_dynamic_when_user_sets_system_prompt() {
        // User owns the system prompt → don't touch it; tool strip still applies.
        let out = lean(&["claude", "-p", "x", "--system-prompt", "custom"], true);
        assert!(
            !has(&out, EXCL),
            "must not relocate when user set --system-prompt"
        );
        assert!(has(&out, "--disallowedTools"));
        // Already-present exclude-dynamic isn't duplicated.
        let out2 = lean(&["claude", "-p", "x", EXCL], true);
        assert_eq!(out2.iter().filter(|a| a.as_str() == EXCL).count(), 1);
    }

    #[test]
    fn noop_when_off_or_unknown_agent() {
        // Lean off → untouched (claude and codex alike).
        assert_eq!(
            lean(&["claude", "-p", "x"], false),
            vec!["claude", "-p", "x"]
        );
        assert_eq!(
            lean(&["codex", "exec", "x"], false),
            vec!["codex", "exec", "x"]
        );
        // Lean on but an unknown agent (neither claude nor codex) → untouched.
        assert_eq!(lean(&["aider", "x"], true), vec!["aider", "x"]);
    }
}

/// Apply rozum's agent-context env defaults (skills/git/CLAUDE.md trimming),
/// each only when the operator hasn't already set it. Independent of model.
fn apply_rozum_agent_env(cmd: &mut std::process::Command) {
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
}

/// Run the prepared agent command to completion and exit with its status code.
async fn spawn_agent_and_exit(mut cmd: std::process::Command, program_name: &str) -> ! {
    let name = program_name.to_owned();
    let status = tokio::task::spawn_blocking(move || cmd.status())
        .await
        .ok()
        .and_then(|r| r.ok());
    let code = match status {
        Some(s) => s.code().unwrap_or(1),
        None => {
            eprintln!("rozum launch: failed to spawn '{name}'");
            127
        }
    };
    // Drop our lease immediately on exit so the shared daemon shuts down right
    // away when we were the last client, instead of waiting for the lease to go
    // stale (LEASE_FRESH_SECS) or for the idle timeout.
    rozum::share::remove_lease(std::process::id());
    std::process::exit(code);
}

async fn run_models(action: ModelsAction) {
    use rozum::models;

    match action {
        ModelsAction::List { remote: false, .. } => {
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

        ModelsAction::List { remote: true, all } => {
            let print_row = |m: &models::RecommendedModel| {
                println!(
                    "{:<55} {:>4.1} GB  {}",
                    m.spec, m.approx_size_gb, m.display_name
                );
                println!("{:<55} {:>7}  {}", "", "", m.notes);
            };
            println!("Curated download recommendations (Apple Silicon 24-36 GB):");
            println!();
            println!("{:<55} {:>7}  {}", "SPEC", "SIZE", "NOTES");
            for m in models::RECOMMENDED {
                print_row(m);
            }
            if all {
                println!();
                println!("Extended fallback catalog (older / niche — for enthusiasts):");
                println!();
                for m in models::EXTRA {
                    print_row(m);
                }
            }
            println!();
            println!("Install by launching with any of these specs, e.g.:");
            println!("  rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude");
            if !all {
                println!("Pass `--all` to also list the extended fallback catalog.");
            }
            println!("Search more on HuggingFace: https://huggingface.co/mlx-community");
        }

        ModelsAction::Info { spec } => {
            run_info(&spec).await;
        }

        ModelsAction::Rm { spec, yes } => {
            run_models_rm(&spec, yes).await;
        }
    }
}

/// Delete a cached model. Resolves `spec` to an installed model (exact match on
/// the spec shown by `models list`), refuses if it is the active gateway model,
/// confirms, then removes it: HuggingFace/LMStudio directories directly, Ollama
/// via `ollama rm` (its blobs are content-addressed and shared).
async fn run_models_rm(spec: &str, yes: bool) {
    use rozum::models::{self, ModelSource};

    let installed = models::scan_all_installed();
    let Some(m) = installed.iter().find(|m| m.spec == spec) else {
        eprintln!(
            "rozum models rm: '{spec}' is not installed. Run `rozum models list` for installed specs."
        );
        std::process::exit(1);
    };

    // Refuse to delete the model a live gateway is serving.
    if let Some(active) = rozum::share::read_active() {
        if active.model == spec && rozum::share::health_ok(active.port).await {
            eprintln!(
                "rozum models rm: '{spec}' is the active gateway model (pid {}); stop it first \
                 with `rozum gateway stop`.",
                active.pid
            );
            std::process::exit(1);
        }
    }

    println!("Will delete this {} model:", m.source.label());
    println!("  spec: {}", m.spec);
    println!("  path: {}", m.path.display());
    println!("  size: {}", models::format_size(m.size_bytes));

    match m.source {
        ModelSource::Ollama => {
            // Ollama blobs are content-addressed and shared between models, so a
            // direct `rm` could corrupt others. Delegate to `ollama rm` — which wants the
            // bare `<name>:<tag>`, NOT our `ollama:`-prefixed spec.
            let ollama_name = spec.strip_prefix("ollama:").unwrap_or(spec);
            if which("ollama").is_none() {
                eprintln!(
                    "rozum models rm: this is an Ollama model and the `ollama` binary was not \
                     found. Its blobs are shared/content-addressed — not removing directly. \
                     Install Ollama and run `ollama rm {ollama_name}`."
                );
                std::process::exit(1);
            }
            if !confirm_delete(yes) {
                return;
            }
            let status = std::process::Command::new("ollama")
                .arg("rm")
                .arg(ollama_name)
                .status();
            match status {
                Ok(s) if s.success() => println!("deleted (ollama rm {ollama_name})"),
                _ => {
                    eprintln!("rozum models rm: `ollama rm {ollama_name}` failed");
                    std::process::exit(1);
                }
            }
        }
        ModelSource::HuggingFace => {
            // `m.path` is the `models--owner--name` cache directory.
            if !confirm_delete(yes) {
                return;
            }
            if let Err(e) = std::fs::remove_dir_all(&m.path) {
                eprintln!(
                    "rozum models rm: failed to delete {}: {e}",
                    m.path.display()
                );
                std::process::exit(1);
            }
            println!(
                "deleted {}, freed {}",
                m.spec,
                models::format_size(m.size_bytes)
            );
        }
        ModelSource::LMStudio => {
            // `m.path` is the .gguf file; remove its containing repo directory.
            let dir = m.path.parent().unwrap_or(&m.path).to_path_buf();
            println!("  (removing directory {})", dir.display());
            if !confirm_delete(yes) {
                return;
            }
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                eprintln!("rozum models rm: failed to delete {}: {e}", dir.display());
                std::process::exit(1);
            }
            println!(
                "deleted {}, freed {}",
                m.spec,
                models::format_size(m.size_bytes)
            );
        }
    }
}

/// Confirm a destructive delete. `yes` skips the prompt; otherwise a TTY is
/// prompted (`y` to proceed) and a non-TTY is refused (must pass `--yes`).
fn confirm_delete(yes: bool) -> bool {
    use std::io::{IsTerminal, Write as _};
    if yes {
        return true;
    }
    if !std::io::stdin().is_terminal() {
        eprintln!("rozum models rm: refusing to delete without confirmation; pass --yes.");
        return false;
    }
    eprint!("Delete this model? [y/N]: ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    if line.trim().eq_ignore_ascii_case("y") {
        true
    } else {
        eprintln!("cancelled.");
        false
    }
}

/// Is `bin` on `PATH`? (dependency-free `which`.)
fn which(bin: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|p| p.is_file())
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

/// `rozum service {install,uninstall,status}` — install the gateway as an always-warm user service.
/// The plist/unit *generation* is the library's tested `rozum::service`; here we write the file and
/// drive `launchctl` / `systemctl`. Spec: `docs/specs/shared-gateway-service.md`.
fn run_service(action: ServiceAction) {
    match action {
        ServiceAction::Install {
            model,
            n_ctx,
            port,
            offline,
            strategy,
        } => {
            let Some(model) = join_models(model) else {
                eprintln!(
                    "rozum service install: --model is required (the model the service serves)"
                );
                std::process::exit(2);
            };
            let program = match std::env::current_exe() {
                Ok(p) => p.to_string_lossy().into_owned(),
                Err(e) => {
                    eprintln!("rozum service: cannot resolve own executable path: {e}");
                    std::process::exit(1);
                }
            };
            let mut args: Vec<String> = vec!["gateway".into()];
            for m in model.split(',').map(str::trim).filter(|m| !m.is_empty()) {
                args.push("--model".into());
                args.push(m.to_string());
            }
            if let Some(n) = n_ctx {
                args.push("--n-ctx".into());
                args.push(n.to_string());
            }
            if let Some(p) = port {
                args.push("--port".into());
                args.push(p.to_string());
            }
            if offline {
                args.push("--offline".into());
            }
            if let Some(s) = &strategy {
                args.push("--strategy".into());
                args.push(s.clone());
            }
            // Inherit the cascade config env so a named/JSON cascade keeps working under the service.
            let mut env: Vec<(String, String)> = Vec::new();
            for k in ["ROZUM_CASCADE", "ROZUM_CONFIG"] {
                if let Ok(v) = std::env::var(k) {
                    env.push((k.to_string(), v));
                }
            }
            install_service(&program, &args, &env);
        }
        ServiceAction::Uninstall => uninstall_service(),
        ServiceAction::Start => start_service(),
        ServiceAction::Stop => stop_service(),
        ServiceAction::Status => status_service(),
    }
}

/// Print whether the external command succeeded.
fn report_status(st: std::io::Result<std::process::ExitStatus>, ok_msg: &str) {
    match st {
        Ok(s) if s.success() => eprintln!("{ok_msg}"),
        Ok(s) => {
            eprintln!("rozum service: command exited with status {s}");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("rozum service: failed to run the service manager: {e}");
            std::process::exit(1);
        }
    }
}

#[cfg(target_os = "macos")]
fn install_service(program: &str, args: &[String], env: &[(String, String)]) {
    let plist = rozum::service::launchd_plist(program, args, env);
    let path = rozum::service::launchd_plist_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = rozum::share::ensure_dir(); // the service.log dir
    if let Err(e) = std::fs::write(&path, plist) {
        eprintln!("rozum service: write {}: {e}", path.display());
        std::process::exit(1);
    }
    let ps = path.to_string_lossy();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", &ps])
        .status(); // idempotent
    let st = std::process::Command::new("launchctl")
        .args(["load", "-w", &ps])
        .status();
    report_status(
        st,
        &format!("installed + started launchd service → {}", path.display()),
    );
}

#[cfg(target_os = "macos")]
fn uninstall_service() {
    let path = rozum::service::launchd_plist_path();
    let _ = std::process::Command::new("launchctl")
        .args(["unload", "-w", &path.to_string_lossy()])
        .status();
    let _ = std::fs::remove_file(&path);
    eprintln!("uninstalled launchd service ({})", path.display());
}

#[cfg(target_os = "macos")]
fn start_service() {
    let path = rozum::service::launchd_plist_path();
    if !path.exists() {
        eprintln!("rozum service: not installed — run `rozum service install --model …` first");
        std::process::exit(1);
    }
    let st = std::process::Command::new("launchctl")
        .args(["load", &path.to_string_lossy()])
        .status();
    report_status(st, "started launchd service");
}

#[cfg(target_os = "macos")]
fn stop_service() {
    let path = rozum::service::launchd_plist_path();
    let st = std::process::Command::new("launchctl")
        .args(["unload", &path.to_string_lossy()])
        .status();
    report_status(st, "stopped launchd service");
}

#[cfg(target_os = "macos")]
fn status_service() {
    let st = std::process::Command::new("launchctl")
        .args(["list", rozum::service::SERVICE_LABEL])
        .status();
    if !matches!(st, Ok(s) if s.success()) {
        eprintln!(
            "rozum service: not installed (launchctl list found no {})",
            rozum::service::SERVICE_LABEL
        );
    }
}

#[cfg(not(target_os = "macos"))]
fn install_service(program: &str, args: &[String], env: &[(String, String)]) {
    let unit = rozum::service::systemd_unit(program, args, env);
    let path = rozum::service::systemd_unit_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = rozum::share::ensure_dir();
    if let Err(e) = std::fs::write(&path, unit) {
        eprintln!("rozum service: write {}: {e}", path.display());
        std::process::exit(1);
    }
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    let st = std::process::Command::new("systemctl")
        .args(["--user", "enable", "--now", rozum::service::SYSTEMD_UNIT])
        .status();
    report_status(
        st,
        &format!(
            "installed + started systemd --user service → {}",
            path.display()
        ),
    );
}

#[cfg(not(target_os = "macos"))]
fn uninstall_service() {
    let path = rozum::service::systemd_unit_path();
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "disable", "--now", rozum::service::SYSTEMD_UNIT])
        .status();
    let _ = std::fs::remove_file(&path);
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    eprintln!("uninstalled systemd --user service ({})", path.display());
}

#[cfg(not(target_os = "macos"))]
fn start_service() {
    let st = std::process::Command::new("systemctl")
        .args(["--user", "start", rozum::service::SYSTEMD_UNIT])
        .status();
    report_status(st, "started systemd --user service");
}

#[cfg(not(target_os = "macos"))]
fn stop_service() {
    let st = std::process::Command::new("systemctl")
        .args(["--user", "stop", rozum::service::SYSTEMD_UNIT])
        .status();
    report_status(st, "stopped systemd --user service");
}

#[cfg(not(target_os = "macos"))]
fn status_service() {
    let _ = std::process::Command::new("systemctl")
        .args(["--user", "status", rozum::service::SYSTEMD_UNIT])
        .status();
}

/// A [`rozum::gateway::BackendBuilder`] over this binary's backend-selection
/// chain, so the daemon can rebuild a model in place on `gateway switch` and
/// lazily reload after `unload` — without the library depending on `main`.
/// Load `rozum.toml` (or the default auto-detect chain) or exit on a malformed /
/// missing-explicit config — a config the user deliberately wrote must surface,
/// not silently fall back. See `docs/specs/runtime-config.md`.
fn load_runtime_config_or_exit() -> rozum::RuntimeConfig {
    match rozum::RuntimeConfig::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("rozum: {e}");
            std::process::exit(2);
        }
    }
}

/// The injected backend builder for the daemon's `Switchboard`. An explicit
/// `--backend B` (`force`) bypasses the config and forces exactly one engine;
/// otherwise it walks the configured `gateway_chain()` (fallback semantics).
fn gateway_backend_builder(
    cfg: std::sync::Arc<rozum::RuntimeConfig>,
) -> rozum::gateway::BackendBuilder {
    std::sync::Arc::new(move |model: String, n_ctx: u32, force: Option<String>| {
        let cfg = std::sync::Arc::clone(&cfg);
        Box::pin(async move {
            // `model: "cascade[:name]"` / a comma-separated list → a CascadeBackend (the
            // request-surface wiring), regardless of any forced engine — the model string
            // is the explicit intent. Shared with the startup build via `try_cascade_backend`.
            if let Some(result) = try_cascade_backend(&cfg, &model, n_ctx).await {
                return result;
            }
            match force.as_deref() {
                Some(f) => build_gateway_backend_forced(&model, n_ctx, f).await,
                None => build_from_config(&cfg, &model, n_ctx).await,
            }
        })
            as std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Option<std::sync::Arc<dyn rozum::ChatBackend>>>
                        + Send,
                >,
            >
    })
}

/// Walk the configured backend chain, returning the first backend that builds
/// (fallback semantics; `single` policy yields a one-element chain). With the
/// default config this reproduces the old `build_gateway_backend` order exactly:
/// `gguf → mistralrs → lmstudio → mlx → url`.
/// `rozum commit-msg` — generate a commit message for the staged diff with a local model.
/// A single `--model` generates directly; a `small,big` comma-list runs the small-first
/// cascade (`cascade::small_task_config`) so the small model answers and a structural
/// commit-message gate escalates to the big model only when the cheap answer is unusable.
async fn run_commit_msg(model: Option<String>, n_ctx: Option<u32>) {
    use rozum::cascade::{CascadeBackend, SmallTask, commit_message_request, small_task_config};

    let diff = match staged_diff() {
        Ok(d) if !d.trim().is_empty() => d,
        Ok(_) => {
            eprintln!("commit-msg: nothing staged — `git add` your changes first");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("commit-msg: {e}");
            std::process::exit(1);
        }
    };

    let cfg = load_runtime_config_or_exit();
    let Some(model) = model.or_else(|| cfg.model.clone()) else {
        eprintln!("commit-msg: no model — pass --model <spec> (or set [runtime].model in rozum.toml)");
        std::process::exit(1);
    };
    let n_ctx = n_ctx.unwrap_or(N_CTX_FALLBACK);

    let names: Vec<&str> = model.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let build = |spec: &str| {
        let cfg = &cfg;
        let spec = spec.to_string();
        async move {
            build_from_config(cfg, &spec, n_ctx).await.unwrap_or_else(|| {
                eprintln!("commit-msg: could not load model '{spec}'");
                std::process::exit(1);
            })
        }
    };

    let backend: std::sync::Arc<dyn rozum::ChatBackend> = if names.len() >= 2 {
        // small-first cascade: cheapest model answers, the commit-message gate escalates.
        let small = build(names[0]).await;
        let big = build(names[names.len() - 1]).await;
        std::sync::Arc::new(CascadeBackend::new(small_task_config(
            SmallTask::CommitMessage,
            small,
            big,
        )))
    } else {
        build(names.first().copied().unwrap_or(model.as_str())).await
    };

    let stream = match backend.chat(commit_message_request(&diff)).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("commit-msg: generation failed: {e}");
            std::process::exit(1);
        }
    };
    let msg = rozum::collect_to_string(stream).await.unwrap_or_default();
    let msg = msg.trim();
    if msg.is_empty() {
        eprintln!("commit-msg: model returned an empty message");
        std::process::exit(1);
    }
    println!("{msg}");
}

/// The staged diff (`git diff --cached`), no color, from the repo at the cwd.
fn staged_diff() -> Result<String, String> {
    staged_diff_in(None)
}

/// `git diff --cached` in `dir` (or the cwd when `None`) — split out so it's testable.
fn staged_diff_in(dir: Option<&std::path::Path>) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["diff", "--cached", "--no-color"]);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().map_err(|e| format!("running git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| format!("git output not UTF-8: {e}"))
}

#[cfg(test)]
mod commit_msg_tests {
    use super::*;

    #[test]
    fn staged_diff_reads_the_index_and_empty_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(p)
                .output()
                .unwrap();
        };
        git(&["init", "-q"]);
        // A fresh repo with nothing staged → empty diff, NOT an error.
        assert!(staged_diff_in(Some(p)).unwrap().trim().is_empty());
        // Stage a file → the diff names it and shows the added content.
        std::fs::write(p.join("hello.txt"), "fn main() {}\n").unwrap();
        git(&["add", "hello.txt"]);
        let diff = staged_diff_in(Some(p)).expect("staged diff");
        assert!(diff.contains("hello.txt"), "diff names the staged file:\n{diff}");
        assert!(diff.contains("fn main()"), "diff shows the added content:\n{diff}");
    }
}

async fn build_from_config(
    cfg: &rozum::RuntimeConfig,
    model: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    for choice in cfg.gateway_chain() {
        if let Some(b) = build_choice(choice, model, n_ctx).await {
            rozum::obs::log_event(serde_json::json!({
                "event": "backend_selected_from_config",
                "backend": choice.engine, "id": choice.id, "model": model,
            }));
            return Some(b);
        }
    }
    None
}

/// Build a single configured backend, applying its per-backend overrides
/// (`model`, `n_ctx`, `url`). Engines that aren't gateway-servable
/// (`hello`/`candle`/`llama-gguf`/`native-rust`/`external-command`) yield `None`
/// so a fallback chain moves past them.
async fn build_choice(
    choice: &rozum::BackendChoice,
    req_model: &str,
    req_n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::concurrency::admit_wrap;
    let model = choice.model.as_deref().unwrap_or(req_model);
    let n_ctx = choice.n_ctx.unwrap_or(req_n_ctx);
    match choice.engine.as_str() {
        // explicit endpoint override → construct the HTTP backend directly
        "lmstudio" | "mlx" | "mlx_lm" | "mlx-server" | "mlx_lm_server" | "mlx-lm-server"
        | "url" | "http"
            if choice.url.is_some() =>
        {
            let url = choice.url.clone().unwrap();
            Some(admit_wrap(
                std::sync::Arc::new(rozum::openai_http::OpenAiHttpBackend::new(url, model))
                    as std::sync::Arc<dyn rozum::ChatBackend>,
            ))
        }
        "gguf" | "mistralrs" | "lmstudio" | "mlx" | "mlx_lm" | "mlx-server" | "mlx_lm_server"
        | "mlx-lm-server" | "url" | "http" => {
            build_gateway_backend_forced(model, n_ctx, &choice.engine).await
        }
        // not servable by the gateway (sync/meeting-room engines)
        _ => None,
    }
}

/// Engine aliases that select the opt-in Python `mlx_lm.server` HTTP backend.
/// Distinct from `mlx`/`mlx-native` (the in-process native MLX runtime).
fn is_mlx_server_engine(engine: &str) -> bool {
    matches!(engine, "mlx-server" | "mlx_lm_server" | "mlx-lm-server")
}

/// Build a [`rozum::cascade::CascadeBackend`] for `model: "cascade[:name]"`. The named spec is
/// loaded from the environment (`ROZUM_CASCADE` for the default, `ROZUM_CASCADE_<NAME>` for a named
/// config) as JSON; each tier is resolved through this binary's normal build chain — locals via
/// `build_from_config`, remotes via the OpenAI-compatible HTTP backend with the configured key.
/// Fold the (repeatable) `--model` values into one model string. Each value may itself be a comma
/// list, so `--model a,b --model c` flattens to `a,b,c`; the comma form then routes to the
/// auto-cascade path. Empty → `None` (no model given).
fn join_models(values: Vec<String>) -> Option<String> {
    let parts: Vec<String> = values
        .iter()
        .flat_map(|s| s.split(','))
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join(","))
}

/// Apply `--strategy` to the cascade builder via `ROZUM_CASCADE_STRATEGY`, which
/// [`build_cascade_from_spec`] reads (and the spawned daemon inherits).
fn apply_cascade_strategy(strategy: Option<&str>) {
    if let Some(s) = strategy.map(str::trim).filter(|s| !s.is_empty()) {
        // SAFETY: set at startup, before the backend/daemon is built or spawned.
        unsafe { std::env::set_var("ROZUM_CASCADE_STRATEGY", s) };
    }
}

/// Apply `--offline` via `ROZUM_OFFLINE`, read by [`build_remote_tier`] (skip remote tiers) and the
/// model picker (hide cloud entries). The spawned daemon inherits it.
fn apply_offline(offline: bool) {
    if offline {
        // SAFETY: set at startup, before the backend/daemon is built or spawned.
        unsafe { std::env::set_var("ROZUM_OFFLINE", "1") };
    }
}

/// Whether offline mode is on (`ROZUM_OFFLINE` truthy) — no remote/cloud models.
fn is_offline() -> bool {
    matches!(
        std::env::var("ROZUM_OFFLINE").ok().as_deref(),
        Some("1" | "true" | "on")
    )
}

/// The cascade request-surface, shared by the gateway's **startup** build and the
/// reload `BackendBuilder` so both honor `--model cascade[:name]` / a comma-separated
/// model list. Returns:
/// - `None` → `model` is NOT a cascade spec → the caller does its normal single-model build;
/// - `Some(result)` → it IS a cascade spec → use `result` directly (`Some` backend, or
///   `None` if the cascade failed to build — do NOT fall back to a literal model named
///   "cascade…").
///
/// A comma list that fails to build returns `None` (not `Some(None)`) so the caller still
/// falls back to a normal build — preserving the prior reload-builder behavior.
async fn try_cascade_backend(
    cfg: &std::sync::Arc<rozum::RuntimeConfig>,
    model: &str,
    n_ctx: u32,
) -> Option<Option<std::sync::Arc<dyn rozum::ChatBackend>>> {
    if let Some(name) = rozum::cascade::parse_cascade_model(model) {
        return Some(build_cascade_backend(cfg, &name, n_ctx).await);
    }
    if model.contains(',') {
        if let Some(be) = build_cascade_from_list(cfg, model, n_ctx).await {
            return Some(Some(be));
        }
    }
    None
}

async fn build_cascade_backend(
    cfg: &std::sync::Arc<rozum::RuntimeConfig>,
    name: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    let spec = load_cascade_spec(cfg, name)?;
    build_cascade_from_spec(cfg, spec, n_ctx, name).await
}

/// The simple path: `model` is a comma-separated list of names → an **auto-cascade** (each name
/// classified local/Claude/OpenAI, the list auto-ordered cheapest→most-capable, `classify`
/// strategy). One name → not a cascade (built by the normal chain). Returns `None` if it isn't a
/// list. `--model "qwen3-4b,claude-haiku-4-5,gpt-4o"` and the multi-select picker land here.
async fn build_cascade_from_list(
    cfg: &std::sync::Arc<rozum::RuntimeConfig>,
    model: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    let names: Vec<String> = model
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if names.len() < 2 {
        return None; // a single name isn't a cascade — let the normal path build it
    }
    let spec = rozum::cascade::from_model_list(&names);
    build_cascade_from_spec(cfg, spec, n_ctx, "auto").await
}

/// Resolve a [`rozum::cascade::CascadeSpec`] to a live `CascadeBackend`: locals via this binary's
/// normal build chain, remotes via the OpenAI/Anthropic HTTP backends.
async fn build_cascade_from_spec(
    cfg: &std::sync::Arc<rozum::RuntimeConfig>,
    mut spec: rozum::cascade::CascadeSpec,
    n_ctx: u32,
    label: &str,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    // `--strategy` (→ ROZUM_CASCADE_STRATEGY) overrides the spec's start-tier strategy.
    if let Some(st) = std::env::var("ROZUM_CASCADE_STRATEGY")
        .ok()
        .and_then(|v| rozum::cascade::StrategyName::parse_cli(&v))
    {
        spec.strategy = st;
    }
    let n_tiers = spec.tiers.len();
    let cfg = std::sync::Arc::clone(cfg);
    let resolver = move |tier: rozum::cascade::TierSpec| {
        let cfg = std::sync::Arc::clone(&cfg);
        async move {
            match tier.location {
                rozum::cascade::Location::Local => {
                    build_from_config(&cfg, &tier.model, n_ctx).await
                }
                rozum::cascade::Location::Remote => build_remote_tier(&tier),
            }
        }
    };
    match rozum::cascade::build_cascade(&spec, resolver).await {
        Ok(be) => {
            rozum::obs::log_event(serde_json::json!({
                "event": "cascade_built", "config": label, "tiers": n_tiers,
            }));
            Some(std::sync::Arc::new(be) as std::sync::Arc<dyn rozum::ChatBackend>)
        }
        Err(e) => {
            rozum::obs::log_event(serde_json::json!({
                "event": "cascade_build_failed", "config": label, "error": e,
            }));
            None
        }
    }
}

/// Resolve a [`rozum::cascade::CascadeSpec`] by name. A `[cascade.<name>]` table in `rozum.toml`
/// wins (`default` for `model: "cascade"`); otherwise fall back to the environment —
/// `ROZUM_CASCADE` / `ROZUM_CASCADE_<NAME>` (upper-cased) as JSON.
fn load_cascade_spec(
    cfg: &rozum::RuntimeConfig,
    name: &str,
) -> Option<rozum::cascade::CascadeSpec> {
    if let Some(spec) = cfg.cascade_spec(name) {
        return Some(spec.clone());
    }
    let var = if name.is_empty() {
        "ROZUM_CASCADE".to_string()
    } else {
        format!("ROZUM_CASCADE_{}", name.to_uppercase())
    };
    let raw = match std::env::var(&var) {
        Ok(r) if !r.trim().is_empty() => r,
        _ => {
            rozum::obs::log_event(serde_json::json!({
                "event": "cascade_spec_missing", "config": name, "env": var,
            }));
            return None;
        }
    };
    match serde_json::from_str(&raw) {
        Ok(s) => Some(s),
        Err(e) => {
            rozum::obs::log_event(serde_json::json!({
                "event": "cascade_spec_invalid", "env": var, "error": e.to_string(),
            }));
            None
        }
    }
}

/// Resolve a remote cascade tier to an HTTP backend. `api: "anthropic"` → the native Claude
/// `/v1/messages` backend (default endpoint `https://api.anthropic.com`, key from
/// `ANTHROPIC_API_KEY`); otherwise an OpenAI-compatible `/v1/chat/completions` backend (covers
/// OpenAI, OpenRouter, LM Studio, mlx_lm.server, …, key from `OPENAI_API_KEY`). `api_key_env` /
/// `endpoint` override the defaults. Returns `None` (→ the tier is skipped) when a required key or
/// endpoint is missing.
fn build_remote_tier(
    tier: &rozum::cascade::TierSpec,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::cascade::RemoteApi;
    use rozum::concurrency::admit_wrap;
    // Offline mode: drop every remote tier (it's skipped like any unbuildable tier).
    if is_offline() {
        rozum::obs::log_event(serde_json::json!({
            "event": "cascade_tier_skipped_offline", "model": tier.model,
        }));
        return None;
    }
    let backend: std::sync::Arc<dyn rozum::ChatBackend> = match tier.api {
        RemoteApi::Anthropic => {
            // Native Anthropic requires a key — skip the tier if it isn't configured.
            let endpoint = tier
                .endpoint
                .as_deref()
                .unwrap_or("https://api.anthropic.com");
            let key_env = tier.api_key_env.as_deref().unwrap_or("ANTHROPIC_API_KEY");
            let key = std::env::var(key_env).ok().filter(|k| !k.is_empty())?;
            std::sync::Arc::new(rozum::anthropic_http::AnthropicHttpBackend::new(
                endpoint,
                &tier.model,
                key,
            ))
        }
        RemoteApi::Openai => {
            // Defaults to OpenAI itself; an OpenAI-compatible provider (OpenRouter, LM Studio, …)
            // sets `endpoint` explicitly.
            let endpoint = tier
                .endpoint
                .as_deref()
                .unwrap_or("https://api.openai.com/v1");
            let key_env = tier.api_key_env.as_deref().unwrap_or("OPENAI_API_KEY");
            let mut b = rozum::openai_http::OpenAiHttpBackend::new(endpoint, &tier.model);
            if let Some(key) = std::env::var(key_env).ok().filter(|k| !k.is_empty()) {
                b = b.with_api_key(key);
            }
            std::sync::Arc::new(b)
        }
    };
    Some(admit_wrap(backend))
}

/// Build a backend forcing a specific engine (`gateway switch --backend B`).
/// Unknown values fall back to the auto-detect chain. Recognized:
/// `gguf`, `mistralrs`, `lmstudio`, `mlx`, `mlx-server`, `url`.
async fn build_gateway_backend_forced(
    model_spec: &str,
    n_ctx: u32,
    force: &str,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::concurrency::admit_wrap;
    rozum::obs::log_event(serde_json::json!({
        "event": "backend_force", "backend": force, "model": model_spec,
    }));
    match force {
        "gguf" => try_build_gguf_backend(model_spec, n_ctx).map(admit_wrap),
        "mistralrs" => try_build_mistralrs_backend(model_spec, n_ctx)
            .await
            .map(admit_wrap),
        "lmstudio" => rozum::openai_http::try_lmstudio_http(model_spec)
            .await
            .map(admit_wrap),
        // `mlx`/`mlx_lm` force the in-process native MLX runtime; `mlx-server`
        // (a.k.a. `mlx_lm_server`) forces the opt-in Python `mlx_lm.server` over
        // HTTP (`ROZUM_MLX_HTTP`).
        "mlx" | "mlx-native" | "mlx_lm" => try_build_mlx_native_backend(model_spec, n_ctx)
            .await
            .map(admit_wrap),
        // The x86 Vulkan-iGPU engine slot (docs/specs/x86-native-runtime.md). Scaffolded but
        // not implemented — `try_build_x86_backend` logs why and returns None, so selection
        // falls through to the next engine instead of failing silently.
        "x86-native" | "x86" | "vulkan" => {
            rozum::x86::try_build_x86_backend(model_spec, n_ctx).map(admit_wrap)
        }
        e if is_mlx_server_engine(e) => rozum::openai_http::try_mlx_server(model_spec)
            .await
            .map(admit_wrap),
        "url" | "http" => std::env::var("ROZUM_BACKEND_URL").ok().map(|url| {
            admit_wrap(
                std::sync::Arc::new(rozum::openai_http::OpenAiHttpBackend::new(url, model_spec))
                    as std::sync::Arc<dyn rozum::ChatBackend>,
            )
        }),
        other => {
            rozum::obs::log_event(serde_json::json!({
                "event": "backend_force_unknown", "backend": other, "fallback": "auto",
            }));
            build_gateway_backend(model_spec, n_ctx).await
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

    // 1. Try the pure-Rust native MLX runtime (the primary in-process backend):
    //    full native MLX forward, no candle, no Python. Covers the Qwen3 / Qwen3.6
    //    / Qwen2 / Llama families and auto-downloads HF / ModelScope MLX repos.
    //    Declines fast for `.gguf` files / `lmstudio:` / `ollama:` specs, so those
    //    fall through to GGUF below.
    if let Some(b) = try_build_mlx_native_backend(model_spec, n_ctx).await {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"mlx-native","model":model_spec}),
        );
        return Some(rozum::concurrency::admit_wrap(b));
    }

    // 2. Try in-process GGUF (the GGUF/llama.cpp fallback, in the default build):
    //    local `.gguf` files, `lmstudio:<repo>`, `ollama:<name>` (cached blobs).
    if let Some(b) = try_build_gguf_backend(model_spec, n_ctx) {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"gguf","model":model_spec}),
        );
        return Some(rozum::concurrency::admit_wrap(b));
    }

    // 3. Try in-process MLX via mistralrs (opt-in `--features mistralrs`): the
    //    broader-catalog candle backend, a fallback for models the native MLX
    //    runtime does not yet port.
    if let Some(b) = try_build_mistralrs_backend(model_spec, n_ctx).await {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"mistralrs","model":model_spec}),
        );
        return Some(rozum::concurrency::admit_wrap(b));
    }

    // 4. Try LM Studio's local server (native MLX runtime via its GUI app), a
    //    fallback for MLX models neither in-process backend covers.
    if let Some(b) = rozum::openai_http::try_lmstudio_http(model_spec).await {
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"lmstudio-http","model":model_spec}),
        );
        return Some(rozum::concurrency::admit_wrap(b));
    }

    // 4b. Opt-in: Python `mlx_lm.server`. Retired as a default (native MLX
    //     supersedes it), so only tried when the operator points us at one via
    //     `ROZUM_MLX_HTTP` — otherwise skipped so its default port isn't probed.
    if std::env::var_os("ROZUM_MLX_HTTP").is_some() {
        if let Some(b) = rozum::openai_http::try_mlx_server(model_spec).await {
            rozum::obs::log_event(
                serde_json::json!({"event":"backend_selected","backend":"mlx-server-http","model":model_spec}),
            );
            return Some(rozum::concurrency::admit_wrap(b));
        }
    }

    // 5. Try user-specified URL via env (any OpenAI-compatible server)
    if let Ok(url) = std::env::var("ROZUM_BACKEND_URL") {
        eprintln!("backend: custom HTTP at {url}");
        rozum::obs::log_event(
            serde_json::json!({"event":"backend_selected","backend":"custom-http","url":url,"model":model_spec}),
        );
        return Some(rozum::concurrency::admit_wrap(std::sync::Arc::new(
            rozum::openai_http::OpenAiHttpBackend::new(url, model_spec),
        )));
    }

    rozum::obs::log_event(serde_json::json!({
        "event": "backend_select_failed", "model": model_spec,
        "note": "no backend: no local file, native MLX/mistralrs load failed, no LM Studio, ROZUM_BACKEND_URL unset",
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
        // (mistralrs is opt-in `--features mistralrs`; the RAM preflight is its own.)
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
        "    rozum launch --model 'ollama:<name>:<tag>'      claude   # reads ~/.ollama/models/blobs/"
    );
    eprintln!();
    eprintln!("  in-process native MLX (on by default, Metal, AFQ safetensors):");
    eprintln!("    rozum launch --model mlx-community:Qwen3.6-35B-A3B-4bit claude");
    eprintln!("    rozum launch --model mlx-community:gpt-oss-20b-MXFP4-Q4 claude");
    eprintln!("    # covers the Qwen3 / Qwen3.6 + gpt-oss families; auto-downloads if not cached");
    eprintln!();
    eprintln!("  in-process mistralrs (opt-in, broader catalog):");
    eprintln!("    cargo build --features mistralrs");
    eprintln!("    rozum launch --model mlx-community:<repo> claude");
    eprintln!();
    eprintln!("  LM Studio (GUI app, native MLX runtime, for models not yet ported):");
    eprintln!("    1. Download LM Studio: https://lmstudio.ai");
    eprintln!("    2. Inside LM Studio, install the model (Search tab → mlx-community/...)");
    eprintln!("    3. Start the local server (Developer tab → Status: Running)");
    eprintln!("    4. rozum launch --model <model-id-shown-in-lmstudio>  claude");
    eprintln!();
    eprintln!("  mlx_lm.server (Python, opt-in — set ROZUM_MLX_HTTP or --backend mlx-server):");
    eprintln!("    python -m mlx_lm.server --model mlx-community/<repo> --port 8080 &");
    eprintln!("    ROZUM_MLX_HTTP=http://localhost:8080/v1 rozum launch --model <id> claude");
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
/// Only the mistralrs `auto_n_ctx` consults it (the native MLX backend reads its
/// own context window from config).
#[cfg(feature = "mistralrs")]
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

/// The model's max context — `max_position_embeddings` from the cached `config.json`
/// (`text_config`'s for multimodal). `None` if the config can't be read.
fn model_max_ctx(model_spec: &str) -> Option<u32> {
    let id = rozum::mistralrs_backend::normalize_spec(model_spec);
    cached_config_json(&id).and_then(|cfg| {
        let t = cfg.get("text_config").cloned().unwrap_or(cfg);
        t.get("max_position_embeddings")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
    })
}

/// The default context window: the model's max, falling back to [`N_CTX_FALLBACK`].
/// **mistralrs** additionally caps it at [`N_CTX_AUTO_CAP`] — it pre-allocates the
/// PagedAttention KV pool, so a huge advertised max (Qwen3.6: 262144) would never fit.
#[cfg(feature = "mistralrs")]
fn auto_n_ctx(model_spec: &str) -> u32 {
    model_max_ctx(model_spec).map_or(N_CTX_FALLBACK, |m| m.min(N_CTX_AUTO_CAP))
}

/// The default context window: the model's max, falling back to [`N_CTX_FALLBACK`].
/// **mlx-native** grows its KV cache lazily per actual token and runs a per-request RAM
/// preflight, so the full model max is safe as the default — no upfront cost, no cap.
#[cfg(not(feature = "mistralrs"))]
fn auto_n_ctx(model_spec: &str) -> u32 {
    model_max_ctx(model_spec).unwrap_or(N_CTX_FALLBACK)
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
    use rozum::concurrency::{
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

/// Build a speculative-decoding backend for `target_spec` accelerated by
/// `draft_spec`. Prefers the real MLX dual-model spec-decode (both dense MLX →
/// the greedy decode speedup, loaded together in one worker so the target isn't
/// loaded twice); falls back to the engine-agnostic `SpecDecodeBackend` SPI
/// wrapper (target-only decode — byte-identical, no speedup) for non-MLX or
/// non-dense pairs. Spec: docs/specs/speculative-decoding.md.
async fn build_spec_decode_backend(
    cfg: &rozum::RuntimeConfig,
    target_spec: &str,
    draft_spec: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    if let Some(b) = try_build_mlx_spec_decode(target_spec, draft_spec, n_ctx).await {
        eprintln!("  spec-decode:        target {target_spec} + draft {draft_spec} (MLX dense)");
        return Some(b);
    }
    // Fallback: target alone, with the draft held resident behind the SPI wrapper.
    let target = build_from_config(cfg, target_spec, n_ctx).await?;
    match build_from_config(cfg, draft_spec, n_ctx).await {
        Some(draft) => {
            eprintln!("  spec-decode draft:  {draft_spec} (SPI wrapper — target-only decode)");
            Some(std::sync::Arc::new(
                rozum::specdecode_backend::SpecDecodeBackend::new(target, draft),
            ))
        }
        None => {
            eprintln!(
                "rozum gateway: --draft-model '{draft_spec}' failed to build; serving target only"
            );
            Some(target)
        }
    }
}

#[cfg(feature = "mlx-native")]
async fn try_build_mlx_spec_decode(
    target_spec: &str,
    draft_spec: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::mlx_native_backend::{MlxNativeBackend, ensure_model_dir};
    // Non-MLX specs (GGUF files, lmstudio:/ollama:) → let the SPI fallback handle it.
    for s in [target_spec, draft_spec] {
        if s.starts_with("lmstudio:")
            || s.starts_with("ollama:")
            || std::path::Path::new(s).is_file()
        {
            return None;
        }
    }
    let target_dir = ensure_model_dir(target_spec).await?;
    let draft_dir = ensure_model_dir(draft_spec).await?;
    let target_id = rozum::mistralrs_backend::normalize_spec(target_spec);
    match MlxNativeBackend::new_spec_decode(target_dir, target_id.clone(), draft_dir, Some(n_ctx))
        .await
    {
        Ok(b) => {
            eprintln!("backend: mlx-native spec-decode (in-process, Metal) — target {target_id}");
            Some(std::sync::Arc::new(b))
        }
        Err(e) => {
            eprintln!("warning: mlx spec-decode build failed ({e}); falling back to target-only");
            None
        }
    }
}

#[cfg(not(feature = "mlx-native"))]
async fn try_build_mlx_spec_decode(
    _target_spec: &str,
    _draft_spec: &str,
    _n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    None
}

#[cfg(feature = "mlx-native")]
async fn try_build_mlx_native_backend(
    model_spec: &str,
    n_ctx: u32,
) -> Option<std::sync::Arc<dyn rozum::ChatBackend>> {
    use rozum::mlx_native_backend::{MlxNativeBackend, ensure_model_dir};
    // GGUF model FILES and `lmstudio:` specs belong to other backends. Use
    // `is_file()` (not `extension()`, which misfires on dotted repo names like
    // `mlx-community:Qwen3.6-27B-4bit`); a local MLX *directory* still resolves.
    if model_spec.starts_with("lmstudio:")
        || model_spec.starts_with("ollama:")
        || std::path::Path::new(model_spec).is_file()
    {
        return None;
    }
    // Use a cached snapshot if present, else auto-download it from HuggingFace
    // (gated on a supported `model_type` so we never pull weights for a repo this
    // runtime can't run). `None` → fall through to the next backend.
    let dir = ensure_model_dir(model_spec).await?;
    let id = rozum::mistralrs_backend::normalize_spec(model_spec);
    // smmr-A (`docs/specs/safe-multi-model-residency.md`): hard-cap THIS process's MLX
    // unified memory at the SAME footprint the residency gate reserved for this model,
    // BEFORE the worker loads. So a co-resident gateway can't let the MLX cache balloon
    // past its share (uncapped it grows to ~total−8 GB regardless of model size — the
    // BUG-003 reboot mechanism); the sum across gateways stays within the host budget.
    // Only for a known-size model; an unknown one keeps the default `total−8 GB` cap.
    let footprint = estimate_model_footprint_bytes(model_spec, n_ctx);
    if footprint < u64::MAX / 8 {
        rozum::mlx_native_backend::set_memory_cap_bytes(footprint);
    }
    match MlxNativeBackend::new(dir.clone(), id.clone(), Some(n_ctx)).await {
        Ok(b) => {
            eprintln!(
                "backend: mlx-native (in-process, Metal) — model: {id} ({})",
                dir.display()
            );
            Some(std::sync::Arc::new(b))
        }
        Err(e) => {
            eprintln!("warning: mlx-native load failed: {e}");
            rozum::obs::log_event(serde_json::json!({
                "event": "backend_load_failed", "backend": "mlx-native", "model": id, "error": e.to_string(),
            }));
            None
        }
    }
}

#[cfg(not(feature = "mlx-native"))]
async fn try_build_mlx_native_backend(
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

    fn reorder(args: &[&str]) -> Vec<String> {
        reorder_launch_args(args.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn claude_channel_version_gate() {
        assert!(claude_version_supports_channels("2.1.172 (Claude Code)"));
        assert!(claude_version_supports_channels("2.1.80"));
        assert!(claude_version_supports_channels("2.2.0 (Claude Code)"));
        assert!(claude_version_supports_channels("3.0.0"));
        assert!(!claude_version_supports_channels("2.1.79"));
        assert!(!claude_version_supports_channels("2.0.99"));
        assert!(!claude_version_supports_channels("garbage"));
        assert!(!claude_version_supports_channels(""));
    }

    #[test]
    fn reorder_pulls_bool_flag_after_program_to_front() {
        // `--no-model` placed after the program name is hoisted ahead of it so
        // clap parses it as a launch flag, not a child arg.
        assert_eq!(
            reorder(&["rozum", "launch", "claude", "--no-model"]),
            vec!["rozum", "launch", "--no-model", "claude"]
        );
        // Mixed with a value flag, both are pulled and order is preserved.
        assert_eq!(
            reorder(&["rozum", "launch", "claude", "--no-model", "--port", "9000"]),
            vec!["rozum", "launch", "--no-model", "--port", "9000", "claude"]
        );
    }

    #[test]
    fn reorder_stops_at_separator() {
        // A `--no-model` after `--` belongs to the child program, untouched.
        assert_eq!(
            reorder(&["rozum", "launch", "claude", "--", "--no-model"]),
            vec!["rozum", "launch", "claude", "--", "--no-model"]
        );
    }

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
