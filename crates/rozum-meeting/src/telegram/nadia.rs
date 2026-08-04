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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

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
    /// The projects this machine knows, with the one this chat is working in marked.
    Projects,
    /// Work here from now on. `None` prints what is set instead of changing it.
    Project(Option<String>),
    /// Plain text goes to nadia instead of to the chat model. `None` reports the setting.
    Dialog(Option<bool>),
}

impl Cmd {
    pub fn need(&self) -> Need {
        match self {
            Cmd::List | Cmd::Status(_) | Cmd::Projects | Cmd::Project(None) | Cmd::Dialog(None) => {
                Need::Look
            }
            // Choosing the workspace and routing your typing into an agent both decide where
            // writes land, so they need the same grant as starting one.
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
        "/projects" => Ok(Cmd::Projects),
        "/project" => Ok(Cmd::Project((!rest.is_empty()).then_some(rest))),
        "/nadia" => match rest.to_ascii_lowercase().as_str() {
            "" => Ok(Cmd::Dialog(None)),
            "on" | "вкл" | "1" => Ok(Cmd::Dialog(Some(true))),
            "off" | "выкл" | "0" => Ok(Cmd::Dialog(Some(false))),
            other => Err(format!("Не понял `{other}`. Использование: /nadia on | off")),
        },
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

// ── Per-chat state ──────────────────────────────────────────────────────────────────────
//
// Two things have to outlive one message: WHERE this chat's agents work, and WHICH chat is
// waiting for a given agent's result. Both live in one small file rather than in memory,
// because the bridge re-execs whenever the group topology changes — and an agent whose
// result was posted to nobody because its bridge restarted is exactly the failure that makes
// a phone workflow useless.

/// What one chat has chosen.
#[derive(Clone, Default, Serialize, Deserialize)]
struct ChatState {
    /// Absolute path the agents of this chat work in. Unset → nadia's own scratch workspace.
    #[serde(default)]
    project: Option<String>,
    /// Plain text goes to nadia rather than to the chat model.
    #[serde(default)]
    dialog: bool,
}

/// One agent someone is waiting on. `task` is kept to detect an id reused by a restarted
/// `nadia serve`: ids are small integers and start again at 1, so an id alone could deliver
/// one chat's result to another.
#[derive(Clone, Serialize, Deserialize)]
struct Watch {
    chat: i64,
    task: String,
    /// WHICH BOT started it. Two bridges share this file, and in a private chat the chat id is
    /// the operator's user id — which both bots can post to. Without this the delivery is a race
    /// between two pollers, and the operator is answered by the bot they did not write to
    /// (reported live 2026-08-04, BUG-020). Entries written before this field belong to
    /// `telegram`: that is the only bridge that could have written them.
    #[serde(default = "legacy_owner")]
    bot: String,
}

fn legacy_owner() -> String {
    "telegram".to_string()
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    chats: BTreeMap<String, ChatState>,
    #[serde(default)]
    watch: BTreeMap<String, Watch>,
}

fn state_path() -> PathBuf {
    crate::meeting::rozum_state_dir().join("nadia-telegram.json")
}

/// Serializes the read-modify-write of the state file. The command handler and the watcher
/// run in the same process on different tasks; a lost write here is a lost notification.
fn state_lock() -> &'static Mutex<()> {
    static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn load_state() -> State {
    let mut s: State = std::fs::read(state_path())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    migrate(&mut s);
    s
}

/// Chat entries written before the key carried a bot belong to `telegram` — the only bridge that
/// existed when they were written. Done on read so an upgrade does not silently lose an
/// operator's `/nadia on`, and so the migration is exercised by every single test that loads.
fn migrate(s: &mut State) {
    let bare: Vec<String> =
        s.chats.keys().filter(|k| !k.contains(':')).cloned().collect();
    for k in bare {
        if let Some(v) = s.chats.remove(&k) {
            s.chats.entry(format!("telegram:{k}")).or_insert(v);
        }
    }
}

fn save_state(s: &State) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec_pretty(s) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, &text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

fn with_state<T>(f: impl FnOnce(&mut State) -> T) -> T {
    let _g = state_lock().lock().unwrap_or_else(|e| e.into_inner());
    let mut s = load_state();
    let out = f(&mut s);
    save_state(&s);
    out
}

/// The key a chat's choices are stored under. Per BOT, for the same reason `Watch` carries one:
/// `/nadia on` in one bot must not turn the other bot's plain messages into agent tasks for the
/// same person, and `/project` in one must not silently move the other's workspace.
fn chat_key(chat_id: i64) -> String {
    format!("{}:{}", super::registry_name(), chat_id)
}

fn chat_state(chat_id: i64) -> ChatState {
    load_state().chats.get(&chat_key(chat_id)).cloned().unwrap_or_default()
}

/// Is this chat routing plain text to nadia? Read by the bridge before it hands a message to
/// the room, so the check has to be cheap and to fail closed (a missing file = off).
pub fn dialog_on(chat_id: i64) -> bool {
    chat_state(chat_id).dialog
}

// ── Projects ────────────────────────────────────────────────────────────────────────────

/// The projects this machine knows: the meeting daemon's registered rooms plus whatever the
/// UCC's "create" button added. The same two sources the UCC's project picker reads, so the
/// phone and the web console offer the same list — read here rather than asked for over
/// HTTP, because that endpoint needs a session cookie this process does not have.
fn known_projects() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for room in crate::meeting::list_registered(&crate::meeting::rozum_state_dir()) {
        let Some(project) = room.project else { continue };
        let path = project.to_string_lossy().to_string();
        if path.is_empty() || path.contains("/tmp/") || path.contains("/.worktrees/") {
            continue;
        }
        if !out.iter().any(|(_, p)| p == &path) {
            out.push((room.name, path));
        }
    }
    let extras = dirs_home().join(".rozum/ucc/projects.json");
    if let Ok(bytes) = std::fs::read(extras) {
        if let Ok(list) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes) {
            for e in &list {
                let (Some(name), Some(path)) = (
                    e.get("name").and_then(|v| v.as_str()),
                    e.get("path").and_then(|v| v.as_str()),
                ) else {
                    continue;
                };
                if !out.iter().any(|(_, p)| p == path) {
                    out.push((name.to_string(), path.to_string()));
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Resolve what the operator typed: a project name, or a path. A path is accepted as typed
/// (`~` expanded) so a project that was never registered is still reachable from the phone.
fn resolve_project(arg: &str) -> Result<(String, String), String> {
    let arg = arg.trim();
    let projects = known_projects();
    if let Some((n, p)) = projects.iter().find(|(n, _)| n.eq_ignore_ascii_case(arg)) {
        return Ok((n.clone(), p.clone()));
    }
    let expanded = if let Some(rest) = arg.strip_prefix("~/") {
        dirs_home().join(rest).to_string_lossy().to_string()
    } else {
        arg.to_string()
    };
    if std::path::Path::new(&expanded).is_dir() {
        let name = std::path::Path::new(&expanded)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| expanded.clone());
        return Ok((name, expanded));
    }
    let names: Vec<&str> = projects.iter().map(|(n, _)| n.as_str()).collect();
    Err(if names.is_empty() {
        format!("Не нашёл проект `{arg}` — и зарегистрированных проектов нет. Укажи путь.")
    } else {
        format!("Не нашёл проект `{arg}`. Есть: {}", names.join(", "))
    })
}

/// Run a parsed command, having checked the caller may. Returns the reply text.
pub fn handle(cmd: Cmd, caps: Caps, chat_id: i64) -> String {
    let need = cmd.need();
    if !need.satisfied_by(caps) {
        return need.refusal().to_string();
    }
    // Answered from disk: these three never need the agent process, and starting it to answer
    // "where am I working" would be a surprising cost.
    match &cmd {
        Cmd::Projects => return render_projects(chat_id),
        Cmd::Project(None) => return render_project(chat_id),
        Cmd::Project(Some(arg)) => return set_project(chat_id, arg),
        Cmd::Dialog(v) => return set_dialog(chat_id, *v),
        _ => {}
    }
    if let Err(e) = ensure_running() {
        return format!("Не смог поднять nadia: {e}");
    }
    match request(&cmd, chat_id) {
        Ok(body) => {
            // Remember who is waiting for this one, so its result can be delivered instead of
            // polled for. Recorded here — where the id and the chat are both known — rather
            // than in the watcher, which only ever sees ids.
            if let Cmd::Spawn(task) = &cmd {
                if let Some(id) = body.get("id").and_then(|v| v.as_u64()) {
                    with_state(|s| {
                        s.watch.insert(
                            id.to_string(),
                            Watch {
                                chat: chat_id,
                                task: task.clone(),
                                bot: super::registry_name(),
                            },
                        );
                    });
                }
            }
            render(&cmd, &body, chat_id)
        }
        Err(e) => format!("nadia: {e}"),
    }
}

fn render_projects(chat_id: i64) -> String {
    let current = chat_state(chat_id).project;
    let projects = known_projects();
    if projects.is_empty() {
        return format!(
            "Проектов не зарегистрировано. Можно указать путь: /project ~/work/my/rozum\n\
             Сейчас: {}",
            current.unwrap_or_else(|| default_workspace().to_string_lossy().into_owned())
        );
    }
    let mut lines = vec!["📁 Проекты (/project <имя>):".to_string()];
    for (name, path) in projects {
        let mark = if current.as_deref() == Some(path.as_str()) { "→ " } else { "  " };
        lines.push(format!("{mark}{name} — {path}"));
    }
    lines.join("\n")
}

fn render_project(chat_id: i64) -> String {
    match chat_state(chat_id).project {
        Some(p) => format!("Агенты этого чата работают в {p}"),
        None => format!(
            "Проект не выбран — агенты работают в {} (личная песочница nadia).\n\
             /projects — список, /project <имя> — выбрать.",
            default_workspace().display()
        ),
    }
}

fn set_project(chat_id: i64, arg: &str) -> String {
    match resolve_project(arg) {
        Ok((name, path)) => {
            with_state(|s| {
                s.chats.entry(chat_key(chat_id)).or_default().project = Some(path.clone());
            });
            format!(
                "📁 {name} — агенты этого чата теперь работают в {path}\n\
                 Уже запущенные остаются там, где начали."
            )
        }
        Err(e) => e,
    }
}

fn set_dialog(chat_id: i64, v: Option<bool>) -> String {
    let Some(on) = v else {
        return if dialog_on(chat_id) {
            "Режим nadia включён: обычный текст идёт агенту. /nadia off — обратно к ассистенту."
                .to_string()
        } else {
            "Режим nadia выключен: обычный текст идёт ассистенту. /nadia on — переключить."
                .to_string()
        };
    };
    with_state(|s| s.chats.entry(chat_key(chat_id)).or_default().dialog = on);
    if on {
        let where_ = chat_state(chat_id)
            .project
            .unwrap_or_else(|| default_workspace().to_string_lossy().into_owned());
        format!(
            "🤖 Режим nadia включён — пиши задачу обычным текстом.\n\
             Работает в {where_}. Пока агент занят, следующее сообщение уйдёт ЕМУ \
             (как /tell), а не запустит второго.\n\
             /nadia off — вернуть обычный чат с ассистентом."
        )
    } else {
        "Режим nadia выключен — обычный текст снова идёт ассистенту.".to_string()
    }
}

/// Plain text in dialog mode: steer the agent that is already working, or start one.
///
/// Continuing beats starting a second agent on the same workspace: two agents editing one
/// tree collide, and on a phone the second one is almost always a follow-up to the first,
/// not a new job. `/spawn` stays the way to say "no, a separate one".
pub fn handle_text(chat_id: i64, text: &str, caps: Caps) -> String {
    if !Need::Drive.satisfied_by(caps) {
        return Need::Drive.refusal().to_string();
    }
    if let Err(e) = ensure_running() {
        return format!("Не смог поднять nadia: {e}");
    }
    match running_agents(chat_id).as_slice() {
        [] => handle(Cmd::Spawn(text.to_string()), caps, chat_id),
        [id] => handle(Cmd::Tell(*id, text.to_string()), caps, chat_id),
        // Two agents are working and this text could be for either. Guessing would hand a
        // steering message to the wrong one, which is worse than one extra tap.
        many => format!(
            "Работают {} агентов ({}). Кому это? /tell <id> <текст>, \
             или /spawn <задача> для нового.",
            many.len(),
            many.iter().map(|i| format!("#{i}")).collect::<Vec<_>>().join(" ")
        ),
    }
}

/// This chat's agents that are still working — running or parked.
fn running_agents(chat_id: i64) -> Vec<u64> {
    let watched = load_state().watch;
    let Ok(body) = curl_json("GET", &format!("{}/agents", base()), None) else {
        return Vec::new();
    };
    let Some(agents) = body.get("agents").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    agents
        .iter()
        .filter(|a| {
            let phase = a.get("phase").and_then(|v| v.as_str()).unwrap_or("");
            let id = a.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let mine = watched.get(&id.to_string()).is_some_and(|w| w.chat == chat_id);
            mine && matches!(phase, "running" | "paused")
        })
        .filter_map(|a| a.get("id").and_then(|v| v.as_u64()))
        .collect()
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

    let gateway = resolve_gateway().ok_or_else(|| {
        format!(
            "не нашёл живой гейтвей (пробовал {}). Модель не поднята — запусти её \
             (`rozum gateway --model …` или сервис com.rozum.gateway) и повтори.",
            gateway_candidates().join(", ")
        )
    })?;
    let mut cmd = Command::new("nadia");
    cmd.arg("serve")
        .arg("--port")
        .arg(PORT.to_string())
        .arg("--workspace")
        .arg(&workspace)
        .arg("--gateway")
        .arg(&gateway)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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

/// nadia's own scratch workspace when the chat has not chosen a project: `~/.nadia`.
///
/// Its own directory, not a corner of `~/.rozum`. What accumulates here is the operator's
/// work — whole projects an agent wrote — and it does not belong under a directory whose
/// other contents (`bin/`, `secrets/`, `ucc/`) are rozum's runtime and get treated as
/// disposable. `$NADIA_WORKSPACE` still overrides it.
fn default_workspace() -> std::path::PathBuf {
    dirs_home().join(".nadia")
}

/// Where the model might be, in the order worth trying.
///
/// nadia's own default is :8080, and this machine's durable gateway is on :8089 — so a
/// `nadia serve` started without `--gateway` talked to a port nothing listens on. Every
/// agent then died in about a second with `Phase::Failed`, no tool calls and (before the
/// fix in `supervisor.rs`) an empty reason, which read from a phone as "the agent is
/// broken". It was a wrong port. Seen live 2026-08-01: three agents, three failures, one
/// missing environment variable in a launchd plist.
fn gateway_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(v) = std::env::var("ROZUM_GATEWAY_URL") {
        let v = v.trim().trim_end_matches('/').trim_end_matches("/v1").to_string();
        if !v.is_empty() {
            out.push(v);
        }
    }
    // The durable resident gateway (`com.rozum.gateway`), then nadia's own default.
    for p in ["http://127.0.0.1:8089", "http://127.0.0.1:8080"] {
        if !out.iter().any(|u| u == p) {
            out.push(p.to_string());
        }
    }
    out
}

/// The first candidate that actually answers. Checked BEFORE starting the agent process,
/// because an agent pointed at a dead port fails a second later with nothing useful to say,
/// and the operator is then debugging the agent instead of the gateway.
fn resolve_gateway() -> Option<String> {
    gateway_candidates()
        .into_iter()
        .find(|base| curl_json("GET", &format!("{base}/v1/models"), None).is_ok())
}

fn dirs_home() -> std::path::PathBuf {
    std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_else(|| "/tmp".into())
}

fn health() -> Result<(), String> {
    curl_json("GET", &format!("{}/health", base()), None).map(|_| ())
}

fn request(cmd: &Cmd, chat_id: i64) -> Result<serde_json::Value, String> {
    let b = base();
    match cmd {
        Cmd::Spawn(task) => {
            // The chat's project, when it has chosen one: an agent started from a phone is
            // almost always meant for a repo, not for nadia's own scratch directory.
            let mut body = serde_json::json!({ "task": task });
            if let Some(p) = chat_state(chat_id).project {
                body["workspace"] = serde_json::json!(p);
            }
            curl_json("POST", &format!("{b}/agents"), Some(body))
        }
        // Handled before any request is made.
        Cmd::Projects | Cmd::Project(_) | Cmd::Dialog(_) => Ok(serde_json::json!({})),
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
fn render(cmd: &Cmd, body: &serde_json::Value, chat_id: i64) -> String {
    if let Some(e) = body.get("error").and_then(|v| v.as_str()) {
        return format!("nadia: {e}");
    }
    match cmd {
        // Say WHERE, always. An agent that writes files somewhere the operator is not
        // looking has, from their side, done nothing — which is exactly how this read from a
        // phone: a whole working Rust project built in nadia's own sandbox while the answer
        // in the chat said only "агент #3 пошёл работать" (seen live 2026-08-01).
        Cmd::Spawn(task) => {
            let id = body.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let mut s = format!("🤖 агент #{id} пошёл работать\n{}", clip(task, 120));
            match chat_state(chat_id).project {
                Some(p) => s.push_str(&format!("\n📁 {p}")),
                None => s.push_str(&format!(
                    "\n📁 {} — это личная песочница nadia, а не твой репозиторий.\n\
                     /projects · /project <имя> — работать в проекте",
                    default_workspace().display()
                )),
            }
            s
        }
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
        Cmd::Projects | Cmd::Project(_) | Cmd::Dialog(_) => String::new(), // answered earlier
        Cmd::Tell(i, _) => format!("сказал агенту #{i} — возьмёт следующим ходом"),
        Cmd::Pause(i) => format!("агент #{i} на паузе"),
        Cmd::Resume(i) => format!("агент #{i} продолжает"),
        Cmd::Stop(i) => format!("агент #{i} закончит текущий вызов и подведёт итог"),
        Cmd::Kill(i) => format!("агент #{i} убит, ресурсы освобождены"),
    }
}

/// The gate's verdict for a finished agent, in one line.
fn verdict_line(a: &serde_json::Value) -> String {
    let check = a.get("check").and_then(|v| v.as_str()).unwrap_or("");
    let repairs = a.get("repairs").and_then(|v| v.as_u64()).unwrap_or(0);
    match a.get("checked").and_then(|v| v.as_bool()) {
        Some(true) if check.is_empty() => "✔ судья-модель подтвердила результат".to_string(),
        Some(true) => {
            let extra = if repairs > 0 { format!(" (после {repairs} раунд(ов) починки)") } else { String::new() };
            format!("✔ проверка прошла: {}{extra}", clip(check, 200))
        }
        Some(false) => {
            let detail = a.get("check_detail").and_then(|v| v.as_str()).unwrap_or("");
            let head = if check.is_empty() {
                "✘ судья-модель отклонила результат".to_string()
            } else {
                format!("✘ проверка НЕ прошла: {}", clip(check, 200))
            };
            if detail.is_empty() { head } else { format!("{head}\n{}", clip(detail, 700)) }
        }
        None => "⚠ не проверено — у задачи не было машинно-проверяемого критерия".to_string(),
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
Агенты nadia (нужны права write+shell):\n\
/nadia on — писать задачи обычным текстом (off — обратно к ассистенту)\n\
/projects · /project <имя> — где агенты работают\n\
/spawn <задача> — запустить агента\n\
/agents — кто чем занят\n\
/status <id> — один агент и его результат\n\
/tell <id> <текст> — дать ему следующий ход\n\
/pause <id> · /resume <id>\n\
/stop <id> — доделать текущий вызов и подвести итог\n\
/kill <id> — убить сейчас и освободить ресурсы\n\
Итог агента приходит сам, как только он закончит — /status спрашивать не нужно.";

/// The nadia entries for the bot's command menu (`setMyCommands`), so they are offered when
/// you type `/` instead of living only in `/help` — which is the difference between a
/// feature an operator uses from a phone and one they have to remember exists.
pub const MENU: &[(&str, &str)] = &[
    ("nadia", "Режим агента: /nadia on | off"),
    ("spawn", "Запустить агента: /spawn <задача>"),
    ("agents", "Кто чем занят"),
    ("status", "Агент и его результат: /status <id>"),
    ("tell", "Дать агенту ход: /tell <id> <текст>"),
    ("stop", "Доделать и подвести итог: /stop <id>"),
    ("projects", "Проекты, где могут работать агенты"),
    ("project", "Выбрать проект: /project <имя>"),
];

// ── Delivering results ──────────────────────────────────────────────────────────────────

/// Watch the agents this bot started and post each one's result into the chat that started
/// it, once, when it reaches a terminal phase.
///
/// This is what makes the bot usable from a phone. Without it the protocol is complete but
/// the workflow is not: you would start an agent and then poll `/status 3` until it changed,
/// which is a job for a machine and is exactly the machine you are talking to.
///
/// Runs while the bridge runs. Everything about it is best-effort: a poll that fails is
/// retried on the next tick, and a chat that cannot be posted to is logged, not retried
/// forever.
pub async fn watch_results(bot: std::sync::Arc<super::bot::TelegramBot>) {
    // Which bot this bridge is. Read once: the answer cannot change under a running process, and
    // the whole point of the field is that the OTHER bridge's watches are not ours to deliver.
    let me = super::registry_name();
    // Slow enough to be free (one loopback GET), fast enough that a finished agent does not
    // sit unreported while you look at the screen.
    const EVERY: Duration = Duration::from_secs(5);
    loop {
        tokio::time::sleep(EVERY).await;
        // Nothing to watch → do not even touch the socket. An operator who never spawns an
        // agent must not pay for `nadia serve` being probed every five seconds.
        if !load_state().watch.values().any(|w| w.bot == me) {
            continue;
        }
        let mine = me.clone();
        let finished = match tokio::task::spawn_blocking(move || collect_finished(&mine)).await {
            Ok(v) => v,
            Err(_) => continue,
        };
        for (chat, text) in finished {
            if let Err(e) = bot.send_message_to(chat, &text).await {
                eprintln!("[telegram-bridge] nadia result to chat {chat} failed: {e}");
            }
        }
    }
}

/// One poll: everything watched that has finished, rendered, and dropped from the watch list
/// so it is reported exactly once. Blocking (curl); called from `spawn_blocking`.
fn collect_finished(me: &str) -> Vec<(i64, String)> {
    let Ok(body) = curl_json("GET", &format!("{}/agents", base()), None) else {
        return Vec::new();
    };
    let agents = body.get("agents").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    with_state(|s| {
        for a in &agents {
            let Some(id) = a.get("id").and_then(|v| v.as_u64()) else { continue };
            let key = id.to_string();
            let Some(w) = s.watch.get(&key).cloned() else { continue };
            // Someone else's watch. Not ours to deliver AND not ours to drop: the bridge that
            // took the command is the one that owes the answer, and removing the entry here
            // would silently swallow it.
            if w.bot != me {
                continue;
            }
            match report_for(&w, a) {
                Report::NotYet => {}
                Report::Reused => {
                    s.watch.remove(&key);
                }
                Report::Ready(text) => {
                    s.watch.remove(&key);
                    out.push((w.chat, text));
                }
            }
        }
        // An agent that vanished entirely (serve restarted, or it was killed and reaped)
        // cannot be reported and must not be watched forever.
        let live: std::collections::HashSet<String> = agents
            .iter()
            .filter_map(|a| a.get("id").and_then(|v| v.as_u64()))
            .map(|i| i.to_string())
            .collect();
        // Only our own: another bridge's entry is its business, and dropping it would leave its
        // operator waiting for a message nobody will now send.
        s.watch.retain(|k, w| w.bot != me || live.contains(k));
    });
    out
}

/// What the watcher should do about one watched agent this tick.
#[derive(Debug, PartialEq, Eq)]
enum Report {
    /// Still working (or parked) — leave it on the list.
    NotYet,
    /// This id is not the agent we were watching: `nadia serve` restarted and handed the same
    /// small integer to different work. Drop it silently. Reporting one chat's result into
    /// another chat is worse than reporting none, and a wrong result reads as a real one.
    Reused,
    /// Finished — post this and stop watching.
    Ready(String),
}

fn report_for(w: &Watch, a: &serde_json::Value) -> Report {
    let phase = a.get("phase").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(phase, "done" | "failed" | "killed") {
        return Report::NotYet;
    }
    let task = a.get("task").and_then(|v| v.as_str()).unwrap_or("");
    if !task.is_empty() && !w.task.is_empty() && task != w.task {
        return Report::Reused;
    }
    let id = a.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    Report::Ready(render_finished(id, phase, a))
}

fn render_finished(id: u64, phase: &str, a: &serde_json::Value) -> String {
    let mark = match phase {
        "done" => "✅",
        "failed" => "❌",
        _ => "⛔",
    };
    let get = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let num = |k: &str| a.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let mut s = format!(
        "{mark} агент #{id} {phase} · {} вызовов · {}с\n{}",
        num("tool_calls"),
        num("elapsed_secs"),
        clip(get("task"), 200)
    );
    // Where it worked and what it wrote. Both come from the dispatch path, not from the
    // model's summary: a model that has lost the thread reports files it never touched, and
    // an operator who cannot find the files concludes nothing happened at all.
    let workspace = get("workspace");
    if !workspace.is_empty() {
        s.push_str(&format!("\n📁 {workspace}"));
    }
    let touched: Vec<&str> =
        a.get("touched").and_then(|v| v.as_array()).map(|v| v.iter().filter_map(|f| f.as_str()).collect()).unwrap_or_default();
    if !touched.is_empty() {
        s.push_str(&format!("\n✍️ {}", touched.join(" · ")));
    } else if phase == "done" {
        s.push_str("\n✍️ файлы не менялись — это был ответ, а не работа");
    }
    // The verdict of the verify gate. This is the line that separates "the agent says it is
    // done" from "a command that either passes or does not says it is done" — and a run that
    // could not be checked SAYS so rather than looking like a pass.
    s.push_str(&format!("\n{}", verdict_line(a)));
    let result = get("result");
    if !result.is_empty() {
        s.push_str(&format!("\n\n{}", clip(result, 2500)));
    } else if phase == "failed" {
        // A failure with nothing to say is the worst message this bot can send: it names the
        // agent and blames nobody. Say where to look instead of leaving a bare ❌.
        s.push_str(
            "\n\n(причина не записана — посмотри `nadia serve` и жив ли гейтвей: \
             /status покажет то же самое)",
        );
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(chat: bool, read: bool, write: bool, shell: bool) -> Caps {
        Caps { chat, read, write, shell }
    }

    /// The bug the operator hit: they wrote to one bot and were answered by the other.
    ///
    /// Both bridges are this same binary against ONE state file, and in a private chat the chat
    /// id is the operator's user id — so the wrong bot CAN post there, and it looks delivered.
    /// One test, not two, because both halves read the same process-wide environment: as
    /// separate `#[test]`s they run on different threads and race on `TELEGRAM_REGISTRY`.
    #[test]
    fn the_bot_that_took_the_command_is_the_bot_that_answers() {
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };

        // Two watches for the SAME private chat, one per bot — exactly the live situation.
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };
        with_state(|s| {
            s.watch.insert(
                "1".into(),
                Watch { chat: 1711036782, task: "t1".into(), bot: super::super::registry_name() },
            );
        });
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram-groups") };
        with_state(|s| {
            s.watch.insert(
                "2".into(),
                Watch { chat: 1711036782, task: "t2".into(), bot: super::super::registry_name() },
            );
        });

        let st = load_state();
        assert_eq!(st.watch["1"].bot, "telegram");
        assert_eq!(st.watch["2"].bot, "telegram-groups");
        // Each bridge sees exactly one thing to deliver, and it is its own.
        for (me, mine) in [("telegram", "1"), ("telegram-groups", "2")] {
            let owned: Vec<&String> = st
                .watch
                .iter()
                .filter(|(_, w)| w.bot == me)
                .map(|(k, _)| k)
                .collect();
            assert_eq!(owned, vec![mine], "{me} would have answered for the other bot");
        }

        // A chat's MODE is per bot too: `/nadia on` in one must not turn the other bot's plain
        // messages into agent tasks for the same person.
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };
        set_dialog(1711036782, Some(true));
        assert!(dialog_on(1711036782));
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram-groups") };
        assert!(!dialog_on(1711036782), "the other bot inherited a mode nobody set in it");

        // An entry written before the field existed can only have come from the first bridge,
        // so it belongs to it — an upgrade must not lose a running operator's answer.
        std::fs::write(
            state_path(),
            br#"{"chats":{"1711036782":{"project":null,"dialog":true}},
                 "watch":{"7":{"chat":1711036782,"task":"old"}}}"#,
        )
        .unwrap();
        let legacy = load_state();
        assert_eq!(legacy.watch["7"].bot, "telegram");
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };
        assert!(dialog_on(1711036782), "the migration lost a mode the operator had set");
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram-groups") };
        assert!(!dialog_on(1711036782));

        unsafe { std::env::remove_var("TELEGRAM_REGISTRY") };
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
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
        let reply = handle(Cmd::Spawn("x".into()), caps(true, true, false, false), 42);
        assert!(reply.contains("/grant"), "a refusal must be actionable: {reply}");
        assert!(reply.contains("write shell"));
    }

    #[test]
    fn the_new_verbs_parse_and_ask_for_the_right_grant() {
        assert_eq!(parse("/projects").unwrap(), Ok(Cmd::Projects));
        assert_eq!(parse("/project").unwrap(), Ok(Cmd::Project(None)));
        assert_eq!(parse("/project rozum").unwrap(), Ok(Cmd::Project(Some("rozum".into()))));
        assert_eq!(parse("/nadia").unwrap(), Ok(Cmd::Dialog(None)));
        assert_eq!(parse("/nadia on").unwrap(), Ok(Cmd::Dialog(Some(true))));
        assert_eq!(parse("/nadia@my_bot off").unwrap(), Ok(Cmd::Dialog(Some(false))));
        assert!(parse("/nadia maybe").unwrap().is_err());

        // Looking is open; anything that decides WHERE writes land needs write+shell — the
        // same grant as starting an agent, because that is what it is choosing.
        assert_eq!(Cmd::Projects.need(), Need::Look);
        assert_eq!(Cmd::Project(None).need(), Need::Look);
        assert_eq!(Cmd::Dialog(None).need(), Need::Look);
        assert_eq!(Cmd::Project(Some("x".into())).need(), Need::Drive);
        assert_eq!(Cmd::Dialog(Some(true)).need(), Need::Drive);
    }

    #[test]
    fn a_finished_agent_is_reported_once_and_never_the_wrong_one() {
        let w = Watch { chat: 7, task: "fix the test".into(), bot: legacy_owner() };
        let agent = |phase: &str, task: &str| {
            serde_json::json!({
                "id": 3, "phase": phase, "task": task,
                "tool_calls": 5, "elapsed_secs": 12, "result": "done, cargo test passes"
            })
        };
        // Still working → nothing is said and it stays watched.
        assert_eq!(report_for(&w, &agent("running", "fix the test")), Report::NotYet);
        assert_eq!(report_for(&w, &agent("paused", "fix the test")), Report::NotYet);

        // Finished → the report carries the outcome, the counts and the result text.
        let Report::Ready(text) = report_for(&w, &agent("done", "fix the test")) else {
            panic!("a finished agent must be reported");
        };
        assert!(text.contains("#3") && text.contains("done"), "{text}");
        assert!(text.contains("cargo test passes"), "the result must reach the chat: {text}");
        assert!(text.contains("fix the test"), "say WHICH task finished: {text}");

        // A failure is reported too — silence would read as "still working".
        assert!(matches!(report_for(&w, &agent("failed", "fix the test")), Report::Ready(_)));
        assert!(matches!(report_for(&w, &agent("killed", "fix the test")), Report::Ready(_)));

        // Same id, different work: `nadia serve` restarted and reused the number. Dropped,
        // never delivered — a result posted to the wrong chat reads as a real one.
        assert_eq!(report_for(&w, &agent("done", "something else entirely")), Report::Reused);
    }

    #[test]
    fn the_gateway_is_looked_for_where_this_machine_actually_runs_one() {
        // The durable resident gateway comes before nadia's own default, which is the port
        // nothing listens on here — the bug that made three agents fail in one second each.
        let c = gateway_candidates();
        let pos = |u: &str| c.iter().position(|x| x == u);
        assert!(pos("http://127.0.0.1:8089").is_some(), "{c:?}");
        assert!(pos("http://127.0.0.1:8089") < pos("http://127.0.0.1:8080"), "{c:?}");
        // No duplicates, whatever the environment says.
        let mut sorted = c.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), c.len(), "duplicate candidates: {c:?}");
    }

    #[test]
    fn a_finished_report_says_where_it_worked_and_what_it_wrote() {
        let a = serde_json::json!({
            "id": 3, "phase": "done", "task": "напиши калькулятор RPN",
            "tool_calls": 9, "elapsed_secs": 120, "result": "готово",
            "workspace": "/Users/x/.nadia",
            "touched": ["Cargo.toml", "src/main.rs"]
        });
        let text = render_finished(3, "done", &a);
        // The two facts an operator needs before they can go and look at the work.
        assert!(text.contains("/Users/x/.nadia"), "must say WHERE: {text}");
        assert!(text.contains("Cargo.toml") && text.contains("src/main.rs"), "must list what: {text}");

        // A run that only talked says so, rather than leaving the operator to search a
        // directory for files that were never written — which is exactly what happened.
        let b = serde_json::json!({
            "id": 4, "phase": "done", "task": "какие у тебя тулы?",
            "tool_calls": 0, "elapsed_secs": 3, "result": "вот список",
            "workspace": "/Users/x/.nadia", "touched": []
        });
        let text = render_finished(4, "done", &b);
        assert!(text.contains("файлы не менялись"), "{text}");
    }

    #[test]
    fn the_verdict_distinguishes_checked_from_merely_finished() {
        let with = |checked: serde_json::Value, check: &str, detail: &str, repairs: u64| {
            serde_json::json!({
                "id": 1, "phase": "done", "task": "t", "tool_calls": 3, "elapsed_secs": 9,
                "result": "ok", "workspace": "/w", "touched": ["src/main.rs"],
                "check": check, "checked": checked, "check_detail": detail, "repairs": repairs
            })
        };
        // Passed: the command itself is the evidence, and repairs are named when there were any.
        let t = render_finished(1, "done", &with(true.into(), "cargo test -q", "", 0));
        assert!(t.contains("✔ проверка прошла: cargo test -q"), "{t}");
        assert!(!t.contains("раунд"), "{t}");
        let t = render_finished(1, "done", &with(true.into(), "cargo test -q", "", 2));
        assert!(t.contains("2 раунд"), "{t}");

        // Failed: what failed AND what it printed, because that is what a person acts on.
        let t = render_finished(1, "done", &with(false.into(), "cargo test -q", "4 + 4 = 7", 2));
        assert!(t.contains("✘ проверка НЕ прошла") && t.contains("4 + 4 = 7"), "{t}");

        // Nothing checkable must never read as a pass — the whole point of the line.
        let t = render_finished(1, "done", &with(serde_json::Value::Null, "", "", 0));
        assert!(t.contains("не проверено"), "{t}");

        // The judge's verdicts are distinguishable from a deterministic check.
        let t = render_finished(1, "done", &with(true.into(), "", "", 0));
        assert!(t.contains("судья-модель подтвердила"), "{t}");
        let t = render_finished(1, "done", &with(false.into(), "", "не реализовано", 0));
        assert!(t.contains("судья-модель отклонила") && t.contains("не реализовано"), "{t}");
    }

    #[test]
    fn a_failure_with_no_reason_says_where_to_look() {
        let a = serde_json::json!({
            "id": 2, "phase": "failed", "task": "привет", "tool_calls": 0, "elapsed_secs": 1,
            "result": ""
        });
        let text = render_finished(2, "failed", &a);
        assert!(text.contains("причина не записана"), "{text}");
        // With a reason, the reason is what is shown — no boilerplate on top of it.
        let b = serde_json::json!({
            "id": 2, "phase": "failed", "task": "привет", "tool_calls": 0, "elapsed_secs": 1,
            "result": "gateway transport failed: Connection refused"
        });
        let text = render_finished(2, "failed", &b);
        assert!(text.contains("Connection refused"), "{text}");
        assert!(!text.contains("причина не записана"), "{text}");
    }

    #[test]
    fn an_error_body_is_shown_rather_than_a_success_line() {
        let body = serde_json::json!({"error": "no agent 7"});
        assert!(render(&Cmd::Status(7), &body, 42).contains("no agent 7"));
    }

    #[test]
    fn a_listing_reads_on_a_phone() {
        let body = serde_json::json!({"agents": [
            {"id": 1, "phase": "running", "tool_calls": 4, "elapsed_secs": 12,
             "last_tool": "bash", "task": "fix the flaky test"}
        ]});
        let s = render(&Cmd::List, &body, 42);
        assert!(s.contains("#1 running"), "{s}");
        assert!(s.contains("[bash]"), "{s}");
        assert!(s.contains("fix the flaky test"), "{s}");
    }
}
