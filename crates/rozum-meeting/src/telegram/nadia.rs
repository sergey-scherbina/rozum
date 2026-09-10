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
    /// How often a running agent's steps are posted. `None` reports the setting.
    Progress(Option<String>),
    /// Forget this chat's remembered past tasks.
    ClearMemory,
    /// Replace this chat's remembered past tasks with one summary an agent writes.
    Compact,
}

impl Cmd {
    pub fn need(&self) -> Need {
        match self {
            Cmd::List
            | Cmd::Status(_)
            | Cmd::Projects
            | Cmd::Project(None)
            | Cmd::Dialog(None)
            | Cmd::Progress(None) => Need::Look,
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
        "/progress" => Ok(Cmd::Progress((!rest.is_empty()).then_some(rest))),
        "/clear" => Ok(Cmd::ClearMemory),
        "/compact" => Ok(Cmd::Compact),
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
#[derive(Clone, Serialize, Deserialize)]
struct ChatState {
    /// Absolute path the agents of this chat work in. Unset → nadia's own scratch workspace.
    #[serde(default)]
    project: Option<String>,
    /// Plain text goes to nadia rather than to the chat model. `None` = never explicitly
    /// chosen — distinct from `Some(false)`, so that setting the PROJECT (which also creates
    /// this chat's entry, via `or_default`) does not silently lock in "off" and pre-empt the
    /// owner default in `dialog_routes` below.
    #[serde(default)]
    dialog: Option<bool>,
    /// How often a running agent's steps are posted: 0 = never (final result only), 1 = every
    /// step (the default — real-time was the point of the feature), N>1 = every Nth step.
    #[serde(default = "default_progress_every")]
    progress_every: u32,
    /// Past tasks in this chat, oldest first — prepended to the next fresh `/spawn` so a new
    /// agent (which starts with NO memory of its own) has some idea what came before, the way
    /// this chat's own history carries forward. Grows until `MEMORY_CHAR_BUDGET`, then drops
    /// the oldest entries rather than the newest — recent context matters more than old.
    /// `/clear` empties it, `/compact` replaces it with one summary.
    #[serde(default)]
    recent: Vec<Recent>,
}

impl Default for ChatState {
    fn default() -> Self {
        ChatState {
            project: None,
            dialog: None,
            progress_every: default_progress_every(),
            recent: Vec::new(),
        }
    }
}

fn default_progress_every() -> u32 {
    1
}

/// One remembered past task: what was asked, and what the agent reported back.
#[derive(Clone, Serialize, Deserialize)]
struct Recent {
    task: String,
    result: String,
}

/// Total chars of task+result text kept per chat. A fresh agent's own budget is tiny
/// (`default_budget` caps `max_tokens` at 4096) and this text is PREPENDED to every new task,
/// competing with the task itself for that budget — so memory is bounded generously, not
/// unboundedly, the way this chat's own history is bounded by compaction rather than by a
/// fixed message count.
const MEMORY_CHAR_BUDGET: usize = 3000;

/// Drop the OLDEST entries first once the total exceeds budget — symmetric with how this
/// chat's own history favors recent turns over old ones.
fn trim_recent(recent: &mut Vec<Recent>) {
    let total = |r: &[Recent]| -> usize {
        r.iter().map(|e| e.task.chars().count() + e.result.chars().count()).sum()
    };
    while total(recent) > MEMORY_CHAR_BUDGET && recent.len() > 1 {
        recent.remove(0);
    }
}

/// The text actually sent to nadia for a fresh `/spawn`: this chat's remembered past tasks,
/// then the new one. Unchanged when there is nothing to remember, so a chat that never
/// accumulated memory pays nothing for this.
fn with_recent_memory(chat_id: i64, task: &str) -> String {
    let recent = chat_state(chat_id).recent;
    if recent.is_empty() {
        return task.to_string();
    }
    let mut s = String::from(
        "Контекст — прошлые задачи в этом чате и их результаты (для справки, не переделывай их):\n",
    );
    for (i, r) in recent.iter().enumerate() {
        s.push_str(&format!("{}. {} → {}\n", i + 1, r.task, r.result));
    }
    s.push_str("\nНовая задача: ");
    s.push_str(task);
    s
}

/// The text of the agent's own result, for memory — its final message if it left one,
/// otherwise a phase note so a failed/killed run still leaves a trace of what was tried.
fn memory_result(a: &serde_json::Value) -> String {
    match a.get("result").and_then(|v| v.as_str()) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => {
            let phase = a.get("phase").and_then(|v| v.as_str()).unwrap_or("done");
            format!("({phase}, без итогового сообщения)")
        }
    }
}

/// One agent someone is waiting on. `task` is kept to detect an id reused by a restarted
/// `nadia serve`: ids are small integers and start again at 1, so an id alone could deliver
/// one chat's result to another.
#[derive(Clone, Serialize, Deserialize)]
struct Watch {
    chat: i64,
    /// Exactly what was POSTed as this agent's task — for a memory-augmented `/spawn` that is
    /// the augmented text, because THAT is what nadia serve echoes back in `/agents/{id}`, and
    /// this field exists to detect an id a restarted `nadia serve` reused (`task` mismatch).
    task: String,
    /// The short text the human actually typed, for the memory this task becomes once it
    /// finishes — distinct from `task` so remembering it does not compound the "Контекст —
    /// прошлые задачи…" preamble into the next preamble. `None` when `/spawn` sent `task`
    /// verbatim (nothing to strip).
    #[serde(default)]
    display_task: Option<String>,
    /// A `/compact` run: its result REPLACES this chat's memory instead of appending to it,
    /// and it is announced differently — the operator asked to shrink memory, not to run a
    /// task, so "агент #N done" would read as a task that never happened.
    #[serde(default)]
    compacting: bool,
    /// WHICH BOT started it. Two bridges share this file, and in a private chat the chat id is
    /// the operator's user id — which both bots can post to. Without this the delivery is a race
    /// between two pollers, and the operator is answered by the bot they did not write to
    /// (reported live 2026-08-04, BUG-020). Entries written before this field belong to
    /// `telegram`: that is the only bridge that could have written them.
    #[serde(default = "legacy_owner")]
    bot: String,
    /// `tool_calls` as of the last progress line posted for this agent, so a step already
    /// reported is never reported again and a poll that finds nothing new stays silent.
    #[serde(default)]
    reported_calls: u64,
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
    chat_state(chat_id).dialog.unwrap_or(false)
}

/// The routing decision the bridge actually acts on: the persisted choice if this chat ever
/// made one, otherwise ON for the owner and OFF for anyone else.
///
/// The point of the feature is to let the owner skip `/nadia on` entirely — but defaulting it
/// on for EVERY chat would intercept a `chat`-only guest's or a group member's plain messages
/// too, and `handle_text` refuses them for lacking write+shell instead of the message ever
/// reaching the ordinary assistant. So the default only fires for the sender the ACL already
/// calls owner; anyone else keeps today's fail-closed default until the owner turns it on for
/// that chat explicitly.
pub fn dialog_routes(chat_id: i64, is_owner: bool) -> bool {
    match load_state().chats.get(&chat_key(chat_id)).and_then(|cs| cs.dialog) {
        Some(v) => v,
        None => is_owner,
    }
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
        Cmd::Progress(None) => return render_progress_setting(chat_id),
        Cmd::Progress(Some(arg)) => return set_progress(chat_id, arg),
        Cmd::ClearMemory => return clear_memory(chat_id),
        Cmd::Compact => {}
        _ => {}
    }
    if matches!(cmd, Cmd::Compact) && chat_state(chat_id).recent.is_empty() {
        return "Нечего сжимать — память этого чата пуста.".to_string();
    }
    if let Err(e) = ensure_running() {
        return format!("Не смог поднять nadia: {e}");
    }
    // Chosen ONCE and used twice — by the request that starts the agent and by the message that
    // tells the operator where it is working. The first version let the two find out separately:
    // the request knew the directory, the message read it back out of a response that does not
    // carry one, and the operator was told the work was in the sandbox root while it was really
    // in its own directory. Two sources for one fact is the bug; this is the fix.
    let workspace = match spawn_workspace(&cmd, chat_id) {
        Ok(w) => w,
        Err(e) => return e,
    };
    // What is actually POSTed differs from what the operator typed for two cases: `/spawn`
    // gets this chat's remembered past tasks prepended (a fresh agent otherwise starts with
    // no memory of its own), and `/compact` becomes a spawn of its own — a summarization task
    // built from that same memory, since nadia's own model is the one already paying for
    // every other read of this chat's context.
    let sent: Cmd = match &cmd {
        Cmd::Spawn(task) => Cmd::Spawn(with_recent_memory(chat_id, task)),
        Cmd::Compact => Cmd::Spawn(compact_task(chat_id)),
        other => other.clone(),
    };
    match request(&sent, workspace.as_deref()) {
        Ok(body) => {
            // Remember who is waiting for this one, so its result can be delivered instead of
            // polled for. Recorded here — where the id and the chat are both known — rather
            // than in the watcher, which only ever sees ids.
            if let Cmd::Spawn(sent_task) = &sent {
                if let Some(id) = body.get("id").and_then(|v| v.as_u64()) {
                    let display_task = match &cmd {
                        Cmd::Spawn(original) if original != sent_task => Some(original.clone()),
                        _ => None,
                    };
                    with_state(|s| {
                        s.watch.insert(
                            id.to_string(),
                            Watch {
                                chat: chat_id,
                                task: sent_task.clone(),
                                display_task,
                                compacting: matches!(cmd, Cmd::Compact),
                                bot: super::registry_name(),
                                reported_calls: 0,
                            },
                        );
                    });
                }
            }
            render(&cmd, &body, chat_id, workspace.as_deref())
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
            "Проект не выбран — каждая задача получает свой каталог в {}/tasks/ \
             (личная песочница nadia).\n\
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
    with_state(|s| s.chats.entry(chat_key(chat_id)).or_default().dialog = Some(on));
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

fn progress_every(chat_id: i64) -> u32 {
    chat_state(chat_id).progress_every
}

fn render_progress_setting(chat_id: i64) -> String {
    match progress_every(chat_id) {
        0 => "Шаги не присылаются — только итог. /progress all — вернуть.".to_string(),
        1 => "Каждый шаг агента приходит сразу (по умолчанию). \
              /progress off — только итог, /progress <N> — раз в N шагов."
            .to_string(),
        n => format!(
            "Шаг приходит раз в {n}. /progress all — каждый, /progress off — только итог."
        ),
    }
}

fn set_progress(chat_id: i64, arg: &str) -> String {
    let arg = arg.trim().to_ascii_lowercase();
    let n = match arg.as_str() {
        "all" | "вкл" | "1" => 1,
        "off" | "выкл" | "0" => 0,
        other => match other.parse::<u32>() {
            Ok(n) if n > 0 => n,
            _ => {
                return format!(
                    "Не понял `{other}`. Использование: /progress all | off | <N> \
                     (раз в N шагов)."
                )
            }
        },
    };
    with_state(|s| s.chats.entry(chat_key(chat_id)).or_default().progress_every = n);
    match n {
        0 => "Шаги отключены — итог придёт как обычно.".to_string(),
        1 => "Теперь каждый шаг агента приходит сразу.".to_string(),
        n => format!("Теперь шаг приходит раз в {n}."),
    }
}

fn clear_memory(chat_id: i64) -> String {
    let had = !chat_state(chat_id).recent.is_empty();
    with_state(|s| {
        s.chats.entry(chat_key(chat_id)).or_default().recent.clear();
    });
    if had {
        "🧹 память очищена — следующая задача начнётся с чистого листа.".to_string()
    } else {
        "Память этого чата и так пуста.".to_string()
    }
}

/// The summarization task `/compact` sends nadia's own model — reusing it rather than
/// summarizing in Rust because it is already the one thing here that reads this chat's whole
/// history for every other purpose.
fn compact_task(chat_id: i64) -> String {
    let recent = chat_state(chat_id).recent;
    let mut s = String::from(
        "Ниже — записи о прошлых задачах в этом чате и их результатах. Сожми это в ОДНО \
         связное резюме не длиннее 500 символов — оно станет памятью для будущих задач в этом \
         чате. Верни только текст резюме, без вступления и без кавычек.\n\n",
    );
    for (i, r) in recent.iter().enumerate() {
        s.push_str(&format!("{}. {} → {}\n", i + 1, r.task, r.result));
    }
    s
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
    // Its OWN process group, so `launchctl bootout` of THIS bridge does not take the agents
    // with it. A deploy of the bridge used to kill `nadia serve` — it happened twice in one
    // evening, unnoticed, because the only symptom is that the next agent comes back as #1.
    // `health()` above means a restarted bridge reattaches to the running one instead of
    // starting a second (docs/specs/nadia-serve-lifetime.md).
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
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
/// Where a bare `/spawn` works: a FRESH directory per task, under the sandbox.
///
/// Measured live 2026-08-05, two identical `/spawn`s in a row. The second agent reported
/// "Created a Rust program … Verified: cargo run -- 3 4 outputs 7", the gate reported `✔`, and it
/// had written **nothing** — `touched` was empty. The first task's program was already in the
/// sandbox root, so the derived check passed the instant it was asked. **A check run in a
/// directory somebody else already satisfied verifies the DIRECTORY, not the run**, which is the
/// same false pass the gate exists to prevent, arriving through the one door left open. The
/// second cost is quieter: task N+1 overwrites task N's `src/main.rs` in place.
///
/// A chat that has chosen a project keeps working in it — there, reuse is the entire point.
fn task_workspace(task: &str) -> std::path::PathBuf {
    default_workspace().join("tasks").join(format!(
        "{}-{}",
        chrono::Local::now().format("%Y-%m-%d-%H%M%S"),
        slug(task)
    ))
}

/// A short, filesystem-safe hint of what the task was.
///
/// Cyrillic is transliterated rather than dropped: the tasks arrive in Russian as often as not,
/// and dropping left `~/.nadia/tasks/2026-08-05-002343-` — a directory the operator asked about
/// precisely because its name said nothing. A directory name is for finding the work again.
fn slug(task: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for c in task.chars().flat_map(translit) {
        if c.is_ascii_alphanumeric() {
            out.extend(c.to_lowercase());
            last_dash = false;
        } else if !last_dash && out.len() < 24 {
            out.push('-');
            last_dash = true;
        }
        if out.len() >= 24 {
            break;
        }
    }
    let out = out.trim_matches('-').to_string();
    // Never a bare trailing dash, and never nothing: a name that is only a timestamp is fine,
    // a name that ENDS in the separator looks like a bug and was one.
    if out.is_empty() { "task".to_string() } else { out }
}

/// One Cyrillic character as ASCII. Deliberately plain — this feeds a directory name, not a
/// transliteration standard, so `ж → zh` and `щ → sch` are close enough and reversibility is
/// not a goal. Anything else passes through and the caller decides what is safe.
fn translit(c: char) -> impl Iterator<Item = char> {
    const MAP: [(char, &str); 33] = [
        ('а', "a"), ('б', "b"), ('в', "v"), ('г', "g"), ('д', "d"), ('е', "e"), ('ё', "e"),
        ('ж', "zh"), ('з', "z"), ('и', "i"), ('й', "y"), ('к', "k"), ('л', "l"), ('м', "m"),
        ('н', "n"), ('о', "o"), ('п', "p"), ('р', "r"), ('с', "s"), ('т', "t"), ('у', "u"),
        ('ф', "f"), ('х', "h"), ('ц', "c"), ('ч', "ch"), ('ш', "sh"), ('щ', "sch"), ('ъ', ""),
        ('ы', "y"), ('ь', ""), ('э', "e"), ('ю', "yu"), ('я', "ya"),
    ];
    let lower = c.to_lowercase().next().unwrap_or(c);
    let mapped = MAP.iter().find(|(k, _)| *k == lower).map(|(_, v)| *v);
    match mapped {
        Some(v) => v.chars().collect::<Vec<_>>().into_iter(),
        None => vec![c].into_iter(),
    }
}

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
    rozum_paths::home_dir().unwrap_or_else(rozum_paths::temp_dir)
}

/// A `serve` that outlived a deploy is serving the old code. Restart it — but only when nobody
/// is working in it.
///
/// This is the hazard detaching creates, and the ordering is the point: **the operator's work
/// outranks our convenience about versions.** A stale `serve` with agents running is left alone
/// and said out loud once; the next idle moment picks the new binary up. Returns what it did, for
/// the log.
pub fn refresh_if_stale() -> Option<String> {
    let h = curl_json("GET", &format!("{}/health", base()), None).ok()?;
    let build = h.get("build")?;
    let (exe, mtime, pid) = (
        build.get("exe")?.as_str()?,
        build.get("mtime")?.as_u64()?,
        build.get("pid")?.as_u64()?,
    );
    let installed = std::fs::metadata(exe)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    if installed <= mtime {
        return None;
    }
    let busy = running_any();
    if busy {
        return Some(format!(
            "nadia serve (pid {pid}) runs a pre-deploy build of {exe}; agents are working, so it              stays until they finish"
        ));
    }
    // SIGTERM: `serve` has nothing to flush — every record is already on disk, written at the
    // moments that change it — so there is nothing to lose and nothing to wait for. Sent with
    // `kill(1)` rather than by linking libc for one call, the same way this module already
    // reaches HTTP with `curl`.
    let _ = Command::new("kill").arg("-TERM").arg(pid.to_string()).status();
    Some(format!("nadia serve (pid {pid}) ran a pre-deploy build; restarted for {exe}"))
}

/// Is any agent working right now? A failed probe answers "yes" on purpose: not knowing is not a
/// licence to kill the process.
fn running_any() -> bool {
    let Ok(body) = curl_json("GET", &format!("{}/agents", base()), None) else { return true };
    body.get("agents")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter().any(|x| {
                !matches!(
                    x.get("phase").and_then(|p| p.as_str()).unwrap_or(""),
                    "done" | "failed" | "killed" | "interrupted"
                )
            })
        })
        .unwrap_or(true)
}

fn health() -> Result<(), String> {
    curl_json("GET", &format!("{}/health", base()), None).map(|_| ())
}

/// Where THIS command's agent will work, decided before anything is started.
///
/// `None` for every command that does not start an agent. A chat with a project chosen works in
/// it; without one, a fresh directory per task (`task_workspace`), created here so the agent does
/// not spend a tool call making it — and sometimes put the project one level down, which the gate
/// then reports as a failure.
fn spawn_workspace(cmd: &Cmd, chat_id: i64) -> Result<Option<std::path::PathBuf>, String> {
    let Cmd::Spawn(task) = cmd else { return Ok(None) };
    if let Some(p) = chat_state(chat_id).project {
        return Ok(Some(std::path::PathBuf::from(p)));
    }
    let w = task_workspace(task);
    std::fs::create_dir_all(&w)
        .map_err(|e| format!("не смог создать каталог задачи {}: {e}", w.display()))?;
    Ok(Some(w))
}

/// Send one command to `nadia serve`. The workspace is decided by [`spawn_workspace`] and passed
/// in — this function does not ask the chat again, because two lookups of one fact is how the ACK
/// came to name a different directory than the agent was started in (BUG-022).
fn request(
    cmd: &Cmd,
    workspace: Option<&std::path::Path>,
) -> Result<serde_json::Value, String> {
    let b = base();
    match cmd {
        Cmd::Spawn(task) => {
            let mut body = serde_json::json!({ "task": task });
            if let Some(w) = workspace {
                body["workspace"] = serde_json::json!(w.to_string_lossy());
            }
            curl_json("POST", &format!("{b}/agents"), Some(body))
        }
        // Handled before any request is made (ClearMemory), or never reaches this function as
        // itself — `handle` turns a `Compact` into a `Spawn` before calling `request`.
        Cmd::Projects | Cmd::Project(_) | Cmd::Dialog(_) | Cmd::Progress(_) | Cmd::ClearMemory
        | Cmd::Compact => Ok(serde_json::json!({})),
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
fn render(
    cmd: &Cmd,
    body: &serde_json::Value,
    chat_id: i64,
    workspace: Option<&std::path::Path>,
) -> String {
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
            // The SAME path the agent was started with — passed in, not read back out of a
            // response that does not carry one.
            match (chat_state(chat_id).project, workspace) {
                (Some(p), _) => s.push_str(&format!("\n📁 {p}")),
                (None, Some(w)) => s.push_str(&format!(
                    "\n📁 {}\n\
                     свежий каталог под эту задачу — прошлые работы рядом, не поверх.\n\
                     /projects · /project <имя> — работать в своём проекте",
                    w.display()
                )),
                (None, None) => s.push_str(&format!(
                    "\n📁 {} — личная песочница nadia, а не твой репозиторий.\n\
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
        Cmd::Projects | Cmd::Project(_) | Cmd::Dialog(_) | Cmd::Progress(_) => String::new(), // answered earlier
        Cmd::ClearMemory => String::new(), // answered earlier, never reaches here
        Cmd::Compact => {
            let id = body.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            format!("🗜 сжимаю память этого чата (агент #{id})…")
        }
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

/// One line for a step the agent just took, posted as its own message (not edited in place) so
/// the chat keeps a full history of what it did, not just the last thing.
/// One progress line. Was `clip(&w.task, 100)` — for a memory-augmented `/spawn` that clips
/// the "Контекст — прошлые задачи…" preamble `task` now starts with, not the task itself, so
/// every step read as the same wall of text with no sign of what was actually happening. Shows
/// what the tool was pointed at instead (a path, a command, a pattern) — the answer to "what is
/// it doing RIGHT NOW", which the task text never was even before that bug.
fn render_progress(id: u64, _w: &Watch, calls: u64, a: &serde_json::Value) -> String {
    let elapsed = a.get("elapsed_secs").and_then(|v| v.as_u64()).unwrap_or(0);
    let tool = a.get("last_tool").and_then(|v| v.as_str()).unwrap_or("?");
    match a.get("last_tool_detail").and_then(|v| v.as_str()) {
        Some(detail) if !detail.is_empty() => {
            format!("⚙️ #{id} шаг {calls} [{tool}] {} · {elapsed}с", clip(detail, 200))
        }
        _ => format!("⚙️ #{id} шаг {calls} [{tool}] · {elapsed}с"),
    }
}

/// The human-readable task for `/agents` and `/status`: this id's `Watch.display_task` if it
/// is still being watched (the same fix as `render_progress` — the agent's own `task` field is
/// the memory-augmented text once `/spawn` prepended anything), else the raw field as a
/// fallback for an id nobody here is watching (delivered already, or started elsewhere).
fn task_for_display(a: &serde_json::Value) -> String {
    let watched = a
        .get("id")
        .and_then(|v| v.as_u64())
        .and_then(|id| load_state().watch.get(&id.to_string()).cloned());
    match watched {
        Some(w) => w.display_task.unwrap_or(w.task),
        None => a.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string(),
    }
}

fn one_line(a: &serde_json::Value) -> String {
    let get = |k: &str| a.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let num = |k: &str| a.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
    let tool = match (
        a.get("last_tool").and_then(|v| v.as_str()),
        a.get("last_tool_detail").and_then(|v| v.as_str()),
    ) {
        (Some(t), Some(d)) if !d.is_empty() => format!(" [{t}] {}", clip(d, 80)),
        (Some(t), _) => format!(" [{t}]"),
        (None, _) => String::new(),
    };
    format!(
        "#{} {} · {} вызовов · {}с{}\n{}",
        num("id"),
        get("phase"),
        num("tool_calls"),
        num("elapsed_secs"),
        tool,
        clip(&task_for_display(a), 120)
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
/progress all | off | <N> — как часто слать шаги (по умолчанию — каждый)\n\
/clear — забыть прошлые задачи этого чата\n\
/compact — сжать их в одну сводку\n\
Пока агент работает, шаги приходят сами (по одному сообщению); итог — тоже сам, как только он закончит — /status спрашивать не нужно. Новый агент помнит прошлые задачи этого чата — не нужно пересказывать заново.";

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
    ("progress", "Как часто слать шаги: /progress all | off | <N>"),
    ("clear", "Забыть прошлые задачи этого чата"),
    ("compact", "Сжать прошлые задачи в одну сводку"),
];

// ── Delivering results ──────────────────────────────────────────────────────────────────

/// Watch the agents this bot started and post into the chat that started each one: a line per
/// new step while it runs, and its result once when it reaches a terminal phase.
///
/// This is what makes the bot usable from a phone. Without it the protocol is complete but
/// the workflow is not: you would start an agent and then poll `/status 3` to see whether it
/// was still there, which is a job for a machine and is exactly the machine you are talking to.
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

/// One poll: a progress line for anything watched that took a new step since the last tick,
/// plus everything that has finished — rendered and dropped from the watch list so a result is
/// reported exactly once. Blocking (curl); called from `spawn_blocking`.
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
                Report::NotYet => {
                    // A step since the last tick: post it once and remember how far we got, so
                    // the next tick — whether it finds one more step or the terminal phase —
                    // never repeats it. `tool_calls` can jump by more than one between two 5s
                    // polls; that shows as one line spanning the gap rather than one per call,
                    // because `last_tool` only ever holds the most recent of them.
                    let calls = a.get("tool_calls").and_then(|v| v.as_u64()).unwrap_or(0);
                    if calls > w.reported_calls {
                        if let Some(entry) = s.watch.get_mut(&key) {
                            entry.reported_calls = calls;
                        }
                        // /progress: 0 = silent until the result, 1 (default) = every step,
                        // N>1 = every Nth. Throttled here rather than at the caller so a
                        // throttled-away step still advances `reported_calls` — otherwise the
                        // NEXT step would render as a jump from the last one actually sent.
                        let every = s
                            .chats
                            .get(&chat_key(w.chat))
                            .map(|c| c.progress_every)
                            .unwrap_or_else(default_progress_every);
                        let due = match every {
                            0 => false,
                            1 => true,
                            n => calls % n as u64 == 0,
                        };
                        if due {
                            out.push((w.chat, render_progress(id, &w, calls, a)));
                        }
                    }
                }
                Report::Reused => {
                    s.watch.remove(&key);
                }
                Report::Ready(text) => {
                    s.watch.remove(&key);
                    if w.compacting {
                        // Replace, not append: that is the whole point of asking for it.
                        let phase = a.get("phase").and_then(|v| v.as_str()).unwrap_or("done");
                        if phase == "done" {
                            let summary = memory_result(a);
                            s.chats.entry(chat_key(w.chat)).or_default().recent = vec![Recent {
                                task: "сводка предыдущих задач".to_string(),
                                result: summary.clone(),
                            }];
                            out.push((w.chat, format!("🗜 память сжата:\n{}", clip(&summary, 800))));
                        } else {
                            out.push((
                                w.chat,
                                format!(
                                    "🗜 не получилось сжать память ({phase}) — память осталась прежней."
                                ),
                            ));
                        }
                    } else {
                        // The task this becomes for the NEXT fresh agent's memory: the human's
                        // own short text, not the (possibly already memory-prefixed) one sent.
                        let display = w.display_task.clone().unwrap_or_else(|| w.task.clone());
                        let cs = s.chats.entry(chat_key(w.chat)).or_default();
                        cs.recent.push(Recent { task: display, result: memory_result(a) });
                        trim_recent(&mut cs.recent);
                        out.push((w.chat, text));
                    }
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

    /// Tests that redirect `HOME` / `XDG_STATE_HOME` / `TELEGRAM_REGISTRY` take this first.
    ///
    /// Those are process-wide, and cargo runs tests on threads: without it, one test's `remove_var`
    /// lands in the middle of another's read. This file already learned that once — the two halves
    /// of the ownership test were merged into one for exactly this reason — and a second test that
    /// needed the same environment is the moment to name the rule instead of merging again.
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner())
    }

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
        let _g = env_guard();
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };

        // Two watches for the SAME private chat, one per bot — exactly the live situation.
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };
        with_state(|s| {
            s.watch.insert(
                "1".into(),
                Watch { chat: 1711036782, task: "t1".into(), display_task: None, compacting: false, bot: super::super::registry_name(), reported_calls: 0 },
            );
        });
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram-groups") };
        with_state(|s| {
            s.watch.insert(
                "2".into(),
                Watch { chat: 1711036782, task: "t2".into(), display_task: None, compacting: false, bot: super::super::registry_name(), reported_calls: 0 },
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

    /// Two tasks must not land in one directory.
    ///
    /// The live failure this prevents (2026-08-05): agent #2 reported "Created a Rust program …
    /// Verified", the gate reported ✔, and it wrote nothing at all — the previous task's program
    /// was already in the shared root, so the check passed on somebody else's work. A green check
    /// that is about the directory rather than the run is worse than no check.
    /// The ACK has to name the directory the work is actually in.
    ///
    /// The first version of this feature read the workspace out of the POST response — which is
    /// `{"id": N}` and carries no workspace — so the message fell through to the old branch and
    /// told the operator the work was in the sandbox root while it was really in its own
    /// directory. The unit test then in place asserted the directory NAME and never the message,
    /// which is why a live report was needed to see it. This renders the real response shape.
    #[test]
    fn the_ack_names_the_directory_the_work_is_in() {
        let _g = env_guard();
        let d = tempfile::tempdir().unwrap();
        // HOME too: `default_workspace()` is `$HOME/.nadia`, and a test must not create task
        // directories in the operator's real sandbox.
        let home = std::env::var_os("HOME");
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };
        unsafe { std::env::set_var("HOME", d.path()) };
        let cmd = Cmd::Spawn("напиши программу".into());

        // The chooser and the message, exactly as dispatch wires them — and `body` is what the
        // server really answers: `{"id": N}`, no workspace in it. The first version of this
        // feature read the path back out of that body, found nothing, and told the operator the
        // work was in the sandbox root while the agent was correctly in its own directory. A test
        // that hand-built a body with the field would have passed through that bug, which is why
        // this one starts where dispatch starts.
        let ws = spawn_workspace(&cmd, 1711036782).unwrap();
        let text = render(&cmd, &serde_json::json!({ "id": 4 }), 1711036782, ws.as_deref());

        let dir = ws.unwrap();
        assert!(dir.starts_with(default_workspace().join("tasks")), "{}", dir.display());
        assert!(dir.is_dir(), "the directory was not created before the agent started");
        assert!(
            text.contains(&dir.display().to_string()),
            "the ACK did not name the task directory:\n{text}"
        );
        assert!(
            !text.contains("личная песочница nadia, а не твой репозиторий"),
            "the ACK fell through to the shared-root hint:\n{text}"
        );
        unsafe { std::env::remove_var("XDG_STATE_HOME") };
        match home {
            Some(h) => unsafe { std::env::set_var("HOME", h) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn every_task_gets_its_own_directory() {
        let _g = env_guard();
        let a = task_workspace("напиши на Rust программу: cargo run -- 3 4 печатает 7");
        let b = task_workspace("совсем другая задача");
        assert_ne!(a, b, "two tasks would have shared a workspace");
        assert!(a.starts_with(default_workspace().join("tasks")), "{}", a.display());

        // The name carries the day and whatever of the task is filesystem-safe. Russian tasks
        // leave only the timestamp, which is the point: a directory name is for finding the work
        // again, not for reproducing the sentence.
        let cyrillic = task_workspace("сделай что-нибудь");
        let name = cyrillic.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name.chars().all(|c| c.is_ascii()), "{name}");
        assert_eq!(slug("add a --json flag"), "add-a-json-flag");
        // Transliterated, not dropped: dropping is what produced `2026-08-05-002343-`, a
        // directory whose name said nothing about what had been asked of it.
        assert_eq!(slug("напиши"), "napishi");
        assert_eq!(slug("Ещё Задача"), "esche-zadacha");
        // And never a name that is only a separator.
        assert_eq!(slug("!!! ???"), "task");
        assert!(!slug("привет мир").ends_with('-'));
        // Long tasks are cut, not carried whole into a path.
        assert!(slug(&"word ".repeat(40)).len() <= 24);
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
        let w = Watch { chat: 7, task: "fix the test".into(), display_task: None, compacting: false, bot: legacy_owner(), reported_calls: 0 };
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
        assert!(render(&Cmd::Status(7), &body, 42, None).contains("no agent 7"));
    }

    #[test]
    fn a_listing_reads_on_a_phone() {
        let body = serde_json::json!({"agents": [
            {"id": 1, "phase": "running", "tool_calls": 4, "elapsed_secs": 12,
             "last_tool": "bash", "task": "fix the flaky test"}
        ]});
        let s = render(&Cmd::List, &body, 42, None);
        assert!(s.contains("#1 running"), "{s}");
        assert!(s.contains("[bash]"), "{s}");
        assert!(s.contains("fix the flaky test"), "{s}");
    }

    /// Plain text defaults to nadia ONLY for the owner, and only until the chat makes its own
    /// choice — a guest granted `chat` (no write/shell) must keep reaching the ordinary
    /// assistant, not the `Need::Drive` refusal `handle_text` gives a stranger to `/spawn`.
    #[test]
    fn dialog_defaults_on_for_the_owner_and_off_for_anyone_else() {
        let _g = env_guard();
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };

        // Nobody has said anything about either chat yet.
        assert!(dialog_routes(1, true), "the owner's own chat should default on");
        assert!(!dialog_routes(2, false), "a guest's chat must not default on");

        // The owner turns it off for their chat: that choice, not the default, now governs.
        set_dialog(1, Some(false));
        assert!(!dialog_routes(1, true));

        // A guest's chat the owner explicitly turns on stays on regardless of who is typing —
        // the setting is per chat, and a shared group chat is exactly the case for this.
        set_dialog(2, Some(true));
        assert!(dialog_routes(2, false));
    }

    #[test]
    fn progress_all_off_and_every_n_parse_and_report_back() {
        let _g = env_guard();
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };

        assert_eq!(progress_every(9), 1, "real-time by default, unset");
        assert!(set_progress(9, "off").contains("отключены"));
        assert_eq!(progress_every(9), 0);
        assert!(set_progress(9, "3").contains("раз в 3"));
        assert_eq!(progress_every(9), 3);
        assert!(set_progress(9, "all").contains("каждый шаг"));
        assert_eq!(progress_every(9), 1);
        assert!(set_progress(9, "не число").starts_with("Не понял"));
        assert_eq!(progress_every(9), 1, "a bad value must not clobber the last good one");

        assert_eq!(parse("/progress"), Some(Ok(Cmd::Progress(None))));
        assert_eq!(parse("/progress off"), Some(Ok(Cmd::Progress(Some("off".into())))));
        assert_eq!(Cmd::Progress(None).need(), Need::Look);
        assert_eq!(Cmd::Progress(Some("off".into())).need(), Need::Drive);
    }

    #[test]
    fn clear_and_compact_parse_and_need_drive() {
        assert_eq!(parse("/clear"), Some(Ok(Cmd::ClearMemory)));
        assert_eq!(parse("/compact"), Some(Ok(Cmd::Compact)));
        assert_eq!(Cmd::ClearMemory.need(), Need::Drive);
        assert_eq!(Cmd::Compact.need(), Need::Drive);
    }

    /// A fresh `/spawn`'s task gets this chat's memory prepended once there is any; `/clear`
    /// empties it back to "send verbatim". Doesn't touch nadia serve — just the text-building
    /// and state-clearing halves, which is what a small model's actual prompt depends on.
    #[test]
    fn recent_memory_is_prepended_to_a_fresh_spawn_and_clear_empties_it() {
        let _g = env_guard();
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("XDG_STATE_HOME", d.path()) };
        unsafe { std::env::set_var("TELEGRAM_REGISTRY", "telegram") };

        assert_eq!(with_recent_memory(11, "почини тест"), "почини тест", "nothing to remember yet");

        with_state(|s| {
            s.chats.entry(chat_key(11)).or_default().recent.push(Recent {
                task: "добавил флаг --json".into(),
                result: "готово, cargo test проходит".into(),
            });
        });
        let augmented = with_recent_memory(11, "почини тест");
        assert!(augmented.contains("добавил флаг --json"), "{augmented}");
        assert!(augmented.contains("cargo test проходит"), "{augmented}");
        assert!(augmented.ends_with("Новая задача: почини тест"), "{augmented}");

        assert!(clear_memory(11).contains("очищена"));
        assert_eq!(with_recent_memory(11, "почини тест"), "почини тест", "cleared, so verbatim again");
        assert!(clear_memory(11).contains("и так пуста"), "clearing twice must not error");
    }

    /// The character budget drops the OLDEST entry first, never the newest — symmetric with
    /// how a long-running chat here favors recent turns.
    #[test]
    fn memory_trims_oldest_first_once_over_budget() {
        let mut recent = vec![
            Recent { task: "a".repeat(MEMORY_CHAR_BUDGET), result: "old".into() },
            Recent { task: "b".repeat(10), result: "new".into() },
        ];
        trim_recent(&mut recent);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].result, "new", "the newest entry must survive the trim");
    }
}
