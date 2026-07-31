//! Driving nadia's subagents from the Telegram bot.
//!
//! No second bot and no second access list: this maps the bot's existing commands onto
//! `nadia serve`'s HTTP protocol and gates them with the roster that already governs the
//! assistant in this chat. A user who may not run shell commands here may not start an
//! agent that runs them either — that is the same authority, expressed once.
//!
//! `nadia serve` is started **on demand** rather than run as a service: the first command
//! that needs it brings it up on loopback and every later one reuses it, so an operator
//! who never spawns an agent never pays for the process. The lifetime is deliberate too —
//! subagents live inside that process, so `/status` and `/pause` only mean anything while
//! it is up, and a service that restarts under them would silently lose their work.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::messenger_acl::Caps;

/// Where `nadia serve` listens. Loopback only: the surface starts processes, and nadia
/// itself refuses to bind anything else without a token.
const PORT: u16 = 8790;

fn base() -> String {
    format!("http://127.0.0.1:{PORT}")
}

/// What a command needs before it is allowed to run.
///
/// Spawning and steering an agent is `write` + `shell`, because that is exactly what the
/// agent will do on the caller's behalf. Looking is `chat`. Granting `/spawn` to someone
/// with neither would be a lie about what the grant means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Need {
    Look,
    Drive,
}

impl Need {
    pub fn satisfied_by(self, c: Caps) -> bool {
        match self {
            Need::Look => c.chat || c.read,
            Need::Drive => c.write && c.shell,
        }
    }

    fn refusal(self) -> &'static str {
        match self {
            Need::Look => "Нужен доступ к этому чату. Попроси владельца: /grant <id> chat",
            Need::Drive => {
                "Запускать и вести агентов может тот, кому разрешены и запись, и команды — \
                 это ровно то, что агент будет делать от твоего имени. \
                 Попроси владельца: /grant <id> write shell"
            }
        }
    }
}

/// One parsed nadia command, before any of it is executed.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Cmd {
    Spawn(String),
    List,
    Status(u64),
    Tell(u64, String),
    Pause(u64),
    Resume(u64),
    Stop(u64),
    Kill(u64),
}

impl Cmd {
    pub fn need(&self) -> Need {
        match self {
            Cmd::List | Cmd::Status(_) => Need::Look,
            _ => Need::Drive,
        }
    }
}

/// Recognize a nadia command. Returns `None` for anything that is not one, so the bot's
/// existing dispatcher keeps handling everything else unchanged.
pub fn parse(text: &str) -> Option<Result<Cmd, String>> {
    let mut parts = text.trim().splitn(2, char::is_whitespace);
    let head = parts.next().unwrap_or("");
    // Group commands carry a `@BotName` suffix.
    let cmd = head.split('@').next().unwrap_or("").to_ascii_lowercase();
    let rest = parts.next().unwrap_or("").trim().to_string();

    let id = |rest: &str| -> Result<u64, String> {
        rest.split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(|| format!("Нужен номер агента: {cmd} <id>"))
    };

    Some(match cmd.as_str() {
        "/spawn" | "/agent" => {
            if rest.is_empty() {
                Err("Что делать агенту? /spawn <задача>".to_string())
            } else {
                Ok(Cmd::Spawn(rest))
            }
        }
        "/agents" => Ok(Cmd::List),
        "/status" => id(&rest).map(Cmd::Status),
        "/pause" => id(&rest).map(Cmd::Pause),
        "/resume" => id(&rest).map(Cmd::Resume),
        "/stop" => id(&rest).map(Cmd::Stop),
        "/kill" => id(&rest).map(Cmd::Kill),
        "/tell" => {
            let mut it = rest.splitn(2, char::is_whitespace);
            match (it.next().and_then(|s| s.parse::<u64>().ok()), it.next()) {
                (Some(i), Some(msg)) if !msg.trim().is_empty() => {
                    Ok(Cmd::Tell(i, msg.trim().to_string()))
                }
                _ => Err("Использование: /tell <id> <сообщение>".to_string()),
            }
        }
        _ => return None,
    })
}

/// Run a parsed command, having checked the caller may. Returns the reply text.
pub fn handle(cmd: Cmd, caps: Caps) -> String {
    let need = cmd.need();
    if !need.satisfied_by(caps) {
        return need.refusal().to_string();
    }
    if let Err(e) = ensure_running() {
        return format!("Не смог поднять nadia: {e}");
    }
    match request(&cmd) {
        Ok(body) => render(&cmd, &body),
        Err(e) => format!("nadia: {e}"),
    }
}

/// Bring `nadia serve` up if it is not answering, and wait until it is.
///
/// Idempotent and cheap when it is already running: one loopback GET. The spawn is
/// detached so the bridge is not its parent's lifetime — an agent started from a phone
/// should outlive the message that started it.
fn ensure_running() -> Result<(), String> {
    if health().is_ok() {
        return Ok(());
    }
    let workspace = std::env::var("NADIA_WORKSPACE")
        .unwrap_or_else(|_| default_workspace().to_string_lossy().into_owned());
    std::fs::create_dir_all(&workspace).map_err(|e| format!("workspace {workspace}: {e}"))?;

    let mut cmd = Command::new("nadia");
    cmd.arg("serve")
        .arg("--port")
        .arg(PORT.to_string())
        .arg("--workspace")
        .arg(&workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Ok(gw) = std::env::var("ROZUM_GATEWAY_URL") {
        cmd.arg("--gateway").arg(gw);
    }
    if let Ok(model) = std::env::var("NADIA_MODEL") {
        cmd.arg("--model").arg(model);
    }
    cmd.spawn().map_err(|e| format!("`nadia serve` не запустился ({e}); установлен ли бинарник?"))?;

    // Poll rather than sleep a fixed amount: the process is up in well under a second on
    // a warm page cache and a fixed sleep would be either flaky or slow.
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if health().is_ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Err("не отвечает после запуска".into())
}

fn default_workspace() -> std::path::PathBuf {
    dirs_home().join(".rozum").join("nadia")
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

fn health() -> Result<(), String> {
    curl_json("GET", &format!("{}/health", base()), None).map(|_| ())
}

fn request(cmd: &Cmd) -> Result<serde_json::Value, String> {
    let b = base();
    match cmd {
        Cmd::Spawn(task) => {
            curl_json("POST", &format!("{b}/agents"), Some(serde_json::json!({"task": task})))
        }
        Cmd::List => curl_json("GET", &format!("{b}/agents"), None),
        Cmd::Status(i) => curl_json("GET", &format!("{b}/agents/{i}"), None),
        Cmd::Tell(i, m) => curl_json(
            "POST",
            &format!("{b}/agents/{i}/tell"),
            Some(serde_json::json!({"message": m})),
        ),
        Cmd::Pause(i) => curl_json("POST", &format!("{b}/agents/{i}/pause"), None),
        Cmd::Resume(i) => curl_json("POST", &format!("{b}/agents/{i}/resume"), None),
        Cmd::Stop(i) => curl_json("POST", &format!("{b}/agents/{i}/stop"), None),
        Cmd::Kill(i) => curl_json("DELETE", &format!("{b}/agents/{i}"), None),
    }
}

/// Loopback HTTP through `curl`. The bridge has no HTTP client of its own and this crate
/// deliberately stays free of one — the whole surface is four verbs against 127.0.0.1.
fn curl_json(method: &str, url: &str, body: Option<serde_json::Value>) -> Result<serde_json::Value, String> {
    let mut c = Command::new("curl");
    c.arg("-s").arg("-m").arg("15").arg("-X").arg(method).arg(url);
    if let Some(b) = body {
        c.arg("-H").arg("content-type: application/json").arg("-d").arg(b.to_string());
    }
    let out = c.output().map_err(|e| format!("curl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        return Err("пустой ответ".into());
    }
    serde_json::from_str(&text).map_err(|_| format!("непонятный ответ: {}", text.trim()))
}

/// Turn a JSON reply into something worth reading on a phone.
fn render(cmd: &Cmd, body: &serde_json::Value) -> String {
    if let Some(e) = body.get("error").and_then(|v| v.as_str()) {
        return format!("nadia: {e}");
    }
    match cmd {
        Cmd::Spawn(task) => format!(
            "🤖 агент #{} пошёл работать\n{}",
            body.get("id").and_then(|v| v.as_u64()).unwrap_or(0),
            clip(task, 120)
        ),
        Cmd::List => {
            let agents = body.get("agents").and_then(|v| v.as_array()).cloned().unwrap_or_default();
            if agents.is_empty() {
                return "агентов нет".to_string();
            }
            agents.iter().map(one_line).collect::<Vec<_>>().join("\n")
        }
        Cmd::Status(_) => {
            let mut s = one_line(body);
            if let Some(r) = body.get("result").and_then(|v| v.as_str()) {
                if !r.is_empty() {
                    s.push_str(&format!("\n\n{}", clip(r, 1500)));
                }
            }
            s
        }
        Cmd::Tell(i, _) => format!("сказал агенту #{i} — возьмёт следующим ходом"),
        Cmd::Pause(i) => format!("агент #{i} на паузе"),
        Cmd::Resume(i) => format!("агент #{i} продолжает"),
        Cmd::Stop(i) => format!("агент #{i} закончит текущий вызов и подведёт итог"),
        Cmd::Kill(i) => format!("агент #{i} убит, ресурсы освобождены"),
    }
}

fn one_line(a: &serde_json::Value) -> String {
    let get = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let num = |k: &str| a.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let tool = match a.get("last_tool").and_then(|v| v.as_str()) {
        Some(t) => format!(" [{t}]"),
        None => String::new(),
    };
    format!(
        "#{} {} · {} вызовов · {}с{}\n{}",
        num("id"),
        get("phase"),
        num("tool_calls"),
        num("elapsed_secs"),
        tool,
        clip(get("task"), 120)
    )
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

/// The lines to add to the bot's `/help`.
pub const HELP: &str = "\n\
Агенты (нужны права write+shell):\n\
/spawn <задача> — запустить агента\n\
/agents — кто чем занят\n\
/status <id> — один агент и его результат\n\
/tell <id> <текст> — дать ему следующий ход\n\
/pause <id> · /resume <id>\n\
/stop <id> — доделать текущий вызов и подвести итог\n\
/kill <id> — убить сейчас и освободить ресурсы";

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(chat: bool, read: bool, write: bool, shell: bool) -> Caps {
        Caps { chat, read, write, shell }
    }

    #[test]
    fn only_nadia_commands_are_claimed() {
        assert!(parse("/help").is_none(), "must not swallow the bot's own commands");
        assert!(parse("/grant 1 all").is_none());
        assert!(parse("just a message").is_none());
        assert!(parse("/agents").is_some());
    }

    #[test]
    fn commands_parse_including_the_group_suffix() {
        assert_eq!(parse("/agents@rozum_bot").unwrap(), Ok(Cmd::List));
        assert_eq!(parse("/spawn fix the test").unwrap(), Ok(Cmd::Spawn("fix the test".into())));
        assert_eq!(parse("/status 3").unwrap(), Ok(Cmd::Status(3)));
        assert_eq!(parse("/tell 3 keep going").unwrap(), Ok(Cmd::Tell(3, "keep going".into())));
        assert_eq!(parse("/kill 9").unwrap(), Ok(Cmd::Kill(9)));
    }

    #[test]
    fn a_malformed_command_explains_itself_rather_than_failing_silently() {
        assert!(parse("/spawn").unwrap().unwrap_err().contains("<задача>"));
        assert!(parse("/status").unwrap().unwrap_err().contains("<id>"));
        assert!(parse("/tell 3").unwrap().unwrap_err().contains("<сообщение>"));
        assert!(parse("/status abc").unwrap().unwrap_err().contains("<id>"));
    }

    #[test]
    fn driving_an_agent_needs_exactly_what_the_agent_will_do() {
        // The grant has to mean something: an agent writes files and runs commands, so
        // starting one requires both. Anything less would hand out capability the
        // roster says the user does not have.
        assert!(!Need::Drive.satisfied_by(caps(true, true, false, false)));
        assert!(!Need::Drive.satisfied_by(caps(true, true, true, false)), "write alone is not enough");
        assert!(!Need::Drive.satisfied_by(caps(true, true, false, true)), "shell alone is not enough");
        assert!(Need::Drive.satisfied_by(caps(false, false, true, true)));
    }

    #[test]
    fn looking_is_open_to_anyone_in_the_chat() {
        assert!(Need::Look.satisfied_by(caps(true, false, false, false)));
        assert!(Need::Look.satisfied_by(caps(false, true, false, false)));
        assert!(!Need::Look.satisfied_by(caps(false, false, false, false)));
    }

    #[test]
    fn a_refused_command_says_which_grant_is_missing() {
        let reply = handle(Cmd::Spawn("x".into()), caps(true, true, false, false));
        assert!(reply.contains("/grant"), "a refusal must be actionable: {reply}");
        assert!(reply.contains("write shell"));
    }

    #[test]
    fn an_error_body_is_shown_rather_than_a_success_line() {
        let body = serde_json::json!({"error": "no agent 7"});
        assert!(render(&Cmd::Status(7), &body).contains("no agent 7"));
    }

    #[test]
    fn a_listing_reads_on_a_phone() {
        let body = serde_json::json!({"agents": [
            {"id": 1, "phase": "running", "tool_calls": 4, "elapsed_secs": 12,
             "last_tool": "bash", "task": "fix the flaky test"}
        ]});
        let s = render(&Cmd::List, &body);
        assert!(s.contains("#1 running"), "{s}");
        assert!(s.contains("[bash]"), "{s}");
        assert!(s.contains("fix the flaky test"), "{s}");
    }
}
