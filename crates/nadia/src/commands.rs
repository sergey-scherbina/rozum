//! The REPL's command table — one place that knows a command exists, what it is called with,
//! and what it does.
//!
//! Spec: `nadia:SPEC.md` §4.2. The clause is there because the first version of this help was a
//! single string literal next to a `match`, which is two lists that must agree and therefore
//! eventually don't: a command gets renamed in the `match` and stays in the help, or gets added
//! and never appears. Here the help is *rendered from* the table the dispatcher also consults, so
//! the drift has nowhere to live.
//!
//! `help` / `?` (and `/help` / `/?`) render it two ways: bare — every command as
//! `format — short`; with a name — that one command's format, its short line and the paragraph
//! that says what is load-bearing about it.

/// One REPL command as the help needs it. `format` is the literal call shape *with* its
/// arguments (`/tell <id> <message>`), because the thing a user does not know at the moment they
/// ask is the arguments, not the name they just typed.
pub struct Command {
    pub name: &'static str,
    pub format: &'static str,
    pub short: &'static str,
    /// What it is for, and whatever costs or consequences are not obvious from the short line.
    pub long: &'static str,
}

pub const COMMANDS: &[Command] = &[
    Command {
        name: "/help",
        format: "help | ? | /help [command]",
        short: "this list, or one command in detail",
        long: "With no argument: every command, with its format and one line. With a command \
               name (the leading slash is optional — `help tell` and `? /tell` are the same \
               question) it prints that command's format, its short line and this paragraph. \
               `help` and `?` work without the slash: at a prompt they are a question for the \
               program, and sending them to the model would spend a turn to be told what nadia \
               already knows.",
    },
    Command {
        name: "/tools",
        format: "/tools",
        short: "list the tools the model can call",
        long: "The six built-ins, plus any tool from an MCP server connected with `--mcp` \
               (those are named `mcp__<server>__<tool>`). Every tool listed here costs schema \
               tokens in every request of every step, which is why the set is small and why MCP \
               servers are opt-in per run rather than loaded because a config file exists.",
    },
    Command {
        name: "/mcp",
        format: "/mcp",
        short: "the connected MCP servers and their tools",
        long: "Shows each server connected for this session and the tools it contributed. An \
               MCP server is a separate process with its own access to the machine: the path \
               jail and the seatbelt profile confine nadia, not it. Its calls still pass the \
               approval gate, exactly like `bash`.",
    },
    Command {
        name: "/approve",
        format: "/approve ask | auto",
        short: "ask before writes and commands, or don't",
        long: "`ask` (the default in chat) prompts before every write_file, edit_file, bash and \
               MCP call, showing a diff for edits. `auto` turns the gate off for the REST OF \
               THE SESSION, not for one call — the sandbox is then the only thing between the \
               model and the workspace. Reads are never gated: prompting for them trains you to \
               hit `y` without reading.",
    },
    Command {
        name: "/clear",
        format: "/clear",
        short: "forget the conversation",
        long: "Drops the message history and starts a fresh context; the workspace and any \
               subagents are untouched. Useful when a long thread has drifted — a small model \
               follows a short, clean context better than a long one it has half-forgotten.",
    },
    Command {
        name: "/context",
        format: "/context",
        short: "how many messages the model is carrying",
        long: "The message count of the current context. It grows with every turn and every \
               tool result; when it gets long, `/clear` is usually better than hoping.",
    },
    Command {
        name: "/quit",
        format: "/quit | /exit",
        short: "exit nadia",
        long: "Leaves the session. Ctrl-D does the same. Subagents live inside this process, so \
               quitting ends them — `/agents` first if you are not sure what is still running.",
    },
    Command {
        name: "/spawn",
        format: "/spawn <task>",
        short: "start a subagent on this workspace",
        long: "Starts an agent with its own budget and mailbox on the SAME workspace, and \
               returns its id immediately. Two agents editing one tree can collide; that is \
               your call to make, and why `/agents` shows what each one is touching.",
    },
    Command {
        name: "/agents",
        format: "/agents",
        short: "what every subagent is doing",
        long: "One line each: id, phase, tool calls, elapsed, task, and the tool it is in right \
               now. The counts are measured from tool dispatch, not from the agent's own \
               report — an agent that has lost the thread reports progress happily.",
    },
    Command {
        name: "/status",
        format: "/status <id>",
        short: "one subagent, with its result once it has one",
        long: "The same line `/agents` prints, plus the final answer when the agent is done or \
               failed. `done`, `failed` and `killed` are terminal; `stopping` means a polite \
               stop is in flight and will complete at the next tool boundary.",
    },
    Command {
        name: "/tell",
        format: "/tell <id> <message>",
        short: "give a subagent something for its next turn",
        long: "Queues a message. It is delivered BETWEEN turns, not during one: the loop owns \
               its message list until it returns, so a `tell` sent mid-turn lands when that \
               turn ends rather than interrupting it.",
    },
    Command {
        name: "/pause",
        format: "/pause <id>",
        short: "park a subagent at the next tool boundary",
        long: "It stops at the next tool call and costs nothing while parked — no model \
               traffic, no tokens. `/resume` continues from exactly there.",
    },
    Command {
        name: "/resume",
        format: "/resume <id>",
        short: "continue a paused subagent",
        long: "Picks up where `/pause` parked it, with its context intact.",
    },
    Command {
        name: "/stop",
        format: "/stop <id>",
        short: "finish the current tool, then wrap up",
        long: "Cooperative: the stop reaches the model as a tool error, so it gets to write a \
               closing summary of what it did. Costs one more model round-trip than `/kill`, \
               and is worth it whenever you want to know what happened.",
    },
    Command {
        name: "/kill",
        format: "/kill <id>",
        short: "abort a subagent now",
        long: "Ends the task immediately and frees the slot. No last words — nothing is written \
               back, so use `/stop` unless the agent is stuck or the work is worthless.",
    },
];

/// Look a command up by the name a user typed: with or without the leading slash, any case.
/// `?` and `help` both resolve to `/help`, because that is the command they are.
pub fn find(name: &str) -> Option<&'static Command> {
    let n = name.trim().trim_start_matches('/').to_ascii_lowercase();
    let n = match n.as_str() {
        "?" | "help" => "help",
        other => other,
    };
    COMMANDS.iter().find(|c| c.name.trim_start_matches('/') == n)
}

/// The bare `help` page: every command as `format — short`, aligned.
pub fn help_all() -> String {
    let width = COMMANDS.iter().map(|c| c.format.len()).max().unwrap_or(0);
    let mut out = String::new();
    for c in COMMANDS {
        out.push_str(&format!("  {:<width$}  {}\n", c.format, c.short, width = width));
    }
    out.push_str("\n  help <command> for one of them in detail. Anything else is a message to the model.");
    out
}

/// `help <command>`: format, short, then the paragraph. `None` when the name is not a command —
/// the caller answers with the names rather than the whole page, so a typo doesn't hide the answer.
pub fn help_one(name: &str) -> Option<String> {
    let c = find(name)?;
    Some(format!("  {}\n  {}\n\n{}", c.format, c.short, wrap(c.long, 76, "  ")))
}

/// The answer to a name that is not a command: say which, then the names only.
pub fn unknown_command(name: &str) -> String {
    let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
    format!("no command `{}`. There is: {}", name.trim(), names.join(" "))
}

/// Soft-wrap a paragraph to `width` columns with `indent` on every line. The long text is
/// written as one string; a terminal is not obliged to be wide.
fn wrap(text: &str, width: usize, indent: &str) -> String {
    let mut out = String::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
            out.push_str(indent);
            out.push_str(&line);
            out.push('\n');
            line.clear();
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        out.push_str(indent);
        out.push_str(&line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_command_carries_its_arguments_in_the_format() {
        for c in COMMANDS {
            assert!(
                c.format.contains(c.name.trim_start_matches('/')),
                "{}: the format must show the command itself, got `{}`",
                c.name,
                c.format
            );
            assert!(!c.short.is_empty() && !c.long.is_empty(), "{} needs both descriptions", c.name);
            // The short line is a line: it goes in a column next to every other one.
            assert!(c.short.len() <= 60, "{} short line is too long: {}", c.name, c.short);
        }
        // The commands that take an argument must SAY so — that is the whole point of printing
        // the format rather than the name.
        for name in ["/spawn", "/status", "/tell", "/pause", "/resume", "/stop", "/kill"] {
            let c = find(name).expect(name);
            assert!(c.format.contains('<'), "{name} takes an argument; its format must show it");
        }
    }

    #[test]
    fn help_resolves_the_names_a_user_actually_types() {
        // With or without the slash, any case — and `?`/`help` are the same command.
        for spelling in ["/help", "help", "?", "/?", "HELP", " help "] {
            assert_eq!(find(spelling).map(|c| c.name), Some("/help"), "spelling: {spelling:?}");
        }
        assert_eq!(find("tell").map(|c| c.name), Some("/tell"));
        assert_eq!(find("/TELL").map(|c| c.name), Some("/tell"));
        assert!(find("nonesuch").is_none());
    }

    #[test]
    fn bare_help_lists_every_command_with_its_format() {
        let page = help_all();
        for c in COMMANDS {
            assert!(page.contains(c.format), "help page is missing `{}`", c.format);
            assert!(page.contains(c.short), "help page is missing the short line of {}", c.name);
        }
    }

    #[test]
    fn detailed_help_is_format_short_and_long() {
        let one = help_one("stop").expect("/stop is a command");
        assert!(one.contains("/stop <id>"), "missing the format: {one}");
        assert!(one.contains("finish the current tool"), "missing the short line: {one}");
        assert!(one.contains("tool error"), "missing the long text: {one}");
        // The long text is wrapped, not one endless line.
        assert!(one.lines().count() >= 4, "long help should wrap: {one}");
        assert!(one.lines().all(|l| l.chars().count() <= 80), "wrapped too wide: {one}");
        assert!(help_one("nonesuch").is_none());
    }

    #[test]
    fn an_unknown_name_gets_the_names_not_the_page() {
        let msg = unknown_command("/tel");
        assert!(msg.contains("/tel"), "must name what was not recognized: {msg}");
        assert!(msg.contains("/tell"), "must list the real names: {msg}");
        // The full page is what we are deliberately NOT dumping.
        assert!(!msg.contains("give a subagent something"), "must not dump the page: {msg}");
    }
}
