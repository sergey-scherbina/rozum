//! `nadia` — batch and interactive front-ends over one agent loop.
//!
//! Batch is the contract `scripts/bench/agentic.sh` needs: a CLI on `PATH`, a prompt as
//! an argument, work done in the current directory, and an exit code that distinguishes
//! "finished" from "gave up" from "the gateway is down" — the harness already reads
//! rc=2 as infrastructure failure rather than a model failure, and conflating the two is
//! how a dead gateway gets recorded as a bad model.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;

use nadia::approval::{describe, ApprovalGate, Mode, Policy, TerminalApprover};
use nadia::commands;
use nadia::sandbox::Sandbox;
use nadia::session::{default_budget, system_prompt, LoopBreaker, Session};
use nadia::serve::{serve, Config};
use nadia::supervisor::{Spec, Supervisor};
use nadia::tools::tool_source;
use rozum_agent::agent::{AgentObserver, AgentStop, MultiToolSource, ToolSource};
use rozum_gateway::openai_http::OpenAiHttpBackend;

const USAGE: &str = "\
nadia — a coding agent on a local model

USAGE:
    nadia run <task>      run one task headlessly in the current directory
    nadia chat            interactive session (default when no arguments)
    nadia serve           expose the subagent protocol over HTTP
    nadia mcp list        the configured MCP servers (--probe also lists their tools)
    nadia help            this text

OPTIONS:
    --workspace <DIR>     where the agent may act        [default: current directory]
    --gateway <URL>       rozum gateway base URL         [env: ROZUM_GATEWAY_URL]
    --model <ID>          model id to ask the gateway for [env: NADIA_MODEL]
    --max-steps <N>       model round-trips per task     [default: 24]
    --allow-net           let `bash` reach the network   [default: denied]
    --no-confine          do not wrap `bash` in sandbox-exec
    --json                batch: print the full result as JSON
    --mcp <NAME>          connect this MCP server's tools (repeatable)
    --mcp-all             connect every server in the config
    --mcp-config <PATH>   [default: <workspace>/.mcp.json, else ~/.config/nadia/mcp.json]
    --port <N>            serve: listen here                  [default: 8790]
    --token <T>           serve: required in x-nadia-token; mandatory off loopback
    --bind <ADDR>         serve: address to bind              [default: 127.0.0.1]
    -h, --help            this text

MCP servers are opt-in per run: a config file that merely exists adds no tools, because
every tool costs schema tokens in every request. Their tools are named
mcp__<server>__<tool>, are gated like `bash`, and run OUTSIDE the workspace jail.

EXIT CODES (batch):
    0  finished          1  budget exhausted          2  gateway/transport failure
";

/// Where to reach the gateway when the caller did not say.
///
/// `rozum launch` already exports both of these to every agent it starts
/// (`src/main.rs`: `OPENAI_BASE_URL` = `<base>/v1`, `ROZUM_GATEWAY_URL` = `<base>`), so
/// honouring them is what makes `AGENTS=nadia scripts/bench/agentic.sh` work with no
/// change to the launcher at all — nadia is wired by the same env contract as every
/// other OpenAI-compatible client.
fn default_gateway() -> String {
    if let Ok(v) = std::env::var("OPENAI_BASE_URL") {
        return with_v1(&v);
    }
    if let Ok(v) = std::env::var("ROZUM_GATEWAY_URL") {
        return with_v1(&v);
    }
    "http://127.0.0.1:8080/v1".into()
}

/// The two spellings of a gateway URL differ by exactly one path segment, and which one
/// you get depends on which env var was read. Normalize rather than make the caller care.
fn with_v1(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

struct Opts {
    workspace: PathBuf,
    gateway: String,
    model: String,
    max_steps: usize,
    allow_net: bool,
    confine: bool,
    json: bool,
    port: u16,
    token: String,
    bind: String,
    /// MCP servers to connect, by name in the config. Opt-in per run (`nadia:SPEC.md` §2.1):
    /// an empty list connects nothing, however many servers the config holds.
    mcp: Vec<String>,
    mcp_all: bool,
    mcp_config: Option<PathBuf>,
    /// `mcp list --probe`: connect each server and list what it actually serves.
    probe: bool,
}

fn parse(args: &[String]) -> Result<(String, String, Opts), String> {
    let mut mode = String::new();
    let mut task = String::new();
    let mut o = Opts {
        workspace: std::env::current_dir().map_err(|e| e.to_string())?,
        gateway: default_gateway(),
        model: std::env::var("NADIA_MODEL").unwrap_or_else(|_| "local".into()),
        max_steps: 24,
        allow_net: false,
        confine: true,
        json: false,
        port: 8790,
        token: std::env::var("NADIA_TOKEN").unwrap_or_default(),
        bind: "127.0.0.1".into(),
        mcp: Vec::new(),
        mcp_all: false,
        mcp_config: None,
        probe: false,
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => return Err(USAGE.into()),
            "--allow-net" => o.allow_net = true,
            "--no-confine" => o.confine = false,
            "--json" => o.json = true,
            "--mcp-all" => o.mcp_all = true,
            "--probe" => o.probe = true,
            "--workspace" | "--gateway" | "--model" | "--max-steps" | "--port" | "--token"
            | "--bind" | "--mcp" | "--mcp-config" => {
                let v = args.get(i + 1).ok_or_else(|| format!("{a} needs a value"))?;
                match a {
                    "--workspace" => o.workspace = PathBuf::from(v),
                    "--gateway" => o.gateway = with_v1(v),
                    "--model" => o.model = v.clone(),
                    "--token" => o.token = v.clone(),
                    "--bind" => o.bind = v.clone(),
                    "--port" => o.port = v.parse().map_err(|_| format!("--port {v}: not a number"))?,
                    // Repeatable: `--mcp a --mcp b` connects both, in the order given.
                    "--mcp" => o.mcp.push(v.clone()),
                    "--mcp-config" => o.mcp_config = Some(PathBuf::from(v)),
                    _ => o.max_steps = v.parse().map_err(|_| format!("--max-steps {v}: not a number"))?,
                }
                i += 1;
            }
            _ if a.starts_with('-') => return Err(format!("unknown option {a}\n\n{USAGE}")),
            _ if mode.is_empty() => mode = a.to_string(),
            _ if task.is_empty() => task = a.to_string(),
            _ => task = format!("{task} {a}"),
        }
        i += 1;
    }
    if mode.is_empty() {
        mode = "chat".into();
    }
    Ok((mode, task, o))
}

/// Is this line a request for help, and for which command? `Some(None)` = the whole list,
/// `Some(Some(name))` = one command in detail, `None` = not a help request at all.
///
/// Accepts the four spellings a person actually types (`help`, `?`, `/help`, `/?`) and only as
/// the whole line: `help me refactor this` is a message for the model, not a command. The
/// argument may carry a slash or not — `help tell` and `? /tell` are the same question.
fn help_request(line: &str) -> Option<Option<&str>> {
    let line = line.trim();
    let (head, rest) = match line.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim()),
        None => (line, ""),
    };
    let is_help = matches!(head.to_ascii_lowercase().as_str(), "help" | "?" | "/help" | "/?");
    if !is_help {
        return None;
    }
    Some((!rest.is_empty()).then_some(rest))
}

/// Connect the MCP servers this run asked for, or exit 2 saying which one could not be reached.
/// Nothing asked for → nothing connected, whatever the config holds: the tools cost schema tokens
/// on every request of every step, so paying is the operator's decision (`nadia:SPEC.md` §2.1).
async fn connect_mcp(opts: &Opts, workspace: &std::path::Path) -> Vec<nadia::mcp::McpServer> {
    if opts.mcp.is_empty() && !opts.mcp_all {
        return Vec::new();
    }
    match load_selected(opts, workspace).await {
        Ok(servers) => servers,
        Err(e) => {
            eprintln!("nadia: {e}");
            std::process::exit(2);
        }
    }
}

/// The fallible half of [`connect_mcp`], so every failure is one `?` and one exit site.
async fn load_selected(
    opts: &Opts,
    workspace: &std::path::Path,
) -> Result<Vec<nadia::mcp::McpServer>, String> {
    let path = nadia::mcp::config_path(opts.mcp_config.as_deref(), workspace).ok_or_else(|| {
        "no MCP config found — looked for <workspace>/.mcp.json and ~/.config/nadia/mcp.json \
         (or pass --mcp-config)"
            .to_string()
    })?;
    let cfg = nadia::mcp::load_config(&path)?;
    let mut out = Vec::new();
    for (name, spec) in nadia::mcp::select(&cfg, &opts.mcp, opts.mcp_all)? {
        out.push(nadia::mcp::McpServer::connect(&name, spec).await?);
    }
    Ok(out)
}

/// `nadia mcp list [--probe]` — what is configured, and with `--probe` what each server actually
/// serves. Listed, never guessed: the prefix a tool will carry is shown, because that is the name
/// the model will use and the one that shows up in the approval prompt.
async fn run_mcp_list(opts: &Opts, workspace: &std::path::Path) -> i32 {
    let Some(path) = nadia::mcp::config_path(opts.mcp_config.as_deref(), workspace) else {
        println!(
            "no MCP config — looked for {}/.mcp.json and ~/.config/nadia/mcp.json",
            workspace.display()
        );
        return 0;
    };
    let cfg = match nadia::mcp::load_config(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nadia: {e}");
            return 2;
        }
    };
    println!("{}", path.display());
    if cfg.servers.is_empty() {
        println!("  (no servers configured)");
        return 0;
    }
    let mut rc = 0;
    for (name, spec) in &cfg.servers {
        match spec.stdio_command(name) {
            Ok(cmd) => println!("  {name}  {cmd} {}", spec.args.join(" ")),
            Err(e) => {
                println!("  {name}  ✗ {e}");
                rc = 2;
                continue;
            }
        }
        if !opts.probe {
            continue;
        }
        match nadia::mcp::McpServer::connect(name, spec).await {
            Ok(s) => {
                for t in s.tool_names() {
                    println!("      {t}");
                }
            }
            Err(e) => {
                println!("      ✗ {e}");
                rc = 2;
            }
        }
    }
    rc
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mode, task, opts) = match parse(&args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(if msg.starts_with("nadia —") { 0 } else { 2 });
        }
    };
    // `nadia help` is the same answer as `-h`, on stdout and exit 0. A user who guesses the word
    // everybody guesses should not be told that their guess is an unknown mode.
    if mode.eq_ignore_ascii_case("help") || mode == "?" {
        println!("{USAGE}");
        std::process::exit(0);
    }

    let mut sb = match Sandbox::new(&opts.workspace) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("nadia: {e}");
            std::process::exit(2);
        }
    };
    sb.allow_net = opts.allow_net;
    sb.confine = opts.confine && cfg!(target_os = "macos");
    let root = sb.root().to_path_buf();
    let sandbox = Arc::new(sb);

    // Which gateway and which model a run actually talked to is the first question asked
    // of any surprising matrix row, and it is not recoverable after the fact.
    if std::env::var("NADIA_DEBUG").is_ok() {
        eprintln!(
            "nadia: gateway={} model={} workspace={}",
            opts.gateway,
            opts.model,
            root.display()
        );
    }
    let backend = OpenAiHttpBackend::new(opts.gateway.clone(), opts.model.clone());
    // Batch runs unattended, so asking would deadlock on a stdin nobody is at; the
    // sandbox is the containment there. Chat has a person present, so the default flips.
    let policy = Policy::new(if mode == "run" { Mode::Auto } else { Mode::Ask });
    // MCP servers, if any were asked for. Connected BEFORE the loop starts and never per step:
    // a server that will not start ends the run here, with its name — a run that silently lost
    // half its tools produces a confidently wrong answer (`nadia:SPEC.md` §2.1).
    let servers = connect_mcp(&opts, &root).await;
    let mut sources = MultiToolSource::new().with(tool_source(sandbox));
    for s in servers {
        println!("{}", nadia::mcp::connected_line(&s));
        sources = sources.with(s);
    }
    let tools = ApprovalGate::new(
        LoopBreaker::new(sources),
        policy.clone(),
        Box::new(TerminalApprover),
    );
    let mut budget = default_budget();
    budget.max_steps = opts.max_steps;

    match mode.as_str() {
        "run" => {
            if task.is_empty() {
                eprintln!("nadia run needs a task\n\n{USAGE}");
                std::process::exit(2);
            }
            let mut session = Session::new(&backend, &tools, &system_prompt(&root), budget);
            // What "done" means, decided before the run (`gate.rs`). NADIA_VERIFY=0 turns it off.
            let check = nadia::gate::derive(&backend, &task, &root).await;
            match (&check, nadia::gate::owner()) {
                (Some(c), _) => eprintln!("nadia: acceptance check — {c}"),
                // Say which gate owns the run rather than looking ungated: an operator reading
                // "no check" would otherwise conclude nothing verifies this.
                (None, Some(o)) => eprintln!("nadia: verification is {o}'s for this run"),
                (None, None) => {}
            }
            let mut outcome = session.turn(&task).await;
            let mut report = nadia::gate::Report::default();
            for round in 0..nadia::gate::rounds() {
                let (r, repair) =
                    nadia::gate::check(&backend, &task, &root, check.as_deref(), &outcome).await;
                report = nadia::gate::Report { rounds: round, ..r };
                // A run that stopped WITHOUT finishing still gets its deterministic check — the
                // artifact is on disk either way, and that run is the one the operator has most
                // doubt about. Measured 2026-08-04: an RPN attempt exhausted its steps, the
                // derived check was discarded unrun, and the report read `⚠ not checked` while
                // the program on disk printed nothing for the argument the task named. What it
                // does NOT get is a repair round: there is no budget left to repair with.
                if !matches!(outcome.stop, AgentStop::Done) {
                    break;
                }
                let Some(prompt) = repair else { break };
                eprintln!("nadia: check failed — repair round {}", round + 1);
                outcome = session.turn(&prompt).await;
            }
            eprintln!("nadia: {}", report.summary());
            if opts.json {
                println!("{}", result_json(&outcome));
            } else {
                println!("{}", outcome.text);
            }
            // A run whose check FAILED did not finish, whatever the model says about it. Exit 1
            // (the "gave up" code the harness reads) rather than 0: the whole point of the gate
            // is that success is not the model's to declare.
            if report.passed == Some(false) {
                std::process::exit(1);
            }
            std::process::exit(match outcome.stop {
                AgentStop::Done => 0,
                AgentStop::BudgetSteps | AgentStop::BudgetTime => {
                    if report.passed == Some(true) {
                        // The check decides in BOTH directions. An agent that satisfied the
                        // acceptance criterion and then ran out of steps has done the task;
                        // reporting that as a failure would be the same mistake as trusting a
                        // model that says it finished.
                        eprintln!(
                            "nadia: budget exhausted after {} steps — but the check passed",
                            outcome.steps
                        );
                        0
                    } else {
                        eprintln!("nadia: budget exhausted after {} steps", outcome.steps);
                        1
                    }
                }
                AgentStop::Error(e) => {
                    eprintln!("nadia: {e}");
                    2
                }
            });
        }
        "chat" => repl(&backend, &tools, &root, budget, &opts, policy).await,
        "serve" => {
            let addr: std::net::SocketAddr = match format!("{}:{}", opts.bind, opts.port).parse() {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("nadia: --bind/--port: {e}");
                    std::process::exit(2);
                }
            };
            let cfg = Config {
                workspace: root.clone(),
                gateway: opts.gateway.clone(),
                model: opts.model.clone(),
                budget,
                allow_net: opts.allow_net,
                confine: opts.confine,
                token: opts.token.clone(),
            };
            println!("nadia serve · {addr} · {} · {}", opts.model, root.display());
            if cfg.token.is_empty() {
                println!("no token: loopback only");
            }
            if let Err(e) = serve(Supervisor::new(), cfg, addr).await {
                eprintln!("nadia: {e}");
                std::process::exit(2);
            }
        }
        // `nadia mcp list [--probe]`. `list` is the only verb, and it is required rather than
        // implied: a bare `nadia mcp` is more likely a half-typed command than a request.
        "mcp" => {
            if task != "list" {
                eprintln!("nadia mcp list [--probe]\n\n{USAGE}");
                std::process::exit(2);
            }
            std::process::exit(run_mcp_list(&opts, &root).await);
        }
        other => {
            eprintln!("unknown mode `{other}`\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn result_json(o: &rozum_agent::agent::AgentOutcome) -> String {
    let ops: Vec<serde_json::Value> = o
        .operations
        .iter()
        .map(|op| {
            serde_json::json!({
                "tool": op.name,
                "input": op.input,
                "ok": op.output.is_ok(),
                "output": match &op.output {
                    Ok(v) => v.clone(),
                    Err(e) => serde_json::Value::String(e.clone()),
                },
            })
        })
        .collect();
    serde_json::json!({
        "text": o.text,
        "stop": format!("{:?}", o.stop),
        "steps": o.steps,
        "operations": ops,
    })
    .to_string()
}

/// Renders the run as it happens.
///
/// Without this the REPL says nothing for the length of a turn — 45 seconds on the
/// slowest benchmark task — and an agent that prints nothing is indistinguishable from an
/// agent that has hung. The text is streamed as the model produces it, and each tool call
/// is announced before it runs, so the last line on screen is always what is happening now.
struct Live;

impl AgentObserver for Live {
    fn on_text(&self, delta: &str) {
        print!("{delta}");
        let _ = std::io::stdout().flush();
    }

    fn on_tool_call(&self, name: &str, input: &serde_json::Value) {
        println!("\n  ⏺ {name} {}", one_line(&describe(name, input)));
    }

    fn on_tool_result(&self, _name: &str, error: Option<&str>) {
        if let Some(e) = error {
            println!("    ✗ {}", one_line(e));
        }
    }
}

/// The interactive front-end. Line-based on purpose: it works over ssh, in a pipe, and
/// inside `tmux` without a terminal-control layer, and the thing a coding agent's UI
/// actually has to get right is showing *what it did* — one line per tool call — not
/// drawing panes. Text streams token by token and
/// each tool call is announced by [`Live`] as it happens.
async fn repl(
    backend: &OpenAiHttpBackend,
    tools: &impl ToolSource,
    root: &std::path::Path,
    budget: rozum_agent::agent::Budget,
    opts: &Opts,
    policy: std::sync::Arc<Policy>,
) {
    let mut session = Session::new(backend, tools, &system_prompt(root), budget);
    let sup = Supervisor::new();
    println!("nadia · {} · {}", opts.model, root.display());
    println!("{} tools · /help for commands · ctrl-d to exit", tools.tools().len());
    if !opts.allow_net {
        println!("network denied to `bash` (--allow-net to permit)");
    }

    loop {
        print!("\n› ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => break, // ctrl-d
            Ok(_) => {}
            Err(e) => {
                eprintln!("nadia: stdin: {e}");
                break;
            }
        }
        let line = line.trim();
        // Under a pipe there is no typed echo, so the prompt and the first line of output
        // collide on one line and the transcript becomes unreadable. Echo it ourselves.
        if !std::io::stdin().is_terminal() {
            println!("{line}");
        }
        if line.is_empty() {
            continue;
        }
        // `help`, `?` and `/?` are the same command as `/help`, with or without an argument:
        // at a prompt they are a question for the program, and spending a model turn to answer
        // what nadia already knows is seconds on a local model (`nadia:SPEC.md` §4.2). The match
        // is on the whole line — `help me refactor this` is a message, and goes to the model.
        if let Some(rest) = help_request(line) {
            match rest {
                None => println!("{}", commands::help_all()),
                Some(name) => match commands::help_one(name) {
                    Some(text) => println!("{text}"),
                    None => println!("{}", commands::unknown_command(name)),
                },
            }
            continue;
        }
        if line.starts_with('/') {
            match line {
                "/quit" | "/exit" => break,
                "/mcp" => {
                    let names: Vec<String> = tools
                        .tools()
                        .into_iter()
                        .map(|t| t.name)
                        .filter(|n| nadia::mcp::is_mcp_tool(n))
                        .collect();
                    if names.is_empty() {
                        println!(
                            "no MCP server connected — start nadia with --mcp <name> \
                             (`nadia mcp list` shows what is configured)"
                        );
                    } else {
                        println!("{} MCP tool(s), OUTSIDE the workspace jail:", names.len());
                        for n in names {
                            println!("  {n}");
                        }
                    }
                }
                "/tools" => {
                    for t in tools.tools() {
                        println!("  {:<11} {}", t.name, t.description.lines().next().unwrap_or(""));
                    }
                }
                "/clear" => {
                    session.reset();
                    println!("context cleared");
                }
                "/context" => println!("{} messages", session.message_count()),
                "/agents" => {
                    let all = sup.list();
                    if all.is_empty() {
                        println!("no subagents");
                    }
                    for a in all {
                        println!(
                            "  #{:<3} {:<9} {:>3} calls  {:>4}s  {}{}",
                            a.id,
                            a.phase.label(),
                            a.tool_calls,
                            a.elapsed.as_secs(),
                            one_line(&a.task),
                            a.last_tool.map(|t| format!("  [{t}]")).unwrap_or_default()
                        );
                    }
                }
                "/approve auto" => {
                    policy.set_mode(Mode::Auto);
                    println!("approval: auto — writes and commands run without asking");
                }
                "/approve ask" => {
                    policy.set_mode(Mode::Ask);
                    println!("approval: ask");
                }
                other => {
                    let (cmd, rest) = match other.split_once(' ') {
                        Some((c, r)) => (c, r.trim()),
                        None => (other, ""),
                    };
                    // Subagent control. Ids are small integers on purpose: these get typed
                    // by a human under time pressure, and from a phone.
                    let id = || -> Result<u64, String> {
                        rest.parse::<u64>().map_err(|_| format!("{cmd} needs an agent id"))
                    };
                    let outcome: Result<String, String> = match cmd {
                        "/spawn" if !rest.is_empty() => sup
                            .spawn(Spec {
                                task: rest.to_string(),
                                // The child shares the parent's workspace: a subagent that
                                // cannot see the repo cannot help with it. Two agents editing
                                // one tree can still collide — that is the operator's call,
                                // and why /agents shows what each one is touching.
                                workspace: root.to_path_buf(),
                                gateway: opts.gateway.clone(),
                                model: opts.model.clone(),
                                budget: default_budget(),
                                parent: None,
                                allow_net: opts.allow_net,
                                confine: opts.confine,
                            })
                            .map(|id| format!("spawned #{id}")),
                        "/status" => id().and_then(|i| sup.status(i)).map(|a| {
                            format!(
                                "#{} {} · {} calls · {}s · {}{}",
                                a.id,
                                a.phase.label(),
                                a.tool_calls,
                                a.elapsed.as_secs(),
                                one_line(&a.task),
                                a.result.map(|r| format!("\n{r}")).unwrap_or_default()
                            )
                        }),
                        "/pause" => id().and_then(|i| sup.pause(i)).map(|_| "paused".into()),
                        "/resume" => id().and_then(|i| sup.resume(i)).map(|_| "resumed".into()),
                        "/stop" => id()
                            .and_then(|i| sup.stop(i))
                            .map(|_| "stopping at the next tool call".into()),
                        "/kill" => id().and_then(|i| sup.kill(i)).map(|_| "killed".into()),
                        "/tell" => match rest.split_once(' ') {
                            Some((i, msg)) => i
                                .parse::<u64>()
                                .map_err(|_| "/tell needs an agent id then a message".to_string())
                                .and_then(|i| sup.tell(i, msg))
                                .map(|_| "queued for its next turn".into()),
                            None => Err("/tell <id> <message>".into()),
                        },
                        _ => Err(commands::unknown_command(cmd)),
                    };
                    match outcome {
                        Ok(msg) => println!("{msg}"),
                        Err(e) => println!("{e}"),
                    }
                }
            }
            continue;
        }

        let outcome = session.turn_observed(line, &Live).await;
        // The text already streamed through the observer; all that is left is to close
        // the line it ended on, and to say why the loop stopped if it was not "done".
        println!();
        match outcome.stop {
            AgentStop::Done => {}
            AgentStop::BudgetSteps => println!("\n[stopped: step budget after {} steps]", outcome.steps),
            AgentStop::BudgetTime => println!("\n[stopped: time budget]"),
            AgentStop::Error(e) => println!("\n[gateway error: {e}]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_url_is_normalized_to_one_spelling() {
        // `rozum launch` sets ROZUM_GATEWAY_URL without /v1 and OPENAI_BASE_URL with it;
        // both must land on the same endpoint or nadia talks to a 404 under the harness.
        assert_eq!(with_v1("http://127.0.0.1:8080"), "http://127.0.0.1:8080/v1");
        assert_eq!(with_v1("http://127.0.0.1:8080/"), "http://127.0.0.1:8080/v1");
        assert_eq!(with_v1("http://127.0.0.1:8080/v1"), "http://127.0.0.1:8080/v1");
        assert_eq!(with_v1("http://127.0.0.1:8080/v1/"), "http://127.0.0.1:8080/v1");
    }

    #[test]
    fn bare_invocation_is_chat_and_run_takes_the_rest_as_the_task() {
        let (mode, task, _) = parse(&[]).unwrap();
        assert_eq!(mode, "chat");
        assert!(task.is_empty());

        let args: Vec<String> =
            ["run", "fix", "the", "test"].iter().map(|s| s.to_string()).collect();
        let (mode, task, _) = parse(&args).unwrap();
        assert_eq!(mode, "run");
        assert_eq!(task, "fix the test");
    }

    #[test]
    fn options_are_parsed_and_unknown_ones_are_refused() {
        let args: Vec<String> = ["run", "t", "--max-steps", "7", "--allow-net", "--no-confine"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (_, _, o) = parse(&args).unwrap();
        assert_eq!(o.max_steps, 7);
        assert!(o.allow_net);
        assert!(!o.confine);

        assert!(parse(&["--nope".to_string()]).is_err());
        assert!(parse(&["run".into(), "t".into(), "--max-steps".into()]).is_err());
    }

    #[test]
    fn mcp_servers_are_opt_in_and_repeatable() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // Nothing asked for → nothing connected, whatever a config might hold.
        let (_, _, o) = parse(&v(&["run", "t"])).unwrap();
        assert!(o.mcp.is_empty() && !o.mcp_all);
        // Repeatable, in the order given.
        let (_, _, o) = parse(&v(&["run", "t", "--mcp", "rozum", "--mcp", "fs"])).unwrap();
        assert_eq!(o.mcp, vec!["rozum".to_string(), "fs".to_string()]);
        let (_, _, o) = parse(&v(&["run", "t", "--mcp-all", "--mcp-config", "/tmp/x.json"])).unwrap();
        assert!(o.mcp_all);
        assert_eq!(o.mcp_config.as_deref(), Some(std::path::Path::new("/tmp/x.json")));
        // A flag that eats the next argument must still refuse to eat nothing.
        assert!(parse(&v(&["run", "t", "--mcp"])).is_err());
    }

    #[test]
    fn help_is_a_command_only_when_it_is_the_whole_line() {
        // The four spellings, bare.
        for line in ["help", "?", "/help", "/?", "  HELP  "] {
            assert_eq!(help_request(line), Some(None), "bare help: {line:?}");
        }
        // With a command name, slash optional.
        assert_eq!(help_request("help tell"), Some(Some("tell")));
        assert_eq!(help_request("? /tell"), Some(Some("/tell")));
        assert_eq!(help_request("/help  spawn "), Some(Some("spawn")));
        // A sentence that merely starts with the word is a message for the model. This is the
        // whole reason the match is on the head token and not on `contains`.
        assert_eq!(help_request("help me refactor this"), Some(Some("me refactor this")));
        assert_eq!(help_request("please help"), None);
        assert_eq!(help_request("/tools"), None);
        assert_eq!(help_request("what does ? mean"), None);
    }

    #[test]
    fn nadia_help_is_the_same_text_as_dash_h() {
        // `nadia help` prints USAGE and exits 0; `-h` returns it as the Err payload that main
        // prints with exit 0. Same text either way — a guessed word must not be a worse answer.
        let (mode, _, _) = parse(&["help".to_string()]).unwrap();
        assert_eq!(mode, "help");
        let usage = match parse(&["-h".to_string()]) {
            Err(u) => u,
            Ok(_) => panic!("-h must not parse as a mode"),
        };
        assert!(usage.starts_with("nadia —"), "-h must yield the usage text");
        assert!(usage.contains("nadia help"), "usage must document the word it accepts");
        assert!(usage.contains("--mcp <NAME>"), "usage must document the MCP flags");
    }
}

fn one_line(s: &str) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() > 96 {
        format!("{}…", flat.chars().take(96).collect::<String>())
    } else {
        flat
    }
}
