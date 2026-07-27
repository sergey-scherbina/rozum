//! Administration of the messenger assistant — bots, their group registries, and the
//! per-room permission rosters — as ONE set of operations.
//!
//! Everything here used to be reachable only from inside Telegram (`/addgroup`, `/grant`, …).
//! That breaks exactly when you need it most: on 2026-07-27 the operator left the test
//! supergroup and could not rejoin, leaving a registry entry that pointed at an unreachable
//! room with no command able to remove it. This module is the answer, and the CLI
//! (`rozum-gateway messenger …`), the UCC REST layer and the in-chat commands all go through
//! it so the three can't drift.
//!
//! Spec: `docs/specs/messenger-admin-console.md`.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::meeting::store::rozum_state_dir;
use crate::messenger_acl::{Acl, Caps};
use crate::messenger_groups::{Registry, default_room};

/// One bot deployment: which secret holds its token, which group-registry namespace it owns,
/// which room its primary (owner DM) chat maps to, and the two launchd services that run it.
///
/// Registries are NAMESPACED per bot on purpose — that is what lets a second bot serve groups
/// without touching the personal bot's groups or rooms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bot {
    /// Short handle, also the registry/secret naming stem (e.g. `telegram`, `telegram-groups`).
    pub name: String,
    #[serde(default = "default_platform")]
    pub platform: String,
    /// Group-registry namespace (`messenger-groups/<registry>.json`).
    pub registry: String,
    /// Room the bot's primary (owner DM) chat maps to.
    pub room: String,
    /// File name under `~/.rozum/secrets/` holding the bot token. The token itself is NEVER
    /// read into any struct that gets serialized back to a caller.
    pub secret: String,
    /// launchd label of the bridge (poller) service.
    pub bridge_label: String,
    /// launchd label of the participant-pool service.
    pub pool_label: String,
    #[serde(default)]
    pub mention_alias: String,
}

fn default_platform() -> String {
    "telegram".into()
}

/// The set of known bots.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Bots {
    #[serde(default)]
    pub bots: Vec<Bot>,
}

/// `~/.rozum/secrets` — mode-600 token files, never in git, never in a plist.
pub fn secrets_dir() -> PathBuf {
    home().join(".rozum/secrets")
}

pub fn secret_path(secret: &str) -> PathBuf {
    secrets_dir().join(secret)
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

impl Bots {
    pub fn path() -> PathBuf {
        rozum_state_dir().join("messenger-bots.json")
    }

    /// Load the bot list. When the file does not exist yet, SEED it from the deployments this
    /// repo has shipped by convention (their secret file existing is the evidence they are
    /// real) — so the console shows the truth on first run instead of an empty list, without
    /// the operator re-registering bots that already work. Seeding is in-memory only; it is
    /// persisted the first time something is actually changed.
    pub fn load(path: &Path) -> Bots {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_default(),
            Err(_) => Bots { bots: seeded_bots(&secrets_dir()) },
        }
    }

    pub fn load_default() -> Bots {
        Bots::load(&Bots::path())
    }

    /// Atomically persist (write temp + rename), creating the parent dir.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let parent =
            path.parent().ok_or_else(|| std::io::Error::other("bots path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?)?;
        std::fs::rename(&tmp, path)
    }

    pub fn get(&self, name: &str) -> Option<&Bot> {
        self.bots.iter().find(|b| b.name == name)
    }

    pub fn remove(&mut self, name: &str) -> Option<Bot> {
        let i = self.bots.iter().position(|b| b.name == name)?;
        Some(self.bots.remove(i))
    }

    /// Add or replace a bot by name.
    pub fn upsert(&mut self, bot: Bot) {
        match self.bots.iter_mut().find(|b| b.name == bot.name) {
            Some(slot) => *slot = bot,
            None => self.bots.push(bot),
        }
    }
}

/// The conventional deployments, included only when their token file is actually present.
/// Kept as data (not magic scattered through the code) so it is obvious what is assumed.
fn seeded_bots(secrets: &Path) -> Vec<Bot> {
    let candidates = [
        Bot {
            name: "telegram".into(),
            platform: "telegram".into(),
            registry: "telegram".into(),
            room: "assistant".into(),
            secret: "telegram-token".into(),
            bridge_label: "com.rozum.telegram".into(),
            pool_label: "com.rozum.assistant".into(),
            mention_alias: String::new(),
        },
        Bot {
            name: "telegram-groups".into(),
            platform: "telegram".into(),
            registry: "telegram-groups".into(),
            room: "rozumia".into(),
            secret: "telegram-groups-token".into(),
            bridge_label: "com.rozum.telegram-groups".into(),
            pool_label: "com.rozum.assistant-groups".into(),
            mention_alias: "@rozumia_bot".into(),
        },
    ];
    candidates.into_iter().filter(|b| secrets.join(&b.secret).exists()).collect()
}

/// Validate a bot name for use in file names, registry namespaces and launchd labels.
/// Deliberately strict: this string becomes a path component and a service label.
pub fn validate_bot_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("имя бота не может быть пустым".into());
    }
    if name.len() > 40 {
        return Err("имя бота слишком длинное (максимум 40 символов)".into());
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-') {
        return Err(format!(
            "недопустимое имя бота '{name}': только строчные латинские буквы, цифры и дефис"
        ));
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err("имя бота не может начинаться или заканчиваться дефисом".into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// What a group mutation did — reported back so a caller can tell "added" from "already there".
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GroupChange {
    pub registry: String,
    pub chat_id: i64,
    pub room: String,
    pub changed: bool,
}

/// Connect a group to a room in `registry`. Idempotent: re-adding an existing chat returns its
/// existing room (`changed = false`) rather than duplicating or re-pointing it — the same
/// contract `/addgroup` has, so the CLI and the chat command cannot disagree.
pub fn group_add(
    registry: &str,
    chat_id: i64,
    room: Option<&str>,
    title: &str,
) -> std::io::Result<GroupChange> {
    let path = Registry::path(registry);
    let mut reg = Registry::load(&path);
    let existed = reg.contains(chat_id);
    let desired = room.map(|r| r.to_string()).unwrap_or_else(|| default_room(chat_id));
    let room = reg.add(chat_id, &desired, title);
    if !existed {
        reg.save(&path)?;
    }
    Ok(GroupChange { registry: registry.into(), chat_id, room, changed: !existed })
}

/// Disconnect a group. `changed = false` when it was not connected in the first place.
pub fn group_remove(registry: &str, chat_id: i64) -> std::io::Result<GroupChange> {
    let path = Registry::path(registry);
    let mut reg = Registry::load(&path);
    match reg.remove(chat_id) {
        Some(g) => {
            reg.save(&path)?;
            Ok(GroupChange { registry: registry.into(), chat_id, room: g.room, changed: true })
        }
        None => Ok(GroupChange {
            registry: registry.into(),
            chat_id,
            room: String::new(),
            changed: false,
        }),
    }
}

/// The groups of one registry, as reported to a caller.
pub fn groups_list(registry: &str) -> Registry {
    Registry::load(&Registry::path(registry))
}

// ---------------------------------------------------------------------------
// ACL rosters
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct AclMemberView {
    pub user_id: i64,
    pub name: String,
    pub caps: String,
    pub owner: bool,
}

/// The roster of one room: the owner first, then members, each with a readable caps summary.
pub fn acl_show(room: &str) -> Vec<AclMemberView> {
    let acl = Acl::load(&Acl::path_for(room));
    let mut out = Vec::new();
    if let Some(owner) = acl.owner {
        out.push(AclMemberView {
            user_id: owner,
            name: "(владелец)".into(),
            caps: Caps::all().summary(),
            owner: true,
        });
    }
    for (id, m) in &acl.members {
        out.push(AclMemberView {
            user_id: *id,
            name: m.name.clone(),
            caps: m.caps.summary(),
            owner: false,
        });
    }
    out
}

/// Grant capabilities in ONE room. `caps` uses the same tokens as `/grant`
/// (`chat read write shell`, or `all` / `none`) so the two interfaces stay identical.
pub fn acl_grant(room: &str, user_id: i64, name: &str, caps: &[String]) -> Result<Caps, String> {
    let parsed = Caps::parse_tokens(caps.iter().map(|s| s.as_str()))?;
    let path = Acl::path_for(room);
    let mut acl = Acl::load(&path);
    if acl.is_owner(user_id) {
        return Err(format!("{user_id} — владелец комнаты, у него уже все права"));
    }
    acl.grant(user_id, name, parsed);
    acl.save(&path).map_err(|e| format!("не удалось сохранить ростер {}: {e}", path.display()))?;
    Ok(parsed)
}

/// Revoke a member from ONE room. Returns false when they were not on the roster.
pub fn acl_revoke(room: &str, user_id: i64) -> Result<bool, String> {
    let path = Acl::path_for(room);
    let mut acl = Acl::load(&path);
    let had = acl.revoke(user_id);
    if had {
        acl.save(&path)
            .map_err(|e| format!("не удалось сохранить ростер {}: {e}", path.display()))?;
    }
    Ok(had)
}

/// Every room that has a roster on disk — the rooms the console can offer for permission edits.
pub fn acl_rooms() -> Vec<String> {
    let dir = rozum_state_dir().join("messenger-acl");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut rooms: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_suffix(".json").map(|s| s.to_string())
        })
        .collect();
    rooms.sort();
    rooms
}

// ---------------------------------------------------------------------------
// Services
// ---------------------------------------------------------------------------

/// Live state of one launchd job, as far as the console cares.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ServiceState {
    pub label: String,
    /// `running`, `not running`, `waiting`, or `not installed`.
    pub state: String,
    pub pid: Option<u32>,
    /// launchd's own run counter — a large number here is the signature of a crash-loop
    /// (BUG-013: the gateway sat at 36301 runs / exit 78 for four days).
    pub runs: Option<u64>,
    pub last_exit: Option<String>,
}

/// Parse the fields the console shows out of `launchctl print` output. Kept separate from the
/// process call so it is unit-testable against captured real output.
pub fn parse_launchctl_print(out: &str) -> ServiceState {
    let mut st = ServiceState::default();
    for line in out.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("state = ") {
            if st.state.is_empty() {
                st.state = v.trim().to_string();
            }
        } else if let Some(v) = t.strip_prefix("pid = ") {
            st.pid = v.trim().parse().ok();
        } else if let Some(v) = t.strip_prefix("runs = ") {
            st.runs = v.trim().parse().ok();
        } else if let Some(v) = t.strip_prefix("last exit code = ") {
            st.last_exit = Some(v.trim().to_string());
        }
    }
    if st.state.is_empty() {
        st.state = "not installed".into();
    }
    st
}

/// Query one launchd job. Non-macOS hosts report `unsupported` rather than pretending.
pub fn service_state(label: &str) -> ServiceState {
    if !cfg!(target_os = "macos") {
        return ServiceState { label: label.into(), state: "unsupported".into(), ..Default::default() };
    }
    let target = format!("gui/{}/{label}", libc_getuid());
    let out = std::process::Command::new("launchctl").args(["print", &target]).output();
    let mut st = match out {
        Ok(o) if o.status.success() => parse_launchctl_print(&String::from_utf8_lossy(&o.stdout)),
        Ok(_) => ServiceState { state: "not installed".into(), ..Default::default() },
        Err(e) => ServiceState { state: format!("launchctl: {e}"), ..Default::default() },
    };
    st.label = label.to_string();
    st
}

fn libc_getuid() -> u32 {
    // `id -u` without a process: the effective uid of this process is what `gui/<uid>` wants.
    // std has no getuid, and pulling in `libc` for one call is not worth it — HOME-independent
    // and correct because control-serve and the CLI both run as the operator.
    std::env::var("UID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::process::Command::new("id")
                .arg("-u")
                .output()
                .ok()
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
                .unwrap_or(501)
        })
}

/// What to do to a service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceAction {
    Start,
    Stop,
    Restart,
}

impl ServiceAction {
    pub fn parse(s: &str) -> Result<ServiceAction, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "start" | "load" => Ok(ServiceAction::Start),
            "stop" | "unload" => Ok(ServiceAction::Stop),
            "restart" | "reload" | "kickstart" => Ok(ServiceAction::Restart),
            other => Err(format!("неизвестное действие '{other}' (start|stop|restart)")),
        }
    }
}

pub fn launchd_plist_path(label: &str) -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{label}.plist"))
}

/// Drive one launchd job. `Restart` uses `kickstart -k` when the job is loaded (cheap, keeps the
/// registration) and falls back to bootout+bootstrap otherwise — the recipe that fixed BUG-013,
/// where a job whose binary had been replaced under it could no longer exec at all.
pub fn service_control(label: &str, action: ServiceAction) -> Result<String, String> {
    if !cfg!(target_os = "macos") {
        return Err("управление сервисами поддерживается только на macOS (launchd)".into());
    }
    let uid = libc_getuid();
    let target = format!("gui/{uid}/{label}");
    let plist = launchd_plist_path(label);
    let run = |args: Vec<String>| -> (bool, String) {
        match std::process::Command::new("launchctl").args(&args).output() {
            Ok(o) => {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                (o.status.success(), s.trim().to_string())
            }
            Err(e) => (false, format!("launchctl: {e}")),
        }
    };
    match action {
        ServiceAction::Stop => {
            let (ok, out) = run(vec!["bootout".into(), target]);
            if ok { Ok(format!("{label}: остановлен")) } else { Err(out) }
        }
        ServiceAction::Start => {
            if !plist.exists() {
                return Err(format!("нет plist: {}", plist.display()));
            }
            let (ok, out) =
                run(vec!["bootstrap".into(), format!("gui/{uid}"), plist.display().to_string()]);
            if ok { Ok(format!("{label}: запущен")) } else { Err(out) }
        }
        ServiceAction::Restart => {
            let (ok, out) = run(vec!["kickstart".into(), "-k".into(), target.clone()]);
            if ok {
                return Ok(format!("{label}: перезапущен"));
            }
            // Not loaded (or a stale registration) — do the full re-register.
            if !plist.exists() {
                return Err(format!("{out}; нет plist: {}", plist.display()));
            }
            let _ = run(vec!["bootout".into(), target]);
            let (ok2, out2) =
                run(vec!["bootstrap".into(), format!("gui/{uid}"), plist.display().to_string()]);
            if ok2 {
                Ok(format!("{label}: перерегистрирован"))
            } else {
                Err(format!("{out}; {out2}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Installing a new bot
// ---------------------------------------------------------------------------

/// The persona every rozum assistant bot runs with. Lives here as ONE constant because it was
/// previously copy-pasted into each plist, and the "rozum is not an acronym" correction in it is
/// load-bearing: a 4B model reliably backronyms the name into "RUM, a Google project" without it.
pub const DEFAULT_PERSONA: &str = "Тебя (модель Qwen) обслуживает локальный проект rozum на Mac пользователя; тебе пишут из Telegram (личный чат или группа). ВНИМАНИЕ О НАЗВАНИИ: проект называется «rozum» (по-русски «Розум», значит «разум»); это НЕ аббревиатура — НИКОГДА не расшифровывай его как «RUM», «Reasoning Unit Model» или подобное и НЕ связывай с Google, это грубая ошибка; если такое встретится в истории — игнорируй. Пиши название только как «rozum» или «Розум». rozum — это local-first система пользователя для запуска LLM и ИИ-агентов на своём железе (Apple Silicon / MLX): локальный OpenAI/Anthropic-совместимый гейтвей для MLX и GGUF моделей; комнаты-встречи, где ИИ-агенты и люди координируются; телефонный контрол-центр (UCC); безопасная резидентность нескольких моделей (контроль допуска памяти). Отвечай кратко и по делу, на языке пользователя.";

pub const DEFAULT_MODEL: &str = "mlx-community:Qwen3.5-4B-MLX-4bit";
pub const DEFAULT_GATEWAY_URL: &str = "http://127.0.0.1:8089/v1";

/// Where a generated bridge wrapper lives.
pub fn wrapper_path(bot: &str) -> PathBuf {
    home().join(".rozum/bin").join(format!("rozum-{bot}-bridge.sh"))
}

/// Minimal XML escaping for plist string values (same rule as `src/service.rs`).
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// Shell single-quote escaping, for values interpolated into the generated wrapper.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The bridge launcher. The TOKEN IS NEVER IN THE PLIST — the wrapper reads it from the
/// mode-600 secret at exec time, which is why a plist (world-readable, and the sort of thing
/// that gets copied into a repo) can't leak it.
pub fn bridge_wrapper_script(bot: &Bot) -> String {
    format!(
        "#!/usr/bin/env bash\n\
         # GENERATED by `rozum-gateway messenger bot add` for @{name}. Do not edit by hand —\n\
         # re-run the command instead. The token is read from the mode-600 secret at exec time\n\
         # and never appears in the plist or in any argument list.\n\
         set -euo pipefail\n\
         SECRETS=\"$HOME/.rozum/secrets\"\n\
         export TELEGRAM_BOT_TOKEN=\"$(cat \"$SECRETS/{secret}\")\"\n\
         export TELEGRAM_CHAT_ID=\"$(cat \"$SECRETS/{chat_secret}\")\"\n\
         export TELEGRAM_OWNER_ID=\"$(cat \"$SECRETS/{chat_secret}\")\"\n\
         export TELEGRAM_REGISTRY={registry}\n\
         exec \"$HOME/.cargo/bin/rozum-gateway\" telegram --room {room} --name {room}\n",
        name = bot.name,
        secret = bot.secret,
        chat_secret = "telegram-chat-id",
        registry = sh_quote(&bot.registry),
        room = sh_quote(&bot.room),
    )
}

fn plist(label: &str, program_args: &[String], log: &str, comment: &str) -> String {
    let mut args = String::new();
    for a in program_args {
        args.push_str(&format!("    <string>{}</string>\n", xml_escape(a)));
    }
    let home = home().display().to_string();
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!--\n  {comment}\n  GENERATED by `rozum-gateway messenger bot add`. No secret is ever written here.\n-->\n\
         <plist version=\"1.0\">\n<dict>\n\
         \x20 <key>Label</key><string>{label}</string>\n\
         \x20 <key>ProgramArguments</key>\n  <array>\n{args}  </array>\n\
         \x20 <key>EnvironmentVariables</key>\n  <dict>\n\
         \x20   <key>HOME</key><string>{home}</string>\n\
         \x20   <key>PATH</key><string>{home}/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>\n\
         \x20 </dict>\n\
         \x20 <key>RunAtLoad</key><true/>\n\
         \x20 <key>KeepAlive</key><true/>\n\
         \x20 <key>StandardOutPath</key><string>{log}</string>\n\
         \x20 <key>StandardErrorPath</key><string>{log}</string>\n\
         </dict>\n</plist>\n"
    )
}

/// The bridge service plist (runs the generated wrapper).
pub fn bridge_plist(bot: &Bot) -> String {
    let log = home().join(format!(".rozum-{}-bridge.log", bot.name)).display().to_string();
    plist(
        &bot.bridge_label,
        &[wrapper_path(&bot.name).display().to_string()],
        &log,
        &format!("Telegram bridge for bot '{}' → room '{}'.", bot.name, bot.room),
    )
}

/// The participant-pool plist: one model per room (primary + every room in this bot's OWN
/// registry). Shares the resident gateway, so a second bot costs no extra model load.
pub fn pool_plist(bot: &Bot, model: &str, gateway_url: &str, sandbox: &str) -> String {
    let exe = home().join(".cargo/bin/rozum-gateway").display().to_string();
    let mut args = vec![
        exe,
        "meetings".into(),
        "participant-pool".into(),
        "--model".into(),
        model.into(),
        "--room".into(),
        bot.room.clone(),
        "--as".into(),
        "qwen".into(),
        "--registry".into(),
        bot.registry.clone(),
        "--gateway-url".into(),
        gateway_url.into(),
        "--sandbox".into(),
        sandbox.into(),
        "--shell".into(),
    ];
    if !bot.mention_alias.is_empty() {
        args.push("--mention-alias".into());
        args.push(bot.mention_alias.clone());
    }
    args.push("--persona".into());
    args.push(DEFAULT_PERSONA.into());
    let log = home().join(format!(".rozum-{}-pool.log", bot.name)).display().to_string();
    plist(
        &bot.pool_label,
        &args,
        &log,
        &format!("Participant pool for bot '{}' (registry '{}').", bot.name, bot.registry),
    )
}

/// Write the token to `~/.rozum/secrets/<secret>` with mode 600, creating the directory 700.
/// The token is passed in, used, and dropped — it is never stored in any struct that is
/// serialized back to a caller, logged, or included in an error message.
pub fn write_token_secret(secret: &str, token: &str) -> std::io::Result<PathBuf> {
    let dir = secrets_dir();
    std::fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(secret);
    std::fs::write(&path, token)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
}

/// Write the wrapper + both plists for a bot. Returns the paths written, in order.
pub fn write_bot_services(
    bot: &Bot,
    model: &str,
    gateway_url: &str,
    sandbox: &str,
) -> std::io::Result<Vec<PathBuf>> {
    let wrapper = wrapper_path(&bot.name);
    if let Some(parent) = wrapper.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&wrapper, bridge_wrapper_script(bot))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))?;
    }
    let bridge = launchd_plist_path(&bot.bridge_label);
    if let Some(parent) = bridge.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&bridge, bridge_plist(bot))?;
    let pool = launchd_plist_path(&bot.pool_label);
    std::fs::write(&pool, pool_plist(bot, model, gateway_url, sandbox))?;
    Ok(vec![wrapper, bridge, pool])
}

/// Build the `Bot` record a new deployment gets, from its name alone. Every derived name is a
/// pure function of the validated bot name, so nothing can collide by accident.
pub fn bot_from_name(name: &str, room: Option<&str>, mention_alias: &str) -> Result<Bot, String> {
    validate_bot_name(name)?;
    Ok(Bot {
        name: name.to_string(),
        platform: "telegram".into(),
        registry: name.to_string(),
        room: room.unwrap_or(name).to_string(),
        secret: format!("{name}-token"),
        bridge_label: format!("com.rozum.{name}"),
        pool_label: format!("com.rozum.{name}-pool"),
        mention_alias: mention_alias.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_plists_never_contain_the_token() {
        let bot = bot_from_name("bot2", Some("room2"), "@bot2").unwrap();
        let secret_ish = "8485632117:AAH-super-secret-token-value";
        let bridge = bridge_plist(&bot);
        let pool = pool_plist(&bot, DEFAULT_MODEL, DEFAULT_GATEWAY_URL, "/tmp/sandbox");
        let wrapper = bridge_wrapper_script(&bot);
        for (what, text) in [("bridge", &bridge), ("pool", &pool), ("wrapper", &wrapper)] {
            assert!(!text.contains(secret_ish), "{what} must never embed a token");
        }
        // The wrapper reads it from the mode-600 secret instead — that indirection IS the fix.
        assert!(wrapper.contains("$SECRETS/bot2-token"));
        assert!(bridge.contains("rozum-bot2-bridge.sh"));
        assert!(bridge.contains("com.rozum.bot2"));
        // The pool must run THIS bot's registry, or a second bot would hijack the first's groups.
        assert!(pool.contains("<string>--registry</string>"));
        assert!(pool.contains("<string>bot2</string>"));
        assert!(pool.contains("<string>--mention-alias</string>"));
    }

    #[test]
    fn derived_names_are_a_pure_function_of_the_bot_name() {
        let b = bot_from_name("groups", None, "").unwrap();
        assert_eq!(b.registry, "groups");
        assert_eq!(b.room, "groups", "room defaults to the bot name");
        assert_eq!(b.secret, "groups-token");
        assert_eq!(b.bridge_label, "com.rozum.groups");
        assert_eq!(b.pool_label, "com.rozum.groups-pool");
        assert!(b.mention_alias.is_empty(), "no alias => pool omits the flag");
        assert!(!pool_plist(&b, DEFAULT_MODEL, DEFAULT_GATEWAY_URL, "/s").contains("--mention-alias"));
        // A bad name is rejected BEFORE it can become a path or a launchd label.
        assert!(bot_from_name("../evil", None, "").is_err());
    }

    #[test]
    fn plist_escapes_xml_and_wrapper_escapes_shell() {
        // A room name with an XML metacharacter must not break the plist.
        let mut b = bot_from_name("bot3", Some("a&b"), "").unwrap();
        assert!(pool_plist(&b, DEFAULT_MODEL, DEFAULT_GATEWAY_URL, "/s").contains("a&amp;b"));
        // …and a quote in a shell-interpolated value must not break out of the wrapper.
        b.room = "it's".into();
        assert!(bridge_wrapper_script(&b).contains(r"'it'\''s'"));
    }

    #[test]
    fn bot_names_must_be_safe_path_and_label_components() {
        assert!(validate_bot_name("telegram-groups").is_ok());
        assert!(validate_bot_name("bot2").is_ok());
        // These are the ones that matter: a name becomes a file path AND a launchd label.
        assert!(validate_bot_name("../../etc/passwd").is_err());
        assert!(validate_bot_name("has space").is_err());
        assert!(validate_bot_name("Upper").is_err());
        assert!(validate_bot_name("-lead").is_err());
        assert!(validate_bot_name("trail-").is_err());
        assert!(validate_bot_name("").is_err());
        assert!(validate_bot_name(&"x".repeat(41)).is_err());
    }

    #[test]
    fn seeding_only_claims_bots_whose_secret_actually_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert!(seeded_bots(dir.path()).is_empty(), "no secrets => no bots invented");

        std::fs::write(dir.path().join("telegram-token"), "x").unwrap();
        let seeded = seeded_bots(dir.path());
        assert_eq!(seeded.len(), 1);
        assert_eq!(seeded[0].name, "telegram");
        assert_eq!(seeded[0].registry, "telegram");
        assert_eq!(seeded[0].pool_label, "com.rozum.assistant");

        std::fs::write(dir.path().join("telegram-groups-token"), "x").unwrap();
        let both = seeded_bots(dir.path());
        assert_eq!(both.len(), 2);
        // The whole point of the second bot: its own registry, its own room, its own services.
        assert_eq!(both[1].registry, "telegram-groups");
        assert_ne!(both[0].registry, both[1].registry);
        assert_ne!(both[0].bridge_label, both[1].bridge_label);
    }

    #[test]
    fn bots_round_trip_and_upsert_replaces_by_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("messenger-bots.json");
        let mut bots = Bots::default();
        bots.upsert(Bot {
            name: "telegram".into(),
            platform: "telegram".into(),
            registry: "telegram".into(),
            room: "assistant".into(),
            secret: "telegram-token".into(),
            bridge_label: "com.rozum.telegram".into(),
            pool_label: "com.rozum.assistant".into(),
            mention_alias: String::new(),
        });
        bots.upsert(Bot {
            name: "telegram".into(),
            platform: "telegram".into(),
            registry: "telegram".into(),
            room: "changed".into(),
            secret: "telegram-token".into(),
            bridge_label: "com.rozum.telegram".into(),
            pool_label: "com.rozum.assistant".into(),
            mention_alias: String::new(),
        });
        assert_eq!(bots.bots.len(), 1, "upsert replaces, never duplicates");
        assert_eq!(bots.get("telegram").unwrap().room, "changed");
        bots.save(&path).unwrap();

        let back = Bots::load(&path);
        assert_eq!(back.bots.len(), 1);
        assert_eq!(back.get("telegram").unwrap().room, "changed");
        assert!(back.get("nope").is_none());
    }

    #[test]
    fn parse_launchctl_print_reads_the_crash_loop_signature() {
        // Real shape of the BUG-013 output (a job respawning forever, never serving).
        let out = "\tstate = spawn scheduled\n\truns = 36301\n\tlast exit code = 78: EX_CONFIG\n\t\tstate = active\n";
        let st = parse_launchctl_print(out);
        assert_eq!(st.state, "spawn scheduled", "first state wins, not the nested 'active'");
        assert_eq!(st.runs, Some(36301));
        assert_eq!(st.last_exit.as_deref(), Some("78: EX_CONFIG"));
        assert_eq!(st.pid, None);

        let healthy = "\tstate = running\n\truns = 1\n\tpid = 54709\n\tlast exit code = (never exited)\n";
        let st2 = parse_launchctl_print(healthy);
        assert_eq!(st2.state, "running");
        assert_eq!(st2.pid, Some(54709));
        assert_eq!(st2.runs, Some(1));

        assert_eq!(parse_launchctl_print("").state, "not installed");
    }

    #[test]
    fn service_action_parses_the_words_people_actually_type() {
        assert_eq!(ServiceAction::parse("start").unwrap(), ServiceAction::Start);
        assert_eq!(ServiceAction::parse("LOAD").unwrap(), ServiceAction::Start);
        assert_eq!(ServiceAction::parse("stop").unwrap(), ServiceAction::Stop);
        assert_eq!(ServiceAction::parse(" restart ").unwrap(), ServiceAction::Restart);
        assert!(ServiceAction::parse("explode").is_err());
    }
}
