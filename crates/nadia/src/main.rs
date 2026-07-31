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
use nadia::sandbox::Sandbox;
use nadia::session::{default_budget, system_prompt, LoopBreaker, Session};
use nadia::serve::{serve, Config};
use nadia::supervisor::{Spec, Supervisor};
use nadia::tools::tool_source;
use rozum_agent::agent::{AgentObserver, AgentStop, ToolSource};
use rozum_gateway::openai_http::OpenAiHttpBackend;

const USAGE: &str = "\
nadia — a coding agent on a local model

USAGE:
    nadia run <task>      run one task headlessly in the current directory
    nadia chat            interactive session (default when no arguments)
    nadia serve           expose the subagent protocol over HTTP

OPTIONS:
    --workspace <DIR>     where the agent may act        [default: current directory]
    --gateway <URL>       rozum gateway base URL         [env: ROZUM_GATEWAY_URL]
    --model <ID>          model id to ask the gateway for [env: NADIA_MODEL]
    --max-steps <N>       model round-trips per task     [default: 24]
    --allow-net           let `bash` reach the network   [default: denied]
    --no-confine          do not wrap `bash` in sandbox-exec
    --json                batch: print the full result as JSON
    --port <N>            serve: listen here                  [default: 8790]
    --token <T>           serve: required in x-nadia-token; mandatory off loopback
    --bind <ADDR>         serve: address to bind              [default: 127.0.0.1]
    -h, --help            this text

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
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => return Err(USAGE.into()),
            "--allow-net" => o.allow_net = true,
            "--no-confine" => o.confine = false,
            "--json" => o.json = true,
            "--workspace" | "--gateway" | "--model" | "--max-steps" | "--port" | "--token"
            | "--bind" => {
                let v = args.get(i + 1).ok_or_else(|| format!("{a} needs a value"))?;
                match a {
                    "--workspace" => o.workspace = PathBuf::from(v),
                    "--gateway" => o.gateway = with_v1(v),
                    "--model" => o.model = v.clone(),
                    "--token" => o.token = v.clone(),
                    "--bind" => o.bind = v.clone(),
                    "--port" => o.port = v.parse().map_err(|_| format!("--port {v}: not a number"))?,
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
    let tools = ApprovalGate::new(
        LoopBreaker::new(tool_source(sandbox)),
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
            let outcome = session.turn(&task).await;
            if opts.json {
                println!("{}", result_json(&outcome));
            } else {
                println!("{}", outcome.text);
            }
            std::process::exit(match outcome.stop {
                AgentStop::Done => 0,
                AgentStop::BudgetSteps | AgentStop::BudgetTime => {
                    eprintln!("nadia: budget exhausted after {} steps", outcome.steps);
                    1
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
        if line.starts_with('/') {
            match line {
                "/quit" | "/exit" => break,
                "/help" => println!(
                    "/tools             list the tools\n\
                     /approve ask|auto  ask before writes and commands, or don't\n\
                     /clear             forget the conversation\n\
                     /context           message count\n\
                     /quit              exit\n\
                     \n\
                     subagents:\n\
                     /spawn <task>      start one on this workspace\n\
                     /agents            what they are all doing\n\
                     /status <id>       one of them, with its result when done\n\
                     /tell <id> <msg>   give it something for its next turn\n\
                     /pause|/resume <id>\n\
                     /stop <id>         finish the current tool, then wrap up\n\
                     /kill <id>         abort now and free the slot"
                ),
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
                        _ => Err(format!("unknown command {other} — /help")),
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
}

fn one_line(s: &str) -> String {
    let flat: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if flat.chars().count() > 96 {
        format!("{}…", flat.chars().take(96).collect::<String>())
    } else {
        flat
    }
}
