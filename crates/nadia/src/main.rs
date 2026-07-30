//! `nadia` — batch and interactive front-ends over one agent loop.
//!
//! Batch is the contract `scripts/bench/agentic.sh` needs: a CLI on `PATH`, a prompt as
//! an argument, work done in the current directory, and an exit code that distinguishes
//! "finished" from "gave up" from "the gateway is down" — the harness already reads
//! rc=2 as infrastructure failure rather than a model failure, and conflating the two is
//! how a dead gateway gets recorded as a bad model.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use nadia::sandbox::Sandbox;
use nadia::session::{default_budget, system_prompt, LoopBreaker, Session};
use nadia::tools::tool_source;
use rozum_agent::agent::{AgentStop, ToolSource};
use rozum_gateway::openai_http::OpenAiHttpBackend;

const USAGE: &str = "\
nadia — a coding agent on a local model

USAGE:
    nadia run <task>      run one task headlessly in the current directory
    nadia chat            interactive session (default when no arguments)

OPTIONS:
    --workspace <DIR>     where the agent may act        [default: current directory]
    --gateway <URL>       rozum gateway base URL         [env: ROZUM_GATEWAY_URL]
    --model <ID>          model id to ask the gateway for [env: NADIA_MODEL]
    --max-steps <N>       model round-trips per task     [default: 24]
    --allow-net           let `bash` reach the network   [default: denied]
    --no-confine          do not wrap `bash` in sandbox-exec
    --json                batch: print the full result as JSON
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
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-h" | "--help" => return Err(USAGE.into()),
            "--allow-net" => o.allow_net = true,
            "--no-confine" => o.confine = false,
            "--json" => o.json = true,
            "--workspace" | "--gateway" | "--model" | "--max-steps" => {
                let v = args.get(i + 1).ok_or_else(|| format!("{a} needs a value"))?;
                match a {
                    "--workspace" => o.workspace = PathBuf::from(v),
                    "--gateway" => o.gateway = with_v1(v),
                    "--model" => o.model = v.clone(),
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
    let tools = LoopBreaker::new(tool_source(sandbox));
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
        "chat" => repl(&backend, &tools, &root, budget, &opts).await,
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

/// The interactive front-end. Line-based on purpose: it works over ssh, in a pipe, and
/// inside `tmux` without a terminal-control layer, and the thing a coding agent's UI
/// actually has to get right is showing *what it did* — one line per tool call — not
/// drawing panes. Token streaming is P1 (`SPEC.md`); the loop entry point used here
/// returns per turn.
async fn repl(
    backend: &OpenAiHttpBackend,
    tools: &impl ToolSource,
    root: &std::path::Path,
    budget: rozum_agent::agent::Budget,
    opts: &Opts,
) {
    let mut session = Session::new(backend, tools, &system_prompt(root), budget);
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
        if line.is_empty() {
            continue;
        }
        if line.starts_with('/') {
            match line {
                "/quit" | "/exit" => break,
                "/help" => println!(
                    "/tools  list the tools\n/clear  forget the conversation\n\
                     /context  message count\n/quit  exit"
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
                other => println!("unknown command {other} — /help"),
            }
            continue;
        }

        let outcome = session.turn(line).await;
        for op in &outcome.operations {
            let summary = match &op.output {
                Ok(v) => one_line(&v.to_string()),
                Err(e) => format!("error: {}", one_line(e)),
            };
            println!("  ⏺ {:<11} {}", op.name, summary);
        }
        if !outcome.text.is_empty() {
            println!("\n{}", outcome.text);
        }
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
