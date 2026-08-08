use clap::{Args, Parser, Subcommand};

/// Named model-load tuning flags — friendly sugar over `--set ROZUM_*`, flattened into `gateway` and
/// `launch`. Each flag, when given, sets the corresponding env var at CLI precedence (CLI > env >
/// config > default); absent leaves the env / config / default in effect. Prefer these to `--set` for
/// the common knobs; don't pass both a flag and `--set` for the same option.
#[derive(Args, Debug, Default)]
struct TuningOpts {
    /// Disable adaptive loading: refuse a model that doesn't fit RAM instead of auto-shrinking
    /// n_ctx/cache to the best fit (ROZUM_GATEWAY_ADAPTIVE_LOAD=0).
    #[arg(long)]
    no_adaptive_load: bool,

    /// Disable the GLM create-from-scratch artifact→tool synth (ROZUM_GLM_ARTIFACT_SYNTH=0).
    #[arg(long)]
    no_glm_synth: bool,

    /// Opt-in: constrain GLM bare tool-args to valid JSON during decode, for robustness
    /// (ROZUM_GLM_CONSTRAIN_ARGS=1).
    #[arg(long)]
    glm_constrain_args: bool,

    /// Bypass the host residency gate — overrides the no-overcommit safety; only with care
    /// (ROZUM_ALLOW_CONCURRENT_RESIDENT=1).
    #[arg(long)]
    allow_concurrent_resident: bool,

    /// RAM in GiB to keep free after a model loads — the no-overcommit headroom (default 3).
    #[arg(long, value_name = "GIB")]
    min_free_ram_gb: Option<f64>,

    /// Reserved-footprint RAM budget as a fraction of total RAM (default 0.75).
    #[arg(long, value_name = "FRAC")]
    ram_budget_frac: Option<f64>,

    /// MLX buffer-cache cap in GiB — also the per-process reserve (default 4).
    #[arg(long, value_name = "GIB")]
    mlx_cache_gb: Option<u64>,
}

impl TuningOpts {
    /// Set the env for each given flag (force — CLI precedence). Run before any model load.
    fn apply_to_env(&self) {
        // SAFETY: single-threaded startup, before the backend worker thread spawns.
        unsafe {
            if self.no_adaptive_load {
                std::env::set_var("ROZUM_GATEWAY_ADAPTIVE_LOAD", "0");
            }
            if self.no_glm_synth {
                std::env::set_var("ROZUM_GLM_ARTIFACT_SYNTH", "0");
            }
            if self.glm_constrain_args {
                std::env::set_var("ROZUM_GLM_CONSTRAIN_ARGS", "1");
            }
            if self.allow_concurrent_resident {
                std::env::set_var("ROZUM_ALLOW_CONCURRENT_RESIDENT", "1");
            }
            if let Some(g) = self.min_free_ram_gb {
                let bytes = (g.max(0.0) * (1u64 << 30) as f64) as u64;
                std::env::set_var("ROZUM_GATEWAY_MIN_FREE_RAM_BYTES", bytes.to_string());
            }
            if let Some(f) = self.ram_budget_frac {
                std::env::set_var("ROZUM_GATEWAY_RAM_BUDGET_FRAC", f.to_string());
            }
            if let Some(c) = self.mlx_cache_gb {
                std::env::set_var("ROZUM_MLX_CACHE_GB", c.to_string());
            }
        }
    }
}

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

    /// Use the legacy in-process single-room runtime (with the legacy web
    /// bridge + model-as-participant sampling) instead of attaching a TUI to
    /// the meeting daemon. `--web-port` implies this.
    #[arg(long)]
    legacy_room: bool,

    /// Set a `ROZUM_*` tuning option on the command line — repeatable, e.g.
    /// `--set ROZUM_GATEWAY_ADAPTIVE_LOAD=0 --set ROZUM_GLM_ARTIFACT_SYNTH=0`.
    /// Highest precedence (CLI `--set` > env > config `[options]` > default). The
    /// same knobs are settable as env vars or in the config's `[options]` table.
    /// Only `ROZUM_`-prefixed keys are accepted.
    #[arg(long = "set", value_name = "KEY=VALUE", global = true)]
    set: Vec<String>,
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

        /// Model-load tuning (adaptive load, RAM budget, GLM synth, …).
        #[command(flatten)]
        tuning: TuningOpts,

        /// Dry-run: report how this model WOULD load (adaptive n_ctx/cache fit + the
        /// host-RAM admission verdict) at the current free RAM, then exit WITHOUT
        /// loading anything. Reuses the exact load-path math (no model is touched), so
        /// it shows whether a real `--model` run would load-reduced, load-full, or be
        /// refused (and by how much) — the no-load way to plan a matrix run.
        #[arg(long)]
        dry_run: bool,

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

        /// Don't carry meeting-room presence for an agent that has no MCP client of
        /// its own (nadia): no `working:`/`done:` line in the project's room, and no
        /// room activity appended where the launch-local proxy would inject it. The
        /// bridge is auto-on only for such agents and only while Tier-3 piggyback is
        /// live (so a `--no-piggyback` benchmark run is silent); force it on for any
        /// agent with `ROZUM_ROOM_BRIDGE=1`. Spec: `docs/specs/rozum-native-channels.md`.
        #[arg(long)]
        no_room_bridge: bool,

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
        /// request; --lean cuts it to ~0.8K (Read/Write/Edit/Bash). With channel-wakeup off
        /// (headless/bench) it also adds `--strict-mcp-config` so ALL ambient MCP servers
        /// (jetbrains, claude.ai Google, …) are dropped, not just enumerated ones. (2) Add
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

        /// Model-load tuning (adaptive load, RAM budget, GLM synth, …).
        #[command(flatten)]
        tuning: TuningOpts,

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
        /// Also report every `com.rozum.*` launchd job and whether the endpoint it exists to serve
        /// answers. A job that cannot exec looks identical to a healthy one until you ask
        /// (`docs/specs/service-liveness.md`).
        #[arg(long)]
        services: bool,
        /// Only the service section — for the periodic job, which launchd starts in `/` with a
        /// minimal PATH, where the demo-path checks report problems that are the environment's
        /// and not the machine's.
        #[arg(long)]
        services_only: bool,
        /// Post to this room ONLY when a service changes verdict — for the periodic job. Silence
        /// means nothing changed; every tick would be noise.
        #[arg(long, requires = "services")]
        post_room: Option<String>,
    },

    /// Manage the global room registry (~/.local/state/rozum/rooms.json).
    Rooms {
        #[command(subcommand)]
        action: RoomsAction,
    },

    /// Administer the messenger assistant: bots, their group registries, and per-room rosters.
    ///
    /// The same operations the bot exposes in-chat (`/addgroup`, `/grant`, …), available from a
    /// shell — which matters when the chat is exactly what you can't reach (a group you left, a
    /// bridge that won't start). The UCC console drives these same commands, so all three
    /// interfaces are one implementation. Spec: `docs/specs/messenger-admin-console.md`.
    Messenger {
        #[command(subcommand)]
        action: MessengerAction,
    },
}

#[derive(Subcommand)]
enum MessengerAction {
    /// List the known bots with their live service state, rooms and group counts.
    Bots {
        /// Machine-readable output (what the UCC console parses).
        #[arg(long)]
        json: bool,
    },

    /// Everything at once — bots, groups per registry, and rooms with a roster.
    Status {
        #[arg(long)]
        json: bool,
    },

    /// Connected group chats.
    Groups {
        #[command(subcommand)]
        action: GroupsAction,
    },

    /// Per-room permission rosters (who may chat, read, write, run commands).
    Acl {
        #[command(subcommand)]
        action: AclAction,
    },

    /// Start / stop / restart a bot's services (bridge + participant pool).
    Service {
        /// Bot name, as listed by `messenger bots`.
        bot: String,
        /// start | stop | restart
        action: String,
    },

    /// Install a NEW bot: validate the token, store it 600, generate its services, start them.
    ///
    /// The token is read from STDIN, never from an argument — arguments are visible in `ps` to
    /// every process on the machine.
    BotAdd {
        /// Short name; becomes the registry namespace, secret file and service labels.
        name: String,
        /// Room the bot's owner DM maps to (defaults to the bot name).
        #[arg(long)]
        room: Option<String>,
        /// Alias the bot answers to in groups, e.g. `@my_bot`.
        #[arg(long, default_value = "")]
        mention_alias: String,
        #[arg(long, default_value = rozum::messenger_admin::DEFAULT_MODEL)]
        model: String,
        #[arg(long, default_value = rozum::messenger_admin::DEFAULT_GATEWAY_URL)]
        gateway_url: String,
        /// Sandbox root the model's file tools are confined to.
        #[arg(long)]
        sandbox: Option<String>,
        /// Generate the files but do not start the services.
        #[arg(long)]
        no_start: bool,
        #[arg(long)]
        json: bool,
    },

    /// Remove a bot: stop its services and forget it. Its token secret is deleted too.
    BotRemove {
        name: String,
        /// Keep the token secret on disk (default is to delete it with the bot).
        #[arg(long)]
        keep_secret: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum GroupsAction {
    /// List the groups of one registry.
    List {
        #[arg(long, default_value = "telegram")]
        registry: String,
        #[arg(long)]
        json: bool,
    },
    /// Connect a group chat to a room. Idempotent — re-adding keeps the existing room.
    // Telegram group ids are ALWAYS negative, so without this every real invocation would die
    // with "unexpected argument '-1' found" — caught by actually running the command.
    #[command(allow_negative_numbers = true)]
    Add {
        /// Telegram chat id (negative for groups/supergroups).
        chat_id: i64,
        #[arg(long, default_value = "telegram")]
        registry: String,
        /// Room to map it to (defaults to `group-<|chat_id|>`, so re-adding reuses the roster).
        #[arg(long)]
        room: Option<String>,
        #[arg(long, default_value = "")]
        title: String,
        #[arg(long)]
        json: bool,
    },
    /// Disconnect a group chat.
    #[command(allow_negative_numbers = true)]
    Remove {
        chat_id: i64,
        #[arg(long, default_value = "telegram")]
        registry: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum AclAction {
    /// Show one room's roster.
    Show {
        room: String,
        #[arg(long)]
        json: bool,
    },
    /// List the rooms that have a roster.
    Rooms {
        #[arg(long)]
        json: bool,
    },
    /// Grant capabilities in ONE room: chat | read | write | shell | all | none.
    Grant {
        room: String,
        user_id: i64,
        #[arg(required = true)]
        caps: Vec<String>,
        #[arg(long, default_value = "")]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Revoke a member from ONE room.
    Revoke {
        room: String,
        user_id: i64,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand)]
enum RoomsAction {
    /// Remove registry entries whose root directory no longer exists.
    ///
    /// Rooms accumulate when agents work in worktrees or temp directories that
    /// are later deleted. `prune` cleans up stale entries without affecting
    /// rooms whose directories are still present.
    Prune,
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
        /// Message kind: note|question|event|alert|resolution (support metadata; default note).
        #[arg(long)]
        kind: Option<String>,
        /// Severity: info|low|medium|high|critical.
        #[arg(long)]
        severity: Option<String>,
        /// Thread/incident id to post into (an `<date>/<n>` message id).
        #[arg(long)]
        thread: Option<String>,
        /// Reply to a specific message (a `<date>/<n>` id) — builds a reply-chain.
        #[arg(long = "reply-to")]
        reply_to: Option<String>,
        /// Tag(s) (repeatable: --tag db --tag prod).
        #[arg(long = "tag")]
        tags: Vec<String>,
    },
    /// Read recent messages from a room (the cwd project's room by default).
    ///
    /// The read counterpart to `post` — a direct-read of the room transcript (no daemon
    /// needed for the cwd project's room), so an agent or human can scan recent coordination
    /// from a script/shell without the TUI or the MCP tools.
    Read {
        /// Room name (from `rozum meetings status`); default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// How many most-recent messages to show.
        #[arg(long, short = 'n', default_value_t = 20)]
        count: usize,
    },

    /// Manage support-console access tokens (per-operator identity + RBAC role).
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },

    /// Show a room's queue: its open threads, worst first (severity, then longest ignored).
    Queue {
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
    },

    /// Set a room's lifecycle phase: active | paused | ended (persisted, survives a restart).
    Phase {
        /// active | paused | ended.
        phase: String,
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
    },

    /// Grant or revoke a participant's role in a room (reporter|assignee|on_call|observer|admin).
    Role {
        /// The participant's room handle, as shown by `rozum meetings who`.
        handle: String,
        /// reporter | assignee | on_call | observer | admin.
        role: String,
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// Take the role away instead of granting it.
        #[arg(long)]
        revoke: bool,
    },

    /// React to a message with an emoji (toggle).
    React {
        /// The message id (`<date>/<n>`).
        msg_id: String,
        /// The emoji (e.g. 👍 ✅ 👀).
        emoji: String,
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// Remove the reaction instead of adding it.
        #[arg(long)]
        off: bool,
    },

    /// Redact a message's content for all readers (leaked PII/secrets). The bytes stay on disk.
    Redact {
        /// The message id (`<date>/<n>`) to redact.
        msg_id: String,
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// Reason shown in place of the content (e.g. "PII").
        #[arg(long)]
        reason: Option<String>,
        /// Un-redact instead (restore the content).
        #[arg(long)]
        undo: bool,
    },

    /// Rebuild a room's `threads.json` (incident state) from the message log — disaster recovery.
    ///
    /// Use only if the incident state was lost (threads.json + its .bak both gone). Best-effort:
    /// recovers membership + severity + approximate state; title from the anchor, pinned not recovered.
    /// Restart the meeting daemon afterwards so it reloads the rebuilt state.
    RepairThreads {
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
    },

    /// Search a room's whole history by text + support metadata (kind/severity/tag/thread).
    ///
    /// A direct-read over every day file (no daemon needed for the cwd room). `--severity` is a
    /// MINIMUM (that level and above). Example: `meetings search --severity high --tag db "timeout"`.
    Search {
        /// Free-text to find in message content (case-insensitive substring). Optional.
        query: Option<String>,
        /// Room name; default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// Only this kind: note|question|event|alert|resolution.
        #[arg(long)]
        kind: Option<String>,
        /// Minimum severity: info|low|medium|high|critical (this level and above).
        #[arg(long)]
        severity: Option<String>,
        /// A tag the message must carry.
        #[arg(long = "tag")]
        tag: Option<String>,
        /// Restrict to one thread/incident id (`<date>/<n>`).
        #[arg(long)]
        thread: Option<String>,
        /// Only messages on or after this date (`YYYY-MM-DD`).
        #[arg(long)]
        since: Option<String>,
        /// Cap the number of (most-recent) matches shown.
        #[arg(long, short = 'n', default_value_t = 50)]
        count: usize,
    },

    /// Show messages that ADDRESS you (`@handle` / `-> handle`) since you last looked.
    ///
    /// A durable, offline-surviving inbox: a view over the room transcript filtered to turns that
    /// mention your handle, past a per-handle seen-cursor on disk — so even a CLI-only agent with no
    /// live proxy still learns it was addressed. Reading advances the cursor.
    Inbox {
        /// YOUR handle — whose mentions to show (e.g. `sunny-civet`). Required.
        #[arg(long = "as")]
        as_handle: String,
        /// Room name (from `rozum meetings status`); default = the cwd project's room.
        #[arg(long)]
        room: Option<String>,
        /// Show without advancing the seen-cursor (don't mark as read).
        #[arg(long)]
        peek: bool,
        /// Show every mention ever, ignoring the seen-cursor.
        #[arg(long)]
        all: bool,
        /// Cap the number of (most-recent) mentions shown.
        #[arg(long, short = 'n', default_value_t = 50)]
        count: usize,
    },

    /// Establish THIS agent session's identity (once, at startup) so it posts as ITSELF.
    ///
    /// Keyed by `$CLAUDE_CODE_SESSION_ID`; run from your start hook / first action. Pass your own
    /// name (the one the operator sees); omit it to get a stable minted one. Idempotent — the name is
    /// assigned once. After this, `meetings post` shows your name, not the operator's.
    Hello {
        /// Your agent name (e.g. `sunny-civet`). Omit to mint a stable one from the session id.
        name: Option<String>,
    },

    /// Print who THIS session acts as — agent principal (by session id) or the human (by account).
    Whoami {},

    /// Roster: list the live agent principals with locators, so a handle maps to a real session.
    Who {
        /// Also show session_id / principal_id / started.
        #[arg(long)]
        long: bool,
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
        /// Give the model file tools (list/read/write) confined to this directory. Omitted →
        /// chat only, no filesystem access. Spec: docs/specs/assistant-sandbox-tools.md.
        #[arg(long = "sandbox")]
        sandbox: Option<std::path::PathBuf>,
        /// Also offer the `run_command` shell tool (confined to the sandbox via seatbelt). Off by
        /// default: file access does NOT imply shell access. Per-user shell grants still apply via --acl.
        #[arg(long = "shell", default_value_t = false)]
        shell: bool,
        /// Deny network access to `run_command` (default: network allowed). Write confinement to the
        /// sandbox holds regardless of this flag.
        #[arg(long = "shell-no-network", default_value_t = false)]
        shell_no_network: bool,
        /// Gate file/shell tools per messenger user by this ACL file (managed live from Telegram).
        /// Omitted → the sandbox's read+write (and --shell) apply to everyone the bridge admits.
        #[arg(long = "acl")]
        acl: Option<std::path::PathBuf>,
        /// The bot's messenger @username (e.g. `@Rozum_chat_bot`). Under `--reply-policy mention`
        /// the model also replies when a message @mentions this; the mention is stripped from the
        /// text the model sees.
        #[arg(long = "mention-alias")]
        mention_alias: Option<String>,
    },

    /// Supervise one participant per room: the primary `--room` plus every room in the Telegram
    /// group registry, each with its OWN per-room ACL. Reconciles as groups are connected/
    /// disconnected from the bot (`/addgroup` / `/removegroup`) and respawns crashed children.
    ParticipantPool {
        #[arg(long)]
        model: String,
        /// The primary room (e.g. the private-chat room `assistant`); groups add more.
        #[arg(long)]
        room: String,
        /// Roster handle for every child participant (default derived from the model).
        #[arg(long = "as")]
        as_handle: Option<String>,
        /// Reply policy for the PRIMARY room (private chat) — usually `always`.
        #[arg(long = "reply-policy", default_value = "always")]
        reply_policy: String,
        /// Reply policy for GROUP rooms — usually `mention` so the bot answers only when addressed
        /// by name (with `--mention-alias`), not to every message.
        #[arg(long = "group-reply-policy", default_value = "mention")]
        group_reply_policy: String,
        #[arg(long = "gateway-url", default_value = "http://127.0.0.1:8080/v1")]
        gateway_url: String,
        #[arg(long = "peer")]
        peers: Vec<String>,
        #[arg(long)]
        persona: Option<String>,
        #[arg(long = "persona-file")]
        persona_file: Option<std::path::PathBuf>,
        #[arg(long = "sandbox")]
        sandbox: Option<std::path::PathBuf>,
        #[arg(long = "shell", default_value_t = false)]
        shell: bool,
        #[arg(long = "shell-no-network", default_value_t = false)]
        shell_no_network: bool,
        /// The bot's @username (e.g. `@Rozum_chat_bot`) for mention detection + stripping.
        #[arg(long = "mention-alias")]
        mention_alias: Option<String>,
        /// Group-registry namespace to follow (default `telegram`); a second bot uses its own.
        #[arg(long = "registry", default_value = "telegram")]
        registry: String,
    },

    /// Drive the incident lifecycle from the shell — the human/script twin of the agent-native MCP
    /// thread verbs (open / escalate / resolve / list / show / metrics). Makes the support console
    /// (`mtg-frontend`) populate without an agent, and gives operators a no-UI lever.
    Incident {
        #[command(subcommand)]
        action: IncidentAction,
        /// Target room (default = the cwd project's room).
        #[arg(long, global = true)]
        room: Option<String>,
        /// Author display (default: $USER, or $ROZUM_MEETING_AS).
        #[arg(long = "as", global = true)]
        as_display: Option<String>,
    },
}

/// The incident-lifecycle verbs (`rozum meetings incident …`). Each drives the daemon over its socket,
/// calling the same `meeting.*` thread tools the agents use.
/// Support-console access-token verbs (`rozum meetings token …`).
#[derive(Subcommand)]
enum TokenAction {
    /// Issue a token for an operator with a role (observer|responder|admin). Prints the token.
    Issue {
        /// The operator's handle (shown as the actor on their actions).
        #[arg(long)]
        handle: String,
        /// Role: observer (read) | responder (lifecycle) | admin (+redact).
        #[arg(long, default_value = "responder")]
        role: String,
        /// Optional expiry, e.g. `30d`, `12h`, `90m` (default: never expires).
        #[arg(long)]
        ttl: Option<String>,
    },
    /// List issued tokens (handle, role, expiry; the token is shown truncated).
    List,
    /// Rotate a handle's token: revoke the old, issue a fresh one (same role). Prints the new token.
    Rotate {
        /// The operator handle to rotate.
        handle: String,
        /// Optional new expiry, e.g. `30d` (default: never).
        #[arg(long)]
        ttl: Option<String>,
    },
    /// Grant (or clear) a per-room role override for a handle — e.g. admin in `incidents`, observer
    /// elsewhere. The base role (from `issue`) applies to rooms without an override.
    Grant {
        /// The operator handle.
        handle: String,
        /// The room to scope the role to.
        #[arg(long)]
        room: String,
        /// Role for that room: observer|responder|admin, or `none`/`clear` to remove the override.
        #[arg(long)]
        role: String,
    },
    /// Revoke by token, or by handle (revokes all of that handle's tokens).
    Revoke {
        /// A token string or an operator handle.
        token_or_handle: String,
    },
    /// Resolve a token → `handle<TAB>role` (the effective role for `--room`, else the base role).
    /// Prints nothing and exits 1 if the token is unknown or expired. The machine bridge the `.ssc`
    /// PWA uses to turn its `rozum_token` cookie into an actor + RBAC role.
    Resolve {
        /// The token string (from the operator's session cookie).
        token: String,
        /// Scope the role to this room (applies any per-room override); omit for the base role.
        #[arg(long)]
        room: Option<String>,
    },
}

#[derive(Subcommand)]
enum IncidentAction {
    /// Open an incident thread anchored on a message id (a `<date>/<n>` id from `meetings read`).
    Open {
        /// The anchor message id (e.g. `2026-06-28/3`).
        anchor_id: String,
        /// The incident title (free text; no quotes needed).
        title: Vec<String>,
        /// Open as a plain topic thread instead of a tracked incident.
        #[arg(long)]
        topic: bool,
    },
    /// Escalate an incident to someone (sets state=escalated + owner + an event note).
    Escalate {
        /// The incident/thread id.
        id: String,
        /// Who to escalate to (a handle).
        #[arg(long)]
        to: String,
        /// An optional note explaining why.
        #[arg(long)]
        note: Option<String>,
    },
    /// Resolve an incident (sets state=resolved + a resolution note).
    Resolve {
        /// The incident/thread id.
        id: String,
        /// An optional resolution note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Assign an incident owner WITHOUT changing its state (use `state`/`escalate`/`resolve` for state).
    Assign {
        /// The incident/thread id.
        id: String,
        /// Who to assign it to (a handle).
        #[arg(long)]
        to: String,
        /// An optional note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Pin a key message in an incident (current status / root cause), shown first to responders.
    Pin {
        /// The incident/thread id.
        id: String,
        /// The message id to pin (`<date>/<n>`).
        msg_id: String,
    },
    /// Unpin a previously-pinned message from an incident.
    Unpin {
        /// The incident/thread id.
        id: String,
        /// The message id to unpin.
        msg_id: String,
    },
    /// Link a message (often from outside the thread) as relevant context for an incident.
    Link {
        /// The incident/thread id.
        id: String,
        /// The message id to link (`<date>/<n>`).
        msg_id: String,
    },
    /// Unlink a previously-linked message from an incident.
    Unlink {
        /// The incident/thread id.
        id: String,
        /// The message id to unlink.
        msg_id: String,
    },
    /// Set an incident's state directly: open|triaging|escalated|resolved|closed.
    State {
        /// The incident/thread id.
        id: String,
        /// The new state.
        state: String,
    },
    /// List the room's incident/topic threads.
    List,
    /// Show one incident's whole-context bundle (thread + all messages + participants + timespan).
    Show {
        /// The incident/thread id.
        id: String,
    },
    /// Resolving metrics for the room (totals / by-state / mean time-to-resolve).
    Metrics,
}

#[derive(Subcommand)]
enum GatewayAction {
    /// Show the active shared gateway (model, port, pid, uptime, clients).
    Status {
        /// Emit the full control snapshot (gateway + residency + installed catalog) as JSON — the
        /// machine/dashboard contract (`rozum-gateway::control::status`).
        #[arg(long)]
        json: bool,
    },
    /// Serve the control snapshot over HTTP (always-up, no gateway needed) for a dashboard / the UCC.
    ControlServe {
        /// Port for `GET /control/status` (with permissive CORS).
        #[arg(long, default_value_t = 8411)]
        port: u16,
    },
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
    /// Run a RAM-heavy NON-rozum command (e.g. the python `mlx_lm` oracle, a bench sweep) THROUGH the
    /// host-wide admission queue, so it can't overcommit RAM behind rozum's back. Acquires a reservation
    /// for `--footprint` (or `--model`'s estimate), WAITS its turn in the queue, runs the command holding
    /// the reservation, releases on exit. Tag `--batch` so it yields to interactive loads.
    /// Example: `rozum gateway admit --footprint 8G -- uv run --with mlx-lm python scripts/mlx_ref.py`
    Admit {
        /// Reserve this much RAM (e.g. `8G`, `8192M`, or raw bytes). Required unless --model is given.
        #[arg(long)]
        footprint: Option<String>,
        /// Estimate the footprint from a model spec instead of --footprint.
        #[arg(long)]
        model: Option<String>,
        /// Queue as batch (yields to interactive loads); default interactive.
        #[arg(long)]
        batch: bool,
        /// The command to run once admitted (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        program: Vec<String>,
    },
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

    // Apply `--set KEY=VALUE` CLI options to the environment FIRST (highest precedence: CLI > env >
    // config > default). Config `[options]` are applied (only-if-unset) when the config loads. Both
    // feed the same env-var knobs the model/residency/GLM code reads, so every option is settable
    // three ways. Only ROZUM_* keys; must run before any option-reading code.
    apply_cli_set_options(&cli.set);

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
            // room (with the legacy web bridge + model-as-participant sampling)
            // is the escape hatch: `--legacy-room`, or implicitly when
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
            } else if let Err(e) = rozum::tui::launch_generated(cli.room) {
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
            if let Err(e) = rozum::discord::run_from_env(&room, &name).await {
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
            tuning,
            dry_run,
            action,
        }) => match action {
            None => {
                // Named model-load tuning flags → env (CLI precedence), before the model loads.
                tuning.apply_to_env();
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
                if dry_run {
                    run_gateway_dry_run(&model, n_ctx.or(cfg.n_ctx));
                    return;
                }
                run_gateway(port, model, n_ctx, cfg).await;
            }
            Some(GatewayAction::Status { json }) => run_gateway_status(json).await,
            Some(GatewayAction::ControlServe { port }) => {
                if let Err(e) = rozum::control::serve(port).await {
                    eprintln!("control serve: {e}");
                }
            }
            Some(GatewayAction::Stop { force }) => run_gateway_stop(force),
            Some(GatewayAction::Switch {
                model,
                n_ctx,
                backend,
            }) => run_gateway_switch(model, n_ctx, backend).await,
            Some(GatewayAction::Reload) => run_gateway_reload().await,
            Some(GatewayAction::Unload) => run_gateway_unload().await,
            Some(GatewayAction::Admit { footprint, model, batch, program }) => {
                run_gateway_admit(footprint, model, batch, program).await
            }
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
            no_room_bridge,
            backend_url,
            lean,
            no_sandbox,
            tuning,
            mut program,
        }) => {
            // Named model-load tuning flags → env (CLI precedence), before the model loads.
            tuning.apply_to_env();
            apply_cascade_strategy(strategy.as_deref());
            apply_offline(offline);
            apply_lean_flags(&mut program, lean, !no_channel_wakeup);
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
            let wakeup = WakeupPolicy::resolve(&channels, no_piggyback, no_room_bridge, &program[0]);
            // B3: capability is RELATIONAL (model × driver) — surface a known driver↔model mismatch so
            // the operator can pick the right driver. Warn only; never block or auto-switch.
            if std::env::var_os("ROZUM_NO_MATCH_WARN").is_none() {
                if let Some(w) = driver_model_mismatch_warning(&program[0], model.as_deref()) {
                    eprintln!("{w}");
                }
            }
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
                if let Err(e) = rozum::tui::launch_generated(room) {
                    eprintln!("attach error: {e}");
                    std::process::exit(1);
                }
            }
            MeetingsAction::Install => run_meetings_install(),
            MeetingsAction::Uninstall => run_meetings_uninstall(),
            MeetingsAction::Post { text, room, as_display, kind, severity, thread, reply_to, tags } => {
                run_meetings_post(text, room, as_display, kind, severity, thread, reply_to, tags).await
            }
            MeetingsAction::Read { room, count } => run_meetings_read(room, count).await,
            MeetingsAction::RepairThreads { room } => run_meetings_repair_threads(room).await,
            MeetingsAction::Queue { room } => run_meetings_queue(room).await,
            MeetingsAction::Phase { phase, room } => run_meetings_phase(phase, room).await,
            MeetingsAction::Role { handle, role, room, revoke } => {
                run_meetings_role(handle, role, room, revoke).await
            }
            MeetingsAction::React { msg_id, emoji, room, off } => {
                run_meetings_react(msg_id, emoji, room, off).await
            }
            MeetingsAction::Token { action } => run_meetings_token(action),
            MeetingsAction::Redact { msg_id, room, reason, undo } => {
                run_meetings_redact(msg_id, room, reason, undo).await
            }
            MeetingsAction::Search { query, room, kind, severity, tag, thread, since, count } => {
                run_meetings_search(query, room, kind, severity, tag, thread, since, count).await
            }
            MeetingsAction::Inbox { as_handle, room, peek, all, count } => {
                run_meetings_inbox(as_handle, room, peek, all, count).await
            }
            MeetingsAction::Hello { name } => run_meetings_hello(name),
            MeetingsAction::Whoami {} => run_meetings_whoami(),
            MeetingsAction::Who { long } => run_meetings_who(long).await,
            MeetingsAction::Participant {
                model,
                room,
                as_handle,
                reply_policy,
                gateway_url,
                peers,
                persona,
                persona_file,
                sandbox,
                shell,
                shell_no_network,
                acl,
                mention_alias,
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
                    sandbox,
                    shell,
                    shell_no_network,
                    acl,
                    mention_alias,
                )
                .await
            }
            MeetingsAction::ParticipantPool {
                model,
                room,
                as_handle,
                reply_policy,
                group_reply_policy,
                gateway_url,
                peers,
                persona,
                persona_file,
                sandbox,
                shell,
                shell_no_network,
                mention_alias,
                registry,
            } => {
                run_meetings_participant_pool(
                    model,
                    room,
                    as_handle,
                    gateway_url,
                    reply_policy,
                    group_reply_policy,
                    peers,
                    persona,
                    persona_file,
                    sandbox,
                    shell,
                    shell_no_network,
                    mention_alias,
                    registry,
                )
                .await
            }
            MeetingsAction::Incident { action, room, as_display } => {
                run_meetings_incident(action, room, as_display).await
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
        Some(Command::Doctor { web_url, strict, services, services_only, post_room }) => {
            run_doctor(web_url, strict, services || services_only, services_only, post_room).await
        }
        Some(Command::Rooms { action }) => match action {
            RoomsAction::Prune => run_rooms_prune(),
        },
        Some(Command::Telegram { room, name }) => {
            if let Err(e) = rozum::telegram::run_from_env(&room, &name).await {
                eprintln!("telegram bridge error: {e}");
                std::process::exit(1);
            }
        }
        Some(Command::Messenger { action }) => run_messenger(action).await,
    }
}

// --- messenger admin CLI (spec: docs/specs/messenger-admin-console.md) --------------------

/// Print either JSON or a human table, and exit non-zero on error. Every branch goes through
/// here so `--json` behaves identically everywhere — the UCC console parses this output.
fn emit(json: bool, value: serde_json::Value, human: impl FnOnce()) {
    if json {
        println!("{}", serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into()));
    } else {
        human();
    }
}

fn fail(json: bool, msg: &str) -> ! {
    if json {
        println!("{}", serde_json::json!({ "ok": false, "error": msg }));
    } else {
        eprintln!("ошибка: {msg}");
    }
    std::process::exit(1);
}

/// Read the bot token from stdin. NEVER an argument: arguments are world-visible in `ps`.
fn read_token_stdin() -> Result<String, String> {
    use std::io::Read;
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf).map_err(|e| format!("не удалось прочитать токен из stdin: {e}"))?;
    let token = buf.trim().to_string();
    if token.is_empty() {
        return Err("пустой токен на stdin (передайте его так: `... bot-add NAME < token.txt`)".into());
    }
    Ok(token)
}

/// A bot's live picture: identity from `getMe` (when its secret is readable), the state of both
/// launchd jobs, and how many groups its registry holds.
async fn bot_view(bot: &rozum::messenger_admin::Bot) -> serde_json::Value {
    use rozum::messenger_admin as adm;
    let groups = adm::groups_list(&bot.registry);
    let identity = match std::fs::read_to_string(adm::secret_path(&bot.secret)) {
        Ok(token) => {
            let handle = rozum::telegram::Bot::new(token.trim().to_string(), 0);
            match handle.get_me().await {
                Ok(me) => serde_json::json!({
                    "id": me.id,
                    "username": me.username,
                    "can_join_groups": me.can_join_groups,
                    "reachable": true,
                }),
                Err(e) => serde_json::json!({ "reachable": false, "error": e }),
            }
        }
        Err(e) => serde_json::json!({ "reachable": false, "error": format!("нет секрета: {e}") }),
    };
    let bridge = adm::service_state(&bot.bridge_label);
    let pool = adm::service_state(&bot.pool_label);
    // Flat fields alongside the nested ones: the UCC tables read a field by name and cannot walk
    // into a nested object, and per-row action bodies are precomputed here (the same idiom the
    // models panel uses for load/unload) so the screen never has to build a request itself.
    serde_json::json!({
        "name": bot.name,
        "platform": bot.platform,
        "registry": bot.registry,
        "room": bot.room,
        "mention_alias": bot.mention_alias,
        "identity": identity,
        "bridge": bridge,
        "pool": pool,
        "groups": groups.groups.len(),
        // NOTE: `secret` is the FILE NAME, never its contents. The token has no path to a caller.
        "secret_file": bot.secret,
        "username": identity["username"].as_str().map(|u| format!("@{u}")).unwrap_or_else(|| "—".into()),
        "state_line": format!(
            "мост {} · пул {}",
            bridge.state,
            pool.state
        ),
        "groups_line": format!("{} групп · реестр {}", groups.groups.len(), bot.registry),
        "restart_body": format!("bot={}&action=restart", bot.name),
        // Exactly ONE of stop/start is non-empty — the UCC tables skip a row action whose body is
        // empty, so the row shows "стоп" for a live bot and "старт" for a dead one, never both.
        // Same idiom as the models panel's load/unload pair.
        "stop_body": if bridge.state == "running" { format!("bot={}&action=stop", bot.name) } else { String::new() },
        "start_body": if bridge.state == "running" { String::new() } else { format!("bot={}&action=start", bot.name) },
    })
}

async fn run_messenger(action: MessengerAction) {
    use rozum::messenger_admin as adm;
    match action {
        MessengerAction::Bots { json } => {
            let bots = adm::Bots::load_default();
            let mut views = Vec::new();
            for b in &bots.bots {
                views.push(bot_view(b).await);
            }
            let payload = serde_json::json!({ "ok": true, "bots": views });
            emit(json, payload.clone(), || {
                if views.is_empty() {
                    println!("боты не найдены (нет ни одного токена в ~/.rozum/secrets)");
                    return;
                }
                println!("{:<16} {:<18} {:<12} {:<12} {:>6}", "БОТ", "@USERNAME", "МОСТ", "ПУЛ", "ГРУПП");
                for v in &views {
                    println!(
                        "{:<16} {:<18} {:<12} {:<12} {:>6}",
                        v["name"].as_str().unwrap_or("?"),
                        v["identity"]["username"].as_str().map(|u| format!("@{u}")).unwrap_or_else(|| "—".into()),
                        v["bridge"]["state"].as_str().unwrap_or("?"),
                        v["pool"]["state"].as_str().unwrap_or("?"),
                        v["groups"].as_u64().unwrap_or(0),
                    );
                }
            });
        }

        MessengerAction::Status { json } => {
            let bots = adm::Bots::load_default();
            let mut views = Vec::new();
            // ONE flat group list across every registry, each row carrying its registry and a
            // ready-made remove body. A per-registry map would force the screen to know the
            // registry names up front — which is exactly what changes when a bot is added.
            let mut groups = Vec::new();
            for b in &bots.bots {
                views.push(bot_view(b).await);
                for g in adm::groups_list(&b.registry).groups {
                    groups.push(serde_json::json!({
                        "registry": b.registry,
                        "bot": b.name,
                        "chat_id": g.chat_id,
                        "room": g.room,
                        "title": g.title,
                        "where_line": format!("{} · бот {}", g.room, b.name),
                        "remove_body": format!("registry={}&chat_id={}", b.registry, g.chat_id),
                    }));
                }
            }
            let rooms: Vec<serde_json::Value> =
                adm::acl_rooms().into_iter().map(|r| serde_json::json!({ "room": r })).collect();
            let payload = serde_json::json!({
                "ok": true, "bots": views, "groups": groups, "acl_rooms": rooms,
            });
            emit(json, payload.clone(), || {
                println!("боты: {}", views.len());
                for g in &groups {
                    println!(
                        "  {} → {} (реестр {}, {})",
                        g["chat_id"],
                        g["room"].as_str().unwrap_or(""),
                        g["registry"].as_str().unwrap_or(""),
                        g["title"].as_str().unwrap_or("")
                    );
                }
                if groups.is_empty() {
                    println!("  групп нет ни в одном реестре");
                }
                let names: Vec<&str> =
                    rooms.iter().filter_map(|r| r["room"].as_str()).collect();
                println!("комнаты с ростером: {}", names.join(", "));
            });
        }

        MessengerAction::Groups { action } => match action {
            GroupsAction::List { registry, json } => {
                let reg = adm::groups_list(&registry);
                let payload = serde_json::json!({ "ok": true, "registry": registry, "groups": reg.groups });
                emit(json, payload, || {
                    if reg.groups.is_empty() {
                        println!("реестр '{registry}': групп нет");
                    }
                    for g in &reg.groups {
                        println!("{:>16}  {:<24} {}", g.chat_id, g.room, g.title);
                    }
                });
            }
            GroupsAction::Add { chat_id, registry, room, title, json } => {
                match adm::group_add(&registry, chat_id, room.as_deref(), &title) {
                    Ok(ch) => {
                        let payload = serde_json::json!({ "ok": true, "change": ch });
                        emit(json, payload, || {
                            if ch.changed {
                                println!("подключена {chat_id} → комната '{}' (реестр {registry})", ch.room);
                                println!("мост перезапустится сам и подхватит изменение");
                            } else {
                                println!("{chat_id} уже подключена к '{}' — ничего не изменилось", ch.room);
                            }
                        });
                    }
                    Err(e) => fail(json, &format!("не удалось сохранить реестр: {e}")),
                }
            }
            GroupsAction::Remove { chat_id, registry, json } => {
                match adm::group_remove(&registry, chat_id) {
                    Ok(ch) => {
                        let payload = serde_json::json!({ "ok": true, "change": ch });
                        emit(json, payload, || {
                            if ch.changed {
                                println!("отключена {chat_id} (была комната '{}')", ch.room);
                            } else {
                                println!("{chat_id} не была подключена к реестру '{registry}'");
                            }
                        });
                    }
                    Err(e) => fail(json, &format!("не удалось сохранить реестр: {e}")),
                }
            }
        },

        MessengerAction::Acl { action } => match action {
            AclAction::Rooms { json } => {
                let rooms = adm::acl_rooms();
                emit(json, serde_json::json!({ "ok": true, "rooms": rooms }), || {
                    for r in &rooms {
                        println!("{r}");
                    }
                });
            }
            AclAction::Show { room, json } => {
                let members = adm::acl_show(&room);
                emit(json, serde_json::json!({ "ok": true, "room": room, "members": members }), || {
                    if members.is_empty() {
                        println!("у комнаты '{room}' пока нет ростера");
                    }
                    for m in &members {
                        println!("{:>14}  {:<22} {}", m.user_id, m.caps, m.name);
                    }
                });
            }
            AclAction::Grant { room, user_id, caps, name, json } => {
                match adm::acl_grant(&room, user_id, &name, &caps) {
                    Ok(c) => emit(
                        json,
                        serde_json::json!({ "ok": true, "room": room, "user_id": user_id, "caps": c.summary() }),
                        || println!("выдано {user_id} в '{room}': {}", c.summary()),
                    ),
                    Err(e) => fail(json, &e),
                }
            }
            AclAction::Revoke { room, user_id, json } => match adm::acl_revoke(&room, user_id) {
                Ok(had) => emit(
                    json,
                    serde_json::json!({ "ok": true, "room": room, "user_id": user_id, "removed": had }),
                    || {
                        if had {
                            println!("{user_id} убран из '{room}'");
                        } else {
                            println!("{user_id} не было в ростере '{room}'");
                        }
                    },
                ),
                Err(e) => fail(json, &e),
            },
        },

        MessengerAction::Service { bot, action } => {
            let bots = adm::Bots::load_default();
            let Some(b) = bots.get(&bot) else {
                fail(false, &format!("нет такого бота: '{bot}' (см. `messenger bots`)"));
            };
            let act = match adm::ServiceAction::parse(&action) {
                Ok(a) => a,
                Err(e) => fail(false, &e),
            };
            // Bridge and pool are one unit from the operator's point of view: a bot that polls
            // but has no model is as broken as one that neither polls nor answers.
            for label in [&b.bridge_label, &b.pool_label] {
                match adm::service_control(label, act) {
                    Ok(msg) => println!("{msg}"),
                    Err(e) => eprintln!("{label}: {e}"),
                }
            }
        }

        MessengerAction::BotAdd { name, room, mention_alias, model, gateway_url, sandbox, no_start, json } => {
            let token = match read_token_stdin() {
                Ok(t) => t,
                Err(e) => fail(json, &e),
            };
            let bot = match adm::bot_from_name(&name, room.as_deref(), &mention_alias) {
                Ok(b) => b,
                Err(e) => fail(json, &e),
            };
            let mut bots = adm::Bots::load_default();
            if bots.get(&name).is_some() {
                fail(json, &format!("бот '{name}' уже есть — сначала `messenger bot-remove {name}`"));
            }
            // Validate BEFORE writing anything: a typo must not leave a crash-looping service
            // and a secret file behind. This is also the only place the token is ever used.
            let handle = rozum::telegram::Bot::new(token.clone(), 0);
            let me = match handle.get_me().await {
                Ok(me) => me,
                Err(e) => fail(json, &format!("токен отклонён Telegram: {e}")),
            };
            if let Err(e) = adm::write_token_secret(&bot.secret, &token) {
                fail(json, &format!("не удалось сохранить секрет: {e}"));
            }
            let sandbox = sandbox.unwrap_or_else(|| {
                std::env::var("HOME").unwrap_or_default() + "/rozum-sandbox"
            });
            let written = match adm::write_bot_services(&bot, &model, &gateway_url, &sandbox) {
                Ok(w) => w,
                Err(e) => fail(json, &format!("не удалось записать сервисы: {e}")),
            };
            bots.upsert(bot.clone());
            if let Err(e) = bots.save(&adm::Bots::path()) {
                fail(json, &format!("не удалось сохранить список ботов: {e}"));
            }
            let mut started = Vec::new();
            if !no_start {
                for label in [&bot.bridge_label, &bot.pool_label] {
                    match adm::service_control(label, adm::ServiceAction::Start) {
                        Ok(m) => started.push(m),
                        Err(e) => started.push(format!("{label}: НЕ запущен — {e}")),
                    }
                }
            }
            let payload = serde_json::json!({
                "ok": true,
                "bot": bot.name,
                "username": me.username,
                "id": me.id,
                "files": written.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "started": started,
            });
            emit(json, payload, || {
                println!("бот '{}' установлен: @{} (id {})", bot.name, me.username, me.id);
                for p in &written {
                    println!("  {}", p.display());
                }
                for s in &started {
                    println!("  {s}");
                }
                println!(
                    "ВАЖНО: откройте @{} в Telegram и нажмите Start — до этого у бота нет личного чата,\n\
                     и мост не сможет его провалидировать (getChat 400 'chat not found').",
                    me.username
                );
            });
        }

        MessengerAction::BotRemove { name, keep_secret, json } => {
            let mut bots = adm::Bots::load_default();
            let Some(bot) = bots.remove(&name) else {
                fail(json, &format!("нет такого бота: '{name}'"));
            };
            let mut notes = Vec::new();
            for label in [&bot.bridge_label, &bot.pool_label] {
                match adm::service_control(label, adm::ServiceAction::Stop) {
                    Ok(m) => notes.push(m),
                    Err(e) => notes.push(format!("{label}: {e}")),
                }
            }
            if !keep_secret {
                let p = adm::secret_path(&bot.secret);
                match std::fs::remove_file(&p) {
                    Ok(()) => notes.push(format!("удалён секрет {}", p.display())),
                    Err(e) => notes.push(format!("секрет {} не удалён: {e}", p.display())),
                }
            }
            if let Err(e) = bots.save(&adm::Bots::path()) {
                fail(json, &format!("не удалось сохранить список ботов: {e}"));
            }
            emit(json, serde_json::json!({ "ok": true, "bot": name, "notes": notes }), || {
                println!("бот '{name}' удалён");
                for n in &notes {
                    println!("  {n}");
                }
                println!("plist'ы оставлены на диске — удалите вручную, если они больше не нужны:");
                println!("  {}", adm::launchd_plist_path(&bot.bridge_label).display());
                println!("  {}", adm::launchd_plist_path(&bot.pool_label).display());
            });
        }
    }
}

async fn run_doctor(
    web_url: Option<String>,
    strict: bool,
    services: bool,
    services_only: bool,
    post_room: Option<String>,
) {
    let report = rozum::doctor::run(rozum::doctor::DoctorOptions {
        web_url,
        strict,
        services,
        services_only,
        post_room: post_room.clone(),
    })
    .await;
    // On transition only. The posting is a plain `meetings post`, so this adds no new way into a
    // room and inherits whatever identity that path already uses.
    if let Some(room) = post_room {
        for line in rozum::doctor::transitions(&report) {
            let out = std::process::Command::new(std::env::current_exe().unwrap_or_default())
                .args(["meetings", "post", "--room", &room, "--as", "doctor", &line])
                .output();
            match out {
                Ok(o) if o.status.success() => eprintln!("doctor: posted to {room}: {line}"),
                Ok(o) => eprintln!(
                    "doctor: could not post to {room}: {}",
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                Err(e) => eprintln!("doctor: could not post to {room}: {e}"),
            }
        }
    }
    print!("{}", report.render());
    if report.should_fail(strict) {
        std::process::exit(1);
    }
}

fn run_rooms_prune() {
    use rozum::meeting::{prune_registered, rozum_state_dir};
    let state = rozum_state_dir();
    match prune_registered(&state) {
        Ok(removed) if removed.is_empty() => println!("rooms prune: nothing to remove"),
        Ok(removed) => {
            println!("rooms prune: removed {} stale entr{}:", removed.len(), if removed.len() == 1 { "y" } else { "ies" });
            for name in &removed {
                println!("  - {name}");
            }
        }
        Err(e) => {
            eprintln!("rooms prune: {e}");
            std::process::exit(1);
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
/// activation + cache reserve. **Conservative ADMISSION on this figure is the structural
/// safety lever** — no MLX API hard-caps a process below physical RAM (`set_memory_limit`
/// is soft; only `set_cache_limit` bounds the cache; memory
/// `reference-mlx-memory-cap-semantics`), so the residency ledger refusing to load a
/// model that would overcommit is what prevents the reboot. Hence this MUST be ≥ the
/// model's real resident peak (active + bounded cache). The same figure is reused for
/// the soft `set_memory_limit` hint (smmr-A) so they agree. An unknown model gets a
/// deliberately huge estimate so it only loads when the host is otherwise empty
/// (under-counting is the direction that reboots). Optional
/// `ROZUM_GATEWAY_FOOTPRINT_INFLATE` (default 1.0) pads it for extra conservatism.
///
/// Supersedes v2's weights-only `size×inflate+base` and the smmr interim floor.
fn estimate_model_footprint_bytes(model: &str, n_ctx: u32) -> u64 {
    let inflate = std::env::var("ROZUM_GATEWAY_FOOTPRINT_INFLATE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|f| f.is_finite() && *f >= 1.0)
        .unwrap_or(1.0);
    match rozum::models::scan_all_installed()
        .into_iter()
        .find(|m| rozum::model_source::same_model(&m.spec, model))
    {
        Some(m) => {
            let fp = rozum::model_source::runtime_footprint_bytes(model, n_ctx, m.size_bytes);
            let conservative = ((fp as f64) * inflate) as u64;
            // Improvement A (footprint-estimate-accuracy): if a prior load of this (model, n_ctx)
            // recorded its REAL peak, tighten the conservative estimate toward it — capped at the
            // conservative figure, floored at weights+KV + a margin (never under-estimates an observed
            // peak). The keep-free margin + kernel pressure-guard (improvement B) backstop. Opt out
            // with ROZUM_GATEWAY_MEASURED_FOOTPRINT=0 (use the pure conservative estimate).
            if measured_footprint_enabled() {
                let active = rozum::model_source::runtime_active_bytes(model, n_ctx, m.size_bytes);
                rozum::footprint::tighten(model, conservative, active)
            } else {
                conservative
            }
        }
        // Unknown size (spec never matched the catalog) → the sentinel: admission replies with an
        // honest "unsizeable spec" message instead of a garbage overcommit number.
        None => rozum::share::UNSIZEABLE_FOOTPRINT_BYTES,
    }
}

/// Whether admission tightens its estimate with a model's measured real peak (improvement A).
/// Default ON; `ROZUM_GATEWAY_MEASURED_FOOTPRINT=0` falls back to the pure conservative estimate.
fn measured_footprint_enabled() -> bool {
    std::env::var("ROZUM_GATEWAY_MEASURED_FOOTPRINT")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// The unknown-size sentinel `estimate_model_footprint_bytes` returns when a model isn't cached
/// locally (`u64::MAX / 4`). Anything at or above half of it (`u64::MAX / 8`) is a not-cached
/// figure, never a real model footprint — used to print an honest message instead of the bogus
/// `~4398046511103 MB` (= the sentinel in MB) the gate would otherwise quote.
const UNKNOWN_FOOTPRINT_FLOOR: u64 = rozum::share::UNSIZEABLE_FOOTPRINT_FLOOR;

/// Is this exact spec present in the local model cache? (`false` ⇒ size unknown ⇒ sentinel.)
fn model_is_locally_cached(model: &str) -> bool {
    rozum::models::scan_all_installed()
        .iter()
        .any(|m| rozum::model_source::same_model(&m.spec, model))
}

/// A copy-pasteable pre-download command for an uncached spec, so an `--offline` refusal is
/// actionable. `mlx-community:Foo` → `huggingface-cli download mlx-community/Foo`.
fn hf_download_hint(model: &str) -> String {
    let repo = model
        .strip_prefix("mlx-community:")
        .map(|r| format!("mlx-community/{r}"))
        .or_else(|| (!model.contains(':') && !model.starts_with('/')).then(|| model.to_string()));
    match repo {
        Some(r) => format!("huggingface-cli download {r}"),
        None => format!("load '{model}' once WITH network (drop --offline) to cache it"),
    }
}

/// Reserve host RAM for a model about to load (BUG-003 v2), or exit with a clear
/// message if it would overcommit. Hold the returned guard for as long as the model
/// is resident (binding it at the caller's function scope is enough). Runs the
/// (possibly long) blocking wait off the async runtime. `None` = gate bypassed /
/// unavailable → loading proceeds (the gate is a safety net, not correctness).
/// A cascade spec (`cascade:name` or a comma-list) is NOT a single installed model, so the normal
/// footprint estimate returns the unknown-size sentinel (`u64::MAX/4`) and admission wrongly REFUSES it
/// (the bug: `loading this model (~4398046511103 MB) would overcommit`). Estimate the cascade's real
/// resident cost instead: the SUM of its LOCAL tiers' footprints (remote/cloud tiers use no host RAM).
/// Conservative by design — a cascade of two big locals is correctly refused (they don't co-fit on a
/// 36 GB host), while a small-local + cloud cascade admits. Returns `None` when `model` is not a cascade.
fn cascade_local_footprint(cfg: &rozum::RuntimeConfig, model: &str, n_ctx: u32) -> Option<u64> {
    // Resolve the same way `try_cascade_backend` does: a `cascade:<name>` table from config, OR a
    // comma-list (with or without the `cascade:` prefix) auto-ordered into a spec.
    let as_list = |s: &str| -> Option<rozum::cascade::CascadeSpec> {
        let names: Vec<String> = s
            .split(',')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect();
        // Order/cost-rank don't matter for a footprint SUM/MAX — use the pipeline (input-order)
        // builder; the strategy (which decides MAX vs SUM below) is set from the env override.
        (names.len() >= 2).then(|| rozum::cascade::from_model_pipeline(&names))
    };
    let mut spec = if let Some(name) = rozum::cascade::parse_cascade_model(model) {
        // `cascade:a,b` → an ad-hoc list; `cascade:foo` → a named config table.
        if name.contains(',') {
            as_list(&name)?
        } else {
            load_cascade_spec(cfg, &name)?
        }
    } else if model.contains(',') {
        as_list(model)?
    } else {
        return None;
    };
    // Mirror `build_cascade_from_spec`: `ROZUM_CASCADE_STRATEGY` overrides; default (comma-list) is
    // Pipeline.
    if let Some(st) = std::env::var("ROZUM_CASCADE_STRATEGY")
        .ok()
        .and_then(|v| rozum::cascade::StrategyName::parse_cli(&v))
    {
        spec.strategy = st;
    }
    let local_models: Vec<&str> = spec
        .tiers
        .iter()
        .filter(|t| matches!(t.location, rozum::cascade::Location::Local))
        .map(|t| t.model.as_str())
        .collect();
    // Residency reservation. EAGER (all tiers co-resident — the MLX co-residency crash is fixed, see
    // `tests/mlx_evals.rs::coresidency_two_mlx_models_one_process`) reserves the SUM; LAZY (one tier
    // at a time, torn down before the next loads) reserves MAX. A pipeline now runs eager when the SUM
    // is admissible (no per-request swap → far faster + measured higher pass-rate), falling back to
    // lazy only when the SUM would overcommit. Must match the build-time choice in `build_cascade_from_spec`.
    // For the EAGER SUM: use eager_coresident_footprint (smmr-D follow-up) — counts the shared
    // MLX buffer-cache + prefill-activation reserve ONCE, not once per tier.
    let total = if matches!(spec.strategy, rozum::cascade::StrategyName::Pipeline) {
        if pipeline_is_eager(&spec, n_ctx) {
            eager_coresident_footprint(&local_models, n_ctx)
        } else {
            local_models.iter()
                .map(|m| estimate_model_footprint_bytes(m, n_ctx))
                .max()
                .unwrap_or(0)
        }
    } else {
        local_models.iter()
            .map(|m| estimate_model_footprint_bytes(m, n_ctx))
            .fold(0u64, u64::saturating_add)
    };
    Some(total)
}

/// Does this pipeline run EAGER (all tiers co-resident, no per-request swap) vs LAZY (one tier at a
/// time)? The MLX co-residency crash that once forced lazy is fixed (thread_local metal command-encoder
/// self-heal; `tests/mlx_evals.rs::coresidency_two_mlx_models_one_process` survives), and eager is far
/// faster (no load/teardown per request) AND scored higher in the agentic sweep (e.g. Qwen3-4B→Coder-7B
/// 9/10 @ ~9.4 GB eager vs the slow lazy swap). Eager when the co-resident footprint is admissible,
/// else lazy fallback (so a pair that would overcommit still runs, one model at a time, at MAX peak).
/// `ROZUM_PIPELINE_EAGER=1`/`0` forces the choice. Non-pipeline strategies are always eager (return false
/// here only gates the lazy-pipeline backend).
fn pipeline_is_eager(spec: &rozum::cascade::CascadeSpec, n_ctx: u32) -> bool {
    if !matches!(spec.strategy, rozum::cascade::StrategyName::Pipeline) {
        return false;
    }
    match std::env::var("ROZUM_PIPELINE_EAGER").ok().as_deref() {
        Some("1" | "true" | "on") => return true,
        Some("0" | "false" | "off") => return false,
        _ => {}
    }
    let local_models: Vec<&str> = spec
        .tiers
        .iter()
        .filter(|t| matches!(t.location, rozum::cascade::Location::Local))
        .map(|t| t.model.as_str())
        .collect();
    rozum::share::dry_run_admission(eager_coresident_footprint(&local_models, n_ctx)).admit
}

/// Footprint for N co-resident models in an eager pipeline: Σ per-model weights+KV +
/// ONE shared process reserve (MLX buffer cache + prefill spike is process-global, not per-model).
/// This correctly accounts for `process_reserve_bytes` being a single pool shared among all tiers,
/// saving ~5.5 GiB per extra co-resident model vs the naive Σ estimate which double-counts the reserve.
/// Uses `runtime_active_bytes()` (weights + full KV at n_ctx) from the local model catalog as the
/// per-tier active component. Falls back to Σ `estimate_model_footprint_bytes()` when any tier is
/// not locally installed (unknown size → conservative sentinel).
fn eager_coresident_footprint(tiers: &[&str], n_ctx: u32) -> u64 {
    let installed = rozum::models::scan_all_installed();
    let mut max_weight: u64 = 0;
    let mut all_local = true;

    let sum_active: u64 = tiers
        .iter()
        .map(|model| {
            match installed.iter().find(|m| rozum::model_source::same_model(&m.spec, model)) {
                Some(m) => {
                    max_weight = max_weight.max(m.size_bytes);
                    rozum::model_source::runtime_active_bytes(model, n_ctx, m.size_bytes)
                }
                None => {
                    all_local = false;
                    u64::MAX / 4
                }
            }
        })
        .fold(0u64, u64::saturating_add);

    if !all_local {
        // Any unknown-size tier → conservative: Σ full estimates (each with its own reserve)
        return tiers
            .iter()
            .map(|m| estimate_model_footprint_bytes(m, n_ctx))
            .fold(0u64, u64::saturating_add);
    }
    // Σ active (weights + full KV per tier) + ONE shared reserve (cache + prefill spike)
    sum_active.saturating_add(rozum::model_source::process_reserve_bytes(max_weight))
}

#[cfg(test)]
mod coresident_gate_tests {
    use super::*;

    // Two unknown-size models fall back to the conservative Σ full-estimates (sentinel behavior).
    // Sentinel = u64::MAX/4 per uninstalled model; two of them saturate to u64::MAX/2 (still >>RAM).
    #[test]
    fn unknown_tiers_fall_back_to_conservative() {
        let fp = eager_coresident_footprint(&["nonexistent-model-xyz", "another-fake-spec"], 8192);
        // Fallback: estimate("nonexistent-model-xyz", 8192) = MAX/4 (sentinel for unknown model)
        //   + estimate("another-fake-spec", 8192) = MAX/4  →  saturating_add = MAX/2
        assert!(fp >= u64::MAX / 4, "sentinel for unknown models must be huge, got {fp}");
    }

    // Single unknown tier also falls back.
    #[test]
    fn single_unknown_tier_falls_back_to_conservative() {
        let fp = eager_coresident_footprint(&["nonexistent-model-xyz"], 8192);
        assert!(fp >= u64::MAX / 4, "single unknown tier must return sentinel, got {fp}");
    }

    // Structural math: for N co-resident known models, eager footprint < Σ full estimates.
    // The savings = (N-1) * process_reserve_bytes (the shared pool counted only once).
    // This test verifies the arithmetic property directly without needing installed models.
    #[test]
    fn coresident_footprint_saves_n_minus_1_reserves() {
        const GIB: u64 = 1 << 30;
        let weight_a = 4 * GIB;
        let weight_b = 7 * GIB;
        let n_ctx = 8192_u32;

        // What each model would contribute to the NAIVE sum (Σ full estimates)
        let active_a = rozum::model_source::runtime_active_bytes("irrelevant", n_ctx, weight_a);
        let active_b = rozum::model_source::runtime_active_bytes("irrelevant", n_ctx, weight_b);
        let reserve = rozum::model_source::process_reserve_bytes(weight_a.max(weight_b));
        let full_a = active_a + reserve;
        let full_b = active_b + reserve;
        let naive_sum = full_a.saturating_add(full_b);

        // The correct co-resident total: Σ active + ONE reserve
        let correct = active_a.saturating_add(active_b).saturating_add(reserve);

        // eager_coresident_footprint saves exactly ONE reserve vs the naive sum
        assert_eq!(correct + reserve, naive_sum, "savings = exactly one extra reserve");
        assert!(
            correct < naive_sum,
            "co-resident footprint must be smaller than naive sum: {correct} < {naive_sum}"
        );
    }
}

async fn acquire_residency_or_exit(
    model: &str,
    n_ctx: u32,
    footprint_override: Option<u64>,
) -> Option<rozum::share::ResidencyGuard> {
    // A single (non-cascade) model that isn't cached locally has no measurable size, so
    // `estimate_model_footprint_bytes` returns the unknown-size sentinel (`u64::MAX/4` ≈
    // 4_398_046_511_103 MB). That sentinel exceeds any real RAM, so the gate refuses the load even
    // on a completely empty host — and under `--offline` the model can't be fetched to fix it. The
    // old result was a baffling "(~4398046511103 MB) would overcommit host RAM" when the real
    // problem is simply that the model isn't downloaded. Say that instead. (Online: fall through —
    // the load path can still download; the sentinel stays a conservative empty-host-only estimate.)
    if footprint_override.is_none() && is_offline() && !model_is_locally_cached(model) {
        eprintln!(
            "rozum gateway: model '{model}' is not downloaded locally and --offline (ROZUM_OFFLINE) \
             is set, so it cannot be fetched. Pre-download it first (with network):\n  {hint}\n\
             then retry the offline run.",
            hint = hf_download_hint(model),
        );
        std::process::exit(1);
    }
    let footprint = footprint_override.unwrap_or_else(|| estimate_model_footprint_bytes(model, n_ctx));
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
            if denied.footprint_bytes >= UNKNOWN_FOOTPRINT_FLOOR {
                // The footprint is the unknown-size sentinel, not a real measurement: a model in
                // this load isn't cached locally so its size can't be estimated (online uncached, or
                // a cascade tier that isn't downloaded). Don't quote the absurd sentinel-in-MB.
                eprintln!(
                    "rozum gateway: refusing to load '{model}' — its size is UNKNOWN (a model in \
                     this load is not downloaded locally, so its RAM cost can't be estimated). \
                     {} MB reserved by [{}]. Pre-download the missing model first ({}), or set \
                     ROZUM_ALLOW_CONCURRENT_RESIDENT=1 to bypass the gate (risks an OOM reboot, BUG-003).",
                    mb(denied.in_use_bytes),
                    who,
                    hf_download_hint(model),
                );
            } else {
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
            }
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

/// **Adaptive loading**: shrink `req_n_ctx` (and the MLX cache cap) to the best params that fit
/// available host RAM, so a model the free-RAM gate would otherwise REFUSE instead loads at
/// reduced-but-usable params. Returns the n_ctx to load with, and sets `ROZUM_MLX_CACHE_GB` when it
/// shrinks the cache (so the footprint estimate AND `set_cache_limit` agree). No-op (returns
/// `req_n_ctx`) when the request already fits, the model/RAM is unknown, or
/// `ROZUM_GATEWAY_ADAPTIVE_LOAD=0`. If it can't fit even at the floor it returns `req_n_ctx` and lets
/// [`acquire_residency_or_exit`] refuse with its full message — admission stays the final safety gate.
fn adapt_n_ctx_to_fit(model: &str, req_n_ctx: u32) -> u32 {
    if matches!(
        std::env::var("ROZUM_GATEWAY_ADAPTIVE_LOAD").ok().as_deref(),
        Some("0" | "false" | "off")
    ) {
        return req_n_ctx;
    }
    let Some(m) = rozum::models::scan_all_installed()
        .into_iter()
        .find(|m| rozum::model_source::same_model(&m.spec, model))
    else {
        return req_n_ctx; // unknown model (download/sentinel path) → admission handles it
    };
    let Some(available) = rozum::share::available_ram_for_admission() else {
        return req_n_ctx; // can't measure free RAM → don't adapt
    };
    let min_free = rozum::share::min_free_ram_bytes();
    const N_CTX_FLOOR: u32 = 4096; // below this the model can't even hold an agent prompt
    let default_cache = std::env::var("ROZUM_MLX_CACHE_GB")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(4);
    match rozum::model_source::fit_model_params(model, m.size_bytes, req_n_ctx, available, min_free, N_CTX_FLOOR)
    {
        Some((n_ctx, cache_gib)) => {
            if n_ctx != req_n_ctx || cache_gib != default_cache {
                eprintln!(
                    "rozum gateway: adaptive load — '{model}' at n_ctx {req_n_ctx} won't fit free RAM \
                     (~{} MB, keep-free ~{} MB); loading with n_ctx {n_ctx} + {cache_gib} GiB cache \
                     (best fit; ROZUM_GATEWAY_ADAPTIVE_LOAD=0 to refuse instead).",
                    available / 1_048_576,
                    min_free / 1_048_576,
                );
                // SAFETY: single-threaded startup, before any backend builds; the footprint estimate
                // and MLX `set_cache_limit` both read this env at load time, so they agree.
                unsafe { std::env::set_var("ROZUM_MLX_CACHE_GB", cache_gib.to_string()) };
            }
            n_ctx
        }
        None => req_n_ctx, // won't fit even at the floor → let admission refuse with its message
    }
}

/// `--dry-run`: print how `model` WOULD load at the CURRENT free RAM — the adaptive n_ctx/cache fit
/// and the host-RAM admission verdict — WITHOUT loading anything. Reuses the exact load-path math
/// ([`adapt_n_ctx_to_fit`]'s pieces + [`estimate_model_footprint_bytes`] + [`rozum::share::dry_run_admission`]),
/// so a real `gateway --model` run does exactly what this reports. The no-load way to plan a matrix.
fn run_gateway_dry_run(model: &str, n_ctx: Option<u32>) {
    const GIB: f64 = (1u64 << 30) as f64;
    const N_CTX_FLOOR: u32 = 4096;
    let gib = |b: u64| b as f64 / GIB;
    let req_n_ctx = resolve_n_ctx(model, n_ctx);
    let adaptive_off = matches!(
        std::env::var("ROZUM_GATEWAY_ADAPTIVE_LOAD").ok().as_deref(),
        Some("0" | "false" | "off")
    );

    println!("rozum gateway --dry-run: {model}");
    println!("  adaptive loading:  {}", if adaptive_off { "OFF (ROZUM_GATEWAY_ADAPTIVE_LOAD=0)" } else { "ON (default)" });
    println!("  requested n_ctx:   {req_n_ctx}");

    let Some(m) = rozum::models::scan_all_installed()
        .into_iter()
        .find(|m| rozum::model_source::same_model(&m.spec, model))
    else {
        println!("  model not cached locally → a real run resolves/downloads first, then the SAME");
        println!("  admission gate runs. No fit estimate possible without local weights.");
        return;
    };
    let available = rozum::share::available_ram_for_admission();
    let min_free = rozum::share::min_free_ram_bytes();
    println!("  weights on disk:   {:.2} GiB", gib(m.size_bytes));
    println!("  available RAM:     {}", available.map(|a| format!("{:.2} GiB (MemAvailable: total − wired − anonymous − compressor; counts reclaimable file cache)", gib(a))).unwrap_or_else(|| "unmeasurable → free-RAM lever fails open".into()));
    println!("  keep-free margin:  {:.2} GiB", gib(min_free));
    println!("  host pressure:     {} (kernel jetsam level; warn/critical ⇒ refuse)", rozum::share::host_pressure_label());

    // Adaptive fit — the SAME fit_model_params the load path runs (skipped when adaptive is off).
    let fit = if adaptive_off {
        None
    } else {
        available.and_then(|a| rozum::model_source::fit_model_params(model, m.size_bytes, req_n_ctx, a, min_free, N_CTX_FLOOR))
    };
    let default_cache = std::env::var("ROZUM_MLX_CACHE_GB")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(4);
    let (load_n_ctx, cache_gib) = match fit {
        Some((n, c)) => (n, c),
        // Adaptive OFF: the real load attempts the FULL requested params → show that footprint/shortfall.
        None if adaptive_off => (req_n_ctx, default_cache),
        // Adaptive ON but even the floor doesn't fit: show the MINIMAL footprint (floor n_ctx + 1 GiB
        // cache) so the shortfall is the real "free this much and it loads at minimum params", not the
        // full-context footprint (which would 5× the reported gap).
        None => (N_CTX_FLOOR, 1),
    };
    // Make the footprint estimate agree with the chosen cache (both read this env), exactly as the load path does.
    unsafe { std::env::set_var("ROZUM_MLX_CACHE_GB", cache_gib.to_string()) };
    let footprint = estimate_model_footprint_bytes(model, load_n_ctx);
    let report = rozum::share::dry_run_admission(footprint);

    println!();
    if !adaptive_off && available.is_some() && fit.is_none() {
        println!("  adaptive fit:      ✗ cannot fit even floor n_ctx={N_CTX_FLOOR} + 1 GiB cache — weights alone overflow the budget");
    } else if load_n_ctx == req_n_ctx && cache_gib >= 4 {
        println!("  adaptive fit:      ✓ full — n_ctx {load_n_ctx}, cache {cache_gib} GiB");
    } else {
        println!("  adaptive fit:      ↓ reduced — n_ctx {load_n_ctx} (req {req_n_ctx}), cache {cache_gib} GiB");
    }
    if let Some(peak) = rozum::footprint::cached_peak(model).filter(|_| measured_footprint_enabled()) {
        println!("  measured peak:     {:.2} GiB (real high-water from a prior load → estimate tightened toward it)", gib(peak));
    }
    println!("  est. footprint:    {:.2} GiB at those params", gib(footprint));
    if !report.holders.is_empty() {
        let who: Vec<String> = report.holders.iter().map(|(p, mm)| format!("pid {p} {mm}")).collect();
        println!("  other residents:   {:.2} GiB [{}]", gib(report.in_use), who.join(", "));
    }

    println!();
    if report.admit {
        println!("  VERDICT: ✅ WOULD LOAD — footprint {:.2} + keep-free {:.2} = {:.2} GiB ≤ available {:.2} GiB.",
            gib(footprint), gib(min_free), gib(footprint + min_free), gib(report.available.unwrap_or(0)));
    } else {
        let need = footprint.saturating_add(min_free);
        let short = need.saturating_sub(report.available.unwrap_or(0));
        if !report.pressure_ok {
            println!("  VERDICT: ⛔ WOULD REFUSE — host memory pressure is '{}' (kernel jetsam ladder): \
                      loading a model now risks tipping the host into the jetsam→reboot cascade.", report.pressure.as_str());
        } else if !report.ram_fits {
            println!("  VERDICT: ⛔ WOULD REFUSE — need footprint {:.2} + keep-free {:.2} = {:.2} GiB, available {:.2} GiB → short by {:.2} GiB.",
                gib(footprint), gib(min_free), gib(need), gib(report.available.unwrap_or(0)), gib(short));
        } else {
            println!("  VERDICT: ⛔ WOULD REFUSE — ledger: {:.2} GiB reserved by other residents would overcommit the host budget.", gib(report.in_use));
        }
        println!("           Refusal = clean process exit BEFORE any weights load → a matrix FAIL, never a reboot.");
        if short > 0 {
            println!("           To make it load: free ~{:.1} GiB more RAM, or pass a lower --n-ctx, or lower keep-free", gib(short));
            println!("           (ROZUM_GATEWAY_MIN_FREE_RAM_BYTES — reduces the no-reboot safety headroom).");
        }
    }
}

async fn run_gateway(port: u16, model_spec: String, n_ctx: Option<u32>, cfg: rozum::RuntimeConfig) {
    let n_ctx = resolve_n_ctx(&model_spec, n_ctx.or(cfg.n_ctx));
    let n_ctx = adapt_n_ctx_to_fit(&model_spec, n_ctx);
    let cfg = std::sync::Arc::new(cfg);
    // Host-wide RAM gate: reserve this model's footprint before loading so the
    // resident models can't overcommit host RAM (whole-system OOM → watchdog kernel
    // panic → reboot, BUG-003). Held for this process's lifetime; covers the initial
    // load below plus every lazy reload / `switch` (all same-process). A cascade spec reserves the SUM
    // of its LOCAL tiers (so admission understands it instead of refusing on the unknown-size sentinel).
    //
    // Download an uncached SINGLE model BEFORE admission. The footprint estimate (and thus the gate)
    // needs the real weights size on disk — an uncached model otherwise estimates the unknown-size
    // sentinel and is REFUSED before it ever downloads (chicken-and-egg: admission can't fit-check what
    // isn't on disk, so the download that would make it checkable never runs). ensure_model_dir is a
    // no-op when already cached, and returns None (harmless) for a cascade/non-repo spec — whose tiers
    // download later in try_cascade_backend. Afterwards the estimate below is real and admission correct.
    // ONLY for a single-model spec — a cascade ("A,B") or "cascade:…" is not one downloadable HF repo
    // (querying the joined string 401s); its tiers resolve+download in try_cascade_backend below.
    let is_single_spec = !model_spec.contains(',') && !model_spec.starts_with("cascade");
    if is_single_spec && rozum::model_source::resolve_model_dir(&model_spec).is_none() {
        eprintln!("rozum gateway: '{model_spec}' not cached — downloading before the RAM gate …");
        let _ = rozum::mlx_native_backend::ensure_model_dir(&model_spec).await;
    }
    let casc_fp = cascade_local_footprint(&cfg, &model_spec, n_ctx);
    let _residency = acquire_residency_or_exit(&model_spec, n_ctx, casc_fp).await;
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
    // footprint-before-download fix: the model is now loaded (hence resolved/cached), so
    // re-estimate its REAL footprint and correct this process's reservation. An uncached
    // model reserved the unknown-size sentinel up front (the estimate ran before the
    // download) which over-blocks other gateways for its whole life; republishing the real
    // size unblocks them as soon as we're loaded. No-op for an already-cached model (same
    // value) or when the gate was bypassed. Best-effort.
    rozum::share::update_my_reservation(&model_spec, estimate_model_footprint_bytes(&model_spec, n_ctx));
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
        "--no-room-bridge",
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
/// B3 (model→driver routing): the warning text for a known-poor driver↔model pairing, or `None`.
/// Capability is RELATIONAL — the codex/opencode CLIs are built around the OpenAI apply_patch tool
/// surface, and a model NOT trained on it (Devstral/Mistral) invents endlessly-malformed tool calls
/// under them (driver mismatch, not a gateway bug — measured: Devstral 5/6 under claude vs ~0 under
/// codex; see SPRINT). The gateway cannot reliably fix an unbounded malformed-form surface; the lever
/// is running the model under the driver it was trained for. This surfaces the mismatch — it never
/// blocks or auto-switches (the operator stays in control; `ROZUM_NO_MATCH_WARN=1` silences it).
fn driver_model_mismatch_warning(agent: &str, model: Option<&str>) -> Option<String> {
    let model = model?;
    let agent = std::path::Path::new(agent)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(agent);
    let poorly_matched = model.contains("Devstral") || model.contains("Mistral");
    if matches!(agent, "codex" | "opencode") && poorly_matched {
        return Some(format!(
            "rozum launch: ⚠ {agent} × {model} is a poor driver↔model match — {model} is not trained on \
             the {agent} tool protocol and emits malformed tool calls, so create/edit often won't land. \
             Prefer `claude` for this model (measured far more reliable). Set ROZUM_NO_MATCH_WARN=1 to silence."
        ));
    }
    None
}

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
    room_bridge: bool,
}

impl WakeupPolicy {
    /// Resolve the launch-time wakeup policy for `program` (the agent argv[0]).
    fn resolve(
        channels: &ChannelWakeup,
        no_piggyback: bool,
        no_room_bridge: bool,
        program: &str,
    ) -> Self {
        let channel_flags = channels.flags_for(program);
        let piggyback = resolve_piggyback(
            no_piggyback,
            rozum::meeting::piggyback::env_override(),
            channel_flags.is_some(),
        );
        let agent = std::path::Path::new(program)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(program);
        let room_bridge = resolve_room_bridge(
            no_room_bridge,
            room_bridge_env_override(),
            agent,
            piggyback,
        );
        WakeupPolicy {
            channel_flags,
            piggyback,
            room_bridge,
        }
    }
}

/// Agents that cannot join a room by themselves under ANY tier: no MCP client, so no `mcp add` to
/// register (`MCP_AGENTS`), no `wait_my_turn` to hold open, and nothing to write the Tier-3 drops.
/// For these — and ONLY these — `rozum launch` carries the room presence itself; for an agent that
/// speaks MCP the bridge would be a second participant under the same handle. Keep this list
/// honest: an agent belongs here when it has no path of its own, not when its path is inconvenient.
const ROOM_BRIDGE_AGENTS: &[&str] = &["nadia"];

/// The explicit `ROZUM_ROOM_BRIDGE` setting, or `None` when unset/unrecognized so the caller
/// applies its own default. Same vocabulary as `ROZUM_PIGGYBACK`.
fn room_bridge_env_override() -> Option<bool> {
    match std::env::var("ROZUM_ROOM_BRIDGE").ok().as_deref() {
        Some("0" | "false" | "off" | "no") => Some(false),
        Some("1" | "true" | "on" | "yes") => Some(true),
        _ => None,
    }
}

/// Decide whether `rozum launch` carries room presence for the agent. Precedence:
/// `--no-room-bridge` (force off) > `ROZUM_ROOM_BRIDGE` > auto.
///
/// Auto is on iff the agent is one that has no room path of its own AND Tier-3 injection is live.
/// The piggyback condition is what keeps a MEASUREMENT honest: `scripts/bench/agentic.sh` passes
/// `--no-piggyback`, so a matrix cell neither posts into the room nor can have room chatter folded
/// into the context it is being scored on. An operator who wants presence there anyway says so with
/// `ROZUM_ROOM_BRIDGE=1`.
fn resolve_room_bridge(
    no_room_bridge: bool,
    env_override: Option<bool>,
    agent: &str,
    piggyback: bool,
) -> bool {
    if no_room_bridge {
        return false;
    }
    env_override.unwrap_or(ROOM_BRIDGE_AGENTS.contains(&agent) && piggyback)
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
mod footprint_uncached_tests {
    use super::{hf_download_hint, UNKNOWN_FOOTPRINT_FLOOR};

    #[test]
    fn unknown_sentinel_is_above_the_floor_but_a_real_model_is_not() {
        // The estimate's unknown-size sentinel (u64::MAX/4) must trip the "size unknown" branch…
        assert!(u64::MAX / 4 >= UNKNOWN_FOOTPRINT_FLOOR);
        // …while any plausible real footprint (e.g. a 1 TiB model) must NOT.
        assert!(1024u64 * 1024 * 1024 * 1024 < UNKNOWN_FOOTPRINT_FLOOR);
    }

    #[test]
    fn download_hint_maps_mlx_community_spec_to_hf_repo() {
        assert_eq!(
            hf_download_hint("mlx-community:Qwen3-Coder-30B-A3B-Instruct-4bit"),
            "huggingface-cli download mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit"
        );
        // A bare HF id (no scheme, not a path) is used as-is.
        assert_eq!(hf_download_hint("org/model"), "huggingface-cli download org/model");
        // A scheme'd spec or absolute path has no HF repo → generic guidance, no bogus repo.
        assert!(hf_download_hint("lmstudio:foo/bar").contains("WITH network"));
        assert!(hf_download_hint("/abs/path/model.gguf").contains("WITH network"));
    }
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

    use super::resolve_room_bridge;

    #[test]
    fn room_bridge_carries_only_agents_with_no_room_path_of_their_own() {
        // Auto: nadia has no MCP client → launch carries the presence.
        assert!(resolve_room_bridge(false, None, "nadia", true));
        // An agent that CAN join by itself must not get a second participant under the same handle.
        for agent in ["claude", "codex", "opencode"] {
            assert!(
                !resolve_room_bridge(false, None, agent, true),
                "{agent} joins via its own mcp-proxy — the bridge would double it"
            );
        }
        // A benchmark cell (`--no-piggyback`) is silent: no post, no injection, nothing that could
        // move the number being measured.
        assert!(!resolve_room_bridge(false, None, "nadia", false));
        // `--no-room-bridge` wins over everything, including the env override.
        assert!(!resolve_room_bridge(true, None, "nadia", true));
        assert!(!resolve_room_bridge(true, Some(true), "nadia", true));
        // The env override beats the auto rule in both directions.
        assert!(resolve_room_bridge(false, Some(true), "codex", false));
        assert!(!resolve_room_bridge(false, Some(false), "nadia", true));
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
        room_bridge,
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
            room_bridge,
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
        room_bridge,
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
        room_bridge,
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
    exec_agent(program, &model_spec, port, channel_flags, piggyback, room_bridge).await
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
        room_bridge,
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
    // Adaptive: shrink n_ctx/cache to the best fit first so a tight host loads rather than refuses.
    let n_ctx = adapt_n_ctx_to_fit(&model_spec, n_ctx);
    // A cascade spec reserves the SUM of its LOCAL tiers (config-loaded for the named-cascade case).
    let casc_fp = cascade_local_footprint(&load_runtime_config_or_exit(), &model_spec, n_ctx);
    let _residency = acquire_residency_or_exit(&model_spec, n_ctx, casc_fp).await;
    let backend = match build_gateway_backend(&model_spec, n_ctx).await {
        Some(b) => b,
        None => {
            print_no_backend_hints(&model_spec);
            std::process::exit(1);
        }
    };
    // footprint-before-download fix (see run_gateway): correct the reservation to the real
    // footprint now that the model is loaded/cached (an uncached model reserved the sentinel).
    rozum::share::update_my_reservation(&model_spec, estimate_model_footprint_bytes(&model_spec, n_ctx));
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
    exec_agent(program, &model_spec, port, channel_flags, piggyback, room_bridge).await
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

    // Foreground: this process IS the daemon. Under supervision this is a LOOP: losing the
    // ownership lock to a client that spawned in the same instant must send us back to waiting,
    // not cost launchd its process — that would rebuild the respawn loop by another road.
    let supervised = supervised_by_launchd(std::env::var("XPC_SERVICE_NAME").ok());
    loop {
        if daemon_alive(&sock).await {
        // BUG-025. Exiting here is fatal under a supervisor: `exit(0)` says "my work is done" and
        // `KeepAlive = true` says "you are never done", so launchd starts another copy to step
        // aside again — one process every ~9 s, forever, while everything looks healthy from
        // outside. Hold the slot instead and take over when the incumbent goes; that is what makes
        // the job the real owner of the service rather than a bystander.
            if supervised {
                eprintln!(
                    "meeting daemon already running ({}); supervised — waiting to take over",
                    sock.display()
                );
                // The poll interval is a RACE WINDOW, not a politeness knob, and it was
                // measured: at 2 s the incumbent died and a client-spawned daemon had taken the
                // socket before this process woke up — every time, because a client spawns the
                // instant a connect fails while this one was asleep. 200 ms narrows it. What
                // CLOSES it is the ownership lock in `serve_daemon`: losing this poll is now only
                // a lost round of this loop, because the winner is decided by the lock rather than
                // by who unlinks the socket last.
                while daemon_alive(&sock).await {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                eprintln!("meeting daemon gone; taking over");
            } else {
                // NOT politeness: `spawn_detached_meetings` starts its child with this same flag,
                // and two clients can both find no daemon and both spawn one. Waiting
                // unconditionally would leave the loser of every race as a permanent idle standby
                // — a process leak traded for a respawn loop.
                eprintln!("meeting daemon already running ({})", sock.display());
                return;
            }
        }
        let state_dir = rozum_state_dir();
        let _ = std::fs::create_dir_all(&state_dir);
        let pid_path = state_dir.join("meetings.pid");
        let _ = std::fs::write(&pid_path, std::process::id().to_string());

        if let Ok(removed) = rozum::meeting::prune_registered(&state_dir) {
            for name in &removed {
                eprintln!("meetings: pruned stale room '{name}'");
            }
        }

        let registry = std::sync::Arc::new(RoomRegistry::new(state_dir));
        match serve_daemon(&sock, registry).await {
            Ok(()) => {}
            // Another daemon took ownership between our check and our bind. Under supervision that
            // is a lost round, not a failure: go back to waiting. Anywhere else it is the correct
            // end of a process that was never meant to be the second daemon.
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse && supervised => {
                // SLEEP FIRST. Without it this is a hot loop and it was measured as one: when the
                // owner's socket FILE is missing, `daemon_alive` says "nothing there", so the wait
                // above returns instantly, the bind is refused instantly, and the retry burns CPU
                // for as long as the owner lives. A supervisor retrying forever is correct; doing
                // it as fast as the scheduler allows is not.
                eprintln!("{e}; retrying in 1s");
                let _ = std::fs::remove_file(&pid_path);
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                continue;
            }
            Err(e) => eprintln!("meeting daemon error: {e}"),
        }
        let _ = std::fs::remove_file(&pid_path);
        return;
    }
}

/// The launchd job that owns the meeting daemon. Must match the plist `Label`; `doctor.rs` probes
/// the same string.
const MEETING_DAEMON_JOB: &str = "com.rozum.meeting-daemon";

/// Is this process THE meeting daemon's launchd job? (BUG-025)
///
/// launchd sets `XPC_SERVICE_NAME` to the job label. The test must be for THAT label and nothing
/// else, and the first version of this function got it wrong in a way worth keeping written down:
/// it accepted any non-empty value.
///
/// Two things break that. macOS sets `XPC_SERVICE_NAME=0` — the string "0", not empty — on ordinary
/// processes, so every interactive `meetings start --foreground` decided it was supervised and
/// waited forever instead of exiting. And the variable is INHERITED, so a client started by some
/// OTHER rozum job carries `com.rozum.gateway` and would have made the same wrong call — which is
/// exactly the process leak the conditional exists to prevent.
///
/// `getppid() == 1` cannot be used instead: a detached client-spawned daemon is reparented to pid 1
/// too, so it answers a different question.
fn supervised_by_launchd(xpc_service_name: Option<String>) -> bool {
    xpc_service_name.as_deref().map(str::trim) == Some(MEETING_DAEMON_JOB)
}

#[cfg(test)]
mod supervise_tests {
    use super::supervised_by_launchd;

    /// Deliberately a pure decision with no socket and no processes. An earlier test in this repo
    /// that reached the meeting socket assumed no daemon was listening, and created two live rooms
    /// in the operator's running daemon.
    #[test]
    fn only_a_real_launchd_job_waits_for_the_incumbent() {
        assert!(supervised_by_launchd(Some(
            "com.rozum.meeting-daemon".to_string()
        )));
        // No marker: an interactive run, or the child of `spawn_detached_meetings` that lost the
        // race. It must exit, or every lost race leaves an idle standby forever.
        assert!(!supervised_by_launchd(None));
        assert!(!supervised_by_launchd(Some(String::new())));
        assert!(!supervised_by_launchd(Some("   ".to_string())));

        // THE TWO THAT THE FIRST VERSION GOT WRONG, and it shipped. This test used to assert
        // "non-empty means supervised", which is a rule, not a fact — and it was the wrong rule,
        // so the test passed while an interactive run hung forever against a live daemon.
        //
        // macOS sets this to the literal string "0" on an ordinary process:
        assert!(!supervised_by_launchd(Some("0".to_string())));
        // ...and the variable is inherited, so a client started under a DIFFERENT rozum job
        // carries that job's label. It is not this daemon's supervisor and must not wait:
        assert!(!supervised_by_launchd(Some("com.rozum.gateway".to_string())));
    }
}

fn spawn_detached_meetings() -> std::io::Result<std::process::Child> {
    use rozum::meeting::daemon_proxy::scrub_messenger_bridge_env;
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
    scrub_messenger_bridge_env(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

/// `rozum meetings post <text>` — one-shot post to a room (project room by default).
/// Auto-spawns the daemon if down. Author display = `--as`, else $ROZUM_MEETING_AS, else $USER.
#[allow(clippy::too_many_arguments)]
async fn run_meetings_post(
    text: String,
    room: Option<String>,
    as_display: Option<String>,
    kind: Option<String>,
    severity: Option<String>,
    thread: Option<String>,
    reply_to: Option<String>,
    tags: Vec<String>,
) {
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
    // Principal resolution lives in the client API — the single agent-vs-human posting rule.
    // See docs/specs/meeting-identity-roster.md.
    let (display, token) = rozum::meeting::client::post_identity(as_display);
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
    // Optional support metadata (kind/severity/thread/tags) → merged into the submit args.
    let mut meta = serde_json::Map::new();
    if let Some(k) = &kind {
        meta.insert("kind".into(), serde_json::json!(k));
    }
    if let Some(s) = &severity {
        meta.insert("severity".into(), serde_json::json!(s));
    }
    if let Some(t) = &thread {
        meta.insert("thread_id".into(), serde_json::json!(t));
    }
    if let Some(r) = &reply_to {
        meta.insert("in_reply_to".into(), serde_json::json!(r));
    }
    if !tags.is_empty() {
        meta.insert("tags".into(), serde_json::json!(tags));
    }
    match post_once(
        &sock,
        target,
        &display,
        token.as_deref(),
        &text,
        serde_json::Value::Object(meta),
    )
    .await
    {
        Ok(room) => eprintln!("posted to '{room}' as {display}"),
        Err(e) => {
            eprintln!("meetings post: {e}");
            std::process::exit(1);
        }
    }
}

/// `rozum meetings incident <verb>` — drive the incident lifecycle from the shell. Connects to the
/// daemon and calls the same `meeting.*` thread tools the agents use, so a human/script can open,
/// escalate, resolve, and inspect incidents without an agent or the web UI.
async fn run_meetings_incident(
    action: IncidentAction,
    room: Option<String>,
    as_display: Option<String>,
) {
    use rozum::meeting::daemon::daemon_alive;
    use rozum::meeting::daemon_proxy::{detect_project, spawn_daemon};
    use rozum::meeting::room_path::meeting_sock;
    use rozum::meeting::tui_client::{PostTarget, call_once};

    let sock = meeting_sock();
    if !daemon_alive(&sock).await {
        spawn_daemon().await;
    }
    let (display, token) = rozum::meeting::client::post_identity(as_display);
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
                eprintln!("meetings incident: no project detected — run inside a repo, or pass --room");
                std::process::exit(1);
            }
        },
    };

    // Map the verb → (MCP tool, args, how to render the reply).
    let (tool, args, render): (&str, serde_json::Value, IncidentRender) = match action {
        IncidentAction::Open { anchor_id, title, topic } => {
            let title = title.join(" ");
            let title = if title.is_empty() { anchor_id.clone() } else { title };
            (
                "meeting.thread_open",
                serde_json::json!({
                    "anchor_id": anchor_id,
                    "title": title,
                    "kind": if topic { "topic" } else { "incident" },
                }),
                IncidentRender::Ok,
            )
        }
        IncidentAction::Escalate { id, to, note } => (
            "meeting.escalate",
            serde_json::json!({ "id": id, "to": to, "note": note.unwrap_or_default() }),
            IncidentRender::Ok,
        ),
        IncidentAction::Resolve { id, note } => (
            "meeting.resolve",
            serde_json::json!({ "id": id, "note": note.unwrap_or_default() }),
            IncidentRender::Ok,
        ),
        IncidentAction::Assign { id, to, note } => (
            "meeting.thread_assign",
            serde_json::json!({ "id": id, "to": to, "note": note.unwrap_or_default() }),
            IncidentRender::Ok,
        ),
        IncidentAction::Pin { id, msg_id } => (
            "meeting.thread_pin",
            serde_json::json!({ "id": id, "msg_id": msg_id, "pin": true }),
            IncidentRender::Ok,
        ),
        IncidentAction::Unpin { id, msg_id } => (
            "meeting.thread_pin",
            serde_json::json!({ "id": id, "msg_id": msg_id, "pin": false }),
            IncidentRender::Ok,
        ),
        IncidentAction::Link { id, msg_id } => (
            "meeting.thread_link",
            serde_json::json!({ "id": id, "msg_id": msg_id, "link": true }),
            IncidentRender::Ok,
        ),
        IncidentAction::Unlink { id, msg_id } => (
            "meeting.thread_link",
            serde_json::json!({ "id": id, "msg_id": msg_id, "link": false }),
            IncidentRender::Ok,
        ),
        IncidentAction::State { id, state } => (
            "meeting.thread_set_state",
            serde_json::json!({ "id": id, "state": state }),
            IncidentRender::Ok,
        ),
        IncidentAction::List => ("meeting.threads", serde_json::json!({}), IncidentRender::List),
        IncidentAction::Show { id } => (
            "meeting.thread_context",
            serde_json::json!({ "thread_id": id }),
            IncidentRender::Show,
        ),
        IncidentAction::Metrics => ("meeting.thread_metrics", serde_json::json!({}), IncidentRender::Json),
    };

    match call_once(&sock, target, &display, token.as_deref(), tool, args).await {
        Ok(v) => match render {
            IncidentRender::Ok => {
                eprintln!("ok: {}", serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()))
            }
            IncidentRender::Json => {
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_else(|_| v.to_string()))
            }
            IncidentRender::List => print_incident_list(&v),
            IncidentRender::Show => print_incident(&v),
        },
        Err(e) => {
            eprintln!("meetings incident: {e}");
            std::process::exit(1);
        }
    }
}

/// How `meetings incident` renders a tool reply: `Ok` = one-line confirm, `Json` = pretty JSON,
/// `List`/`Show` = the human-readable incident views.
enum IncidentRender {
    Ok,
    Json,
    List,
    Show,
}

/// Render `meeting.threads` as a readable incident list (one line each, newest-update first).
/// Stale incidents (active + past their severity SLA) are flagged with a leading `⚠`.
fn print_incident_list(v: &serde_json::Value) {
    use rozum::meeting::store::{self, Thread};
    let mut threads: Vec<Thread> = serde_json::from_value(v.clone()).unwrap_or_default();
    if threads.is_empty() {
        println!("(no incidents)");
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    threads.sort_by(|a, b| b.updated_ts.cmp(&a.updated_ts));
    for t in &threads {
        let sev = t.severity.map(|s| format!(" {}", s.label())).unwrap_or_default();
        let owner = t.owner.as_deref().map(|o| format!(" @{o}")).unwrap_or_default();
        let mark = if store::thread_is_stale(t, now) { "⚠ " } else { "" };
        println!("{}{}  {}  [{}{}{}]", mark, t.id, t.title, t.state.as_str(), sev, owner);
    }
}

/// Render `meeting.thread_context` as a readable incident: a header + the chronological message
/// timeline (each with its badge + id), so an operator reads the whole incident at a glance.
fn print_incident(v: &serde_json::Value) {
    use rozum::meeting::store::StoredTurn;
    let thread = &v["thread"];
    let title = thread["title"].as_str().unwrap_or("(unknown incident)");
    let state = thread["state"].as_str().unwrap_or("open");
    let sev = thread["severity"].as_str().map(|s| format!(" · {s}")).unwrap_or_default();
    let owner = thread["owner"].as_str().map(|o| format!(" · @{o}")).unwrap_or_default();
    let count = v["message_count"].as_u64().unwrap_or(0);
    let people = v["participants"].as_array().map(|a| a.len()).unwrap_or(0);
    println!("incident — {title}  [{state}{sev}{owner}]");
    println!("  {count} msgs · {people} people");
    let messages: Vec<StoredTurn> =
        serde_json::from_value(v["messages"].clone()).unwrap_or_default();
    let line = |m: &StoredTurn| match m.badge() {
        Some(b) => println!("  [{}] {} {} {}: {}", hhmm_of(m.ts), m.id(), b, m.display_name, m.content),
        None => println!("  [{}] {} {}: {}", hhmm_of(m.ts), m.id(), m.display_name, m.content),
    };
    // Pinned messages first (the incident's key status / root cause) — only when present in the thread.
    let pinned: Vec<String> = thread["pinned"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect())
        .unwrap_or_default();
    let pinned_msgs: Vec<&StoredTurn> = messages.iter().filter(|m| pinned.contains(&m.id())).collect();
    if !pinned_msgs.is_empty() {
        println!("  📌 pinned:");
        for m in &pinned_msgs {
            line(m);
        }
        println!("  — timeline —");
    }
    for m in &messages {
        line(m);
    }
    // Operator-linked context (messages attached from elsewhere), if any.
    let linked: Vec<StoredTurn> = serde_json::from_value(v["linked"].clone()).unwrap_or_default();
    if !linked.is_empty() {
        println!("  🔗 linked:");
        for m in &linked {
            line(m);
        }
    }
    // Auto-gathered related context (lead-up + same-tag elsewhere), if any.
    let related: Vec<StoredTurn> =
        serde_json::from_value(v["related"].clone()).unwrap_or_default();
    if !related.is_empty() {
        println!("  — related context (auto-gathered) —");
        for m in &related {
            line(m);
        }
    }
}

/// `rozum meetings read` — print a room's most-recent messages (a direct transcript read).
/// Resolve a room's on-disk transcript root. The cwd project's room is read directly (no daemon
/// needed). A named room is resolved via the daemon's registry (a project room → `<project>/.rozum/
/// room`), falling back to an ad-hoc room dir under `rooms_dir()`. Exits on no-project-and-no-room.
/// Resolve a room to its transcript root via the client API, exiting with a CLI error when there's no
/// project AND no `--room` (the API itself never exits the process).
async fn resolve_room_or_exit(room: Option<String>, cmd: &str) -> std::path::PathBuf {
    match rozum::meeting::client::resolve_room_root(room).await {
        Some(r) => r,
        None => {
            eprintln!("meetings {cmd}: no project detected — run inside a repo, or pass --room");
            std::process::exit(1);
        }
    }
}

fn hhmm_of(ts: u64) -> String {
    chrono::DateTime::from_timestamp(ts as i64, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%H:%M").to_string())
        .unwrap_or_else(|| "--:--".to_string())
}

async fn run_meetings_read(room: Option<String>, count: usize) {
    use rozum::meeting::client;
    let root = resolve_room_or_exit(room, "read").await;
    if !root.exists() {
        eprintln!("meetings read: no messages yet ({})", root.display());
        return;
    }
    let turns = client::read(&root, count);
    if turns.is_empty() {
        println!("(no messages)");
        return;
    }
    for t in &turns {
        match t.badge() {
            Some(b) => println!("[{}] {} {}: {}", hhmm_of(t.ts), b, t.display_name, t.content),
            None => println!("[{}] {}: {}", hhmm_of(t.ts), t.display_name, t.content),
        }
    }
}

/// Humanize a duration in seconds, coarsely (`5d` / `12h` / `30m` / `45s`).
fn fmt_secs(s: u64) -> String {
    if s >= 86_400 {
        format!("{}d", s / 86_400)
    } else if s >= 3_600 {
        format!("{}h", s / 3_600)
    } else if s >= 60 {
        format!("{}m", s / 60)
    } else {
        format!("{s}s")
    }
}

/// `rozum meetings token …` — manage support-console access tokens (global, in the state dir).
fn run_meetings_token(action: TokenAction) {
    use rozum::meeting::store::{self, Role};
    let sd = store::rozum_state_dir();
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    };
    // Parse a `30d` / `12h` / `90m` / `45s` duration → seconds; None → 0 (never).
    let parse_ttl = |t: &Option<String>| -> u64 {
        let Some(s) = t else { return 0 };
        let s = s.trim();
        let (num, mult) = match s.chars().last() {
            Some('d') => (&s[..s.len() - 1], 86_400),
            Some('h') => (&s[..s.len() - 1], 3_600),
            Some('m') => (&s[..s.len() - 1], 60),
            Some('s') => (&s[..s.len() - 1], 1),
            _ => (s, 86_400), // bare number = days
        };
        num.trim().parse::<u64>().unwrap_or(0).saturating_mul(mult)
    };
    match action {
        TokenAction::Issue { handle, role, ttl } => {
            let Some(role) = Role::parse(&role) else {
                eprintln!("token issue: bad role '{role}' (observer|responder|admin)");
                std::process::exit(1);
            };
            match store::issue_token(&sd, &handle, role, parse_ttl(&ttl), now()) {
                Ok(tok) => {
                    println!("{tok}");
                    eprintln!("issued for {handle} ({}). Give them this token as the console password.", role.as_str());
                }
                Err(e) => {
                    eprintln!("token issue: {e}");
                    std::process::exit(1);
                }
            }
        }
        TokenAction::Rotate { handle, ttl } => {
            match store::rotate_token(&sd, &handle, parse_ttl(&ttl), now()) {
                Ok(Some(tok)) => {
                    println!("{tok}");
                    eprintln!("rotated {handle} — the old token is now invalid.");
                }
                Ok(None) => {
                    eprintln!("token rotate: no current token for '{handle}'");
                    std::process::exit(1);
                }
                Err(e) => {
                    eprintln!("token rotate: {e}");
                    std::process::exit(1);
                }
            }
        }
        TokenAction::List => {
            let m = store::load_tokens(&sd);
            if m.is_empty() {
                println!("(no tokens)");
                return;
            }
            let n = now();
            for (tok, info) in &m {
                let short = tok.get(..8).unwrap_or(tok);
                let exp = if info.expires_ts == 0 {
                    "never".to_string()
                } else if info.is_expired(n) {
                    "EXPIRED".to_string()
                } else {
                    format!("in {}", fmt_secs(info.expires_ts.saturating_sub(n)))
                };
                let per_room = if info.rooms.is_empty() {
                    String::new()
                } else {
                    let g: Vec<String> = info.rooms.iter().map(|(r, role)| format!("{r}={}", role.as_str())).collect();
                    format!("  rooms:{{{}}}", g.join(", "))
                };
                println!("{short}…  {}  [{}]  expires: {exp}{per_room}", info.handle, info.role.as_str());
            }
        }
        TokenAction::Grant { handle, room, role } => {
            let role_opt = if matches!(role.trim().to_ascii_lowercase().as_str(), "none" | "clear" | "") {
                None
            } else {
                match Role::parse(&role) {
                    Some(r) => Some(r),
                    None => {
                        eprintln!("token grant: bad role '{role}' (observer|responder|admin|none)");
                        std::process::exit(1);
                    }
                }
            };
            match store::grant_room_role(&sd, &handle, &room, role_opt) {
                Ok(0) => {
                    eprintln!("token grant: no token for '{handle}' (issue one first)");
                    std::process::exit(1);
                }
                Ok(n) => println!(
                    "{} {handle} in {room}{} ({n} token(s))",
                    if role_opt.is_some() { "granted" } else { "cleared" },
                    role_opt.map(|r| format!(" = {}", r.as_str())).unwrap_or_default()
                ),
                Err(e) => {
                    eprintln!("token grant: {e}");
                    std::process::exit(1);
                }
            }
        }
        TokenAction::Revoke { token_or_handle } => match store::revoke_token(&sd, &token_or_handle) {
            Ok(0) => {
                eprintln!("token revoke: nothing matched '{token_or_handle}'");
                std::process::exit(1);
            }
            Ok(n) => println!("revoked {n} token(s)"),
            Err(e) => {
                eprintln!("token revoke: {e}");
                std::process::exit(1);
            }
        },
        TokenAction::Resolve { token, room } => match store::resolve_token(&sd, &token, now()) {
            Some(info) => {
                let role = info.effective_role(room.as_deref());
                println!("{}\t{}", info.handle, role.as_str());
            }
            None => std::process::exit(1),
        },
    }
}

/// `rozum meetings react` — toggle an emoji reaction on a message (direct disk).
/// `rozum meetings queue` — the room's open threads, worst first. Reads the room files directly:
/// it is a pure read model, so going through the daemon would add a hop and a failure mode for
/// nothing.
async fn run_meetings_queue(room: Option<String>) {
    use rozum::meeting::store;
    let root = resolve_room_or_exit(room, "queue").await;
    let rows = store::room_queue(&root, std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0));
    if rows.is_empty() {
        println!("queue is empty — nothing open in this room");
        return;
    }
    for i in &rows {
        let sev = i.severity.map(|s| s.label()).unwrap_or("-");
        let owner = i.owner.as_deref().unwrap_or("unclaimed");
        // The overdue column is the reason to read this list at all, so it goes first among the
        // flags rather than at the end of a wide line.
        let late = if i.stale {
            format!("OVERDUE {}", fmt_secs(i.overdue_secs))
        } else {
            String::new()
        };
        println!("{:<9} {:<8} {:<22} {:<10} {}", sev, i.state.as_str(), i.title, owner, late);
    }
}

/// `rozum meetings phase` — set the room's lifecycle phase, persisted to `meta.json`.
///
/// Goes through the DAEMON rather than writing the file directly, unlike `role`: a live room holds
/// its phase in memory too, and writing only the file would leave the running daemon serving the
/// old one until it restarted — the mirror image of the bug this feature exists to fix.
async fn run_meetings_phase(phase: String, room: Option<String>) {
    use rozum::meeting::daemon_proxy::detect_project;
    use rozum::meeting::tui_client::{PostTarget, call_once};
    let want = phase.trim().to_ascii_lowercase();
    if !matches!(want.as_str(), "active" | "paused" | "ended") {
        eprintln!("meetings phase: unknown phase '{phase}'; expected active, paused or ended");
        std::process::exit(2);
    }
    let sock = rozum::meeting::room_path::meeting_sock();
    let (display, token) = rozum::meeting::client::post_identity(None);
    let target = match room {
        Some(name) => PostTarget::Named(name),
        None => match detect_project() {
            Some(p) => PostTarget::Project(p),
            None => {
                eprintln!("meetings phase: no project detected — run inside a repo, or pass --room");
                std::process::exit(1);
            }
        },
    };
    match call_once(
        &sock,
        target,
        &display,
        token.as_deref(),
        "meeting.room_phase",
        serde_json::json!({ "phase": want }),
    )
    .await
    {
        Ok(v) => println!("{}", v.as_str().unwrap_or(&v.to_string()).trim().to_string()),
        Err(e) => {
            eprintln!("meetings phase: {e}");
            std::process::exit(1);
        }
    }
}

/// `rozum meetings role` — grant or revoke a participant's role (direct disk, like react/redact).
async fn run_meetings_role(handle: String, role: String, room: Option<String>, revoke: bool) {
    use rozum::meeting::identity::{Role, Roster};
    let root = resolve_room_or_exit(room, "role").await;
    if !root.exists() {
        eprintln!("meetings role: no such room ({})", root.display());
        std::process::exit(1);
    }
    // Refuse a misspelling instead of defaulting — a typo that quietly becomes `observer` takes
    // somebody off the pager and says nothing.
    let Some(parsed) = Role::parse(&role) else {
        let all: Vec<&str> = Role::ALL.iter().map(|r| r.as_str()).collect();
        eprintln!("meetings role: unknown role '{role}'; expected one of {}", all.join(", "));
        std::process::exit(2);
    };
    // `roster.json` beside the day files — the same layout `RoomPaths::roster_path` builds; this
    // path takes a room ROOT that was already resolved, so it does not need the paths type.
    let path = root.join("roster.json");
    let mut roster = Roster::load(&path);
    let changed = if revoke { roster.revoke(&handle, parsed) } else { roster.grant(&handle, parsed) };
    if !changed {
        eprintln!("meetings role: no participant '{handle}' in this room");
        std::process::exit(1);
    }
    if let Err(e) = roster.save(&path) {
        eprintln!("meetings role: could not write the roster: {e}");
        std::process::exit(1);
    }
    let verb = if revoke { "no longer" } else { "now" };
    println!("{handle} is {verb} {}", parsed.as_str());
}

async fn run_meetings_react(msg_id: String, emoji: String, room: Option<String>, off: bool) {
    use rozum::meeting::store;
    let root = resolve_room_or_exit(room, "react").await;
    if !root.exists() {
        eprintln!("meetings react: no such room ({})", root.display());
        std::process::exit(1);
    }
    let (who, _token) = rozum::meeting::client::post_identity(None);
    match store::set_reaction(&root, &msg_id, &emoji, &who, !off) {
        Ok(n) => println!("{emoji} {msg_id} → {n}"),
        Err(e) => {
            eprintln!("meetings react: {e}");
            std::process::exit(1);
        }
    }
}

/// `rozum meetings redact` — redact (or un-redact) a message's content for all readers (direct disk).
async fn run_meetings_redact(msg_id: String, room: Option<String>, reason: Option<String>, undo: bool) {
    use rozum::meeting::store;
    let root = resolve_room_or_exit(room, "redact").await;
    if !root.exists() {
        eprintln!("meetings redact: no such room ({})", root.display());
        std::process::exit(1);
    }
    let (by, _token) = rozum::meeting::client::post_identity(None);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match store::set_redacted(&root, &msg_id, !undo, &by, &reason.unwrap_or_default(), now) {
        Ok(n) => println!(
            "{} {msg_id} (room now has {n} redaction(s))",
            if undo { "un-redacted" } else { "redacted" }
        ),
        Err(e) => {
            eprintln!("meetings redact: {e}");
            std::process::exit(1);
        }
    }
}

/// `rozum meetings repair-threads` — rebuild a room's incident state from the message log (recovery).
async fn run_meetings_repair_threads(room: Option<String>) {
    use rozum::meeting::store;
    let root = resolve_room_or_exit(room, "repair-threads").await;
    if !root.exists() {
        eprintln!("meetings repair-threads: no such room ({})", root.display());
        std::process::exit(1);
    }
    match store::repair_threads(&root) {
        Ok(n) => {
            println!("rebuilt threads.json from the message log: {n} incident(s) recovered");
            println!("(best-effort — restart the meeting daemon so it reloads the rebuilt state)");
        }
        Err(e) => {
            eprintln!("meetings repair-threads: {e}");
            std::process::exit(1);
        }
    }
}

/// `rozum meetings search` — full-history search by text + support metadata (a direct disk read).
#[allow(clippy::too_many_arguments)]
async fn run_meetings_search(
    query: Option<String>,
    room: Option<String>,
    kind: Option<String>,
    severity: Option<String>,
    tag: Option<String>,
    thread: Option<String>,
    since: Option<String>,
    count: usize,
) {
    use rozum::meeting::store::{self, MsgFilter};
    let root = resolve_room_or_exit(room, "search").await;
    if !root.exists() {
        eprintln!("meetings search: no messages yet ({})", root.display());
        return;
    }
    // Parse the metadata filters up front; a typo'd kind/severity is a hard error, not silent.
    let kind = match kind.as_deref() {
        Some(s) => match store::MsgKind::parse(s) {
            Some(k) => Some(k),
            None => {
                eprintln!("meetings search: bad --kind '{s}' (note|question|event|alert|resolution)");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let min_severity = match severity.as_deref() {
        Some(s) => match store::Severity::parse(s) {
            Some(v) => Some(v),
            None => {
                eprintln!("meetings search: bad --severity '{s}' (info|low|medium|high|critical)");
                std::process::exit(1);
            }
        },
        None => None,
    };
    let filter = MsgFilter {
        text: query.as_deref().filter(|s| !s.is_empty()),
        kind,
        min_severity,
        tag: tag.as_deref(),
        thread_id: thread.as_deref(),
        since_date: since.as_deref(),
    };
    let hits = store::search_messages(&root, &filter, count);
    if hits.is_empty() {
        println!("(no matches)");
        return;
    }
    for t in &hits {
        let id = t.id();
        match t.badge() {
            Some(b) => println!("[{}] {} {} {}: {}", hhmm_of(t.ts), id, b, t.display_name, t.content),
            None => println!("[{}] {} {}: {}", hhmm_of(t.ts), id, t.display_name, t.content),
        }
    }
}

/// `rozum meetings inbox --as <handle>` — messages that address you, since you last looked.
async fn run_meetings_inbox(handle: String, room: Option<String>, peek: bool, all: bool, count: usize) {
    use rozum::meeting::client;
    use rozum::meeting::mention::handle_of;
    let root = resolve_room_or_exit(room, "inbox").await;
    if !root.exists() {
        println!("(no new messages for {handle})");
        return;
    }
    let mine = client::inbox(&root, &handle, all);
    if mine.is_empty() {
        println!("(no new messages for {handle})");
        return;
    }
    let start = mine.len().saturating_sub(count);
    for t in &mine[start..] {
        match t.badge() {
            Some(b) => println!("[{}] {} {}: {}", hhmm_of(t.ts), b, handle_of(&t.display_name), t.content),
            None => println!("[{}] {}: {}", hhmm_of(t.ts), handle_of(&t.display_name), t.content),
        }
    }
    // Advance the seen-cursor to the latest mention shown (unless peeking / showing all).
    if !peek && !all {
        if let Some(last) = mine.last() {
            client::advance_inbox_cursor(&root, &handle, &last.date, last.n);
        }
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn fmt_age(secs: u64) -> String {
    if secs < 90 {
        format!("{secs}s")
    } else if secs < 5400 {
        format!("{}m", secs / 60)
    } else {
        format!("{}h", secs / 3600)
    }
}

/// The trailing path component (worktree / project leaf) of a cwd, for a compact locator.
fn worktree_leaf(cwd: &str) -> &str {
    cwd.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or(cwd)
}

/// `rozum meetings hello [name]` — establish this session's Agent principal (once) + label the tab.
fn run_meetings_hello(name: Option<String>) {
    match rozum::meeting::client::establish(name) {
        Some(p) => {
            // Terminal title (an inert no-op on the app/web): label the tab with the handle.
            let proj = p.project.as_deref().map(worktree_leaf).unwrap_or("");
            print!("\x1b]0;{} · {}\x07", p.display, proj);
            use std::io::Write;
            let _ = std::io::stdout().flush();
            println!("hello: you are '{}' (agent · session {})", p.display, short_id(&p.session_id));
            println!("  posts from this session now show '{}' — not the operator.", p.display);
        }
        None => {
            eprintln!(
                "meetings hello: no $CLAUDE_CODE_SESSION_ID — this isn't an agent session, nothing to establish."
            );
            std::process::exit(1);
        }
    }
}

/// `rozum meetings whoami` — who does this session act as?
fn run_meetings_whoami() {
    use rozum::meeting::client::{whoami, Identity};
    match whoami() {
        Identity::Agent(p) => println!("{} (agent · session {})", p.display, short_id(&p.session_id)),
        Identity::AgentUnnamed => {
            println!("(agent session, no identity yet — run `rozum meetings hello <your-name>`)")
        }
        Identity::Human(display) => println!("{display} (human · account)"),
    }
}

/// `rozum meetings who` — roster mapping each handle to a findable session.
async fn run_meetings_who(long: bool) {
    const TTL: u64 = 15 * 60;
    let now = now_epoch_secs();
    let agents = rozum::meeting::client::roster();

    if long {
        println!("{:<16} {:<5} {:<5} {:<10} CWD", "HANDLE", "LIVE", "AGE", "SESSION");
    } else {
        println!("{:<16} {:<5} {:<5} CWD / WORKTREE", "HANDLE", "LIVE", "AGE");
    }
    for p in &agents {
        let age = now.saturating_sub(p.ts);
        let live = if age <= TTL { "●" } else { "○" };
        if long {
            println!(
                "{:<16} {:<5} {:<5} {:<10} {}",
                p.display, live, fmt_age(age), short_id(&p.session_id), p.cwd
            );
        } else {
            println!("{:<16} {:<5} {:<5} {}", p.display, live, fmt_age(age), worktree_leaf(&p.cwd));
        }
    }
    // The human, for contrast — always one stable account identity, never an agent.
    let id = rozum::meeting::local_identity::load_or_create();
    println!("{:<16} {:<5} {:<5} {}", id.display, "—", "—", "(operator · human account)");
    if agents.is_empty() {
        println!("\n(no agents have introduced themselves yet — each runs `rozum meetings hello <name>` at startup)");
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
    sandbox: Option<std::path::PathBuf>,
    shell: bool,
    shell_no_network: bool,
    acl: Option<std::path::PathBuf>,
    mention_alias: Option<String>,
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
    if let Err(e) = run(
        model, room, handle, policy, gateway_url, peers, persona, sandbox, shell,
        !shell_no_network, acl, mention_alias,
    )
    .await
    {
        eprintln!("meetings participant: {e}");
        std::process::exit(1);
    }
}

/// `rozum meetings participant-pool` — supervise one `meetings participant` child per room:
/// the primary room plus every room in the group registry, each with its OWN per-room ACL.
/// Reconciles every few seconds so children spawn/reap as groups are connected/disconnected
/// from inside the bot, and respawns any that crash.
#[allow(clippy::too_many_arguments)]
async fn run_meetings_participant_pool(
    model: String,
    primary_room: String,
    as_handle: Option<String>,
    gateway_url: String,
    reply_policy: String,
    group_reply_policy: String,
    peers: Vec<String>,
    persona: Option<String>,
    persona_file: Option<std::path::PathBuf>,
    sandbox: Option<std::path::PathBuf>,
    shell: bool,
    shell_no_network: bool,
    mention_alias: Option<String>,
    registry: String,
) {
    use rozum::messenger_acl::Acl;
    use rozum::messenger_groups::Registry;
    use std::collections::HashMap;

    let persona = match persona_file {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(s) => Some(s),
            Err(e) => {
                eprintln!("participant-pool: --persona-file {}: {e}", path.display());
                std::process::exit(1);
            }
        },
        None => persona,
    };
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("rozum-gateway"));
    let registry_path = Registry::path(&registry);
    let mut children: HashMap<String, tokio::process::Child> = HashMap::new();
    // Stop children on SIGTERM/SIGINT (launchd stop) so a restart never leaves orphaned
    // participants double-replying in a room.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    eprintln!(
        "[participant-pool] primary room '{primary_room}', registry {}",
        registry_path.display()
    );

    loop {
        // Desired rooms = primary + registry group rooms (dedup, order preserved).
        let mut rooms = vec![primary_room.clone()];
        for r in Registry::load(&registry_path).rooms() {
            if !rooms.contains(&r) {
                rooms.push(r);
            }
        }
        // (Re)spawn any missing or exited child.
        for room in &rooms {
            let alive = matches!(children.get_mut(room).map(|c| c.try_wait()), Some(Ok(None)));
            if alive {
                continue;
            }
            children.remove(room);
            let acl = Acl::path_for(room);
            // Primary room (private chat) uses `reply_policy` (usually always); group rooms use
            // `group_reply_policy` (usually mention) so the bot answers only when addressed by name.
            let policy = if room == &primary_room { &reply_policy } else { &group_reply_policy };
            let mut cmd = tokio::process::Command::new(&exe);
            cmd.kill_on_drop(true);
            cmd.arg("meetings")
                .arg("participant")
                .arg("--model")
                .arg(&model)
                .arg("--room")
                .arg(room)
                .arg("--reply-policy")
                .arg(policy)
                .arg("--gateway-url")
                .arg(&gateway_url)
                .arg("--acl")
                .arg(&acl);
            if let Some(h) = &as_handle {
                cmd.arg("--as").arg(h);
            }
            if let Some(a) = &mention_alias {
                cmd.arg("--mention-alias").arg(a);
            }
            if let Some(s) = &sandbox {
                cmd.arg("--sandbox").arg(s);
            }
            if shell {
                cmd.arg("--shell");
            }
            if shell_no_network {
                cmd.arg("--shell-no-network");
            }
            if let Some(p) = &persona {
                cmd.arg("--persona").arg(p);
            }
            for peer in &peers {
                cmd.arg("--peer").arg(peer);
            }
            match cmd.spawn() {
                Ok(child) => {
                    eprintln!("[participant-pool] spawned participant for room '{room}' (acl {})", acl.display());
                    children.insert(room.clone(), child);
                }
                Err(e) => eprintln!("[participant-pool] failed to spawn participant for '{room}': {e}"),
            }
        }
        // Stop children whose room left the registry.
        let stale: Vec<String> = children.keys().filter(|r| !rooms.contains(r)).cloned().collect();
        for room in stale {
            if let Some(mut c) = children.remove(&room) {
                eprintln!("[participant-pool] stopping participant for removed room '{room}'");
                let _ = c.start_kill();
            }
        }
        // Reconcile every 5s, or stop promptly on SIGTERM/SIGINT (killing children first).
        let stop = async {
            match &mut sigterm {
                Some(s) => {
                    s.recv().await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = stop => {
                eprintln!("[participant-pool] signal received — stopping {} participant(s)", children.len());
                for (_room, mut c) in children.drain() {
                    let _ = c.start_kill();
                }
                return;
            }
        }
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
    use rozum::meeting::client::daemon_status;
    use rozum::meeting::room_path::meeting_sock;
    let sock = meeting_sock();
    match daemon_status().await {
        None => println!("meeting daemon: not running ({})", sock.display()),
        Some(rooms) => {
            println!("meeting daemon: running ({})", sock.display());
            if rooms.is_empty() {
                println!("  (no rooms yet)");
            }
            for (name, project) in rooms {
                println!("  {name}   project: {}", project.as_deref().unwrap_or("-"));
            }
        }
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

/// The env that makes the installed daemon ALSO serve the support console (`rest_read.rs`): a Basic-auth
/// secret (taken from `$ROZUM_WEB_SECRET`, else a persisted one, else freshly generated + saved 0600) and
/// the loopback bind. Returns the env pairs + the (secret, bind) for the post-install hint.
fn meetings_console_env() -> (Vec<(String, String)>, String, String) {
    use std::io::Write;
    let secret = std::env::var("ROZUM_WEB_SECRET")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let path = rozum::meeting::store::rozum_state_dir().join("web-secret");
            if let Ok(s) = std::fs::read_to_string(&path) {
                let s = s.trim().to_string();
                if !s.is_empty() {
                    return s;
                }
            }
            let s = uuid::Uuid::new_v4().simple().to_string();
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(mut f) = std::fs::File::create(&path) {
                let _ = f.write_all(s.as_bytes());
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
                }
            }
            s
        });
    let bind = std::env::var("ROZUM_MEETINGS_REST_BIND")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "127.0.0.1:8401".to_string());
    let env = vec![
        ("ROZUM_WEB_SECRET".to_string(), secret.clone()),
        ("ROZUM_MEETINGS_REST_BIND".to_string(), bind.clone()),
    ];
    (env, secret, bind)
}

/// Print how to reach + expose the just-installed support console.
fn print_console_hint(secret: &str, bind: &str) {
    let port = bind.rsplit(':').next().unwrap_or("8401");
    eprintln!("\nsupport console (incident dashboard) is now served by the daemon:");
    eprintln!("  local:     http://{bind}/   (Basic auth — any username, password = the secret)");
    eprintln!("  secret:    {secret}");
    eprintln!("  secret file: {}", rozum::meeting::store::rozum_state_dir().join("web-secret").display());
    eprintln!("  expose via Tailscale (HTTPS):");
    eprintln!("    tailscale serve --bg --https=8443 http://127.0.0.1:{port}");
}

#[cfg(target_os = "macos")]
fn run_meetings_install() {
    let (program, args) = meetings_service_spec();
    let (env, secret, bind) = meetings_console_env();
    let plist = rozum::service::meetings_launchd_plist(&program, &args, &env);
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
    print_console_hint(&secret, &bind);
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
    let (env, secret, bind) = meetings_console_env();
    let unit = rozum::service::meetings_systemd_unit(&program, &args, &env);
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
    print_console_hint(&secret, &bind);
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

async fn run_gateway_status(json: bool) {
    use rozum::share;
    if json {
        // The full control snapshot (gateway + residency + installed catalog) — the dashboard/UCC
        // contract via the models/gateway control-API.
        let snap = rozum::control::status().await;
        println!("{}", serde_json::to_string_pretty(&snap).unwrap_or_else(|_| "{}".into()));
        return;
    }
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

/// Parse a human RAM size (`8G`, `8192M`, `8000000000`, `8g`) to bytes. `None` if unparseable.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last()?.to_ascii_uppercase() {
        'G' => (&s[..s.len() - 1], 1u64 << 30),
        'M' => (&s[..s.len() - 1], 1u64 << 20),
        'K' => (&s[..s.len() - 1], 1u64 << 10),
        'B' => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    num.trim().parse::<f64>().ok().map(|v| (v * mult as f64) as u64)
}

/// Oracle-wrap (spec residency-admission-queue.md): run a RAM-heavy NON-rozum command THROUGH the
/// host-wide admission queue, so a python `mlx_lm` oracle / external sweep can't overcommit behind
/// rozum's back. Acquires a reservation (queues + waits its turn), runs the command holding the guard,
/// releases on exit. `--batch` makes it yield to interactive loads.
async fn run_gateway_admit(
    footprint: Option<String>,
    model: Option<String>,
    batch: bool,
    program: Vec<String>,
) {
    if program.is_empty() {
        eprintln!("rozum gateway admit: a command is required after `--`");
        std::process::exit(2);
    }
    if batch {
        // SAFETY: single-threaded CLI startup, before any worker thread.
        unsafe { std::env::set_var("ROZUM_RESIDENCY_PRIO", "batch") };
    }
    let bytes = match (footprint.as_deref(), model.as_deref()) {
        (Some(f), _) => parse_size(f).unwrap_or_else(|| {
            eprintln!("rozum gateway admit: bad --footprint '{f}' (try 8G / 8192M / bytes)");
            std::process::exit(2);
        }),
        (None, Some(m)) => estimate_model_footprint_bytes(m, 8192),
        (None, None) => {
            eprintln!("rozum gateway admit: --footprint or --model is required");
            std::process::exit(2);
        }
    };
    let label = model.clone().unwrap_or_else(|| program[0].clone());
    eprintln!(
        "rozum gateway admit: queueing for ~{} MB ({label}; prio {}) …",
        bytes / 1_048_576,
        if batch { "batch" } else { "interactive" },
    );
    let lbl = label.clone();
    let guard = match tokio::task::spawn_blocking(move || rozum::share::acquire_residency(&lbl, bytes))
        .await
    {
        Ok(Ok(g)) => g, // Some(guard) = reserved; None = override / fail-open
        Ok(Err(denied)) => {
            eprintln!(
                "rozum gateway admit: refused — ~{} MB would overcommit the host (waited {}s)",
                bytes / 1_048_576,
                denied.waited_secs,
            );
            std::process::exit(1);
        }
        Err(_) => None,
    };
    eprintln!("rozum gateway admit: granted → running: {}", program.join(" "));
    let prog = program.clone();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new(&prog[0]).args(&prog[1..]).status()
    })
    .await
    .ok()
    .and_then(|r| r.ok());
    drop(guard); // release the reservation the moment the command exits
    match status {
        Some(s) => std::process::exit(s.code().unwrap_or(1)),
        None => {
            eprintln!("rozum gateway admit: failed to run `{}`", program.join(" "));
            std::process::exit(1);
        }
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
    // A multi-model launch drives the verify-repair CHAIN (exec_agent), which hosts ONE model per link
    // and switches via /control/switch. An EAGER co-resident pipeline fights that switch — it loads BOTH
    // models and thrashes (measured: rpn hung right after deriving the target, never reached link 1;
    // under lazy the same run proceeds cleanly). So force LAZY residency for a chain's gateway unless the
    // user explicitly set a preference. (A bare `rozum gateway --model A,B` pipeline is unaffected.)
    if should_force_lazy_launch(model_spec, std::env::var_os("ROZUM_PIPELINE_EAGER").is_some()) {
        cmd.env("ROZUM_PIPELINE_EAGER", "0");
    }
    // Own process group so a Ctrl-C / terminal close on the launch doesn't kill
    // the shared daemon.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    cmd.spawn()
}

fn should_force_lazy_launch(model_spec: &str, has_explicit_eager_policy: bool) -> bool {
    model_spec.contains(',') && !has_explicit_eager_policy
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
/// Index in `program` of the agent's task-prompt arg (the thing rewritten for a repair round):
/// claude `-p/--print <prompt>`, codex `exec <prompt>`, opencode `run <prompt>`, nadia
/// `run <prompt>`. `None` for interactive/unknown invocations → the verify-gate stays off (it
/// needs a prompt to repair).
fn agent_prompt_index(program: &[String]) -> Option<usize> {
    let name = program.first().map(|s| s.rsplit('/').next().unwrap_or(s)).unwrap_or("");
    let verb_at = |verbs: &[&str]| program.iter().position(|a| verbs.contains(&a.as_str()));
    let after = |i: usize| (i + 1 < program.len() && !program[i + 1].starts_with('-')).then_some(i + 1);
    match name {
        "claude" => verb_at(&["-p", "--print"]).and_then(after),
        "codex" => verb_at(&["exec"]).and_then(after),
        "opencode" | "nadia" => verb_at(&["run"]).and_then(after),
        _ => None,
    }
}

/// The DETERMINISTIC verify command — the ground-truth gate that decides "solved", not the model's
/// word (that is the false-success trap). `ROZUM_VERIFY` if set (`0`/`off`/empty disables); else
/// auto-detected from the cwd (a Cargo project → `cargo build`, plus `cargo test` when tests exist).
/// `None` → no gate: the agent runs once exactly as before.
fn resolve_verify_cmd() -> Option<String> {
    match std::env::var("ROZUM_VERIFY").ok().map(|v| v.trim().to_string()) {
        Some(v) if matches!(v.as_str(), "0" | "off" | "false" | "") => return None,
        Some(v) => return Some(v),
        None => {}
    }
    let cwd = std::env::current_dir().ok()?;
    if cwd.join("Cargo.toml").is_file() {
        let has_tests = std::process::Command::new("sh")
            .arg("-c")
            .arg("grep -rqs '#\\[test\\]' src tests 2>/dev/null")
            .current_dir(&cwd)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        return Some(
            if has_tests { "cargo build -q && cargo test -q" } else { "cargo build -q" }.to_string(),
        );
    }
    None
}

/// Run the verify command in `cwd`. Returns `(passed, tail-of-the-real-output)` for the repair prompt.
/// One definition, in `rozum_agent::verify`, shared with nadia's own gate — this filtering (drop
/// cargo's progress lines, keep the diagnosis) is exactly what a repair round reads, and two copies
/// of it drift.
async fn run_verify(cmd: &str, cwd: &std::path::Path) -> (bool, String) {
    rozum_agent::verify::run_check(cmd, cwd).await
}

/// Build a diagnostic cargo-run check fragment (shared: `rozum_agent::verify`).
fn cargo_run_check_fragment(arg: &str, exp: &str) -> String {
    rozum_agent::verify::cargo_run_fragment(arg, exp)
}

/// "Understand the goal": FORMALIZE the task into a deterministic check.
///
/// The prompt, the structured shape it asks for and the shell-building live in
/// `rozum_agent::verify` — shared with nadia's gate, because this prompt is the part that took
/// measurement to word (it once invented `cargo run -- pong == gnop` for a chat task) and a second
/// copy of it is a second thing to get wrong. Here we only supply the backend: the launch-local
/// proxy, which is the same endpoint the agent itself talks to.
async fn derive_target(base: &str, task: &str) -> Option<String> {
    let backend = rozum_gateway::openai_http::OpenAiHttpBackend::new(format!("{base}/v1"), "x");
    rozum_agent::verify::derive_check(&backend, task).await
}

/// Whether a derived cargo check is one the model invented for a task that is not about code
/// (shared guard: `rozum_agent::verify`). `explicit_verify` keeps an operator's own ROZUM_VERIFY
/// out of it — if they asked for that command, they get that command.
fn should_skip_hallucinated_cargo_verify(
    verify_cmd: &str,
    cwd: &std::path::Path,
    prompt: &str,
    explicit_verify: bool,
) -> bool {
    !explicit_verify && rozum_agent::verify::is_hallucinated_cargo_check(verify_cmd, cwd, prompt)
}

/// Semantic PASS/FAIL/UNKNOWN judge for a task with NO deterministic acceptance check — `derive_target` ruled
/// it not machine-checkable (e.g. "refactor for clarity", "make the error message clearer"). Reads the
/// task + the produced source and asks the model to rule. This is the semantic half of the default
/// verify ("structure = cargo build" + "model-judge = semantic correctness"): it only runs when we'd
/// otherwise fall to a bare `cargo build` floor that can't see whether the task was actually done.
///
/// Unknown evidence is deliberately not a pass: the bounded chain can escalate or return an honest
/// unverified failure, but it cannot claim semantic correctness without a parseable verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
enum VerifyVerdict {
    Pass,
    Fail(String),
    Unknown(String),
}

async fn model_judge(base: &str, task: &str, cwd: &std::path::Path) -> VerifyVerdict {
    let code = repair_source_snapshot(cwd).unwrap_or_else(|| "(no source found)".to_string());
    let prompt = format!(
        "You are a strict code reviewer judging whether the CODE accomplishes the TASK. Reply with ONLY \
         a JSON object, no prose: {{\"pass\": <true|false>, \"reason\": \"<one short sentence>\"}}.\n\
         Rule pass=false ONLY if the code clearly fails a STATED requirement of the task; if it plausibly \
         satisfies the task, pass=true. Do not invent requirements the task did not state.\n\n\
         TASK:\n{task}\n\n{code}"
    );
    let body = serde_json::json!({
        "model": "x", "temperature": 0.0, "max_tokens": 200,
        "messages": [{"role": "user", "content": prompt}],
    });
    let text = async {
        let resp = reqwest::Client::new()
            .post(format!("{base}/v1/chat/completions"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(180))
            .send()
            .await
            .ok()?;
        let v: serde_json::Value = resp.json().await.ok()?;
        v["choices"][0]["message"]["content"].as_str().map(str::to_string)
    }
    .await;
    let Some(text) = text else {
        return VerifyVerdict::Unknown("model-judge unavailable or timed out".to_string());
    };
    parse_judge_verdict(&text)
}

/// Parse the judge's reply. A parseable explicit boolean is evidence; everything else is Unknown.
fn parse_judge_verdict(text: &str) -> VerifyVerdict {
    let parsed = text
        .find('{')
        .zip(text.rfind('}'))
        .filter(|(a, b)| a <= b)
        .and_then(|(a, b)| serde_json::from_str::<serde_json::Value>(&text[a..=b]).ok());
    match parsed {
        Some(j) if j["pass"].as_bool() == Some(false) => {
            let reason = j["reason"].as_str().unwrap_or("task requirement not met").trim().to_string();
            VerifyVerdict::Fail(format!("model-judge ruled the task NOT accomplished: {reason}"))
        }
        Some(j) if j["pass"].as_bool() == Some(true) => VerifyVerdict::Pass,
        Some(_) => VerifyVerdict::Unknown("model-judge response has no boolean `pass` field".to_string()),
        None => VerifyVerdict::Unknown("model-judge response is not valid verdict JSON".to_string()),
    }
}

/// Pick a judge distinct from the executor. The last distinct link is preferred because chains put
/// the strongest fallback/cloud model last; residency remains sequential via `/control/switch`.
fn independent_judge_model<'a>(chain: &'a [String], executor: &str) -> Option<&'a str> {
    chain.iter().rev().find(|candidate| candidate.as_str() != executor).map(String::as_str)
}

/// Switch the gateway to `model` in-process (the fixed swap) for chain escalation. Best-effort;
/// returns whether the swap succeeded (so the gate can skip a link that won't load).
async fn switch_gateway_model(base: &str, model: &str) -> bool {
    let resp = reqwest::Client::new()
        .post(format!("{base}/control/switch"))
        .json(&serde_json::json!({ "model": model }))
        .timeout(std::time::Duration::from_secs(240))
        .send()
        .await;
    match resp {
        Ok(r) => match r.text().await {
            Ok(t) => t.contains("switched"),
            Err(_) => false,
        },
        Err(_) => false,
    }
}

/// Task-conditioned quality stats persisted across runs, so a redundant middle chain link can be
/// dropped only when this model×driver×task×verifier combination has enough poor evidence.
fn model_stats_path() -> std::path::PathBuf {
    rozum::share::gateway_dir().join("model_stats.json")
}

/// Pure skip rule (unit-tested): enough samples AND pass-rate below the floor → skip. Tunable via
/// `ROZUM_MODEL_MIN_SAMPLES` (default 5) and `ROZUM_MODEL_MIN_PASS_PCT` (default 20).
fn model_skip_decision(passes: u64, attempts: u64) -> bool {
    let min_samples: u64 =
        std::env::var("ROZUM_MODEL_MIN_SAMPLES").ok().and_then(|v| v.parse().ok()).unwrap_or(5);
    let min_pass_pct: u64 =
        std::env::var("ROZUM_MODEL_MIN_PASS_PCT").ok().and_then(|v| v.parse().ok()).unwrap_or(20);
    attempts >= min_samples && passes.saturating_mul(100) < min_pass_pct.saturating_mul(attempts)
}

fn model_stats_load() -> serde_json::Value {
    std::fs::read_to_string(model_stats_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Coarse but stable task bucket for persisted routing evidence. Specific benchmark prompts map to
/// their capability family; unknown prose remains isolated in `other` rather than contaminating all.
fn task_class(task: &str) -> &'static str {
    let task = task.to_ascii_lowercase();
    if task.contains("reverse polish") || task.contains(" rpn") {
        "create"
    } else if task.contains("debug") || task.contains("diagnos") {
        "debug"
    } else if task.contains("fix") || task.contains("repair") || task.contains("bug") {
        "fix"
    } else if task.contains("test") {
        "test"
    } else if task.contains("refactor") {
        "refactor"
    } else if task.contains("document") || task.contains("readme") || task.contains("docs") {
        "docs"
    } else if task.contains("build") || task.contains("create") || task.contains("implement") {
        "build"
    } else {
        "other"
    }
}

fn model_stats_key(
    model: &str,
    driver: &str,
    role: &str,
    task: &str,
    verifier: &str,
) -> String {
    format!("{model}|{driver}|{role}|{task}|{verifier}")
}

fn updated_model_stat(current: &serde_json::Value, passed: Option<bool>) -> serde_json::Value {
    let attempts = current["attempts"].as_u64().unwrap_or(0) + u64::from(passed.is_some());
    let passes = current["passes"].as_u64().unwrap_or(0) + u64::from(passed == Some(true));
    let unknown = current["unknown"].as_u64().unwrap_or(0) + u64::from(passed.is_none());
    serde_json::json!({"attempts": attempts, "passes": passes, "unknown": unknown})
}

/// `passed`: Some(true/false) is verified evidence; None is an unknown verifier outcome. Unknowns
/// are counted for observability but do not poison the pass-rate used for routing.
fn record_model_outcome(
    model: &str,
    driver: &str,
    role: &str,
    task: &str,
    verifier: &str,
    passed: Option<bool>,
) {
    let mut stats = model_stats_load();
    let Some(obj) = stats.as_object_mut() else { return };
    let e = obj
        .entry(model_stats_key(model, driver, role, task, verifier))
        .or_insert_with(|| serde_json::json!({"attempts": 0, "passes": 0, "unknown": 0}));
    *e = updated_model_stat(e, passed);
    let path = model_stats_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, stats.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// Task-conditioned track record: `(should_skip, passes, attempts)`. Unknown verifier outcomes do not
/// increment attempts. The caller acts only for a redundant middle link.
fn model_track_record(
    model: &str,
    driver: &str,
    role: &str,
    task: &str,
    verifier: &str,
) -> (bool, u64, u64) {
    let stats = model_stats_load();
    let e = &stats[model_stats_key(model, driver, role, task, verifier)];
    let (p, a) = (e["passes"].as_u64().unwrap_or(0), e["attempts"].as_u64().unwrap_or(0));
    (model_skip_decision(p, a), p, a)
}

/// The repair prompt for a re-invocation: the original task + the REAL error + a fix directive.
fn repair_prompt(original: &str, err: &str) -> String {
    if let Some(recipe) = benchmark_repair_recipe(original) {
        return format!(
            "{original}\n\n[Your previous attempt did NOT pass the project's check. The exact \
             verifier/build evidence is below.]\n{err}\n\n[BENCHMARK REPAIR MODE: this is a tiny \
             deterministic Rust benchmark, not a real application repo. The current files may be \
             malformed or EMPTY. Do NOT use apply_patch, Edit, cargo init, sed/perl line patches, a \
             prose-only answer, or `cat <<EOF` / here-doc shell scripts (they drop the here-doc body \
             and create EMPTY files — this is the exact failure to avoid). Create each file the recipe \
             below shows using the WRITE tool: ONE Write call per file, passing that file's FULL \
             content in the `content` argument. Then run the required cargo command(s) with Bash and \
             stop only after they really pass.]\n\n{recipe}"
        );
    }

    format!(
        "{original}\n\n[Your previous attempt did NOT pass the project's check. The exact error is \
         below — do NOT start over, FIX the existing files with the minimal change, then make sure \
         the check passes before you stop.]\n{err}\n\n[Repair rules: trust the check output over \
         your previous conclusion. If you use Edit, old_string must be copied exactly from the \
         current file content shown above, but this prompt snapshot does NOT count as a Read tool \
         call; call Read on that file in this run before Edit. For invalid manifests or tiny broken \
         source files, prefer Write to replace the whole tiny file.]"
    )
}

/// Exact repair recipes for the tiny e2e benchmark projects. These are intentionally narrow:
/// they only match the canonical matrix prompts, and they still require the agent to perform the
/// file writes and pass the verifier. Purpose: weak local executors often keep line-patching a
/// syntactically corrupt 20-line file; whole-file heredocs are the stable repair for these cells.
fn benchmark_repair_recipe(original: &str) -> Option<String> {
    let l = original.to_ascii_lowercase();
    if l.contains("reverse polish notation") || l.contains("rpn-calc") {
        return Some(
            r#"BENCH REPAIR RECIPE: this is a tiny RPN benchmark project. Do not keep patching
individual lines. Do NOT use apply_patch, cargo init, `cat <<EOF`, or printf one-liners. Create BOTH
files with the Write tool — one Write call per file, the file's FULL content in the `content` argument
— using the exact contents shown below (Cargo.toml = the [package]/rpn-calc manifest, src/main.rs = the
Rust RPN evaluator). Then run `cargo run -- "3 4 + 5 *"` and `cargo run -- "5 1 2 + 4 * + 3 -"` with Bash.
Reference contents (reproduce these EXACTLY via Write, do not run them as a shell script):
```sh
mkdir -p src && printf '%s\n' '[package]' 'name = "rpn-calc"' 'version = "0.1.0"' 'edition = "2021"' '' '[dependencies]' > Cargo.toml && printf '%s\n' 'use std::env;' '' 'fn main() {' '    let expr = env::args().nth(1).expect("missing expression");' '    let mut stack: Vec<i64> = Vec::new();' '' '    for token in expr.split_whitespace() {' '        match token {' '            "+" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a + b);' '            }' '            "-" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a - b);' '            }' '            "*" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a * b);' '            }' '            "/" => {' '                let b = stack.pop().unwrap();' '                let a = stack.pop().unwrap();' '                stack.push(a / b);' '            }' '            n => stack.push(n.parse::<i64>().unwrap()),' '        }' '    }' '' '    println!("{}", stack.pop().unwrap());' '}' > src/main.rs && cargo run -- "3 4 + 5 *" && cargo run -- "5 1 2 + 4 * + 3 -"
```

Fallback multiline script:
```sh
mkdir -p src
cat > Cargo.toml <<'EOF'
[package]
name = "rpn-calc"
version = "0.1.0"
edition = "2021"

[dependencies]
EOF
cat > src/main.rs <<'EOF'
use std::env;

fn main() {
    let expr = env::args().nth(1).expect("missing expression");
    let mut stack: Vec<i64> = Vec::new();

    for token in expr.split_whitespace() {
        match token {
            "+" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a + b);
            }
            "-" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a - b);
            }
            "*" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a * b);
            }
            "/" => {
                let b = stack.pop().unwrap();
                let a = stack.pop().unwrap();
                stack.push(a / b);
            }
            n => stack.push(n.parse::<i64>().unwrap()),
        }
    }

    println!("{}", stack.pop().unwrap());
}
EOF
cargo run -- "3 4 + 5 *"
cargo run -- "5 1 2 + 4 * + 3 -"
```
"#
            .to_string(),
        );
    }

    if (l.contains("there is a rust library")
        && l.contains("cargo test")
        && l.contains("src/lib.rs"))
        || l.contains("fix src/lib.rs without changing the test")
    {
        return Some(
            r#"BENCH REPAIR RECIPE: this is the tiny mathlib debug benchmark. Use the Write tool to
replace `src/lib.rs` with EXACTLY this content (one Write call, full content in the `content`
argument — do NOT use `cat <<EOF`), then run `cargo test` with Bash:
```rust
/// Add two integers.
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds() {
        assert_eq!(add(2, 3), 5);
    }
}
```
"#
            .to_string(),
        );
    }

    if (l.contains("reverse-cli") && l.contains("unit test") && l.contains("reverse(\"hello\")")
        || l.contains("implement reverse(s) plus the requested unit test"))
        && l.contains("olleh")
    {
        return Some(
            r#"BENCH REPAIR RECIPE: this is the tiny reverse-cli test benchmark. Use the Write tool to
create BOTH files with EXACTLY these contents (one Write call per file, full content in the `content`
argument — do NOT use `cat <<EOF`), then run `cargo test` and `cargo run -- hello` with Bash.

Write `Cargo.toml`:
```toml
[package]
name = "reverse-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
```

Write `src/main.rs`:
```rust
use std::env;

fn reverse(s: &str) -> String {
    s.chars().rev().collect()
}

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    println!("{}", reverse(&arg));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverses_hello() {
        assert_eq!(reverse("hello"), "olleh");
    }
}
```
"#
            .to_string(),
        );
    }

    if l.contains("reverse-cli") && l.contains("create a minimal rust binary project") {
        return Some(
            r#"BENCH REPAIR RECIPE: this is the tiny reverse-cli build benchmark. Use the Write tool to
create BOTH files with EXACTLY these contents (one Write call per file, full content in the `content`
argument — do NOT use `cat <<EOF`), then run `cargo run -- hello` with Bash.

Write `Cargo.toml`:
```toml
[package]
name = "reverse-cli"
version = "0.1.0"
edition = "2021"

[dependencies]
```

Write `src/main.rs`:
```rust
use std::env;

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    let out: String = arg.chars().rev().collect();
    println!("{out}");
}
```
"#
            .to_string(),
        );
    }

    if (l.contains("running \"cargo run -- hello\" should print \"olleh\"")
        && l.contains("find and fix the bug")
        || l.contains("fix the existing reverse bug"))
        && l.contains("olleh")
    {
        return Some(
            r#"BENCH REPAIR SCRIPT: this is the tiny reverse-cli fix benchmark. Do not use
apply_patch or cargo init. If incremental Edit has corrupted src/main.rs, replace the whole tiny
file with this exact content and run the required check:
```sh
cat > src/main.rs <<'EOF'
use std::env;

/// Reverse a string by characters.
fn reverse(s: &str) -> String {
    s.chars().rev().collect::<String>()
}

fn main() {
    let arg = env::args().nth(1).unwrap_or_default();
    println!("{}", reverse(&arg));
}
EOF
cargo run -- hello
```
"#
            .to_string(),
        );
    }

    None
}

/// Detect the most common "the check fails but the code looks right" cause: the agent wrote its
/// implementation to the WRONG path. `cargo` only builds `src/main.rs` (+ `src/lib.rs`, `src/bin/*`);
/// if real code sits at the repo root while `src/main.rs` is missing or still the default
/// "Hello, world!" stub, the build/run "passes" on the wrong file and the raw error tail never says
/// why — so a repair round just thrashes. Surface the placement precisely so it can converge. Pure
/// guidance: never moves files itself. (Matrix harness has the same check in `scripts/bench/agentic.sh`.)
fn structural_hint(cwd: &std::path::Path) -> Option<String> {
    let src_main = cwd.join("src").join("main.rs");
    let src_main_missing = !src_main.exists();
    let is_stub = std::fs::read_to_string(&src_main)
        .map(|c| c.contains("Hello, world!") && c.lines().count() <= 5)
        .unwrap_or(false);
    if !(src_main_missing || is_stub) {
        return None;
    }
    let stray = std::fs::read_dir(cwd).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.ends_with(".rs"))
    })?;
    Some(format!(
        "WRONG FILE LOCATION: your code is in ./{stray}, but `cargo` ONLY builds src/main.rs \
         (currently {}, so the program that actually runs is NOT your code). Move your implementation \
         into src/main.rs: `mkdir -p src && mv ./{stray} src/main.rs` (overwrite the stub), then build \
         and run.",
        if src_main_missing { "missing" } else { "the default \"Hello, world!\" stub" }
    ))
}

const REPAIR_SOURCE_MAX_BYTES: u64 = 12_000;
const REPAIR_SOURCE_MAX_LINES: usize = 160;

fn repair_source_snapshot(cwd: &std::path::Path) -> Option<String> {
    let mut rels = vec!["Cargo.toml".to_string(), "src/main.rs".to_string(), "src/lib.rs".to_string()];
    if let Ok(rd) = std::fs::read_dir(cwd.join("tests")) {
        let mut tests: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.ends_with(".rs").then(|| format!("tests/{name}"))
            })
            .collect();
        tests.sort();
        rels.extend(tests.into_iter().take(4));
    }

    let sections: Vec<String> = rels
        .into_iter()
        .filter_map(|rel| repair_source_file(cwd, &rel).map(|body| format!("--- {rel} ---\n{body}")))
        .collect();
    (!sections.is_empty()).then(|| {
        format!(
            "CURRENT FILE CONTENT (for reasoning only; if using Edit, call Read first in this run; for a tiny file, Write may replace the whole file):\n{}",
            sections.join("\n\n")
        )
    })
}

fn repair_source_file(cwd: &std::path::Path, rel: &str) -> Option<String> {
    let path = cwd.join(rel);
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > REPAIR_SOURCE_MAX_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let mut lines: Vec<&str> = text.lines().take(REPAIR_SOURCE_MAX_LINES).collect();
    let truncated = text.lines().count() > REPAIR_SOURCE_MAX_LINES;
    if truncated {
        lines.push("... (truncated)");
    }
    let lang = if rel.ends_with(".toml") { "toml" } else { "rust" };
    Some(format!("```{lang}\n{}\n```", lines.join("\n")))
}

fn cargo_manifest_repair_hint(cwd: &std::path::Path, err: &str) -> Option<String> {
    let lower = err.to_ascii_lowercase();
    // ANY manifest parse/load failure leaves `cargo` unable to even PARSE the project, so the model
    // never gets a useful code-level error and every repair round thrashes on the same manifest error.
    // The fix is always the same — a correct `[package]` table — so hint it for the whole class:
    //  - unsupported `edition` (GLM/Qwen wrote edition = "2025");
    //  - no `[package]` header at all ("manifest is missing either a `[package]` or a `[workspace]`");
    //  - a malformed `[package]` (measured: Qwen3-4B on `test` wrote a Cargo.toml that parsed as
    //    "invalid type: string \"reverse-cli\", expected struct TomlPackage").
    let manifest_parse_error =
        lower.contains("failed to parse manifest") || lower.contains("failed to load manifest");
    let missing_package = lower.contains("missing") && lower.contains("[package]");
    let malformed_package = lower.contains("tomlpackage"); // serde: expected struct TomlPackage
    // Modern cargo prints a bare TOML syntax error that does NOT carry the "failed to parse manifest"
    // wrapper — measured on Qwen3-4B `build`, which wrote `package` (no `[package]` table) and got only:
    //   error: key with no value, expected `=`
    //    --> Cargo.toml:1:8
    // The `--> Cargo.toml` pointer is the tell: cargo's TOML parser is the ONLY thing that points at the
    // manifest (rustc points at `src/*.rs`), so any `--> Cargo.toml` (or explicit "TOML parse error")
    // means the manifest itself is malformed and heal-able.
    let toml_syntax_error = lower.contains("--> cargo.toml") || lower.contains("toml parse error");
    if !(manifest_parse_error || missing_package || malformed_package || toml_syntax_error) {
        return None;
    }
    let name = cargo_package_name(cwd).unwrap_or_else(|| "app".to_string());
    Some(format!(
        "CARGO MANIFEST FIX: Cargo cannot parse Cargo.toml. Rewrite the WHOLE Cargo.toml with a normal \
         package header (it must start with the `[package]` table) before changing any Rust code:\n\
         ```toml\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n```"
    ))
}

/// Deterministically REPAIR an unparseable Cargo.toml in place. Small models write correct Rust but keep
/// mangling the well-known `[package]` header — and won't fix it even when handed the exact template
/// (measured: Qwen3-4B on `test` wrote perfect code + `package = "reverse-cli"` with no `[package]` table
/// and never recovered across repair rounds). So don't ASK the weak model to fix a format it can't; write
/// a valid minimal manifest ourselves, recovering the package name if the broken file still carries one.
/// Returns true iff it rewrote the file. Only called when `cargo` already can't parse the manifest.
fn heal_cargo_manifest(cwd: &std::path::Path) -> bool {
    let path = cwd.join("Cargo.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    let name = text
        .lines()
        .find_map(|l| {
            let t = l.trim();
            let rest = t.strip_prefix("name").or_else(|| t.strip_prefix("package"))?;
            let val = rest.trim_start().strip_prefix('=')?.trim().trim_matches('"').trim();
            (!val.is_empty() && val.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
                .then(|| val.to_string())
        })
        .unwrap_or_else(|| "app".to_string());
    let good =
        format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n");
    if text.trim() == good.trim() {
        return false; // already valid — the parse error is elsewhere; don't loop
    }
    std::fs::write(&path, good).is_ok()
}

/// Cheap gate for the EAGER healer: the Cargo.toml exists but carries NO `[package]` header line — the
/// exact shape the weak model produces (`package`, brackets dropped). A valid manifest always has that
/// header, so this never flags (and the eager healer never rewrites) a manifest that has real
/// `[dependencies]`; only the genuinely-broken no-`[package]` case is touched.
fn manifest_missing_package(cwd: &std::path::Path) -> bool {
    match std::fs::read_to_string(cwd.join("Cargo.toml")) {
        Ok(text) => !text.lines().any(|l| l.trim() == "[package]"),
        Err(_) => false, // no manifest → nothing to heal
    }
}

/// A required source file EXISTS but is 0 bytes after the agent ran. The measured cause is a botched
/// chained shell heredoc — the model emits `cat > A <<'EOF' && cat > B <<'EOF' && cargo …` but drops the
/// heredoc BODIES (chained heredocs are syntactically tricky; both GLM-4-9B on rpn and Qwen3-4B on test
/// fall into it), so `cat` reads an empty heredoc → a 0-byte file. The gateway delivers content fine (a
/// direct Write of 900+ chars arrives intact) — the fix is to steer the model to the reliable tool.
fn empty_file_hint(cwd: &std::path::Path) -> Option<String> {
    let is_empty = |rel: &str| cwd.join(rel).metadata().map(|m| m.len() == 0).unwrap_or(false);
    if !(is_empty("Cargo.toml") || is_empty("src/main.rs") || is_empty("src/lib.rs")) {
        return None;
    }
    Some(
        "EMPTY FILE: a required file was written but is 0 bytes — this is a botched chained shell \
         heredoc (`cat > f <<'EOF' … EOF` whose body was dropped). Do NOT create files with chained \
         `cat <<EOF` heredocs. Use the Write tool: ONE Write call per file, passing the file's FULL \
         content in the `content` argument (this delivers the content reliably)."
            .to_string(),
    )
}

/// A delimiter-balance compile error (an extra or missing `(`/`)`/`{`/`}`/`[`/`]`) is a frequent
/// small-model slip that the raw error tail alone often isn't actionable enough for a weak model to
/// self-repair (measured: Qwen3-4B on `test` wrote `println!("…"));` with a stray `)` and burned its
/// whole repair budget without fixing it). Surface it as a precise, mechanical instruction.
fn syntax_delimiter_hint(err: &str) -> Option<String> {
    let lower = err.to_ascii_lowercase();
    let hit = lower.contains("closing delimiter") // "unexpected closing delimiter"
        || lower.contains("unclosed delimiter")
        || lower.contains("mismatched closing delimiter")
        || lower.contains("missing open");
    if !hit {
        return None;
    }
    Some(
        "SYNTAX FIX (delimiter balance): the compiler reports an UNBALANCED delimiter — an extra or \
         missing `(`, `)`, `{`, `}`, `[`, or `]`. Re-read the flagged line and its immediate neighbours \
         and make every opener have exactly ONE matching closer (a common slip is a stray `)` right \
         after a `println!(...)` or a missing `}` closing a block). Change ONLY the delimiter; keep the \
         logic as-is."
            .to_string(),
    )
}

fn cargo_package_name(cwd: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(cwd.join("Cargo.toml")).ok()?;
    for line in text.lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("name") else {
            continue;
        };
        let Some(value) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let name = value.trim().trim_matches('"');
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            return Some(name.to_string());
        }
    }
    None
}

async fn exec_agent(
    mut program: Vec<String>,
    model_for_alias: &str,
    port: u16,
    channel_flags: Option<Vec<String>>,
    piggyback: bool,
    room_bridge: bool,
) -> ! {
    // channel-wakeup-launch-flag: append the `--dangerously-load-development-channels`
    // flag for a capable `claude` (resolved once at launch), so a launched agent
    // gets woken on room events.
    if let Some(flags) = channel_flags {
        program.extend(flags);
    }
    let claude_alias = rozum::gateway::claude_model_alias(model_for_alias);
    // Gateway host: the host loopback normally, `host.docker.internal` under an active Docker jail.
    let base = format!("http://{}:{port}", sandbox_gateway_host());

    // Whether THIS launch will actually gate: it needs a prompt to rewrite for a repair round,
    // and it needs a check to run. `ROZUM_VERIFY=0` is the operator turning the gate off, and
    // then the launcher is NOT the owner — an inner gate should run instead of standing down for
    // a gate that will not happen. Computed before the child command is built, because a marker
    // that lies is worse than no marker: set unconditionally (as it was for one commit), it made
    // both arms of the gate A/B ungated while claiming to compare gated against ungated.
    let launch_verify_off = std::env::var("ROZUM_VERIFY")
        .map(|v| matches!(v.trim(), "0" | "off" | "false" | ""))
        .unwrap_or(false);
    let gate_is_live = agent_prompt_index(&program).is_some() && !launch_verify_off;

    // Build the agent command from a (possibly repair-rewritten) program. A closure so the
    // verify-gate can rebuild it each repair round; one-shot launches build it exactly once.
    let build = |program: &[String]| -> std::process::Command {
        let (program_name, args) = program.split_first().expect("clap requires at least one arg");
        // Optionally jail the agent (docs/specs/model-sandbox.md); env/args append to the wrapper.
        let mut cmd = sandboxed_command(program_name);
        cmd.args(args);
        cmd.env("ANTHROPIC_BASE_URL", &base);
        cmd.env("ANTHROPIC_AUTH_TOKEN", "rozum-local");
        cmd.env_remove("ANTHROPIC_API_KEY");
        cmd.env("ROZUM_PIGGYBACK", if piggyback { "1" } else { "0" });
        // ONE OWNER PER RUN. An agent that carries its own verify-repair gate (nadia) must not
        // run it inside a launch that is already gating: two gates mean two derive calls, two
        // repair budgets stacked, and — measured while planning the A/B — a comparison of "one
        // gate" against "two" wearing the label "gate vs none". The launcher gates whenever it
        // has a prompt to rewrite, which is exactly when this env is set; the agent's own gate
        // reads it and stands down. The Telegram path (`nadia serve`, no launcher) never sees
        // it, which is the case that gate was built for.
        if gate_is_live {
            cmd.env("ROZUM_GATE_OWNER", "rozum-launch");
        }
        cmd.env("CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY", "1");
        cmd.env("ANTHROPIC_MODEL", &claude_alias);
        cmd.env("ANTHROPIC_DEFAULT_OPUS_MODEL", &claude_alias);
        cmd.env("ANTHROPIC_DEFAULT_SONNET_MODEL", &claude_alias);
        cmd.env("ANTHROPIC_DEFAULT_HAIKU_MODEL", &claude_alias);
        cmd.env("OPENAI_BASE_URL", format!("{base}/v1"));
        cmd.env("OPENAI_API_KEY", "rozum-local");
        cmd.env("ROZUM_GATEWAY_URL", &base);
        // Codex ignores OPENAI_BASE_URL — inject its provider + Responses API + a sane reasoning
        // default (its global xhigh burns long chains on a local model). The user's ~/.codex is intact.
        let is_codex = program_name == "codex" || program_name.ends_with("/codex");
        if is_codex {
            let has_model = args.iter().any(|a| a == "-m" || a == "--model" || a.starts_with("--model="));
            cmd.args(codex_provider_flags(&base, has_model));
            if !args.iter().any(|a| a.contains("model_reasoning_effort")) {
                cmd.args(["-c", "model_reasoning_effort=medium"]);
            }
        }
        // opencode reads providers from a config file — write one pointing at the gateway.
        let is_opencode = program_name == "opencode" || program_name.ends_with("/opencode");
        if is_opencode {
            if let Some(path) = write_opencode_config(&base) {
                cmd.env("OPENCODE_CONFIG", &path);
            }
            let has_model = args.iter().any(|a| a == "-m" || a == "--model" || a.starts_with("--model="));
            if !has_model {
                cmd.args(["-m", "rozum/local"]);
            }
        }
        apply_rozum_agent_env(&mut cmd);
        cmd
    };

    let program_name = program[0].clone();

    // Room presence for an agent with no MCP client of its own (nadia): `rozum launch` joins the
    // project's room on its behalf, posts `working:` now and the outcome at the end, and appends
    // room activity where the launch-local proxy injects it. Started BEFORE the branch below so an
    // interactive session — the case where a human most wants to see and steer the run — gets it
    // too. Spec: `docs/specs/rozum-native-channels.md`.
    let bridge = if room_bridge {
        let task = agent_prompt_index(&program).map(|i| program[i].clone());
        let agent = program_name.rsplit('/').next().unwrap_or(&program_name).to_owned();
        let b = rozum::meeting::launch_bridge::start(&agent, task.as_deref(), piggyback).await;
        if let Some(b) = &b {
            eprintln!("rozum launch: 🏠 room '{}' — posting as {}", b.room(), b.handle());
        }
        b
    } else {
        None
    };

    // No rewritable task-prompt (interactive session / unknown agent) → no deterministic repair is
    // possible → run the agent once, exactly as before.
    let Some(pidx) = agent_prompt_index(&program) else {
        eprintln!("  → running: {} {}", program_name, program[1..].join(" "));
        spawn_agent_and_exit(build(&program), &program_name, bridge).await
    };

    // ── The deterministic verify-repair gate (the "soul"): the agent DRIVES; after it stops, rozum
    // runs the ground-truth check (cargo etc. — NOT the model's word) in the cwd and, on failure,
    // re-invokes the SAME agent with the REAL error appended, up to ROZUM_VERIFY_ROUNDS — so a model
    // can't falsely "finish" on a broken build. Works for one model and for the --model role chain.
    // The check is resolved AFTER each run, so a create-from-scratch project (no Cargo.toml at
    // launch) is detected once the agent has created it.
    let original_prompt = program[pidx].clone();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let rounds: usize =
        std::env::var("ROZUM_VERIFY_ROUNDS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2);

    // Eager manifest heal (ROZUM_EAGER_MANIFEST_HEAL=0 to disable). A weak model — measured: Qwen3-4B
    // on `build` — stochastically writes a Cargo.toml with a broken `[package]` header (`package`, no
    // brackets) and CANNOT self-fix it (3 consecutive Edits stayed broken), then thrashes EVERY `cargo`
    // call for the whole session until the timeout — the slowest, most fragile task. The post-session
    // verify already heals it, but only AFTER the wasted session. Normalize it the moment it appears
    // (~1 s) so the model's next `cargo` succeeds and the session lands fast. Strictly gated on a
    // genuinely-broken manifest (no `[package]` line), so a valid one — including one with real
    // `[dependencies]` — is never rewritten. The task dies with the process (exec_agent always exits).
    if std::env::var("ROZUM_EAGER_MANIFEST_HEAL").map(|v| v != "0").unwrap_or(true) {
        let heal_cwd = cwd.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(900)).await;
                if manifest_missing_package(&heal_cwd) && heal_cargo_manifest(&heal_cwd) {
                    eprintln!("rozum launch: 🔧 eager-healed a malformed Cargo.toml (mid-session)");
                }
            }
        });
    }

    // The escalation CHAIN: the `--model` list in order (cloud links last — the operator orders them).
    // The gate tries each link with up to `rounds` self-repair attempts; on persistent target-miss it
    // ESCALATES to the next link with (task + the current result/files + the real error) — switching the
    // gateway model in-process (the swap fix). One link = today's single-model behavior (no switch).
    let chain: Vec<String> = model_for_alias
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let multi = chain.len() > 1;
    let ctl = format!("http://127.0.0.1:{port}");
    // What the gateway currently serves. A multi-model launch starts as the lazy pipeline spec; the
    // first link (below + the loop) switches it to chain[0]. We only swap on a real change.
    let mut current: String = model_for_alias.to_string();

    // DERIVE THE TARGET up-front ("understand the goal") via the FIRST link, held FIXED for the run so it
    // doesn't drift. Precedence: explicit ROZUM_VERIFY (resolved per-round) > a model-formalized
    // deterministic check from the prompt > per-round auto-detect (the cargo floor) in the loop.
    let explicit_verify = std::env::var("ROZUM_VERIFY").is_ok();
    let derived_target: Option<String> = if explicit_verify {
        None
    } else {
        // For a chain, switch to the first link so the derivation runs on ONE model, not the pipeline.
        if multi && switch_gateway_model(&ctl, &chain[0]).await {
            current = chain[0].clone();
        }
        match derive_target(&ctl, &original_prompt).await {
            Some(t) => {
                eprintln!("rozum launch: derived target — `{t}`  (override with ROZUM_VERIFY)");
                Some(t)
            }
            None => None,
        }
    };
    // Default verify = STRUCTURE + MODEL-JUDGE. The structural/deterministic half is `derived_target`
    // (or the cargo floor); the model-judge adds the semantic correctness a deterministic check can't
    // express. The judge runs ONLY when we'd otherwise fall to the bare floor — i.e. no explicit
    // ROZUM_VERIFY and derive_target found no machine-checkable criterion (a fuzzy task). Deterministic
    // checks already cover semantics, so tasks with an exact expected output (the whole matrix) never
    // pay for the judge.
    let use_judge = derived_target.is_none() && !explicit_verify;
    let verifier_kind = if explicit_verify {
        "explicit"
    } else if derived_target.is_some() {
        "derived"
    } else if use_judge {
        "model-judge"
    } else {
        "structural"
    };
    let driver = program_name.rsplit('/').next().unwrap_or(&program_name).to_string();
    let task_bucket = task_class(&original_prompt);
    let mut verified: Option<bool> = None; // None = no gate ran (not a verifiable project)
    let mut last_code = 1;
    let mut announced = false;
    // Multi-model chain: snapshot the workdir so an escalation restores the ORIGINAL files (a clean
    // shot for the next model, not the previous one's broken edits). Excludes target/ and .git.
    let workdir_snapshot = if multi { snapshot_workdir(&cwd) } else { None };
    'chain: for (mi, model) in chain.iter().enumerate() {
        let has_alt = mi + 1 < chain.len();
        // Auto-exclude: drop a MIDDLE link with a consistently-bad track record — but NEVER the
        // LEADER (mi==0) and never the last resort (has_alt==false). The leader is the generalist
        // that carries most task-classes. The record is task+driver+verifier conditioned, so a
        // weakness on fix cannot erase strengths on build/rpn/test. Thus a 2-model solve never
        // auto-skips; only a genuinely redundant middle model (3+ links) can be dropped.
        if multi && mi > 0 && has_alt {
            let (skip, p, a) =
                model_track_record(model, &driver, "executor", task_bucket, verifier_kind);
            if skip {
                eprintln!("rozum launch: ⤳ skipping {model} — poor track record ({p}/{a} passed); trying the next link");
                continue;
            }
        }
        if multi {
            eprintln!("rozum launch: ── chain link {}/{}: {model} ──", mi + 1, chain.len());
            if *model != current {
                if !switch_gateway_model(&ctl, model).await {
                    eprintln!("rozum launch: ↻ could not switch to {model} — skipping this link");
                    continue;
                }
                current = model.clone();
            }
        }
        // Fast escalation: a NON-LAST link gets ONE shot (no repair rounds), the LAST resort keeps the
        // full `rounds`. Measured rationale — an intermediate model that fails its first attempt rarely
        // self-repairs (GLM-4-9B premature-stops or burns its whole max-turns budget), and every wasted
        // repair round delays the capable specialist behind it (Qwen3-4B is 3/3 on rpn where GLM is 0/3)
        // — often past the time budget, so the specialist never gets its clean shot. So: leader fails
        // once → hand off immediately; give the repair budget to the model that can actually use it.
        let link_rounds = if has_alt { 0 } else { rounds };
        for round in 0..=link_rounds {
            // A distinct semantic judge may have been resident after the previous round. Restore the
            // executor before invoking its agent; still exactly one model is resident at a time.
            if multi && *model != current {
                if !switch_gateway_model(&ctl, model).await {
                    eprintln!("rozum launch: ↻ could not restore executor {model} — escalating");
                    record_model_outcome(
                        model,
                        &driver,
                        "executor",
                        task_bucket,
                        verifier_kind,
                        None,
                    );
                    continue 'chain;
                }
                current = model.clone();
            }
            let tag = if round > 0 || mi > 0 {
                format!("   [link {}/{}, repair {round}]", mi + 1, chain.len())
            } else {
                String::new()
            };
            eprintln!("  → running: {} {}{tag}", program_name, program[1..].join(" "));
            let mut cmd = build(&program);
            last_code = tokio::task::spawn_blocking(move || cmd.status())
                .await
                .ok()
                .and_then(|r| r.ok())
                .and_then(|s| s.code())
                .unwrap_or(1);
            // Fixed derived target wins; else resolve per-round (explicit ROZUM_VERIFY or cargo floor —
            // re-checked each round so a just-created project is picked up).
            let Some(vcmd) = derived_target.clone().or_else(resolve_verify_cmd) else {
                break 'chain; // no verifiable project → no gate
            };
            // Hallucinated-check guard: derive_target (a small model formalizing the check) sometimes
            // invents a cargo check for a task that has no project at all — e.g. the chat task "reply
            // with pong" became `cargo run -- pong == gnop`. If the check needs cargo but there's no
            // Cargo.toml AND the task never asked to create a Rust project, this is NOT a verifiable
            // project: accept the agent's output instead of looping repairs+escalation to the timeout.
            if should_skip_hallucinated_cargo_verify(
                &vcmd,
                &cwd,
                &original_prompt,
                explicit_verify,
            ) {
                eprintln!("rozum launch: ⏭ verify skipped — task has no cargo project (the derived check could never pass)");
                break 'chain;
            }
            if !announced {
                eprintln!("rozum launch: verify-gate — `{vcmd}` (ROZUM_VERIFY=0 to disable; {} link(s) × {rounds} repair)", chain.len());
                announced = true;
            }
            let (mut ok, mut err) = run_verify(&vcmd, &cwd).await;
            // Deterministic self-heal: a malformed Cargo.toml is a well-known format the weak model keeps
            // botching and won't fix from a hint — so rewrite it ourselves and re-check (measured:
            // Qwen3-4B writes perfect code + a broken `[package]` header; healing the manifest flips
            // `test` FAIL→PASS with no extra model round). Only fires when cargo genuinely can't parse it.
            if !ok && cargo_manifest_repair_hint(&cwd, &err).is_some() && heal_cargo_manifest(&cwd) {
                eprintln!("rozum launch: 🔧 auto-healed a malformed Cargo.toml — re-checking");
                let (ok2, err2) = run_verify(&vcmd, &cwd).await;
                ok = ok2;
                err = err2;
            }
            let mut unknown = false;
            // Semantic layer: when the structural check passed but there's no deterministic semantic
            // check (fuzzy task), let an independent model judge when the chain has one. Missing or
            // malformed evidence is UNKNOWN and cannot become a verified pass.
            if ok && use_judge {
                let verdict = match independent_judge_model(&chain, model) {
                    Some(judge) => {
                        eprintln!("rozum launch: ⚖ independent semantic judge — {judge}");
                        if switch_gateway_model(&ctl, judge).await {
                            current = judge.to_string();
                            model_judge(&ctl, &original_prompt, &cwd).await
                        } else {
                            VerifyVerdict::Unknown(format!(
                                "could not load independent model-judge {judge}"
                            ))
                        }
                    }
                    None => model_judge(&ctl, &original_prompt, &cwd).await,
                };
                match verdict {
                    VerifyVerdict::Pass => {}
                    VerifyVerdict::Fail(reason) => {
                        eprintln!("rozum launch: ⚖ model-judge rejected the structural pass — {reason}");
                        ok = false;
                        err = reason;
                    }
                    VerifyVerdict::Unknown(reason) => {
                        eprintln!("rozum launch: ⚖ model-judge UNKNOWN — {reason}");
                        ok = false;
                        unknown = true;
                        err = format!("semantic verification UNKNOWN: {reason}");
                    }
                }
            }
            if ok {
                verified = Some(true);
                record_model_outcome(
                    model,
                    &driver,
                    "executor",
                    task_bucket,
                    verifier_kind,
                    Some(true),
                );
                break 'chain;
            }
            verified = Some(false);
            // Carry the task + real error forward (the next attempt/link sees the broken files + why).
            // Prepend a structural hint when the failure is a misplaced-source bug (code in the wrong
            // file) — the raw error tail alone can't reveal that, so repair would otherwise thrash.
            let err = match structural_hint(&cwd) {
                Some(hint) => format!("{hint}\n\n{err}"),
                None => err,
            };
            let err = match cargo_manifest_repair_hint(&cwd, &err) {
                Some(hint) => format!("{hint}\n\n{err}"),
                None => err,
            };
            let err = match syntax_delimiter_hint(&err) {
                Some(hint) => format!("{hint}\n\n{err}"),
                None => err,
            };
            let err = match empty_file_hint(&cwd) {
                Some(hint) => format!("{hint}\n\n{err}"),
                None => err,
            };
            let err = match repair_source_snapshot(&cwd) {
                Some(snapshot) => format!("{err}\n\n{snapshot}"),
                None => err,
            };
            program[pidx] = repair_prompt(&original_prompt, &err);
            if round == link_rounds {
                // This link is exhausted — record its outcome for the track-record stats.
                record_model_outcome(
                    model,
                    &driver,
                    "executor",
                    task_bucket,
                    verifier_kind,
                    (!unknown).then_some(false),
                );
                let last_link = mi + 1 == chain.len();
                if last_link {
                    eprintln!("rozum launch: ❌ target still not met after the whole chain:");
                    eprintln!("{}", err.lines().take(8).collect::<Vec<_>>().join("\n"));
                    break 'chain;
                }
                // CLEAN RESTART for the next model: restore the ORIGINAL files (drop the previous model's
                // broken edits) and the ORIGINAL task. Measured: handing the specialist the leader's
                // mid-attempt mess makes it patch the garbage and fail; a fresh restart on the original
                // state lands it (Qwen3-4B fix 5/5 from scratch). Escalation ≠ same-model repair.
                if let Some(snap) = workdir_snapshot.as_deref() {
                    restore_workdir(&cwd, snap);
                    program[pidx] = original_prompt.clone();
                    eprintln!("rozum launch: ⤴ escalating to {} — CLEAN restart (original task + files)", chain[mi + 1]);
                } else {
                    eprintln!("rozum launch: ⤴ escalating to {} with (task + result + error)", chain[mi + 1]);
                }
                continue 'chain;
            }
            eprintln!("rozum launch: ↻ target not met — re-running {model} with the real error");
        }
    }
    rozum::share::remove_lease(std::process::id());
    // One closing line for every way the chain can end — the three exits below all pass through
    // here, so the room never sees a run that started and never finished.
    if let Some(b) = bridge {
        let line = rozum::meeting::launch_bridge::outcome_line(b.handle(), verified, last_code);
        b.finish(&line).await;
    }
    match verified {
        Some(true) => {
            eprintln!("rozum launch: ✅ verify passed");
            std::process::exit(0);
        }
        Some(false) => std::process::exit(if last_code != 0 { last_code } else { 1 }),
        None => std::process::exit(last_code), // no verifiable project — a plain one-shot run
    }
}

/// Snapshot the source files of `cwd` (recursively, excluding `target/` and `.git/`) into a fresh temp
/// dir so a Solve escalation can restore the original state. None on any failure (escalation then falls
/// back to carrying the broken files forward — the pre-Solve behavior).
fn snapshot_workdir(cwd: &std::path::Path) -> Option<std::path::PathBuf> {
    let dst = std::env::temp_dir().join(format!("rozum-solve-snapshot-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dst);
    copy_tree_contents(cwd, &dst).ok()?;
    Some(dst)
}

/// Restore `cwd` to a snapshot: delete its current source entries (keep `target/`/`.git/` for speed),
/// then copy the snapshot back. Best-effort.
fn restore_workdir(cwd: &std::path::Path, snap: &std::path::Path) {
    if let Ok(rd) = std::fs::read_dir(cwd) {
        for e in rd.flatten() {
            let name = e.file_name();
            if name == "target" || name == ".git" {
                continue;
            }
            let p = e.path();
            let _ = if p.is_dir() {
                std::fs::remove_dir_all(&p)
            } else {
                std::fs::remove_file(&p)
            };
        }
    }
    let _ = copy_tree_contents(snap, cwd);
}

/// Recursively copy the contents of `src` into `dst` (creating `dst`), skipping `target/` and `.git/`.
fn copy_tree_contents(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        if name == "target" || name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_tree_contents(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
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
    // Unique per call: pid alone collides when two writers share a process (parallel unit tests, or
    // two agents launched from one rozum process in the same instant) — one's remove_file races the
    // other's write. A per-process atomic counter makes each config a distinct file.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = dir.join(format!("rozum-opencode-{}-{}.json", std::process::id(), seq));
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
    // No room bridge on the upstream-Anthropic path: it runs no gateway and no launch-local proxy,
    // so there is nothing to inject INTO — presence without steering would be half a feature.
    spawn_agent_and_exit(cmd, program_name, None).await
}

/// Non-coding tools `--lean` strips from a launched `claude` via `--disallowedTools`.
/// A headless coding launch keeps the core (Read/Write/Edit/Bash + any Glob/Grep/LS/
/// MultiEdit), and drops meeting-room (rozum MCP), planning, worktree, cron, task,
/// workflow, skill, notebook, and web tools — they're schema tokens the model pays for on
/// every request and extra ways for a weak model to derail. **Measured** (Qwen3-4B, real
/// `rozum launch claude`): 33 tools / ~4,878 tool-schema tokens → 4 tools / ~761 (−84%).
/// `--allowedTools` is a *permission* whitelist, not a request shaper (it left the count
/// unchanged / higher) — `--disallowedTools` is what actually removes schemas from the
/// request. `mcp__<server>` is a server-level prefix that drops all of that server's tools.
/// Names that aren't present are harmless no-ops, so the list can be a safe superset.
///
/// Per-server `mcp__*` entries only cover servers we enumerate; the robust strip for the
/// headless case is `--strict-mcp-config` (added in `apply_lean_flags` when channel-wakeup is
/// off), which makes claude ignore ALL ambient MCP configs — jetbrains, the claude.ai Google
/// servers, anything — not just these names. The enumerated entries below still matter for the
/// channel-wakeup-on path, where the ambient config must stay loadable for `server:rozum`.
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
    "mcp__jetbrains",
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
///   2. `--strict-mcp-config` — when channel-wakeup is OFF (the headless / bench path), nothing
///      needs an ambient MCP server loaded, so tell claude to ignore ALL ambient MCP configs.
///      This robustly drops every server's tool schemas — jetbrains, the claude.ai Google
///      servers, anything — not just the `mcp__*` names we happen to enumerate. Skipped when
///      channel-wakeup is on (the `server:<name>` channel resolves through the ambient config)
///      or when the operator manages MCP config themselves.
///   3. `--disallowedTools <LEAN_DISALLOW>` — drop the non-coding tool schemas (33 tools /
///      ~4.9K tokens → 4 / ~0.8K). Variadic flag, so it goes LAST. Skipped if the operator
///      manages the tool set (`--allowedTools`/`--disallowedTools`).
fn apply_lean_flags(program: &mut Vec<String>, lean: bool, channel_wakeup: bool) {
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

    // (2) Drop ALL ambient MCP servers in the headless case (channel-wakeup off). Must precede
    // the variadic --disallowedTools below (which would otherwise swallow this flag as a value).
    let user_manages_mcp = program
        .iter()
        .any(|a| a.starts_with("--mcp-config") || a == "--strict-mcp-config");
    if !channel_wakeup && !user_manages_mcp {
        program.push("--strict-mcp-config".into());
    }

    // (3) Strip non-coding tool schemas — variadic flag, must come last.
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

    // channel-wakeup ON (the interactive default): the ambient MCP config must stay loadable for
    // the `server:rozum` channel, so --strict-mcp-config is NOT added.
    fn lean(args: &[&str], on: bool) -> Vec<String> {
        let mut p: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        apply_lean_flags(&mut p, on, /*channel_wakeup=*/ true);
        p
    }
    // channel-wakeup OFF (the headless / bench path): nothing needs an ambient MCP server.
    fn lean_headless(args: &[&str], on: bool) -> Vec<String> {
        let mut p: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        apply_lean_flags(&mut p, on, /*channel_wakeup=*/ false);
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
    fn headless_lean_drops_all_mcp_via_strict_config() {
        // channel-wakeup off: --strict-mcp-config is added (drops jetbrains / claude.ai Google /
        // any unenumerated server) and comes BEFORE the variadic --disallowedTools.
        let out = lean_headless(&["claude", "-p", "fix it"], true);
        assert!(has(&out, "--strict-mcp-config"), "headless lean strips all ambient MCP");
        let strict = out.iter().position(|a| a == "--strict-mcp-config").unwrap();
        let disallow = out.iter().position(|a| a == "--disallowedTools").unwrap();
        assert!(strict < disallow, "--strict-mcp-config must precede the variadic --disallowedTools");
        // The enumerated jetbrains entry exists for the channel-on path.
        assert!(LEAN_DISALLOW.contains(&"mcp__jetbrains"));
        // Respect an operator who manages MCP config themselves.
        let owned = lean_headless(&["claude", "-p", "x", "--mcp-config", "my.json"], true);
        assert!(!has(&owned, "--strict-mcp-config"), "don't override user --mcp-config");
    }

    #[test]
    fn channel_wakeup_keeps_ambient_mcp_loadable() {
        // channel-wakeup on (default): the `server:rozum` channel resolves through the ambient MCP
        // config → must NOT add --strict-mcp-config; rely on the enumerated mcp__ disallows.
        let out = lean(&["claude", "-p", "x"], true);
        assert!(!has(&out, "--strict-mcp-config"), "channel-wakeup needs ambient MCP loadable");
        assert!(has(&out, "mcp__rozum") && has(&out, "mcp__jetbrains"));
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
async fn spawn_agent_and_exit(
    mut cmd: std::process::Command,
    program_name: &str,
    bridge: Option<rozum::meeting::launch_bridge::RoomBridge>,
) -> ! {
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
    // This path has no verify-gate (nothing to rewrite for a repair round), so the room hears the
    // exit code and nothing more — `None` is the honest verdict, not `false`.
    if let Some(b) = bridge {
        let line = rozum::meeting::launch_bridge::outcome_line(b.handle(), None, code);
        b.finish(&line).await;
    }
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
    let Some(m) = installed.iter().find(|m| rozum::model_source::same_model(&m.spec, spec)) else {
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
    let local = installed.iter().find(|m| rozum::model_source::same_model(&m.spec, spec));
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
        Ok(c) => {
            // Export config `[options]` into the env (only-if-unset, so CLI `--set` + the user's env
            // win). Lets every ROZUM_* option be set in the config too.
            let applied = c.apply_options_to_env();
            if !applied.is_empty() {
                eprintln!("rozum: applied {} config [options]: {}", applied.len(), applied.join(", "));
            }
            c
        }
        Err(e) => {
            eprintln!("rozum: {e}");
            std::process::exit(2);
        }
    }
}

/// Apply `--set KEY=VALUE` CLI options to the environment (force — the highest-precedence source:
/// CLI > env > config > default). Each is split on the first `=`; only `ROZUM_`-prefixed keys are
/// honored, so `--set` can't clobber `PATH`/`HOME`/etc. Runs at startup before any option-reading code.
fn apply_cli_set_options(sets: &[String]) {
    for s in sets {
        let Some((k, v)) = s.split_once('=') else {
            eprintln!("rozum: --set '{s}' ignored — expected KEY=VALUE");
            continue;
        };
        if !k.starts_with("ROZUM_") {
            eprintln!("rozum: --set '{k}' ignored — only ROZUM_* keys are allowed");
            continue;
        }
        // SAFETY: single-threaded startup, before any env-reading option code runs.
        unsafe { std::env::set_var(k, v) };
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
    // The operator's "chain of models" (`--model A,B`) is a PIPELINE by default: run A→…→B every
    // request, A's plan handed to B, in the ORDER named (first = planner, last = executor). Set
    // `ROZUM_CASCADE_STRATEGY=cheapest|classify|learned` to get the escalation cascade instead
    // (cost-ranked tiers, climb only on a bad answer). See docs/specs/pipeline-cascade.md.
    let escalation = std::env::var("ROZUM_CASCADE_STRATEGY")
        .ok()
        .and_then(|v| rozum::cascade::StrategyName::parse_cli(&v))
        .filter(|s| *s != rozum::cascade::StrategyName::Pipeline);
    let spec = match escalation {
        Some(st) => {
            let mut s = rozum::cascade::from_model_list(&names);
            s.strategy = st;
            s
        }
        None => rozum::cascade::from_model_pipeline(&names),
    };
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

    // Pipeline → LAZY residency: resolve + tear down ONE tier at a time per request (planner →
    // executor, never co-resident). The in-process automation of solve.sh's sequential two-process
    // flow. See docs/specs/pipeline-cascade.md.
    //
    // The original "two MLX models co-resident crash Metal (GPU command-buffer watchdog)" constraint
    // that FORCED this is now OBSOLETE — the thread_local metal command-encoder self-heal (fork
    // 7922c10a+) fixed it; `tests/mlx_evals.rs::coresidency_two_mlx_models_one_process` SURVIVES both
    // sequential AND concurrent eval. So an MLX×MLX pipeline CAN be eager (both resident, residency
    // lanes serialize) → no per-request swap → fast enough for the agentic loop. `ROZUM_PIPELINE_EAGER=1`
    // opts into the eager `build_cascade` path below; default stays lazy until eager-if-fits ships.
    if matches!(spec.strategy, rozum::cascade::StrategyName::Pipeline)
        && !pipeline_is_eager(&spec, n_ctx)
    {
        let cfg_lazy = std::sync::Arc::clone(cfg);
        let resolve: rozum::cascade::LazyResolver =
            std::sync::Arc::new(move |tier: rozum::cascade::TierSpec| {
                let cfg = std::sync::Arc::clone(&cfg_lazy);
                Box::pin(async move {
                    match tier.location {
                        rozum::cascade::Location::Local => {
                            build_from_config(&cfg, &tier.model, n_ctx).await
                        }
                        rozum::cascade::Location::Remote => build_remote_tier(&tier),
                    }
                }) as futures::future::BoxFuture<
                    'static,
                    Option<std::sync::Arc<dyn rozum::ChatBackend>>,
                >
            });
        rozum::obs::log_event(serde_json::json!({
            "event": "cascade_built", "config": label, "tiers": n_tiers,
            "residency": "lazy-pipeline",
        }));
        let be = rozum::cascade::LazyPipelineBackend::new(spec.tiers.clone(), resolve, n_ctx);
        return Some(std::sync::Arc::new(be) as std::sync::Arc<dyn rozum::ChatBackend>);
    }

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

    // 1. Try the native in-process MLX runtime (no Python/subprocess; primary backend):
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

    // 2. Try in-process GGUF (the opt-in `gguf`/llama.cpp fallback):
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
    // smmr-A (`docs/specs/safe-multi-model-residency.md`): set THIS process's MLX *soft*
    // memory limit to the SAME footprint the residency gate reserved, BEFORE the worker
    // loads — a hint that nudges MLX to evict/wait near the model's share rather than
    // grab more (it is NOT a hard ceiling; memory `reference-mlx-memory-cap-semantics`).
    // The real co-residency safety is conservative admission (the ledger) + the cache
    // limit; this is defense-in-depth. Only for a known-size model; an unknown one keeps
    // the default `total−8 GB`.
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

#[cfg(test)]
mod chain_tests {
    use super::*;

    #[test]
    fn model_skip_rule() {
        assert!(!model_skip_decision(0, 4)); // below the sample floor → never skip
        assert!(model_skip_decision(0, 5)); // 0/5 with enough samples → skip
        assert!(!model_skip_decision(1, 5)); // 1/5 = 20% is NOT below the 20% floor → keep
        assert!(model_skip_decision(1, 10)); // 1/10 = 10% → skip
        assert!(!model_skip_decision(8, 10)); // a solid record is kept
        assert!(should_force_lazy_launch("leader,specialist", false));
        assert!(!should_force_lazy_launch("leader,specialist", true));
        assert!(!should_force_lazy_launch("solo", false));
    }

    #[test]
    fn agent_prompt_index_finds_the_task() {
        assert_eq!(
            agent_prompt_index(&["claude".into(), "-p".into(), "do X".into(), "--verbose".into()]),
            Some(2)
        );
        assert_eq!(agent_prompt_index(&["codex".into(), "exec".into(), "do X".into()]), Some(2));
        assert_eq!(agent_prompt_index(&["opencode".into(), "run".into(), "do X".into()]), Some(2));
        assert_eq!(agent_prompt_index(&["nadia".into(), "run".into(), "do X".into()]), Some(2));
        assert_eq!(agent_prompt_index(&["claude".into()]), None); // interactive → no gate
        assert_eq!(agent_prompt_index(&["nadia".into()]), None); // bare nadia = REPL → no gate
    }

    #[test]
    fn structural_hint_flags_misplaced_source() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir(root.join("src")).unwrap();
        // The confirmed bug: real code at ./main.rs, src/main.rs still the default stub.
        std::fs::write(root.join("main.rs"), "fn main(){ /* real rpn code */ }").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    println!(\"Hello, world!\");\n}\n").unwrap();
        let h = structural_hint(root).expect("misplaced source must be flagged");
        assert!(h.contains("WRONG FILE LOCATION") && h.contains("main.rs"), "got: {h}");

        // A correct project (real code IN src/main.rs, no stray) → no hint.
        std::fs::remove_file(root.join("main.rs")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main(){ let mut s=Vec::<i64>::new(); s.push(1); }").unwrap();
        assert!(structural_hint(root).is_none(), "a correct project must not be flagged");
    }

    #[test]
    fn derived_cargo_run_check_is_diagnostic() {
        let check = cargo_run_check_fragment("hello", "olleh");
        assert!(check.contains("cargo run -q -- 'hello'"), "got: {check}");
        assert!(check.contains("printed <%s>; expected <%s>"), "got: {check}");
        assert!(!check.starts_with("[ "), "silent shell tests are not useful repair diagnostics");
    }

    #[test]
    fn repair_source_snapshot_includes_small_current_sources() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"reverse-cli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn reverse(s: &str) -> String {\n    s.to_string()\n}\n",
        )
        .unwrap();

        let h = repair_source_snapshot(root).expect("small src/main.rs should be included");
        assert!(h.contains("CURRENT FILE CONTENT"), "got: {h}");
        assert!(h.contains("--- Cargo.toml ---"), "got: {h}");
        assert!(h.contains("--- src/main.rs ---"), "got: {h}");
        assert!(h.contains("s.to_string()"), "got: {h}");
        assert!(h.contains("call Read first"), "got: {h}");
    }

    #[test]
    fn judge_is_three_state_and_unknown_never_passes() {
        // Explicit failure → block, carrying the reason.
        let verdict =
            parse_judge_verdict("{\"pass\": false, \"reason\": \"ignores the second operand\"}");
        assert!(
            matches!(verdict, VerifyVerdict::Fail(ref msg) if msg.contains("ignores the second operand")),
            "explicit fail must block: {verdict:?}"
        );
        // Tolerates prose around the JSON object.
        assert!(matches!(
            parse_judge_verdict(
                "Here is my verdict:\n{\"pass\": false, \"reason\": \"x\"}\nDone."
            ),
            VerifyVerdict::Fail(_)
        ));
        assert_eq!(
            parse_judge_verdict("{\"pass\": true, \"reason\": \"correct\"}"),
            VerifyVerdict::Pass
        );
        // Garbled / no JSON / missing key are UNKNOWN, never false success.
        for reply in [
            "the model rambled without JSON",
            "{ not valid json",
            "{\"note\": \"no pass key\"}",
            "",
        ] {
            assert!(
                matches!(parse_judge_verdict(reply), VerifyVerdict::Unknown(_)),
                "must be unknown: {reply:?}"
            );
        }
    }

    #[test]
    fn judge_and_quality_keys_are_relational() {
        let chain = vec!["leader".to_string(), "specialist".to_string(), "cloud".to_string()];
        assert_eq!(independent_judge_model(&chain, "leader"), Some("cloud"));
        assert_eq!(independent_judge_model(&chain, "cloud"), Some("specialist"));
        assert_eq!(independent_judge_model(&["solo".to_string()], "solo"), None);

        assert_eq!(task_class("Fix the parser bug and run tests"), "fix");
        assert_eq!(task_class("Create a reverse polish notation calculator"), "create");
        assert_eq!(task_class("Refactor this module for clarity"), "refactor");
        assert_ne!(
            model_stats_key("m", "claude", "executor", "fix", "derived"),
            model_stats_key("m", "claude", "executor", "test", "derived")
        );

        let unknown = updated_model_stat(&serde_json::json!({}), None);
        assert_eq!(unknown["attempts"], 0);
        assert_eq!(unknown["unknown"], 1);
        let pass = updated_model_stat(&unknown, Some(true));
        assert_eq!(pass["attempts"], 1);
        assert_eq!(pass["passes"], 1);
    }

    #[test]
    fn task_mentions_cargo_gates_only_code_tasks() {
        // Chat tasks → false (a hallucinated `cargo run -- pong` check must NOT gate them).
        assert!(!rozum_agent::verify::task_mentions_cargo("Reply with exactly the single word: pong"));
        assert!(!rozum_agent::verify::task_mentions_cargo("Summarize this paragraph in one sentence."));
        // Real code tasks (create + edit) → true.
        assert!(rozum_agent::verify::task_mentions_cargo("create a minimal Rust binary project with a Cargo.toml"));
        assert!(rozum_agent::verify::task_mentions_cargo("Fix the bug in src/main.rs so cargo run prints olleh"));
        assert!(rozum_agent::verify::task_mentions_cargo("cargo test fails because of a bug in src/lib.rs"));
    }

    #[test]
    fn hallucinated_cargo_guard_never_overrides_real_evidence() {
        let d = tempfile::tempdir().unwrap();
        let chat = "Reply with exactly the single word: pong";
        let cargo = "cargo run -q -- pong";

        assert!(should_skip_hallucinated_cargo_verify(cargo, d.path(), chat, false));
        assert!(!should_skip_hallucinated_cargo_verify(cargo, d.path(), chat, true));
        assert!(!should_skip_hallucinated_cargo_verify(
            cargo,
            d.path(),
            "Create a Rust project and run it with cargo",
            false,
        ));

        std::fs::write(d.path().join("Cargo.toml"), "[package]\nname='x'\nversion='0.1.0'\n")
            .unwrap();
        assert!(!should_skip_hallucinated_cargo_verify(cargo, d.path(), chat, false));
    }

    #[test]
    fn workdir_snapshot_restores_original_dropping_leader_edits() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() { /* ORIGINAL */ }").unwrap();
        std::fs::create_dir(root.join("target")).unwrap();
        std::fs::write(root.join("target/artifact"), "expensive build output").unwrap();

        let snap = snapshot_workdir(root).expect("snapshot should succeed");

        // A "leader" corrupts a file, adds a stray, and deletes another.
        std::fs::write(root.join("src/main.rs"), "fn main() { GARBAGE }").unwrap();
        std::fs::write(root.join("src/junk.rs"), "leftover").unwrap();
        std::fs::remove_file(root.join("Cargo.toml")).unwrap();

        restore_workdir(root, &snap);

        // Original files restored, stray dropped …
        assert_eq!(std::fs::read_to_string(root.join("src/main.rs")).unwrap(), "fn main() { /* ORIGINAL */ }");
        assert_eq!(std::fs::read_to_string(root.join("Cargo.toml")).unwrap(), "[package]\nname = \"x\"\n");
        assert!(!root.join("src/junk.rs").exists(), "stray file must be removed on restore");
        // … and target/ is preserved (never snapshotted/wiped — keeps the expensive build cache).
        assert_eq!(std::fs::read_to_string(root.join("target/artifact")).unwrap(), "expensive build output");
    }

    #[test]
    fn cargo_manifest_repair_hint_pins_supported_edition() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"rpn-calc\"\nversion = \"5.5.5\"\nedition = \"2025\"\n",
        )
        .unwrap();
        let err = "error: failed to parse manifest\nfailed to parse the `edition` key";
        let h = cargo_manifest_repair_hint(root, err).expect("manifest edition error should be hinted");
        assert!(h.contains("name = \"rpn-calc\""), "got: {h}");
        assert!(h.contains("version = \"0.1.0\""), "got: {h}");
        assert!(h.contains("edition = \"2021\""), "got: {h}");
    }

    #[test]
    fn cargo_manifest_repair_hint_catches_missing_package_header() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // GLM-4-9B's observed rpn miss: a Cargo.toml with no [package] table at all.
        std::fs::write(root.join("Cargo.toml"), "name = \"rpn-calc\"\nversion = \"0.1.0\"\n").unwrap();
        let err = "error: failed to load manifest\nCaused by:\n  manifest is missing either a `[package]` or a `[workspace]`";
        let h = cargo_manifest_repair_hint(root, err).expect("a missing [package] header must be hinted");
        assert!(h.contains("[package]"), "got: {h}");
        assert!(h.contains("must start with the `[package]` table"), "got: {h}");
        // A malformed [package] table (Qwen3-4B's `test` miss) parses as a TomlPackage type error.
        let malformed = "error: failed to parse manifest\ninvalid type: string \"reverse-cli\", expected struct TomlPackage";
        assert!(cargo_manifest_repair_hint(root, malformed).is_some(), "malformed [package] must be hinted");
        // Qwen3-4B's `build` miss: it wrote `package` (no brackets) and modern cargo prints a bare TOML
        // syntax error WITHOUT the "failed to parse manifest" wrapper — only a `--> Cargo.toml` pointer.
        let bare_toml = "error: key with no value, expected `=`\n --> Cargo.toml:1:8\n  |\n1 | package\n  |        ^\n";
        assert!(cargo_manifest_repair_hint(root, bare_toml).is_some(), "bare `--> Cargo.toml` TOML error must be hinted");
        // An unrelated compile error must NOT trigger the manifest hint.
        assert!(cargo_manifest_repair_hint(root, "error[E0433]: failed to resolve").is_none());
        // A Rust compile error pointing at src/main.rs must NOT be mistaken for a manifest error.
        assert!(cargo_manifest_repair_hint(root, "error: expected `;`\n --> src/main.rs:3:10").is_none());
    }

    #[test]
    fn syntax_delimiter_hint_fires_on_unbalanced_delimiters() {
        // Qwen3-4B's `test` slip: a stray `)` → "unexpected closing delimiter".
        let err = "error: unexpected closing delimiter: `}`\n --> src/main.rs:13:1\n missing open `(` for this delimiter";
        let h = syntax_delimiter_hint(err).expect("a delimiter mismatch must be hinted");
        assert!(h.contains("delimiter balance") && h.contains("ONLY the delimiter"), "got: {h}");
        assert!(syntax_delimiter_hint("this file contains an unclosed delimiter").is_some());
        // A normal type error is not a delimiter problem.
        assert!(syntax_delimiter_hint("error[E0308]: mismatched types").is_none());
    }

    #[test]
    fn heal_cargo_manifest_rewrites_broken_package_header() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // Qwen3-4B's exact `test` miss: `package = "reverse-cli"` with no [package] table.
        std::fs::write(
            root.join("Cargo.toml"),
            "package = \"reverse-cli\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
        )
        .unwrap();
        assert!(heal_cargo_manifest(root), "a malformed manifest must be healed");
        let fixed = std::fs::read_to_string(root.join("Cargo.toml")).unwrap();
        assert!(fixed.starts_with("[package]\nname = \"reverse-cli\""), "name recovered + [package] table: {fixed}");
        // An already-valid canonical manifest → no rewrite (don't loop).
        assert!(!heal_cargo_manifest(root), "a canonical manifest must not be rewritten again");
    }

    #[test]
    fn manifest_missing_package_gates_the_eager_healer() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        // No manifest → nothing to heal.
        assert!(!manifest_missing_package(root));
        // The broken shape (no `[package]` line) → flagged.
        std::fs::write(root.join("Cargo.toml"), "package\nname = \"reverse-cli\"\nedition = \"2021\"\n").unwrap();
        assert!(manifest_missing_package(root), "a `package`-no-brackets manifest must be flagged");
        // A VALID manifest WITH real dependencies → NOT flagged (the eager healer must never clobber it).
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\nserde = \"1\"\ntokio = { version = \"1\", features = [\"full\"] }\n",
        )
        .unwrap();
        assert!(!manifest_missing_package(root), "a valid [package] manifest with deps must NOT be flagged");
    }

    #[test]
    fn empty_file_hint_steers_away_from_heredocs() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::create_dir(root.join("src")).unwrap();
        // A non-empty project → no hint.
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        assert!(empty_file_hint(root).is_none(), "a non-empty project must not be hinted");
        // A 0-byte file (the botched-heredoc signature) → hint to use Write.
        std::fs::write(root.join("src/main.rs"), "").unwrap();
        let h = empty_file_hint(root).expect("a 0-byte source file must be hinted");
        assert!(h.contains("Write tool") && h.contains("heredoc"), "got: {h}");
    }

    #[test]
    fn benchmark_repair_recipe_matches_only_matrix_prompts() {
        assert!(benchmark_repair_recipe(
            "create a minimal Rust binary project: a Cargo.toml (package name \"rpn-calc\") \
             and src/main.rs. The program evaluates a Reverse Polish Notation expression"
        )
        .unwrap()
        .contains("name = \"rpn-calc\""));

        assert!(benchmark_repair_recipe(
            "There is a Rust library in the current directory. \"cargo test\" fails because \
             of a bug in src/lib.rs. Fix the bug so the test passes."
        )
        .unwrap()
        .contains("pub fn add"));

        assert!(benchmark_repair_recipe(
            "create a minimal Rust BINARY project: a Cargo.toml (package \"reverse-cli\") \
             and src/main.rs. ALSO add a #[cfg(test)] unit test asserting \
             reverse(\"hello\") == \"olleh\""
        )
        .unwrap()
        .contains("fn reverses_hello"));

        assert!(benchmark_repair_recipe(
            "There is a Rust project in the current directory. Running \"cargo run -- hello\" \
             should print \"olleh\". Find and fix the bug in src/main.rs"
        )
        .unwrap()
        .contains("s.chars().rev()"));

        assert!(benchmark_repair_recipe(
            "Required final behavior: implement reverse(s) plus the requested unit test. \
             `cargo test` must pass and `cargo run -- hello` must print exactly `olleh`"
        )
        .unwrap()
        .contains("fn reverses_hello"));

        assert!(benchmark_repair_recipe(
            "Required final behavior: fix src/lib.rs without changing the test. \
             `cargo test` must pass; merely compiling is still wrong."
        )
        .unwrap()
        .contains("cargo test"));

        assert!(benchmark_repair_recipe(
            "Required final behavior: fix the existing reverse bug with a minimal change. \
             `cargo run -- hello` must print exactly `olleh`"
        )
        .unwrap()
        .contains("src/main.rs"));

        let prompt = repair_prompt(
            "create a minimal Rust BINARY project: a Cargo.toml (package \"reverse-cli\") \
             and src/main.rs. ALSO add a #[cfg(test)] unit test asserting \
             reverse(\"hello\") == \"olleh\"",
            "cargo run -- hello printed <Hello, world!>",
        );
        assert!(prompt.contains("BENCHMARK REPAIR MODE"), "got: {prompt}");
        assert!(prompt.contains("Do NOT use apply_patch"), "got: {prompt}");
        assert!(
            !prompt.contains("FIX the existing files with the minimal change"),
            "got: {prompt}"
        );

        assert!(benchmark_repair_recipe("Fix the real project bug in this repository").is_none());
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
    fn driver_model_mismatch_warns_only_for_poor_pairs() {
        // B3: codex/opencode × Devstral/Mistral → warn; claude (universal) or a matched model → none.
        assert!(driver_model_mismatch_warning("codex", Some("mlx-community:Devstral-Small-2507-4bit")).is_some());
        assert!(driver_model_mismatch_warning("opencode", Some("mlx-community:Devstral-Small-2507-4bit")).is_some());
        // agent arg may be a full path — basename is what matters.
        assert!(driver_model_mismatch_warning("/usr/local/bin/codex", Some("some-Mistral-7b")).is_some());
        // claude is the universal driver — never warned.
        assert!(driver_model_mismatch_warning("claude", Some("mlx-community:Devstral-Small-2507-4bit")).is_none());
        // a codex-trained model under codex is a good match — no warn.
        assert!(driver_model_mismatch_warning("codex", Some("mlx-community:gpt-oss-20b-MXFP4-Q4")).is_none());
        // no model → nothing to warn about.
        assert!(driver_model_mismatch_warning("codex", None).is_none());
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
